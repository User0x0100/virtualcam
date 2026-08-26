# Managed NV12 DirectShow driver

`virtualcam-managed.dll` is the native receiver used by `ManagedCameraManager`.
It is independent of OBS Virtual Camera: every registered camera gets its own
COM CLSID and its own `Local\\VirtualCamRs-{CLSID}` shared-memory queue.

## Format contract

Registration stores only persistent device identity: CLSID, friendly name, and
the deterministic instance queue name. Width, height, and frame rate are runtime
producer state rather than registry state.

For each producer session:

- subtype is always `MEDIASUBTYPE_NV12`;
- the Rust producer first creates `Local\\VirtualCamRs-{CLSID}-Ready`, then creates
  `Local\\VirtualCamRs-{CLSID}` and writes width, height, and frame interval into
  the OBS-compatible queue header while its state is unpublished;
- after all header fields are initialized, Rust publishes `STARTING` and signals
  the manual-reset ready event;
- `IClassFactory::CreateInstance` waits for that event before reading the live
  queue header and constructs one DirectShow output mode for that connection;
- a later producer session may use another valid even NV12 resolution or frame
  rate without changing the registered CLSID.

The producer must be open before the DirectShow consumer creates the filter. A
format is fixed for one connected DirectShow graph; in-place hot format changes
are intentionally unsupported. If a producer disappears after the filter has
already been created, the filter keeps its negotiated format and emits black NV12
frames until a matching queue is readable again.

No RGB conversion, I420/YUY2 conversion, or scaling is performed in this DLL.
`QueryAccept`, `Connect`, and `IAMStreamConfig::SetFormat` reject media types that
do not exactly match the runtime NV12 dimensions and frame interval, so a
consumer cannot resize the allocator while the producer still writes fixed-size
frames.

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
Before Rust opens a runtime producer queue, it resolves that CLSID's actual
`InprocServer32` target and loads it to verify ABI v2. This prevents a separate
manager DLL from hiding an obsolete or missing COM driver registration.

## Native exports

The Rust wrapper requires ABI version 2 and uses these C ABI exports:

- `VirtualCamGetAbiVersion`
- `VirtualCamRegister`
- `VirtualCamUnregister`
- `VirtualCamRename`
- `VirtualCamGet`
- `VirtualCamEnumerate`

COM uses `DllGetClassObject` and `DllCanUnloadNow`. ABI v2 keeps the original
`VirtualCamRegister` parameter layout and `CameraInfoAbi` size for defensive
binary compatibility, but the four legacy format arguments must all be zero;
nonzero values are rejected with `ERROR_REVISION_MISMATCH`. Runtime format is
carried only by the live queue header.

## Third-party code and license

The `third_party/libdshowcapture` subset is copied from OBS's pinned
`obsproject/libdshowcapture` commit `8878638324393815512f802640b0d5ce940161f1`.
Those files retain their upstream LGPL-2.1-or-later headers and the complete
license text is included as `third_party/libdshowcapture/COPYING`.

The Rust crate and `managed-virtualcam.cpp` are otherwise under this repository's
MIT license. Distributors of the native DLL must also satisfy the LGPL terms for
the linked libdshowcapture-derived portion.
