//! Sixel encoding for server-side Kitty→Sixel graphics transcode.
//!
//! Converts RGBA pixel buffers into standard Sixel DCS sequences for
//! attached clients whose outer terminal renders Sixel but not Kitty
//! graphics (e.g. xterm.js with the image addon behind ttyd). The encoder
//! is deliberately dependency-free: median-cut quantization to at most 256
//! palette entries, nearest-neighbor scaling, and run-length-encoded sixel
//! emission.
//!
//! Alpha is thresholded: pixels with `a < 128` are left unpainted (the DCS
//! header requests "keep existing content" for zero bits), everything else
//! is treated as opaque.

use std::collections::HashMap;

/// Maximum palette entries a Sixel stream can address per emission here.
const MAX_PALETTE: usize = 256;

/// Alpha threshold below which a pixel stays unpainted.
const OPAQUE_ALPHA: u8 = 128;

/// Hard bound on either target dimension; larger requests are clamped to
/// keep a single encode's cost bounded (the per-frame byte budget guards
/// the aggregate).
const MAX_TARGET_DIM: u32 = 4096;

/// Encodes an RGBA buffer as a complete Sixel DCS sequence, scaling to
/// `target_w`×`target_h` with nearest-neighbor sampling first.
///
/// Returns an empty vector when the input is degenerate (zero dimension or
/// short buffer). Fully transparent input produces a valid sequence that
/// paints nothing.
pub(crate) fn encode_sixel_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
) -> Vec<u8> {
    let target_w = target_w.min(MAX_TARGET_DIM);
    let target_h = target_h.min(MAX_TARGET_DIM);
    if width == 0 || height == 0 || target_w == 0 || target_h == 0 {
        return Vec::new();
    }
    let expected = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if rgba.len() < expected {
        return Vec::new();
    }

    let scaled;
    let pixels: &[u8] = if width == target_w && height == target_h {
        rgba
    } else {
        scaled = scale_rgba_nearest(rgba, width, height, target_w, target_h);
        &scaled
    };

    encode_exact(pixels, target_w, target_h)
}

/// Extracts a `w`×`h` RGBA sub-rectangle starting at (`x`, `y`).
///
/// The rectangle is clamped to the source bounds; a rectangle that starts
/// outside the source yields an empty vector.
pub(crate) fn crop_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    if x >= width || y >= height {
        return None;
    }
    let w = w.min(width - x);
    let h = h.min(height - y);
    if w == 0 || h == 0 {
        return None;
    }
    let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if rgba.len() < expected {
        return None;
    }
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for row in y..y + h {
        let start = ((row as usize) * (width as usize) + (x as usize)) * 4;
        out.extend_from_slice(&rgba[start..start + (w as usize) * 4]);
    }
    Some((out, w, h))
}

/// Expands a packed RGB buffer to RGBA with opaque alpha.
pub(crate) fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for px in rgb.chunks_exact(3) {
        out.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    out
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

fn pack_rgb(px: &[u8]) -> u32 {
    (u32::from(px[0]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[2])
}

/// Median-cut quantization over the opaque pixels.
///
/// Returns the palette (packed RGB) plus a map from every distinct opaque
/// input color to its palette index. Deterministic for a given input.
fn quantize(pixels: &[u8]) -> (Vec<u32>, HashMap<u32, u16>) {
    let mut counts: HashMap<u32, u32> = HashMap::new();
    for px in pixels.chunks_exact(4) {
        if px[3] >= OPAQUE_ALPHA {
            *counts.entry(pack_rgb(px)).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return (Vec::new(), HashMap::new());
    }

    let mut colors: Vec<(u32, u32)> = counts.into_iter().collect();
    colors.sort_unstable_by_key(|&(color, _)| color);

    if colors.len() <= MAX_PALETTE {
        let palette: Vec<u32> = colors.iter().map(|&(color, _)| color).collect();
        let map = palette
            .iter()
            .enumerate()
            .map(|(index, &color)| (color, index as u16))
            .collect();
        return (palette, map);
    }

    // Boxes hold index ranges into `colors`; split the box with the widest
    // channel spread at its weighted median until we reach MAX_PALETTE.
    let mut boxes: Vec<(usize, usize)> = vec![(0, colors.len())];
    while boxes.len() < MAX_PALETTE {
        let mut widest: Option<(usize, u32, u8)> = None; // (box, spread, channel)
        for (box_index, &(start, end)) in boxes.iter().enumerate() {
            if end - start < 2 {
                continue;
            }
            let slice = &colors[start..end];
            for channel in 0..3u8 {
                let shift = 16 - channel * 8;
                let mut min = u32::MAX;
                let mut max = 0u32;
                for &(color, _) in slice {
                    let value = (color >> shift) & 0xFF;
                    min = min.min(value);
                    max = max.max(value);
                }
                let spread = max.saturating_sub(min);
                if widest.map(|(_, s, _)| spread > s).unwrap_or(spread > 0) {
                    widest = Some((box_index, spread, channel));
                }
            }
        }
        let Some((box_index, _, channel)) = widest else {
            break;
        };
        let (start, end) = boxes[box_index];
        let shift = 16 - channel * 8;
        colors[start..end].sort_unstable_by_key(|&(color, _)| ((color >> shift) & 0xFF, color));

        let total: u64 = colors[start..end].iter().map(|&(_, n)| u64::from(n)).sum();
        let mut acc = 0u64;
        let mut split = start + 1;
        for (offset, &(_, n)) in colors[start..end].iter().enumerate() {
            acc += u64::from(n);
            if acc * 2 >= total {
                split = (start + offset + 1).min(end - 1).max(start + 1);
                break;
            }
        }
        boxes[box_index] = (start, split);
        boxes.push((split, end));
    }

    let mut palette = Vec::with_capacity(boxes.len());
    let mut map = HashMap::with_capacity(colors.len());
    boxes.sort_unstable();
    for (index, &(start, end)) in boxes.iter().enumerate() {
        let slice = &colors[start..end];
        let mut r = 0u64;
        let mut g = 0u64;
        let mut b = 0u64;
        let mut total = 0u64;
        for &(color, n) in slice {
            let n64 = u64::from(n);
            r += u64::from((color >> 16) & 0xFF) * n64;
            g += u64::from((color >> 8) & 0xFF) * n64;
            b += u64::from(color & 0xFF) * n64;
            total += n64;
        }
        let total = total.max(1);
        let avg = ((r / total) as u32) << 16 | ((g / total) as u32) << 8 | (b / total) as u32;
        palette.push(avg);
        for &(color, _) in slice {
            map.insert(color, index as u16);
        }
    }
    (palette, map)
}

fn sixel_component(value: u32) -> u32 {
    (value * 100 + 127) / 255
}

/// Encodes an exact-size RGBA buffer (no scaling) as a Sixel sequence.
fn encode_exact(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (palette, map) = quantize(pixels);

    let mut out = Vec::with_capacity(256 + palette.len() * 16);
    // DCS q with P2=1: zero bits keep existing content (transparency).
    out.extend_from_slice(b"\x1bP0;1;0q");
    // Raster attributes: 1:1 aspect and the pixel extent.
    out.extend_from_slice(format!("\"1;1;{width};{height}").as_bytes());
    for (index, &color) in palette.iter().enumerate() {
        let r = sixel_component((color >> 16) & 0xFF);
        let g = sixel_component((color >> 8) & 0xFF);
        let b = sixel_component(color & 0xFF);
        out.extend_from_slice(format!("#{index};2;{r};{g};{b}").as_bytes());
    }

    let w = width as usize;
    let mut band_bits: HashMap<u16, Vec<u8>> = HashMap::new();
    let mut band_order: Vec<u16> = Vec::new();
    let mut band_start_row = 0u32;
    while band_start_row < height {
        band_bits.clear();
        band_order.clear();
        let rows = (height - band_start_row).min(6);
        for dy in 0..rows {
            let row = (band_start_row + dy) as usize;
            let base = row * w * 4;
            for x in 0..w {
                let px = &pixels[base + x * 4..base + x * 4 + 4];
                if px[3] < OPAQUE_ALPHA {
                    continue;
                }
                let Some(&index) = map.get(&pack_rgb(px)) else {
                    continue;
                };
                let bits = band_bits.entry(index).or_insert_with(|| {
                    band_order.push(index);
                    vec![0u8; w]
                });
                bits[x] |= 1 << dy;
            }
        }
        band_order.sort_unstable();
        for (position, index) in band_order.iter().enumerate() {
            if position > 0 {
                out.push(b'$');
            }
            out.extend_from_slice(format!("#{index}").as_bytes());
            let bits = &band_bits[index];
            let mut x = 0usize;
            while x < w {
                let value = bits[x];
                let mut run = 1usize;
                while x + run < w && bits[x + run] == value {
                    run += 1;
                }
                let ch = 0x3F + value;
                if run > 3 {
                    out.extend_from_slice(format!("!{run}").as_bytes());
                    out.push(ch);
                } else {
                    for _ in 0..run {
                        out.push(ch);
                    }
                }
                x += run;
            }
        }
        band_start_row += 6;
        if band_start_row < height {
            out.push(b'-');
        }
    }

    out.extend_from_slice(b"\x1b\\");
    out
}

// ---------------------------------------------------------------------------
// Encode cache
// ---------------------------------------------------------------------------

/// Cache key: one encoded emission is a pure function of the source image
/// bytes (fingerprint + length), the sampled source rectangle, and the
/// target pixel size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SixelEncodeKey {
    pub(crate) data_fingerprint: u64,
    pub(crate) data_len: usize,
    pub(crate) source: (u32, u32, u32, u32),
    pub(crate) target: (u32, u32),
}

/// Byte-bounded LRU cache of encoded Sixel emissions shared by all
/// transcoding clients of one server.
pub(crate) struct SixelEncodeCache {
    entries: HashMap<SixelEncodeKey, (Vec<u8>, u64)>,
    total_bytes: usize,
    cap_bytes: usize,
    stamp: u64,
}

/// Default cache capacity (~64 MiB of encoded payloads).
const DEFAULT_CACHE_BYTES: usize = 64 * 1024 * 1024;

impl Default for SixelEncodeCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CACHE_BYTES)
    }
}

impl SixelEncodeCache {
    pub(crate) fn with_capacity(cap_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            total_bytes: 0,
            cap_bytes,
            stamp: 0,
        }
    }

    /// True when an emission for `key` is already cached (does not touch
    /// recency).
    pub(crate) fn contains(&self, key: &SixelEncodeKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Returns a clone of the cached emission, refreshing its recency.
    pub(crate) fn get(&mut self, key: &SixelEncodeKey) -> Option<Vec<u8>> {
        self.stamp += 1;
        let stamp = self.stamp;
        self.entries.get_mut(key).map(|(data, used)| {
            *used = stamp;
            data.clone()
        })
    }

    /// Inserts an emission, evicting least-recently-used entries until the
    /// cache fits its byte budget. Oversized single emissions are stored
    /// anyway (the frame budget bounds what rides a frame).
    pub(crate) fn insert(&mut self, key: SixelEncodeKey, data: Vec<u8>) {
        self.stamp += 1;
        if let Some((old, _)) = self.entries.remove(&key) {
            self.total_bytes -= old.len();
        }
        self.total_bytes += data.len();
        self.entries.insert(key, (data, self.stamp));
        while self.total_bytes > self.cap_bytes && self.entries.len() > 1 {
            let Some((&victim, _)) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, value)| (key, value))
            else {
                break;
            };
            if let Some((old, _)) = self.entries.remove(&victim) {
                self.total_bytes -= old.len();
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 4×4 checkerboard: opaque red/blue alternating per pixel.
    fn checkerboard_rgba() -> Vec<u8> {
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let mut out = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                if (x + y) % 2 == 0 {
                    out.extend_from_slice(&red);
                } else {
                    out.extend_from_slice(&blue);
                }
            }
        }
        out
    }

    #[test]
    fn checkerboard_encodes_exact_size_sixel() {
        let data = checkerboard_rgba();
        let encoded = encode_sixel_rgba(&data, 4, 4, 4, 4);
        let text = String::from_utf8(encoded.clone()).expect("sixel output is ascii");

        assert!(text.starts_with("\u{1b}P0;1;0q"), "DCS header: {text:?}");
        assert!(text.contains("\"1;1;4;4"), "raster dims: {text:?}");
        assert!(text.ends_with("\u{1b}\\"), "ST terminator: {text:?}");
        // Two distinct colors → two palette definitions with 0-100 scaling.
        assert!(text.contains("#0;2;0;0;100"), "blue palette: {text:?}");
        assert!(text.contains("#1;2;100;0;0"), "red palette: {text:?}");
    }

    #[test]
    fn scaling_doubles_raster_dims() {
        let data = checkerboard_rgba();
        let encoded = encode_sixel_rgba(&data, 4, 4, 8, 8);
        let text = String::from_utf8(encoded).expect("sixel output is ascii");
        assert!(text.contains("\"1;1;8;8"), "scaled raster dims: {text:?}");
        assert!(text.starts_with("\u{1b}P0;1;0q"));
        assert!(text.ends_with("\u{1b}\\"));
    }

    #[test]
    fn transparent_pixels_paint_nothing() {
        let data = vec![0u8; 4 * 4 * 4];
        let encoded = encode_sixel_rgba(&data, 4, 4, 4, 4);
        let text = String::from_utf8(encoded).expect("ascii");
        assert!(text.starts_with("\u{1b}P0;1;0q"));
        assert!(text.ends_with("\u{1b}\\"));
        assert!(!text.contains('#'), "no palette or paints: {text:?}");
    }

    #[test]
    fn degenerate_input_yields_empty_output() {
        assert!(encode_sixel_rgba(&[], 0, 0, 4, 4).is_empty());
        assert!(encode_sixel_rgba(&[0, 0, 0], 1, 1, 1, 1).is_empty());
    }

    #[test]
    fn quantizes_many_colors_to_at_most_256() {
        // 32×32 gradient with 1024 distinct colors.
        let mut data = Vec::new();
        for y in 0..32u32 {
            for x in 0..32u32 {
                data.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8, 255]);
            }
        }
        let encoded = encode_sixel_rgba(&data, 32, 32, 32, 32);
        let text = String::from_utf8(encoded).expect("ascii");
        // Every `#N` token (palette definitions and band color selects)
        // must address a palette slot below 256.
        let mut indexes = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'#' {
                let mut value = 0u32;
                let mut digits = 0;
                while i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                    value = value * 10 + u32::from(bytes[i + 1] - b'0');
                    digits += 1;
                    i += 1;
                }
                assert!(digits > 0, "bare # in output");
                indexes.push(value);
            }
            i += 1;
        }
        let max_index = indexes.iter().copied().max().expect("palette used");
        assert!(max_index < 256, "palette index out of range: {max_index}");
        assert!(text.contains(";2;"), "no palette definitions");
    }

    #[test]
    fn crop_extracts_expected_band() {
        let data = checkerboard_rgba();
        let (band, w, h) = crop_rgba(&data, 4, 4, 0, 1, 4, 1).expect("crop");
        assert_eq!((w, h), (4, 1));
        // Row 1 starts blue.
        assert_eq!(&band[0..4], &[0, 0, 255, 255]);
        assert!(crop_rgba(&data, 4, 4, 4, 0, 1, 1).is_none());
    }

    #[test]
    fn cache_evicts_oldest_when_over_budget() {
        let mut cache = SixelEncodeCache::with_capacity(10);
        let key = |i: u64| SixelEncodeKey {
            data_fingerprint: i,
            data_len: 1,
            source: (0, 0, 1, 1),
            target: (1, 1),
        };
        cache.insert(key(1), vec![0u8; 6]);
        cache.insert(key(2), vec![0u8; 6]);
        assert_eq!(cache.len(), 1, "oldest evicted");
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(2)).is_some());
    }

    #[test]
    fn cache_get_refreshes_recency() {
        let mut cache = SixelEncodeCache::with_capacity(12);
        let key = |i: u64| SixelEncodeKey {
            data_fingerprint: i,
            data_len: 1,
            source: (0, 0, 1, 1),
            target: (1, 1),
        };
        cache.insert(key(1), vec![0u8; 5]);
        cache.insert(key(2), vec![0u8; 5]);
        assert!(cache.get(&key(1)).is_some());
        cache.insert(key(3), vec![0u8; 5]);
        assert!(cache.get(&key(1)).is_some(), "recently used survives");
        assert!(cache.get(&key(2)).is_none(), "stale entry evicted");
    }

    #[test]
    fn rgb_expands_to_opaque_rgba() {
        assert_eq!(rgb_to_rgba(&[1, 2, 3, 4, 5, 6]), vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }
}
