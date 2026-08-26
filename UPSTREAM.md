# Upstream protocol record

This file pins the protocol sources used by `virtualcam` 0.1.0. The two Rust examples supplied with the original task were used only as calling-scenario clues.

## Revisions

| Project | Branch inspected | Commit |
| --- | --- | --- |
| [obsproject/obs-studio](https://github.com/obsproject/obs-studio) | `master` | `ca937366c1e9fc7f22fd5f1c7d3a3a5f7951170b` |
| [obsproject/libdshowcapture](https://github.com/obsproject/libdshowcapture) | OBS pinned submodule | `8878638324393815512f802640b0d5ce940161f1` |
| [schellingb/UnityCapture](https://github.com/schellingb/UnityCapture) | `master` | `3ed54c325e0ad71afcf4f246c07e5e17b3d7f2d2` |

The OBS and Unity revisions were resolved and checked out on 2026-07-14; the libdshowcapture revision is the submodule pinned by that OBS tree and was materialized for the managed-driver work on 2026-08-26. Runtime code does not fetch or follow these branches.

## OBS mapping

Primary sources:

- `shared/obs-shared-memory-queue/shared-memory-queue.c`
- `shared/obs-shared-memory-queue/shared-memory-queue.h`
- `plugins/win-dshow/virtualcam.c`
- `plugins/win-dshow/virtualcam-module/virtualcam-filter.cpp`
- `libobs/media-io/video-io.h`
- `libobs/obs-video.c`
- `libobs/util/platform-windows.c`
- `libobs/util/util_uint64.h`
- `CMakePresets.json` (installed virtual-camera CLSID)

Rust coverage: `src/backend/windows/obs.rs` and `src/backend/windows/protocol/obs.rs`.

| Property | Pinned upstream behavior | Rust mapping |
| --- | --- | --- |
| Mapping | wide name `OBSVirtualCamVideo` | exact UTF-16 name |
| Writer occupancy | `OpenFileMappingW(FILE_MAP_READ)` succeeds => fail | normal `DeviceInUse` |
| Creation/view | page-file `CreateFileMappingW(PAGE_READWRITE)`, `MapViewOfFile(FILE_MAP_ALL_ACCESS)` | same APIs/access |
| Header | 80 bytes, alignment 8; volatile `uint32_t` write/read/state | `#[repr(C)] QueueHeader`; narrow volatile helpers |
| States | invalid 0, starting 1, ready 2, stopping 3 | exact values |
| Slots | 3; header and each `(32-byte frame header + NV12)` extent aligned to 32 | checked layout calculation |
| Frame | timestamp at slot start; image at `slot + 32`; Y then UV | tight NV12 split at `width * height` |
| Publish order | increment write index, timestamp, Y, UV, read index, READY | identical observable order |
| Interval | `fps_den * 10_000_000 / fps_num` in 100ns units | exact `ObsFrameRate` integer calculation |
| Timestamp | `video_data.timestamp`, derived from QPC as nanoseconds | QPC converted to nanoseconds with upstream mul/div algorithm |
| Close | writer stores STOPPING before unmap/close | same |

The DirectShow consumer currently discards the stored slot timestamp when producing its sample, but the Rust writer still publishes the upstream nanosecond value.

## Managed NV12 DirectShow driver

Primary sources/design references:

- OBS `plugins/win-dshow/virtualcam-module/virtualcam-filter.cpp`
- OBS `plugins/win-dshow/virtualcam-module/virtualcam-module.cpp`
- OBS `shared/obs-shared-memory-queue/shared-memory-queue.c`
- libdshowcapture `source/output-filter.cpp/.hpp`
- libdshowcapture media-type helpers pinned above

Native coverage: `native/managed-virtualcam`; Rust management coverage: `src/managed.rs` plus the named-queue reuse in `src/backend/windows/obs.rs`.

The managed driver intentionally keeps only the NV12 path. It does not register the OBS CLSID, does not use `OBSVirtualCamVideo`, and does not advertise I420/YUY2. Registration persists only device identity. Runtime ABI v3 uses four CLSID-derived object classes: `-Producer` is an exclusive producer lease, `-Ready` is a reusable publication event, `-Control` is a fixed 32-byte format declaration, and the actual OBS-compatible three-slot queue is `-Data-{width}x{height}-{interval}`. The producer reserves the unsignaled lease, resets Ready, invalidates the control plane, initializes the format-specific data mapping, publishes control metadata, signals Ready, and signals the producer lease last. `IClassFactory::CreateInstance` waits on that publication and validates the control plane; only a genuinely absent producer becomes the `640x480@30` black-frame fallback, while corrupt/inaccessible live-session metadata is surfaced as an activation error. A connected graph remains fixed-mode, but an old consumer can retain its prior data section without limiting the next producer's section size. Queue discovery is throttled after missing/incompatible sessions, and stagnant `read_idx` values trigger producer-lease/control checks so an abnormally terminated producer falls back to black instead of freezing its final frame. Rust `open` resolves the CLSID's actual `InprocServer32` target and requires ABI v3, so incompatible v2 runtime transports are rejected. Registration may point to a stable app-local DLL on a local drive; the module is canonicalized through a file handle, UNC/network modules are rejected, and the ordinary local DOS path is persisted to `InprocServer32`. Rename/unregister rollback snapshots and restores that original COM module path rather than substituting the currently loaded manager DLL.

The native DLL uses libdshowcapture's `OutputFilter` infrastructure under its upstream LGPL-2.1-or-later license. The vendored subset and complete LGPL text live under `native/managed-virtualcam/third_party/libdshowcapture`. The local `output-filter.cpp` copy carries one deliberate fixed-mode hardening change: `Connect`, `QueryAccept`, and `IAMStreamConfig::SetFormat` reject media types whose video subtype, dimensions, or frame interval differ from the mode selected for that graph (live producer format or idle fallback), preventing allocator-size drift from the NV12 producer contract. Two additional vendored edits are warning-only and behavior-neutral: the local `Pause` variable avoids shadowing the `pin` member, and `log.hpp` guards its `WIN32_LEAN_AND_MEAN` definition.

## Unity Capture mapping

Primary sources:

- `Source/shared.inl`
- `Source/UnityCapturePlugin.cpp`
- `Source/UnityCaptureFilter.cpp`
- `UnityCaptureSample/Assets/UnityCapture/UnityCapture.cs` (default timeout/resize/mirror choices)

Rust coverage: `src/backend/windows/unity_capture.rs` and `src/backend/windows/protocol/unity_capture.rs`.

| Property | Pinned upstream sender behavior | Rust mapping |
| --- | --- | --- |
| Names | `UnityCapture_Mutx0`, `Want0`, `Sent0`, `Data0`; capture 0 replaces suffix with NUL | exact suffix rule for configurable capture numbers |
| Mutex | receiver creates; sender opens with `SYNCHRONIZE`; infinite wait | same |
| Want event | sender creates automatic-reset event | same |
| Sent event | receiver creates automatic-reset event; sender opens `EVENT_MODIFY_STATE` | same |
| Mapping | receiver creates `sizeof(header) + MAX_SHARED_IMAGE_SIZE`; sender opens/maps `FILE_MAP_WRITE` | same sender path |
| Header | `DWORD maxSize`, then seven C `int`, then `uint8_t data[1]` | size 36, alignment 4, data offset 32 |
| Maximum | `3840 * 2160 * 4 * sizeof(short)` = 66,355,200 bytes | exact bound |
| Formats | `FORMAT_UINT8`, `FORMAT_FP16_GAMMA`, `FORMAT_FP16_LINEAR` | exact configurable values 0, 1, 2 |
| Stride | D3D `RowPitch / 4` or `/ 8`, in pixels | tight `width` default or explicit pixel stride |
| Metadata | resize mode, mirror mode, timeout | configurable exact C enum/int values; upstream defaults 0, 0, 1000ms |
| Send order | maxSize check; mutex; metadata; data; release; signal Sent; poll Want | exact order |
| Skip | Want poll is not signaled after the send | `FrameSkippedByConsumer` after successful publication |

`SharedMemHeader` has no `data_size` or `frame_counter` field.

## Intentional differences and safety checks

- OBS exposes its upstream numerator/denominator rate pair instead of losing precision through a floating frame rate.
- A registry preflight reports `BackendUnavailable` before creating resources. OBS uses its pinned CLSID; Unity uses the architecture-specific capture-0 CLSID.
- If a mapping appears in OBS's normal-mode race window between preflight open and creation, Rust returns `DeviceInUse`. Upstream does not inspect `ERROR_ALREADY_EXISTS` at that second point.
- `force_open` is a requested extension, isolated in the OBS open path. It validates the accessible mapped region with `VirtualQuery` before reinitializing.
- Unity Capture's C++ destructor omits `UnmapViewOfFile`; Rust owns and unmaps the view with RAII.
- Failed mutex waits/releases and event operations become errors with their exact Win32 code instead of being ignored.
- The OBS application also writes `obs-virtualcam.txt` as a last-format fallback. This library owns only transfer/protocol resources and does not write application configuration files; a live queue carries its current dimensions and interval.
- Unity's D3D11 texture readback and optional double-buffering are outside `SharedImageMemory::Send`; host/CUDA callers already provide an accessible buffer, so those plugin-only controls are not fabricated by this crate.
- CUDA writes replace only the upstream CPU data-copy step: one Runtime D2H copy
  targets the selected OBS slot or Unity data field directly, and that stream is
  synchronized before OBS publication fields or Unity's mutex/event publication.
  The mapped destination is contiguous virtual memory but is not registered or
  page-locked by this crate.

## Upgrade checklist

Before changing either pinned SHA, re-check object names/access masks, struct size/alignment/offsets on 32-bit and 64-bit MSVC, state values, slot count/alignment, interval/timestamp units, publish order, sender/receiver create/open roles, automatic-reset flags, maximum mapping size, capture suffix encoding, metadata enum values, mutex/event order, frame-skip behavior, and closing transitions. Update conformance assertions and this file in the same change.
