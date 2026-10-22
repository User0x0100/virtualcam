# AGENTS.md

本文件是本仓库 coding agent 的长期上下文。开始修改代码前必须先读本文件，再读任务相关源码和 `WINDOWS_NATIVE_VIRTUAL_CAMERA_PLAN.md`。

如果本文件、旧会话记忆和当前代码冲突，以用户最新指令 + 当前代码 + 可验证平台事实为准，并同步修正文档。

## 1. 项目定位

`virtualcam` 是 Rust Windows virtual-camera library。

当前三类能力：

1. OBS Virtual Camera producer。
2. Unity Capture producer。
3. Managed camera：
   - 当前 source 仍是 C++ DirectShow + libdshowcapture-derived。
   - 目标 Pure Rust DirectShow + Media Foundation。

OBS backend != managed DShow backend。

## 2. 当前事实

当前 Rust：

- `src/managed/mod.rs`：ManagedCameraId、registration/stream config、`ManagedCameraManager`。
- `src/managed/native_driver.rs`：C++ management ABI v3 FFI client（Phase 1 用 Rust registration 替换）。
- `src/backend/windows/obs.rs`：OBS producer（`OBSVirtualCamVideo`、occupancy、`obs-virtualcam.txt` hint）。
- `src/backend/windows/managed_dshow/`：managed DShow producer（Producer/Ready/Control/Data lifecycle + per-user last-format history）。
- `src/backend/windows/queue_writer.rs`：OBS 与 managed DShow 共享的 3-slot queue writer。
- `src/backend/windows/unity_capture.rs`
- `src/backend/windows/protocol/frame_queue.rs`：OBS-compatible queue layout。
- `src/backend/windows/protocol/publication.rs`：跨进程 publish/observe helper。
- `src/backend/windows/protocol/dshow_control.rs`：DShow control header + object names。
- `src/platform/windows.rs`

`protocol/` 只含 layout/name/ordering，不持有 handle，可直接迁入未来 `managed-protocol` crate。

`CameraType::ManagedDirectShow` 标识 managed DShow producer；`CameraType` 是 `#[non_exhaustive]`，MF backend 落地时新增 variant。`VirtualCamera` 是 `Send`、非 `Sync`。

当前 native：

`native/managed-virtualcam/`

仍包含：

- C++ COM DirectShow source。
- CMake。
- libdshowcapture-derived implementation。
- native tests。

不要把规划中的 Rust crates 当成已经存在。

## 3. 最终 backend 模型

```text
DirectShow
-----------
producer owns runtime format/control
session-local transport
dshow-source.dll

Media Foundation
----------------
source owns media capability/control
cross-session transport
mf-source.dll
FrameServer / Local Service / Session 0
```

两个 backend **不是同一个 transport lifecycle 换个 consumer API**。

## 4. Common protocol 只共享 frame queue

未来 `managed-protocol` 建议拆：

```text
frame_queue
publication helpers
dshow_control
mf_control
```

可以共享：

- frame queue layout。
- slot/sequence。
- payload validation。
- publication ordering。
- stale detection。

不能默认共享：

- control header。
- who selects format。
- object namespace。
- ownership。
- readiness。
- registration semantics。

## 5. Cross-process memory ordering 是协议

当前 Rust producer 的 write_idx/read_idx/state 只经 `protocol/publication.rs`（SeqCst 32-bit atomic，等价 `Interlocked*`）发布；C++ consumer 用 `MemoryBarrier`。

所以 protocol contract = layout + ordering。

不要只实现 `#[repr(C)]` structs 就认为兼容完成。

Writer：

```text
advance/reserve sequence
write timestamp/payload
publication barrier
publish readable index
publish READY
```

Reader：

```text
observe ready/index
observation barrier
validate
read/copy payload
```

Pure Rust 后把 unsafe publication/observation 收敛进 managed-protocol helper。

Windows-only v1 优先统一使用 Windows `Interlocked*` primitive 做跨进程 publish/observe fields，不让各 backend 自由混用不同 atomic strategy。

必须考虑 ARM64，不依赖 x86 强内存顺序。

当前 OBS-compatible 3-slot queue 没有 per-slot generation/seqlock。reader 读取一次 `read_idx` 后直接 memcpy；producer 连续绕回同一个 slot 时存在理论 torn-frame 窗口。

规则：

- Phase 0A 做 high-rate writer + deliberately slow reader + pattern/checksum stress。
- Phase 2 不顺手改 ABI。
- 若实测需要修，单独设计 protocol v2 + per-slot generation/seqlock。

Phase 0A 已有 oracle（`src/backend/windows/queue_writer.rs`）：

- `lapping_writer_reuses_the_selected_slot_without_a_generation_marker` 固定当前 ABI 性质：writer 绕回 3 帧后覆盖 reader 已选 slot。
- `high_rate_writer_against_slow_reader_reports_torn_frames`（ignored）实测：2026-10-22，x64，640x480，无节流 writer ≈60k fps + 分块 yield 的慢 reader，3 s 内 26,376 次读取中 74 次 torn。torn 风险真实存在，但需要 reader 单次 copy 期间 writer 推进 ≥3 帧；正常 30/60 fps 下只在 consumer 线程被长时间挂起时出现。是否做 protocol v2 仍需单独决策，不在 DShow rewrite 中顺手改。

## 6. DShow transport hard constraints

Namespace：

```text
Local\VirtualCamRs-{CLSID}-Producer
Local\VirtualCamRs-{CLSID}-Ready
Local\VirtualCamRs-{CLSID}-Control
Local\VirtualCamRs-{CLSID}-Data-{width}x{height}-{interval}
```

Control current ABI：

- 32 bytes。
- align 8。
- magic VCMR。
- version 1。
- STARTING/READY/STOPPING。

Data：

- OBS-compatible 3 slots。
- format-specific mapping。

Format：

- NV12 only。
- tightly packed shared-memory source frame。
- even dimensions。
- no conversion。

Lifecycle：

- one producer。
- producer decides runtime width/height/fps。
- registration 不固化 format。
- idle 使用同一 Windows 用户、同一 CLSID 最近一次成功输出的格式；无有效历史时 640x480@30 black。已协商的 graph 格式仍固定。
- connected graph mode fixed。
- crash -> black。
- corrupt active producer != idle。

## 7. DShow Rust implementation scope

至少：

- IClassFactory。
- IBaseFilter/IMediaFilter/IPersist。
- IPin。
- IAMStreamConfig。
- IKsPropertySet。
- IEnumPins。
- IEnumMediaTypes。
- IMemInputPin downstream query。
- IMemAllocator negotiation。
- IMediaSample。
- Receive。

不要复制当前 libdshowcapture `OutputPin::QueryInterface(IID_IMemInputPin)` self-cast。

Parity = observable behavior，不是逐行翻译。

DirectShow registration/filter contract 保持 current baseline：

```text
category = CLSID_VideoInputDeviceCategory
merit    = MERIT_DO_NOT_USE

REGFILTER2 version = 1
cPins = 1
pin:
  rendered = FALSE
  output   = TRUE
  zero     = FALSE
  many     = FALSE
  media type = MEDIATYPE_Video / MEDIASUBTYPE_NV12

runtime output pin IKsPropertySet:
  AMPROPSETID_Pin / AMPROPERTY_PIN_CATEGORY
    -> PIN_CATEGORY_CAPTURE
```

不要在 Rust rewrite 时改变这些可观察 registration/pin 语义。

DirectShow source 是 in-process COM DLL。

如果产品保留 32-bit consumer support，必须分别构建：

```text
dshow-source-x64.dll
dshow-source-x86.dll
```

同 camera CLSID 分别注册到 64-bit / 32-bit WOW64 registry view，并验证两个 bitness 的 DirectShow category enumeration/activation。

若支持 x86，parent x64 transaction coordinator 持有全局 management mutex，x86 helper/process **不得再次获取同一个 transaction mutex**；helper 只执行 x86 COM/FilterMapper registration 并返回结构化结果。不要让 x64 manager 手工构造 32-bit FilterData blob。

x64+x86 management 是一个 logical transaction：snapshot 两边 -> mutate 两边 -> 两边都成功后才 commit metadata。任一失败回滚已成功一边；list/get 必须识别 partial/degraded registration。

`IAMStreamConfig`/media-type parity 不是“接口存在即可”。Phase 0A/2 至少覆盖 `GetFormat`、`GetNumberOfCapabilities`、`GetStreamCaps`、valid/invalid `SetFormat`、`IPin::QueryAccept`、`IEnumMediaTypes`，并分别覆盖 idle fallback 与 producer runtime mode。

manager 调用 COM/`IFilterMapper2` 时必须尊重调用线程现有 COM apartment；只有自己成功初始化 apartment 时才负责对应 `CoUninitialize`，不能无条件 init/uninit。

COM registration contract：

```text
dshow-source.dll InprocServer32 ThreadingModel = Both
mf-source.dll    InprocServer32 ThreadingModel = Both
```

`Both` 只是 registration model，不替代 MF FTM/agility。

两个 source DLL 的 `DllMain` 必须接近空 stub：只做必要 HMODULE 记录和可选 `DisableThreadLibraryCalls`。禁止 COM、registry、LoadLibrary、IPC 初始化、worker startup/join、blocking wait、allocator/source/filter construction；所有实质初始化延迟到正常 COM lifecycle，避免 loader-lock deadlock。

Rust COM/FFI boundary 不能传播 panic：

- release source DLL 使用可捕获 panic 的策略。
- DLL export / class factory / COM method 最外层 `catch_unwind`。
- panic -> stable HRESULT。
- 外部输入和共享内存处理禁止依赖 unwrap/expect/assert。
- `panic=abort` 不作为 in-process source DLL 默认策略。

## 8. MF v1 的 format model 与 DShow 不同

这是 hard constraint。

MF source 在 producer 不存在时也必须能被 Frame Server 枚举 media types。

所以 MF v1 采用：

**每台 MF logical camera 一个持久化 native NV12 mode。**

Record 包含：

- width。
- height。
- fps numerator。
- fps denominator。
- NV12。

source 只 advertise 这个 mode。

producer open 必须匹配这个 mode。

第一版禁止：

- producer 任意改变 camera runtime mode。
- dynamic capability mutation。
- implicit resize/fps convert。

native mode 创建后 immutable。修改 native mode = recreate camera transaction，不能只更新 metadata/vendor attributes，否则可能和已激活 FrameServer source 的旧 descriptor split-brain。

future multi-mode 由 source/client selection 驱动，再通过 mf_control 告诉 producer。

## 9. MF source host

Frame Server custom source：

- Local Service。
- Session 0。

不能用：

```text
Local\VirtualCamRs-*
```

作为 production MF IPC。

候选：

```text
source creates Global\VirtualCamRs-MF-{TransportId}-{GenerationNonce}-...
authenticated desktop producer attaches
```

`TransportId` 只是 routing/config hint，不是 security capability。每个 source transport generation 由 source 生成高熵 `GenerationNonce`。

Named-object squatting hard rule：

- same-type pre-existing object / `ERROR_ALREADY_EXISTS`：绝不继续使用。
- different-type object already occupies same named-object namespace / `ERROR_INVALID_HANDLE`：同样视为 collision/attack。
- regenerate fresh source-owned nonce 或 fail activation。
- mapping/event/semaphore/mutex 都做 pre-creation collision tests。

Phase 0B 真机验证后冻结。

MF source native architecture v1 scope：**x64-only**。ARM64 shared-memory ordering 仍保持正确，但不等于已经支持 ARM64 MF deployment。未来支持 Windows on ARM 时单独产出/register native `mf-source-arm64.dll` 并做真实 FrameServer activation/IPC tests。

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
HKCU\Software\VirtualCamRs\ManagedCameras
```

或等价 per-user protected storage。

DirectShow machine-wide metadata 可以继续 HKLM。

Local Service source 的 HKCU != camera owner HKCU。

因此 MF source runtime routing不能“直接读当前 HKCU”。

v1 进一步禁止 source 侧 `symbolic-link -> owner HKCU` fallback：source 不能从 untrusted attribute 得到可信 owner SID。HKCU 只用于当前用户 manager bookkeeping/reconcile；runtime config 缺失时 fail closed，由 manager repair/recreate。

因为 MF v1 是 CurrentUser + System lifetime，machine-wide source uninstall 必须先清理或证明可安全保留所有仍指向本 source CLSID 的 persistent virtual cameras。两用户/Fast User Switching 下必须验证 admin cleanup 能找到本 source 的所有 camera；无法证明完整时 installer fail safe，不能先 unregister/delete source DLL。

## 12. MF fixed CLSID + IMFActivate

固定 `mf-source.dll` COM CLSID。

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

`MfSourceActivate` / `MfMediaSource` 必须是 free-threaded/neutral；优先验证聚合 Free-Threaded Marshaler（FTM）或等价 agile/free-threaded 策略。不要假设 `windows` crate 普通 `#[implement]` 自动提供 FTM。Phase 0B 必须在 MTA、多线程和 FrameServer cross-process activation 下验证 marshaling。

同一个 `MfSourceActivate` 的重复 `ActivateObject` 行为必须 Phase 0B 实测；production 不允许无意创建两个互不关联、各自拥有 transport 的 source instance。若平台表现为 singleton-per-activation，则缓存复用。

MF source 同样必须有 panic boundary：所有 exports、factory、IMFActivate/source/stream/IKsControl/allocator callbacks 最外层 `catch_unwind`，panic 转 HRESULT，不能让 recoverable Rust panic 直接终止 FrameServer。

## 13. MF vendor attributes 是正式主路由

Microsoft VirtualCamera sample 已使用 custom `IMFVirtualCamera::Set*` 属性向 source activation传配置。

本项目使用**配置型** attributes：

```text
VIRTUALCAMRS_CAMERA_ID
VIRTUALCAMRS_TRANSPORT_ID          # routing hint only
VIRTUALCAMRS_RENDEZVOUS_ID         # high-entropy routing id, not authorization
VIRTUALCAMRS_EXPECTED_OWNER_SID    # assertion only, never direct authorization
VIRTUALCAMRS_PROTOCOL_VERSION
VIRTUALCAMRS_NATIVE_WIDTH
VIRTUALCAMRS_NATIVE_HEIGHT
VIRTUALCAMRS_NATIVE_FPS_NUM
VIRTUALCAMRS_NATIVE_FPS_DEN
```

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

如果必需 attributes 缺失/越界/版本不支持，source fail closed 或只进入安全 black 状态（Phase 0B 决定）；不从任意用户 HKCU 猜 runtime routing。symbolic link 只供 manager 做 system identity/reconcile/repair。

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

Phase 0B 必须先验证 producer authentication / owner binding。real identity 来自 Windows token/impersonation/handle semantics，不来自 custom attributes。

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

Phase 0B 先测 1080p30/60 等目标 mode 的 RequestSample latency。

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

Camera Profile 不默认照抄 sample：Phase 0B 测试“不发布 profile collection”与“发布 profile collection”两种路径。若发布，必须包含 `KSCAMERAPROFILE_Legacy`，且 filter 必须与本 camera 的真实 native mode 相容；不要直接复制 sample 的固定 30/60fps filter。

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
- PAUSED 与 source-level Pause 不等价，Phase 0B 按 FrameServer 实际行为再定。
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

## 20. MF producer-first 仍待 Phase 0B 验证

不要提前把 MF open 定义成必须 `ReceiverInactive`，但也不要认为 `GetMediaSource()` 本身已经启动 stream。

正确 spike：

```text
MFCreateVirtualCamera
Start
GetMediaSource
CreatePresentationDescriptor/select native stream
通过 SourceReader 或等价 shared-client pipeline 真正 Start/read
MfMediaSource::Start
transport / RequestSample active?
```

`GetMediaSource` 是 shared-client/reduced-function source，生命周期绑定 IMFVirtualCamera；取得 interface != stream running。

测试 keepalive 是否影响其他 camera clients，以及 producer close 后如何释放 shared-client resources 而不 Remove system camera。

如果可稳定激活 transport，producer API 保持 producer-first。

禁止用 exclusive `MFCreateDeviceSource` 作为常驻 keepalive，避免锁掉其他 camera clients。

## 21. MF lifecycle

Configured -> Started -> Stopped -> Removed。

Shutdown 是 object resource lifecycle，不等于 permanent unregister。

Permanent delete 必须 Remove。

v1 only tests System lifetime。Session lifetime仅作为 future RAII-handle design 的平台事实，不进入当前 manager contract。

## 22. Old DirectShow camera migration

Phase 3 前不能直接删 old C++ DLL。

默认 v1 选择 breaking migration：

```text
unregister old camera while old DLL exists
install Rust dshow-source
recreate camera
```

必须提供明确 cleanup/migration path。

如果以后用户要求 preserve CLSID/system device identity，再设计 in-place InprocServer32 migration。

source DLL update 优先采用版本化文件名 + registration 原子切换：新进程加载新 DLL，仍持有旧 DLL 的进程继续运行；确认旧版本不再被使用后再清理文件。不要默认覆盖正在被 FrameServer/第三方进程加载的 DLL。

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

CUDA producer仍直接 D2H到 shared slot。

## 25. 实施顺序

### Phase 0A

DShow baseline + memory ordering oracle + 3-slot torn-frame stress + current bitness registration baseline。

### Phase 0B

MF architecture spike（v1 x64-only）：

- activation/vendor attributes + repeated ActivateObject instance behavior。
- formal `VIRTUALCAMRS_EXPECTED_OWNER_SID` assertion + `VIRTUALCAMRS_RENDEZVOUS_ID` routing attributes。
- vendor attributes treated as untrusted configuration；malformed/oversized values严格校验。
- free-threaded/FTM-or-equivalent marshaling in MTA/FrameServer。
- same-store GetSourceAttributes/GetStreamAttributes semantics。
- blocking GetEvent outside state lock。
- source<->stream Shutdown 拆环 + Source->Stream lock hierarchy。
- immutable native one-mode enumeration。
- Camera Profile A/B strategy；发布 profiles 时 Legacy profile 与 native mode 相容。
- required stream attributes。
- required Start/Stop event set + valid state transitions；不锁死跨 queue 总顺序。
- Start position validation。
- IMFMediaStream2 RUNNING/STOPPED contract。
- no EOS/EOP on normal live-camera transitions。
- CurrentUser HKCU metadata仅 manager bookkeeping；source 不读 owner HKCU fallback。
- Local Service/session0。
- named pipe uses explicit DACL + first-instance + local-only + message mode + overlapped I/O；禁止 default pipe DACL。
- bounded handshake must be read before `ImpersonateNamedPipeClient`。
- token SID compare + impersonation-level validation + all-path `RevertToSelf`。
- authentication is lazy/non-blocking：ActivateObject/Start 不等待 producer；未认证 producer 时继续 black。
- stalled/malformed client deadline；rendezvous worker cancellation/Shutdown/`DllCanUnloadNow` lifecycle。
- source-generated GenerationNonce only after authenticated attach。
- Global named-object squatting/pre-creation rejection。
- object-type-specific DACL/access masks。
- cross-user/Fast User Switching isolation；same-user malicious process明确不在 v1 isolation guarantee。
- synchronous RequestSample token + stopped `MF_E_MEDIA_SOURCE_WRONGSTATE` + non-duplicated error signaling + latency measurement。
- preferred provided allocator lifecycle + standard MF fallback。
- unsupported GetService/KsControl/SetD3DManager HRESULTs。
- post-Shutdown `MF_E_SHUTDOWN` behavior。
- pitch/timestamps。
- producer-first GetMediaSource + actual shared-client Start/read。
- multi-user System-camera uninstall/source-removal cleanup。
- client compatibility。

### Phase 1

Rust DShow management，包括完整 REGFILTER2 v1 single-output-pin NV12 metadata、`CLSID_VideoInputDeviceCategory`、`MERIT_DO_NOT_USE`、COM apartment ownership、x64/x86 registry views 和 architecture-matched FilterMapper registration；若支持 x86，不由 x64 manager 手工伪造 32-bit FilterData。x64+x86 mutate 是一个 logical transaction，list/get 能识别 partial/degraded registration。

### Phase 2

Pure Rust DShow source，包括 x64/x86 artifact scope、panic boundary、module/object lock，以及 `IAMStreamConfig` / `IPin::QueryAccept` / media-type enumeration parity。

### Phase 3

old-camera migration/cleanup，然后删除 C++/CMake/libdshowcapture。

### Phase 4

Production MF source：同步 RequestSample 优先；只有 latency gate 证明必要才加 async work queue；同时实现 free-threaded/FTM-or-equivalent marshaling、repeated ActivateObject policy、required event/state contract、same-store attributes、GetEvent lock discipline、source<->stream Shutdown 拆环/锁层级、provided allocator lifecycle + standard MF fallback、non-duplicated error signaling、unsupported-interface HRESULTs、post-Shutdown `MF_E_SHUTDOWN` 和 panic boundary。

### Phase 5

MF manager：CurrentUser + System lifetime only。Session lifetime 未来单独 RAII handle。Source uninstall/upgrade 必须先清理或证明可安全保留所有指向该 source CLSID 的 System cameras；多用户 cleanup 不可靠时 fail safe，不先删除 source registration/DLL。

### Phase 6

Unified API / Auto。

## 26. API 不要伪统一

不要强迫两个 backend registration语义一样。

可能的方向：

```rust
ManagedCameraRegistration::DirectShow {
    name,
}

ManagedCameraRegistration::MediaFoundation {
    name,
    native_stream,
}
```

DShow stream format在 producer open时决定。

MF v1 native stream在 registration时决定；access 固定 CurrentUser，lifetime 固定 System，不暴露 lifetime 参数。

## 27. 修改工作流

每次：

1. git status。
2. 读 AGENTS。
3. 读 plan relevant phase。
4. 读当前代码。
5. 保护 user uncommitted changes。
6. 小步改。
7. 测试。
8. review diff。
9. 更新 docs。

禁止：

- reset --hard。
- 无关 checkout。
- 删除未知文件。
- 覆盖用户变更。

## 28. 决策记录

### D001
DShow rewrite期间不改其 transport ABI。

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
当前 3-slot queue torn-frame 风险先 stress，不在 DShow rewrite 中静默改 ABI。

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
Repeated `IMFActivate::ActivateObject` behavior is verified in Phase 0B; one activation object must not accidentally create multiple unrelated source/transport instances.

### D035
Unsupported MF services/controls return explicit contract HRESULTs; post-Shutdown methods return `MF_E_SHUTDOWN` without touching released state.

### D036
MF vendor attributes are untrusted configuration, never producer identity/authorization. `OWNER_SID` is not a production authorization attribute.

### D037
MF production transport requires an authenticated producer-binding design based on Windows token/impersonation/handle semantics before Phase 4; HKCU is manager bookkeeping only.

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
first version NV12-only。

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
