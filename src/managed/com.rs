//! COM apartment ownership for manager calls.

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

use crate::{Error, Result};

/// Joins (or initializes) a COM apartment for the current thread.
///
/// The caller's existing apartment is respected: if the thread is already
/// STA, COM is used as-is and never uninitialized here. `CoUninitialize` runs
/// on drop only when this guard's own initialization succeeded.
pub(crate) struct ComApartment {
    owns_init: bool,
}

impl ComApartment {
    pub(crate) fn enter() -> Result<Self> {
        // SAFETY: Plain apartment initialization for the calling thread.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self { owns_init: false });
        }
        if hr.is_err() {
            return Err(Error::Hresult { operation: "CoInitializeEx", code: hr.0 });
        }
        // S_OK and S_FALSE both take a reference that must be balanced.
        Ok(Self { owns_init: true })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owns_init {
            // SAFETY: Balances this guard's successful CoInitializeEx.
            unsafe { CoUninitialize() };
        }
    }
}

pub(crate) fn hresult_error(operation: &'static str, error: &windows::core::Error) -> Error {
    Error::Hresult { operation, code: error.code().0 }
}
