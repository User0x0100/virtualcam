# Upstream protocol record

This file pins the protocol sources used by `virtualcam` 0.1.0. The two Rust examples supplied with the original task were used only as calling-scenario clues.

## Revisions

| Project | Branch inspected | Commit |
| --- | --- | --- |
| [obsproject/obs-studio](https://github.com/obsproject/obs-studio) | `master` | `ca937366c1e9fc7f22fd5f1c7d3a3a5f7951170b` |
| [schellingb/UnityCapture](https://github.com/schellingb/UnityCapture) | `master` | `3ed54c325e0ad71afcf4f246c07e5e17b3d7f2d2` |

The OBS and Unity revisions were resolved and checked out on 2026-07-14. Runtime code does not fetch or follow these branches.

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

Rust coverage: `src/backend/windows/obs.rs` (naming, occupancy, resolution hint), `src/backend/windows/queue_writer.rs` (queue writer), `src/backend/windows/protocol/frame_queue.rs` (layout), and `src/backend/windows/protocol/publication.rs` (ordering).

| Property | Pinned upstream behavior | Rust mapping |
| --- | --- | --- |
| Mapping | wide name `OBSVirtualCamVideo` | exact UTF-16 name |
| Writer occupancy | `OpenFileMappingW(FILE_MAP_READ)` succeeds => fail | normal `DeviceInUse` |
| Creation/view | page-file `CreateFileMappingW(PAGE_READWRITE)`, `MapViewOfFile(FILE_MAP_ALL_ACCESS)` | same APIs/access |
| Header | 80 bytes, alignment 8; volatile `uint32_t` write/read/state | `#[repr(C)] QueueHeader`; write/read/state accessed only through sequentially consistent 32-bit atomic helpers (`Interlocked*` equivalent) |
| States | invalid 0, starting 1, ready 2, stopping 3 | exact values |
| Slots | 3; header and each `(32-byte frame header + NV12)` extent aligned to 32 | checked layout calculation |
| Frame | timestamp at slot start; image at `slot + 32`; Y then UV | tight NV12 split at `width * height` |
| Publish order | increment write index, timestamp, Y, UV, read index, READY | identical observable order; every publish is a full barrier, so the order also holds on ARM64 |
| Interval | `fps_den * 10_000_000 / fps_num` in 100ns units | exact `ObsFrameRate` integer calculation |
| Timestamp | `video_data.timestamp`, derived from QPC as nanoseconds | QPC converted to nanoseconds with upstream mul/div algorithm |
| Close | writer stores STOPPING before unmap/close | same |
| Resolution hint | `virtualcam_start` writes `%APPDATA%\obs-virtualcam.txt` as `%dx%dx%lld` (tmp + replace) before creating the queue; the filter reads it when no queue exists | same format and replace-through-temporary write, but only after the queue was created |

The DirectShow consumer currently discards the stored slot timestamp when producing its sample, but the Rust writer still publishes the upstream nanosecond value.

## Managed cameras

The managed DirectShow and Media Foundation cameras (`crates/virtualcam-dshow`, `crates/virtualcam-mf`, `crates/virtualcam-protocol`) are original Rust implementations of the public DirectShow and Frame Server custom-media-source contracts. They contain no OBS or libdshowcapture code; the retired libdshowcapture-derived C++ driver was removed after `ManagedCameraManager::legacy_cameras` / `remove_legacy_camera` provided its migration path. The OBS queue above is used only by the OBS producer.

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
- Like the OBS application, a successful OBS open writes `%APPDATA%\obs-virtualcam.txt`, because the OBS filter uses it as its consumer-first mode; without it a consumer that opens first negotiates a stale mode and the filter scales this producer's frames into it. Unlike upstream, the file is written only after the queue was created, so a producer rejected with `DeviceInUse` never overwrites the active producer's mode. (Earlier releases deliberately skipped this file.)
- Queue control fields use sequentially consistent atomics instead of relying on MSVC's x86/x64 `/volatile:ms` acquire/release semantics. The binary layout and the publication sequence are unchanged.
- Unity's D3D11 texture readback and optional double-buffering are outside `SharedImageMemory::Send`; host/CUDA callers already provide an accessible buffer, so those plugin-only controls are not fabricated by this crate.
- CUDA writes replace only the upstream CPU data-copy step: one Runtime D2H copy
  targets the selected OBS slot or Unity data field directly, and that stream is
  synchronized before OBS publication fields or Unity's mutex/event publication.
  The mapped destination is contiguous virtual memory but is not registered or
  page-locked by this crate.

## Upgrade checklist

Before changing either pinned SHA, re-check object names/access masks, struct size/alignment/offsets on 32-bit and 64-bit MSVC, state values, slot count/alignment, interval/timestamp units, publish order, sender/receiver create/open roles, automatic-reset flags, maximum mapping size, capture suffix encoding, metadata enum values, mutex/event order, frame-skip behavior, and closing transitions. Update conformance assertions and this file in the same change.
