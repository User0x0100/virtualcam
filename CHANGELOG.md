# Changelog

## Unreleased

- Added an independently managed x64 NV12 DirectShow virtual-camera driver with generated per-device CLSIDs, custom friendly names, fixed registration-time modes, and independent named shared-memory queues.
- Added `ManagedCameraManager` APIs for register, unregister, rename, list, get, and producer open operations without modifying the existing OBS Virtual Camera registration.
- Added machine-wide management hardening: Program Files-only COM DLL registration, cross-process serialization, transactional rollback for rename/unregister failures, and strict fixed-mode DirectShow media-type validation.
- Added native fixed-mode regression coverage plus an elevated end-to-end test covering two simultaneous managed cameras, FFmpeg enumeration/capture, rename, independent queues, and cleanup.
- Hardened managed camera CLSID parsing so malformed Unicode input returns `InvalidCameraId` instead of risking a string-slicing panic.

## 0.1.0 - 2026-07-14

- Added a platform-neutral `VirtualCamera` API with Windows OBS and Unity Capture backends.
- Added tightly packed NV12 and RGBA8 host-memory publication.
- Added optional direct CUDA D2H into camera shared memory with caller-owned
  streams and contexts, without a crate-owned intermediate allocation.
- Added optional zero-default-overhead `tracing` events.
- Added checked ABI layouts, real Win32 named-object state tests, a real CUDA D2H
  shared-mapping test, and ignored installed-driver integration tests.
