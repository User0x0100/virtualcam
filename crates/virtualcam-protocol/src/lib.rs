//! Wire protocol shared by the `virtualcam` producer library and the Rust
//! DirectShow / Media Foundation camera sources.
//!
//! This crate is platform-neutral and owns no OS handles. It defines:
//!
//! - [`format`]: the pixel-format / colorimetry / frame-rate model and its
//!   fixed 32-byte wire encoding.
//! - [`ring`]: the transport v2 frame ring — a header plus N slots, each slot
//!   guarded by a sequence lock so a reader never consumes a torn frame.
//! - [`sync`]: the cross-process publication primitives every shared field
//!   goes through (sequentially consistent atomics, ARM64-safe).
//! - [`dshow`]: DirectShow control-plane layout and object names
//!   (producer-owned format, session-local namespace).
//! - [`mf`]: Media Foundation vendor attributes and rendezvous handshake
//!   (source-owned format, cross-session transport).
//! - [`planes`]: pitch-aware plane copy and black fill used by both sources.
//!
//! Only the frame ring is common to both backends. Control planes,
//! ownership, naming, and readiness stay backend-specific by design.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod dshow;
pub mod format;
pub mod id;
pub mod mf;
pub mod planes;
pub mod registry;
pub mod ring;
pub mod sync;

pub use format::{ColorMatrix, ColorPrimaries, ColorRange, ColorSpace, FormatError, FrameLayout, FrameRate, PixelFormat, Plane, TransferFunction, ValidatedFormat, VideoFormat, WireVideoFormat};
pub use id::{CameraId, ParseIdError};
