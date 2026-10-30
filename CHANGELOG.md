# Changelog

## Unreleased

- DirectShow cameras can also register an ordered `DirectShowRegistration::frame_rates` list (`ManagedCameraInfo::frame_rates`, registry `FrameRates`, at most 16): every offered pixel format is advertised at the current rate and then at each registered rate. The connected graph's rate reaches the producer through `requested_format()`, and the source paces delivery to it whatever rate the producer writes at. Records without `FrameRates` keep advertising one rate. **Breaking** for struct literals: `DirectShowRegistration` and `ManagedCameraInfo` gained a field.
- The `cuda` feature no longer links `cudart`: `write_cuda` resolves its two CUDA Runtime functions at runtime from the Runtime DLL the application loaded (else the standard DLL search). Fixes duplicate-symbol link errors in applications that link the CUDA Runtime themselves (for example through `raw-dylib`) and removes the CUDA major-version pin. A missing Runtime surfaces as `Error::Windows`.
- DirectShow cameras register an ordered `DirectShowRegistration::pixel_formats` list (`ManagedCameraInfo::pixel_formats`, registry `PixelFormats`) and advertise all of them at the current size and rate. Only one consumer graph may connect at a time; it publishes its negotiated format, which producers read with the new `VirtualCamera::requested_format()` to switch pixel format.
- DirectShow delivery is frame-driven: producers set a per-session frame event and the source pushes each frame as it is published. Measured 1080p30 source latency dropped from a median of 31.2 ms (one frame interval) to 3.5 ms.
- A registered DirectShow `default_format` now outranks the user's last published format for idle cameras.
- Closing a managed DirectShow producer releases its single-writer lease immediately, so a new session can open before the old object is dropped.
- **Breaking:** replaced the C++ managed DirectShow driver with pure Rust sources: `virtualcam_dshow.dll` (DirectShow) and `virtualcam_mf.dll` (Media Foundation / Frame Server), built from the new `virtualcam-dshow` and `virtualcam-mf` workspace crates. `native/managed-virtualcam` and the managed ABI v3 are gone.
- Added multi-format managed cameras: `PixelFormat` NV12, I420, YUY2, UYVY, P010, BGRA32, RGB24; `ColorSpace` (matrix/range/primaries/transfer) signaled as VIDEOINFOHEADER2 / `MF_MT_*` metadata; any even resolution up to 16384 and rational frame rates. Sources copy pitch-aware without conversion.
- Added `AppId` and `ManagedCameraManager::for_app`: a program-scoped namespace for registering and listing its own cameras (not authentication).
- Added `ManagedCameraRegistration::{DirectShow, MediaFoundation}`. DirectShow cameras register per user without elevation (or machine-wide when elevated) and their format is chosen by each producer session; Media Foundation cameras (`CurrentUser` access, `System` lifetime) have one immutable native mode fixed at registration. Rename of a Media Foundation camera recreates it.
- Added transport v2 (`virtualcam-protocol`): a per-slot seqlock ring that never delivers torn frames, a seqlock'd DirectShow control block, producer liveness via process handle + creation time, and for Media Foundation an authenticated named-pipe rendezvous (token SID check) with an owner-only `Global\` ring.
- Added `ManagedCameraManager::legacy_cameras` / `remove_legacy_camera` to clean up cameras of the retired C++ driver, plus `install_media_foundation_source` / `uninstall_media_foundation_source`.
- Added `CameraType::ManagedMediaFoundation` and `VirtualCamera::video_format`; new errors `InvalidAppId`, `UnsupportedFormat`, `FormatMismatch`, `CameraNotFound`, `NameConflict`, `RegistrationDegraded`; removed `ManagedDriverAbi`.
- **Breaking:** renamed `CameraType::ManagedNv12` to `CameraType::ManagedDirectShow` and made `CameraType` `#[non_exhaustive]`, so a future Media Foundation managed backend can be added without another break.
- `VirtualCamera` is now `Send` (still not `Sync`), so a camera can be opened on one thread and written from a dedicated producer thread.
- OBS producers now write `%APPDATA%\obs-virtualcam.txt` after a successful open, matching OBS Studio, so consumers that open the OBS camera before the producer negotiate the producer's mode instead of a stale one.
- OBS queue publication fields now use sequentially consistent atomic stores instead of volatile stores plus compiler fences, so the unchanged publication order also holds on ARM64.
- Fixed the `cuda` feature failing to link against CUDA 13 toolkits, whose `cudart.lib` has no `__imp_` import thunks.
- Added Phase 0A tests for the OBS-compatible queue writer, including an ignored torn-frame stress measurement (the reason managed cameras moved to the seqlock ring).

## 0.1.0 - 2026-07-14

- Added a platform-neutral `VirtualCamera` API with Windows OBS and Unity Capture backends.
- Added tightly packed NV12 and RGBA8 host-memory publication.
- Added optional direct CUDA D2H into camera shared memory with caller-owned
  streams and contexts, without a crate-owned intermediate allocation.
- Added optional zero-default-overhead `tracing` events.
- Added checked ABI layouts, real Win32 named-object state tests, a real CUDA D2H
  shared-mapping test, and ignored installed-driver integration tests.
