# virtualcam

`virtualcam` is a Rust 2024 library for writing frames to virtual-camera backends that are already installed on Windows. It implements the producers' shared-memory protocols directly in Rust; it does not wrap OBS or Unity DLL functions.

| Backend | Input | Layout |
| --- | --- | --- |
| OBS Virtual Camera | NV12 | tightly packed Y plane, then interleaved UV |
| Unity Capture | RGBA8 / `FORMAT_UINT8` | tightly packed four-byte RGBA pixels |

The crate does no scaling, pixel conversion, color conversion, frame-rate control, or background threading. The first release targets Unity Capture device/capture number 0.

## Host frames

```rust,no_run
use virtualcam::{CameraConfig, CameraType, VirtualCamera, WriteStatus};

fn main() -> Result<(), virtualcam::Error> {
    let mut camera = VirtualCamera::open(
        CameraType::Obs,
        CameraConfig {
            width: 1280,
            height: 720,
            fps: 30.0,
        },
    )?;

    let nv12 = vec![0_u8; camera.frame_size()];
    assert_eq!(camera.write_host(&nv12)?, WriteStatus::Written);
    camera.close()?;
    Ok(())
}
```

Frames must be contiguous and have exactly `camera.frame_size()` bytes. NV12 width and height must both be even. Unity's tightly packed RGBA8 stride is published in pixels, so it equals `width`, not `width * 4`.

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

`None` passes a null `cudaStream_t` to the CUDA Runtime. `Some(CudaStream)` is forwarded unchanged. Each camera lazily owns one reusable pinned staging buffer:

```text
device memory -> pinned host staging -> backend shared memory
```

The call submits `cudaMemcpyAsync`, waits only with `cudaStreamSynchronize(stream)`, and publishes after D2H completion. There are two copies per frame. The crate never calls `cudaSetDevice`, device/context initialization or switching APIs, `cudaDeviceSynchronize`, stream creation/destruction, `cudaHostRegister`, or `cudaHostUnregister`. The caller must keep the CUDA Runtime usable until `close`/drop releases the pinned allocation.

On Windows, linking uses `cudart`; `build.rs` adds `%CUDA_PATH%\lib\x64` (or `Win32`) when that environment variable is present. The default build never compiles or links CUDA code.

## `force_open`

`OpenOptions::force_open` is an OBS-only dangerous recovery option. Normal mode rejects an existing `OBSVirtualCamVideo` mapping as `DeviceInUse`. Force mode creates/opens it, verifies the mapped region is large enough, and reinitializes the upstream header.

The caller guarantees there is no other active producer. `force_open` cannot establish that fact; concurrent writers create a protocol-level data race and undefined output. It never terminates a process, closes another process's handles, or deletes a referenced named object. Unity Capture has no equivalent producer occupancy check, so this option does nothing extra there.

## Platforms and features

| Platform | Compiles | Runtime behavior |
| --- | --- | --- |
| Windows | yes | OBS and Unity Capture producers |
| Linux | yes | `VirtualCamera::open` returns `UnsupportedPlatform` |
| macOS | yes | `VirtualCamera::open` returns `UnsupportedPlatform` |

- default: only the target-specific `windows-sys` dependency on Windows.
- `cuda`: direct CUDA Runtime FFI; independent of `tracing`.
- `tracing`: emits open/close, recovery, inactive-consumer, skip, and error events. The crate does not install a subscriber. Without this feature there is no `tracing` dependency or log formatting.

See [UPSTREAM.md](UPSTREAM.md) for pinned protocol sources, exact ABI details, and intentional differences.

## Testing

The default suite uses real Win32 mappings, mutexes, and events under unique
test-only names. It covers protocol layout, queue publication and wraparound,
Unity's inactive/partial/active handshakes, resource cleanup, public API
validation, and injected CUDA allocation/copy/synchronization/release failures:

```powershell
cargo test --all-features
```

Two ignored tests exercise the globally named objects of the drivers installed
on the current Windows host. Close any OBS producer before running them; a Unity
consumer may be open or closed because both states are valid and checked:

```powershell
cargo test --all-features --test windows_installed -- --ignored --test-threads=1
```
