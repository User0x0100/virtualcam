# Managed NV12 DirectShow driver

`virtualcam-managed.dll` is the native receiver used by `ManagedCameraManager`.
It is independent of OBS Virtual Camera: every registered camera gets its own
COM CLSID and its own `Local\\VirtualCamRs-{CLSID}` shared-memory queue.

## Format contract

Each registration exposes exactly one fixed DirectShow video mode:

- subtype: `MEDIASUBTYPE_NV12`
- dimensions: fixed at registration time
- frame rate: fixed at registration time
- producer layout: the OBS three-slot shared-memory queue ABI, but with an
  instance-specific mapping name

No RGB conversion, I420/YUY2 conversion, or scaling is performed in this DLL.
`QueryAccept`, `Connect`, and `IAMStreamConfig::SetFormat` reject media types that
do not exactly match the registered NV12 dimensions and frame interval, so a
consumer cannot resize the allocator while the producer still writes fixed-size
frames. When no producer is active, the filter emits a black NV12 frame at the
registered rate.

## Build

From a Visual Studio/MSVC developer environment, or with CMake able to discover
Visual Studio Build Tools:

```powershell
cmake -S native/managed-virtualcam -B native/managed-virtualcam/build -G "Visual Studio 18 2026" -A x64
cmake --build native/managed-virtualcam/build --config Release
```

The resulting DLL is:

`native/managed-virtualcam/build/Release/virtualcam-managed.dll`

The Rust API loads this DLL only for management calls. DirectShow applications
later load the same DLL through the COM registration written for each camera.
Registration is system-wide and therefore normally requires elevation. Before
any register/rename/unregister mutation, the DLL resolves its own canonical path
and requires it to be beneath the canonical Program Files directory; this prevents
a machine-wide `InprocServer32` registration from pointing at a user-writable
development directory. Copy the built DLL to a stable Program Files installation
directory before registering cameras.

Management mutations are serialized with a global named mutex. Rename and
unregister use ordered rollback so a failing FilterMapper/COM/registry step keeps
enough metadata to retry instead of intentionally deleting the remaining state.

## Native exports

The Rust wrapper uses these C ABI exports:

- `VirtualCamRegister`
- `VirtualCamUnregister`
- `VirtualCamRename`
- `VirtualCamGet`
- `VirtualCamEnumerate`

COM uses `DllGetClassObject` and `DllCanUnloadNow`.

## Third-party code and license

The `third_party/libdshowcapture` subset is copied from OBS's pinned
`obsproject/libdshowcapture` commit `8878638324393815512f802640b0d5ce940161f1`.
Those files retain their upstream LGPL-2.1-or-later headers and the complete
license text is included as `third_party/libdshowcapture/COPYING`.

The Rust crate and `managed-virtualcam.cpp` are otherwise under this repository's
MIT license. Distributors of the native DLL must also satisfy the LGPL terms for
the linked libdshowcapture-derived portion.
