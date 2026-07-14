# Changelog

## 0.1.0 - 2026-07-14

- Added a platform-neutral `VirtualCamera` API with Windows OBS and Unity Capture backends.
- Added tightly packed NV12 and RGBA8 host-memory publication.
- Added optional direct CUDA D2H into camera shared memory with caller-owned
  streams and contexts, without a crate-owned intermediate allocation.
- Added optional zero-default-overhead `tracing` events.
- Added checked ABI layouts, real Win32 named-object state tests, a real CUDA D2H
  shared-mapping test, and ignored installed-driver integration tests.
