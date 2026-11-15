//! Source configuration decoded from untrusted vendor attributes.

use virtualcam_protocol::mf::{ATTR_CAMERA_ID, ATTR_DEFAULT_FORMAT, ATTR_EXPECTED_OWNER_SID, ATTR_PIXEL_FORMATS, ATTR_PROTOCOL_VERSION, ATTR_RENDEZVOUS_ID, MAX_STRING_ATTRIBUTE, PROTOCOL_VERSION, is_valid_rendezvous_id};
use virtualcam_protocol::registry::{MAX_PIXEL_FORMATS, decode_pixel_formats};
use virtualcam_protocol::{CameraId, PixelFormat, ValidatedFormat, VideoFormat, WireVideoFormat};
use windows::Win32::Media::MediaFoundation::{IMFAttributes, MF_E_INVALIDREQUEST};
use windows_core::GUID;

use crate::security::Sid;

/// Everything the source needs; immutable for the source's lifetime.
#[derive(Clone, Debug)]
pub(crate) struct SourceConfig {
    pub(crate) camera_id: CameraId,
    /// Every mode the source offers, in stream-descriptor order (see
    /// [`descriptor_order`]). Non-empty; the default mode is one of them.
    pub(crate) offers: Vec<ValidatedFormat>,
    /// Index into `offers` of the mode consumers get when they select none.
    pub(crate) default: usize,
    /// Producer rendezvous; `None` (fail closed: black only) when the
    /// routing id or owner assertion is missing or malformed.
    pub(crate) rendezvous: Option<Rendezvous>,
}

#[derive(Clone, Debug)]
pub(crate) struct Rendezvous {
    pub(crate) id: String,
    pub(crate) expected_owner: Sid,
}

fn key(id: CameraId) -> GUID {
    GUID::from_u128(id.to_u128())
}

/// Reads a bounded string attribute.
fn string(attributes: &IMFAttributes, id: CameraId) -> Option<String> {
    // SAFETY: Valid attribute store; the buffer is sized from the bounded length.
    let length = unsafe { attributes.GetStringLength(&key(id)) }.ok()? as usize;
    if length == 0 || length > MAX_STRING_ATTRIBUTE {
        return None;
    }
    let mut buffer = vec![0_u16; length + 1];
    // SAFETY: `buffer` holds `length + 1` units including the terminator.
    unsafe { attributes.GetString(&key(id), &mut buffer, None) }.ok()?;
    String::from_utf16(&buffer[..length]).ok()
}

/// Reads a `WireVideoFormat` blob.
fn wire_format(attributes: &IMFAttributes, id: CameraId) -> Option<VideoFormat> {
    let mut blob = [0_u8; WireVideoFormat::SIZE];
    let mut size = 0_u32;
    // SAFETY: `blob` has the exact wire size; larger blobs fail the call.
    unsafe { attributes.GetBlob(&key(id), &mut blob, Some(&mut size)) }.ok()?;
    if size as usize != WireVideoFormat::SIZE {
        return None;
    }
    WireVideoFormat::from_bytes(&blob).and_then(WireVideoFormat::decode).ok()
}

/// Reads the declared pixel-format list.
fn pixel_formats(attributes: &IMFAttributes, id: CameraId) -> Option<Vec<PixelFormat>> {
    let size = unsafe { attributes.GetBlobSize(&key(id)) }.ok()? as usize;
    // Bounded before allocating: the manager never writes more than this.
    if size == 0 || size > MAX_PIXEL_FORMATS * 4 || !size.is_multiple_of(4) {
        return None;
    }
    let mut blob = vec![0_u8; size];
    let mut read = 0_u32;
    // SAFETY: `blob` holds `size` writable bytes, the attribute's exact length.
    unsafe { attributes.GetBlob(&key(id), &mut blob, Some(&mut read)) }.ok()?;
    let formats = decode_pixel_formats(&blob[..read as usize]);
    (!formats.is_empty()).then_some(formats)
}

/// Stream-descriptor order of the declared modes.
///
/// Frame Server's DirectShow bridge serves a DirectShow request from the first
/// listed type it can reach, converting itself when that type is not the
/// requested one. Its own 4:2:0 -> 4:2:2 conversion pads rows — a 1080-wide
/// YUY2 frame arrived sheared — so packed 4:2:2 modes are listed before
/// planar 4:2:0 ones, and every request then finds an exact type instead of a
/// conversion. Declaration order is preserved inside each group.
fn descriptor_order(formats: &[PixelFormat]) -> Vec<PixelFormat> {
    let rank = |format: PixelFormat| match format {
        PixelFormat::Yuy2 | PixelFormat::Uyvy => 0,
        PixelFormat::Nv12 | PixelFormat::I420 | PixelFormat::P010 => 1,
        _ => 2,
    };
    let mut ordered = formats.to_vec();
    // Stable: equal ranks stay in declaration order.
    ordered.sort_by_key(|format| rank(*format));
    ordered
}

impl SourceConfig {
    /// The declared mode that equals `format` exactly, if any.
    pub(crate) fn offer_of(&self, format: &VideoFormat) -> Option<ValidatedFormat> {
        self.offers.iter().copied().find(|offer| offer.format() == *format)
    }

    /// Validates the camera's attributes. A missing or malformed id, version,
    /// default format, or pixel-format list fails activation; a missing
    /// rendezvous only disables producer attach.
    pub(crate) fn from_attributes(attributes: &IMFAttributes) -> windows_core::Result<Self> {
        let invalid = || windows_core::Error::from(MF_E_INVALIDREQUEST);
        // SAFETY: Valid attribute store.
        if unsafe { attributes.GetUINT32(&key(ATTR_PROTOCOL_VERSION)) }.ok() != Some(PROTOCOL_VERSION) {
            return Err(invalid());
        }
        let camera_id: CameraId = string(attributes, ATTR_CAMERA_ID).and_then(|text| text.parse().ok()).filter(|id: &CameraId| !id.is_nil()).ok_or_else(invalid)?;
        let default = wire_format(attributes, ATTR_DEFAULT_FORMAT).and_then(|format| format.validate().ok()).ok_or_else(invalid)?;
        let declared = pixel_formats(attributes, ATTR_PIXEL_FORMATS).ok_or_else(invalid)?;
        // Only the YUV formats this source carries may be declared; RGB is
        // DirectShow-only, since the camera fixes one colorimetry and never
        // converts. Attributes are untrusted configuration, so fail closed.
        if declared.iter().any(|format| !PixelFormat::MEDIA_FOUNDATION.contains(format)) {
            return Err(invalid());
        }
        let base = default.format();
        let offers = descriptor_order(&declared)
            .into_iter()
            .map(|pixel_format| VideoFormat { pixel_format, ..base }.validate().ok().ok_or_else(invalid))
            .collect::<windows_core::Result<Vec<_>>>()?;
        let default = offers.iter().position(|offer| offer.format().pixel_format == base.pixel_format).ok_or_else(invalid)?;
        let rendezvous = match (string(attributes, ATTR_RENDEZVOUS_ID), string(attributes, ATTR_EXPECTED_OWNER_SID).and_then(|text| Sid::parse(&text))) {
            (Some(id), Some(expected_owner)) if is_valid_rendezvous_id(&id) => Some(Rendezvous { id, expected_owner }),
            _ => None,
        };
        Ok(Self { camera_id, offers, default, rendezvous })
    }
}

#[cfg(test)]
mod tests {
    use virtualcam_protocol::FrameRate;
    use windows::Win32::Media::MediaFoundation::MFCreateAttributes;
    use windows_core::HSTRING;

    use super::*;

    const CAMERA: &str = "{11111111-2222-4333-8444-555555555555}";

    struct Store(IMFAttributes);

    impl Store {
        fn new(default: &VideoFormat, declared: &[PixelFormat]) -> Self {
            let mut store = None;
            // SAFETY: Out pointer receives a new attribute store.
            unsafe { MFCreateAttributes(&mut store, 4) }.expect("attributes");
            let store = store.expect("attributes");
            let formats: Vec<u8> = declared.iter().filter_map(|format| format.wire_code()).flat_map(u32::to_le_bytes).collect();
            // SAFETY: Plain attribute writes.
            unsafe {
                store.SetUINT32(&key(ATTR_PROTOCOL_VERSION), PROTOCOL_VERSION).expect("version");
                store.SetString(&key(ATTR_CAMERA_ID), &HSTRING::from(CAMERA)).expect("id");
                store.SetBlob(&key(ATTR_DEFAULT_FORMAT), &default.to_wire().expect("wire").to_bytes()).expect("default");
                if !formats.is_empty() {
                    store.SetBlob(&key(ATTR_PIXEL_FORMATS), &formats).expect("formats");
                }
            }
            Self(store)
        }

        fn parse(&self) -> windows_core::Result<SourceConfig> {
            SourceConfig::from_attributes(&self.0)
        }
    }

    fn base(pixel_format: PixelFormat) -> VideoFormat {
        VideoFormat::new(pixel_format, 64, 48, FrameRate::fps(30))
    }

    const FIVE: [PixelFormat; 5] = [PixelFormat::Nv12, PixelFormat::I420, PixelFormat::Yuy2, PixelFormat::Uyvy, PixelFormat::P010];

    #[test]
    fn every_declared_mode_is_offered_with_packed_422_first() {
        let default = base(PixelFormat::Nv12);
        let config = Store::new(&default, &FIVE).parse().expect("config");
        let offered: Vec<PixelFormat> = config.offers.iter().map(|offer| offer.format().pixel_format).collect();
        assert_eq!(offered, [PixelFormat::Yuy2, PixelFormat::Uyvy, PixelFormat::Nv12, PixelFormat::I420, PixelFormat::P010]);
        assert_eq!(config.offers[config.default].format(), default, "the default mode is the declared one, not the first listed");
        assert_eq!(config.offers[config.default].interval_100ns(), 333_333);
        assert_eq!(config.offer_of(&base(PixelFormat::Yuy2)), Some(base(PixelFormat::Yuy2).validate().expect("valid")));
        assert_eq!(config.offer_of(&base(PixelFormat::Bgra32)), None, "undeclared modes are not offered");
        assert_eq!(config.offer_of(&VideoFormat::new(PixelFormat::Nv12, 32, 48, FrameRate::fps(30))), None, "another size is not offered");
        assert_eq!(config.offer_of(&VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(60))), None, "another rate is not offered");
    }

    #[test]
    fn declaration_order_breaks_ties_inside_a_group() {
        let config = Store::new(&base(PixelFormat::P010), &[PixelFormat::P010, PixelFormat::I420, PixelFormat::Uyvy, PixelFormat::Yuy2])
            .parse()
            .expect("config");
        let offered: Vec<PixelFormat> = config.offers.iter().map(|offer| offer.format().pixel_format).collect();
        assert_eq!(offered, [PixelFormat::Uyvy, PixelFormat::Yuy2, PixelFormat::P010, PixelFormat::I420]);
        assert_eq!(config.offers[config.default].format().pixel_format, PixelFormat::P010);
    }

    #[test]
    fn activation_fails_closed_on_missing_or_malformed_attributes() {
        let default = base(PixelFormat::Nv12);
        let config = Store::new(&default, &[PixelFormat::Nv12, PixelFormat::Yuy2]).parse().expect("config");
        assert_eq!(config.offers.len(), 2);

        // No default format, no pixel-format list, an empty list, a default
        // missing from the list, and a format invalid at this geometry.
        assert!(Store::new(&default, &[]).parse().is_err());
        let missing = Store::new(&default, &[PixelFormat::Yuy2]);
        assert!(missing.parse().is_err(), "the default mode must be declared");
        let store = Store::new(&default, &[PixelFormat::Nv12]);
        // SAFETY: Attribute write of a blob one byte too long.
        unsafe { store.0.SetBlob(&key(ATTR_PIXEL_FORMATS), &[0_u8; 33]) }.expect("set");
        assert!(store.parse().is_err(), "a truncated wire list is rejected");
        let oversized = Store::new(&default, &[PixelFormat::Nv12]);
        // SAFETY: Attribute write of a list longer than the schema allows.
        unsafe { oversized.0.SetBlob(&key(ATTR_PIXEL_FORMATS), &[1_u8; MAX_PIXEL_FORMATS * 4 + 4]) }.expect("set");
        assert!(oversized.parse().is_err(), "an oversized wire list is rejected");
        let odd = VideoFormat::new(PixelFormat::Nv12, 63, 48, FrameRate::fps(30));
        assert!(Store::new(&odd, &[PixelFormat::Nv12]).parse().is_err(), "an odd NV12 width is not a valid mode");
        // RGB is not a Media Foundation mode: the camera fixes one
        // colorimetry and never converts.
        let rgb = VideoFormat::new(PixelFormat::Bgra32, 64, 48, FrameRate::fps(30));
        assert!(Store::new(&rgb, &[PixelFormat::Bgra32]).parse().is_err(), "RGB cannot be declared");
        assert!(Store::new(&default, &[PixelFormat::Nv12, PixelFormat::Rgb24]).parse().is_err(), "RGB beside YUV cannot be declared");
        let store = Store::new(&default, &[PixelFormat::Nv12]);
        // SAFETY: Attribute write.
        unsafe { store.0.SetUINT32(&key(ATTR_PROTOCOL_VERSION), 1) }.expect("set");
        assert!(store.parse().is_err(), "another protocol version is rejected");
    }
}
