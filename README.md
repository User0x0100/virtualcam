# virtualcam

`virtualcam` is a Rust 2024 library for Windows virtual cameras. It registers and feeds its **own** virtual cameras — pure Rust DirectShow and Media Foundation sources, with common pixel formats, colorimetry, resolutions, and frame rates — and can also write to the installed OBS Virtual Camera and Unity Capture shared-memory protocols.

| Backend | Input formats | Layout |
| --- | --- | --- |
| Managed DirectShow camera | NV12, I420, YUY2, UYVY, P010, BGRA32, RGB24 | tightly packed (see `PixelFormat::layout`) |
| Managed Media Foundation camera | same set; one immutable native mode per camera | tightly packed |
| OBS Virtual Camera | NV12 | tightly packed Y plane, then interleaved UV |
| Unity Capture | RGBA8, RGBA FP16 gamma, or RGBA FP16 linear | tight or explicitly padded rows |

Producers never scale or convert pixels. Sources copy each tightly packed frame straight into the consumer's buffer, honoring its pitch row by row (one `memcpy` when the layouts match); colorimetry is signaled as metadata only.

## Managed cameras

The workspace builds two COM source DLLs and the library that manages them:

| | DirectShow (`virtualcam_dshow.dll`) | Media Foundation (`virtualcam_mf.dll`) |
| --- | --- | --- |
| Seen by | DirectShow apps: FFmpeg `dshow`, OpenCV `CAP_DSHOW`, OBS, legacy conferencing apps | Frame Server apps: Windows Camera, browsers, Teams, WinRT `MediaCapture`, `IMFSourceReader` |
| Install | none; each registration points at the DLL | once per machine, elevated: `install_media_foundation_source` |
| Registration | `CurrentUser` (no elevation) or `AllUsers` (elevated) | per-user (`CurrentUser` access, `System` lifetime), no elevation |
| Format | chosen by the producer for each session | one immutable native mode fixed at registration |
| Transport | producer-owned, session-local | source-owned, authenticated, cross-session |

```powershell
cargo build --release -p virtualcam-dshow -p virtualcam-mf
# -> target\release\virtualcam_dshow.dll, target\release\virtualcam_mf.dll
```

### Application ids

Every camera can carry an `AppId` (`A-Z a-z 0-9 . _ -`, up to 128 chars). A manager created with `ManagedCameraManager::for_app` stamps its id on new cameras and only sees cameras with that id; `ManagedCameraManager::new()` sees everything. It is a namespace to tell programs' cameras apart, not a security boundary.

```rust,no_run
use virtualcam::{AppId, ColorSpace, DirectShowRegistration, FrameRate, ManagedCameraManager, PixelFormat, VideoFormat, WriteStatus};

# fn run() -> Result<(), virtualcam::Error> {
let manager = ManagedCameraManager::for_app(AppId::new("com.example.studio")?)
    .with_dshow_source(r"C:\Program Files\Studio\virtualcam_dshow.dll");

// Per-user DirectShow camera: no elevation needed.
let info = manager.register(DirectShowRegistration::new("Studio Cam"))?;

// Each producer session picks its own format.
let format = VideoFormat::new(PixelFormat::Yuy2, 1280, 720, FrameRate::fps(60)).with_color(ColorSpace::BT709);
let mut camera = manager.open(info.id, format)?;
let frame = vec![0_u8; camera.frame_size()];
assert_eq!(camera.write_host(&frame)?, WriteStatus::Written);
camera.close()?;

// Only this application's cameras.
for camera in manager.list()? {
    println!("{} {} {:?}", camera.id, camera.name, camera.health);
}
manager.rename(info.id, "Studio Cam Main")?;
manager.unregister(info.id)?;
# Ok(())
# }
```

Media Foundation cameras fix their format at registration because Frame Server enumerates media types before any producer exists:

```rust,no_run
use virtualcam::{FrameRate, ManagedCameraManager, MediaFoundationRegistration, PixelFormat, VideoFormat, WriteStatus};

# fn run() -> Result<(), virtualcam::Error> {
// Once per machine, elevated, from a folder Local Service can read:
// ManagedCameraManager::install_media_foundation_source(r"C:\Program Files\Studio\virtualcam_mf.dll")?;
let manager = ManagedCameraManager::new();
let native = VideoFormat::new(PixelFormat::Nv12, 1920, 1080, FrameRate::fps(30));
let info = manager.register(MediaFoundationRegistration::new("Studio MF Cam", native))?;
let mut camera = manager.open_registered(info.id)?; // `open` with any other format: FormatMismatch
let frame = vec![0_u8; camera.frame_size()];
// ConsumerInactive until an application starts the camera; then Written.
let _status: WriteStatus = camera.write_host(&frame)?;
# Ok(())
# }
```

### Formats and colorimetry

| `PixelFormat` | Layout (tightly packed) | DirectShow subtype | MF subtype |
| --- | --- | --- | --- |
| `Nv12` | Y, then interleaved UV (4:2:0) | `NV12` | `NV12` |
| `I420` | Y, U, V planes (4:2:0) | `I420` (accepts `IYUV`) | `I420` |
| `Yuy2` / `Uyvy` | packed 4:2:2 | `YUY2` / `UYVY` | `YUY2` / `UYVY` |
| `P010` | 16-bit LE words, data in the high 10 bits; Y, then UV | `P010` | `P010` |
| `Bgra32` | `B G R A`, top-down | `RGB32` (bottom-up DIB, flipped by the source) | `RGB32` |
| `Rgb24` | `B G R`, top-down, no row padding | `RGB24` (DWORD-aligned DIB rows) | `RGB24` |

`ColorSpace` (matrix, range, primaries, transfer) defaults per format and height (`BT601` SD, `BT709` HD, `BT2020` for P010, `SRGB` for RGB) and has presets including `BT709_FULL`, `BT2100_PQ`, and `BT2100_HLG`. It is published as `VIDEOINFOHEADER2` DXVA extended-format bits (DirectShow) and `MF_MT_*` attributes (Media Foundation), and selects the black level (limited Y=16 / full Y=0) of idle frames. Dimensions up to 16384, rates from one frame per minute to 1000 fps.

### Runtime behavior

- **Idle cameras** emit black. A DirectShow camera advertises the live producer's format, else this user's last published format for that camera, else the registered `default_format`, else NV12 640x480@30. `IAMStreamConfig::SetFormat` lets a consumer pick any format before connecting.
- A connected DirectShow graph keeps its negotiated format; a producer with another format shows black there until the graph reconnects. Producers with the same format attach without reconnecting.
- **One producer per camera** (`Error::DeviceInUse` otherwise). A crashed producer is detected (process handle + creation time / pipe closure) and consumers fall back to black.
- **Transport v2**: every session writes a fresh ring whose slots are guarded by a sequence lock, so consumers never accept a torn frame and the writer never waits. DirectShow delivery is paced by a high-resolution timer without catch-up bursts.
- **Media Foundation producers** attach lazily: `write_host` returns `ConsumerInactive` (and `is_ready` is `false`) until a client starts the camera. The source authenticates the producer's Windows token against the camera owner on a DACL-protected pipe and hands it a ring created with an owner-only DACL; another user cannot inject frames.
- Every exported function and COM method is panic-guarded; `DllMain` does nothing.

### Migrating cameras of the retired C++ driver

Cameras registered by the previous C++ DirectShow DLL keep working only while that DLL exists. `ManagedCameraManager::legacy_cameras()` lists them; `remove_legacy_camera(id)` (elevated) removes one, after which a new camera can be registered. The CLSID changes.

### Scope and limitations

- x64 only. There is no x86 DirectShow DLL yet (32-bit DirectShow apps do not see managed cameras) and no ARM64 build.
- FFmpeg's `dshow` demuxer does not implement P010; other DirectShow consumers may.
- `uninstall_media_foundation_source` cannot enumerate other Windows users' cameras; it refuses unless told they were cleaned.
- The Frame Server path is covered by an opt-in test that needs the source installed (elevated); it is verified for a single user, not yet across multiple Windows users. Frame Server shows the camera under its name plus a localized " (Windows Virtual Camera)" suffix.

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

Every frame must contain exactly `camera.frame_size()` bytes. 4:2:0 formats
need even width and height, 4:2:2 formats an even width.

`is_ready()` follows what each protocol can actually report. Unity returns `false` until its DirectShow receiver has created the required objects, and a managed Media Foundation camera until a client started it and the producer attached. OBS and managed DirectShow have no consumer-presence handshake, so their producers are ready as soon as they are open.

Like OBS Studio's own `virtualcam_start`, a successful `open_obs` records the producer mode as `{width}x{height}x{interval}` in `%APPDATA%\obs-virtualcam.txt` (replaced through a temporary file). The OBS DirectShow filter reads that file when a consumer opens the camera while no producer queue exists, so consumer-first applications negotiate this producer's resolution and frame rate instead of scaling from a stale mode. Unlike upstream, the file is written only after the queue was created, so an open rejected with `DeviceInUse` never overwrites the active producer's mode. A failure to write it is not an error.

`VirtualCamera` is `Send` but not `Sync`: open a camera on one thread and move it to a dedicated capture or render thread; writes stay serialized through `&mut self`. For `write_cuda`, each call's device pointer and stream must belong to the CUDA context current on the calling thread.

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

On Windows, linking uses the toolkit's `cudart.lib`; `build.rs` adds `%CUDA_PATH%\lib\x64` (or `Win32`) when that environment variable is present. The library is linked as an unbundled static library: CUDA 13 ships `cudart.lib` as a loader shim without `__imp_` import thunks, and plain-symbol references resolve against both that shim and older import-library toolkits. The default build never compiles or links CUDA code.

## `force_open`

`ObsOptions::force_open` is an OBS-only dangerous recovery option. Normal mode rejects an existing `OBSVirtualCamVideo` mapping as `DeviceInUse`. Force mode creates/opens it, verifies the mapped region is large enough, and reinitializes the upstream header.

The caller guarantees there is no other active producer. `force_open` cannot establish that fact; concurrent writers create a protocol-level data race and undefined output. It never terminates a process, closes another process's handles, or deletes a referenced named object. Unity Capture has no equivalent field because it has no corresponding producer occupancy check.

## Platforms and features

| Platform | Compiles | Runtime behavior |
| --- | --- | --- |
| Windows | yes | managed DirectShow and Media Foundation cameras plus OBS and Unity Capture producers |
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
consumer may be open or closed because both states are valid and checked. The OBS
test restores `%APPDATA%\obs-virtualcam.txt` afterwards:

```powershell
cargo test --all-features --test windows_installed -- --ignored --test-threads=1
```

The OBS-compatible three-slot queue has no per-slot generation, so a writer that
laps a slow reader can overwrite the slot being copied. A default test pins that
ABI property, and an ignored stress test measures how often a deliberately slow
reader observes a torn frame:

```powershell
cargo test --release --lib high_rate_writer_against_slow_reader -- --ignored --nocapture
```

Managed cameras have two more opt-in suites. The DirectShow suite registers per-user cameras (no elevation), checks FFmpeg enumeration, sends every format FFmpeg supports byte-exact through a real DirectShow graph (including bottom-up RGB and DIB row padding), checks idle black at the registered default, crash-to-black with last-format idle, app-id isolation, rename, and unregister:

```powershell
cargo test --release --test windows_dshow -- --ignored --test-threads=1
```

The Media Foundation contract (activation, attributes, events, `RequestSample`, shutdown) and an in-process end-to-end run through `IMFSourceReader` with the real producer and authenticated rendezvous are part of the default suite. The Frame Server test needs the source installed (see `tests/windows_mf.rs`):

```powershell
cargo test --test windows_mf -- --ignored
```

`cargo run --example managed_camera -- register|list|stream|unregister|legacy|remove-legacy ...` exercises the manager from the command line.
