//! Location and camera data removed from an image sent without re-encoding, the pixels left
//! untouched. Re-encoded outputs already drop them (`-map_metadata -1`); a photo sent as a file,
//! or too elongated to be a Telegram photo, used to keep its Exif: GPS position, camera and
//! lens serial numbers, dates, owner name.
//!
//! - JPEG: keeps the JFIF header, the ICC profile and the Adobe segment (needed to decode CMYK),
//!   drops every other application segment (Exif and XMP in APP1, the multi-picture index in
//!   APP2, IPTC/Photoshop in APP13, maker data) and the comments, and stops at the end of the
//!   image: the previews a camera appends after it carry their own Exif.
//! - PNG: keeps the chunks that say how to draw the image, drops `eXIf`, the text chunks (XMP
//!   lives in `iTXt`), `tIME` and unknown ancillary chunks, and anything after `IEND`.
//! - WebP: keeps the image and colour chunks, drops `EXIF`, `XMP ` and unknown chunks.
//!
//! Orientation: a camera records a portrait photo as landscape pixels plus an Exif orientation.
//! Without it the photo would show sideways, so when it is not "normal" a minimal Exif holding
//! that single tag is written back: no position, no camera, no date.
//!
//! HEIC/AVIF are not handled (their Exif is an item of an ISO-BMFF `meta` box, which would need
//! the file rewritten around it); `stage` sends them unchanged.

/// Why a file could not be read as the format its first bytes claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StripError {
    #[error("truncated {0} file")]
    Truncated(&'static str),
    #[error("malformed {0} file")]
    Malformed(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
/// Bytes `sniff` needs.
pub const SNIFF_LEN: usize = 12;
/// Exif tag 0x0112.
const ORIENTATION_TAG: u16 = 0x0112;
const EXIF_PREFIX: &[u8] = b"Exif\0\0";

/// The format from the first bytes (never the extension, which can lie).
pub fn sniff(head: &[u8]) -> Option<ImageFormat> {
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if head.starts_with(&PNG_SIGNATURE) {
        Some(ImageFormat::Png)
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

/// The image without its metadata. `Ok(None)`: not a format handled here.
pub fn strip(data: &[u8]) -> Result<Option<Vec<u8>>, StripError> {
    match sniff(data) {
        Some(ImageFormat::Jpeg) => strip_jpeg(data).map(Some),
        Some(ImageFormat::Png) => strip_png(data).map(Some),
        Some(ImageFormat::Webp) => strip_webp(data).map(Some),
        None => Ok(None),
    }
}

// ---------- Exif orientation ----------

/// Orientation (1 to 8) of a TIFF structure (the body of an Exif block), if it has one.
fn tiff_orientation(tiff: &[u8]) -> Option<u16> {
    let little = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let bytes: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let count = usize::from(u16_at(ifd)?);
    (0..count).find_map(|index| {
        let entry = ifd + 2 + index * 12;
        if u16_at(entry)? != ORIENTATION_TAG {
            return None;
        }
        // One value, in the value field itself: a SHORT as the standard says, or a LONG as
        // some tools write it.
        let value = match u16_at(entry + 2)? {
            3 => u16_at(entry + 8)?,
            4 => u16::try_from(u32_at(entry + 8)?).ok()?,
            _ => return None,
        };
        (1..=8).contains(&value).then_some(value)
    })
}

/// A TIFF structure holding only the orientation (little endian, one IFD, one entry).
fn orientation_tiff(orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::with_capacity(26);
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&ORIENTATION_TAG.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&u32::from(orientation).to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff
}

/// The orientation worth keeping: anything but 1 (the default).
fn kept_orientation(orientation: Option<u16>) -> Option<u16> {
    orientation.filter(|value| *value != 1)
}

// ---------- JPEG ----------

const JPEG: &str = "JPEG";
const SOS: u8 = 0xda;
const EOI: u8 = 0xd9;
const APP0: u8 = 0xe0;
const APP1: u8 = 0xe1;
const APP2: u8 = 0xe2;
const APP14: u8 = 0xee;
const COM: u8 = 0xfe;

/// Whether a segment is kept: everything that decodes the image, plus the JFIF header, the ICC
/// profile and the Adobe colour transform.
fn keep_jpeg_segment(marker: u8, payload: &[u8]) -> bool {
    match marker {
        APP0 => payload.starts_with(b"JFIF\0"),
        APP2 => payload.starts_with(b"ICC_PROFILE\0"),
        APP14 => true,
        0xe1..=0xef | COM => false,
        _ => true,
    }
}

/// End of the entropy-coded data that starts at `from`: the next marker that is not a stuffed
/// `FF 00`, a restart marker or a fill byte.
fn scan_end(data: &[u8], from: usize) -> Result<usize, StripError> {
    let mut at = from;
    while at + 1 < data.len() {
        if data[at] == 0xff {
            match data[at + 1] {
                0x00 | 0xd0..=0xd7 => at += 2,
                0xff => at += 1,
                _ => return Ok(at),
            }
        } else {
            at += 1;
        }
    }
    Err(StripError::Truncated(JPEG))
}

fn strip_jpeg(data: &[u8]) -> Result<Vec<u8>, StripError> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return Err(StripError::Malformed(JPEG));
    }
    let mut kept = Vec::with_capacity(data.len());
    // End of a leading JFIF header in `kept`: the minimal Exif goes after it.
    let mut jfif_end = 0;
    let mut orientation = None;
    let mut at = 2;
    loop {
        if data.get(at) != Some(&0xff) {
            return Err(if at >= data.len() {
                StripError::Truncated(JPEG)
            } else {
                StripError::Malformed(JPEG)
            });
        }
        while data.get(at) == Some(&0xff) {
            at += 1;
        }
        let marker = *data.get(at).ok_or(StripError::Truncated(JPEG))?;
        at += 1;
        match marker {
            EOI => {
                kept.extend_from_slice(&[0xff, EOI]);
                break;
            }
            0xd8 | 0x00 => return Err(StripError::Malformed(JPEG)),
            // Standalone markers (TEM, RSTn) have no length.
            0x01 | 0xd0..=0xd7 => kept.extend_from_slice(&[0xff, marker]),
            _ => {
                let length = data
                    .get(at..at + 2)
                    .map(|bytes| usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
                    .ok_or(StripError::Truncated(JPEG))?;
                if length < 2 {
                    return Err(StripError::Malformed(JPEG));
                }
                let end = at + length;
                let payload = data.get(at + 2..end).ok_or(StripError::Truncated(JPEG))?;
                if marker == APP1 && orientation.is_none() {
                    orientation = payload.strip_prefix(EXIF_PREFIX).and_then(tiff_orientation);
                }
                if keep_jpeg_segment(marker, payload) {
                    kept.extend_from_slice(&[0xff, marker]);
                    kept.extend_from_slice(&data[at..end]);
                    if marker == APP0 && jfif_end == 0 && kept.len() == length + 2 {
                        jfif_end = kept.len();
                    }
                }
                at = end;
                if marker == SOS {
                    let scan = scan_end(data, at)?;
                    kept.extend_from_slice(&data[at..scan]);
                    at = scan;
                }
            }
        }
    }
    let mut out = Vec::with_capacity(kept.len() + 40);
    out.extend_from_slice(&[0xff, 0xd8]);
    out.extend_from_slice(&kept[..jfif_end]);
    if let Some(orientation) = kept_orientation(orientation) {
        let tiff = orientation_tiff(orientation);
        let length = u16::try_from(2 + EXIF_PREFIX.len() + tiff.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&[0xff, APP1]);
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(EXIF_PREFIX);
        out.extend_from_slice(&tiff);
    }
    out.extend_from_slice(&kept[jfif_end..]);
    Ok(out)
}

// ---------- PNG ----------

const PNG: &str = "PNG";

/// Ancillary chunks kept: how to draw the image (transparency, colour, gamma, HDR, pixel size,
/// background) and the APNG animation.
const PNG_KEPT: [&[u8; 4]; 16] = [
    b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB", b"cICP", b"mDCV", b"cLLI", b"bKGD",
    b"pHYs", b"hIST", b"sPLT", b"acTL", b"fcTL", b"fdAT",
];

/// CRC-32 (ISO-HDLC, the one PNG uses) of `bytes`.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in parts.iter().flat_map(|part| part.iter()) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub(super) fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

fn strip_png(data: &[u8]) -> Result<Vec<u8>, StripError> {
    let mut chunks: Vec<(&[u8; 4], &[u8])> = Vec::new();
    let mut orientation = None;
    let mut at = PNG_SIGNATURE.len();
    loop {
        let header = data.get(at..at + 8).ok_or(StripError::Truncated(PNG))?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let length = usize::try_from(length).map_err(|_| StripError::Malformed(PNG))?;
        let kind: &[u8; 4] = header[4..8]
            .try_into()
            .map_err(|_| StripError::Malformed(PNG))?;
        let body = data
            .get(at + 8..at + 8 + length)
            .ok_or(StripError::Truncated(PNG))?;
        data.get(at + 8 + length..at + 12 + length)
            .ok_or(StripError::Truncated(PNG))?;
        at += 12 + length;
        if kind == b"eXIf" && orientation.is_none() {
            orientation = tiff_orientation(body);
        }
        // A critical chunk (upper-case first letter) is always kept: a decoder needs it.
        if kind[0].is_ascii_uppercase() || PNG_KEPT.contains(&kind) {
            chunks.push((kind, body));
        }
        if kind == b"IEND" {
            break;
        }
    }
    if chunks.first().map(|(kind, _)| *kind) != Some(b"IHDR") {
        return Err(StripError::Malformed(PNG));
    }
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&PNG_SIGNATURE);
    for (index, (kind, body)) in chunks.iter().enumerate() {
        png_chunk(&mut out, kind, body);
        // `eXIf` must come before the image data: right after the header.
        if index == 0
            && let Some(orientation) = kept_orientation(orientation)
        {
            png_chunk(&mut out, b"eXIf", &orientation_tiff(orientation));
        }
    }
    Ok(out)
}

// ---------- WebP ----------

const WEBP: &str = "WebP";
const VP8X_EXIF: u8 = 0x08;
const VP8X_XMP: u8 = 0x04;
/// Chunks kept: the extended header, the colour profile, the animation and the image data.
const WEBP_KEPT: [&[u8; 4]; 7] = [
    b"VP8X", b"ICCP", b"ANIM", b"ANMF", b"ALPH", b"VP8 ", b"VP8L",
];

fn webp_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(kind);
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

fn strip_webp(data: &[u8]) -> Result<Vec<u8>, StripError> {
    let riff = data
        .get(4..8)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or(StripError::Truncated(WEBP))?;
    let end = usize::try_from(riff)
        .ok()
        .and_then(|riff| riff.checked_add(8))
        .ok_or(StripError::Malformed(WEBP))?;
    let body = data.get(..end).ok_or(StripError::Truncated(WEBP))?;
    let mut chunks: Vec<(&[u8; 4], Vec<u8>)> = Vec::new();
    let mut orientation = None;
    let mut at = 12;
    while at < body.len() {
        let header = body.get(at..at + 8).ok_or(StripError::Truncated(WEBP))?;
        let kind: &[u8; 4] = header[..4]
            .try_into()
            .map_err(|_| StripError::Malformed(WEBP))?;
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let length = usize::try_from(length).map_err(|_| StripError::Malformed(WEBP))?;
        let chunk = body
            .get(at + 8..at + 8 + length)
            .ok_or(StripError::Truncated(WEBP))?;
        at += 8 + length + length % 2;
        if kind == b"EXIF" && orientation.is_none() {
            // Written with or without the JPEG-style prefix, depending on the tool.
            orientation = tiff_orientation(chunk.strip_prefix(EXIF_PREFIX).unwrap_or(chunk));
        }
        if WEBP_KEPT.contains(&kind) {
            chunks.push((kind, chunk.to_vec()));
        }
    }
    let orientation = kept_orientation(orientation);
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(b"RIFF\0\0\0\0WEBP");
    for (kind, chunk) in &mut chunks {
        if *kind == b"VP8X"
            && let Some(flags) = chunk.first_mut()
        {
            *flags &= !(VP8X_EXIF | VP8X_XMP);
            if orientation.is_some() {
                *flags |= VP8X_EXIF;
            }
        }
        webp_chunk(&mut out, kind, chunk);
    }
    // A simple (non-VP8X) file has no metadata flag to set: its orientation, if any, cannot be
    // declared and is dropped with the rest.
    let extended = chunks.first().is_some_and(|(kind, _)| *kind == b"VP8X");
    if let Some(orientation) = orientation.filter(|_| extended) {
        webp_chunk(&mut out, b"EXIF", &orientation_tiff(orientation));
    }
    let riff = u32::try_from(out.len() - 8).map_err(|_| StripError::Malformed(WEBP))?;
    out[4..8].copy_from_slice(&riff.to_le_bytes());
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Text that only fake metadata holds: none of it may survive.
    pub(crate) const SECRETS: [&[u8]; 6] = [
        b"FAKEGPS48.8584N",
        b"FAKE-CAMERA-SN-0042",
        b"<x:xmpmeta",
        b"FAKE IPTC BYLINE",
        b"FAKE COMMENT",
        b"FAKE-PREVIEW-EXIF",
    ];

    pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// An Exif TIFF block (big endian, as many cameras write it) with an orientation, a GPS
    /// IFD and a serial number, all fake.
    pub(crate) fn camera_tiff(orientation: u16) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"MM\0\x2a\0\0\0\x08");
        // IFD0: orientation, GPS IFD pointer, body serial (ASCII at offset 50).
        tiff.extend_from_slice(&3u16.to_be_bytes());
        tiff.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1]);
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&[0x88, 0x25, 0, 4, 0, 0, 0, 1, 0, 0, 0, 70]);
        tiff.extend_from_slice(&[0xa4, 0x31, 0, 2, 0, 0, 0, 20, 0, 0, 0, 50]);
        tiff.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(tiff.len(), 50);
        tiff.extend_from_slice(b"FAKE-CAMERA-SN-0042\0");
        assert_eq!(tiff.len(), 70);
        // GPS IFD: one ASCII tag with a fake position.
        tiff.extend_from_slice(&1u16.to_be_bytes());
        tiff.extend_from_slice(&[0x00, 0x1b, 0, 2, 0, 0, 0, 16, 0, 0, 0, 88]);
        tiff.extend_from_slice(&[0, 0, 0, 0]);
        tiff.extend_from_slice(b"FAKEGPS48.8584N\0");
        tiff
    }

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0xff, marker];
        out.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A JPEG structure with every kind of metadata around image segments (not decodable:
    /// `integration_tests` checks real pixels).
    fn camera_jpeg(orientation: u16) -> Vec<u8> {
        let mut exif = EXIF_PREFIX.to_vec();
        exif.extend_from_slice(&camera_tiff(orientation));
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend(segment(APP0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0"));
        jpeg.extend(segment(APP1, &exif));
        jpeg.extend(segment(
            APP1,
            b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>FAKEGPS48.8584N</x:xmpmeta>",
        ));
        jpeg.extend(segment(APP2, b"ICC_PROFILE\0\x01\x01fake-profile"));
        jpeg.extend(segment(APP2, b"MPF\0FAKE-PREVIEW-EXIF index"));
        jpeg.extend(segment(0xed, b"Photoshop 3.0\0FAKE IPTC BYLINE"));
        jpeg.extend(segment(COM, b"FAKE COMMENT"));
        jpeg.extend(segment(0xdb, &[0; 65]));
        jpeg.extend(segment(0xc0, &[8, 0, 16, 0, 16, 1, 1, 0x11, 0]));
        jpeg.extend(segment(0xc4, &[0; 20]));
        jpeg.extend(segment(SOS, &[1, 1, 0, 0, 0x3f, 0]));
        // Entropy-coded data with a stuffed FF00, a restart marker and fill bytes.
        jpeg.extend_from_slice(&[0x12, 0xff, 0x00, 0x34, 0xff, 0xd0, 0x56, 0xff, 0xff]);
        jpeg.extend_from_slice(&[0xff, EOI]);
        // What a camera appends: a preview with its own Exif.
        jpeg.extend_from_slice(b"\xff\xd8\xff\xe1FAKE-PREVIEW-EXIF FAKEGPS48.8584N\xff\xd9");
        jpeg
    }

    fn no_secret(data: &[u8]) {
        for secret in SECRETS {
            assert!(
                !contains(data, secret),
                "{} left",
                String::from_utf8_lossy(secret)
            );
        }
    }

    #[test]
    fn formats_are_told_by_their_first_bytes() {
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some(ImageFormat::Jpeg));
        assert_eq!(sniff(&PNG_SIGNATURE), Some(ImageFormat::Png));
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEBPVP8 "), Some(ImageFormat::Webp));
        assert_eq!(sniff(b"RIFF\x10\0\0\0AVI LIST"), None);
        assert_eq!(sniff(b"\0\0\0\x18ftypheic"), None, "HEIC is not handled");
        assert_eq!(strip(b"plain text").unwrap(), None);
    }

    #[test]
    fn exif_orientation_is_read_in_both_byte_orders() {
        assert_eq!(tiff_orientation(&camera_tiff(6)), Some(6));
        // Written as a LONG (type 4), big and little endian.
        let mut long_be = camera_tiff(6);
        long_be[13] = 4;
        long_be[18..22].copy_from_slice(&6u32.to_be_bytes());
        assert_eq!(tiff_orientation(&long_be), Some(6));
        let mut long_le = orientation_tiff(5);
        long_le[12] = 4;
        assert_eq!(tiff_orientation(&long_le), Some(5));
        let mut rational = orientation_tiff(5);
        rational[12] = 5;
        assert_eq!(tiff_orientation(&rational), None, "not an integer type");
        assert_eq!(tiff_orientation(&orientation_tiff(8)), Some(8));
        assert_eq!(tiff_orientation(&camera_tiff(9)), None, "out of range");
        assert_eq!(
            tiff_orientation(b"II\x2a\0\xff\xff\0\0"),
            None,
            "bad offset"
        );
        assert_eq!(tiff_orientation(b"XX"), None);
    }

    #[test]
    fn jpeg_keeps_the_image_profile_and_orientation_only() {
        let source = camera_jpeg(6);
        let stripped = strip(&source).unwrap().unwrap();
        no_secret(&stripped);
        assert!(contains(&stripped, b"ICC_PROFILE\0\x01\x01fake-profile"));
        assert!(contains(&stripped, b"JFIF\0"));
        // Image segments and entropy data byte for byte, ending at the first EOI.
        let image_start = source.windows(2).position(|w| w == [0xff, 0xdb]).unwrap();
        let image_end = source.windows(2).position(|w| w == [0xff, EOI]).unwrap() + 2;
        assert!(stripped.ends_with(&source[image_start..image_end]));
        // SOI, JFIF, then the minimal Exif with the orientation.
        let jfif = segment(APP0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
        assert_eq!(&stripped[2..2 + jfif.len()], jfif.as_slice());
        let exif = &stripped[2 + jfif.len()..];
        assert_eq!(&exif[..2], &[0xff, APP1]);
        assert_eq!(tiff_orientation(&exif[4 + EXIF_PREFIX.len()..]), Some(6));
        assert_eq!(strip(&stripped).unwrap().unwrap(), stripped, "idempotent");

        // Upright: no Exif at all.
        let upright = strip(&camera_jpeg(1)).unwrap().unwrap();
        assert!(!contains(&upright, EXIF_PREFIX));
    }

    #[test]
    fn broken_jpegs_are_refused_never_passed_through() {
        let source = camera_jpeg(1);
        let cut = &source[..source.len() / 2];
        assert_eq!(strip(cut), Err(StripError::Truncated(JPEG)));
        let mut garbage = source[..20].to_vec();
        garbage[2] = 0x00;
        assert!(strip_jpeg(&garbage).is_err());
        assert_eq!(
            strip_jpeg(&[0xff, 0xd8, 0xff, 0xe1, 0x00, 0x01]),
            Err(StripError::Malformed(JPEG))
        );
    }

    fn png_with_metadata() -> Vec<u8> {
        let mut png = PNG_SIGNATURE.to_vec();
        png_chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
        png_chunk(&mut png, b"tEXt", b"Comment\0FAKE COMMENT");
        png_chunk(
            &mut png,
            b"iTXt",
            b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta/>",
        );
        png_chunk(&mut png, b"eXIf", &camera_tiff(3));
        png_chunk(&mut png, b"iCCP", b"profile\0\0fake");
        png_chunk(&mut png, b"tIME", &[7, 0xea, 1, 2, 3, 4, 5]);
        png_chunk(&mut png, b"caBX", b"FAKE-CAMERA-SN-0042");
        png_chunk(&mut png, b"IDAT", &[0x78, 0x9c, 0x63, 0, 0, 0, 1, 0, 1]);
        png_chunk(&mut png, b"IEND", &[]);
        png.extend_from_slice(b"FAKE IPTC BYLINE trailer");
        png
    }

    fn png_chunks(png: &[u8]) -> Vec<String> {
        let mut kinds = Vec::new();
        let mut at = 8;
        while at < png.len() {
            let length = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind = &png[at + 4..at + 8];
            let crc =
                u32::from_be_bytes(png[at + 8 + length..at + 12 + length].try_into().unwrap());
            assert_eq!(crc, crc32(&[kind, &png[at + 8..at + 8 + length]]), "CRC");
            kinds.push(String::from_utf8_lossy(kind).into_owned());
            at += 12 + length;
        }
        kinds
    }

    #[test]
    fn png_keeps_drawing_chunks_and_a_minimal_exif() {
        assert_eq!(crc32(&[b"123456789"]), 0xcbf4_3926, "standard check value");
        let stripped = strip(&png_with_metadata()).unwrap().unwrap();
        no_secret(&stripped);
        assert_eq!(
            png_chunks(&stripped),
            ["IHDR", "eXIf", "iCCP", "IDAT", "IEND"]
        );
        let at = 8 + 25 + 8;
        assert_eq!(tiff_orientation(&stripped[at..at + 26]), Some(3));
        assert_eq!(
            strip(&stripped[..stripped.len() - 6]),
            Err(StripError::Truncated(PNG))
        );
    }

    fn webp_with_metadata(extended: bool) -> Vec<u8> {
        let mut chunks = Vec::new();
        if extended {
            webp_chunk(&mut chunks, b"VP8X", &[0x2c, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            webp_chunk(&mut chunks, b"ICCP", b"fake-profile");
        }
        webp_chunk(&mut chunks, b"VP8L", &[0x2f, 0, 0, 0, 0x10, 0x07]);
        if extended {
            let mut exif = EXIF_PREFIX.to_vec();
            exif.extend_from_slice(&camera_tiff(8));
            webp_chunk(&mut chunks, b"EXIF", &exif);
            webp_chunk(
                &mut chunks,
                b"XMP ",
                b"<x:xmpmeta>FAKEGPS48.8584N</x:xmpmeta>",
            );
        }
        webp_chunk(&mut chunks, b"FAKE", b"FAKE COMMENT");
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&u32::try_from(chunks.len() + 4).unwrap().to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(&chunks);
        webp
    }

    #[test]
    fn webp_drops_exif_xmp_and_unknown_chunks() {
        let stripped = strip(&webp_with_metadata(true)).unwrap().unwrap();
        no_secret(&stripped);
        let riff = u32::from_le_bytes(stripped[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff + 8, stripped.len(), "RIFF size rewritten");
        // ICC flag kept, XMP flag cleared, EXIF flag set for the minimal orientation.
        assert_eq!(stripped[20], 0x20 | VP8X_EXIF);
        assert!(contains(&stripped, b"fake-profile"));
        let exif = stripped.windows(4).position(|w| w == b"EXIF").unwrap();
        assert_eq!(tiff_orientation(&stripped[exif + 8..exif + 34]), Some(8));

        let simple = strip(&webp_with_metadata(false)).unwrap().unwrap();
        assert!(!contains(&simple, b"FAKE"));
        assert!(contains(&simple, b"VP8L"));
    }
}
