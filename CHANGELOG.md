# Changelog

## 0.1.0 - 2026-07-14

- Added a platform-neutral `VirtualCamera` API with Windows OBS and Unity Capture backends.
- Added tightly packed NV12 and RGBA8 host-memory publication.
- Added optional synchronous CUDA D2H staging with caller-owned streams and contexts.
- Added optional zero-default-overhead `tracing` events.
- Added checked ABI layouts, real Win32 named-object state tests, CUDA failure/order mocks, and ignored installed-driver integration tests.
