# virtualcam

`virtualcam` is a Rust 2024 library for writing frames to virtual-camera backends that are already installed on Windows. It implements the producers' shared-memory protocols directly in Rust; it does not wrap OBS or Unity DLL functions.

| Backend | Input | Layout |
| --- | --- | --- |
| OBS Virtual Camera | NV12 | tightly packed Y plane, then interleaved UV |
| Unity Capture | RGBA8, RGBA FP16 gamma, or RGBA FP16 linear | tight or explicitly padded rows |

The crate does no scaling, pixel conversion, color conversion, frame-rate control, or background threading. Unity's receiver can optionally scale or mirror after receiving a frame.

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

Every frame must contain exactly `camera.frame_size()` bytes. OBS NV12 width and
height must both be even.

`is_ready()` follows what each upstream protocol can actually report. Unity returns `false` until its DirectShow receiver has created the required objects. OBS has no consumer-presence handshake, so its producer is ready as soon as its queue is open.

## Generated test video example

The `generated_test_video` example creates a moving color-bar, checkerboard, and
bouncing-marker pattern entirely in code. By default it writes to both installed
cameras concurrently for 30 seconds at 1280x720 and 30 FPS:

```powershell
cargo run --release --example generated_test_video
```

The duration, output selection, dimensions, and frame rate are adjustable:

```powershell
# Both cameras for 60 seconds.
cargo run --release --example generated_test_video -- --duration 60

# Test only OBS Virtual Camera.
cargo run --release --example generated_test_video -- --camera obs --duration 30

# Test only Unity Capture device #1 at a different mode.
cargo run --release --example generated_test_video -- --camera unity --duration 30 --width 640 --height 480 --fps 24
```

OBS output uses generated NV12 frames; Unity output uses generated RGBA8 frames.
No external image or video is read. Unity may be opened by a consumer before or
during the run—the example keeps polling while its consumer is inactive. Use
`--help` to list all options. `--release` is recommended for real-time generation.

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
| Windows | yes | OBS and Unity Capture producers |
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

Two ignored tests exercise the globally named objects of the drivers installed
on the current Windows host. Close any OBS producer before running them; a Unity
consumer may be open or closed because both states are valid and checked:

```powershell
cargo test --all-features --test windows_installed -- --ignored --test-threads=1
```
