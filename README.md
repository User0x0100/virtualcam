# virtualcam

`virtualcam` is a Rust 2024 library for Windows virtual cameras. It can write directly to installed OBS/Unity Capture shared-memory protocols and can also manage this repository's independent NV12 DirectShow camera driver.

| Backend | Input | Layout |
| --- | --- | --- |
| Managed NV12 DirectShow camera | NV12 | tightly packed Y plane, then interleaved UV |
| OBS Virtual Camera | NV12 | tightly packed Y plane, then interleaved UV |
| Unity Capture | RGBA8, RGBA FP16 gamma, or RGBA FP16 linear | tight or explicitly padded rows |

The Rust producer path does no scaling, pixel conversion, or color conversion. Managed camera registration stores only persistent device identity; each producer session selects one NV12 width/height/frame-rate before a DirectShow consumer connects. The native DLL owns only output pacing and copies the selected NV12 queue slot into the consumer sample. Unity's receiver can optionally scale or mirror after receiving a frame.

## Managed independent NV12 cameras

`native/managed-virtualcam` builds a DirectShow in-process COM DLL derived around OBS/libdshowcapture's output-filter infrastructure. It does **not** reuse OBS's CLSID or `OBSVirtualCamVideo` object name. One DLL can back multiple simultaneously registered cameras because every registration gets:

- a generated, independent COM CLSID;
- an arbitrary DirectShow friendly name;
- its own `Local\\VirtualCamRs-{CLSID}` three-slot NV12 mapping name.

Width, height, and frame rate are not part of registration. `ManagedCameraManager::open` creates that camera's queue with the requested runtime format and a per-camera ready event. The queue header is initialized in an unpublished state, then `STARTING` is published with the final volatile state store before the ready event is signaled. The DirectShow DLL waits for that event before reading the header, so a consumer racing producer startup cannot observe a partially initialized format. The same CLSID can therefore be reopened later with another even NV12 resolution or frame rate without unregistering the device.

Build the x64 DLL first, then install it under `%ProgramFiles%` from an elevated shell. Machine-wide COM registration intentionally rejects DLLs loaded from user-writable locations.

```powershell
cmake -S native/managed-virtualcam -B native/managed-virtualcam/build -G "Visual Studio 18 2026" -A x64
cmake --build native/managed-virtualcam/build --config Release
$driverDir = Join-Path $env:ProgramFiles 'VirtualCamRs'
New-Item -ItemType Directory -Force $driverDir | Out-Null
Copy-Item native/managed-virtualcam/build/Release/virtualcam-managed.dll $driverDir/virtualcam-managed.dll -Force
```

Then manage devices through the public Rust API:

```rust,no_run
use virtualcam::{
    ManagedCameraConfig, ManagedCameraManager, ManagedStreamConfig, ObsFrameRate,
    WriteStatus,
};

# fn run() -> Result<(), virtualcam::Error> {
let manager = ManagedCameraManager::new(
    r"C:\Program Files\VirtualCamRs\virtualcam-managed.dll",
);
let info = manager.register(ManagedCameraConfig {
    name: "FaceSwap Output".into(),
})?;

let mut camera = manager.open(
    info.id,
    ManagedStreamConfig {
        width: 1080,
        height: 1920,
        frame_rate: ObsFrameRate { numerator: 60, denominator: 1 },
    },
)?;
let nv12 = vec![0_u8; camera.frame_size()];
assert_eq!(camera.write_host(&nv12)?, WriteStatus::Written);
camera.close()?;

// The registration and CLSID stay unchanged. A later producer session may use
// another format before the next DirectShow consumer connects.
let mut camera = manager.open(
    info.id,
    ManagedStreamConfig {
        width: 2560,
        height: 1440,
        frame_rate: ObsFrameRate { numerator: 30, denominator: 1 },
    },
)?;
camera.close()?;

manager.rename(info.id, "FaceSwap Main")?;
manager.unregister(info.id)?;
# Ok(())
# }
```

`register`, `unregister`, `rename`, `get`, and `list` are persistent management operations. Registration uses DirectShow's filter mapper and writes system COM registration, so register/rename/unregister normally require elevation; read/open operations do not. `open` creates the per-camera runtime queue and selects that producer session's format. Before creating the queue it also resolves the camera's actual `HKCR\\CLSID\\{id}\\InprocServer32` target and verifies that the DLL COM will load implements managed-driver ABI v2; the manager's own DLL path is not treated as authoritative for an existing registration. A registered camera can now be opened by DirectShow/OpenCV before any producer exists: the native filter advertises an idle `640x480@30` NV12 mode and emits black frames. When a live queue already exists, its runtime format is used instead. A format remains fixed for one connected DirectShow graph, so a consumer that opened first keeps the `640x480@30` fallback and can consume a later producer without reconnecting only when that producer uses the same mode; otherwise reopen the consumer to negotiate the producer's live format. Mutating operations are serialized across processes and the native DLL must resolve beneath the canonical Program Files directory before any machine-wide registration is changed. Canonical extended-length paths are used only for that security check; `InprocServer32` is persisted as a normal DOS/UNC full path rather than a `\\?\...` path so COM/DirectShow activation is not exposed to extended-path compatibility quirks. Keep that installed DLL at a stable absolute location for as long as any registration exists, because COM records the loaded module path. Existing OBS Virtual Camera registration is untouched. The first native build target is x64; supporting 32-bit DirectShow consumers requires a corresponding x86 DLL/registry view.

## Independent backend controls

OBS and Unity Capture use separate option types and separate constructors. There
is no combined structure containing controls that only make sense for one
backend. Both option types implement `Default`; their 1920x1080 initial mode
matches the filters' first/default media type.

OBS exposes the complete upstream queue creation parameters—dimensions and an
exact rational frame rate—plus its backend-only recovery extension:

```rust,no_run
use virtualcam::{ObsFrameRate, ObsOptions, VirtualCamera, WriteStatus};

fn main() -> Result<(), virtualcam::Error> {
    let mut camera = VirtualCamera::open_obs(ObsOptions {
            width: 1280,
            height: 720,
            frame_rate: ObsFrameRate {
                numerator: 30_000,
                denominator: 1001,
            },
            force_open: false,
        })?;

    let nv12 = vec![0_u8; camera.frame_size()];
    assert_eq!(camera.write_host(&nv12)?, WriteStatus::Written);
    camera.close()?;
    Ok(())
}
```

Unity independently exposes every `SharedImageMemory::Send` metadata control and
the constructor's capture number:

```rust,no_run
use virtualcam::{
    UnityCaptureFormat, UnityCaptureMirrorMode, UnityCaptureOptions,
    UnityCaptureResizeMode, VirtualCamera,
};

# fn run() -> Result<(), virtualcam::Error> {
let mut camera = VirtualCamera::open_unity_capture(UnityCaptureOptions {
    width: 1280,
    height: 720,
    capture_number: 0, // zero-based: Unity Capture device #1
    format: UnityCaptureFormat::Rgba8,
    stride_pixels: None, // tightly packed; Some(value) preserves row padding
    resize_mode: UnityCaptureResizeMode::Linear,
    mirror_mode: UnityCaptureMirrorMode::Horizontal,
    timeout_ms: 1000,
})?;
let rgba = vec![0_u8; camera.frame_size()];
camera.write_host(&rgba)?;
# Ok(())
# }
```

`UnityCaptureFormat` maps exactly to upstream values 0, 1, and 2. Stride is
expressed in pixels, as in the C++ sender, rather than bytes. `frame_size()`
includes explicitly configured row padding. The source buffer/data-size
parameters are supplied by `write_host` or `write_cuda`; width, height, stride,
format, resize, mirror, and timeout are written into every shared header.

The upstream D3D11 plugin's texture pointer, texture readback, and double
buffering are not shared-memory camera controls. They are intentionally absent
because this crate receives an already accessible host or CUDA buffer instead
of a Unity `ID3D11Texture2D`.

Every frame must contain exactly `camera.frame_size()` bytes. OBS and managed
NV12 width and height must both be even.

`is_ready()` follows what each upstream protocol can actually report. Unity returns `false` until its DirectShow receiver has created the required objects. OBS has no consumer-presence handshake, so its producer is ready as soon as its queue is open.

## Generated test video example

The `generated_test_video` example creates a moving color-bar, checkerboard, and
bouncing-marker pattern entirely in code. By default it writes to both installed
cameras concurrently for 30 seconds at 1280x720 and 30 FPS:

```powershell
cargo run --release --example generated_test_video
```

The memory path, CUDA device, duration, output selection, dimensions, and frame
rate are adjustable. Host memory remains the default:

```powershell
# Both cameras for 60 seconds.
cargo run --release --example generated_test_video -- --duration 60

# Test only OBS Virtual Camera.
cargo run --release --example generated_test_video -- --camera obs --duration 30

# Test only Unity Capture device #1 at a different mode.
cargo run --release --example generated_test_video -- --camera unity --duration 30 --width 640 --height 480 --fps 24

# Write the generated video to both cameras from CUDA device memory on device 0.
cargo run --release --features cuda --example generated_test_video -- --memory cuda --cuda-device 0 --duration 30
```

OBS output uses generated NV12 frames; Unity output uses generated RGBA8 frames.
No external image or video is read. Unity may be opened by a consumer before or
during the run—the example keeps polling while its consumer is inactive. Use
`--help` to list all options. `--release` is recommended for real-time generation.

CUDA mode still generates each test frame in code on the CPU, then enqueues H2D
into a camera-specific device allocation and calls `write_cuda` on the same
default CUDA stream. The library's D2H therefore runs after that upload and
targets the selected OBS or Unity shared-memory slot directly. Each camera owns
an independent device allocation, released explicitly at the end.

## CUDA frames

Enable `cuda` and pass a CUDA Runtime device pointer:

```rust,no_run
# #[cfg(feature = "cuda")]
# fn send(
#     camera: &mut virtualcam::VirtualCamera,
#     device_ptr: *const core::ffi::c_void,
# ) -> Result<(), virtualcam::Error> {
// SAFETY: the application established the current CUDA context and guarantees
// that device_ptr is readable for camera.frame_size() bytes until return.
unsafe {
    camera.write_cuda(device_ptr, camera.frame_size(), None)?;
}
# Ok(())
# }
```

### Required contiguous layout

`write_cuda` requires one contiguous, linear device-memory byte range. The range
must start at `device_ptr`, remain readable through `device_ptr + byte_len`, and
`byte_len` must equal `camera.frame_size()` exactly. The API performs one 1D copy;
it does not accept separate plane pointers, CUDA arrays, textures, surfaces, or a
source pitch argument.

- OBS: the source is tightly packed NV12: `width * height` Y bytes immediately
  followed by `width * height / 2` interleaved UV bytes. There is no row or
  plane padding. A pitched CUDA allocation whose pitch differs from `width`
  cannot be passed directly.
- Unity Capture: the source is one contiguous sequence of
  `stride_pixels * height * bytes_per_pixel` bytes. Row padding is allowed only
  when the same `stride_pixels` is configured in `UnityCaptureOptions`; the
  padding bytes are included in `frame_size()` and copied too. A CUDA pitch must
  be divisible by the format's bytes per pixel before it can be represented by
  `stride_pixels`.

Host writes have the same layout requirement, although Rust's `&[u8]` already
guarantees a contiguous byte range.

### Direct shared-memory D2H behavior

`None` passes a null `cudaStream_t`; `Some(CudaStream)` is forwarded unchanged.
The frame-producing work must already be ordered before this copy in that stream,
or be synchronized by the caller. The implementation performs exactly this
crate-visible transfer sequence:

```text
cudaMemcpyAsync(camera shared-memory slot, device_ptr, frame_size,
                cudaMemcpyDeviceToHost, stream)
cudaStreamSynchronize(stream)
publish the completed slot to the virtual-camera consumer
```

There is no crate-owned CUDA allocation, pinned staging buffer, or second CPU
copy. For OBS the selected queue slot is reserved until synchronization succeeds;
for Unity the receiver mutex remains held through D2H and synchronization. A CUDA
error prevents the slot/event from being published as a completed frame.

The Windows shared-memory destination is one contiguous virtual address range,
but it is an ordinary, unregistered mapping—not memory allocated by
`cudaHostAlloc`, and this crate does not call `cudaHostRegister`. Consequently,
CUDA may treat it as pageable memory: `cudaMemcpyAsync` can behave synchronously
with respect to the host, and the CUDA driver may use internal staging. The crate
guarantees that it allocates no intermediate frame buffer; it cannot guarantee
that the CUDA Runtime/driver performs no internal allocation or staging, nor can
it promise copy/compute overlap. See NVIDIA's
[API synchronization behavior](https://docs.nvidia.com/cuda/cuda-runtime-api/api-sync-behavior.html)
and [`cudaMemcpyAsync` contract](https://docs.nvidia.com/cuda/cuda-runtime-api/group__CUDART__MEMORY.html).

The call is synchronous from the crate user's perspective. On success the D2H
copy is complete, the source may be reused, and the frame has been published.
CUDA Runtime calls can also surface errors from earlier asynchronous work. The
caller must initialize CUDA, select the correct device/current context, keep the
source and stream valid until return, and maintain any required thread/context
affinity. The crate never calls `cudaSetDevice`, context APIs,
`cudaDeviceSynchronize`, stream creation/destruction, `cudaHostRegister`, or
`cudaHostUnregister`. `close` and `Drop` make no CUDA calls. CUDA failures retain
the failing operation and exact numeric `cudaError_t`; the minimal binding does
not call `cudaGetErrorString`.

On Windows, linking uses `cudart`; `build.rs` adds `%CUDA_PATH%\lib\x64` (or `Win32`) when that environment variable is present. The default build never compiles or links CUDA code.

## `force_open`

`ObsOptions::force_open` is an OBS-only dangerous recovery option. Normal mode rejects an existing `OBSVirtualCamVideo` mapping as `DeviceInUse`. Force mode creates/opens it, verifies the mapped region is large enough, and reinitializes the upstream header.

The caller guarantees there is no other active producer. `force_open` cannot establish that fact; concurrent writers create a protocol-level data race and undefined output. It never terminates a process, closes another process's handles, or deletes a referenced named object. Unity Capture has no equivalent field because it has no corresponding producer occupancy check.

## Platforms and features

| Platform | Compiles | Runtime behavior |
| --- | --- | --- |
| Windows | yes | managed NV12 driver plus OBS and Unity Capture producers |
| Linux | yes | backend-specific open methods return `UnsupportedPlatform` |
| macOS | yes | backend-specific open methods return `UnsupportedPlatform` |

- default: only the target-specific `windows-sys` dependency on Windows.
- `cuda`: direct CUDA Runtime FFI; independent of `tracing`.
- `tracing`: emits open/close, recovery, inactive-consumer, skip, and error events. The crate does not install a subscriber. Without this feature there is no `tracing` dependency or log formatting.

See [UPSTREAM.md](UPSTREAM.md) for pinned protocol sources, exact ABI details, and intentional differences.

## Testing

The default suite uses real Win32 mappings, mutexes, and events under unique
test-only names. It covers protocol layout, queue publication and wraparound,
Unity's inactive/partial/active handshakes, resource cleanup, public API
validation, direct shared-slot publication, and exact CUDA error mapping:

```powershell
cargo test --all-features
```

One ignored test performs real `cudaMalloc`/H2D followed by the library's D2H
directly into an ordinary Windows shared mapping and verifies the bytes in that
mapping. Run it on a CUDA-capable host with:

```powershell
cargo test --all-features cuda::tests::real_cuda_d2h_writes_directly_to_a_windows_shared_mapping -- --ignored --exact
```

Two ignored tests exercise the globally named objects of the OBS/Unity drivers installed
on the current Windows host. Close any OBS producer before running them; a Unity
consumer may be open or closed because both states are valid and checked:

```powershell
cargo test --all-features --test windows_installed -- --ignored --test-threads=1
```

A separate ignored integration test performs the full managed-camera lifecycle: it registers two independent devices, renames one, verifies both through FFmpeg's DirectShow enumeration, opens independent NV12 producer queues, then captures the same CLSID first at `320x240@30` and later at `240x320@25` without re-registering it. It finally unregisters both devices. Run from an elevated shell after building the DLL:

```powershell
$env:VIRTUALCAM_MANAGED_DRIVER = "$PWD\native\managed-virtualcam\build\Release\virtualcam-managed.dll"
cargo test --all-features --test windows_managed -- --ignored --nocapture
```
