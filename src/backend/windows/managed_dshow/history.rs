//! Best-effort per-user, per-camera history for the last successfully published NV12 mode.
//! A single REG_BINARY value makes the three format fields an atomic update.
//!
//! The native DirectShow source reads this record to choose its idle mode when
//! no producer exists. It is DirectShow-only state: a Media Foundation camera
//! owns a persistent native mode instead and must not consult it.

use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_BINARY, REG_OPTION_NON_VOLATILE, RegCloseKey, RegCreateKeyExW, RegSetValueExW};

use crate::ManagedCameraId;
use crate::config::Nv12Format;
use crate::platform::windows::wide_nul;

const HISTORY_ROOT: &str = "Software\\VirtualCamRs\\ManagedCameraFormats";
const VALUE_NAME: &str = "LastFormat";
const MAGIC: u32 = u32::from_le_bytes(*b"VCMF");
const VERSION: u32 = 1;
const RECORD_SIZE: usize = 24;

fn encode_format(format: Nv12Format) -> [u8; RECORD_SIZE] {
    let mut bytes = [0_u8; RECORD_SIZE];
    bytes[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
    bytes[8..12].copy_from_slice(&format.width.to_le_bytes());
    bytes[12..16].copy_from_slice(&format.height.to_le_bytes());
    bytes[16..24].copy_from_slice(&format.interval_100ns.to_le_bytes());
    bytes
}

/// Persists `format` for `id`, returning the Win32 status on failure.
pub(super) fn persist(id: ManagedCameraId, format: Nv12Format) -> Result<(), u32> {
    let path = wide_nul(&format!("{HISTORY_ROOT}\\{id}"));
    let value_name = wide_nul(VALUE_NAME);
    let mut key: HKEY = core::ptr::null_mut();
    // SAFETY: Both strings are NUL-terminated and the returned key is closed below.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            core::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            core::ptr::null(),
            &mut key,
            core::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(status);
    }
    let data = encode_format(format);
    // SAFETY: The registry key and value buffer remain valid throughout the synchronous call.
    let status = unsafe { RegSetValueExW(key, value_name.as_ptr(), 0, REG_BINARY, data.as_ptr(), RECORD_SIZE as u32) };
    // SAFETY: RegCreateKeyExW returned one owned HKEY.
    unsafe {
        RegCloseKey(key);
    }
    if status != 0 {
        return Err(status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrameRate;

    fn format(width: u32, height: u32, fps: u32) -> Nv12Format {
        Nv12Format::new(width, height, FrameRate { numerator: fps, denominator: 1 }).expect("valid test format")
    }

    #[test]
    fn registry_persists_per_camera_and_replaces_format_atomically() {
        use windows_sys::Win32::System::Registry::{RRF_RT_REG_BINARY, RegDeleteTreeW, RegGetValueW};

        let id: ManagedCameraId = format!("{{{:08X}-A156-4BCE-9317-22F111AB8C01}}", std::process::id()).parse().expect("unique test camera id");
        let key = wide_nul(&format!("{HISTORY_ROOT}\\{id}"));
        let value = wide_nul(VALUE_NAME);
        struct Cleanup(Vec<u16>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                // SAFETY: Only the ephemeral per-test HKCU key is removed.
                unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, self.0.as_ptr()) };
            }
        }
        let _cleanup = Cleanup(key.clone());

        let read = || {
            let mut data = [0_u8; RECORD_SIZE];
            let mut data_type = 0;
            let mut length = RECORD_SIZE as u32;
            // SAFETY: The value buffer has exactly the advertised capacity.
            let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_BINARY, &mut data_type, data.as_mut_ptr().cast(), &mut length) };
            assert_eq!(status, 0, "read saved format");
            assert_eq!(data_type, REG_BINARY);
            assert_eq!(length, RECORD_SIZE as u32);
            data
        };
        persist(id, format(1280, 720, 30)).expect("save initial mode");
        assert_eq!(read(), encode_format(format(1280, 720, 30)));
        persist(id, format(1920, 1080, 60)).expect("replace saved mode");
        assert_eq!(read(), encode_format(format(1920, 1080, 60)));
    }

    #[test]
    fn history_record_has_stable_little_endian_layout() {
        let bytes = encode_format(format(1920, 1080, 60));
        assert_eq!(&bytes[0..4], b"VCMF");
        assert_eq!(&bytes[4..8], &1_u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &1920_u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &1080_u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &166_666_u64.to_le_bytes());
    }
}
