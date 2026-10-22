//! Cross-process publication primitives for shared-memory protocol fields.
//!
//! Layout alone is not the protocol: readers in other processes rely on the
//! order in which producer fields become visible. Every publish/observe of a
//! cross-process control field goes through these helpers so that ordering is
//! defined in one place instead of being rebuilt from volatile accesses and
//! fences at each call site.
//!
//! Each helper is a sequentially consistent 32-bit atomic access, the Rust
//! equivalent of the Win32 `Interlocked*` family used by MSVC peers. On x64 the
//! writes compile to locked instructions; on ARM64 they use acquire/release
//! instructions, so the protocol never depends on x86 store ordering. None of
//! them changes a field's size, alignment, or offset.

use core::sync::atomic::{AtomicU32, Ordering};

/// Atomically increments a producer sequence field and returns the new value,
/// wrapping like C unsigned arithmetic (`InterlockedIncrement`).
///
/// # Safety
///
/// `field` must point to a live, 4-byte-aligned `u32` in writable shared
/// memory. No access in this process may touch it non-atomically while the
/// call runs.
pub(crate) unsafe fn increment_u32(field: *mut u32) -> u32 {
    // SAFETY: The caller guarantees an aligned, live, writable field.
    unsafe { AtomicU32::from_ptr(field) }.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
}

/// Publishes `value` after every earlier write by this thread, including
/// non-atomic payload copies, is visible (`InterlockedExchange`).
///
/// # Safety
///
/// Same contract as [`increment_u32`].
pub(crate) unsafe fn publish_u32(field: *mut u32, value: u32) {
    // SAFETY: The caller guarantees an aligned, live, writable field.
    unsafe { AtomicU32::from_ptr(field) }.swap(value, Ordering::SeqCst);
}

/// Observes a published field; later reads by this thread are ordered after
/// it. This is the reader half of the contract and works on read-only views.
///
/// # Safety
///
/// `field` must point to a live, 4-byte-aligned `u32` inside a mapped view.
#[cfg(test)]
pub(crate) unsafe fn observe_u32(field: *const u32) -> u32 {
    // A volatile load plus a full barrier mirrors the native reader's
    // `MemoryBarrier` and, unlike an atomic RMW, never writes the page.
    // SAFETY: The caller guarantees an aligned, live, readable field.
    let value = unsafe { core::ptr::read_volatile(field) };
    core::sync::atomic::fence(Ordering::SeqCst);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_wraps_like_c_unsigned_arithmetic() {
        let mut field = u32::MAX - 1;
        // SAFETY: `field` is a live aligned local only accessed through helpers.
        unsafe {
            assert_eq!(increment_u32(&raw mut field), u32::MAX);
            assert_eq!(increment_u32(&raw mut field), 0);
            publish_u32(&raw mut field, 7);
            assert_eq!(observe_u32(&raw const field), 7);
        }
    }
}
