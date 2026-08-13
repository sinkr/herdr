//! Virtual rendering helpers for headless client frame streaming.

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::layout::{Position, Rect, Size};

use crate::app::state::AppState;
use crate::app::Mode;
use crate::protocol::render_ansi::{BlitEncoder, EncodedBlit};
use crate::protocol::{
    CursorState, FrameData, RawOsc, RenderEncoding, ServerMessage, SixelSplice, TerminalFrame,
};
use crate::terminal::TerminalRuntimeRegistry;

/// Per-client render baseline for the negotiated render encoding.
pub(crate) enum ClientRenderState {
    /// Semantic clients compare full frame data and skip identical frames.
    Semantic { last_frame: Option<FrameData> },
    /// Terminal-ANSI clients keep a terminal diff encoder and sequence number.
    TerminalAnsi {
        blit_encoder: BlitEncoder,
        seq: u64,
        repaint_pending: bool,
    },
}

impl ClientRenderState {
    pub(crate) fn new(render_encoding: RenderEncoding) -> Self {
        match render_encoding {
            RenderEncoding::SemanticFrame => Self::Semantic { last_frame: None },
            RenderEncoding::TerminalAnsi => Self::TerminalAnsi {
                blit_encoder: BlitEncoder::new(),
                seq: 0,
                repaint_pending: false,
            },
        }
    }

    pub(crate) fn reset_baseline(&mut self) {
        match self {
            Self::Semantic { last_frame } => *last_frame = None,
            Self::TerminalAnsi {
                blit_encoder,
                repaint_pending,
                ..
            } => {
                *blit_encoder = BlitEncoder::new();
                *repaint_pending = false;
            }
        }
    }

    pub(crate) fn request_repaint(&mut self) {
        match self {
            Self::Semantic { last_frame } => *last_frame = None,
            Self::TerminalAnsi {
                repaint_pending, ..
            } => *repaint_pending = true,
        }
    }

    pub(crate) fn reset_semantic_input_baseline(&mut self) {
        if let Self::Semantic { last_frame } = self {
            *last_frame = None;
        }
    }

    pub(crate) fn is_terminal_ansi(&self) -> bool {
        matches!(self, Self::TerminalAnsi { .. })
    }

    pub(crate) fn prepare_frame(&mut self, frame: FrameData) -> Option<PreparedRender> {
        self.prepare_frame_with_sixels(frame, &[], Vec::new(), &[], Vec::new())
    }

    /// Prepares a frame like [`Self::prepare_frame`], additionally carrying
    /// one-shot passthrough emissions: pre-encoded ANSI bytes spliced into
    /// TerminalAnsi output (`sixels` positioned, `osc_bytes` verbatim), or
    /// records riding the semantic frame message (`splices` positioned,
    /// `raw_osc` verbatim). Passthrough payloads are never stored in the
    /// client baseline, so they are sent at most once even when the
    /// surrounding frame is otherwise unchanged.
    pub(crate) fn prepare_frame_with_sixels(
        &mut self,
        frame: FrameData,
        sixels: &[u8],
        splices: Vec<SixelSplice>,
        osc_bytes: &[u8],
        raw_osc: Vec<RawOsc>,
    ) -> Option<PreparedRender> {
        match self {
            Self::Semantic { last_frame } => {
                if splices.is_empty() && raw_osc.is_empty() && last_frame.as_ref() == Some(&frame)
                {
                    crate::render_prof::event("prepare_frame.semantic.skip_current");
                    return None;
                }
                crate::render_prof::event("prepare_frame.semantic.changed");
                crate::render_prof::counter(
                    "prepare_frame.sixel.splices",
                    splices.len() as u64,
                );
                crate::render_prof::counter("prepare_frame.raw_osc.records", raw_osc.len() as u64);
                Some(PreparedRender::Semantic {
                    message: ServerMessage::Frame {
                        frame,
                        sixels: splices,
                        raw_osc,
                    },
                })
            }
            Self::TerminalAnsi {
                blit_encoder,
                seq,
                repaint_pending,
            } => {
                debug_assert!(
                    splices.is_empty() && raw_osc.is_empty(),
                    "semantic passthrough records offered to a TerminalAnsi client"
                );
                if sixels.is_empty()
                    && osc_bytes.is_empty()
                    && !*repaint_pending
                    && blit_encoder.is_current(&frame)
                {
                    crate::render_prof::event("prepare_frame.ansi.skip_current");
                    return None;
                }
                let mut encoded = blit_encoder.encode(&frame, *repaint_pending);
                crate::render_prof::event("prepare_frame.ansi.changed");
                crate::render_prof::counter("prepare_frame.ansi.bytes", encoded.bytes.len() as u64);
                if encoded.full {
                    crate::render_prof::event("prepare_frame.ansi.full");
                } else {
                    crate::render_prof::event("prepare_frame.ansi.partial");
                }
                insert_graphics_before_sync_end(&mut encoded.bytes, &frame.graphics);
                insert_graphics_before_sync_end(&mut encoded.bytes, sixels);
                insert_graphics_before_sync_end(&mut encoded.bytes, osc_bytes);
                crate::render_prof::counter("prepare_frame.sixel.bytes", sixels.len() as u64);
                crate::render_prof::counter("prepare_frame.raw_osc.bytes", osc_bytes.len() as u64);
                crate::render_prof::counter(
                    "prepare_frame.graphics.bytes",
                    frame.graphics.len() as u64,
                );
                Some(PreparedRender::TerminalAnsi {
                    message: ServerMessage::Terminal(TerminalFrame {
                        seq: *seq + 1,
                        width: frame.width,
                        height: frame.height,
                        full: encoded.full,
                        bytes: encoded.bytes.clone(),
                    }),
                    frame,
                    encoded: Some(encoded),
                })
            }
        }
    }

    pub(crate) fn last_frame(&self) -> Option<&FrameData> {
        match self {
            Self::Semantic { last_frame } => last_frame.as_ref(),
            Self::TerminalAnsi { blit_encoder, .. } => blit_encoder.last_frame(),
        }
    }

    pub(crate) fn commit_sent_frame(&mut self, prepared: PreparedRender) {
        match (self, prepared) {
            (
                Self::Semantic { last_frame },
                PreparedRender::Semantic {
                    // Sixels are one-shot: only the frame enters the baseline.
                    message: ServerMessage::Frame { frame, .. },
                },
            ) => *last_frame = Some(frame),
            (
                Self::TerminalAnsi {
                    blit_encoder,
                    seq,
                    repaint_pending,
                },
                PreparedRender::TerminalAnsi {
                    frame,
                    encoded: Some(encoded),
                    ..
                },
            ) => {
                blit_encoder.commit(frame, encoded);
                *seq += 1;
                *repaint_pending = false;
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn terminal_seq(&self) -> Option<u64> {
        match self {
            Self::Semantic { .. } => None,
            Self::TerminalAnsi { seq, .. } => Some(*seq),
        }
    }
}

fn insert_graphics_before_sync_end(encoded: &mut Vec<u8>, graphics: &[u8]) {
    if graphics.is_empty() {
        return;
    }

    if let Some(sync_end) = crate::protocol::render_ansi::final_sync_output_end(encoded) {
        encoded.splice(sync_end..sync_end, graphics.iter().copied());
    } else {
        encoded.extend_from_slice(graphics);
    }
}

/// A prepared client render message plus any baseline state needed after send.
pub(crate) enum PreparedRender {
    Semantic {
        message: ServerMessage,
    },
    TerminalAnsi {
        message: ServerMessage,
        frame: FrameData,
        encoded: Option<EncodedBlit>,
    },
}

impl PreparedRender {
    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Semantic { message } | Self::TerminalAnsi { message, .. } => message,
        }
    }

    pub(crate) fn into_frame(self) -> Option<FrameData> {
        match self {
            Self::Semantic {
                message: ServerMessage::Frame { frame, .. },
            } => Some(frame),
            Self::TerminalAnsi { frame, .. } => Some(frame),
            _ => None,
        }
    }
}

struct CursorTrackingBackend {
    inner: TestBackend,
    rendered_cursor: Option<Position>,
}

impl CursorTrackingBackend {
    fn new(width: u16, height: u16) -> Self {
        Self {
            inner: TestBackend::new(width, height),
            rendered_cursor: None,
        }
    }

    fn buffer(&self) -> &ratatui::buffer::Buffer {
        self.inner.buffer()
    }

    fn rendered_cursor(&self) -> Option<CursorState> {
        self.rendered_cursor.map(|pos| CursorState {
            x: pos.x,
            y: pos.y,
            visible: true,
            shape: 0,
        })
    }
}

impl Backend for CursorTrackingBackend {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()?;
        self.rendered_cursor = None;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.rendered_cursor = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

/// Renders the AppState to an in-memory ratatui Buffer.
///
/// This produces the same output as the monolithic binary's terminal draw,
/// but writes to a `Buffer` instead of stdout. Cursor visibility is captured
/// from explicit frame cursor intent rather than incidental backend state.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn render_virtual(
    app_state: &mut AppState,
    area: Rect,
    resize_panes: bool,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let terminal_runtimes = TerminalRuntimeRegistry::new();
    render_virtual_with_runtime_registry(
        app_state,
        &terminal_runtimes,
        area,
        resize_panes,
        crate::kitty_graphics::HostCellSize::default(),
    )
}

pub(crate) fn render_virtual_with_runtime_registry(
    app_state: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let popup_visible = app_state.popup_pane.is_some();
    let pre_compute_suppresses_focused_terminal_cursor =
        !popup_visible && focused_terminal_suppresses_host_cursor(app_state, terminal_runtimes);
    if resize_panes {
        crate::ui::compute_view_with_cell_size(app_state, terminal_runtimes, area, cell_size);
    } else {
        crate::ui::compute_view_without_resizing_panes(app_state, terminal_runtimes, area);
    }
    let suppress_focused_terminal_cursor = pre_compute_suppresses_focused_terminal_cursor
        || (!popup_visible
            && focused_terminal_suppresses_host_cursor(app_state, terminal_runtimes));

    let backend = CursorTrackingBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");

    terminal
        .draw(|frame| {
            crate::ui::render_with_runtime_registry(app_state, terminal_runtimes, frame);
        })
        .expect("render to TestBackend should never fail");

    let buffer = terminal.backend().buffer().clone();
    let cursor = if popup_visible {
        popup_terminal_cursor(app_state, terminal_runtimes)
    } else if suppress_focused_terminal_cursor {
        None
    } else {
        focused_terminal_cursor(app_state, terminal_runtimes).or_else(|| {
            (!focused_terminal_owns_host_cursor(app_state, terminal_runtimes))
                .then(|| terminal.backend().rendered_cursor())
                .flatten()
        })
    };

    (buffer, cursor)
}

fn popup_terminal_cursor(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Option<CursorState> {
    let popup = app_state.popup_pane.as_ref()?;
    let runtime = terminal_runtimes.get(&popup.terminal_id)?;
    if runtime.synchronized_output_active() {
        return None;
    }
    let (_, inner) = crate::ui::popup_pane_rects(app_state, app_state.view.terminal_area)?;
    let cursor = runtime.cursor_state(inner, true)?;
    Some(CursorState {
        x: cursor.x,
        y: cursor.y,
        visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
        shape: cursor.shape,
    })
}

/// Renders one server-owned terminal directly for `terminal attach` clients.
pub(crate) fn render_terminal_virtual(
    runtime: &crate::terminal::TerminalRuntime,
    area: Rect,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let suppress_cursor = runtime.synchronized_output_active();
    let backend = CursorTrackingBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");

    terminal
        .draw(|frame| {
            runtime.render(frame, area, true);
        })
        .expect("render to TestBackend should never fail");

    let buffer = terminal.backend().buffer().clone();
    let cursor = (!suppress_cursor)
        .then(|| runtime.cursor_state(area, true))
        .flatten()
        .map(|cursor| CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
            shape: cursor.shape,
        })
        .or_else(|| {
            (!suppress_cursor)
                .then(|| terminal.backend().rendered_cursor())
                .flatten()
        });

    (buffer, cursor)
}

pub(crate) fn visible_hyperlinks(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Vec<((u16, u16), String, String)> {
    crate::ui::tab_surface_hyperlinks(app_state, terminal_runtimes, app_state.view.tab_surface())
}

pub(crate) fn focused_terminal_cursor(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Option<CursorState> {
    crate::ui::tab_surface_cursor(app_state, terminal_runtimes, app_state.view.tab_surface())
}

fn focused_terminal_owns_host_cursor(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> bool {
    if app_state.mode != Mode::Terminal {
        return false;
    }

    let Some(ws_idx) = app_state.active else {
        return false;
    };
    let Some(info) = app_state
        .view
        .pane_infos
        .iter()
        .find(|info| info.is_focused)
    else {
        return false;
    };
    if !app_state.pane_exposes_host_cursor(ws_idx, info.id) {
        return false;
    }

    app_state
        .runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)
        .is_some()
}

fn focused_terminal_suppresses_host_cursor(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> bool {
    if app_state.mode != Mode::Terminal {
        return false;
    }

    let Some(ws_idx) = app_state.active else {
        return false;
    };
    let Some(info) = app_state
        .view
        .pane_infos
        .iter()
        .find(|info| info.is_focused)
    else {
        return false;
    };
    if !app_state.pane_exposes_host_cursor(ws_idx, info.id) {
        return false;
    }

    app_state
        .runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)
        .is_some_and(crate::terminal::TerminalRuntime::synchronized_output_active)
}

#[cfg(test)]
mod sixel_passthrough_tests {
    use super::*;
    use crate::protocol::RenderEncoding;
    use crate::terminal::TerminalRuntime;

    const SIXEL: &[u8] = b"\x1bP0;0;8q\"1;1;4;4#0;2;0;0;0#0!4~-!4~\x1b\\";
    const SIXEL_FRAME_BUDGET: usize = 9 * 1024 * 1024;

    fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() || haystack.len() < needle.len() {
            return 0;
        }
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    fn client_frame(runtime: &TerminalRuntime, area: Rect) -> FrameData {
        let (buffer, cursor) = render_terminal_virtual(runtime, area);
        FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &[])
    }

    fn terminal_frame_bytes(prepared: &PreparedRender) -> Vec<u8> {
        match prepared.message() {
            ServerMessage::Terminal(frame) => frame.bytes.clone(),
            other => panic!("expected terminal frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sixel_passthrough_delivered_once_to_terminal_ansi_client() {
        let (runtime, _rx) = TerminalRuntime::test_with_channel(80, 24);
        let area = Rect::new(0, 0, 80, 24);
        let mut render_state = ClientRenderState::new(RenderEncoding::TerminalAnsi);

        // Client attached while the pane had no emissions: watermark is the
        // pane's newest sequence (0), mirroring headless lazy initialization.
        let mut watermark = runtime.latest_sixel_seq();
        assert_eq!(watermark, 0);

        let mut pty_bytes = b"hello".to_vec();
        pty_bytes.extend_from_slice(SIXEL);
        pty_bytes.extend_from_slice(b"world");
        runtime.test_process_pty_bytes(&pty_bytes);

        // First frame: the sixel DCS is spliced exactly once, wrapped in
        // DECSC + CUP (cursor was after "hello": row 1, col 6) + DECRC.
        let mut sixel_bytes = Vec::new();
        let advanced = runtime.encode_pending_sixels_after(
            watermark,
            (0, 0),
            SIXEL_FRAME_BUDGET,
            &mut sixel_bytes,
        );
        assert_eq!(advanced, 1);
        let prepared = render_state
            .prepare_frame_with_sixels(
                client_frame(&runtime, area),
                &sixel_bytes,
                Vec::new(),
                &[],
                Vec::new(),
            )
            .expect("first frame must encode");
        let client_bytes = terminal_frame_bytes(&prepared);
        assert_eq!(count_occurrences(&client_bytes, SIXEL), 1);
        let mut wrapped = b"\x1b7\x1b[1;6H".to_vec();
        wrapped.extend_from_slice(SIXEL);
        wrapped.extend_from_slice(b"\x1b8");
        assert_eq!(count_occurrences(&client_bytes, &wrapped), 1);
        render_state.commit_sent_frame(prepared);
        watermark = advanced;

        // The DCS payload must not have corrupted the text grid.
        let text_row: String = client_frame(&runtime, area).cells[..10]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();
        assert_eq!(text_row, "helloworld");

        // Second frame (new text, no new emission): zero sixel bytes.
        runtime.test_process_pty_bytes(b" again");
        let mut sixel_bytes = Vec::new();
        let advanced = runtime.encode_pending_sixels_after(
            watermark,
            (0, 0),
            SIXEL_FRAME_BUDGET,
            &mut sixel_bytes,
        );
        assert_eq!(advanced, watermark);
        assert!(sixel_bytes.is_empty());
        let prepared = render_state
            .prepare_frame_with_sixels(
                client_frame(&runtime, area),
                &sixel_bytes,
                Vec::new(),
                &[],
                Vec::new(),
            )
            .expect("changed text must encode a second frame");
        let client_bytes = terminal_frame_bytes(&prepared);
        assert_eq!(count_occurrences(&client_bytes, b"\x1bP"), 0);
        render_state.commit_sent_frame(prepared);

        // An unchanged frame with no pending sixels is skipped entirely.
        assert!(render_state
            .prepare_frame_with_sixels(
                client_frame(&runtime, area),
                &[],
                Vec::new(),
                &[],
                Vec::new()
            )
            .is_none());
    }

    #[tokio::test]
    async fn osc5522_passthrough_delivered_once_to_terminal_ansi_client() {
        const OSC: &[u8] = b"\x1b]5522;type=read:pw=abc\x07";
        const OSC_FRAME_BUDGET: usize = 4 * 1024 * 1024;

        let (runtime, _rx) = TerminalRuntime::test_with_channel(80, 24);
        let area = Rect::new(0, 0, 80, 24);
        let mut render_state = ClientRenderState::new(RenderEncoding::TerminalAnsi);

        let mut watermark = runtime.latest_osc5522_seq();
        assert_eq!(watermark, 0);

        let mut pty_bytes = b"hello".to_vec();
        pty_bytes.extend_from_slice(OSC);
        pty_bytes.extend_from_slice(b"world");
        runtime.test_process_pty_bytes(&pty_bytes);

        // First frame: the OSC is spliced exactly once, verbatim, with no
        // positioning wrap.
        let mut osc_bytes = Vec::new();
        let advanced =
            runtime.encode_pending_osc5522_after(watermark, OSC_FRAME_BUDGET, &mut osc_bytes);
        assert_eq!(advanced, 1);
        assert_eq!(osc_bytes, OSC);
        let prepared = render_state
            .prepare_frame_with_sixels(
                client_frame(&runtime, area),
                &[],
                Vec::new(),
                &osc_bytes,
                Vec::new(),
            )
            .expect("first frame must encode");
        let client_bytes = terminal_frame_bytes(&prepared);
        assert_eq!(count_occurrences(&client_bytes, OSC), 1);
        render_state.commit_sent_frame(prepared);
        watermark = advanced;

        // The OSC payload must not have corrupted the text grid.
        let text_row: String = client_frame(&runtime, area).cells[..10]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();
        assert_eq!(text_row, "helloworld");

        // Second frame (new text, no new emission): zero OSC bytes.
        runtime.test_process_pty_bytes(b" again");
        let mut osc_bytes = Vec::new();
        let advanced =
            runtime.encode_pending_osc5522_after(watermark, OSC_FRAME_BUDGET, &mut osc_bytes);
        assert_eq!(advanced, watermark);
        assert!(osc_bytes.is_empty());
        let prepared = render_state
            .prepare_frame_with_sixels(
                client_frame(&runtime, area),
                &[],
                Vec::new(),
                &osc_bytes,
                Vec::new(),
            )
            .expect("changed text must encode a second frame");
        let client_bytes = terminal_frame_bytes(&prepared);
        assert_eq!(count_occurrences(&client_bytes, b"\x1b]5522"), 0);
        render_state.commit_sent_frame(prepared);

        // An unchanged frame with no pending emissions is skipped entirely.
        assert!(render_state
            .prepare_frame_with_sixels(
                client_frame(&runtime, area),
                &[],
                Vec::new(),
                &[],
                Vec::new()
            )
            .is_none());
    }
}

#[cfg(test)]
mod render_scale_benchmark {
    use std::hint::black_box;
    use std::time::Instant;

    use ratatui::layout::Direction;

    use super::*;
    use crate::app::Mode;
    use crate::terminal::TerminalRuntime;
    use crate::workspace::Workspace;

    const AREA: Rect = Rect::new(0, 0, 120, 40);
    const SAMPLE_COUNT: usize = 40;
    const WARMUP_COUNT: usize = 5;

    #[derive(Clone, Copy)]
    struct RenderStats {
        median_us: u128,
        p95_us: u128,
        max_us: u128,
    }

    fn history() -> String {
        (0..2_000).map(|line| format!("line-{line}\r\n")).collect()
    }

    fn runtime(history: &str) -> TerminalRuntime {
        TerminalRuntime::test_with_scrollback_bytes(
            AREA.width,
            AREA.height,
            1024 * 1024,
            history.as_bytes(),
        )
    }

    fn app_with_workspaces(workspace_count: usize) -> AppState {
        let history = history();
        let workspaces = (0..workspace_count)
            .map(|index| {
                let mut workspace = Workspace::test_new(&format!("bench-{}", index + 1));
                let root_pane = workspace.tabs[0].root_pane;
                workspace.tabs[0]
                    .runtimes
                    .insert(root_pane, runtime(&history));
                workspace
            })
            .collect();
        app_with(workspaces)
    }

    fn app_with_active_panes(pane_count: usize) -> AppState {
        let history = history();
        let mut workspace = Workspace::test_new("bench");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0]
            .runtimes
            .insert(root_pane, runtime(&history));
        let mut pane_ids = vec![root_pane];

        for index in 1..pane_count {
            let target = pane_ids[(index - 1) / 2];
            workspace.tabs[0].layout.focus_pane(target);
            let direction = if index % 2 == 0 {
                Direction::Vertical
            } else {
                Direction::Horizontal
            };
            let pane_id = workspace.test_split(direction);
            workspace.tabs[0]
                .runtimes
                .insert(pane_id, runtime(&history));
            pane_ids.push(pane_id);
        }

        app_with(vec![workspace])
    }

    fn app_with(workspaces: Vec<Workspace>) -> AppState {
        let mut app = AppState::test_new();
        app.mode = Mode::Terminal;
        app.pane_scrollbars = true;
        app.workspaces = workspaces;
        app.active = Some(0);
        app.selected = 0;
        app
    }

    fn profile(mut app: AppState) -> RenderStats {
        for _ in 0..WARMUP_COUNT {
            black_box(render_virtual(&mut app, AREA, true));
        }

        let mut samples = Vec::with_capacity(SAMPLE_COUNT);
        for _ in 0..SAMPLE_COUNT {
            let started = Instant::now();
            black_box(render_virtual(&mut app, AREA, true));
            samples.push(started.elapsed().as_micros());
        }
        samples.sort_unstable();

        RenderStats {
            median_us: samples[SAMPLE_COUNT / 2],
            p95_us: samples[(SAMPLE_COUNT - 1) * 95 / 100],
            max_us: samples[SAMPLE_COUNT - 1],
        }
    }

    fn profile_cardinalities(build: fn(usize) -> AppState) -> [(usize, RenderStats); 3] {
        [1, 15, 50].map(|count| (count, profile(build(count))))
    }

    fn print_profiles(label: &str, profiles: [(usize, RenderStats); 3]) {
        let baseline_median_us = profiles[0].1.median_us as f64;
        let baseline_p95_us = profiles[0].1.p95_us as f64;
        println!("{label}");
        println!("     count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x");
        for (count, stats) in profiles {
            println!(
                "{count:>10}  {:>9}  {:>6}  {:>6}  {:>12.2}  {:>9.2}",
                stats.median_us,
                stats.p95_us,
                stats.max_us,
                stats.median_us as f64 / baseline_median_us,
                stats.p95_us as f64 / baseline_p95_us,
            );
        }
    }

    fn assert_full_render_avoids_aggregate_input_state(mut app: AppState, scenario: &str) {
        crate::pane::reset_aggregate_input_state_reads();
        black_box(render_virtual(&mut app, AREA, true));
        assert_eq!(
            crate::pane::aggregate_input_state_reads(),
            0,
            "full render collected aggregate input state for {scenario}",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aggregate_input_state_counter_records_reads() {
        let runtime = TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        crate::pane::reset_aggregate_input_state_reads();
        black_box(runtime.input_state());
        assert_eq!(crate::pane::aggregate_input_state_reads(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn full_render_avoids_aggregate_input_state_reads() {
        assert_full_render_avoids_aggregate_input_state(
            app_with_workspaces(15),
            "background workspaces",
        );
        assert_full_render_avoids_aggregate_input_state(app_with_active_panes(15), "active panes");
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "manual full-render scaling profile"]
    async fn render_scale_profile() {
        print_profiles(
            "background-workspace resize/layout (one pane each)",
            profile_cardinalities(app_with_workspaces),
        );
        print_profiles(
            "active panes (one workspace)",
            profile_cardinalities(app_with_active_panes),
        );
    }
}
