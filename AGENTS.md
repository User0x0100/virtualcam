# AGENTS.md

本文件是本仓库 coding agent 的长期上下文。开始修改代码前必须先读本文件，再读任务相关源码。用户文档见 `README.md` 与 `docs/USER_GUIDE.zh-CN.md`。

如果本文件、旧会话记忆和当前代码冲突，以用户最新指令 + 当前代码 + 可验证平台事实为准，并同步修正文档。

## 1. 项目定位

`virtualcam` 是 Rust Windows virtual-camera library。

三类能力：

1. OBS Virtual Camera producer（第三方协议，NV12，3-slot queue）。
2. Unity Capture producer（第三方协议，RGBA）。
3. Managed camera（本项目自有，pure Rust）：
   - DirectShow：`crates/virtualcam-dshow` -> `virtualcam_dshow.dll`。
   - Media Foundation：`crates/virtualcam-mf` -> `virtualcam_mf.dll`。

OBS backend != managed DShow backend。

用户要求（2026-10-23）：两个 backend、多格式/色彩空间/分辨率/帧率、pure Rust、App ID 区分程序注册（不是鉴权）、性能优先、产品未定型可自由重构。

## 2. 当前事实

Workspace：

- `crates/virtualcam-protocol`：`CameraId`、`PixelFormat`/`ColorSpace`/`VideoFormat`/`WireVideoFormat`、plane layout + pitch-aware copy/black、transport v2 seqlock ring（`ring.rs`）、DShow control/names（`dshow.rs`）、MF attributes/pipe messages/names（`mf.rs`）、registry schema。只含 layout/name/ordering，不持有 handle。
- `crates/virtualcam-dshow`：DirectShow source filter cdylib（filter/pin/IAMStreamConfig/IKsPropertySet/allocator/push thread）。
- `crates/virtualcam-mf`：MF source cdylib（IMFActivate + FTM、IMFMediaSourceEx、IMFMediaStream2、named-pipe rendezvous、owner-only ring）。
- root `virtualcam`：
  - `src/managed/mod.rs`：`AppId`、`ManagedCameraRegistration::{DirectShow, MediaFoundation}`、`ManagedCameraManager`（`for_app` scoping、register/unregister/rename/list/get/open/open_registered、MF source install/uninstall、legacy）。
  - `src/managed/dshow.rs`：HKCU/HKLM COM + category instance + 系统 `IAMFilterData` FilterData。
  - `src/managed/mf.rs`：`MFCreateVirtualCamera`（CurrentUser + System）。
  - `src/managed/metadata.rs`：`{HKCU|HKLM}\Software\VirtualCamRs\Cameras\{ID}` records；`Global\VirtualCamRs-Management` mutex。
  - `src/managed/legacy.rs`：已删除的 C++ driver 留下的 camera 列出/删除。
  - `src/backend/windows/managed_dshow/`、`managed_mf.rs`：两个 managed producer。
  - `src/backend/windows/obs.rs`、`queue_writer.rs`、`protocol/{frame_queue,publication}.rs`：OBS producer（3-slot queue 仅 OBS 使用）。
  - `src/backend/windows/unity_capture.rs`、`src/platform/`。
- `examples/managed_camera.rs`：命令行管理工具；`examples/generated_test_video.rs`：OBS/Unity 测试图案。
- `tests/`：`public_api`、`windows_installed`（OBS/Unity，ignored）、`windows_dshow`（FFmpeg 端到端，ignored）、`windows_mf`（真机 Frame Server，ignored，需先 install-mf）。

`CameraType` 是 `#[non_exhaustive]`，含 `Obs`、`UnityCapture`、`ManagedDirectShow`、`ManagedMediaFoundation`。`VirtualCamera` 是 `Send`、非 `Sync`。

C++ driver（`native/managed-virtualcam`、libdshowcapture、CMake）已删除；旧 camera 只能通过 `legacy_cameras`/`remove_legacy_camera` 清理。

验证状态（2026-10-24）：DShow FFmpeg 端到端逐字节通过；MF contract + in-process `IMFSourceReader` 通过；真机 Frame Server（Local Service / Session 0）单用户逐字节通过。未验证：多用户 / Fast User Switching。

## 3. Backend 模型

```text
DirectShow
-----------
producer owns runtime format/control
session-local transport (Local\)
virtualcam_dshow.dll

Media Foundation
----------------
source owns media capability/control
cross-session transport (Global\, same-session fallback Local\)
virtualcam_mf.dll
FrameServer / Local Service / Session 0
```

两个 backend **不是同一个 transport lifecycle 换个 consumer API**。

## 4. Common protocol 只共享 frame ring

`virtualcam-protocol` 两个 backend 共享：

- ring layout、slot/sequence、seqlock。
- format/payload validation、plane layout、pitch-aware copy/black。
- publication ordering（`sync.rs`）。

不共享：

- control header（DShow `DshowControl` vs MF pipe Hello/Welcome）。
- who selects format。
- object namespace。
- ownership / readiness / registration semantics。

## 5. Cross-process memory ordering 是协议

protocol contract = layout + ordering。不要只实现 `#[repr(C)]` structs 就认为兼容完成。

跨进程 publish/observe 字段只经 SeqCst atomic helper（等价 Windows `Interlocked*`）：managed 用 `virtualcam-protocol/src/sync.rs`，OBS 用 `src/backend/windows/protocol/publication.rs`。必须考虑 ARM64，不依赖 x86 强内存顺序。

Transport v2 ring（VCR2）：

```text
4 KiB header page（256-byte header）+ 4 KiB-aligned slots（64-byte slot header）
writer: slot lock = 2s+1 -> write payload -> slot lock = 2s -> publish latest_seq
reader: observe latest_seq -> read slot lock (even, matches) -> copy -> re-check lock
        mismatch -> retry (≤4) -> Contended；never accept a torn frame
writer 从不等待 reader；每个 producer session 用新 ring（generation 命名）
```

OBS-compatible 3-slot queue（仅 OBS）没有 per-slot generation：writer 绕回 3 帧会覆盖 reader 已选 slot。oracle 在 `src/backend/windows/queue_writer.rs`（`lapping_writer_reuses_the_selected_slot_without_a_generation_marker`；ignored stress 2026-10-22 实测 26,376 次读取中 74 次 torn）。这是 OBS 上游 ABI，不改；managed 已因此改用 v2。

## 6. DShow transport

Namespace（`virtualcam-protocol/src/dshow.rs`）：

```text
Local\VirtualCamRs2-{ID}-Lease            event；存在即 single-writer lease，仅 producer 持有
Local\VirtualCamRs2-{ID}-Control          128-byte DshowControl（magic "VCD2"，seqlock'd）
Local\VirtualCamRs2-{ID}-Ring-{GEN:016X}  per-session VCR2 ring
Local\VirtualCamRs2-{ID}-Frame-{GEN:016X} per-session auto-reset event，producer 每帧 commit 后 SetEvent
Local\VirtualCamRs2-{ID}-Consumer        event；存在即 single-consumer lease，仅已连接的 graph 持有
Local\VirtualCamRs2-{ID}-Request         64-byte DshowRequest（magic "VCQ2"，seqlock'd），consumer -> producer
```

`DshowControl`：format（WireVideoFormat）、ring generation/slots/size、producer pid + process start time。
`DshowRequest`：state（NONE/CONNECTED）+ 已连接 graph 的 format；只有 consumer lease 持有者写。

Lifecycle：

- one producer（第二个 -> `DeviceInUse`）；`close()` 即释放 producer lease（不必等 drop），便于立即以新格式重开。
- one consumer graph（2026-10-24 用户要求去掉多消费者）：output pin `Connect` 时取 consumer lease，第二个 graph `Connect` 返回 `HRESULT_FROM_WIN32(ERROR_BUSY)`；`Disconnect`/filter 释放时归还并把 request 置 NONE。
- producer decides runtime format；registration 存可选 `default_format` 与 `pixel_formats`（offered 列表）。
- advertised：当前格式（SetFormat > live producer > registered default > per-user last format（`HKCU\Software\VirtualCamRs\History\{ID}\LastFormat`）> NV12 640x480@30）在前，其后是其余 registered pixel formats（同尺寸/帧率）。显式注册的默认格式优先于历史（2026-10-24 改）；`pixel_formats` 为空的旧注册只宣告当前格式。
- connected graph mode fixed；连接成功后 consumer 把格式写入 `DshowRequest`，producer 经 `VirtualCamera::requested_format()` 读到后自行决定是否以该 pixel format 重开（新 generation）。格式不同的 producer 在该 graph 上显示 black。
- push thread 帧驱动：live producer 服务当前连接格式且有 frame event 时，等待 frame event，新 sequence 立即交付（速率上限约 0.75 interval，停滞时每 2 interval 重复最后一帧）；无 producer/格式不符/旧 producer 无 frame event 时，按 negotiated interval 定时器节拍发黑帧/帧。实测（`directshow_delivery_latency`，FFmpeg 低延迟参数，1080p30）中位延迟 31.2 ms -> 3.5 ms。
- producer liveness = 打开 producer 进程（SYNCHRONIZE）+ 比对 creation time；死亡 -> black。
- source 不做格式转换（像素格式切换由 producer 完成）；RGB 自上而下输入，source 翻转为 bottom-up DIB 并处理 RGB24 DWORD 行对齐。

## 7. DShow source / registration contract

实现接口：IClassFactory、IBaseFilter/IMediaFilter/IPersist（raw HRESULT，paused `GetState` 返回 `VFW_S_CANT_CUE`）、IPin、IAMStreamConfig、IKsPropertySet、IEnumPins、IEnumMediaTypes、IMemInputPin downstream query、IMemAllocator negotiation（downstream 优先，`CLSID_MemoryAllocator` fallback）、Receive（尊重 `S_FALSE`）。

Filter strong-own pin，pin weak-ref filter。media types 同时提供 VIH 与 VIH2（DXVA colorimetry bits），每个 offered format 各一组；`GetNumberOfCapabilities`/`GetStreamCaps` 枚举全部 offered format；`SetFormat`/`Connect` 只接受 offered pixel format。push thread 见 §6，无 catch-up burst。

Registration contract：

```text
category = CLSID_VideoInputDeviceCategory
merit    = MERIT_DO_NOT_USE
REGFILTER2 version = 1, cPins = 1
pin: rendered=FALSE output=TRUE zero=FALSE many=FALSE
     media types = MEDIATYPE_Video / 每个 managed PixelFormat 的 subtype
FilterData 由系统 IAMFilterData::CreateFilterData 生成，不手工构造
runtime output pin IKsPropertySet: AMPROPERTY_PIN_CATEGORY -> PIN_CATEGORY_CAPTURE
scope: CurrentUser = HKCU Classes（免提权）；AllUsers = HKLM（需提权）
```

manager 调用 COM 时尊重调用线程现有 apartment（`src/managed/com.rs` `ComApartment`）；只有自己成功初始化才 `CoUninitialize`。

COM registration：两个 DLL 的 InprocServer32 `ThreadingModel = Both`。`Both` 只是 registration model，不替代 MF FTM/agility。

两个 source DLL 的 `DllMain` 必须接近空：禁止 COM、registry、LoadLibrary、IPC、worker startup/join、blocking wait、对象构造。

Rust COM/FFI boundary 不传播 panic：export / class factory / COM method 最外层 `catch_unwind`（`guard`/`guard_hr`），panic -> stable HRESULT；外部输入和共享内存处理禁止 unwrap/expect/assert；不用 `panic=abort`。

x86：目前不支持。若以后加入，必须单独构建 x86 DLL、注册到 WOW64 view、x64+x86 作为一个 logical transaction（失败回滚、list/get 识别 partial），x86 helper 不得再次获取 management mutex，不由 x64 手工构造 32-bit FilterData。

## 8. MF format model 与 DShow 不同

MF source 在 producer 不存在时也必须能被 Frame Server 枚举 media types，所以：

**每台 MF camera 一个持久化 native mode**（任一 managed `PixelFormat` + width/height + fps num/den + `ColorSpace`），以 `WireVideoFormat` blob 存于 vendor attribute。

- source 只 advertise 这个 mode；producer open 必须匹配（否则 `FormatMismatch`）。
- 禁止 producer 改变 runtime mode、dynamic capability mutation、implicit resize/fps convert。
- native mode immutable。修改 = recreate camera，不能只更新 metadata/attributes，否则与已激活 FrameServer source 的旧 descriptor split-brain。
- future multi-mode 由 source/client selection 驱动，再通知 producer。

## 9. MF source host

Frame Server custom source：

- Local Service。
- Session 0。

不能用：

```text
Local\VirtualCamRs-*
```

作为 production MF IPC。

实现：

```text
source 在 producer 认证成功后创建 Global\VirtualCamRs-MF-{GenerationNonce:032X}
（同 session 时 fallback Local\），owner-only DACL，经 pipe Welcome 返回名字
```

`GenerationNonce` 由 source 生成（高熵），rendezvous id 只是 routing，不是 security capability。

Named-object squatting hard rule：

- same-type pre-existing object / `ERROR_ALREADY_EXISTS`：绝不继续使用。
- different-type object already occupies same named-object namespace / `ERROR_INVALID_HANDLE`：同样视为 collision/attack。
- regenerate fresh source-owned nonce 或 fail activation。
- mapping/event/semaphore/mutex 都做 pre-creation collision tests。

MF source native architecture v1 scope：**x64-only**。ARM64 shared-memory ordering 仍保持正确，但不等于已经支持 ARM64 MF deployment。未来支持 Windows on ARM 时单独产出/register native ARM64 `virtualcam_mf.dll` 并做真实 FrameServer activation/IPC tests。

## 10. MF CurrentUser-only v1

MF v1 收敛为：

```text
access   = CurrentUser
lifetime = System
```

不要支持 AllUsers，也不要支持 Session lifetime。

Session lifetime 未来必须用长期持有 `IMFVirtualCamera` 的 RAII handle 单独设计；当前 register/list/get/unregister manager API 不承担 session-object ownership。

不要混淆 access 与 lifetime。

## 11. MF per-user metadata / source uninstall

MF CurrentUser camera records 必须 user-scoped：

```text
HKCU\Software\VirtualCamRs\Cameras\{ID}
```

或等价 per-user protected storage。

DirectShow machine-wide metadata 可以继续 HKLM。

Local Service source 的 HKCU != camera owner HKCU。

因此 MF source runtime routing不能“直接读当前 HKCU”。

v1 进一步禁止 source 侧 `symbolic-link -> owner HKCU` fallback：source 不能从 untrusted attribute 得到可信 owner SID。HKCU 只用于当前用户 manager bookkeeping/reconcile；runtime config 缺失时 fail closed，由 manager repair/recreate。

因为 MF v1 是 CurrentUser + System lifetime，machine-wide source uninstall 必须先清理或证明可安全保留所有仍指向本 source CLSID 的 persistent virtual cameras。两用户/Fast User Switching 下必须验证 admin cleanup 能找到本 source 的所有 camera；无法证明完整时 installer fail safe，不能先 unregister/delete source DLL。

## 12. MF fixed CLSID + IMFActivate

固定 `virtualcam_mf.dll` COM CLSID `{872FEE29-230E-41D8-83A7-2A383D956486}`（`virtualcam_protocol::mf::SOURCE_CLSID`）。

Factory：

```text
MfSourceActivate
├── IMFActivate
└── IMFAttributes
       |
       v
MfMediaSource
├── IMFMediaSourceEx
├── IMFGetService
├── IKsControl
└── IMFSampleAllocatorControl   # preferred, not mandatory for correctness
       |
       v
MfMediaStream
└── IMFMediaStream2
```

若 activation/source 分离，activation attributes 可以复制到 source 初始化 store；但 `GetSourceAttributes()` 必须返回 source 持有的同一个 attribute-store reference，不能每次 clone。`GetStreamAttributes()` 同理返回 stream 自己的同一个 store reference。

按 Frame Server 官方 contract：

- `DetachObject -> E_NOTIMPL`
- `ShutdownObject -> E_NOTIMPL`

不要照搬某个 sample 版本中与正式 contract 不一致的返回值。

DLL：

- `DllGetClassObject`
- `DllCanUnloadNow`
- 正确 module/object lock。
- live event queue / allocator / COM object / future async work 存在时不得过早 unload。

ThreadingModel=Both 不代表 object 可以无锁，也不代表对象天然满足 Media Foundation 的 cross-apartment/cross-process marshaling 要求。

`MfSourceActivate` / `MfMediaSource` 必须是 free-threaded/neutral：当前聚合 Free-Threaded Marshaler（`crates/virtualcam-mf/src/ftm.rs`，raw-vtable 转发 `IMarshal`）并实现 `IAgileObject`；FrameServer cross-process activation 已真机验证。不要假设 `windows` crate 普通 `#[implement]` 自动提供 FTM。

同一个 `MfSourceActivate` 的重复 `ActivateObject` 返回同一个缓存 source（直到它 Shutdown），不会创建两个互不关联、各自拥有 transport 的 source instance。

MF source 同样必须有 panic boundary：所有 exports、factory、IMFActivate/source/stream/IKsControl/allocator callbacks 最外层 `catch_unwind`，panic 转 HRESULT，不能让 recoverable Rust panic 直接终止 FrameServer。

## 13. MF vendor attributes 是正式主路由

Microsoft VirtualCamera sample 已使用 custom `IMFVirtualCamera::Set*` 属性向 source activation传配置。

本项目使用**配置型** attributes（`virtualcam-protocol/src/mf.rs`）：

```text
ATTR_CAMERA_ID
ATTR_RENDEZVOUS_ID         # high-entropy routing id, not authorization
ATTR_EXPECTED_OWNER_SID    # assertion only, never direct authorization
ATTR_PROTOCOL_VERSION
ATTR_NATIVE_FORMAT         # WireVideoFormat blob
```

Rendezvous pipe：`\\.\pipe\VirtualCamRs-MF-{RID}`；Hello 64 B / Welcome 232 B fixed-size messages；handshake deadline 2 s。

不要把 `OWNER_SID` vendor attribute 当 production authorization input。所有 custom attributes 都是 untrusted configuration：必须 bounds/version/overflow validation，不能直接决定 DACL。

producer authorization v1 优先验证 named-pipe rendezvous。

Pipe hard constraints：

- explicit DACL；禁止 NULL/default named-pipe security descriptor。
- `PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE`。
- `PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS`。
- minimum client rights only；不要给 client `FILE_CREATE_PIPE_INSTANCE`/server-instance rights。
- same-user expected SID 可用于缩小 pipe DACL，但只是 defense-in-depth，不替代 token authentication。

Authentication order：

```text
ConnectNamedPipe completion
 -> bounded handshake ReadFile completion
 -> validate magic/version/fixed size
 -> ImpersonateNamedPipeClient
 -> OpenThreadToken / TokenUser
 -> reject anonymous/insufficient identity
 -> compare actual SID with EXPECTED_OWNER_SID assertion
 -> RevertToSelf on every path
```

`ImpersonateNamedPipeClient` 依赖最后一条已读取消息的 client context，所以禁止 connect 后未 read 就 impersonate。不要信 client 自报 PID/SID。

Rendezvous worker：

- overlapped Connect/Read；不用 `PIPE_NOWAIT` 模拟 async。
- bounded bytes + handshake deadline。
- stalled/malformed client 断开，不得永久 pin worker。
- Source Shutdown cancel/close pending pipe I/O，worker exit/join 在 state lock 外完成。
- worker/pending pipe I/O 存在时 `DllCanUnloadNow -> S_FALSE`。
- worker 不在 `DllMain` 启停。

`EXPECTED_OWNER_SID` 只是 assertion，不是 credential，不能直接决定最终 transport authorization。认证成功后 source 再生成 fresh `GenerationNonce` 并返回/duplicate transport information。若 named-pipe spike 不成立，再考虑更复杂 broker/handle transfer。

v1 threat model 只承诺阻止 cross-user / remote / unauthenticated injection；不承诺隔离同一个 Windows user account 下的恶意进程。

仍测试：

- reopen。
- System lifetime reboot。
- manager releases/shuts down its `IMFVirtualCamera` object but System camera remains registered until Remove。
- multiple users。
- malicious/oversized/malformed attributes。

如果必需 attributes 缺失/越界/版本不支持，`ActivateObject` fail closed（`SourceConfig::from_attributes` 返回错误）；不从任意用户 HKCU 猜 runtime routing。symbolic link 只供 manager 做 system identity/reconcile/repair。

## 14. MF creation tuple

相同 `MFCreateVirtualCamera` parameters 可 reopen同一 system camera。

因此 MF v1：

- fixed source CLSID。
- CurrentUser。
- friendly name 在本项目同用户 scope 唯一。
- same tuple => existing/conflict。
- different ManagedCameraId 不允许伪装成第二台相同 system camera。

Start 后保存 symbolic link。

## 15. MF transport security / ACL

不要写死 `SYSTEM + owner`，也不要信 vendor `OWNER_SID`。

producer authentication / owner binding 已实现并真机验证（单用户）。real identity 来自 Windows token/impersonation/handle semantics，不来自 custom attributes。

DACL/access mask 按 object type 分开：

```text
file mapping handle/view:
  FILE_MAP_READ / FILE_MAP_WRITE as applicable
  + only necessary standard rights
  # file-mapping object 不支持/不依赖 SYNCHRONIZE

event:
  SYNCHRONIZE + EVENT_MODIFY_STATE

semaphore:
  SYNCHRONIZE + SEMAPHORE_MODIFY_STATE

mutex if used:
  SYNCHRONIZE + MUTEX_MODIFY_STATE
```

保持：source host 获得自身需要权限；Everyone 无 frame-write；unrelated users denied；maintenance principal only if actually needed。

## 16. MF RequestSample contract

RequestSample 必须 request-driven，不主动按 DShow pacing thread push。

**v1 同步优先。不要未经测量就实现 FIFO pending queue / async work queue。**

默认路径：

```text
RequestSample
 -> validate running
 -> allocator AllocateSample
 -> lock destination
 -> copy latest frame or black
 -> SetSampleTime/Duration
 -> echo MFSampleExtension_Token
 -> Queue MEMediaSample
 -> return
```

要求：

- non-null token 在调用期间保持有效，并原样挂到 output sample。
- failure path 不泄漏 sample/buffer/token reference。
- source/stream stopped 时 `RequestSample -> MF_E_MEDIA_SOURCE_WRONGSTATE`。
- source/stream shutdown 时 `RequestSample -> MF_E_SHUTDOWN`。
- 不用 generic `E_FAIL`/近似 HRESULT 替代 stopped-state contract。
- no producer 不等待，直接 black。

RequestSample error signaling：

- 调用内检测到 active transport corruption / allocator / frame validation failure -> 直接返回 HRESULT，不同时 queue `MEError`。
- 没有对应 RequestSample 的后台/控制路径发现 fatal error -> 才 queue `MEError`。
- no producer/stale producer -> black fallback，不是 fatal error。

RequestSample latency 尚未专门测量；真机 640x360@30 端到端正常。

只有同步路径确实阻塞 Frame Server/超过预算时，才单独引入 async：届时才需要 FIFO、Start generation、Stop/Shutdown cancellation 和 stale-work protection。

## 17. MF allocator + pitch copy

`IMFSampleAllocatorControl` 是 preferred optimization，不是 MF source 正确性的 mandatory interface。

provided allocator contract：

```text
SetDefaultAllocator
 -> QI IMFVideoSampleAllocator
 -> only replace/store while stream not RUNNING

Start(selected media type)
 -> InitializeSampleAllocator / Ex

RequestSample
 -> AllocateSample

Stop / media-type replacement / Shutdown
 -> UninitializeSampleAllocator as required
 -> release/rebuild media-type-bound allocator state
```

不能 retain 后直接 AllocateSample，否则可能得到 `MF_E_NOT_INITIALIZED`。

如果 pipeline 没提供可用 allocator，fallback 到标准：

```text
MFCreateSample
+
MFCreate2DMediaBuffer / 合适的官方 MF buffer API
```

最小闭环不能被 allocator-control callback 行为阻断。

destination buffer 不保证 tightly packed。

锁定 fallback：

1. IMF2DBuffer2::Lock2DSize
2. IMF2DBuffer::Lock2D
3. IMFMediaBuffer::Lock

shared queue NV12：

- Y width bytes/row。
- UV width bytes/row。

destination：

- honor pitch。
- pitch 可为负。
- validate bufferStart/bufferLength。
- Y row-by-row。
- UV row-by-row。
- no whole-frame memcpy。
- error path也必须 unlock。

black：

- Y=0x10。
- UV=0x80。
- 同样按 pitch 写。

## 18. MF timestamps

output sample：

- sample time 必填。
- duration 必填。
- QPC based。
- 使用 `MFGetSystemTime()`。
- duration = native interval。

producer timestamps只作 freshness/latency，不作 presentation clock。

## 19. MF state / attributes / event contract

Source：

- CreatePresentationDescriptor。
- GetCharacteristics -> `MFMEDIASOURCE_IS_LIVE`。
- Start。
- Stop。
- Pause。
- Shutdown。
- source/stream attributes。
- event queues。
- reentrancy。

Stream attributes 至少验证：

- `MF_DEVICESTREAM_STREAM_CATEGORY = PINNAME_VIDEO_CAPTURE`
- `MF_DEVICESTREAM_STREAM_ID = 0`
- `MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES = MFFrameSourceTypes_Color`
- `MF_DEVICESTREAM_FRAMESERVER_SHARED = 1`，若目标 OS/SDK contract 支持并实测正确。

Camera Profile：当前**不发布** profile collection（真机 Frame Server 可正常枚举/读取）。若发布，必须包含 `KSCAMERAPROFILE_Legacy`，且 filter 必须与本 camera 的真实 native mode 相容；不要直接复制 sample 的固定 30/60fps filter。

Stream：

- RequestSample。
- Set/GetStreamState。
- Shutdown。

不要把 source queue 与 stream queue 之间的唯一全局事件顺序当 ABI；Microsoft 文档/sample 在顺序细节上并不完全一致。

必须保证事件/状态集合：

- first/new selected stream -> `MENewStream`。
- known stream restart/update -> `MEUpdatedStream`。
- stream running -> `MEStreamStarted`。
- source started -> `MESourceStarted`。
- stream stopped -> `MEStreamStopped`。
- source stopped -> `MESourceStopped`。
- normal Stop、producer absence、temporary transport failure 不发送 `MEEndOfStream`/`MEEndOfPresentation`；无 producer 继续 black。

Start 参数：

- time format 只接受 null/GUID_NULL。
- live non-seek v1 接受 `VT_EMPTY`，兼容时接受 `VT_I8 == 0`。
- non-zero `VT_I8` 作为 seek 拒绝。
- unsupported variant type 拒绝。

Event queue locking：

- `GetEvent()` 可能无限阻塞，禁止持 source/stream state mutex 调用。
- 锁内只 check shutdown + clone/AddRef event queue pointer，解锁后调用 `GetEvent()`。
- Source/Stream 两套 queue 都遵守。

IMFMediaStream2 state：

- RUNNING -> RequestSample 可产出 sample。
- STOPPED -> RequestSample 必须 `MF_E_MEDIA_SOURCE_WRONGSTATE`。
- `SetStreamState(PAUSED)` 被接受并记录；RequestSample 只在 RUNNING 产出（PAUSED 同样 `MF_E_MEDIA_SOURCE_WRONGSTATE`）。RUNNING 只能经 source Start 进入。
- source-level Pause 返回 `MF_E_INVALID_STATE_TRANSITION`。

Source/Stream ownership 与锁顺序：

- Source 可能 strong-own Stream，Stream 为 `GetMediaSource()` strong-own parent Source，必须由 Shutdown 显式拆环。
- `Source::Shutdown` 先 state->Shutdown，再 shutdown source event queue，再逐个 `Stream::Shutdown`；Stream shutdown 必须 reset parent、event queue、allocator/media type/descriptors/transport，最后 Source release stream list。
- 默认双对象调用方向只允许 Source -> Stream。
- Stream hot path 持 stream lock 时不得调用需要 source lock 的方法。
- `GetMediaSource()` 只 copy/AddRef parent interface，不回调 source stateful method。
- 禁止形成 Source-lock -> Stream-lock 与 Stream-lock -> Source-lock 两种路径并存的 ABBA deadlock。

Unsupported stubs：

- `IMFGetService::GetService` 无 service -> `MF_E_UNSUPPORTED_SERVICE`。
- unsupported `IKsControl::KsProperty/KsMethod/KsEvent` -> `HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND)`。
- CPU-only `SetD3DManager` -> `E_NOTIMPL`。

进入 Shutdown 后，除 `IUnknown` 基础行为外，公开 MF methods 按 contract 返回 `MF_E_SHUTDOWN`，不得触碰已释放 event queue/attributes/allocator/transport；使用统一 `check_shutdown()` helper，但保持各 interface 对参数检查与 shutdown 检查的可观察 HRESULT 顺序。

Seek unsupported。

## 20. MF producer attach

实现为 lazy attach，不 keepalive：

- producer `open` 不启动 camera，也不调用 `GetMediaSource`。
- producer 每 250 ms 尝试连接 rendezvous pipe；source 只在被 client Start 后才 listen。
- 未连上时 `write_host -> WriteStatus::ConsumerInactive`、`is_ready() == false`。
- 连上后 PeekNamedPipe 每 100 ms 检测 liveness；pipe 关闭 -> source 回 black、producer 重新等待。

禁止用 exclusive `MFCreateDeviceSource` 作为常驻 keepalive，避免锁掉其他 camera clients。

## 21. MF lifecycle

Configured -> Started -> Stopped -> Removed。

Shutdown 是 object resource lifecycle，不等于 permanent unregister。

Permanent delete 必须 Remove。

v1 only tests System lifetime。Session lifetime仅作为 future RAII-handle design 的平台事实，不进入当前 manager contract。

## 22. 旧 C++ camera 清理 / source DLL 升级

C++ driver 已删除。它注册的 camera（`HKLM\SOFTWARE\VirtualCamRs\ManagedCameras\{CLSID}`）只在旧 DLL 仍存在时可用：

```text
legacy_cameras()            列出 id/name/旧 DLL 路径
remove_legacy_camera(id)    elevated；删除 category instance + COM class + record
register(...)               重建（CLSID 改变，breaking migration）
```

用户机器上的 "iPhone Camera" 来自其他项目（`iphone-usb-webcam`），不要删除。

source DLL update 优先采用版本化文件名 + registration 切换：新进程加载新 DLL，仍持有旧 DLL 的进程继续运行；确认不再被使用后再清理文件。不要覆盖正在被 FrameServer/第三方进程加载的 DLL。

## 23. MF rename

MF rename = recreate transaction。

不保证 symbolic link不变。

API最终宜返回新 `ManagedCameraInfo`。

## 24. Performance

禁止：

- per-frame registry。
- per-frame kernel reopen。
- per-frame Vec。
- hidden conversion。
- whole-frame copy到 pitched buffer。
- unnecessary staging。
- catch-up burst。

CUDA producer仍直接 D2H到 shared slot。CUDA Runtime 不在链接期绑定：运行时从应用已加载的 `cudart64_*.dll` 解析 `cudaMemcpyAsync`/`cudaStreamSynchronize`（固定模块，找不到再按系统搜索加载），避免与自行链接 Runtime 的应用符号重复，也不锁定 CUDA 主版本。

## 25. 未完成工作

- 多用户 / Fast User Switching 下 MF 隔离与 source uninstall cleanup 的真机验证。
- x86 DirectShow DLL（见 §7）；ARM64 构建。
- `Auto` backend 选择 / 更高层统一 API。
- MF multi-mode（§8 future）。
- 若需要：RequestSample latency 超预算时才引入 async work queue（§16）。

## 26. API 不要伪统一

不要强迫两个 backend registration语义一样。

当前 API：

```rust
ManagedCameraRegistration::DirectShow(DirectShowRegistration { name, scope, default_format, pixel_formats })
ManagedCameraRegistration::MediaFoundation(MediaFoundationRegistration { name, native_format })
```

DShow stream format在 producer open时决定。

MF v1 native stream在 registration时决定；access 固定 CurrentUser，lifetime 固定 System，不暴露 lifetime 参数。

## 27. 修改工作流

每次：

1. git status。
2. 读 AGENTS。
3. 读当前代码。
4. 保护 user uncommitted changes。
5. 小步改。
6. 测试。
7. review diff。
8. 更新 docs（README、CHANGELOG、`docs/USER_GUIDE.zh-CN.md`、本文件）。
9. 按阶段分别 commit（D058）。

禁止：

- reset --hard。
- 无关 checkout。
- 删除未知文件。
- 覆盖用户变更。

## 28. 决策记录

### D001
~~DShow rewrite期间不改其 transport ABI。~~ 被用户 2026-10-23 "性能优先、可自由重构" 取代，见 D054。

### D002
Common只共享 frame queue/protocol primitives，不强制共享 control plane。

### D003
DShow producer owns runtime format。

### D004
MF source owns media capability/control。

### D005
MF v1 每台 camera一个 persistent native NV12 mode。

### D006
ManagedCameraId backend-neutral。

### D007
DShow/MF分别 Rust cdylib。

### D008
MF fixed CLSID -> IMFActivate。

### D009
MF vendor attributes为primary routing。

### D010
MF CurrentUser-only v1。

### D011
MF per-user manager metadata不依赖 service HKCU。

### D012
MF source-owned cross-session transport先 spike再冻结。

### D013
ACL = actual source principal + owner，禁止 Everyone write。

### D014
MF RequestSample v1 同步优先；token echo 必须正确，async 只有 latency gate 证明必要才加入。

### D015
MF destination copy必须 pitch-aware。

### D016
MF sample clock使用 MFGetSystemTime/QPC。

### D017
MF producer-first由 GetMediaSource + actual shared-client Start/read spike决定。

### D018
cross-process memory ordering是协议的一部分，Windows v1 优先统一 Interlocked publication helper。

### D019
（managed 已改用 v2 ring，见 D054；本条仅约束 OBS 上游 ABI）当前 3-slot queue torn-frame 风险先 stress，不在 DShow rewrite 中静默改 ABI。

### D020
旧 DShow registration在删 C++前必须 cleanup/migrate。

### D021
DShow x86/x64 support scope 必须明确；若支持，两套 DLL/registry view 都要测试。

### D022
Rust COM source DLL panic 必须在 FFI/COM boundary 被捕获并转换 HRESULT。

### D023
MF native mode immutable；修改 mode = recreate。

### D024
MF correctness contract 是 required event set + state transitions，不把跨 source/stream queue 的唯一全局事件顺序当 ABI。

### D025
MF `GetSourceAttributes`/`GetStreamAttributes` 返回各自持有的 same-store reference；blocking `GetEvent` 必须在 state lock 外。

### D026
MfSourceActivate/MfMediaSource 必须验证 free-threaded/FTM-or-equivalent marshaling，不把 ThreadingModel=Both 当 agility。

### D027
stopped `RequestSample` 必须返回 `MF_E_MEDIA_SOURCE_WRONGSTATE`；shutdown 返回 `MF_E_SHUTDOWN`。

### D028
IMFSampleAllocatorControl 是 preferred optimization；标准 MF sample/buffer allocation fallback 必须可工作。

### D029
DShow registration preserves `CLSID_VideoInputDeviceCategory` + `MERIT_DO_NOT_USE`; output pin preserves `PIN_CATEGORY_CAPTURE`. x86 uses architecture-matched FilterMapper helper if supported.

### D030
MF Source/Stream circular references must be explicitly broken by Shutdown; default lock hierarchy is Source -> Stream only.

### D031
Provided MF allocator must follow Initialize -> Allocate -> Uninitialize/rebuild lifecycle; retain-only is invalid.

### D032
MF data-path errors use one signaling path: RequestSample HRESULT or background `MEError`, never both for the same failure.

### D033
If camera profiles are published, `KSCAMERAPROFILE_Legacy` is required and filters must match the camera's real native mode.

### D034
Repeated `IMFActivate::ActivateObject` returns the cached source; one activation object must not accidentally create multiple unrelated source/transport instances.

### D035
Unsupported MF services/controls return explicit contract HRESULTs; post-Shutdown methods return `MF_E_SHUTDOWN` without touching released state.

### D036
MF vendor attributes are untrusted configuration, never producer identity/authorization. `OWNER_SID` is not a production authorization attribute.

### D037
MF production transport requires an authenticated producer-binding design based on Windows token/impersonation/handle semantics ; HKCU is manager bookkeeping only.

### D038
Source-owned Global named objects use a source-generated generation nonce and reject same-type `ERROR_ALREADY_EXISTS` and cross-type `ERROR_INVALID_HANDLE` namespace collisions; never reuse pre-existing objects.

### D039
Transport access masks are object-type-specific; file mappings do not use a generic `SYNCHRONIZE` mask.

### D040
DShow registration parity includes full REGFILTER2 v1 pin/media-type metadata, not only category/merit/pin-category.

### D041
DShow x64+x86 management is one logical transaction with rollback and partial-registration detection.

### D042
DShow `IAMStreamConfig` / QueryAccept / media-type enumeration behavior is parity-tested for idle and producer modes.

### D043
MF v1 producer authentication bootstrap prefers named-pipe impersonation: expected owner is an assertion, actual identity comes from client token SID; authentication is lazy and never blocks source Start.

### D044
MF v1 is x64-only + CurrentUser + System lifetime. Session lifetime requires a future RAII `IMFVirtualCamera` owner; ARM64 requires a future native source build and real-device tests.

### D045
Both source DLLs register `ThreadingModel=Both`, but MF still requires FTM/agility. `DllMain` stays minimal and does no COM/registry/IPC/blocking work.

### D046
DShow parent transaction coordinator owns the management mutex; x86 helper never reacquires the same transaction mutex.

### D047
MF rendezvous pipe uses explicit DACL + `FILE_FLAG_FIRST_PIPE_INSTANCE` + `PIPE_REJECT_REMOTE_CLIENTS` + message mode + overlapped I/O. Default pipe security is forbidden.

### D048
MF authentication reads and validates a bounded handshake before `ImpersonateNamedPipeClient`; actual identity comes from the impersonation token, anonymous/insufficient identity is rejected, and every impersonated path calls `RevertToSelf`.

### D049
MF rendezvous worker is cancellable: stalled clients have byte/time budgets, Shutdown cancels pending I/O and waits outside state locks, and live worker/I/O forces `DllCanUnloadNow=S_FALSE`.

### D050
MF v1 threat model blocks cross-user/remote/unauthenticated injection but does not isolate a malicious process already running as the same Windows user.

### D051
CurrentUser/System cameras must be cleaned before source uninstall; if multi-user cleanup cannot be proven complete, installer fails safe without deleting source registration/DLL.

### D052
~~first version NV12-only~~ 被 D053 取代。

### D053
Managed cameras 支持 NV12/I420/YUY2/UYVY/P010/BGRA32/RGB24；`ColorSpace` 只作 metadata（VIH2 DXVA bits / `MF_MT_*`），source 不做转换，按 pitch 逐行 copy。

### D054
Managed transport v2：per-slot seqlock ring（VCR2），writer 不等待、reader 不接受 torn frame。DShow namespace `Local\VirtualCamRs2-{ID}-{Lease,Control,Ring-{GEN}}`；producer liveness = process handle + creation time。

### D055
`AppId` 是 per-program namespace（`A-Za-z0-9._-`，≤128），不是鉴权；`for_app` manager 只看到自己 app id 的 camera，其它返回 `CameraNotFound`。

### D056
DShow 支持 per-user（HKCU，免提权）与 machine-wide（HKLM）注册；FilterData 由系统 `IAMFilterData::CreateFilterData` 生成，不手工构造。

### D057
当前 scope x64-only；x86 DShow DLL 与 ARM64 未做。FFmpeg dshow demuxer 不支持 P010（不是 source bug）。

### D058
提交按阶段拆分（protocol / DShow management / DShow source / MF / migration / docs），不做一次性大提交。

### D059
实现完成后删除 `WINDOWS_NATIVE_VIRTUAL_CAMERA_PLAN.md`（git 历史可查）；设计约束以本文件为准，用户文档为 README 与 `docs/USER_GUIDE.zh-CN.md`。

## 29. 官方参考

- https://learn.microsoft.com/windows/win32/api/mfvirtualcamera/nf-mfvirtualcamera-mfcreatevirtualcamera
- https://learn.microsoft.com/windows/win32/api/mfvirtualcamera/nf-mfvirtualcamera-imfvirtualcamera-getmediasource
- https://learn.microsoft.com/windows-hardware/drivers/stream/frame-server-custom-media-source
- https://learn.microsoft.com/windows/win32/api/mfidl/nf-mfidl-imfmediastream-requestsample
- https://learn.microsoft.com/windows/win32/api/mfobjects/nf-mfobjects-imf2dbuffer2-lock2dsize
- https://github.com/microsoft/Windows-Camera/tree/master/Samples/VirtualCamera
- https://learn.microsoft.com/windows/win32/directshow/
- https://learn.microsoft.com/windows/win32/termserv/kernel-object-namespaces

不确定平台行为时，优先 Microsoft 当前文档/SDK/sample + 真机测试。
