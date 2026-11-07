# virtualcam 使用说明书

适用版本：`virtualcam` 0.1.0（workspace `master`，2026-10-24）
平台：Windows 10/11 x64

---

## 目录

1. [概述](#1-概述)
2. [选择哪种摄像头](#2-选择哪种摄像头)
3. [构建](#3-构建)
4. [部署与安装](#4-部署与安装)
5. [命令行快速上手](#5-命令行快速上手)
6. [在 Rust 程序中使用](#6-在-rust-程序中使用)
7. [格式、分辨率、帧率与色彩空间](#7-格式分辨率帧率与色彩空间)
8. [运行时行为](#8-运行时行为)
9. [OBS Virtual Camera 与 Unity Capture](#9-obs-virtual-camera-与-unity-capture)
10. [CUDA 直写](#10-cuda-直写)
11. [旧版 C++ 驱动摄像头迁移](#11-旧版-c-驱动摄像头迁移)
12. [卸载](#12-卸载)
13. [错误与排查](#13-错误与排查)
14. [测试](#14-测试)
15. [限制](#15-限制)

---

## 1. 概述

`virtualcam` 是一个 Rust 库，让你的程序把视频帧送进 Windows 虚拟摄像头。它提供两类能力：

- **自有摄像头（managed camera）**：由本项目注册、名字自定、可同时存在多台。纯 Rust 实现两个后端：
  - **DirectShow**：`virtualcam_dshow.dll`
  - **Media Foundation（Frame Server）**：`virtualcam_mf.dll`
- **第三方摄像头协议**：向已安装的 OBS Virtual Camera、Unity Capture 写帧。

整体数据流：

```text
你的程序 ──write_host / write_cuda──▶ 共享内存 ring ──▶ source DLL ──▶ 摄像头应用
                                                     (DirectShow 或 Frame Server)
```

设计原则：

- 不缩放像素。你写入什么格式，消费端就收到什么格式；唯一例外是 Media Foundation 摄像头在原生 NV12/I420 之外还提供由 source 自行转换的 YUY2（避免 Windows DirectShow 桥接自带转换在宽度非 32 倍数时错位）。
- 共享内存 ring 带 per-slot 序列锁，消费端永远不会拿到"撕裂"的半帧；写入端从不等待消费端。
- 没有生产者时摄像头输出黑帧，而不是报错或冻结。

## 2. 选择哪种摄像头

| | DirectShow | Media Foundation |
| --- | --- | --- |
| 能看到它的应用 | FFmpeg `-f dshow`、OpenCV `CAP_DSHOW`、OBS、Zoom 等传统 DirectShow 应用 | Windows 相机、Chrome/Edge、Teams、WinRT `MediaCapture`、`IMFSourceReader` |
| 安装 | 无需安装；注册时指向 DLL 路径 | 每台机器一次，需管理员 |
| 注册 | `CurrentUser`（免管理员）或 `AllUsers`（需管理员） | 仅当前用户，免管理员 |
| 格式 | 每次打开生产者时自由决定 | 注册时固定一个原生格式，之后不可改 |
| 跨会话 | 仅同一登录会话 | Frame Server 在 Session 0，跨会话 |

经验法则：

- 面向现代应用（浏览器、Teams、Windows 相机）→ **Media Foundation**。
- 面向 FFmpeg/OpenCV/OBS，或需要运行时切换分辨率/格式 → **DirectShow**。
- 两种都要 → 各注册一台，两个生产者分别写入。

## 3. 构建

需要：Rust（edition 2024 工具链）、MSVC 构建工具、Windows SDK。不需要 CMake 或 C++ 编译器。

```powershell
cargo build --release -p virtualcam-dshow -p virtualcam-mf
cargo build --release --example managed_camera
```

产物：

| 文件 | 作用 |
| --- | --- |
| `target\release\virtualcam_dshow.dll` | DirectShow source |
| `target\release\virtualcam_mf.dll` | Media Foundation source |
| `target\release\examples\managed_camera.exe` | 命令行管理工具（示例） |

在自己的项目里引用库：

```toml
[dependencies]
virtualcam = { path = "../virtualcam" }               # 或 git 依赖
# virtualcam = { path = "../virtualcam", features = ["cuda", "tracing"] }
```

可选 feature：

- `cuda`：启用 `write_cuda`（设备显存直接 D2H 到共享内存）。
- `tracing`：输出 `tracing` 调试/警告事件；不开启时零开销。

## 4. 部署与安装

### 4.1 DirectShow

无需安装步骤。注册时会把 DLL 的规范化绝对路径写进 COM 注册表，因此：

- 把 `virtualcam_dshow.dll` 放在**稳定**的本地路径（例如程序安装目录）。网络/UNC 路径会被拒绝。
- 默认查找位置是**当前可执行文件同目录**下的 `virtualcam_dshow.dll`；也可用 `with_dshow_source(path)` 或命令行 `--dshow-source <path>` 指定。
- 注册后不要移动或删除该 DLL，否则摄像头会显示为 `Degraded`。

### 4.2 Media Foundation（每台机器一次，需管理员）

Frame Server 以 **Local Service** 身份加载 DLL，所以 DLL 必须放在该账户可读的位置（如 `Program Files`），**不能放在用户目录下**。

在**管理员** PowerShell 中：

```powershell
New-Item -ItemType Directory -Force "C:\Program Files\VirtualCamRs"
Copy-Item .\target\release\virtualcam_mf.dll "C:\Program Files\VirtualCamRs\"
.\target\release\examples\managed_camera.exe install-mf "C:\Program Files\VirtualCamRs\virtualcam_mf.dll"
```

成功时输出 `installed Some("C:\\Program Files\\VirtualCamRs\\virtualcam_mf.dll")`。

在代码中等价调用：`ManagedCameraManager::install_media_foundation_source(path)`；查询是否已安装：`ManagedCameraManager::media_foundation_source()`。

> 升级 DLL：正在被 Frame Server 或其他进程加载的 DLL 无法覆盖。建议使用带版本号的新文件名，再执行一次 `install-mf` 指向新文件；旧文件在不再被占用后删除。

## 5. 命令行快速上手

`managed_camera` 示例覆盖了全部管理操作：

```text
managed_camera register "<名字>" [--app <APP_ID>] [--backend dshow|mf] [--scope user|machine]
                                 [--format nv12] [--size 1280x720] [--fps 30]
managed_camera list         [--app <APP_ID>]
managed_camera stream <ID>  [--format yuy2] [--size 1280x720] [--fps 30] [--seconds 30]
managed_camera unregister <ID> [--app <APP_ID>]
managed_camera legacy                       # 列出旧 C++ 驱动的摄像头
managed_camera remove-legacy <ID>           # 管理员
managed_camera install-mf <PATH>            # 管理员，每台机器一次
managed_camera uninstall-mf [--other-users-cleaned]   # 管理员
```

### 5.1 DirectShow 示例

```powershell
$exe = ".\target\release\examples\managed_camera.exe"
$id  = & $exe register "Studio Cam" --app com.example.studio --size 1280x720 --fps 30
& $exe list --app com.example.studio
& $exe stream $id --format yuy2 --size 1920x1080 --fps 60 --seconds 60
```

推流期间用 FFmpeg 验证：

```powershell
ffplay -f dshow -i video="Studio Cam"
```

画面应为亮度渐变背景上一根移动的白色竖条。

### 5.2 Media Foundation 示例

```powershell
$id = & $exe register "Studio MF Cam" --backend mf --size 1280x720 --fps 30
& $exe stream $id --seconds 60
```

推流期间打开"相机"应用或浏览器，选择 **Studio MF Cam (Windows 虚拟摄像头)**。Windows 会在名字后自动追加本地化后缀。

MF 摄像头在 `stream` 时自动使用注册时的格式；`--format/--size/--fps` 对 MF 的 `stream` 无效。

### 5.3 删除

```powershell
& $exe unregister $id
```

## 6. 在 Rust 程序中使用

### 6.1 核心类型

| 类型 | 作用 |
| --- | --- |
| `ManagedCameraManager` | 注册/列出/重命名/删除/打开自有摄像头；可廉价 clone |
| `AppId` | 程序命名空间：`A-Z a-z 0-9 . _ -`，1–128 字符 |
| `DirectShowRegistration` | DirectShow 注册参数：`name`、`scope`、`default_format`、`pixel_formats`（按偏好排序的可选像素格式）、`frame_rates`（按偏好排序的可选帧率，至多 16 个） |
| `MediaFoundationRegistration` | MF 注册参数：`name`、`native_format` |
| `ManagedCameraInfo` | 查询结果：`id`、`name`、`backend`、`scope`、`app_id`、`format`、`health` |
| `ManagedCameraId` | 稳定 ID（GUID 形式），可 `Display`/`parse` |
| `VirtualCamera` | 已打开的生产者：`write_host`、`is_ready`、`close` 等 |
| `VideoFormat` | 像素格式 + 宽高 + 帧率 + 色彩空间 |
| `WriteStatus` | 单次写入的非致命结果 |

### 6.2 AppId：区分不同程序的摄像头

```rust
let manager = ManagedCameraManager::for_app(AppId::new("com.example.studio")?);
```

- 用 `for_app` 创建的 manager 注册的摄像头会带上该 AppId。
- 它的 `list()` 只返回该 AppId 的摄像头；对其他摄像头的 `get/open/rename/unregister` 返回 `Error::CameraNotFound`。
- `ManagedCameraManager::new()` 不过滤，能看到当前用户可见的全部摄像头。
- **AppId 不是安全边界**，只是命名空间。任何程序都可以用同一个 AppId。

推荐使用反向域名风格，例如 `com.company.product`。

### 6.3 DirectShow：注册、推流、删除

```rust
use virtualcam::{
    AppId, ColorSpace, DirectShowRegistration, FrameRate, ManagedCameraManager,
    PixelFormat, RegistrationScope, VideoFormat, WriteStatus,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ManagedCameraManager::for_app(AppId::new("com.example.studio")?)
        .with_dshow_source(r"C:\Program Files\Studio\virtualcam_dshow.dll");

    // 已存在同名摄像头时复用，否则注册。
    let info = match manager.list()?.into_iter().find(|c| c.name == "Studio Cam") {
        Some(info) => info,
        None => manager.register(DirectShowRegistration {
            name: "Studio Cam".into(),
            scope: RegistrationScope::CurrentUser, // 免管理员
            // 没有生产者时，空闲黑帧使用此格式。
            default_format: Some(VideoFormat::new(PixelFormat::Nv12, 1280, 720, FrameRate::fps(30))),
            // 同时提供给应用的像素格式；应用的选择经 requested_format() 告知生产者。
            pixel_formats: vec![PixelFormat::Nv12, PixelFormat::Yuy2],
            frame_rates: vec![FrameRate::fps(25), FrameRate::fps(60)],
        })?,
    };

    // 每次打开都可以选不同格式。
    let format = VideoFormat::new(PixelFormat::Yuy2, 1920, 1080, FrameRate::fps(60))
        .with_color(ColorSpace::BT709);
    let mut camera = manager.open(info.id, format)?;

    let mut frame = vec![0_u8; camera.frame_size()];
    for _ in 0..600 {
        // ……把 YUY2 像素填进 frame……
        frame.fill(0x80);
        match camera.write_host(&frame)? {
            WriteStatus::Written => {}
            other => eprintln!("frame not delivered: {other:?}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    camera.close()?; // 消费端回到黑帧

    // manager.unregister(info.id)?;  // 需要永久删除时
    Ok(())
}
```

要点：

- `DirectShowRegistration::new(name)` 等价于 `CurrentUser` + 无默认格式。
- `RegistrationScope::AllUsers` 写入 HKLM，对本机所有用户可见，需要管理员。
- `write_host` 的 `frame` 长度必须**精确等于** `camera.frame_size()`，否则返回 `Error::FrameSizeMismatch`。
- 你负责帧节奏（按帧率写入）。source 端用高精度定时器按协商帧率输出最新帧，不会补发积压帧。

### 6.4 Media Foundation：注册、推流

```rust
use virtualcam::{
    FrameRate, ManagedCameraManager, MediaFoundationRegistration, PixelFormat, VideoFormat, WriteStatus,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if ManagedCameraManager::media_foundation_source().is_none() {
        return Err("Media Foundation source 未安装（见第 4.2 节）".into());
    }
    let manager = ManagedCameraManager::new();
    let native = VideoFormat::new(PixelFormat::Nv12, 1920, 1080, FrameRate::fps(30));
    let info = manager.register(MediaFoundationRegistration::new("Studio MF Cam", native))?;

    // 使用原生格式；open(id, format) 也接受 source 提供的 YUY2 mode。
    let mut camera = manager.open_registered(info.id)?;
    let frame = vec![0x10_u8; camera.frame_size()];
    loop {
        match camera.write_host(&frame)? {
            WriteStatus::Written => {}                 // 有应用正在使用摄像头
            WriteStatus::ConsumerInactive => {}        // 没有应用打开它；帧被丢弃，可降低工作量
            WriteStatus::FrameSkippedByConsumer => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(33));
    }
}
```

要点：

- MF 摄像头的格式在注册时固定。要换格式，就 `unregister` 后重新 `register`。
- 生产者是**惰性连接**的：没有应用打开摄像头时，`write_host` 返回 `WriteStatus::ConsumerInactive`，`is_ready()` 为 `false`。应用打开摄像头后，生产者在约 250 ms 内自动连接，之后返回 `Written`。
- 只有摄像头所属的 Windows 用户能向它推流。source 通过命名管道校验生产者进程令牌中的 SID；其他用户的进程无法注入画面。
- `rename` 对 MF 摄像头是"删除后重建"，Windows 的 symbolic link 可能改变，已经打开它的应用需要重新打开。

### 6.5 查询、重命名、健康状态

```rust
for cam in manager.list()? {
    println!("{} {:?} {:?} {:?} {:?}", cam.id, cam.name, cam.backend, cam.format, cam.health);
}
let renamed = manager.rename(id, "New Name")?;     // 返回新的 ManagedCameraInfo
let one = manager.get(id)?;
```

`RegistrationHealth`：

- `Healthy`：注册完整。
- `Degraded(原因)`：注册不完整，例如 DLL 已被移走、COM/类别项缺失、MF source 未安装。处理方式：`unregister` 后重新 `register`。

名字规则：同一可见范围内名字唯一，重复时返回 `Error::NameConflict`。

### 6.6 VirtualCamera 其他方法

| 方法 | 说明 |
| --- | --- |
| `camera_type()` | `ManagedDirectShow` / `ManagedMediaFoundation` / OBS / Unity |
| `video_format()` | 当前流格式（Unity 为 `None`） |
| `frame_size()` | 每帧精确字节数 |
| `is_open()` | 本地对象是否未关闭 |
| `is_ready()` | 后端当前能否接收帧（见第 8 节） |
| `close()` | 显式关闭；`Drop` 也会关闭 |

`VirtualCamera` 是 `Send` 但不是 `Sync`：可以在一个线程打开，再移到专用推流线程中写入。

## 7. 格式、分辨率、帧率与色彩空间

### 7.1 像素格式

所有输入都是**紧密排列**（tightly packed）：行与行、平面与平面之间没有填充。

| `PixelFormat` | 内存布局 | 每帧字节数（W×H） | 尺寸要求 |
| --- | --- | --- | --- |
| `Nv12` | Y 平面，随后 UV 交错平面（4:2:0） | W·H·3/2 | 宽高均为偶数 |
| `I420` | Y、U、V 三个平面（4:2:0） | W·H·3/2 | 宽高均为偶数 |
| `Yuy2` | `Y0 U Y1 V` 打包（4:2:2） | W·H·2 | 宽为偶数 |
| `Uyvy` | `U Y0 V Y1` 打包（4:2:2） | W·H·2 | 宽为偶数 |
| `P010` | 16 位小端，数据在高 10 位；Y 平面后接 UV 交错平面 | W·H·3 | 宽高均为偶数 |
| `Bgra32` | `B G R A`，**自上而下** | W·H·4 | — |
| `Rgb24` | `B G R`，**自上而下**，行尾无填充 | W·H·3 | — |

说明：

- RGB 格式一律按自上而下提供。DirectShow 的 RGB 是自下而上的 DIB，source 会在拷贝时自动翻转，并处理 RGB24 的 4 字节行对齐。
- 如需精确的平面偏移与行宽，可调用 `PixelFormat::layout(width, height)`。
- 若源数据有行填充（pitch ≠ 紧密行宽），需先在你这侧整理为紧密排列。

### 7.2 分辨率与帧率

- 宽高：1–16384，并满足上表的偶数要求。
- 帧率：`FrameRate { numerator, denominator }` 任意有理数，范围为每分钟 1 帧到 1000 fps。
  - `FrameRate::fps(30)` → 30/1
  - `FrameRate { numerator: 30000, denominator: 1001 }` → 29.97

### 7.3 色彩空间

`ColorSpace` 由矩阵（matrix）、范围（range）、原色（primaries）、传递函数（transfer）组成，**只作为元数据**传给消费端，不会改动像素。

预设：

| 预设 | 用途 |
| --- | --- |
| `ColorSpace::BT601` | SD，limited range |
| `ColorSpace::BT709` | HD，limited range |
| `ColorSpace::BT709_FULL` | HD，full range |
| `ColorSpace::BT2020` | UHD/HDR 容器，limited range |
| `ColorSpace::BT2100_PQ` | HDR10（PQ） |
| `ColorSpace::BT2100_HLG` | HLG |
| `ColorSpace::SRGB` | RGB 格式 |

不调用 `with_color` 时使用默认值 `ColorSpace::default_for(format, height)`：SD 为 BT601，HD 为 BT709，P010 为 BT2020，RGB 为 SRGB。

色彩空间还决定空闲黑帧的黑电平：limited range 为 Y=16，full range 为 Y=0。

消费端看到的形式：DirectShow 为 `VIDEOINFOHEADER2` 中的 DXVA 扩展格式位；Media Foundation 为 `MF_MT_YUV_MATRIX`、`MF_MT_VIDEO_NOMINAL_RANGE`、`MF_MT_VIDEO_PRIMARIES`、`MF_MT_TRANSFER_FUNCTION`。

## 8. 运行时行为

### 8.1 DirectShow

- **空闲**（没有生产者）时输出黑帧。对外宣告的格式按以下优先级选取：
  1. 消费端通过 `IAMStreamConfig::SetFormat` 指定的格式；
  2. 正在运行的生产者的格式；
  3. 注册时的 `default_format`；
  4. 当前用户对该摄像头最近一次成功推流的格式（历史记录）；
  5. NV12 640×480@30。

  当前格式排在最前，其后是注册的 `pixel_formats` 中其余像素格式（同尺寸）；每种像素格式先按当前帧率、再按注册的 `frame_rates` 逐一提供。接收端选定的帧率经 `requested_format()` 交给发布端，源端按该帧率节流交付，与发布端写入速率无关。
- 同一时间只允许一个 graph 连接；第二个应用连接时返回 `ERROR_BUSY`。
- 已连接的 graph 会固定其协商格式，并把它写给生产者：生产者调用 `VirtualCamera::requested_format()` 读取，可关闭后以该像素格式重开（`close()` 立即释放生产者租约）。生产者格式不同时该 graph 显示黑帧。
- 帧驱动交付：生产者每写入一帧都会通知 source，source 立即推送，不再按自己的定时器节拍取帧。
- 每台摄像头同一时间只能有一个生产者；第二个返回 `Error::DeviceInUse`。
- 生产者崩溃或退出后，消费端在短时间内回到黑帧（source 通过进程句柄和进程创建时间检测存活）。
- `is_ready()` 在打开后即为 `true`，因为 DirectShow 没有消费端在场握手。

### 8.2 Media Foundation

- 两个 managed 后端共用交付判断：按 generation + sequence 识别新帧，按协商帧率的理论时间网格限速：新帧不早于网格时刻前四分之一帧间隔时立即交付，更早到的帧直接跳过而不是延后（容忍生产者抖动、不增加延迟，网格自动对齐生产者节奏），长期速率不超过协商帧率（例如 60 FPS 写入 30 FPS 摄像头时恰好每两帧交一帧）；没有生产者帧时按网格交黑帧；生产者停滞时每秒重交一次最新帧。应用连续请求也不会超过这个节奏。
- 生产端写入并提交帧后触发新帧事件。MF source 在认证成功后创建 ring 和跨进程帧事件，经握手返回两者名字；二者使用相同的授权身份，事件使用独立的等待/触发权限。source 等待事件、状态变化或交付期限，不再每 1 ms 轮询 ring。
- 生产者格式无法转换时输出的黑帧也遵守同样的节流规则。stream 暂停后恢复会继续处理待交付请求；停止会取消待交付请求，重启后不再交付旧请求。
- 时间戳使用系统 QPC 时钟（`MFGetSystemTime`），时长等于原生帧间隔。
- 生产者断开（管道关闭）时立即回到黑帧；正常停止不会发送流结束事件，应用可以继续使用摄像头。
- 原生 NV12/I420 的摄像头还提供 YUY2。应用选定的格式经管道告知生产者，生产者用 `VirtualCamera::requested_format()` 读取（随存活检查约每 100 ms 更新），可关闭后以 `ManagedCameraManager::open(id, requested)` 重开并自己产出该格式，source 此后只复制；重开之前 source 能转换就自行转换（NV12/I420 → YUY2），不能转换则输出黑帧。
- 摄像头是 System lifetime：重启后仍然存在，直到 `unregister`。
- MF 握手已升级到 wire version 2（Welcome 424 字节）：生产端库与 source DLL 需一起升级。持久化摄像头配置仍为 version 1，无需重建已有注册。两个 managed 后端的 `requested_format()` 都只在所选格式与当前写入格式不同时返回 `Some`。
- MF 交付期限使用高精度 waitable timer，避免 Session 0 的普通毫秒超时把空闲 30 FPS 拉低到约 21 FPS。[本机 Frame Server CPU 与延迟记录](performance/2026-11-07-mf-events.md)列出了实际帧率和各负载差异，事件驱动并非所有负载都更省 CPU。

## 9. OBS Virtual Camera 与 Unity Capture

这两个后端写入**已安装**的第三方虚拟摄像头，本库不注册也不安装它们。

```rust
use virtualcam::{FrameRate, ObsOptions, VirtualCamera};

let mut camera = VirtualCamera::open_obs(ObsOptions {
    width: 1920,
    height: 1080,
    frame_rate: FrameRate::fps(30),
    force_open: false,
})?;
let frame = vec![0_u8; camera.frame_size()]; // NV12
camera.write_host(&frame)?;
```

- **OBS**：只支持 NV12。成功打开后会写入 `%APPDATA%\obs-virtualcam.txt`，使先于生产者打开摄像头的应用也能协商到正确模式。`force_open` 会跳过占用检查，只有在确认没有其他写入者时才使用。
- **Unity Capture**：`VirtualCamera::open_unity_capture(UnityCaptureOptions { .. })`，支持 RGBA8 / RGBA FP16（gamma 或 linear），可通过 `stride_pixels` 指定行填充；`capture_number` 选择设备编号。在接收端创建对象之前，`write_host` 返回 `ConsumerInactive`。

更多协议细节见 [README.md](../README.md) 和 [UPSTREAM.md](../UPSTREAM.md)。

## 10. CUDA 直写

启用 `cuda` feature 后：

```rust
// SAFETY: device_ptr 指向当前 CUDA 上下文中、长度为 frame_size() 的连续显存，
// 且产生该帧的工作已在 stream 上排在本次拷贝之前。
let status = unsafe { camera.write_cuda(device_ptr, camera.frame_size(), Some(stream)) }?;
```

- 显存数据直接 D2H 拷贝到共享内存，库内部不分配中间缓冲。
- 这是同步调用：返回时拷贝和 `cudaStreamSynchronize` 已完成，帧已经发布。
- 本库不链接任何 CUDA 库：首次调用时从应用已加载的 CUDA Runtime DLL（依次 `cudart64_13.dll`、`_12`、`_110`、`_101`，找到后固定在进程内）取 `cudaMemcpyAsync` 与 `cudaStreamSynchronize`，找不到再按系统 DLL 搜索加载；因此应用以何种方式链接 CUDA Runtime 都不会符号重复，也不绑定 CUDA 主版本。缺少 Runtime 时返回 `Error::Windows`。
- 显存中的数据必须已经是目标格式的紧密排列；不接受 CUDA array、texture 或带 pitch 的分配。

## 11. 旧版 C++ 驱动摄像头迁移

早期版本使用 C++ DirectShow 驱动（已从仓库删除）注册摄像头。这些摄像头只有在旧 DLL 仍然存在时才能工作，且与新的 Rust source 不兼容。

```powershell
& $exe legacy                     # 列出：ID、名字、旧 DLL 路径
& $exe remove-legacy <ID>         # 管理员：删除注册
& $exe register "<原名字>" ...     # 用新后端重新注册
```

迁移会改变 CLSID：应用里保存的设备选择需要重新选一次。

代码中对应 `ManagedCameraManager::legacy_cameras()` 和 `remove_legacy_camera(id)`。

## 12. 卸载

1. 每个用户删除自己的摄像头：`unregister <ID>`（DirectShow `AllUsers` 摄像头需管理员）。
2. 删除 MF source（管理员）：

   ```powershell
   & $exe uninstall-mf --other-users-cleaned
   ```

   出于安全考虑，以下情况会拒绝卸载并返回 `RegistrationDegraded`：
   - 当前用户仍有 MF 摄像头；
   - 未传 `--other-users-cleaned`。本工具无法枚举其他 Windows 用户的摄像头，只有在确认他们都已删除后才能传此参数。
3. 删除 DLL 文件。正在被 Frame Server 加载的 DLL 可能要等该服务释放后才能删除。

## 13. 错误与排查

### 13.1 常见错误

| 错误 | 原因 | 处理 |
| --- | --- | --- |
| `InvalidAppId` | AppId 为空、过长或含非法字符 | 只用 `A-Z a-z 0-9 . _ -`，长度 ≤128 |
| `InvalidCameraName` | 名字为空或过长 | 使用正常的显示名 |
| `NameConflict` | 同一范围内已有同名摄像头 | 换名字，或复用已有摄像头 |
| `CameraNotFound` | ID 不存在，或不属于当前 AppId | 检查 `list()`；或改用 `ManagedCameraManager::new()` |
| `InvalidDimensions` / `InvalidFrameRate` | 超出范围或违反奇偶要求 | 见第 7 节 |
| `UnsupportedFormat` | 该后端不支持此像素格式 | 自有摄像头使用第 7.1 节列出的格式 |
| `FormatMismatch` | MF 摄像头以原生及 offered 列表以外的格式打开 | 用 `open_registered`，或 `requested_format()` 返回的 offered mode |
| `FrameSizeMismatch` | `write_host` 长度 ≠ `frame_size()` | 按 `frame_size()` 分配缓冲 |
| `DeviceInUse` | 已有另一个生产者 | 关闭另一个生产者 |
| `BackendUnavailable(..)` | DLL 未找到、MF source 未安装、路径为网络盘等 | 检查第 4 节 |
| `RegistrationDegraded` | 注册不完整，或卸载被安全拒绝 | `unregister` 后重新注册；卸载见第 12 节 |
| `Hresult` / `Windows` | 底层系统调用失败 | 查看错误码；常见原因是权限不足（需要管理员的操作） |
| `AlreadyClosed` | 对已关闭的 camera 操作 | 重新 `open` |

### 13.2 症状排查

**在 MF 应用里找不到摄像头**
- 确认 `media_foundation_source()` 返回了路径（是否已安装）。
- Windows 显示的名字是"<名字> (Windows 虚拟摄像头)"，按名字精确匹配的代码需要考虑这个后缀。
- 确认 DLL 不在用户目录下（Local Service 无法读取）。
- 查看事件日志 `Microsoft-Windows-MF-FrameServer/Camera_FrameServer`。

**MF 摄像头一直黑屏**
- 生产者是否在运行，且 `write_host` 是否返回 `Written`？如果一直是 `ConsumerInactive`，说明没有应用真正在读取，或者生产者不是摄像头所属用户的进程。
- 生产者是否以 `open_registered` 打开（格式一致）？

**DirectShow 应用里看不到摄像头**
- 32 位应用看不到（目前只有 x64 DLL）。
- `list()` 中的 `health` 是否为 `Healthy`？DLL 是否被移动过？
- `CurrentUser` 摄像头只对注册它的 Windows 用户可见。

**DirectShow 画面黑屏但生产者在写**
- 消费端 graph 已按其他格式连接。让应用重新打开摄像头，或让生产者使用与之前相同的格式。

**FFmpeg 无法读取 P010**
- 这是 FFmpeg `dshow` 输入设备的限制，不是 source 的问题。可以改用 NV12，或换一个支持 P010 的 DirectShow 消费端。

**颜色偏差**
- 检查 `ColorSpace` 是否与实际数据一致（BT601/BT709、limited/full range），并确认消费端是否遵循这些元数据。

## 14. 测试

```powershell
# 默认测试：协议布局、ring、MF contract、MF 进程内端到端
cargo test --workspace

# DirectShow 端到端（需要 PATH 中有 ffmpeg，免管理员）
cargo test --release --test windows_dshow -- --ignored --test-threads=1

# 真机 Frame Server 端到端（需先按第 4.2 节安装 MF source）
cargo test --test windows_mf -- --ignored

# OBS / Unity 已安装驱动（需要先安装对应软件）
cargo test --all-features --test windows_installed -- --ignored --test-threads=1
```

## 15. 限制

- 仅支持 x64：没有 x86 DirectShow DLL（32 位应用看不到摄像头），也没有 ARM64 构建。
- MF 摄像头每台只有一个原生格式；改格式需要重建摄像头。
- MF 只支持当前用户访问 + System lifetime；不支持 AllUsers，也不支持 Session lifetime。
- 同一 Windows 用户下的恶意进程可以向该用户自己的摄像头推流（威胁模型只防跨用户、远程和未认证注入）。
- 多用户 / 快速用户切换场景尚未经过真机验证。
- 不做像素格式转换或缩放：消费端需要的格式应由生产者直接提供。
