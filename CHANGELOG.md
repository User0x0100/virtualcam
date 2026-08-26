# Changelog

## Unreleased

- Added an independently managed x64 NV12 DirectShow virtual-camera driver with generated per-device CLSIDs, custom friendly names, and independent named shared-memory queues.
- Added `ManagedCameraManager` APIs for register, unregister, rename, list, get, and producer open operations without modifying the existing OBS Virtual Camera registration. Registration now stores only device identity; each `open` selects the session's width, height, and frame rate from the live queue header, so the same CLSID can be reopened later at another valid NV12 resolution.
- Added machine-wide management hardening: Program Files-only COM DLL registration, normal DOS/UNC `InprocServer32` persistence after canonical security validation, cross-process serialization, transactional rollback for rename/unregister failures, strict per-session DirectShow media-type validation, an explicit managed-driver ABI v2 compatibility check against the registered `InprocServer32` target, and a per-camera ready event that prevents DirectShow from reading a partially initialized runtime queue header.
- Added native fixed-mode/OpenCV-style capture-pin regression coverage plus an elevated end-to-end test covering two simultaneous managed cameras, idle no-producer DirectShow capture via the `640x480@30` black-frame fallback, throttled queue retries, FFmpeg enumeration/capture, rename, independent queues, and sequential landscape/portrait formats on the same CLSID.
- Hardened managed camera CLSID parsing so malformed Unicode input returns `InvalidCameraId` instead of risking a string-slicing panic.

## 0.1.0 - 2026-07-14

- Added a platform-neutral `VirtualCamera` API with Windows OBS and Unity Capture backends.
- Added tightly packed NV12 and RGBA8 host-memory publication.
- Added optional direct CUDA D2H into camera shared memory with caller-owned
  streams and contexts, without a crate-owned intermediate allocation.
- Added optional zero-default-overhead `tracing` events.
- Added checked ABI layouts, real Win32 named-object state tests, a real CUDA D2H
  shared-mapping test, and ignored installed-driver integration tests.
