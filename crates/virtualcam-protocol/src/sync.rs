//! Cross-process publication primitives.
//!
//! Layout alone is not the protocol: peers in other processes rely on the
//! order in which fields become visible. Every cross-process control field is
//! published and observed through these helpers, which are sequentially
//! consistent atomics — the Rust equivalent of Win32 `Interlocked*`. On x64
//! they compile to locked instructions; on ARM64 to acquire/release
//! instructions, so the protocol never depends on x86 store ordering.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// Publishes `value` after every earlier write by this thread (including
/// plain payload copies) is visible.
///
/// # Safety
///
/// `field` must point to a live, 4-byte-aligned `u32` in writable memory that
/// this process accesses only through these helpers.
pub unsafe fn publish_u32(field: *mut u32, value: u32) {
    // SAFETY: Forwarded caller contract.
    unsafe { AtomicU32::from_ptr(field) }.swap(value, Ordering::SeqCst);
}

/// Observes a published `u32`; later reads by this thread are ordered after it.
///
/// Implemented as a volatile load plus a full fence so it works on read-only
/// views and never dirties the page.
///
/// # Safety
///
/// `field` must point to a live, 4-byte-aligned `u32` inside a mapped view.
pub unsafe fn observe_u32(field: *const u32) -> u32 {
    // SAFETY: Forwarded caller contract.
    let value = unsafe { core::ptr::read_volatile(field) };
    fence(Ordering::SeqCst);
    value
}

/// Publishes a `u64` (see [`publish_u32`]).
///
/// # Safety
///
/// `field` must point to a live, 8-byte-aligned `u64` in writable memory.
pub unsafe fn publish_u64(field: *mut u64, value: u64) {
    // SAFETY: Forwarded caller contract.
    unsafe { AtomicU64::from_ptr(field) }.swap(value, Ordering::SeqCst);
}

/// Observes a published `u64` (see [`observe_u32`]).
///
/// Aligned 64-bit loads are single-copy atomic on every supported Windows
/// target (x64, ARM64), so a volatile load cannot tear.
///
/// # Safety
///
/// `field` must point to a live, 8-byte-aligned `u64` inside a mapped view.
pub unsafe fn observe_u64(field: *const u64) -> u64 {
    // SAFETY: Forwarded caller contract.
    let value = unsafe { core::ptr::read_volatile(field) };
    fence(Ordering::SeqCst);
    value
}

/// Orders all earlier plain reads before later observations (seqlock reader
/// validation step).
pub fn read_barrier() {
    fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_round_trip_values() {
        let mut a = 0_u32;
        let mut b = 0_u64;
        // SAFETY: Live aligned locals accessed only through the helpers.
        unsafe {
            publish_u32(&raw mut a, 7);
            publish_u64(&raw mut b, u64::MAX - 1);
            assert_eq!(observe_u32(&raw const a), 7);
            assert_eq!(observe_u64(&raw const b), u64::MAX - 1);
        }
        read_barrier();
    }
}
