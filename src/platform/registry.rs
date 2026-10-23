//! Minimal owned registry key wrapper over the wide-character Win32 API.

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_BINARY, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW,
};

use super::windows::wide_nul;
use crate::{Error, Result};

/// Predefined registry root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hive {
    CurrentUser,
    LocalMachine,
}

impl Hive {
    const fn raw(self) -> HKEY {
        match self {
            Self::CurrentUser => HKEY_CURRENT_USER,
            Self::LocalMachine => HKEY_LOCAL_MACHINE,
        }
    }
}

fn check(operation: &'static str, status: u32) -> Result<()> {
    if status == ERROR_SUCCESS { Ok(()) } else { Err(Error::Windows { operation, code: status }) }
}

pub(crate) struct Key(HKEY);

impl Key {
    /// Opens `path` for reading; `Ok(None)` when it does not exist.
    pub(crate) fn open(hive: Hive, path: &str) -> Result<Option<Self>> {
        Self::open_with(hive, path, KEY_READ)
    }

    /// Opens `path` for reading and writing; `Ok(None)` when it does not exist.
    pub(crate) fn open_writable(hive: Hive, path: &str) -> Result<Option<Self>> {
        Self::open_with(hive, path, KEY_READ | KEY_WRITE)
    }

    fn open_with(hive: Hive, path: &str, access: u32) -> Result<Option<Self>> {
        let path = wide_nul(path);
        let mut key: HKEY = core::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated and `key` is writable output storage.
        let status = unsafe { RegOpenKeyExW(hive.raw(), path.as_ptr(), 0, access, &mut key) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check("RegOpenKeyExW", status)?;
        Ok(Some(Self(key)))
    }

    /// Creates (or opens) `path` for reading and writing.
    pub(crate) fn create(hive: Hive, path: &str) -> Result<Self> {
        let path = wide_nul(path);
        let mut key: HKEY = core::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated and `key` is writable output storage.
        let status = unsafe {
            RegCreateKeyExW(
                hive.raw(),
                path.as_ptr(),
                0,
                core::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                core::ptr::null(),
                &mut key,
                core::ptr::null_mut(),
            )
        };
        check("RegCreateKeyExW", status)?;
        Ok(Self(key))
    }

    /// Deletes `path` and all its subkeys; a missing key is not an error.
    pub(crate) fn delete_tree(hive: Hive, path: &str) -> Result<()> {
        let path = wide_nul(path);
        // SAFETY: `path` is NUL-terminated; the hive is a predefined root.
        let status = unsafe { RegDeleteTreeW(hive.raw(), path.as_ptr()) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check("RegDeleteTreeW", status)
    }

    /// Reports whether `path` exists.
    pub(crate) fn exists(hive: Hive, path: &str) -> bool {
        matches!(Self::open(hive, path), Ok(Some(_)))
    }

    fn set_raw(&self, name: Option<&str>, kind: u32, data: &[u8]) -> Result<()> {
        let name = name.map(wide_nul);
        let length = u32::try_from(data.len()).map_err(|_| Error::FrameSizeOverflow)?;
        // SAFETY: The key is open; `name` (or null for the default value) and
        // `data` stay alive for the synchronous call.
        let status = unsafe { RegSetValueExW(self.0, name.as_ref().map_or(core::ptr::null(), |n| n.as_ptr()), 0, kind, data.as_ptr(), length) };
        check("RegSetValueExW", status)
    }

    /// Writes a `REG_SZ` value; `None` names the key's default value.
    pub(crate) fn set_string(&self, name: Option<&str>, value: &str) -> Result<()> {
        let bytes: Vec<u8> = wide_nul(value).iter().flat_map(|unit| unit.to_le_bytes()).collect();
        self.set_raw(name, REG_SZ, &bytes)
    }

    /// Writes a `REG_DWORD` value.
    pub(crate) fn set_u32(&self, name: &str, value: u32) -> Result<()> {
        self.set_raw(Some(name), REG_DWORD, &value.to_le_bytes())
    }

    /// Writes a `REG_BINARY` value.
    pub(crate) fn set_binary(&self, name: &str, value: &[u8]) -> Result<()> {
        self.set_raw(Some(name), REG_BINARY, value)
    }

    /// Reads a value of `kind`; `Ok(None)` when absent or of another type.
    fn get_raw(&self, name: Option<&str>, kind: u32) -> Result<Option<Vec<u8>>> {
        let name = name.map(wide_nul);
        let name_ptr = name.as_ref().map_or(core::ptr::null(), |n| n.as_ptr());
        // Values can change between the size query and the read; retry a few times.
        for _ in 0..4 {
            let mut actual = 0_u32;
            let mut length = 0_u32;
            // SAFETY: Size-only query on an open key.
            let status = unsafe { RegQueryValueExW(self.0, name_ptr, core::ptr::null(), &mut actual, core::ptr::null_mut(), &mut length) };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            check("RegQueryValueExW(size)", status)?;
            if actual != kind {
                return Ok(None);
            }
            let mut buffer = vec![0_u8; length as usize];
            // SAFETY: `buffer` has exactly `length` writable bytes.
            let status = unsafe { RegQueryValueExW(self.0, name_ptr, core::ptr::null(), &mut actual, buffer.as_mut_ptr(), &mut length) };
            if status == windows_sys::Win32::Foundation::ERROR_MORE_DATA {
                continue;
            }
            check("RegQueryValueExW", status)?;
            if actual != kind {
                return Ok(None);
            }
            buffer.truncate(length as usize);
            return Ok(Some(buffer));
        }
        Err(Error::Windows {
            operation: "RegQueryValueExW(value kept changing)",
            code: windows_sys::Win32::Foundation::ERROR_MORE_DATA,
        })
    }

    /// Reads a `REG_SZ` value; `None` names the default value.
    pub(crate) fn get_string(&self, name: Option<&str>) -> Result<Option<String>> {
        let Some(bytes) = self.get_raw(name, REG_SZ)? else {
            return Ok(None);
        };
        let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        let end = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());
        Ok(String::from_utf16(&units[..end]).ok())
    }

    /// Reads a `REG_DWORD` value.
    pub(crate) fn get_u32(&self, name: &str) -> Result<Option<u32>> {
        Ok(self.get_raw(Some(name), REG_DWORD)?.and_then(|bytes| <[u8; 4]>::try_from(bytes.as_slice()).ok()).map(u32::from_le_bytes))
    }

    /// Reads a `REG_BINARY` value.
    pub(crate) fn get_binary(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.get_raw(Some(name), REG_BINARY)
    }

    /// Names of the direct subkeys.
    pub(crate) fn subkeys(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut buffer = [0_u16; 256];
        for index in 0.. {
            let mut length = buffer.len() as u32;
            // SAFETY: `buffer` has `length` writable UTF-16 units.
            let status = unsafe { RegEnumKeyExW(self.0, index, buffer.as_mut_ptr(), &mut length, core::ptr::null(), core::ptr::null_mut(), core::ptr::null_mut(), core::ptr::null_mut()) };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            check("RegEnumKeyExW", status)?;
            if let Ok(name) = String::from_utf16(&buffer[..length as usize]) {
                names.push(name);
            }
        }
        Ok(names)
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: `Key` owns one open registry handle.
        unsafe { RegCloseKey(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_under_hkcu() {
        let path = format!("Software\\VirtualCamRs\\Test-{}", std::process::id());
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Key::delete_tree(Hive::CurrentUser, &self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());

        assert!(Key::open(Hive::CurrentUser, &path).expect("open").is_none());
        let key = Key::create(Hive::CurrentUser, &path).expect("create");
        key.set_string(None, "default").expect("default");
        key.set_string(Some("Name"), "Caméra ✓").expect("string");
        key.set_u32("Number", 7).expect("dword");
        key.set_binary("Blob", &[1, 2, 3]).expect("binary");
        Key::create(Hive::CurrentUser, &format!("{path}\\Child")).expect("child");

        let key = Key::open(Hive::CurrentUser, &path).expect("open").expect("exists");
        assert_eq!(key.get_string(None).expect("read"), Some("default".into()));
        assert_eq!(key.get_string(Some("Name")).expect("read"), Some("Caméra ✓".into()));
        assert_eq!(key.get_u32("Number").expect("read"), Some(7));
        assert_eq!(key.get_binary("Blob").expect("read"), Some(vec![1, 2, 3]));
        assert_eq!(key.get_u32("Name").expect("wrong type"), None);
        assert_eq!(key.get_string(Some("Missing")).expect("missing"), None);
        assert_eq!(key.subkeys().expect("enum"), vec!["Child".to_owned()]);

        Key::delete_tree(Hive::CurrentUser, &path).expect("delete");
        assert!(!Key::exists(Hive::CurrentUser, &path));
        Key::delete_tree(Hive::CurrentUser, &path).expect("delete missing is ok");
    }
}
