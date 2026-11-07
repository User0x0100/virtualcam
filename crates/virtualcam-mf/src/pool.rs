//! Reusable linear samples for the default allocation path.
//!
//! Without a usable pipeline allocator every delivery used to create a fresh
//! sample and frame-sized memory buffer. Large buffers come straight from the
//! OS, so each frame paid for page faults and zeroing (about 0.45 ms per
//! 1080x1920 NV12 frame on the development machine). Tracked samples call back
//! (on an MF work queue) when the last consumer releases them; the pool takes them back and the next
//! delivery reuses the buffer.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL};
use windows::Win32::Media::MediaFoundation::{IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFSample, IMFTrackedSample, MFCreateMemoryBuffer, MFCreateTrackedSample};
use windows_core::{Interface, Ref, implement};

use crate::ModuleRef;

/// Released samples kept for reuse; more in flight are simply freed.
const CAPACITY: usize = 10;

/// A released sample and the size of its single buffer.
struct Free {
    size: usize,
    sample: IMFSample,
}

// SAFETY: Media Foundation samples and memory buffers are free-threaded.
unsafe impl Send for Free {}

#[derive(Default)]
struct Shelf {
    free: Mutex<Vec<Free>>,
}

impl Shelf {
    fn lock(&self) -> MutexGuard<'_, Vec<Free>> {
        self.free.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct SamplePool {
    shelf: Arc<Shelf>,
    recycle: IMFAsyncCallback,
}

// SAFETY: The callback is a free-threaded object of this crate; the shelf is lock-protected.
unsafe impl Send for SamplePool {}
// SAFETY: As above; no state outside the lock.
unsafe impl Sync for SamplePool {}

impl SamplePool {
    pub(crate) fn new() -> Self {
        let shelf = Arc::new(Shelf::default());
        let recycle = Recycle {
            shelf: Arc::downgrade(&shelf),
            _module: ModuleRef::new(),
        }
        .into();
        Self { shelf, recycle }
    }

    /// A sample with one linear buffer of exactly `size` bytes, attributes and
    /// flags cleared. The pool gets it back once every consumer released it.
    pub(crate) fn take(&self, size: usize) -> windows_core::Result<IMFSample> {
        let (reused, stale) = {
            let mut free = self.shelf.lock();
            let reused = free.iter().rposition(|entry| entry.size == size).map(|index| free.swap_remove(index).sample);
            // Another frame size (a new mode): those buffers are never reused.
            let stale: Vec<Free> = if reused.is_none() { free.drain(..).collect() } else { Vec::new() };
            (reused, stale)
        };
        // Freed outside the lock; their callbacks were already cleared.
        drop(stale);
        let sample = match reused {
            Some(sample) => {
                // SAFETY: Valid sample that only this pool references now.
                unsafe {
                    sample.DeleteAllItems()?;
                    sample.SetSampleFlags(0)?;
                }
                sample
            }
            None => {
                let length = u32::try_from(size).map_err(|_| windows_core::Error::from(E_INVALIDARG))?;
                // SAFETY: Plain MF object creation; valid sample and buffer.
                unsafe {
                    let sample: IMFSample = MFCreateTrackedSample()?.cast()?;
                    sample.AddBuffer(&MFCreateMemoryBuffer(length)?)?;
                    sample
                }
            }
        };
        let tracked: IMFTrackedSample = sample.cast()?;
        // SAFETY: Valid tracked sample whose previous callback already fired (or none was set).
        unsafe { tracked.SetAllocator(&self.recycle, None) }?;
        Ok(sample)
    }
}

/// Puts released samples back on the shelf; outlives the pool harmlessly.
#[implement(IMFAsyncCallback)]
struct Recycle {
    shelf: Weak<Shelf>,
    _module: ModuleRef,
}

impl IMFAsyncCallback_Impl for Recycle_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows_core::Result<()> {
        let result = result.ok()?;
        // SAFETY: Valid async result; its object is the released sample.
        let sample: IMFSample = unsafe { result.GetObject() }?.cast()?;
        // SAFETY: Valid sample with the single buffer this pool added.
        let size = unsafe { sample.GetBufferByIndex(0).and_then(|buffer| buffer.GetMaxLength()) };
        let (Some(shelf), Ok(size)) = (self.shelf.upgrade(), size) else { return Ok(()) };
        let mut free = shelf.lock();
        if free.len() < CAPACITY {
            free.push(Free { size: size as usize, sample });
            return Ok(());
        }
        drop(free);
        // `sample` is freed here, outside the lock; its callback is already cleared.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFStartup};
    use windows_core::GUID;

    use super::*;

    /// The release callback runs on an MF work queue, not inside `Release`.
    fn shelved(pool: &SamplePool, count: usize) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while pool.shelf.lock().len() != count {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        true
    }

    #[test]
    fn released_samples_are_reused_and_cleared() {
        // SAFETY: Starts MF for this test; every object below is valid and owned.
        unsafe {
            MFStartup(MF_VERSION, 0).expect("MF");
            let pool = SamplePool::new();
            let first = pool.take(4096).expect("sample");
            let buffer = first.GetBufferByIndex(0).expect("buffer").as_raw();
            first.SetUINT32(&GUID::from_u128(1), 7).expect("attribute");
            first.SetSampleTime(42).expect("time");
            drop(first);
            assert!(shelved(&pool, 1), "release returned the sample");

            let second = pool.take(4096).expect("reused");
            assert_eq!(second.GetBufferByIndex(0).expect("buffer").as_raw(), buffer, "same buffer reused");
            assert_eq!(second.GetCount().expect("count"), 0, "attributes such as the request token are cleared");
            drop(second);
            assert!(shelved(&pool, 1));

            let other = pool.take(8192).expect("new size");
            assert!(pool.shelf.lock().is_empty(), "buffers of another size are dropped");
            assert_eq!(other.GetBufferByIndex(0).and_then(|buffer| buffer.GetMaxLength()).expect("length"), 8192);
            drop(other);
            assert!(shelved(&pool, 1));
            drop(pool);
        }
    }
}
