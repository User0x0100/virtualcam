# Windows 原生虚拟摄像头双后端实施规划

> 状态：已完成第九轮架构审查，待实施  
> 文档版本：v10  
> 基线日期：2026-10-04  
> 适用仓库：`virtualcam`

## 1. 项目目标

把当前 managed virtual camera 从：

```text
Rust producer
    |
    v
shared memory
    |
    v
C++ DirectShow source
    |
    +-- OBS/libdshowcapture-derived code
    +-- CMake/MSVC C++ build
```

演进为两个 Windows 原生、纯 Rust 的 managed backend：

```text
                         Rust producer API
                                |
                     logical camera abstraction
                                |
                  +-------------+-------------+
                  |                           |
                  v                           v
          DirectShow backend         Media Foundation backend
                  |                           |
          producer-owned               source-owned
           control plane               control plane
                  |                           |
          session-local IPC            cross-session IPC
                  |                           |
                  v                           v
         dshow-source.dll               mf-source.dll
           Pure Rust                      Pure Rust
                  |                           |
                  v                           v
          DirectShow clients         Windows Camera Frame Server
                                              |
                                              v
                                        MF camera clients
```

最终目标：

1. managed DirectShow registration/management 由 Rust 实现。
2. DirectShow source/filter/pin/allocator/sample delivery 全部由 Rust 实现。
3. MF activation object、custom media source、stream、virtual-camera management 全部由 Rust 实现。
4. managed implementation 不再依赖 OBS/libdshowcapture C/C++ 源码。
5. managed build 不再需要 CMake/C++。
6. 两个 backend 共享 frame queue payload ABI、frame validation 和 publication semantics，但**不强制共享 control-plane ownership、IPC namespace 或 format negotiation 模型**。
7. OBS Virtual Camera / Unity Capture 继续作为第三方 producer backend 保留。

“纯 Rust”只约束本项目实现语言；Windows SDK、系统 DLL、COM、Media Foundation、DirectShow 和 MSVC linker 都是正常平台依赖。

## 2. 当前代码基线

### 2.1 OBS backend

`src/backend/windows/obs.rs`

Rust 已实现 OBS Virtual Camera shared-memory producer，包括与 OBS Studio 相同的 `%APPDATA%\obs-virtualcam.txt` consumer-first mode hint（仅在 queue 创建成功后写入）。

这是第三方 OBS device backend，不是本项目 managed DirectShow source。它与 managed DShow producer 只共享 `src/backend/windows/queue_writer.rs`。

### 2.2 Unity Capture backend

`src/backend/windows/unity_capture.rs`

Rust 已实现 Unity Capture producer。

### 2.3 Managed DirectShow baseline

Rust：

- `src/managed/mod.rs`（manager facade）
- `src/managed/native_driver.rs`（C++ management ABI v3 FFI，Phase 1 替换目标）
- `src/backend/windows/managed_dshow/`（producer control plane + last-format history）
- `src/backend/windows/queue_writer.rs`（共享 queue writer）
- `src/backend/windows/protocol/frame_queue.rs`
- `src/backend/windows/protocol/publication.rs`
- `src/backend/windows/protocol/dshow_control.rs`
- `src/platform/windows.rs`

`protocol/` 已按 §3.1 / §24 的 `frame_queue` / publication helpers / `dshow_control` 边界拆分，不持有 handle；`mf_control` 尚不存在。

Native：

`native/managed-virtualcam/`

当前 C++ DLL 负责：

- COM class factory。
- DirectShow filter/pin。
- media types/capabilities。
- downstream allocator negotiation。
- `IMediaSample` delivery。
- managed registry metadata。
- register/unregister/rename/list/get。
- queue reader。
- pacing。
- black fallback。
- stale producer recovery。

当前 management ABI v3：

- `VirtualCamGetAbiVersion`
- `VirtualCamRegister`
- `VirtualCamUnregister`
- `VirtualCamRename`
- `VirtualCamGet`
- `VirtualCamEnumerate`

COM exports：

- `DllGetClassObject`
- `DllCanUnloadNow`

## 3. 顶层架构原则

### 3.1 Shared queue protocol 与 backend control plane 分离

经过前序审查后，共享 protocol 的边界明确拆成：

```text
managed-protocol/
├── frame_queue.rs
│   ├── QueueHeader/layout
│   ├── slots
│   ├── sequence
│   ├── frame payload
│   ├── validation
│   └── publication/observation ordering
│
├── dshow_control.rs
│   └── producer -> source
│       producer declares runtime format
│
└── mf_control.rs
    └── source -> producer
        source owns/advertises selected native format
```

共享：

- NV12 frame payload definition。
- queue slot layout。
- sequence semantics。
- stale/partial publish detection。
- validation helpers。
- memory publication/observation rules。

不共享为同一个 header：

- DShow producer control。
- MF source control。
- DShow format ownership。
- MF media type selection。
- backend-specific readiness/lifecycle。

### 3.2 DirectShow control plane：producer-owned

保持当前语义：

```text
producer open
   |
   +-- chooses width/height/fps
   +-- publishes control
   +-- creates Data-{format}
   |
DirectShow source
   |
   +-- discovers producer runtime format
   +-- advertises/negotiates accordingly
```

DShow registration 继续只持久化 device identity，不固化 width/height/fps。

### 3.3 Media Foundation control plane：source-owned

MF source 必须在 producer 不存在时就能被 Frame Server 创建、枚举 streams/media types。

因此 MF 不能使用“producer 任意决定运行时格式，source 跟随”的 DShow 模型。

MF v1 采用**每台 logical camera 一个持久化 native output mode**：

```text
MF camera record
   |
   +-- native width
   +-- native height
   +-- native frame rate
   +-- subtype = NV12
   |
MfMediaSource
   |
   +-- advertises exactly this v1 native mode
   |
FrameServer/client
   |
   +-- selects that mode
   |
producer open
   |
   +-- must match camera native mode
```

第一版不支持：

- producer 任意把 720p30 camera 临时变成 1080p60。
- source 根据 active producer 动态改变 capability list。
- source 内部 resize/fps-convert/color-convert。

以后若要支持多 media type：

```text
source-selected media type
      |
mf_control
      |
producer adapts/matches selected mode
```

必须作为独立 phase 实现和验证。

### 3.4 ManagedCameraId backend-neutral

`ManagedCameraId` 是项目 logical identity。

DirectShow：

```text
ManagedCameraId 可以直接 == DirectShow camera CLSID
```

Media Foundation：

```text
ManagedCameraId != fixed MF source CLSID
```

MF 同时具有：

1. logical camera ID。
2. fixed source implementation CLSID。
3. Windows virtual-camera symbolic link。
4. random transport ID。

不要混用。

## 4. DirectShow transport 与行为不变量

### 4.1 Current namespace

```text
Local\VirtualCamRs-{CLSID}-Producer
Local\VirtualCamRs-{CLSID}-Ready
Local\VirtualCamRs-{CLSID}-Control
Local\VirtualCamRs-{CLSID}-Data-{width}x{height}-{interval}
```

Pure Rust DShow 迁移期间先保持。

### 4.2 Current control ABI

当前：

- 32 bytes。
- alignment 8。
- magic `VCMR`。
- control version 1。
- STARTING = 1。
- READY = 2。
- STOPPING = 3。

native management ABI v3 与 control version 1 是不同版本域。

### 4.3 Format

DShow v1：

- NV12 only。
- tightly packed shared-memory frame。
- even width/height。
- no resize。
- no conversion。

### 4.4 Runtime format

producer session 决定：

- width。
- height。
- frame rate。

registration 不固定 format。

### 4.5 Idle fallback

无 producer：

```text
same-user + same-CLSID last successfully published format
else 640x480 @ 30
NV12
black
```

当前 C++ baseline 从 `HKCU\\Software\\VirtualCamRs\\ManagedCameraFormats\\{CLSID}`
读取单值 `LastFormat`（校验 magic/version/dimensions/frame interval）；首次成功发布 NV12 帧后由 Rust producer 保存。
未来 Pure Rust DShow 实现应保留该行为，不要和 MF source 的配置模型混用。

camera 仍可被 consumer 打开。

### 4.6 Connected graph

graph 一旦连接，negotiated media type 固定。

matching producer 可接管；different producer mode 需要 consumer reopen。

### 4.7 Stale/corrupt producer

producer crash/stale：

- 不冻结最后一帧。
- 回 black。
- 后续 matching producer 可恢复。

active producer corruption/version mismatch/access failure：

- 不得静默冒充 idle。

### 4.8 Format-specific mapping

保留 `Data-{width}x{height}-{interval}`，避免旧 consumer retained mapping 限制新 producer section capacity。

## 5. 跨进程 frame publication contract

ABI layout 不是完整协议。

当前 producer 已经依赖：

- volatile read/write。
- compiler fence。
- full fence。
- C++ consumer memory barrier。

因此本方案把 memory ordering 作为 protocol 的一部分。

### 5.1 Writer contract

概念顺序：

```text
reserve/advance write sequence
        |
write timestamp + payload
        |
release/full publication barrier
        |
publish readable sequence/index
        |
publish READY/state
```

### 5.2 Reader contract

```text
observe READY/readable sequence
        |
acquire/full observation barrier
        |
validate header/slot bounds
        |
copy/read payload
        |
optionally re-check sequence/state
```

### 5.3 Rust implementation

不要让每个 backend 自己拼：

- `read_volatile`
- `write_volatile`
- `compiler_fence`
- `fence`

最终把 publication/observation 封装进 `managed-protocol` 的少量 unsafe helper。

Windows-only v1 优先使用 Windows `Interlocked*` primitive 作为跨进程 publish/observe field 的原子与内存屏障基础，而不是让不同 backend 自由选择 Rust atomics 或 Interlocked。

选择标准：

- x64 正确。
- ARM64 正确。
- 跨进程 shared memory 正确。
- 与迁移期间 C++ baseline 可兼容。
- 不改变现有 queue binary layout，除非明确 bump ABI。

当前实现（producer 侧）：`src/backend/windows/protocol/publication.rs` 是唯一 publish/observe 入口。`increment_u32` / `publish_u32` 是 SeqCst 32-bit atomic RMW（与 MSVC `InterlockedIncrement` / `InterlockedExchange` 同为 full barrier），`observe_u32` 是 volatile load + full fence（对应 C++ `MemoryBarrier`，可用于只读 view）。OBS 与 managed DShow 都经 `queue_writer.rs` 使用它；publication 顺序与 binary layout 未变。Rust DShow/MF source 的 reader 侧应复用同一模块。

### 5.4 当前 3-slot queue 的既有限制

当前 OBS-compatible 3-slot queue 没有 per-slot generation/seqlock。

现有 reader 的基本过程是：

```text
read read_idx
select slot = read_idx % 3
memcpy whole frame
```

如果 reader 正在复制 slot N，而 producer 足够快地再发布 3 帧并绕回 slot N，理论上 producer 可以在 reader copy 期间覆盖同一个 slot，产生 torn frame。

这是当前 queue ABI 的既有限制，不是 Pure Rust rewrite 新引入的问题。

Phase 0A 必须增加：

- 高频 writer。
- 故意慢 reader。
- pattern/checksum frame。
- 长时间 wraparound stress。
- torn-frame detection。

当前进展（2026-10-22）：Rust producer 侧 oracle 已在 `src/backend/windows/queue_writer.rs`。确定性测试证明 writer 绕回 3 帧会覆盖 reader 已选 slot；ignored stress test 在 x64、640x480、无节流 writer（≈60k fps）与分块 yield 慢 reader 下，3 s 内 26,376 次读取出现 74 次 torn frame。torn 风险在当前 ABI 中真实存在；正常帧率下需要 consumer 单次 copy 跨过约 2 个帧间隔。C++ consumer 侧 oracle 与长时间 wraparound 仍待补。

Phase 2 不允许为了顺手修复它而静默修改当前 ABI。

如果压力测试证明需要 protocol v2，优先考虑 per-slot generation/seqlock：

```text
writer:
  seq = odd
  write payload
  publish barrier
  seq = even

reader:
  seq_before
  copy payload
  seq_after
  accept only if seq_before == seq_after && even
```

protocol v2 必须单独设计、版本化和迁移。

### 5.5 迁移规则

Phase 0A 先把当前 C++/Rust ordering 作为 oracle。

Pure Rust source 完成前不得因为“Rust atomic 更漂亮”随意改变 publication sequence。

## 6. Pure Rust DirectShow source 范围

### 6.1 COM/filter interfaces

至少：

- `IClassFactory`
- `IUnknown`
- `IPersist`
- `IMediaFilter`
- `IBaseFilter`
- 必要时 `IAMFilterMiscFlags`
- `IPin`
- `IAMStreamConfig`
- `IKsPropertySet`
- `IEnumPins`
- `IEnumMediaTypes`

DirectShow device/filter registration contract 保持当前 baseline：

```text
filter category = CLSID_VideoInputDeviceCategory
filter merit    = MERIT_DO_NOT_USE

REGFILTER2 version = 1
cPins = 1
pin:
  rendered = FALSE
  output   = TRUE
  zero     = FALSE
  many     = FALSE
  connect category = CLSID_NULL
  media types:
    MEDIATYPE_Video
    MEDIASUBTYPE_NV12

runtime output pin category via IKsPropertySet:
  AMPROPSETID_Pin / AMPROPERTY_PIN_CATEGORY
      -> PIN_CATEGORY_CAPTURE
```

FilterMapper registration metadata、runtime pin category 和 COM class registration 都属于兼容性 contract，不在 Rust rewrite 时随意改变。

### 6.2 Downstream sample pipeline

正确实现：

```text
downstream IPin
   |
QI(IMemInputPin)
   |
GetAllocator / CLSID_MemoryAllocator
   |
GetAllocatorRequirements
SetProperties
NotifyAllocator
Commit / Decommit
   |
GetBuffer -> IMediaSample
   |
fill frame
SetActualDataLength
SetTime
SetMediaTime as appropriate
SetSyncPoint
   |
IMemInputPin::Receive
```

当前 libdshowcapture-derived `OutputPin::QueryInterface(IID_IMemInputPin)` self-cast 不属于 parity contract。

### 6.3 x86/x64 in-process COM deployment

DirectShow source 是 in-process COM DLL。

因此要支持 64-bit 和 32-bit consumer，需要分别构建：

```text
dshow-source-x64.dll
dshow-source-x86.dll
```

同一个 camera CLSID 需要分别注册到对应 registry view：

```text
64-bit COM/DirectShow registry view -> x64 DLL
32-bit WOW64 registry view          -> x86 DLL
```

不能假设 x64 manager 正常调用一次 `IFilterMapper2::RegisterFilter` 就自动覆盖 32-bit DirectShow 枚举。

若保留 x86 support，优先使用**架构匹配的注册 helper**：

```text
x64 parent transaction coordinator
    -> owns global management mutex
    -> performs x64 registration
    -> launches x86 helper

x86 helper/process
    -> does NOT reacquire parent transaction mutex
    -> performs x86 COM + FilterMapper registration
    -> returns structured result
```

不要让 x64 manager 手工生成/伪造 32-bit DirectShow `FilterData` registry blob；让对应 bitness 的 `IFilterMapper2` 产生并维护它。

跨进程 helper 不能复用一套会再次拿同名 global management mutex 的普通 management entrypoint，否则 parent 等 child、child 等 parent 会自锁。

Phase 1 必须验证：

- 64-bit `InprocServer32`。
- 32-bit WOW64 `InprocServer32`。
- 两个 view 的 `CLSID_VideoInputDeviceCategory` + REGFILTER2/FilterData registration。
- category/merit/pin/media-type metadata parity。
- x64 consumer activation。
- x86 consumer activation。

### 6.3.1 Cross-architecture management transaction

如果产品同时支持 x64/x86，register/rename/unregister 必须作为**一个 logical transaction**处理：

```text
snapshot x64 registration
snapshot x86 registration
        |
mutate x64
        |
mutate x86 helper/process
        |
commit managed metadata only after both succeed
```

任一 bitness 操作失败：

- 回滚已经成功的另一个 registry view/category registration。
- 不提交新的 managed metadata。
- rollback failure 单独报告。

`list/get` 必须识别 partial/degraded registration，例如：

- x64 exists, x86 missing。
- x86 exists, x64 missing。
- COM path exists but FilterMapper category missing。
- category exists but `InprocServer32` invalid。

不要把 partial state 简化成“camera exists = healthy”。

如果产品最终明确不支持 32-bit consumer，可以在 release scope 中显式取消 x86；在此之前不能假定单一 x64 DLL 足够。

### 6.4 COM apartment ownership

manager 在使用 COM/`IFilterMapper2` 前必须有明确 apartment 策略。

规则：

- 不无条件 `CoInitializeEx` 后再 `CoUninitialize`，避免破坏调用线程已有 COM apartment ownership。
- 如果当前线程未初始化 COM，manager 可以初始化并只在自己成功初始化时负责对应 `CoUninitialize`。
- 如果调用方已经初始化，复用调用方 apartment。
- `RPC_E_CHANGED_MODE` 不能简单吞掉或把线程强切 apartment。

### 6.4.1 IAMStreamConfig / media-type parity

Phase 0A/2 必须把当前 observable media-type behavior 固化成 parity tests，而不是只验证 interface 存在。

至少覆盖：

- `GetFormat`。
- `GetNumberOfCapabilities`。
- `GetStreamCaps`。
- `SetFormat(valid)`。
- `SetFormat(invalid)`。
- `IPin::QueryAccept`。
- `IEnumMediaTypes` enumeration/reset/clone behavior。
- consumer-first 最近有效历史格式（无历史则 640x480@30）fallback 下上述接口。
- producer-first/runtime mode 下上述接口。

当前 baseline 的 `GetStreamCaps` 对每个 advertised mode 返回固定 size/frame interval bounds；`SetFormat` 只接受 source 当前允许的 media type。Pure Rust rewrite 要保持这些外部语义，除非后续明确设计 breaking change。

### 6.5 DllMain / loader-lock hard constraint

两个 in-process source DLL 的 `DllMain` 都必须保持最小化：

```text
DLL_PROCESS_ATTACH:
  optionally DisableThreadLibraryCalls
  record HMODULE only if needed
  return
```

禁止在 `DllMain`：

- COM initialization/uninitialization。
- registry access。
- `LoadLibrary` / dynamic dependency loading。
- named-pipe/shared-memory/transport initialization。
- worker thread startup/join。
- blocking mutex/event waits。
- allocator/source/filter construction。

所有实质初始化延迟到 COM activation/class factory/source/filter lifecycle，避免 loader-lock deadlock。

### 6.6 InprocServer32 ThreadingModel

安装 contract 明确：

```text
dshow-source.dll:
  InprocServer32 = architecture-matching DLL
  ThreadingModel = Both

mf-source.dll:
  InprocServer32 = architecture-matching DLL
  ThreadingModel = Both
```

`ThreadingModel=Both` 只是 COM registration model；MF source 仍必须满足 9.2 的 FTM/free-threaded/agility 要求。

同一个 DLL 暴露的 COM classes 不混用互相矛盾的 apartment registration。

### 6.7 Rust panic/FFI boundary

`dshow-source.dll` 会被加载进第三方 consumer process。

任何 Rust panic 穿过 COM/FFI boundary 都不可接受。

release source DLL 规则：

- 使用可捕获 panic 的构建策略。
- 所有 DLL exports、class factory 和 COM method 的最外层边界统一 `catch_unwind`。
- panic 转换为稳定 HRESULT，例如 `E_UNEXPECTED`/`E_FAIL`，并记录诊断。
- 内部避免 `unwrap`/`expect`/assert 处理外部数据或可恢复错误。
- 不允许 panic unwind 穿过 non-unwind Windows ABI。

`panic=abort` 不作为 source DLL 默认策略，因为库内部 bug 不应直接终止 OBS、浏览器、会议软件等宿主进程。

## 7. Media Foundation source host 与 IPC

### 7.1 Host

Frame Server Custom Media Source 运行于 service environment：

- Local Service。
- Session 0。

所以 MF production backend 禁止直接使用 DShow 的：

```text
Local\VirtualCamRs-*
```

### 7.2 Source-owned cross-session transport

候选：

```text
MfMediaSource
   |
   | creates secured cross-session objects
   v
Global\VirtualCamRs-MF-{TransportId}-{GenerationNonce}-...
   ^
   | authenticated producer attaches
   |
desktop producer
```

normal producer 不应为了 MF backend 需要 administrator/`SeCreateGlobalPrivilege`。

`TransportId` / camera id / native-mode vendor attributes 只能用于**配置和路由**，不能作为身份凭据。

source 每次 activation/start transport generation 必须自己生成高熵 `GenerationNonce`；最终 kernel-object name 不能完全由 caller-provided attribute 决定。

### 7.3 Named-object squatting

对 source-owned named mapping/event/semaphore/mutex：

- Create* 后必须检查对象是否为本次真正新建。
- `ERROR_ALREADY_EXISTS` / same-type pre-existing object => 不继续使用该对象。
- `ERROR_INVALID_HANDLE` / different-type object already occupies same namespace name => 同样视为 collision/attack，不降级复用。
- 重新生成 source-owned generation nonce 后重试，或直接使当前 activation 失败。
- 不接受“已有对象但 DACL/size 看起来差不多”作为成功。
- mapping、event、semaphore、mutex 都要覆盖 pre-creation collision tests。

原因：Windows named-object Create* 遇到同名 object 时可能打开已有 object；此时 caller 指定的新 security descriptor / mapping size 不会把已有对象变成受信对象。

### 7.4 Producer authentication / owner binding

**vendor attributes 是 untrusted configuration，不是 authorization。**

尤其这些值不能直接决定 DACL：

```text
VIRTUALCAMRS_OWNER_SID
VIRTUALCAMRS_TRANSPORT_ID
VIRTUALCAMRS_* native mode
```

任何能按 Windows API 创建/配置使用本 source CLSID 的 caller 都可能提供自定义 attributes，因此 source 必须把它们当不可信输入并完整校验。

Phase 0B 必须先证明 producer authentication / owner-binding，再冻结 MF production transport。

v1 优先验证 **named-pipe authenticated rendezvous**，而不是一开始引入独立 broker：

```text
manager registration/config
   |
   +-- EXPECTED_OWNER_SID       # assertion only, not credential
   +-- random RENDEZVOUS_ID     # routing only
   v
activation attributes
   |
   v
Local Service source
   |
   +-- creates explicit-ACL local-only first-instance pipe
   +-- starts cancellable overlapped rendezvous worker
   +-- source Start does NOT wait for producer
   |
producer connects later
   |
server reads one complete bounded handshake message
   |
validate magic/version/fixed sizes
   |
ImpersonateNamedPipeClient     # after successful read
   |
OpenThreadToken / TokenUser
   |
reject anonymous/insufficient identity level
   |
compare actual SID == EXPECTED_OWNER_SID
   |
RevertToSelf on every path
   |
authenticated
   |
source generates fresh GenerationNonce
   |
create/open transport for this generation
   |
return transport information / duplicate handles
```

其中 `EXPECTED_OWNER_SID` 只是待验证 assertion：

- 不能直接决定 DACL。
- 不能替代 client-token authentication。
- mismatch => reject producer attach。

`RENDEZVOUS_ID`、camera id、symbolic link、TransportId 都只用于定位 endpoint，不是 secret capability。

如果 named-pipe spike 无法满足 FrameServer/Local Service 安全与 lifecycle 要求，再评估 capability/handle-based broker/rendezvous；不要预先实现复杂 broker。

#### Rendezvous pipe security contract

`CreateNamedPipeW` 必须使用显式 security descriptor，禁止 NULL/default DACL。默认 pipe security 会给 Everyone/anonymous read access，不适合本项目。

v1 candidate：

```text
open mode:
  PIPE_ACCESS_DUPLEX
  | FILE_FLAG_OVERLAPPED
  | FILE_FLAG_FIRST_PIPE_INSTANCE

pipe mode:
  PIPE_TYPE_MESSAGE
  | PIPE_READMODE_MESSAGE
  | PIPE_WAIT
  | PIPE_REJECT_REMOTE_CLIENTS

instances:
  1 per source generation / rendezvous endpoint unless spike proves more are needed

security descriptor:
  explicit DACL
  Local Service/source principal: server rights
  expected CurrentUser SID: minimum client connect/read/write rights
  no anonymous
  no Everyone broad access
```

`EXPECTED_OWNER_SID` 可用于**缩小 pipe DACL**，但仍只是 assertion/defense-in-depth；最终 authorization 必须来自实际 client token SID 比较。

不要给 client 不必要的 `FILE_CREATE_PIPE_INSTANCE`/server-instance creation 能力。

#### Handshake / impersonation order

`ImpersonateNamedPipeClient` 使用的是 pipe 上**最后一条已读取消息**的 client security context，因此顺序锁死：

```text
ConnectNamedPipe / completed overlapped connect
        ↓
ReadFile / completed overlapped bounded message
        ↓
validate fixed handshake envelope
        ↓
ImpersonateNamedPipeClient
        ↓
OpenThreadToken(..., OpenAsSelf=TRUE when needed)
        ↓
TokenUser + impersonation level
        ↓
SID comparison
        ↓
RevertToSelf
```

禁止连接后未读消息就 impersonate。

固定 handshake v1 只包含有限、非 secret routing data，例如：

```text
magic
version
camera_id
protocol_version
```

不信任 client 自报 PID/SID。长度必须固定或有严格小上限；不得按 client-supplied length 无界分配。

至少要求可识别 client identity；`SecurityAnonymous` 必须拒绝。客户端如果显式设置 SQOS，不得把 server 降到无法可靠取得 identity 的级别后仍继续授权。

#### Rendezvous worker lifecycle

认证必须 lazy/non-blocking，但这意味着 pipe I/O 也必须可取消：

- 使用 overlapped `ConnectNamedPipe` / `ReadFile`，不使用 `PIPE_NOWAIT` 模拟 async。
- 每个连接的 handshake 有有限 deadline / byte budget。
- client connect 后不发送完整 handshake，deadline 到期后断开。
- Source::Shutdown 设置 stop state，取消/关闭 pending pipe I/O，唤醒并 join/确认 worker 退出。
- rendezvous worker 存活期间 `DllCanUnloadNow` 必须返回 `S_FALSE`。
- worker 不在 `DllMain` 创建/销毁。
- source restart/新 activation 使用新的 rendezvous/generation state，旧 worker/旧 endpoint 不能继续授权新 generation。

设计要求：

- `ImpersonateNamedPipeClient` 失败 => authentication 失败；绝不能继续以 Local Service 身份执行 authorization path。
- impersonation 后必须在所有 success/error path `RevertToSelf`。
- real user identity 来自 Windows token/impersonation semantics。
- second user 必须无法连接/写入另一用户 camera transport。
- native width/height/fps/version/string length 等 activation attributes 做严格 bounds validation + checked arithmetic，避免 FrameServer allocation/CPU DoS。
- authentication/rendezvous 不成为 `ActivateObject` 或 source `Start` 的同步前置条件。

#### v1 threat model

v1 明确防护：

- cross-user frame injection。
- remote named-pipe client。
- unauthenticated/pre-created transport/rendezvous objects。

v1 **不承诺隔离同一个 Windows account 下的恶意进程**：同一用户的任意进程拥有相同 TokenUser SID。若未来需要“只有官方 producer executable 才能写”，再增加 per-camera secret capability / signed-process policy 等第二因子，不把这项复杂度塞进 v1。

在 Phase 0B auth spike 通过前，`Global\...` 方案仍是 transport candidate，不是已冻结 production security model。

### 7.5 ACL 按对象类型拆分

不要设计一个通用 `read/write/synchronize` access mask。

最小权限按 kernel-object type 分别定义，例如：

```text
file mapping handle/view:
  documented Win32 `FILE_MAP_READ` / `FILE_MAP_WRITE` as applicable
  + only required object standard rights
  # file-mapping object 不支持/不依赖 SYNCHRONIZE

event:
  SYNCHRONIZE
  EVENT_MODIFY_STATE

semaphore:
  SYNCHRONIZE
  SEMAPHORE_MODIFY_STATE

mutex (若实际使用):
  SYNCHRONIZE
  MUTEX_MODIFY_STATE
```

DACL principal 由最终经过验证的 authentication/owner-binding 方案决定，而不是 vendor `OWNER_SID`。

仍保持：

- source host/Local Service 获得自身需要的权限。
- Everyone 禁止 frame write。
- unrelated users denied。
- SYSTEM/Admin 仅在维护确有需要时授权。

### 7.6 MF source native architecture scope

MF custom media source 同样是 in-process DLL，必须匹配实际 FrameServer host architecture。

v1 release scope 必须显式二选一：

```text
A. x64-only Windows support
   -> ship/register mf-source-x64.dll only
   -> ARM64 Windows explicitly unsupported for MF backend

B. x64 + ARM64
   -> build/register native mf-source-x64.dll
   -> build/register native mf-source-arm64.dll
   -> installer chooses architecture-matching source DLL
```

当前计划优先按 **x64-only v1** 收敛；ARM64 的 shared-memory ordering 仍应正确，但这不等于已经支持 ARM64 MF deployment。

如果后续开启 ARM64 support，必须增加真实 ARM64 FrameServer activation/IPC tests，而不是只依赖 cross-compile。

## 8. MF source installation 与 user metadata

### 8.1 Source installation

固定 `mf-source.dll`：

- stable local path。
- fixed COM CLSID。
- machine-level COM registration。
- installer/admin deployment。
- version/update/uninstall strategy。

由于 v1 camera 是 `CurrentUser + System lifetime`，source uninstall **不能只删除 COM registration/DLL**。卸载前必须清理仍指向本 source CLSID 的持久 virtual cameras，否则会留下可枚举但无法激活的 broken device。

Phase 0B/5 必须验证 multi-user uninstall cleanup：

```text
User A CurrentUser/System camera
User B CurrentUser/System camera
        |
admin source uninstall
        |
enumerate virtual-camera devices / source ownership
        |
remove every camera owned by this source CLSID
        |
verify no orphan device remains
        |
unregister source COM
        |
remove old source DLL when no process holds it
```

可参考 Windows Camera sample 的 installer cleanup：枚举 camera device，检查其 custom capture source CLSID，再移除对应 device/devnode；但不能假定该方法天然覆盖所有用户，必须在两用户/Fast User Switching 场景真机验证。

如果无法可靠跨用户清理，installer 必须 fail safe：不能先删 source CLSID/DLL；应要求每用户 cleanup/reconciliation，或采用能可靠枚举所有本 source camera 的管理员清理路径。

### 8.2 Camera records

MF v1 access 固定：

`MFVirtualCameraAccess_CurrentUser`

因此 camera bookkeeping 使用 user-scoped storage：

```text
HKCU\Software\VirtualCamRs\ManagedCameras\...
```

或等价的 per-user protected storage。

DirectShow machine-wide records 保持 machine-scoped。

### 8.3 HKCU 只用于 manager bookkeeping

FrameServer/Local Service 下的 HKCU 不是 camera owner 的 HKCU，而且 source 不能从不可信 `OWNER_SID` 反推出一个可信用户 hive。

因此 v1 直接规定：

- runtime source 不读取 manager 的 HKCU record。
- HKCU 仅供当前用户 manager 做 list/get/reconcile/recreate bookkeeping。
- source 初始化依赖经过严格校验的 activation configuration + Phase 0B 确认的 authenticated control/owner-binding path。
- **取消** `symbolic-link -> owner HKCU -> runtime routing` fallback。
- activation/routing 必需配置缺失或不一致时 source fail closed；manager 负责 repair/recreate camera，而不是 source 猜用户 hive。

symbolic link 继续作为 Windows system identity 和 manager reconciliation key，不作为跨用户 runtime authorization mechanism。

## 9. MF fixed CLSID + IMFActivate

`mf-source.dll` 注册一个 fixed COM CLSID。

Class factory 创建：

```text
MfSourceActivate
├── IMFActivate
└── IMFAttributes
        |
        | ActivateObject()
        v
MfMediaSource
├── IMFMediaSourceEx
├── IMFGetService
├── IKsControl
└── IMFSampleAllocatorControl   # preferred optimization, not correctness prerequisite
        |
        v
MfMediaStream
└── IMFMediaStream2
```

`IMFMediaSourceEx` / `IMFMediaStream2` 继承各自 event-generator interface。

本项目把 IMFActivate 设为 requirement，用于：

- per-instance initialization。
- symbolic link/configuration。
- vendor attributes。
- transport identity。

如果 activate/source 分离：

- activation attributes 可以复制到 source 自己的 attribute store；或
- activation/source 共享初始化 attribute state。

但 source 初始化完成后，`GetSourceAttributes()` 必须返回 **source 自己持有的同一个 attribute store 引用**，不能每次 clone/copy 新 store。上游可能修改该 store，并期待修改持续可见。

`GetStreamAttributes(stream_id)` 同样返回对应 stream 持有的实际 attribute store reference，不每次制造副本。

按 Microsoft Frame Server contract：

- `IMFActivate::DetachObject` 返回 `E_NOTIMPL`。
- `IMFActivate::ShutdownObject` 返回 `E_NOTIMPL`。

即使 Microsoft sample 某版本实现不同，也以当前正式 Frame Server contract 为目标。

### 9.1 MF Rust panic/FFI boundary

`mf-source.dll` 运行在 Windows Camera Frame Server/FrameServerMonitor 进程中，因此 panic boundary 比普通应用库更严格。

规则与 DirectShow source 相同：

- release source DLL 使用可捕获 panic 的构建策略。
- `DllGetClassObject`、class factory、`IMFActivate`、`IMFMediaSourceEx`、`IMFMediaStream2`、`IKsControl`、allocator callbacks 等最外层统一 `catch_unwind`。
- panic 转换为稳定 HRESULT，并记录诊断。
- 外部数据/共享内存/COM input 不使用 `unwrap`/`expect`/assert 处理可恢复错误。
- panic 不允许穿过 Windows non-unwind ABI。
- `DllCanUnloadNow` 必须同时考虑 live COM object、event queue、allocator reference、rendezvous worker/pending overlapped pipe I/O，以及未来如果引入 async media work 时的 outstanding work。

source DLL 内部 bug 不应直接终止 Frame Server。

### 9.2 Free-threaded / COM marshaling

`ThreadingModel=Both` 只描述 COM activation/threading registration，不足以证明对象可安全跨 apartment/process proxy 使用。

`MfSourceActivate` 和 `MfMediaSource` 必须满足 Media Foundation 对 free-threaded/neutral component 的要求。

Phase 0B 先验证 Rust COM implementation 的 marshaling/agility 策略：

- 优先评估聚合 Free-Threaded Marshaler（FTM）或等价、可验证的 agile/free-threaded 实现。
- 不假设 `windows` crate 的普通 `#[implement]` 自动提供 FTM/agility。
- 在 MTA、多线程和 FrameServer cross-process activation 下验证 interface marshaling。
- 如果 FTM 与所用 Rust COM abstraction 不能可靠组合，先解决 marshaling，再继续 production MF source。

对象内部状态仍必须同步；free-threaded 不等于 lock-free。

### 9.3 ActivateObject instance identity

通用 `IMFActivate` 语义倾向于重复 `ActivateObject` 返回同一个已激活对象，直到 activation object 被 detach/shutdown；但 Frame Server virtual-camera activation 与 Microsoft sample 历史实现存在差异。

因此 Phase 0B 必须实测同一个 `MfSourceActivate`：

```text
ActivateObject #1 -> source A
ActivateObject #2 -> ?
```

production 目标：

- 同一个 activation object 不得无意创建两个互不关联、各自拥有 transport 的 media source。
- 如果平台实际要求 singleton-per-activation，则缓存并复用 source instance。
- 如果 Frame Server 明确以其他生命周期管理 source，则按实测 contract 实现。

在该 spike 完成前，不把 sample 某一版本的 `DetachObject/ShutdownObject` 行为反推成 source-instance policy。

## 10. MF instance routing

### 10.1 Vendor attributes 是主路径

Microsoft VirtualCamera sample 已实际使用 `IMFVirtualCamera::SetUINT32/SetString` 将自定义配置交给 media-source activation。

因此本方案将它作为**正式 primary design**。

manager 在 `Start` 前设置**配置型** vendor attributes：

```text
VIRTUALCAMRS_CAMERA_ID
VIRTUALCAMRS_TRANSPORT_ID          # routing hint, not a security capability
VIRTUALCAMRS_RENDEZVOUS_ID         # high-entropy routing id, not authorization
VIRTUALCAMRS_EXPECTED_OWNER_SID    # assertion only, never direct authorization
VIRTUALCAMRS_PROTOCOL_VERSION
VIRTUALCAMRS_NATIVE_WIDTH
VIRTUALCAMRS_NATIVE_HEIGHT
VIRTUALCAMRS_NATIVE_FPS_NUM
VIRTUALCAMRS_NATIVE_FPS_DEN
```

安全语义锁死：

- `VIRTUALCAMRS_EXPECTED_OWNER_SID` 是 untrusted assertion；只能与 named-pipe 实际 client token SID 比较，不能直接当 credential 或直接决定最终 transport authorization。
- `VIRTUALCAMRS_RENDEZVOUS_ID` 是 endpoint routing identifier；高熵用于降低碰撞/猜测，但不作为授权依据。
- 不再使用 `VIRTUALCAMRS_OWNER_SID` 这种容易被误解为“可信 owner”的字段名。

activation object：

```text
IMFActivate attributes
   |
   +-- logical camera/config identity
   +-- routing hint
   +-- protocol/native media mode
   |
   +-- strict validation
   |
   v
MfMediaSource
   |
   +-- authenticated producer binding (separate security plane)
```

所有整数/尺寸/速率/string/GUID 输入必须先做上限、非零、偶数 NV12 dimensions、分母、overflow 和 allocation-size 检查，再进入 source state。

仍然测试：

- reopen existing camera。
- System lifetime + reboot persistence。
- manager object release/Shutdown does not unregister System-lifetime camera。
- multi-user isolation。
- malicious/oversized/malformed custom attributes。

### 10.2 Missing configuration is fail-closed

v1 不再提供 source 侧的 owner-HKCU fallback。

如果 activation 时必需 vendor configuration 缺失、版本不支持、native mode 越界或 authenticated producer binding 无法建立：

- source 返回明确 configuration/activation error，或保持可激活但只进入安全的 no-producer black 状态，具体行为由 Phase 0B compatibility test 决定。
- 绝不根据不可信 owner SID 去打开任意用户 hive。
- manager 使用自身 HKCU record + symbolic link 做 repair/recreate。

runtime hot path 不查 registry。

## 11. MF creation tuple 与多实例

`MFCreateVirtualCamera` 相同 creation parameters 可重新打开 existing virtual camera。

因此：

- different `ManagedCameraId` 不保证得到不同 system camera。
- fixed source CLSID + same friendly name + same lifetime/access/categories 必须视为 collision。

MF v1：

- `CurrentUser`。
- same user 下 friendly name 唯一。
- same creation tuple => existing/conflict。
- 不创建第二个 logical record 假装 system device 不同。

`Start` 后保存：

`MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK`

作为 system device identity。

## 12. MF v1 format model

这是当前方案的关键收敛点。

### 12.1 Persistent native mode

MF camera registration/config record 必须包含：

```text
native_width
native_height
native_fps_num
native_fps_den
subtype = NV12
```

不同于 DShow，MF v1 的 mode 是 camera instance capability 的一部分。

### 12.2 Advertised media type 与 stream attributes

source 在没有 producer 时也必须能构造：

- presentation descriptor。
- stream descriptor。
- media type handler。
- source/stream attributes。

第一版只 advertise 一个 native NV12 media type。

media type 至少设置：

- `MF_MT_MAJOR_TYPE = MFMediaType_Video`
- `MF_MT_SUBTYPE = MFVideoFormat_NV12`
- `MF_MT_INTERLACE_MODE = MFVideoInterlace_Progressive`
- `MF_MT_ALL_SAMPLES_INDEPENDENT = TRUE`
- `MF_MT_FRAME_SIZE`
- `MF_MT_FRAME_RATE`
- `MF_MT_PIXEL_ASPECT_RATIO = 1:1`
- 正确 `MF_MT_AVG_BITRATE`

stream/source attributes 至少验证并设置：

- `MF_DEVICESTREAM_STREAM_CATEGORY = PINNAME_VIDEO_CAPTURE`
- `MF_DEVICESTREAM_STREAM_ID = 0`
- `MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES = MFFrameSourceTypes_Color`
- `MF_DEVICESTREAM_FRAMESERVER_SHARED = 1`，若目标 OS/SDK contract 支持且实测符合预期。

Camera Profile 不默认照抄 Microsoft sample。

Phase 0B 明确验证两种策略：

```text
A. 不发布 MF_DEVICEMFT_SENSORPROFILE_COLLECTION
   -> 验证普通 Win11 / FrameServer / browser / conferencing 兼容性

B. 发布 profile collection
   -> 必须包含 KSCAMERAPROFILE_Legacy
   -> profile filter 必须与该 camera 的真实 native mode 相容
```

不要直接复制 sample 的 `FRT<=30` / `FRT>=60` filter；例如 60fps-only camera 如果 Legacy profile 过滤掉唯一 native mode，会得到错误 capability。

其他 sensor/profile attributes 只在官方 contract 或目标兼容性确实需要时增加，不为“看起来完整”堆属性。

### 12.3 Producer open

MF producer open 传入 `ManagedStreamConfig` 时：

- 必须和 persisted native mode 完全匹配；或
- future API 可以让 MF open 省略 stream config，直接读取 camera native mode。

v1 不隐式 resize/convert。

### 12.4 Native mode immutable

MF v1 的 native mode 在 camera instance 创建后视为 immutable。

不得对已经存在的 camera 原地把：

```text
1080p60 -> 720p30
```

仅靠更新 manager metadata 或 vendor attributes 完成。

原因是正在运行/缓存的 FrameServer source 可能仍持有旧 presentation descriptor/media type，形成 source、metadata 和 producer 的 split-brain。

修改 native mode = recreate transaction：

1. 创建/验证 replacement configuration。
2. 按 rename/recreate 同类 transaction 切换 system camera。
3. 获取新的 symbolic link。
4. commit metadata。
5. Remove old instance。

具体先创建新还是先移除旧，由 collision/rollback 测试决定。

### 12.5 Future multi-mode

未来允许多个 media type 时，control ownership 仍是：

```text
FrameServer/client selects media type
       |
       v
MfMediaSource
       |
       v
mf_control publishes selected mode
       |
       v
producer must match/adapt
```

而不是反过来。

## 13. MF RequestSample correctness

MF delivery 必须 RequestSample-driven，但 v1 **同步优先，不预设自建异步 work queue**。

### 13.1 v1 synchronous path

第一版优先直接在 `RequestSample(pToken)` 中完成：

1. 验证 stream 为 running。
2. 从 pipeline-provided allocator 获取 sample。
3. 获取/锁定 destination buffer。
4. 复制 latest shared frame；无 producer 则填 black。
5. 设置 sample time/duration。
6. 如果 `pToken != null`：
   `sample.SetUnknown(MFSampleExtension_Token, pToken)`。
7. queue `MEMediaSample`。
8. 返回。

这里没有 hardware blocking I/O；shared-memory -> MF buffer copy 应先按同步路径测量，而不是提前增加 pending-request state machine。

### 13.2 Token contract

即使同步实现，也必须保持 token contract：

- 非空 token 在调用期间保持有效 COM reference。
- 返回的 sample 必须携带同一个 `MFSampleExtension_Token`。
- 任意 failure path 不泄漏 token/sample/buffer reference。

如果以后改为 async：

- 才引入 FIFO pending requests。
- 每个 Start 引入 generation ID。
- Stop/Shutdown cancel pending generation。
- stale work item 不允许跨 Start 投递。

不要在 v1 未证明需要前预先实现这些异步复杂度。

### 13.3 Wrong-state behavior

`RequestSample` 的 stopped-state contract 直接锁死：

- source/stream 已 Stop：返回 `MF_E_MEDIA_SOURCE_WRONGSTATE`。
- source/stream 已 Shutdown：返回 `MF_E_SHUTDOWN`。
- 不能用 `E_FAIL`、`MF_E_INVALIDREQUEST`、`MF_E_INVALID_STATE_TRANSITION` 等 generic/近似错误替代 stopped-state 的 `MF_E_MEDIA_SOURCE_WRONGSTATE`。

这是 pipeline correctness contract，不等 Phase 0B 再决定。Phase 0B 只做真实 FrameServer/SourceReader 回归验证。

### 13.4 Error signaling

同步-first v1 的 data-path error 只走一条 signaling path：

- 在 `RequestSample` 调用期间检测到 active transport corruption / frame validation failure / allocator failure：直接返回 HRESULT，不同时 queue `MEError`。
- 在没有对应 `RequestSample` 调用的后台/控制路径检测到真正 fatal source error：才 queue `MEError`。
- no producer / stale producer 是正常 black-fallback 状态，不发送 `MEError`。

不要对同一个 failure 同时“方法返回失败 + queue MEError”，避免 pipeline 收到重复 fatal signaling。

### 13.5 Latency gate for async

Phase 0B 测量同步 `RequestSample`：

- 1080p30。
- 1080p60。
- 若计划支持则测试 4K。
- producer present / black fallback 两种路径。

只有同步 copy/allocator 明显阻塞 Frame Server 或超过可接受预算时，才进入独立 async/work-queue 优化。

## 14. MF sample allocator 与 pitch-aware NV12 copy

### 14.1 Allocator

`IMFSampleAllocatorControl` 是 **preferred path，不是 MF source 正确性的硬前提**。

优先实现：

```text
GetAllocatorUsage
  -> MFSampleAllocatorUsage_UsesProvidedAllocator

SetDefaultAllocator
  -> QI IMFVideoSampleAllocator
  -> 仅在 stream 非 RUNNING 时替换/保存 allocator
```

provided allocator 的生命周期必须完整：

```text
Start(selected media type)
    -> InitializeSampleAllocator / InitializeSampleAllocatorEx
       使用当前 selected/native media type

RequestSample
    -> AllocateSample

Stop / media-type replacement / Shutdown
    -> UninitializeSampleAllocator（若 allocator contract/实现需要）
    -> 释放或重建与旧 media type 绑定的 allocator state
```

不能只 `SetDefaultAllocator -> retain -> AllocateSample`；未初始化 allocator 的 `AllocateSample` 可能返回 `MF_E_NOT_INITIALIZED`。

如果 FrameServer/pipeline 没有提供可用 allocator，标准 fallback 合法且必须可工作：

```text
MFCreateSample
+
MFCreate2DMediaBuffer / 其他合适的官方 MF buffer API
```

因此 Phase 0B 的最小闭环不能因为 `IMFSampleAllocatorControl` callback 行为和预期不同就被阻断。

禁止自建 custom frame `IMFMediaBuffer`。

### 14.2 Shared frame layout

shared queue 中的 producer NV12 仍是 tightly packed：

```text
Y:
height rows
width bytes each

UV:
height/2 rows
width bytes each
```

### 14.3 Destination is not necessarily tightly packed

MF allocator 的 destination buffer 不能假设：

```text
pitch == width
```

优先：

1. `IMF2DBuffer2::Lock2DSize`
2. fallback `IMF2DBuffer::Lock2D`
3. fallback `IMFMediaBuffer::Lock`

对于 2D buffer：

- 使用返回的 pitch。
- 处理正/负 pitch。
- 使用 bufferStart/bufferLength 做边界检查。
- Y plane 按行复制 width bytes。
- UV plane 按实际 2D layout/pitch 按行复制 width bytes。
- 不用单个 `memcpy(width*height*3/2)` 直接覆盖 MF destination。
- 解锁必须保证 error path 也执行。

### 14.4 Black frame

black NV12：

- Y = 0x10。
- UV = 0x80。

同样按 destination pitch 填充，而不是假定 contiguous width stride。

## 15. MF timestamp

所有 output sample：

- SetSampleTime。
- SetSampleDuration。
- QPC-based。
- 优先 `MFGetSystemTime()`。
- duration = negotiated/native frame interval in 100ns。

producer queue timestamp/sequence 只作：

- freshness。
- dropped frame detection。
- latency telemetry。

不直接成为 MF presentation clock。

## 16. MF source state 与事件协议

Source 至少正确处理：

- CreatePresentationDescriptor。
- GetCharacteristics。
- Start。
- Stop。
- Pause。
- Shutdown。
- GetSourceAttributes。
- GetStreamAttributes。
- SetD3DManager。
- event queue。
- reentrancy。

`GetSourceAttributes()` 返回 source 持有的同一个 attribute store reference；`GetStreamAttributes()` 返回 stream 持有的同一个 store reference，不 clone/copy 新 store。

`GetCharacteristics()` 返回 live-source characteristics，v1 至少包含：

`MFMEDIASOURCE_IS_LIVE`

Stream：

- GetMediaSource。
- GetStreamDescriptor。
- RequestSample。
- Get/SetStreamState。
- Shutdown。

### 16.1 Start event/state contract

不要把不同 Microsoft 文档/sample 之间并不完全一致的**全局事件顺序**当成 ABI。

v1 锁死的是必需事件和状态关系：

- 首次暴露/选择一个新 stream：source 必须产生 `MENewStream(stream)`。
- 已存在/已知 stream 再 Start：按 prior selection/state 使用 `MEUpdatedStream`，不伪装成新 stream。
- stream 成功进入 running：产生 `MEStreamStarted`。
- source 成功进入 started：产生 `MESourceStarted`。
- 不依赖 `MENewStream`、`MEStreamStarted`、`MESourceStarted` 三者在不同 queue 间具有唯一全局总顺序。
- event payload/time 采用当前正式 contract/sample 可接受形式；测试不要把 sample 版本里的可选 payload 当 ABI。

presentation descriptor / Start 参数必须验证：

- stream count/id 正确。
- 至少一个 stream selected。
- selected media type 是本 source 支持的 native type。
- `pguidTimeFormat == nullptr || GUID_NULL`；其他 time format 返回 `MF_E_UNSUPPORTED_TIME_FORMAT`。
- `pvarStartPosition` 只接受 live non-seek source 合法形式：`VT_EMPTY`，以及需要兼容时的 `VT_I8 == 0`。
- 非零 `VT_I8` 视为 seek 并拒绝。
- 其他不支持的 variant type 返回对应 invalid/unsupported argument error。

### 16.2 Stop event/state contract

Stop 必须产生：

- selected/running stream 的 `MEStreamStopped`。
- source 的 `MESourceStopped`。

同样不把跨 source/stream 两个 event queue 的唯一总顺序写成 ABI。

Stop 后更新 internal presentation descriptor/selection/stream state，使后续 `RequestSample` 返回 `MF_E_MEDIA_SOURCE_WRONGSTATE`。

### 16.3 Live camera 禁止 EOS 语义

普通 Stop、producer absence 或 temporary transport failure 都不是 end-of-stream。

v1 不因这些情况发送：

- `MEEndOfStream`
- `MEEndOfPresentation`

无 producer 时继续按 native format 提供 black sample。

### 16.4 Event queue locking

`IMFMediaEventGenerator::GetEvent()` 可能无限阻塞，因此 **禁止持有 source/stream state mutex 调用 `GetEvent()`**。

正确模式：

```text
lock
  check shutdown
  clone/AddRef IMFMediaEventQueue
unlock

queue.GetEvent(...)
```

`BeginGetEvent` / `EndGetEvent` / `QueueEvent` 仍按各自 thread-safety contract 实现，但不能设计成一个 blocking `GetEvent` 占住主状态锁后导致 Stop/Shutdown/QueueEvent 永久等待。

Source 和 Stream 两套 event queue 都遵守此规则。

### 16.5 IMFMediaStream2 state contract

v1 至少锁死：

```text
MF_STREAM_STATE_RUNNING
    -> RequestSample 可生产 sample

MF_STREAM_STATE_STOPPED
    -> RequestSample 返回 MF_E_MEDIA_SOURCE_WRONGSTATE
```

`MF_STREAM_STATE_PAUSED` 不要与 source-level `IMFMediaSource::Pause()` 混为一谈。source-level Pause 明确不支持；stream-level PAUSED 的 FrameServer 实际调用模式放在 Phase 0B 验证后定稿。

`SetStreamState` / `GetStreamState` 必须保持内部状态一致，不允许出现 source 已 Stop 而 stream 仍对 `RequestSample` 报 running。

### 16.6 Source/Stream ownership、Shutdown 与锁顺序

Source 通常持有 Stream strong reference，而 Stream 为实现 `GetMediaSource()` 也可能持有 Source strong reference，因此必须显式处理 circular reference。

v1 Shutdown contract：

```text
Source::Shutdown
  1. source state -> Shutdown
  2. signal rendezvous stop / cancel pending overlapped pipe I/O
  3. wait until rendezvous worker exits without holding source/stream state locks
  4. close rendezvous pipe / transport-generation control handles
  5. shutdown/release source event queue
  6. 对每个 stream 调 Stream::Shutdown
       - stream state -> stopped/shutdown
       - break/reset parent source reference
       - shutdown/release stream event queue
       - release allocator/media type/descriptors/transport refs
  7. source release stream list
  8. release presentation descriptor / source attributes / remaining transport refs
```

不能只依赖 Rust/COM reference counting 自动收集 source<->stream cycle。

锁层级规则：

- 默认只允许 `Source -> Stream` 的双对象调用方向。
- Stream hot path 在持 stream state lock 时，不调用需要 source state lock 的方法。
- `GetMediaSource()` 只复制/AddRef 已保存 parent interface，不回调 source stateful method。
- Shutdown/Start/Stop 中如需跨对象调用，必须审查锁顺序，禁止 Source-lock -> Stream-lock 与 Stream-lock -> Source-lock 同时存在造成 ABBA deadlock。
- 等待 rendezvous worker 退出、等待 overlapped cancellation completion、blocking `GetEvent` 都必须在主 source/stream state lock 外。
- worker 不得持有 source state lock 再等待 client I/O；只在短临界区读取/提交 generation/auth state。

### 16.7 Unsupported-service/control stubs

CPU-only synthetic MF v1 对暂不支持的接口行为直接固定：

- `IMFGetService::GetService`：无支持 service 时返回 `MF_E_UNSUPPORTED_SERVICE`。
- `IKsControl::KsProperty/KsMethod/KsEvent`：不支持的 set/control 返回 `HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND)`；未来实现 sync/control event 时只开放明确支持的 set。
- `IMFMediaSourceEx::SetD3DManager`：CPU-only v1 返回 `E_NOTIMPL`，直到真正支持 DXGI/D3D sample path。

不要返回 `S_OK` 但什么也不做，也不要统一用 `E_FAIL`。

### 16.8 Shutdown method contract

source/stream 进入 Shutdown 后，除 `IUnknown` 引用计数/QI 等 COM 基础行为外，公开 MF methods 应按各自 contract 返回 `MF_E_SHUTDOWN`，不得访问已经释放的 event queue、attributes、allocator 或 transport。

实现时使用统一的 `check_shutdown()`/等价 helper，避免每个 method 自己拼状态判断。

参数指针检查和 shutdown 检查的先后顺序按具体 Windows interface contract 保持一致；不要为了 helper 统一而改变明显可观察 HRESULT。

### 16.9 Other state rules

Frame Server custom source：

- 不支持 seek。
- `Pause` 返回 `MF_E_INVALID_STATE_TRANSITION`；shutdown 后返回 shutdown error。
- 不生成 seeked events。
- expensive resource acquisition 不放在 COM activation constructor 中。
- Shutdown 后 source/stream event queue 按 contract shutdown。

## 17. MF virtual-camera management lifecycle

MF v1 进一步收敛为：

```text
access   = CurrentUser
lifetime = System
```

原因：当前 manager API 是注册/枚举/删除型，不持有一个长期 `IMFVirtualCamera` ownership handle。Session lifetime 依赖对应 `IMFVirtualCamera` 对象生命周期，如果 `register()` 返回后 COM object 被释放，camera 生命周期就不再稳定。

因此 v1 **不支持 Session lifetime**。

未来若需要 Session lifetime，单独设计 RAII API，例如：

```text
MfSessionCameraHandle
  owns IMFVirtualCamera for the whole session lifetime
```

由 handle 明确维持生命周期，而不是让普通 metadata record 假装拥有 session camera。

不要把 access/lifetime 两个维度混在一起。

### Configured

`MFCreateVirtualCamera` 返回对象，还没首次成功 Start。

### Started

`Start` 成功：

- virtual camera 注册/可枚举。
- symbolic link available。

### Stopped

`Stop`：

- 禁止新 enumerate/activate。
- 不等于 Remove。

### Removed

`Remove`：

- disable + unregister。
- permanent delete 必须走这里。

### Shutdown

`Shutdown`：

- 释放 virtual camera object resources。
- v1 仅使用 System lifetime，因此不能把 Shutdown 当 unregister。
- permanent delete 仍必须 `Remove`。

Session lifetime 的“释放/Shutdown 即结束 camera lifetime”只作为 future RAII design 的平台事实记录，不进入 v1 manager contract。

## 18. MF producer-first / source activation spike

不要提前把 MF open 定义成必须 `ReceiverInactive`，但也不能把 `GetMediaSource()` 本身误认为 stream 已经 Start。

Phase 0B 必须验证完整 shared-client 路径：

```text
manager.open(MF)
   |
MFCreateVirtualCamera(same tuple)
   |
IMFVirtualCamera::Start()
   |
IMFVirtualCamera::GetMediaSource()
   |
CreatePresentationDescriptor / select native stream
   |
通过 SourceReader 或等价 shared-client pipeline 真正启动读取
   |
MfMediaSource::Start
   |
transport created / RequestSample begins
```

`GetMediaSource()` 返回的是 reduced-function/shared-client media source，生命周期绑定对应 `IMFVirtualCamera`；**取得 interface 不等于 source stream 已经运行**。

需要验证：

- shared-client Start/read 是否稳定激活 transport。
- 保持 `IMFVirtualCamera + shared-client reader/source` 是否允许 producer-first。
- keepalive 是否影响其他普通 camera clients。
- producer close 后如何释放 shared-client resources 而不 Remove system camera。

如果成立：

- MF producer API 可以支持 producer-first。
- `ManagedCamera` 内部持有 virtual-camera + shared-client keepalive。

如果不成立：

- MF backend 明确返回 `ReceiverInactive/NotReady`。

禁止用 exclusive/full-control `MFCreateDeviceSource` 作为常驻 keepalive，避免锁住其他 camera clients。

## 19. MF sync APIs

Phase 0B 评估：

- `IMFVirtualCamera::CreateSyncEvent`
- `CreateSyncSemaphore`
- source `IKsControl::KsEvent`

候选用途：

- source-ready。
- consumer-active。
- restart/shutdown。
- rendezvous。

frame payload 继续 shared memory。

不使用 per-frame KsEvent 代替 queue。

## 20. MF CurrentUser + System-only v1

MF v1 明确：

```text
access   = CurrentUser
lifetime = System
```

不支持 `AllUsers`，也不支持 Session lifetime。

原因：

- CurrentUser owner/authentication policy 清晰。
- per-user manager metadata 简单。
- cross-user isolation 可以严格验证。
- System lifetime 与 register/list/get/unregister 型 manager API 一致，不需要长期持有 `IMFVirtualCamera` RAII object。
- 避免同时解决“多个用户谁是 producer owner”和“谁持有 Session camera lifetime”的问题。

未来单独设计：

AllUsers：
- producer authorization。
- writer arbitration。
- multi-session policy。
- machine-scoped record。
- admin lifecycle。

Session lifetime：
- RAII `MfSessionCameraHandle`。
- explicit COM object ownership。
- drop/shutdown semantics。

## 21. MF capability / Auto

最低 API build：22000。

runtime capability 使用：

`MFIsVirtualCameraTypeSupported(MFVirtualCameraType_SoftwareCameraSource)`

`Auto` 仅在以下都满足时考虑 MF：

- API available。
- type supported。
- mf-source installed/version compatible。
- current privacy/policy/access允许。
- camera config/native mode合法。

候选：

```text
Win11 + MF supported
    -> MF

otherwise
    -> DShow
```

但 Phase 0B 必测 MF-created camera 对：

- MF capture。
- OpenCV CAP_DSHOW。
- OBS。
- Chrome/Edge。
- conferencing apps。

如果 MF device 已覆盖主要 Win11 DirectShow consumer，则 Win11 上 DShow 定位为显式 compatibility backend。

## 22. DShow -> Rust source 升级策略

在删除旧 C++ DLL 前必须处理已经注册的 camera。

现有 registration 包含：

- camera CLSID。
- friendly name。
- queue identity。
- `InprocServer32 -> old virtualcam-managed.dll`。

v1 项目目前处于早期阶段，因此默认选择**显式 breaking migration**，而不是实现复杂兼容 upgrader：

```text
old managed camera
   |
unregister with old implementation while DLL still exists
   |
install/register Rust dshow-source
   |
recreate camera
```

规则：

- Phase 3 删除 C++ 前必须提供 migration/cleanup command 或明确 release-note 操作。
- 不允许先删除 old DLL 再留下 broken COM registration。
- 若用户后续要求 preserve CLSID/device identity，再单独设计 in-place `InprocServer32` migration transaction。

## 23. MF rename

MF friendly name 属于 creation tuple。

第一版按 recreate transaction 处理。

MF rename 不保证 symbolic link/system identity 保持。

最终 public API宜返回新的 `ManagedCameraInfo`。

## 24. Rust workspace

目标：

```text
root virtualcam
├── producer APIs
├── manager
└── backend adapters

crates/
├── managed-protocol/
│   ├── frame_queue
│   ├── publication helpers
│   ├── dshow_control
│   └── mf_control
├── dshow-source/   # cdylib
└── mf-source/      # cdylib
```

Binding：

- Win32 low-level：`windows-sys` 可继续使用。
- COM implementation：优先 `windows` crate。
- 只有 bindings 不足时局部手写 vtable。

## 25. 性能原则

Producer：

```text
host/CUDA D2H -> shared-memory slot -> publish
```

DShow：

```text
shared queue -> IMediaSample
```

MF：

```text
shared queue -> pipeline-provided pitched MF buffer
```

允许的必要 copy 不等于允许多余 staging。

禁止：

- per-frame registry。
- per-frame object reopen，recovery 除外。
- per-frame Vec allocation。
- hidden resize/conversion。
- whole-frame memcpy into pitched MF buffer。
- historical burst catch-up。

## 26. 错误模型

DirectShow：

- active producer corruption != idle。
- source missing != camera not found。
- rollback failure 单独暴露。

MF 至少区分：

- unsupported OS/API。
- virtual camera type unsupported。
- source not installed。
- source ABI incompatible。
- privacy denied。
- name/creation-tuple conflict。
- native-mode mismatch。
- source inactive。
- transport not ready。
- transport access denied。
- producer already active。
- metadata/system-device mismatch。
- wrong stream state。

## 27. 安全要求

### DLL

- stable local path。
- reject network source。
- 不依赖 cwd DLL search。
- minimal dependency surface。
- active FrameServer/third-party process load 下安全 update/uninstall。
- 优先使用版本化 DLL 文件名 + registration 原子切换，而不是覆盖正在被加载的同名 DLL。
- 新 registration 生效后，旧 DLL 文件等确认无进程持有后再清理。
- DirectShow x86/x64 artifact 必须分别管理，不能让一个 registry view 指向错误 bitness DLL。

### Shared memory

- source 不信任 producer header。
- magic/version/state验证。
- checked offset/size arithmetic。
- frame dimensions和payload length验证。
- publication ordering正确。
- partial/stale frame可恢复。

### User isolation

- CurrentUser v1。
- owner SID。
- no Everyone write。
- second user denied。
- Fast User Switching测试。
- service HKCU 不作为 owner metadata source。

## 28. 实施阶段

### Phase 0A — DirectShow baseline + protocol ordering oracle

补齐：

- ABI/layout。
- publication sequence。
- no producer black。
- producer-first。
- consumer-first。
- stale/crash。
- old/new mapping。
- allocator/sample。
- timestamps。
- rollback。
- 高频 writer + 故意慢 reader 的 3-slot torn-frame stress。
- pattern/checksum 验证，记录当前 ABI 是否真实出现 torn frame。
- x64/x86 current registration/activation baseline（若现有 C++ build 支持）。

### Phase 0B — MF architecture spike

#### Activation / routing

1. fixed CLSID -> IMFActivate。
2. custom attributes reach activation。
3. activation -> source attributes。
4. symbolic link。
5. reopen。
6. same activation object repeated `ActivateObject` instance behavior。
7. System lifetime + reboot。
8. manager releases/shuts down `IMFVirtualCamera` object but System camera remains registered until Remove。
9. vendor attribute persistence/recovery。

#### Identity / metadata

10. same tuple collision。
11. unique friendly name。
12. HKCU CurrentUser record。
13. Local Service source does not rely on own HKCU。

#### Format

14. source with one persistent native mode。
15. enumeration before producer exists。
16. presentation descriptor/media type。
17. producer native-mode mismatch。
18. Camera Profile strategy A/B；如果发布 profiles，Legacy profile 与 native mode 必须相容。
19. test whether dynamic mode update is rejected/ignored/possible; v1 still does not rely on it。

#### IPC/security

20. Local Service/Session 0。
21. source token / actual service principal。
22. vendor attributes treated as untrusted configuration, not authorization。
23. malicious owner/routing/native-mode attributes cannot alter authorization or trigger unbounded allocation。
24. named-pipe rendezvous bootstrap: formal vendor attrs `EXPECTED_OWNER_SID` assertion + random `RENDEZVOUS_ID` routing。
25. rendezvous pipe uses explicit DACL, `FILE_FLAG_FIRST_PIPE_INSTANCE`, `PIPE_REJECT_REMOTE_CLIENTS`, message mode and overlapped I/O；default pipe DACL禁止。
26. bounded fixed/small handshake message is successfully read before impersonation。
27. `ImpersonateNamedPipeClient` -> actual TokenUser SID compare -> identity level check -> always `RevertToSelf`；anonymous/insufficient identity rejected。
28. stalled/malformed client has handshake byte/time budget and cannot pin worker forever。
29. authentication is lazy/non-blocking: `ActivateObject` / source `Start` never wait for producer; unauthenticated state outputs black。
30. rendezvous worker cancellation/Shutdown/`DllCanUnloadNow` lifecycle verified；worker wait happens outside state locks。
31. Global named-object candidate creation only after/alongside authenticated generation setup。
32. source-generated generation nonce after authenticated attach。
33. mapping/event/semaphore/mutex same-type `ERROR_ALREADY_EXISTS` and cross-type `ERROR_INVALID_HANDLE` collision => reject/regenerate, never reuse attacker-owned object。
34. object-type-specific access masks/DACLs using documented Win32 rights (`FILE_MAP_*`, event/semaphore/mutex rights)。
35. unprivileged legitimate producer can attach after authentication。
36. second user denied。
37. same-user malicious process limitation is documented as outside v1 threat model。
38. Fast User Switching。
39. source/FrameServer restart creates fresh generation and stale transport cannot be reattached。
40. source never uses owner-HKCU runtime fallback。

#### Media pipeline

41. IMFMediaSourceEx。
42. IMFMediaStream2。
43. IMFActivate/IMFMediaSource free-threaded/FTM-or-equivalent marshaling spike。
44. `GetSourceAttributes` / `GetStreamAttributes` same-store reference semantics。
45. blocking `GetEvent` runs outside source/stream state mutex。
46. source<->stream circular reference is broken by Shutdown。
47. Source->Stream lock hierarchy；禁止 ABBA lock inversion。
48. preferred `IMFSampleAllocatorControl` path。
49. provided allocator Initialize/Allocate/Uninitialize lifecycle。
50. fallback `MFCreateSample + MFCreate2DMediaBuffer` path。
51. synchronous RequestSample token echo。
52. stopped RequestSample => `MF_E_MEDIA_SOURCE_WRONGSTATE`；shutdown => `MF_E_SHUTDOWN`。
53. RequestSample error signaling: return HRESULT xor queue `MEError`，same failure 不双报。
54. RequestSample latency under target modes；只有测量证明需要才做 async spike。
55. required Start event set/state transitions，不把跨 queue 全局顺序当 ABI。
56. required Stop event set/state transitions。
57. no EOS/EOP on normal stop or producer absence。
58. `MFMEDIASOURCE_IS_LIVE`。
59. mandatory/recommended stream attributes。
60. `IMFGetService` / `IKsControl` / `SetD3DManager` unsupported-path HRESULTs。
61. Start `pvarStartPosition` validation。
62. IMFMediaStream2 RUNNING/STOPPED state contract。
63. QPC timestamps。
64. pitch-aware NV12 copy。
65. black fallback。
66. producer attach/stale/restart。
67. post-Shutdown public MF methods return `MF_E_SHUTDOWN` without touching released state。

#### Producer-first

68. Start + GetMediaSource + actual shared-client Start/read 是否激活 transport。
69. shared-client keepalive 是否影响其他 camera clients。
70. producer close 后 shared-client cleanup。
71. 不使用 exclusive MFCreateDeviceSource 作为 producer keepalive。

#### Sync

72. CreateSyncEvent/CreateSyncSemaphore feasibility。

#### Installation / cleanup

73. User A + User B 各有 CurrentUser/System camera 时，admin source uninstall 能否枚举并清理本 source CLSID 的所有 camera。
74. cleanup 不完整时 installer fail safe，不先 unregister/delete source DLL。
75. source upgrade/uninstall 后无 orphan virtual-camera device。

#### Compatibility

76. MF consumer。
77. OpenCV CAP_DSHOW。
78. OBS。
79. Chrome/Edge。
80. conferencing clients。

Phase 0B 结束后更新本规划和 AGENTS，再冻结 production MF design。

### Phase 1 — Rust 化 DShow management

迁移：

- GUID。
- metadata。
- COM registration。
- `CLSID_VideoInputDeviceCategory` + `MERIT_DO_NOT_USE` registration contract。
- full REGFILTER2 v1 single-output-pin NV12 metadata parity。
- FilterMapper。
- list/get + partial/degraded-state detection。
- register/unregister。
- rename。
- mutex。
- rollback。
- source path。
- COM apartment ownership。
- x64 registry view。
- x86 WOW64 registry view。
- x64/x86 DirectShow category registration。
- 若保留 x86，使用 x86 registration helper/process 调用 x86 `IFilterMapper2`，不由 x64 manager 手工伪造 32-bit FilterData。
- parent transaction coordinator 持 global management mutex；x86 helper 不重复获取同一 transaction mutex，避免 parent/child 自锁。
- x64+x86 logical transaction：两边都成功后才 commit metadata，任一失败回滚另一边。

若最终 release 明确取消 x86 支持，必须在这一阶段显式记录，而不是遗漏。

### Phase 2 — Pure Rust DirectShow source

实现并通过 parity，包括：

- x64 source DLL。
- x86 source DLL（若仍在产品 scope）。
- COM/object/module lock。
- minimal `DllMain` / loader-lock discipline。
- `InprocServer32 ThreadingModel=Both` registration contract。
- panic/FFI catch boundary。
- allocator/sample delivery。
- protocol publication helpers。
- `IAMStreamConfig` / `IPin::QueryAccept` / media-type enumeration parity。
- real x64/x86 consumer activation tests。

### Phase 3 — DShow migration + 删除 C++

顺序：

1. 提供 old-camera cleanup/migration。
2. 确认无 broken registration。
3. 删除 C++ source/CMake/libdshowcapture。
4. 更新 license/docs/CI。

### Phase 4 — Production MF source

**只有 Phase 0B producer-authentication / owner-binding 方案验证通过后才能进入本阶段。**

按 0B 固化的：

- fixed immutable native mode。
- IMFActivate routing + repeated ActivateObject instance policy。
- vendor attributes are untrusted config with strict validation。
- named-pipe authenticated producer bootstrap：formal expected-owner assertion + rendezvous id attributes。
- rendezvous pipe explicit DACL + `FILE_FLAG_FIRST_PIPE_INSTANCE` + `PIPE_REJECT_REMOTE_CLIENTS` + message mode + overlapped I/O。
- bounded handshake read completes before `ImpersonateNamedPipeClient`；token SID/identity-level validation；all-path `RevertToSelf`。
- stalled/malformed client deadline + cancellable rendezvous worker；pending worker/I/O keeps `DllCanUnloadNow=S_FALSE`。
- authentication is lazy/non-blocking：source activation/Start succeeds without producer and outputs black。
- authenticated producer binding based on Windows token/impersonation/handle semantics。
- source-owned transport generation + source-generated nonce after authenticated attach。
- named-object pre-creation/squatting rejection。
- object-type-specific ACL/access masks。
- no owner-HKCU runtime fallback。
- free-threaded/FTM-or-equivalent COM marshaling strategy。
- source-owned IPC。
- required source/stream event + state contract，不锁死跨 queue 全局顺序。
- same-store source/stream attribute semantics。
- blocking `GetEvent` lock discipline。
- source<->stream Shutdown 拆环 + Source->Stream 单向锁层级。
- stream attributes / profile strategy。
- preferred provided allocator + Initialize/Allocate/Uninitialize lifecycle。
- standard MF fallback allocator path。
- pitch copy。
- synchronous-first RequestSample/token path。
- stopped `RequestSample => MF_E_MEDIA_SOURCE_WRONGSTATE`。
- RequestSample error signaling 不重复 HRESULT + `MEError`。
- `IMFGetService` / `IKsControl` / `SetD3DManager` 明确 unsupported HRESULT。
- Start-position validation。
- IMFMediaStream2 RUNNING/STOPPED contract。
- post-Shutdown methods return `MF_E_SHUTDOWN` without touching released state。
- 只有 Phase 0B latency gate 证明必要时才加入 async work queue。
- timestamp。
- panic/FFI catch boundary。
- restart。

### Phase 5 — MF manager

- x64-only v1 deployment scope。
- CurrentUser only。
- System lifetime only。
- register/list/get。
- Start/Stop/Remove/Shutdown。
- rename/recreate。
- orphan reconciliation。
- HKCU records。
- source upgrade/uninstall 前枚举并清理本 source CLSID 的 persistent cameras。
- two-user/Fast User Switching uninstall cleanup 验证。
- cleanup 无法证明完整时 fail safe，不先删除 source COM registration/DLL。

### Phase 6 — Unified public API / Auto

最后处理：

- backend enum。
- constructor。
- DShow runtime-stream config。
- MF persistent native-mode config。
- backend-specific info。
- Auto。

## 29. Public API 方向

不要强行让两个 backend 的 registration config 完全同形。

目标可能是：

```rust
pub enum ManagedCameraRegistration {
    DirectShow {
        name: String,
    },
    MediaFoundation {
        name: String,
        native_stream: ManagedStreamConfig,
    },
}
```

MF v1：

- access 固定 CurrentUser。
- lifetime 固定 System，不暴露可配置 `lifetime` 参数。
- native stream registration-time 固定。

DShow：

- stream 仍由 producer `open` 时决定。

也可以保留统一 config + backend-specific sub-config，但不要隐藏这个真实语义差异。

## 30. 测试矩阵

### Common frame protocol

- queue offsets。
- layout。
- frame-size overflow。
- sequence wrap。
- writer publication ordering。
- reader observation ordering。
- stale/partial publish。
- high-rate writer + slow-reader torn-frame stress。
- pattern/checksum validation。
- x64。
- ARM64（若 CI/硬件可用）。

### DShow

- CoCreateInstance x64。
- CoCreateInstance x86（若 scope 保留）。
- 64-bit registry view。
- 32-bit WOW64 registry view。
- `CLSID_VideoInputDeviceCategory` registration。
- `MERIT_DO_NOT_USE` preserved。
- REGFILTER2 v1 single output pin metadata exactly preserved。
- FilterMapper media type `MEDIATYPE_Video / MEDIASUBTYPE_NV12` preserved。
- output pin `PIN_CATEGORY_CAPTURE` via `IKsPropertySet`。
- DirectShow category enumeration in both bitnesses。
- x86 registration helper/FilterMapper path if x86 supported。
- parent holds transaction mutex; child helper does not reacquire it。
- x64+x86 register/rename/unregister transaction rollback。
- partial/degraded registration detection in list/get。
- minimal DllMain / no loader-lock work。
- InprocServer32 ThreadingModel=Both。
- `GetFormat`。
- `GetNumberOfCapabilities`。
- `GetStreamCaps`。
- valid/invalid `SetFormat`。
- `IPin::QueryAccept`。
- `IEnumMediaTypes` enumeration/reset/clone。
- idle fallback and producer-mode media-type parity。
- allocator。
- graph run/stop。
- black。
- producer crash。
- multiple camera。
- COM apartment ownership。
- panic boundary converts to HRESULT rather than killing host。
- rollback。
- migration cleanup。

### MF source

- IMFActivate。
- repeated ActivateObject instance behavior。
- E_NOTIMPL DetachObject/ShutdownObject。
- free-threaded/FTM-or-equivalent cross-apartment/cross-process marshaling。
- vendor attributes。
- `GetSourceAttributes` returns the same source store reference。
- `GetStreamAttributes` returns the same stream store reference。
- blocking `GetEvent` outside source/stream state mutex。
- source<->stream circular reference is broken on Shutdown。
- Source->Stream lock hierarchy; no ABBA inversion。
- descriptors。
- one immutable native media type。
- required stream/source attributes。
- Camera Profile strategy with Legacy compatibility if profiles are published。
- `MFMEDIASOURCE_IS_LIVE`。
- required Start events/state transitions；不测试唯一跨 queue 总顺序。
- required Stop events/state transitions。
- no EOS/EOP on normal stop/producer absence。
- preferred provided allocator。
- provided allocator Initialize/Allocate/Uninitialize lifecycle。
- standard `MFCreateSample + MFCreate2DMediaBuffer` fallback。
- synchronous RequestSample token echo。
- stopped RequestSample returns `MF_E_MEDIA_SOURCE_WRONGSTATE`。
- shutdown RequestSample returns `MF_E_SHUTDOWN`。
- RequestSample error signaling does not double-report HRESULT + `MEError`。
- unsupported GetService/KsControl/SetD3DManager HRESULTs。
- Start-position validation。
- IMFMediaStream2 RUNNING/STOPPED state contract。
- post-Shutdown public methods return `MF_E_SHUTDOWN` without dereferencing released state。
- RequestSample latency budget。
- pitch positive/negative。
- black NV12 with pitch。
- timestamps。
- panic boundary converts to HRESULT rather than killing FrameServer。
- source restart。

### MF management/security

- x64-only v1 MF deployment scope。
- CurrentUser。
- System lifetime/reboot。
- manager object release/Shutdown does not substitute for Remove。
- same tuple reopen/conflict。
- HKCU bookkeeping only; source never uses owner-HKCU runtime fallback。
- vendor attributes are untrusted and malicious oversized values are rejected safely。
- formal `VIRTUALCAMRS_EXPECTED_OWNER_SID` / `VIRTUALCAMRS_RENDEZVOUS_ID` semantics。
- named pipe explicit DACL；default pipe DACL rejected。
- `FILE_FLAG_FIRST_PIPE_INSTANCE` + `PIPE_REJECT_REMOTE_CLIENTS` + message mode + overlapped I/O。
- bounded handshake read occurs before impersonation。
- actual client TokenUser SID/identity-level validation；all paths `RevertToSelf`。
- stalled/malformed client timeout/byte budget。
- rendezvous worker cancellation and `DllCanUnloadNow` lifetime。
- authentication does not block ActivateObject/Start; no producer => black。
- source-generated generation nonce only after authenticated attach。
- `ERROR_ALREADY_EXISTS` same-type collision rejection。
- `ERROR_INVALID_HANDLE` cross-type namespace collision rejection。
- object-type-specific ACL/access masks。
- Local Service routing。
- cross-user denial。
- same-user malicious-process limitation documented as outside v1 threat model。
- Fast User Switching。
- stale generation cannot attach after source restart。
- privacy denied。
- two-user persistent-camera uninstall cleanup。
- installer fail-safe when cleanup incomplete。
- orphan repair。

### Formats

至少：

- 640x480@30。
- 1280x720@30。
- 1920x1080@30。
- 1920x1080@60。
- 30000/1001。

第一版 MF 每台 camera advertise其中一个 native mode，不是一次 advertise全部。

## 31. 明确非目标

当前计划不包括：

- kernel camera driver。
- Linux/macOS。
- audio。
- UVC emulation。
- image resize。
- color conversion。
- dynamic MF multi-mode source。
- AllUsers MF access。
- D3D/GPU zero-copy。
- per-camera MF source CLSID。
- same logical camera默认双注册。
- producer elevation。
- Everyone-writable Global object。
- service-account HKCU runtime routing。
- producer-selected arbitrary MF runtime mode。
- whole-frame copy到未知 pitch destination。
- per-frame registry lookup。

## 32. 完成定义

只有满足以下条件才完成：

1. Rust DShow management。
2. Rust DShow source/filter。
3. DShow preserves full REGFILTER2/category/merit/NV12 pin metadata and runtime `PIN_CATEGORY_CAPTURE` contract。
4. DShow x64/x86 support scope 明确；cross-architecture transaction、parent/child mutex ownership、rollback、partial-state detection、registry/FilterMapper/consumer tests 全通过。
5. DShow `IAMStreamConfig` / QueryAccept / media-type enumeration parity通过。
6. DShow/MF source DLL `DllMain` minimal-loader-lock contract 与 `ThreadingModel=Both` registration contract 通过。
7. Rust MF activation/source/stream。
8. MF activation/source 的 free-threaded/FTM-or-equivalent marshaling 经过 FrameServer/MTA 实测。
9. repeated `ActivateObject` source-instance policy 有实测结论。
10. MF vendor attributes 已按 untrusted input 处理，malformed/oversized input 不可越权或造成 unbounded allocation。
11. named-pipe producer authentication bootstrap使用显式 DACL、first-instance/local-only/message/overlapped pipe，default pipe security不被使用。
12. bounded handshake 在 impersonation 前完整读取；actual TokenUser SID/identity-level compare、all-path `RevertToSelf` 已实测闭环。
13. stalled/malformed client 无法永久占用 rendezvous worker；Shutdown 可取消 I/O/退出 worker，worker 存活时 `DllCanUnloadNow=S_FALSE`。
14. authentication 不阻塞 ActivateObject/Start；未认证 producer 时 camera 持续 black。
15. source-generated generation nonce、same/cross-type named-object squatting rejection 和 object-type-specific ACL tests 通过。
16. v1 threat model 明确：防 cross-user/remote/unauthenticated injection，不承诺隔离同一 Windows user 下恶意进程。
17. HKCU 仅用于 manager bookkeeping，source 不存在跨用户 HKCU runtime fallback。
18. MF v1 明确 x64-only + CurrentUser + System lifetime；Session/ARM64 不被误报为已支持。
19. multi-user persistent System-camera uninstall cleanup 已验证；cleanup不完整时installer fail safe且不先删除source registration/DLL。
20. Rust MF manager。
21. common frame queue + backend-specific control-plane 明确。
22. memory ordering helpers 固化并测试。
23. 当前 3-slot torn-frame 风险有压力测试结论；若保留 v1 ABI，限制已记录。
24. MF v1 immutable native-mode model验证。
25. MF required event/state contract测试通过，且不把 sample-specific 全局事件顺序当 ABI。
26. same-store attributes、blocking GetEvent lock discipline、source<->stream Shutdown 拆环和锁层级测试通过。
27. synchronous-first RequestSample token correctness、`MF_E_MEDIA_SOURCE_WRONGSTATE` stopped behavior、单一路径 error signaling 和 latency gate通过。
28. preferred allocator 的 Initialize/Allocate/Uninitialize lifecycle 与标准 MF fallback 两条路径至少各有验证。
29. Camera Profile 策略有实测结论；若发布 profiles，Legacy profile 与 native mode 一致。
30. unsupported GetService/KsControl/SetD3DManager HRESULTs 和 post-Shutdown `MF_E_SHUTDOWN` contract 通过。
31. pitch-aware NV12 output correctness。
32. CurrentUser cross-session security通过。
33. producer-first MF行为有明确实测结论。
34. Rust COM panic boundary不会让 recoverable panic 穿越 FFI。
35. old DShow registration migration/cleanup明确。
36. no C++/CMake/libdshowcapture。
37. integration compatibility tests通过。
38. README只写已实现功能。

## 33. 官方参考

- MFCreateVirtualCamera  
  https://learn.microsoft.com/windows/win32/api/mfvirtualcamera/nf-mfvirtualcamera-mfcreatevirtualcamera
- MFIsVirtualCameraTypeSupported  
  https://learn.microsoft.com/windows/win32/api/mfvirtualcamera/nf-mfvirtualcamera-mfisvirtualcameratypesupported
- IMFVirtualCamera  
  https://learn.microsoft.com/windows/win32/api/mfvirtualcamera/nn-mfvirtualcamera-imfvirtualcamera
- IMFVirtualCamera::GetMediaSource  
  https://learn.microsoft.com/windows/win32/api/mfvirtualcamera/nf-mfvirtualcamera-imfvirtualcamera-getmediasource
- Frame Server Custom Media Source  
  https://learn.microsoft.com/windows-hardware/drivers/stream/frame-server-custom-media-source
- IMFMediaStream::RequestSample  
  https://learn.microsoft.com/windows/win32/api/mfidl/nf-mfidl-imfmediastream-requestsample
- IMF2DBuffer2::Lock2DSize  
  https://learn.microsoft.com/windows/win32/api/mfobjects/nf-mfobjects-imf2dbuffer2-lock2dsize
- IMFSampleAllocatorControl  
  https://learn.microsoft.com/windows/win32/api/mfidl/nn-mfidl-imfsampleallocatorcontrol
- Microsoft Windows Camera VirtualCamera sample  
  https://github.com/microsoft/Windows-Camera/tree/master/Samples/VirtualCamera
- DirectShow  
  https://learn.microsoft.com/windows/win32/directshow/
- DirectShow allocator negotiation  
  https://learn.microsoft.com/windows/win32/directshow/negotiating-allocators
- Kernel object namespaces  
  https://learn.microsoft.com/windows/win32/termserv/kernel-object-namespaces

Windows 平台事实以当前 Microsoft 文档、SDK header、官方 sample 和目标 OS 实测为准。
