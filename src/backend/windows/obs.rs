//! OBS Virtual Camera producer.
//!
//! This is the third-party OBS device protocol, not this crate's managed
//! DirectShow camera: it uses OBS's fixed `OBSVirtualCamVideo` queue name and
//! OBS's mapping-existence occupancy rule.

use std::path::Path;

use windows_sys::Win32::System::Memory::{FILE_MAP_READ, OpenFileMappingW};

use super::FrameSource;
use super::queue_writer::{ExistingQueue, QueueWriter};
use crate::config::{Nv12Format, ValidatedObsOptions};
use crate::platform::windows::{OwnedHandle, roaming_app_data_dir, wide_nul};
use crate::{CameraType, Error, Result, WriteStatus};

pub(super) const OBS_CLSID_KEY: &str = "CLSID\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}";
const QUEUE_NAME: &str = "OBSVirtualCamVideo";
const RESOLUTION_HINT_FILE: &str = "obs-virtualcam.txt";

pub(crate) struct ObsBackend {
    queue: Option<QueueWriter>,
}

impl ObsBackend {
    pub(crate) fn open(options: ValidatedObsOptions) -> Result<Self> {
        let backend = Self::open_named(options, &wide_nul(QUEUE_NAME))?;
        persist_resolution_hint(options.format);
        Ok(backend)
    }

    fn open_named(options: ValidatedObsOptions, name: &[u16]) -> Result<Self> {
        let existing = if options.force_open {
            trace_warn!("OBS force_open bypassed the conservative occupancy check");
            ExistingQueue::Reinitialize
        } else {
            // OBS has no separate producer lease. Preserve upstream
            // `video_queue_create`: any existing mapping means "in use".
            if mapping_exists(name) {
                trace_warn!("OBS queue mapping already exists");
                return Err(Error::DeviceInUse);
            }
            ExistingQueue::Reject
        };
        let queue = QueueWriter::create(name, options.format, existing, CameraType::Obs)?;
        Ok(Self { queue: Some(queue) })
    }

    pub(crate) fn is_ready(&mut self) -> Result<bool> {
        // OBS exposes no receiver-to-writer presence signal. A live queue is
        // protocol-ready regardless of whether a filter currently maps it.
        self.queue.as_ref().map(|_| true).ok_or(Error::AlreadyClosed)
    }

    pub(super) fn write_frame(&mut self, frame: FrameSource<'_>) -> Result<WriteStatus> {
        self.queue.as_mut().ok_or(Error::AlreadyClosed)?.write(frame)?;
        Ok(WriteStatus::Written)
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        // Dropping the writer publishes STOPPING before unmapping.
        self.queue = None;
        Ok(())
    }
}

fn mapping_exists(name: &[u16]) -> bool {
    // SAFETY: `name` is a NUL-terminated UTF-16 object name.
    let existing = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
    if existing.is_null() {
        return false;
    }
    // SAFETY: A non-null OpenFileMappingW result is one owned handle.
    drop(unsafe { OwnedHandle::from_raw(existing) });
    true
}

/// Mirrors OBS's `virtualcam_start`, which records the producer mode as
/// `{cx}x{cy}x{interval}` in `%APPDATA%\obs-virtualcam.txt`.
///
/// The OBS DirectShow filter reads this file when a consumer opens the camera
/// while no queue exists, so consumer-first applications negotiate this
/// producer's mode instead of scaling it from a stale one. Unlike upstream,
/// the hint is written only after the queue was created, so a failed open
/// never overwrites the mode of the producer that owns the camera. Failures
/// are not fatal: the filter then falls back to its previous or default mode.
fn persist_resolution_hint(format: Nv12Format) {
    let result = roaming_app_data_dir()
        .map_err(|error| error.to_string())
        .and_then(|directory| write_replacing(&directory.join(RESOLUTION_HINT_FILE), resolution_hint(format).as_bytes()).map_err(|error| error.to_string()));
    if let Err(error) = result {
        trace_warn!(%error, "could not record OBS virtual camera resolution hint");
        #[cfg(not(feature = "tracing"))]
        let _ = error;
    }
}

fn resolution_hint(format: Nv12Format) -> String {
    format!("{}x{}x{}", format.width, format.height, format.interval_100ns)
}

/// Replaces `path` through a sibling temporary file, like upstream
/// `os_quick_write_utf8_file_safe`, so readers never observe a partial hint.
fn write_replacing(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::super::queue_writer::tests::{QueueObserver, nv12, test_name};
    use super::*;
    use crate::backend::windows::protocol::frame_queue::{STATE_READY, STATE_STARTING, STATE_STOPPING};

    fn options(width: u32, height: u32, force_open: bool) -> ValidatedObsOptions {
        ValidatedObsOptions { format: nv12(width, height), force_open }
    }

    #[test]
    fn resolution_hint_matches_obs_virtualcam_txt_format() {
        let format = Nv12Format::new(1280, 720, crate::FrameRate { numerator: 30_000, denominator: 1001 }).expect("valid format");
        assert_eq!(resolution_hint(format), "1280x720x333666");
    }

    #[test]
    fn hint_replacement_overwrites_without_leaving_a_temporary_file() {
        let directory = std::env::temp_dir().join(format!("VirtualCamRs-ObsHint-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join(RESOLUTION_HINT_FILE);
        write_replacing(&path, b"1920x1080x333333").expect("write hint");
        write_replacing(&path, b"640x480x333333").expect("replace hint");
        assert_eq!(std::fs::read(&path).expect("read hint"), b"640x480x333333");
        assert!(!directory.join("obs-virtualcam.txt.tmp").exists());
        std::fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[test]
    fn real_mapping_covers_occupancy_force_write_and_stopping() {
        let name = test_name("Obs", "state");
        let mut original = ObsBackend::open_named(options(2, 2, false), &name).expect("first writer opens a new mapping");
        let observer = QueueObserver::open(&name);
        assert_eq!(observer.state(), STATE_STARTING);

        assert!(matches!(ObsBackend::open_named(options(2, 2, false), &name), Err(Error::DeviceInUse)));

        let mut forced = ObsBackend::open_named(options(2, 2, true), &name).expect("force mode reuses a large enough mapping");
        assert_eq!(forced.is_ready(), Ok(true));
        let frame = [1_u8, 2, 3, 4, 5, 6];
        assert!(matches!(forced.write_frame(FrameSource::Host(&frame[..5])), Err(Error::FrameSizeMismatch { expected: 6, actual: 5 })));
        assert_eq!(forced.write_frame(FrameSource::Host(&frame)), Ok(WriteStatus::Written));
        assert_eq!((observer.read_idx(), observer.state()), (1, STATE_READY));
        assert_eq!(observer.slot(1, frame.len()).1, frame);

        forced.close().expect("forced writer closes");
        forced.close().expect("close is idempotent");
        assert_eq!(forced.is_ready(), Err(Error::AlreadyClosed));
        assert_eq!(forced.write_frame(FrameSource::Host(&frame)), Err(Error::AlreadyClosed));
        // The original writer still maps the section and observes STOPPING.
        assert_eq!(observer.state(), STATE_STOPPING);
        original.close().expect("original writer closes");
    }

    #[test]
    fn force_open_rejects_a_small_existing_mapping() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};

        let name = test_name("Obs", "small");
        // SAFETY: Creates a page-file-backed test mapping with a unique name and
        // a deliberately insufficient one-page extent.
        let raw = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, 4096, name.as_ptr()) };
        // SAFETY: A non-null CreateFileMappingW result is one owned handle.
        let _mapping = unsafe { OwnedHandle::from_result(raw, "CreateFileMappingW(test small mapping)") }.expect("create small mapping");
        let error = match ObsBackend::open_named(options(64, 64, true), &name) {
            Ok(_) => panic!("small mapping must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::ExistingMappingTooSmall { required, available } if required > available));
    }
}
