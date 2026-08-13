//! iTerm2 Inline Image Protocol (OSC 1337) encoding for server-side
//! Kitty→IIP graphics transcode.
//!
//! Builds complete `ESC ] 1337 ; File=... : <base64> ESC \` sequences for
//! attached clients whose outer terminal renders IIP but not Kitty graphics
//! (e.g. xterm.js with the image addon behind ttyd). PNG uploads pass
//! through untouched (base64 only, no re-encode); RGBA buffers are
//! PNG-encoded first, nearest-neighbor downscaled beforehand when they
//! exceed the client's pixel budget.
//!
//! The bundled IIPHandler aborts on a missing/zero/oversized `size` key, so
//! `size=` always carries the honest decoded byte count and payloads whose
//! decoded size exceeds [`IIP_SIZE_LIMIT`] are refused here instead.

use std::io::Write as _;

use base64::Engine;

/// Decoded-byte ceiling enforced by the client (xterm.js image addon
/// `iipSizeLimit` default). Payloads larger than this are silently aborted
/// by the handler, so they are rejected here instead.
const IIP_SIZE_LIMIT: usize = 20_000_000;

/// Pixel-count ceiling for RGBA sources; the client drops images at or
/// above its `pixelLimit` (16,777,216), so larger inputs are downscaled to
/// fit under this slightly conservative bound first.
const IIP_PIXEL_LIMIT: u64 = 16_000_000;

/// Image payload for one IIP emission.
pub enum IipSource<'a> {
    /// Raw PNG file bytes (kitty f=100) — base64 passthrough, NO re-encode.
    Png(&'a [u8]),
    /// RGBA8 pixels (post prepare_image/crop) — PNG-encode then base64.
    Rgba { data: &'a [u8], width: u32, height: u32 },
}

/// Encodes `source` as a full OSC 1337 sequence:
/// `ESC ] 1337 ; File=inline=1;size=<decoded_len>;width=<cols>;height=<rows>;preserveAspectRatio=0 : <base64> ESC \`
///
/// `cols`/`rows` are CELL counts (bare ints = cells for the xterm.js
/// IIPHandler). Returns `None` on degenerate input (zero dims, zero
/// cols/rows, empty data) or when the decoded payload would exceed
/// [`IIP_SIZE_LIMIT`]. Rgba wider than [`IIP_PIXEL_LIMIT`] pixels is
/// nearest-neighbor downscaled to fit, preserving aspect ratio.
pub fn encode_iip(source: IipSource<'_>, cols: u32, rows: u32) -> Option<Vec<u8>> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let png_bytes: Vec<u8>;
    let decoded: &[u8] = match source {
        IipSource::Png(bytes) => {
            if bytes.is_empty() {
                return None;
            }
            bytes
        }
        IipSource::Rgba { data, width, height } => {
            png_bytes = encode_rgba_png(data, width, height)?;
            &png_bytes
        }
    };
    if decoded.len() > IIP_SIZE_LIMIT {
        return None;
    }

    let payload = base64::engine::general_purpose::STANDARD.encode(decoded);
    let mut out = Vec::with_capacity(payload.len() + 96);
    let _ = write!(
        out,
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=0:{payload}\x1b\\",
        decoded.len(),
    );
    Some(out)
}

/// PNG-encodes an RGBA buffer, downscaling to fit [`IIP_PIXEL_LIMIT`]
/// first. Returns `None` on degenerate input (zero dimension or short
/// buffer).
fn encode_rgba_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || rgba.is_empty() {
        return None;
    }
    let expected = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if rgba.len() < expected {
        return None;
    }

    let (target_w, target_h) = fit_to_pixel_limit(width, height);
    let scaled;
    let pixels: &[u8] = if width == target_w && height == target_h {
        &rgba[..expected]
    } else {
        scaled = scale_rgba_nearest(rgba, width, height, target_w, target_h);
        &scaled
    };

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, target_w, target_h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(pixels).ok()?;
    }
    Some(out)
}

/// Largest aspect-preserving size not exceeding [`IIP_PIXEL_LIMIT`] pixels;
/// inputs already under the limit pass through unchanged.
fn fit_to_pixel_limit(width: u32, height: u32) -> (u32, u32) {
    let pixels = u64::from(width) * u64::from(height);
    if pixels <= IIP_PIXEL_LIMIT {
        return (width, height);
    }
    let scale = (IIP_PIXEL_LIMIT as f64 / pixels as f64).sqrt();
    let target_w = ((f64::from(width) * scale).floor() as u32).max(1);
    let mut target_h = ((f64::from(height) * scale).floor() as u32).max(1);
    // The `.max(1)` clamp on a degenerate aspect (e.g. 1 x 20M) can push the
    // product back over the limit; recompute the free dimension from the
    // clamped one so the invariant holds for every input.
    if u64::from(target_w) * u64::from(target_h) > IIP_PIXEL_LIMIT {
        target_h = ((IIP_PIXEL_LIMIT / u64::from(target_w)) as u32).max(1);
    }
    (target_w, target_h)
}

fn scale_rgba_nearest(
    rgba: &[u8],
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (target_w as usize) * (target_h as usize) * 4];
    for ty in 0..target_h {
        let sy = ((u64::from(ty) * u64::from(height)) / u64::from(target_h)) as usize;
        for tx in 0..target_w {
            let sx = ((u64::from(tx) * u64::from(width)) / u64::from(target_w)) as usize;
            let src = (sy * (width as usize) + sx) * 4;
            let dst = ((ty as usize) * (target_w as usize) + (tx as usize)) * 4;
            out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Splits a full sequence into its `File=` header fields and the raw
    /// base64 payload, asserting the OSC 1337 framing on the way.
    fn parse_sequence(seq: &[u8]) -> (Vec<(String, String)>, Vec<u8>) {
        let text = std::str::from_utf8(seq).expect("sequence is ascii");
        let body = text
            .strip_prefix("\x1b]1337;File=")
            .expect("OSC 1337 File= prefix");
        let body = body.strip_suffix("\x1b\\").expect("ST terminator");
        let (header, payload) = body.split_once(':').expect("':' header terminator");
        let fields = header
            .split(';')
            .map(|kv| {
                let (k, v) = kv.split_once('=').expect("key=value field");
                (k.to_string(), v.to_string())
            })
            .collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("valid base64 payload");
        (fields, decoded)
    }

    fn field<'a>(fields: &'a [(String, String)], key: &str) -> &'a str {
        &fields
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("missing header field {key}"))
            .1
    }

    /// Decodes a PNG byte stream into (RGBA pixels, width, height).
    fn decode_png_rgba(bytes: &[u8]) -> (Vec<u8>, u32, u32) {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("valid PNG stream");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("PNG frame decodes");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        buf.truncate(info.buffer_size());
        (buf, info.width, info.height)
    }

    #[test]
    fn png_passthrough_preserves_bytes_and_declares_honest_header() {
        // Passthrough never inspects the bytes; arbitrary content stands in
        // for a real PNG upload.
        let input: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let seq = encode_iip(IipSource::Png(&input), 12, 7).expect("passthrough encodes");

        let (fields, decoded) = parse_sequence(&seq);
        assert_eq!(decoded, input, "base64 payload decodes byte-identical");
        assert_eq!(field(&fields, "inline"), "1");
        assert_eq!(field(&fields, "size"), input.len().to_string());
        assert_eq!(field(&fields, "width"), "12");
        assert_eq!(field(&fields, "height"), "7");
        assert_eq!(field(&fields, "preserveAspectRatio"), "0");
    }

    #[test]
    fn rgba_encodes_valid_png_with_matching_pixels() {
        // 2×1: red then blue.
        let rgba = [255u8, 0, 0, 255, 0, 0, 255, 255];
        let seq = encode_iip(
            IipSource::Rgba { data: &rgba, width: 2, height: 1 },
            4,
            2,
        )
        .expect("rgba encodes");

        let (fields, decoded) = parse_sequence(&seq);
        assert_eq!(field(&fields, "size"), decoded.len().to_string());
        let (pixels, w, h) = decode_png_rgba(&decoded);
        assert_eq!((w, h), (2, 1));
        assert_eq!(pixels, rgba);
    }

    #[test]
    fn oversized_rgba_is_downscaled_under_pixel_limit_preserving_aspect() {
        // 6000×3000 = 18 Mpx, above the 16 Mpx budget.
        let (width, height) = (6000u32, 3000u32);
        let rgba = vec![0x7fu8; (width as usize) * (height as usize) * 4];
        let seq = encode_iip(
            IipSource::Rgba { data: &rgba, width, height },
            80,
            24,
        )
        .expect("oversized rgba encodes after downscale");

        let (_, decoded) = parse_sequence(&seq);
        let (_, w, h) = decode_png_rgba(&decoded);
        assert!(w < width && h < height, "downscaled: {w}x{h}");
        assert!(
            u64::from(w) * u64::from(h) <= 16_000_000,
            "fits pixel budget: {w}x{h}"
        );
        // Aspect preserved: expected height from the scaled width within 1px.
        let expected_h = (u64::from(w) * u64::from(height)) / u64::from(width);
        assert!(
            u64::from(h).abs_diff(expected_h) <= 1,
            "aspect preserved: {w}x{h} vs expected height {expected_h}"
        );
    }

    #[test]
    fn oversized_decoded_payload_is_rejected() {
        let input = vec![0u8; IIP_SIZE_LIMIT + 1];
        assert!(encode_iip(IipSource::Png(&input), 10, 10).is_none());
        // At the boundary the payload still passes.
        let input = vec![0u8; IIP_SIZE_LIMIT];
        assert!(encode_iip(IipSource::Png(&input), 10, 10).is_some());
    }

    #[test]
    fn degenerate_input_yields_none() {
        assert!(encode_iip(IipSource::Png(&[]), 10, 10).is_none());
        assert!(encode_iip(IipSource::Png(b"png"), 0, 10).is_none());
        assert!(encode_iip(IipSource::Png(b"png"), 10, 0).is_none());
        assert!(
            encode_iip(IipSource::Rgba { data: &[], width: 0, height: 0 }, 10, 10).is_none()
        );
        assert!(
            encode_iip(IipSource::Rgba { data: &[], width: 1, height: 1 }, 10, 10).is_none()
        );
        // Short buffer for the declared dimensions.
        assert!(
            encode_iip(IipSource::Rgba { data: &[0, 0, 0], width: 1, height: 1 }, 10, 10)
                .is_none()
        );
    }

    #[test]
    fn sequence_terminates_with_st() {
        let seq = encode_iip(IipSource::Png(b"payload"), 1, 1).expect("encodes");
        assert_eq!(&seq[seq.len() - 2..], b"\x1b\\", "ESC backslash terminator");
    }
}
