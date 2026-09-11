use std::collections::HashSet;
use std::io;
use std::sync::{Mutex, OnceLock};

use crate::protocol::render_ansi;

static RECEIVED_KITTY_GRAPHICS_IDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

pub(super) fn write_encoded_frame_with_graphics(
    mut writer: impl io::Write,
    encoded: &[u8],
    graphics: &[u8],
) -> io::Result<()> {
    write_encoded_frame_with_passthrough(&mut writer, encoded, graphics, &[])
}

pub(super) fn write_encoded_frame_with_passthrough(
    mut writer: impl io::Write,
    encoded: &[u8],
    graphics: &[u8],
    passthrough: &[u8],
) -> io::Result<()> {
    if graphics.is_empty() && passthrough.is_empty() {
        return writer.write_all(encoded);
    }
    let insertion = render_ansi::final_sync_output_end(encoded).unwrap_or(encoded.len());
    writer.write_all(&encoded[..insertion])?;
    if !graphics.is_empty() {
        record_received_kitty_graphics(graphics);
        writer.write_all(b"\x1b7")?;
        writer.write_all(graphics)?;
        writer.write_all(b"\x1b8")?;
    }
    writer.write_all(passthrough)?;
    writer.write_all(&encoded[insertion..])
}

pub(super) fn contains_kitty_graphics_bytes(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|window| window == b"\x1b_G")
}

pub(super) fn record_received_kitty_graphics(bytes: &[u8]) {
    let ids = kitty_graphics_image_ids(bytes);
    if ids.is_empty() {
        return;
    }
    let set = RECEIVED_KITTY_GRAPHICS_IDS.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = set.lock() {
        set.extend(ids);
    }
}

pub(super) fn clear_received_kitty_graphics(mut writer: impl io::Write) -> io::Result<()> {
    let Some(set) = RECEIVED_KITTY_GRAPHICS_IDS.get() else {
        return Ok(());
    };
    let Ok(mut set) = set.lock() else {
        return Ok(());
    };
    for id in set.drain() {
        write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\")?;
    }
    writer.flush()
}

pub(super) fn kitty_graphics_image_ids(bytes: &[u8]) -> Vec<u32> {
    let mut ids = Vec::new();
    let mut index = 0usize;
    while let Some(start) = find_subslice(&bytes[index..], b"\x1b_G") {
        let command_start = index + start + 3;
        let Some(end) = find_subslice(&bytes[command_start..], b"\x1b\\") else {
            break;
        };
        let command = &bytes[command_start..command_start + end];
        if let Some(id) = kitty_graphics_command_image_id(command) {
            ids.push(id);
        }
        index = command_start + end + 2;
    }
    ids
}

fn kitty_graphics_command_image_id(command: &[u8]) -> Option<u32> {
    let header_end = command
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(command.len());
    for part in command[..header_end].split(|byte| *byte == b',') {
        let Some(value) = part.strip_prefix(b"i=") else {
            continue;
        };
        let text = std::str::from_utf8(value).ok()?;
        if let Ok(id) = text.parse::<u32>() {
            return Some(id);
        }
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Appends one Sixel emission as a cursor-safe positioned write: DECSC
/// (`ESC 7`) + CUP to the 1-based absolute cell + the raw DCS payload +
/// DECRC (`ESC 8`).
///
/// `pane_rect` is the emitting pane's on-screen content rect for the frame
/// the splice arrived with; `None` (pane not visible) emits nothing, as
/// does a pane-local cell that falls outside the rect (pane resized or
/// scrolled since capture) or an absolute cell outside the frame.
pub(super) fn append_sixel_splice(
    out: &mut Vec<u8>,
    pane_rect: Option<crate::protocol::SixelPaneRect>,
    row: u16,
    col: u16,
    data: &[u8],
    frame_size: (u16, u16),
) {
    let Some(rect) = pane_rect else {
        return;
    };
    if row >= rect.height || col >= rect.width {
        return;
    }
    let abs_row = u32::from(rect.y) + u32::from(row);
    let abs_col = u32::from(rect.x) + u32::from(col);
    if abs_row >= u32::from(frame_size.1) || abs_col >= u32::from(frame_size.0) {
        return;
    }
    out.extend_from_slice(b"\x1b7");
    out.extend_from_slice(format!("\x1b[{};{}H", abs_row + 1, abs_col + 1).as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(b"\x1b8");
}
