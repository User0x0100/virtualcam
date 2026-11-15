# AGENTS.md

本文件描述当前 `virtualcam` 仓库的实现边界与开发约束。修改前先读取相关源码；代码与可验证平台行为优先于本文。用户文档为 `README.md` 与 `docs/USER_GUIDE.zh-CN.md`。

## 项目与工作区

`virtualcam` 是 Rust 2024 Windows 虚拟摄像头库，当前原生源 DLL 面向 Windows x64。工作区：

| 位置 | 职责 |
| --- | --- |
| `src/` | 公开 API、注册管理、OBS/Unity producer、Managed producer、CUDA D2H |
| `crates/virtualcam-protocol/` | 自有帧格式、ring、同步、DShow/MF IPC 结构 |
| `crates/virtualcam-dshow/` | Rust DirectShow COM source filter（`virtualcam_dshow.dll`） |
| `crates/virtualcam-mf/` | Rust Media Foundation / Frame Server source（`virtualcam_mf.dll`） |
| `examples/managed_camera.rs` | 注册、列出、推流、注销及 MF source 安装 |
| `examples/generated_test_video.rs` | OBS / Unity 测试图案 |
| `examples/mf_delivery_bench.rs` | MF 交付性能测量工具 |
| `tests/` | 公开 API、已安装第三方驱动、真机 DShow / Frame Server 集成测试 |

现有后端：

- OBS Virtual Camera：第三方 NV12 三槽共享队列；库只提供 producer，不安装摄像头。
- Unity Capture：第三方 SharedImageMemory，RGBA8/FP16；库只提供 producer。
- Managed DirectShow：每次 producer session 决定 video format；注册可为 CurrentUser 或 AllUsers。
- Managed Media Foundation：每台摄像头一个注册时确定的 immutable 格式集合（一个默认格式 + 若干可选像素格式，共用默认格式的尺寸/帧率/色彩空间），且只允许 NV12/I420/YUY2/UYVY/P010 五种 YUV 格式（RGB 仅 DirectShow）；CurrentUser access、System lifetime，Frame Server 可跨 session 加载 source。

OBS/Unity 协议兼容是现有功能要求；不得因简化仓库而修改其对象名、结构布局、状态值、发布顺序或握手。外部协议参见 `UPSTREAM.md`。

`AppId` 是区分应用注册摄像头的标签（1–128 个 ASCII 字母、数字或 `._-`），不是安全凭据。`ManagedCameraManager::for_app` 仅管理匹配标签的摄像头。`CameraType`、`ManagedBackend` 等类型按公开定义使用，不人为合并两个 managed 后端的生命周期和注册语义。

## 帧、格式与数据路径

- Managed `PixelFormat`：NV12、I420、YUY2、UYVY、P010、BGRA32、RGB24；`VideoFormat` 携带尺寸、分数帧率和 `ColorSpace`。生产端必须写紧密排列的对应格式，不能隐式缩放、重采样或转换。
- DShow source 复制帧，并在需要时按 DIB 行序/行对齐处理 RGB；`VIDEOINFOHEADER2` 宣告色彩信息。consumer 连接后维持协商格式，实际 producer 格式不符时输出黑帧。
- MF source 只宣告注册时声明的格式集合，并为每个格式各建一个 media type；不做任何派生或转换。生产会话格式由 `media_foundation_session_format` 判定：像素格式必须在集合内、尺寸/帧率与默认格式一致、`ColorSpace` 必须与注册值完全相同，否则 `Error::FormatMismatch`；不允许静默替换请求的色彩参数。消费端选定的 mode 经 `requested_format()` 通知 producer 自己生成该格式；producer 尚未切换时输出黑帧。媒体类型列表按 4:2:2（YUY2/UYVY）在前、4:2:0（NV12/I420/P010）在后的稳定顺序列出（组内保持声明顺序），避免 Frame Server 的 DirectShow bridge 自行做 4:2:0 → 4:2:2 转换导致非 32 像素倍数宽度的行错位。
- 色彩属性是 metadata，不做自动色彩校正。必须按实际 `ColorSpace` 处理黑电平与格式布局。
- Frame Server 的 DirectShow bridge 读取紧密样本内存；MF 输出使用紧密线性 buffer，不假定消费者理解 2D allocator 的 padded pitch。`SamplePool` 重用 tracked samples。拷贝到外部 buffer 时遵守实际 pitch、负 stride、长度和 unlock 约定。

## 自有共享帧协议

Managed DirectShow 与 MF 复用 `virtualcam-protocol` 的 VCR2 ring、plane layout、pitch-aware copy/black、`sync.rs` 的发布顺序，以及 `delivery.rs` 的交付判断。control plane、注册、认证、格式选择和所有权由各后端独立管理。

VCR2 ring：4 KiB header page、4 KiB 对齐的 slots、64-byte slot header；每个 producer session 有新 generation。writer 对 slot 使用 odd/even seqlock，填充数据后发布 latest sequence；reader 复制前后验证，失败重试或返回 Contended，绝不交付 torn frame。writer 不等待 reader。

跨进程发布/观察字段只走 SeqCst atomic helper（Windows Interlocked 等价语义）；不能依赖 x86 的强顺序。OBS 发布也使用独立的 SeqCst helper，但 OBS 原生三槽 ABI 没有 per-slot generation，不能偷偷替换为 VCR2。

共用 managed 交付规则：generation + sequence 判断新帧；GCRA 虚拟网格按消费者协商帧率限速。新帧初次可见时不早于网格时刻前 1/4 interval 则立即交付，更早则跳过而非延迟；晚于网格 1/4 interval 才重锚定，不补发、不突发。没有 producer 帧时按节奏发黑帧；producer 停滞后每秒重复一次最新帧。producer 提交后通过 auto-reset frame event 通知；MF 按事件/状态/期限等待，不做毫秒轮询，并用高精度 waitable timer 维持空闲节奏。Contended 一律填黑，不能发布部分帧。

## Managed DirectShow

- IPC 为 session-local `Local\VirtualCamRs2-{ID}-...`：producer lease、seqlock control、generation ring/frame event、consumer lease/request。一个 producer、一个已连接 consumer graph；第二个返回 busy。`close()` 立即释放 writer lease。
- control 记录实际格式、generation、PID/创建时间；consumer request 记录协商格式。producer 可通过 `VirtualCamera::requested_format()` 读取不同于当前写入的请求；切换需 producer 自行以所需格式重开。
- 注册目录为 `CLSID_VideoInputDeviceCategory`；`REGFILTER2` v1、`MERIT_DO_NOT_USE`、单输出 pin；`FilterData` 必须经系统 `IAMFilterData::CreateFilterData` 序列化。提供对应 `IAMStreamConfig`、`IKsPropertySet`、media type/capability 枚举。注册作用域 CurrentUser（HKCU）或 AllUsers（HKLM）。
- idle 格式选择：显式 `SetFormat` > live producer > registered default > 当前用户最近成功推流格式 > NV12 640×480@30；随后依次宣告所注册的 pixel formats 与 frame rates。后两者为空时仅宣告当前格式和帧率。
- source 用 producer 进程 handle 加 creation time 校验存活，不能只相信 PID。释放的 producer 或不可用数据由 source 交黑帧。

## Managed Media Foundation

- source 使用固定 `SOURCE_CLSID`；`IMFActivate`（FTM/`IAgileObject`）、`IMFMediaSourceEx`、`IMFMediaStream2`、`IMFGetService`、`IKsControl` 以及适用的 allocator control。`ThreadingModel=Both` 不是 agility 的替代品。同一 activation 对象缓存其 source instance。
- 系统摄像头通过 `MFCreateVirtualCamera` 注册为 CurrentUser access、System lifetime。格式集合（默认格式 + 可选像素格式）注册后不可修改，改动需 recreate；rename 也是 recreate transaction，symbolic link 可变化。消费端在集合内部切换格式不需要 recreate。永久注销必须调用 `Remove`，`Shutdown` 只释放对象资源。
- per-user manager metadata 在 HKCU `Software\VirtualCamRs\Cameras\{ID}`，MF source 运行于 Local Service / Session 0，不能读取自己的 HKCU 当作 owner 数据。runtime 参数来自 MF vendor attributes，并须校验大小、版本与格式；缺失时 fail closed，禁止从未验证的 symbolic link 猜 owner。
- source 经 `\\.\pipe\VirtualCamRs-MF-{RID}` rendezvous：显式 DACL、first-pipe-instance、overlapped message mode、拒绝远端客户端；有界的握手读取后才 impersonate，并从 token SID 验证 owner，所有路径恢复身份。`EXPECTED_OWNER_SID` 只供校验，不能直接视为授权凭据。source 再创建随机命名、权限受控的 Global/同 session Local ring 与 frame event；发现已有任何类型同名内核对象都必须拒绝或换新 nonce。不得暴露跨用户帧注入。
- worker 可取消、有握手时限；`Shutdown` 取消未完成 I/O、在状态锁外等待 worker，不能在 `DllMain` 停启。producer lazy attach，未有活跃消费端时 `write_host` 返回 `ConsumerInactive`；pipe 关闭后重新等待，source 则回黑帧。
- `RequestSample` 在 RUNNING 下受帧事件和交付期限驱动；STOPPED / PAUSED 不立即输出。PAUSED 保留待处理请求，恢复时唤醒；STOPPED 清空旧请求，防止重启后交付旧 token。正常停止、生产者缺席、短暂通信故障不发送 EOS；源与流都维护正确 MF events/attributes。
- sample time 使用 `MFGetSystemTime`；duration = 默认格式的帧间隔（集合内所有格式共用）。若消费者提供 allocator，按 Initialize/Allocate/Uninitialize 合同处理，异常时可用标准 MF buffer fallback。Source/Stream 有 shutdown 时拆掉的引用环；禁止锁内 blocking `GetEvent`，避免反向锁顺序。
- 卸载 machine-wide source 前必须移除当前用户的 MF cameras 并确认其他用户的注册也已清理；无法证明时 fail safe。不能假设可跨用户枚举所有注册。

## COM / FFI / 运行时性能

- 两个 DLL 的 `DllMain` 保持极简：不做 COM/注册表/IPC/worker 创建或等待。
- COM exports、class factory 与方法在 FFI 边界 catch panic，返回稳定 HRESULT，不让 panic 穿越 ABI。外部属性、共享内存、管道消息在读取前先验证长度、边界与身份，不使用不受控输入的 unchecked unwrap。
- COM apartment 初始化仅由实际成功初始化的一方配对取消；source 和 manager 不得随意解除调用线程的 COM 初始化。
- 热路径避免逐帧注册表访问、反复打开内核对象、帧大小分配、隐藏转换和不必要的 staging/whole-frame pitch copy。
- CUDA feature 运行时解析应用已加载或可加载的 CUDA Runtime，`write_cuda` 直接从设备内存 D2H 到目标共享帧 slot，并等待 stream 后再发布；不额外分配整帧中转缓存。调用方管理设备/context/stream 生命周期。

## 支持边界与验证

原生 managed DLL 当前仅支持 x64；不宣称 x86 DirectShow DLL 或 ARM64 MF 安装可用。多用户 / Fast User Switching 的 MF 真机隔离与卸载完整性仍需验证；当前威胁模型仅隔离跨用户、远程和未认证 producer，不隔离同一 Windows 用户的恶意进程。

常用验证：

```powershell
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo test --workspace --all-features
cargo test --release --test windows_dshow -- --ignored --test-threads=1
cargo test --test windows_mf -- --ignored
```

最后两组需要 Windows 本机、相关 DLL/FFmpeg/Frame Server 配置，不作为普通默认测试。已安装 OBS/Unity 的端到端测试也需显式运行 ignored suite。

修改流程：先检查 `git status` 与相关代码；保护未提交改动；修改后执行适用测试并检查 `git -c core.whitespace=cr-at-eol diff --check`、公开 API、示例、README 和中文指南。不要 `reset --hard`、无关 checkout、删除未知用户文件或重写 Git 历史。

官方平台约束优先查 Microsoft 文档和 Windows SDK，第三方协议对照 `UPSTREAM.md` 的固定引用。
