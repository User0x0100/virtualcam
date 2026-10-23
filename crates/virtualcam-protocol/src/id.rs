//! Backend-neutral camera identity.

use core::fmt;
use core::str::FromStr;

/// Stable identifier of one managed virtual camera.
///
/// It is a GUID with Windows `GUID` memory layout. For DirectShow cameras it
/// is also the filter's COM CLSID; for Media Foundation cameras it is the
/// routing identity passed to the shared source through vendor attributes.
/// [`Display`](fmt::Display) yields the canonical
/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` form accepted by [`FromStr`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CameraId {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// A camera id string was not a canonical GUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseIdError;

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid camera GUID")
    }
}

impl CameraId {
    /// The all-zero GUID; never a valid camera.
    pub const NIL: Self = Self::from_u128(0);

    /// Builds an id from its big-endian textual value, e.g.
    /// `0x12345678_9ABC_DEF0_1122_334455667788`.
    pub const fn from_u128(value: u128) -> Self {
        let bytes = value.to_be_bytes();
        Self {
            data1: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            data2: u16::from_be_bytes([bytes[4], bytes[5]]),
            data3: u16::from_be_bytes([bytes[6], bytes[7]]),
            data4: [bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]],
        }
    }

    /// Returns the big-endian textual value; inverse of [`Self::from_u128`].
    pub const fn to_u128(self) -> u128 {
        let a = self.data1.to_be_bytes();
        let b = self.data2.to_be_bytes();
        let c = self.data3.to_be_bytes();
        let d = self.data4;
        u128::from_be_bytes([a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
    }

    /// Builds a random-version (v4) id from 16 random bytes.
    pub const fn from_random_bytes(mut bytes: [u8; 16]) -> Self {
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        Self::from_u128(u128::from_be_bytes(bytes))
    }

    /// Reports whether this is [`Self::NIL`].
    pub const fn is_nil(self) -> bool {
        self.to_u128() == 0
    }
}

impl fmt::Display for CameraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1, self.data2, self.data3, self.data4[0], self.data4[1], self.data4[2], self.data4[3], self.data4[4], self.data4[5], self.data4[6], self.data4[7]
        )
    }
}

impl FromStr for CameraId {
    type Err = ParseIdError;

    /// Accepts the canonical form with or without braces, case-insensitively.
    fn from_str(value: &str) -> Result<Self, ParseIdError> {
        let value = value.strip_prefix('{').and_then(|v| v.strip_suffix('}')).unwrap_or(value);
        let bytes = value.as_bytes();
        if bytes.len() != 36 {
            return Err(ParseIdError);
        }
        let mut out = 0_u128;
        let mut digits = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) {
                if byte != b'-' {
                    return Err(ParseIdError);
                }
                continue;
            }
            let nibble = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(ParseIdError),
            };
            out = (out << 4) | u128::from(nibble);
            digits += 1;
        }
        debug_assert_eq!(digits, 32);
        Ok(Self::from_u128(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_canonical_text() {
        let text = "{12345678-9ABC-DEF0-1122-334455667788}";
        let id: CameraId = text.parse().expect("valid id");
        assert_eq!(id.to_string(), text);
        assert_eq!(id.to_u128(), 0x12345678_9ABC_DEF0_1122_334455667788);
        assert_eq!(id, "12345678-9abc-def0-1122-334455667788".parse().expect("unbraced lowercase"));
    }

    #[test]
    fn rejects_malformed_text() {
        for value in [
            "",
            "1234",
            "{12345678-9ABC-DEF0-1122-33445566778Z}",
            "12345678-9ABC-DEF0-1122",
            "12345678-9ABC-DEF0-éé-334455667788",
            "123456789-ABC-DEF0-1122-334455667788",
        ] {
            assert_eq!(value.parse::<CameraId>(), Err(ParseIdError), "{value}");
        }
    }

    #[test]
    fn random_ids_are_version_4() {
        let id = CameraId::from_random_bytes([0xFF; 16]);
        let text = id.to_string();
        assert_eq!(&text[15..16], "4");
        assert!(matches!(&text[20..21], "8" | "9" | "A" | "B"));
    }

    #[test]
    fn memory_layout_matches_windows_guid() {
        assert_eq!(core::mem::size_of::<CameraId>(), 16);
        assert_eq!(core::mem::align_of::<CameraId>(), 4);
    }
}
