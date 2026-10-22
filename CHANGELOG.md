# Changelog

## Unreleased

- Added an independently managed x64 NV12 DirectShow virtual-camera driver with generated per-device CLSIDs, custom friendly names, and independent named shared-memory queues.
- Added `ManagedCameraManager` APIs for register, unregister, rename, list, get, and producer open operations without modifying the existing OBS Virtual Camera registration. Registration stores only device identity; managed runtime ABI v3 uses a fixed control mapping plus format-specific data mappings so the same CLSID can reopen at another valid NV12 resolution even while an old consumer retains its previous mapping.
- Added machine-wide management hardening and portable deployment: a managed DLL may live at a stable app-local path on a local drive, its file-handle-canonicalized DOS path is persisted to `InprocServer32`, UNC/network modules are rejected, mutations are cross-process serialized, and rename/unregister rollback restores the original registered module identity. Rust validates the actual registered DLL as managed-driver ABI v3 before opening a producer.
- Added producer lifecycle hardening: separate producer-lease and Ready events, explicit control/data publication ordering, fixed-mode DirectShow validation, idle `640x480@30` black-frame activation for OpenCV/DirectShow, throttled queue retries, stale-frame producer-death detection, and missed-deadline dropping instead of burst catch-up after downstream stalls.
- Added native fixed-mode/OpenCV-style capture-pin regression coverage plus elevated end-to-end coverage for two simultaneous managed cameras, no-producer fallback, abnormal producer termination returning to black, FFmpeg enumeration/capture, rename, independent queues, and sequential landscape/portrait formats on the same CLSID.
- Hardened managed camera CLSID parsing so malformed Unicode input returns `InvalidCameraId` instead of risking a string-slicing panic.
- **Breaking:** renamed `CameraType::ManagedNv12` to `CameraType::ManagedDirectShow` and made `CameraType` `#[non_exhaustive]`, so a future Media Foundation managed backend can be added without another break.
- `VirtualCamera` is now `Send` (still not `Sync`), so a camera can be opened on one thread and written from a dedicated producer thread.
- OBS producers now write `%APPDATA%\obs-virtualcam.txt` after a successful open, matching OBS Studio, so consumers that open the OBS camera before the producer negotiate the producer's mode instead of a stale one.
- OBS and managed queue publication fields now use sequentially consistent atomic stores instead of volatile stores plus compiler fences, so the unchanged publication order also holds on ARM64.
- Fixed the `cuda` feature failing to link against CUDA 13 toolkits, whose `cudart.lib` has no `__imp_` import thunks.
- Split the OBS producer from the managed DirectShow producer: both now share one OBS-compatible queue writer, and the DirectShow control plane (`-Producer`/`-Ready`/`-Control`/`-Data-*` and last-format history) lives in its own module. Added Phase 0A queue tests, including an ignored torn-frame stress measurement.

## 0.1.0 - 2026-07-14

- Added a platform-neutral `VirtualCamera` API with Windows OBS and Unity Capture backends.
- Added tightly packed NV12 and RGBA8 host-memory publication.
- Added optional direct CUDA D2H into camera shared memory with caller-owned
  streams and contexts, without a crate-owned intermediate allocation.
- Added optional zero-default-overhead `tracing` events.
- Added checked ABI layouts, real Win32 named-object state tests, a real CUDA D2H
  shared-mapping test, and ignored installed-driver integration tests.
