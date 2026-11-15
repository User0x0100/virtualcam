//! Registry schema shared by the manager and the sources.
//!
//! ```text
//! {HKCU|HKLM}\Software\VirtualCamRs\Cameras\{ID}
//!     Backend       REG_DWORD   BACKEND_*
//!     Name          REG_SZ      friendly name
//!     AppId         REG_SZ      optional owning-application tag
//!     Format        REG_BINARY  WireVideoFormat: DirectShow idle default /
//!                               Media Foundation default format
//!     PixelFormats  REG_BINARY  offered pixel formats, in order, as
//!                               little-endian u32 wire codes (Media
//!                               Foundation: the declared format set)
//!     FrameRates    REG_BINARY  DirectShow: offered frame rates, in order, as
//!                               little-endian u32 numerator/denominator pairs
//!     SourcePath    REG_SZ      DirectShow: registered source DLL
//!     RendezvousId  REG_SZ      Media Foundation: pipe routing id
//!     SymbolicLink  REG_SZ      Media Foundation: device symbolic link
//!
//! HKCU\Software\VirtualCamRs\History\{ID}
//!     LastFormat    REG_BINARY  DirectShow: last successfully published format
//! ```
//!
//! HKCU holds per-user cameras (DirectShow per-user scope and every Media
//! Foundation camera); HKLM holds machine-wide DirectShow cameras. The Media
//! Foundation source never reads these keys: it runs as Local Service and
//! receives its configuration through vendor attributes.

use crate::format::{FrameRate, PixelFormat};

/// Most pixel formats a `PixelFormats` value may list.
pub const MAX_PIXEL_FORMATS: usize = 16;

/// Serializes a `PixelFormats` value; unmanaged formats are skipped.
pub fn encode_pixel_formats(formats: &[PixelFormat]) -> Vec<u8> {
    formats.iter().filter_map(|format| format.wire_code()).take(MAX_PIXEL_FORMATS).flat_map(u32::to_le_bytes).collect()
}

/// Parses a `PixelFormats` value, dropping unknown codes and duplicates.
pub fn decode_pixel_formats(bytes: &[u8]) -> Vec<PixelFormat> {
    let mut formats = Vec::new();
    for chunk in bytes.chunks_exact(4).take(MAX_PIXEL_FORMATS) {
        if let Some(format) = PixelFormat::from_wire_code(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            && !formats.contains(&format)
        {
            formats.push(format);
        }
    }
    formats
}

/// Most frame rates a `FrameRates` value may list.
pub const MAX_FRAME_RATES: usize = 16;

/// Serializes a `FrameRates` value; rates without a frame interval are skipped.
pub fn encode_frame_rates(rates: &[FrameRate]) -> Vec<u8> {
    rates
        .iter()
        .filter(|rate| rate.interval_100ns().is_some_and(|interval| interval > 0))
        .take(MAX_FRAME_RATES)
        .flat_map(|rate| rate.numerator.to_le_bytes().into_iter().chain(rate.denominator.to_le_bytes()))
        .collect()
}

/// Parses a `FrameRates` value, dropping invalid rates and rates whose frame
/// interval repeats an earlier one.
pub fn decode_frame_rates(bytes: &[u8]) -> Vec<FrameRate> {
    let mut rates: Vec<FrameRate> = Vec::new();
    for chunk in bytes.chunks_exact(8).take(MAX_FRAME_RATES) {
        let rate = FrameRate {
            numerator: u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            denominator: u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
        };
        if let Some(interval) = rate.interval_100ns().filter(|interval| *interval > 0)
            && !rates.iter().any(|known| known.interval_100ns() == Some(interval))
        {
            rates.push(rate);
        }
    }
    rates
}

/// Camera records root, relative to HKCU or HKLM.
pub const CAMERAS_KEY: &str = "Software\\VirtualCamRs\\Cameras";
/// Per-user DirectShow format history root, relative to HKCU.
pub const HISTORY_KEY: &str = "Software\\VirtualCamRs\\History";

/// `Backend` value of a DirectShow camera.
pub const BACKEND_DIRECTSHOW: u32 = 1;
/// `Backend` value of a Media Foundation camera.
pub const BACKEND_MEDIA_FOUNDATION: u32 = 2;

/// Value names.
pub mod values {
    /// `REG_DWORD` backend code.
    pub const BACKEND: &str = "Backend";
    /// `REG_SZ` friendly name.
    pub const NAME: &str = "Name";
    /// `REG_SZ` application tag.
    pub const APP_ID: &str = "AppId";
    /// `REG_BINARY` wire format.
    pub const FORMAT: &str = "Format";
    /// `REG_BINARY` offered pixel formats.
    pub const PIXEL_FORMATS: &str = "PixelFormats";
    /// `REG_BINARY` DirectShow offered frame rates.
    pub const FRAME_RATES: &str = "FrameRates";
    /// `REG_SZ` DirectShow source path.
    pub const SOURCE_PATH: &str = "SourcePath";
    /// `REG_SZ` Media Foundation rendezvous id.
    pub const RENDEZVOUS_ID: &str = "RendezvousId";
    /// `REG_SZ` Media Foundation symbolic link.
    pub const SYMBOLIC_LINK: &str = "SymbolicLink";
    /// `REG_BINARY` last published DirectShow format.
    pub const LAST_FORMAT: &str = "LastFormat";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rates_round_trip_without_invalid_or_repeated_intervals() {
        let ntsc = FrameRate { numerator: 30_000, denominator: 1001 };
        let rates = [FrameRate::fps(30), FrameRate { numerator: 0, denominator: 1 }, ntsc, FrameRate { numerator: 60, denominator: 2 }, FrameRate::fps(60)];
        let bytes = encode_frame_rates(&rates);
        assert_eq!(bytes.len(), 4 * 8, "the zero rate is not encoded");
        assert_eq!(decode_frame_rates(&bytes), [FrameRate::fps(30), ntsc, FrameRate::fps(60)]);
        assert!(decode_frame_rates(&[1, 2, 3]).is_empty());
        let many: Vec<FrameRate> = (1..=40).map(FrameRate::fps).collect();
        assert_eq!(decode_frame_rates(&encode_frame_rates(&many)).len(), MAX_FRAME_RATES);
    }
}
