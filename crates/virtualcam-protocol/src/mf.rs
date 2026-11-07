//! Media Foundation configuration attributes and producer rendezvous protocol.
//!
//! The MF source runs inside Frame Server (Local Service, session 0) and owns
//! the camera's media capability. The manager passes configuration to the
//! fixed-CLSID source through vendor attributes on `IMFVirtualCamera`; all of
//! them are untrusted configuration, never authorization.
//!
//! Producer attach:
//!
//! ```text
//! producer                                   source (Frame Server)
//!   CreateFile(\\.\pipe\VirtualCamRs-MF-{RENDEZVOUS}, SECURITY_IDENTIFICATION)
//!   -> Hello { camera id, format }   ----->  bounded read, validate
//!                                            ImpersonateNamedPipeClient
//!                                            TokenUser SID == expected owner SID
//!                                            RevertToSelf
//!                                            create ring + frame event with fresh nonce + DACL
//!   <- Welcome { status, ring name, frame event name } <-----
//!   OpenFileMapping(ring), OpenEvent(frame), attach writer
//!   commit frame -> SetEvent(frame)  ----->  deliver newest frame when due
//!   (pipe stays open: its closure is the liveness signal for both sides)
//!   <- Request { format }            <-----  whenever the consumers' selected
//!                                            mode differs from the producer's
//! ```
//!
//! The producer may write the native mode or any mode the source offers as
//! a conversion of it (`crate::convert::is_offered`); the source converts
//! when the selected mode differs and it can. A [`Request`] asks the
//! producer to reopen in the selected mode so it can produce that layout
//! itself (for example on the GPU) and the source only copies. Producers
//! that never read the pipe simply keep the native mode.

use crate::format::{FormatError, VideoFormat, WireVideoFormat};
use crate::id::CameraId;

/// Fixed COM CLSID of the Rust Media Foundation source (`virtualcam_mf.dll`).
pub const SOURCE_CLSID: CameraId = CameraId::from_u128(0x872FEE29_230E_41D8_83A7_2A383D956486);

/// String: canonical [`CameraId`] of the logical camera.
pub const ATTR_CAMERA_ID: CameraId = CameraId::from_u128(0xCBD5F0B4_50AA_4B3B_A032_F05C2EFC4DA1);
/// String: 32 hex digits naming the rendezvous pipe (routing, not a secret).
pub const ATTR_RENDEZVOUS_ID: CameraId = CameraId::from_u128(0x76D66E69_4E68_456B_83DC_69F0CE8BDC23);
/// String: SID of the camera owner; an assertion compared against the
/// producer's actual token, never an authorization input on its own.
pub const ATTR_EXPECTED_OWNER_SID: CameraId = CameraId::from_u128(0x56CCA6A6_EAAA_4B9E_BE29_CBE2745B10D2);
/// UINT32: [`PROTOCOL_VERSION`].
pub const ATTR_PROTOCOL_VERSION: CameraId = CameraId::from_u128(0x0EB9BE63_C946_43DE_A8D8_6A541245BA50);
/// Blob: the immutable native mode as a 32-byte [`WireVideoFormat`].
pub const ATTR_NATIVE_FORMAT: CameraId = CameraId::from_u128(0xD51FA021_9DC8_4234_8D4B_F94FF8DEA092);

/// Version of the persisted attribute set; independent of the rendezvous wire version.
pub const PROTOCOL_VERSION: u32 = 1;
/// Rendezvous wire version. Version 2 requires the source-created frame event.
pub const RENDEZVOUS_VERSION: u32 = 2;
/// Upper bound of string attributes the source accepts (UTF-16 units).
pub const MAX_STRING_ATTRIBUTE: usize = 256;

/// Validates a rendezvous id (exactly 32 uppercase hex digits).
pub fn is_valid_rendezvous_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

/// Pipe path of a rendezvous id, NUL-terminated UTF-16.
pub fn pipe_name(rendezvous_id: &str) -> Vec<u16> {
    format!("\\\\.\\pipe\\VirtualCamRs-MF-{rendezvous_id}").encode_utf16().chain(core::iter::once(0)).collect()
}

/// Ring mapping name for a source-generated nonce. `global` selects the
/// cross-session namespace used inside Frame Server.
pub fn ring_name(global: bool, nonce: u128) -> String {
    format!("{}\\VirtualCamRs-MF-{nonce:032X}", if global { "Global" } else { "Local" })
}

/// Frame event paired with a ring under the same source-generated nonce.
pub fn frame_event_name(global: bool, nonce: u128) -> String {
    format!("{}-Frame", ring_name(global, nonce))
}

/// Handshake failure on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageError {
    /// Wrong length, magic, or version.
    Malformed,
    /// The embedded format is invalid.
    Format(FormatError),
}

/// `Hello::magic`: `"VCMH"`.
pub const HELLO_MAGIC: u32 = u32::from_le_bytes(*b"VCMH");
/// `Welcome::magic`: `"VCMW"`.
pub const WELCOME_MAGIC: u32 = u32::from_le_bytes(*b"VCMW");

/// Producer -> source request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// Camera the producer intends to feed.
    pub camera_id: CameraId,
    /// Format the producer will write: the native mode or one of the
    /// conversions the source offers.
    pub format: VideoFormat,
}

impl Hello {
    /// Exact encoded size.
    pub const SIZE: usize = 64;

    /// Encodes the message.
    pub fn encode(&self) -> Result<[u8; Self::SIZE], MessageError> {
        let mut out = [0_u8; Self::SIZE];
        out[0..4].copy_from_slice(&HELLO_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&RENDEZVOUS_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&(Self::SIZE as u32).to_le_bytes());
        out[16..32].copy_from_slice(&self.camera_id.to_u128().to_be_bytes());
        out[32..64].copy_from_slice(&self.format.to_wire().map_err(MessageError::Format)?.to_bytes());
        Ok(out)
    }

    /// Decodes and validates the message.
    pub fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
        if bytes.len() != Self::SIZE || u32_at(bytes, 0) != HELLO_MAGIC || u32_at(bytes, 4) != RENDEZVOUS_VERSION || u32_at(bytes, 8) != Self::SIZE as u32 || u32_at(bytes, 12) != 0 {
            return Err(MessageError::Malformed);
        }
        let mut id = [0_u8; 16];
        id.copy_from_slice(&bytes[16..32]);
        let format = WireVideoFormat::from_bytes(&bytes[32..64]).and_then(WireVideoFormat::decode).map_err(MessageError::Format)?;
        Ok(Self {
            camera_id: CameraId::from_u128(u128::from_be_bytes(id)),
            format,
        })
    }
}

/// Outcome codes carried in [`Welcome::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WelcomeStatus {
    /// Attached; the ring name is valid.
    Ok = 0,
    /// Unsupported protocol version.
    Version = 1,
    /// The Hello named a different camera.
    CameraMismatch = 2,
    /// The producer's format is not one the source offers.
    FormatMismatch = 3,
    /// Producer identity does not match the camera owner.
    Unauthorized = 4,
    /// Another producer is attached.
    Busy = 5,
    /// The source could not create the transport.
    Internal = 6,
}

impl WelcomeStatus {
    fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Ok,
            1 => Self::Version,
            2 => Self::CameraMismatch,
            3 => Self::FormatMismatch,
            4 => Self::Unauthorized,
            5 => Self::Busy,
            6 => Self::Internal,
            _ => return None,
        })
    }
}

/// Capacity of [`Welcome::ring_name`] in UTF-16 units including the NUL.
pub const RING_NAME_CAPACITY: usize = 96;

/// Source -> producer response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    /// Attach outcome.
    pub status: WelcomeStatus,
    /// Ring slot count.
    pub slot_count: u32,
    /// Ring mapping size in bytes.
    pub ring_size: u64,
    /// Transport generation.
    pub generation: u64,
    /// Ring mapping name (empty unless `status == Ok`).
    pub ring_name: String,
    /// Auto-reset frame event name (empty unless `status == Ok`).
    pub frame_event_name: String,
}

impl Welcome {
    /// Exact encoded size.
    pub const SIZE: usize = 40 + RING_NAME_CAPACITY * 4;

    /// A response carrying only a failure status.
    pub fn failure(status: WelcomeStatus) -> Self {
        Self {
            status,
            slot_count: 0,
            ring_size: 0,
            generation: 0,
            ring_name: String::new(),
            frame_event_name: String::new(),
        }
    }

    /// Encodes the message.
    pub fn encode(&self) -> Result<[u8; Self::SIZE], MessageError> {
        let mut out = [0_u8; Self::SIZE];
        out[0..4].copy_from_slice(&WELCOME_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&RENDEZVOUS_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&(Self::SIZE as u32).to_le_bytes());
        out[12..16].copy_from_slice(&(self.status as u32).to_le_bytes());
        out[16..20].copy_from_slice(&self.slot_count.to_le_bytes());
        out[24..32].copy_from_slice(&self.ring_size.to_le_bytes());
        out[32..40].copy_from_slice(&self.generation.to_le_bytes());
        for (index, name) in [&self.ring_name, &self.frame_event_name].into_iter().enumerate() {
            let name: Vec<u16> = name.encode_utf16().collect();
            if name.len() >= RING_NAME_CAPACITY || name.contains(&0) || (self.status == WelcomeStatus::Ok && name.is_empty()) {
                return Err(MessageError::Malformed);
            }
            let start = 40 + index * RING_NAME_CAPACITY * 2;
            for (offset, unit) in name.iter().enumerate() {
                out[start + offset * 2..start + offset * 2 + 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        Ok(out)
    }

    /// Decodes and validates the message.
    pub fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
        if bytes.len() != Self::SIZE || u32_at(bytes, 0) != WELCOME_MAGIC || u32_at(bytes, 4) != RENDEZVOUS_VERSION || u32_at(bytes, 8) != Self::SIZE as u32 || u32_at(bytes, 20) != 0 {
            return Err(MessageError::Malformed);
        }
        let status = WelcomeStatus::from_code(u32_at(bytes, 12)).ok_or(MessageError::Malformed)?;
        let name = |bytes: &[u8]| {
            let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
            let end = units.iter().position(|unit| *unit == 0).ok_or(MessageError::Malformed)?;
            if status == WelcomeStatus::Ok && end == 0 {
                return Err(MessageError::Malformed);
            }
            String::from_utf16(&units[..end]).map_err(|_| MessageError::Malformed)
        };
        Ok(Self {
            status,
            slot_count: u32_at(bytes, 16),
            ring_size: u64_at(bytes, 24),
            generation: u64_at(bytes, 32),
            ring_name: name(&bytes[40..40 + RING_NAME_CAPACITY * 2])?,
            frame_event_name: name(&bytes[40 + RING_NAME_CAPACITY * 2..])?,
        })
    }
}

/// `Request::magic`: `"VCMR"`.
pub const REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"VCMR");

/// Source -> producer notice, sent after [`Welcome`] while attached: the
/// mode consumers selected, when it differs from the producer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// The selected mode.
    pub format: VideoFormat,
}

impl Request {
    /// Exact encoded size.
    pub const SIZE: usize = 48;

    /// Encodes the message.
    pub fn encode(&self) -> Result<[u8; Self::SIZE], MessageError> {
        let mut out = [0_u8; Self::SIZE];
        out[0..4].copy_from_slice(&REQUEST_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&RENDEZVOUS_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&(Self::SIZE as u32).to_le_bytes());
        out[16..48].copy_from_slice(&self.format.to_wire().map_err(MessageError::Format)?.to_bytes());
        Ok(out)
    }

    /// Decodes and validates the message.
    pub fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
        if bytes.len() != Self::SIZE || u32_at(bytes, 0) != REQUEST_MAGIC || u32_at(bytes, 4) != RENDEZVOUS_VERSION || u32_at(bytes, 8) != Self::SIZE as u32 || u32_at(bytes, 12) != 0 {
            return Err(MessageError::Malformed);
        }
        let format = WireVideoFormat::from_bytes(&bytes[16..48]).and_then(WireVideoFormat::decode).map_err(MessageError::Format)?;
        Ok(Self { format })
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut out = [0_u8; 8];
    out.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{FrameRate, PixelFormat};

    #[test]
    fn hello_round_trips_and_rejects_tampering() {
        let hello = Hello {
            camera_id: "{12345678-9ABC-DEF0-1122-334455667788}".parse().expect("id"),
            format: VideoFormat::new(PixelFormat::Nv12, 1920, 1080, FrameRate::fps(30)),
        };
        let bytes = hello.encode().expect("encode");
        assert_eq!(Hello::decode(&bytes), Ok(hello));
        assert_eq!(Hello::decode(&bytes[..63]), Err(MessageError::Malformed));
        for offset in [0, 4, 8, 12] {
            let mut corrupt = bytes;
            corrupt[offset] ^= 1;
            assert_eq!(Hello::decode(&corrupt), Err(MessageError::Malformed), "offset {offset}");
        }
        let mut corrupt = bytes;
        corrupt[32] = 99;
        assert!(matches!(Hello::decode(&corrupt), Err(MessageError::Format(_))));
    }

    #[test]
    fn welcome_round_trips_and_bounds_ring_name() {
        let welcome = Welcome {
            status: WelcomeStatus::Ok,
            slot_count: 3,
            ring_size: 1 << 20,
            generation: 42,
            ring_name: ring_name(true, 0xABC),
            frame_event_name: frame_event_name(true, 0xABC),
        };
        let bytes = welcome.encode().expect("encode");
        assert_eq!(Welcome::decode(&bytes), Ok(welcome.clone()));
        assert_eq!(Welcome::decode(&Welcome::failure(WelcomeStatus::Busy).encode().expect("encode")).map(|w| w.status), Ok(WelcomeStatus::Busy));
        let long = Welcome {
            ring_name: "x".repeat(RING_NAME_CAPACITY),
            ..Welcome::failure(WelcomeStatus::Ok)
        };
        assert_eq!(long.encode(), Err(MessageError::Malformed));
        let mut unterminated = bytes;
        for byte in &mut unterminated[40..] {
            *byte = b'A';
        }
        assert_eq!(Welcome::decode(&unterminated), Err(MessageError::Malformed));
        let long_event = Welcome {
            frame_event_name: "x".repeat(RING_NAME_CAPACITY),
            ..welcome.clone()
        };
        assert_eq!(long_event.encode(), Err(MessageError::Malformed));
        let mut missing_event = bytes;
        missing_event[40 + RING_NAME_CAPACITY * 2..].fill(0);
        assert_eq!(Welcome::decode(&missing_event), Err(MessageError::Malformed));
        let mut old_version = bytes;
        old_version[4..8].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(Welcome::decode(&old_version), Err(MessageError::Malformed));
    }

    #[test]
    fn request_round_trips_and_rejects_tampering() {
        let request = Request {
            format: VideoFormat::new(PixelFormat::Yuy2, 1080, 1920, FrameRate::fps(60)),
        };
        let bytes = request.encode().expect("encode");
        assert_eq!(Request::decode(&bytes), Ok(request));
        assert_eq!(Request::decode(&bytes[..47]), Err(MessageError::Malformed));
        for offset in [0, 4, 8, 12] {
            let mut corrupt = bytes;
            corrupt[offset] ^= 1;
            assert_eq!(Request::decode(&corrupt), Err(MessageError::Malformed), "offset {offset}");
        }
        let hello = Hello {
            camera_id: CameraId::from_u128(1),
            format: request.format,
        };
        assert_eq!(Request::decode(&hello.encode().expect("hello")[..48]), Err(MessageError::Malformed), "other magic");
    }

    #[test]
    fn names_and_ids_are_canonical() {
        assert!(is_valid_rendezvous_id("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_valid_rendezvous_id("0123456789abcdef0123456789ABCDEF"));
        assert!(!is_valid_rendezvous_id("0123"));
        let pipe = String::from_utf16(&pipe_name("AB")[..]).expect("utf16");
        assert_eq!(pipe, "\\\\.\\pipe\\VirtualCamRs-MF-AB\0");
        assert_eq!(ring_name(false, 1), "Local\\VirtualCamRs-MF-00000000000000000000000000000001");
        assert_eq!(frame_event_name(true, 1), "Global\\VirtualCamRs-MF-00000000000000000000000000000001-Frame");
        assert_eq!(SOURCE_CLSID.to_string(), "{872FEE29-230E-41D8-83A7-2A383D956486}");
    }
}
