# Changelog

## Unreleased

- Added an independently managed x64 NV12 DirectShow virtual-camera driver with generated per-device CLSIDs, custom friendly names, and independent named shared-memory queues.
- Added `ManagedCameraManager` APIs for register, unregister, rename, list, get, and producer open operations without modifying the existing OBS Virtual Camera registration. Registration stores only device identity; managed runtime ABI v3 uses a fixed control mapping plus format-specific data mappings so the same CLSID can reopen at another valid NV12 resolution even while an old consumer retains its previous mapping.
- Added machine-wide management hardening and portable deployment: a managed DLL may live at a stable app-local path on a local drive, its file-handle-canonicalized DOS path is persisted to `InprocServer32`, UNC/network modules are rejected, mutations are cross-process serialized, and rename/unregister rollback restores the original registered module identity. Rust validates the actual registered DLL as managed-driver ABI v3 before opening a producer.
- Added producer lifecycle hardening: separate producer-lease and Ready events, explicit control/data publication ordering, fixed-mode DirectShow validation, idle `640x480@30` black-frame activation for OpenCV/DirectShow, throttled queue retries, stale-frame producer-death detection, and missed-deadline dropping instead of burst catch-up after downstream stalls.
- Added native fixed-mode/OpenCV-style capture-pin regression coverage plus elevated end-to-end coverage for two simultaneous managed cameras, no-producer fallback, abnormal producer termination returning to black, FFmpeg enumeration/capture, rename, independent queues, and sequential landscape/portrait formats on the same CLSID.
- Hardened managed camera CLSID parsing so malformed Unicode input returns `InvalidCameraId` instead of risking a string-slicing panic.

## 0.1.0 - 2026-07-14

- Added a platform-neutral `VirtualCamera` API with Windows OBS and Unity Capture backends.
- Added tightly packed NV12 and RGBA8 host-memory publication.
- Added optional direct CUDA D2H into camera shared memory with caller-owned
  streams and contexts, without a crate-owned intermediate allocation.
- Added optional zero-default-overhead `tracing` events.
- Added checked ABI layouts, real Win32 named-object state tests, a real CUDA D2H
  shared-mapping test, and ignored installed-driver integration tests.
