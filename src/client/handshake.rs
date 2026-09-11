use std::io;
#[cfg(unix)]
use std::io::IsTerminal as _;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
#[cfg(windows)]
use tracing::debug;
use tracing::info;

use crate::ipc::LocalStream;
use crate::protocol::endpoint::{
    EndpointClientHello, EndpointServerWelcome, BLOB_CODEC_V1, ENDPOINT_HELLO_KIND,
    ENDPOINT_PROTOCOL_GENERATION, ENDPOINT_WELCOME_KIND, INPUT_CODEC_V1, SNAPSHOT_CODEC_V1,
    SURFACE_CODEC_V1,
};
use crate::protocol::{
    self, ClientMessage, RenderEncoding, ServerMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};

#[cfg(unix)]
use super::terminal_setup::is_ssh_session;
use super::{shell, ClientError};

/// Time to wait for the server's Welcome reply during the handshake.
///
/// A local client talks to an already-connected server, so 5s is plenty. The
/// remote bridge client (`herdr --remote`) sits behind a fresh per-attach ssh
/// connection whose cold-connect (TCP + key exchange + auth) happens inside this
/// window; on a high-latency link that easily exceeds 5s, so it gets a far
/// larger budget. See issue #753.
pub(super) const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn is_remote_client_process() -> bool {
    std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR).is_ok()
}

pub(super) fn client_shell_keybinding_source() -> shell::ClientShellKeybindingSource {
    match std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR)
        .ok()
        .as_deref()
    {
        Some("server") => shell::ClientShellKeybindingSource::Endpoint,
        Some(_) => shell::ClientShellKeybindingSource::RemoteLocal,
        None => shell::ClientShellKeybindingSource::Local,
    }
}

pub(super) fn handshake_read_timeout() -> Duration {
    if is_remote_client_process() {
        return REMOTE_HANDSHAKE_READ_TIMEOUT;
    }
    LOCAL_HANDSHAKE_READ_TIMEOUT
}

#[cfg(any(unix, test))]
pub(super) fn direct_graphics_profile_values(
    term_program: &str,
    term: &str,
    kitty_window: bool,
    blocked_transport: bool,
    terminals: bool,
) -> bool {
    let supported = term_program.eq_ignore_ascii_case("ghostty")
        || term_program.eq_ignore_ascii_case("wezterm")
        || matches!(term, "xterm-ghostty" | "xterm-kitty" | "xterm-wezterm")
        || kitty_window;
    supported && !blocked_transport && terminals
}

#[cfg(unix)]
fn direct_graphics_profile_allowed() -> bool {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    direct_graphics_profile_values(
        &term_program,
        &term,
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
        is_remote_client_process()
            || is_ssh_session()
            || std::env::var_os("TMUX").is_some()
            || std::env::var_os("STY").is_some(),
        io::stdin().is_terminal() && io::stdout().is_terminal(),
    )
}

#[cfg(not(unix))]
fn direct_graphics_profile_allowed() -> bool {
    false
}

#[cfg(windows)]
fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    context: &'static str,
) -> Result<(), ClientError> {
    match stream.set_recv_timeout(timeout) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            debug!(err = %err, context, "client socket receive timeout unavailable");
            Ok(())
        }
        Err(err) => Err(ClientError::ConnectionFailed(err)),
    }
}

#[cfg(not(windows))]
fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    _context: &'static str,
) -> Result<(), ClientError> {
    stream
        .set_recv_timeout(timeout)
        .map_err(ClientError::ConnectionFailed)
}

#[derive(Debug)]
pub(super) struct HandshakeResult {
    pub(super) encoding: RenderEncoding,
    pub(super) endpoint_methods: Option<Vec<String>>,
    pub(super) endpoint_capabilities: Option<Vec<String>>,
}

pub(crate) fn probe_endpoint_negotiation(
    stream: &mut LocalStream,
) -> io::Result<super::endpoint::EndpointNegotiation> {
    let handshake = do_handshake(
        stream,
        80,
        24,
        0,
        0,
        false,
        Some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
        false,
        false,
        false,
    )
    .map_err(io::Error::other)?;
    Ok(super::endpoint::EndpointNegotiation::new(
        handshake.endpoint_methods.unwrap_or_default(),
        handshake.endpoint_capabilities.unwrap_or_default(),
    ))
}

/// Performs the client→server handshake.
///
/// Direct terminal clients retain the same-install private protocol. Client-owned
/// shells use the stable endpoint generation and negotiate whole codecs without
/// comparing Herdr build versions.
pub(super) fn do_handshake(
    stream: &mut LocalStream,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    exact_cell_size: bool,
    shell_surface_size: Option<crate::protocol::ClientSurfaceSize>,
    endpoint_keybindings: bool,
    mouse_capture: bool,
    surface_active: bool,
) -> Result<HandshakeResult, ClientError> {
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    let endpoint_shell = shell_surface_size.is_some();
    let hello = if let Some(surface_size) = shell_surface_size {
        let hello = EndpointClientHello {
            generation: ENDPOINT_PROTOCOL_GENERATION,
            cell_width_px,
            cell_height_px,
            surface_size,
            pixel_mouse: exact_cell_size && cfg!(unix),
            direct_graphics: exact_cell_size
                && cell_width_px > 0
                && cell_height_px > 0
                && direct_graphics_profile_allowed(),
            endpoint_keybindings,
            mouse_capture,
            surface_active,
            sixel_graphics: HOST_GRAPHICS.get().is_some_and(|graphics| graphics.0),
            iip_graphics: HOST_GRAPHICS.get().is_some_and(|graphics| graphics.1),
            geometry_passive: geometry_passive_capability(),
            passthrough: true,
            snapshot_codecs: vec![SNAPSHOT_CODEC_V1.into()],
            surface_codecs: vec![SURFACE_CODEC_V1.into()],
            input_codecs: vec![INPUT_CODEC_V1.into()],
            blob_codecs: vec![BLOB_CODEC_V1.into()],
        };
        ClientMessage::EndpointControl {
            kind: ENDPOINT_HELLO_KIND.into(),
            data: serde_json::to_string(&hello).map_err(|error| {
                ClientError::ConnectionFailed(io::Error::new(io::ErrorKind::InvalidData, error))
            })?,
        }
    } else {
        ClientMessage::TerminalHello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse: exact_cell_size && cfg!(unix),
        }
    };
    protocol::write_message(stream, &hello)
        .map_err(|e| ClientError::ConnectionFailed(io::Error::other(e.to_string())))?;

    let read_timeout = if endpoint_shell && !surface_active {
        REMOTE_HANDSHAKE_READ_TIMEOUT
    } else {
        handshake_read_timeout()
    };
    set_handshake_recv_timeout(
        stream,
        Some(read_timeout),
        "client handshake read timeout unavailable",
    )?;
    let welcome: ServerMessage = protocol::read_message(stream, MAX_FRAME_SIZE)?;
    set_handshake_recv_timeout(
        stream,
        None,
        "failed to clear client handshake read timeout",
    )?;

    if endpoint_shell {
        let ServerMessage::EndpointControl { kind, data } = welcome else {
            return Err(ClientError::Protocol(protocol::FramingError::Io(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "server does not support the stable Herdr endpoint protocol; update this machine",
                ),
            )));
        };
        if kind != ENDPOINT_WELCOME_KIND {
            return Err(ClientError::Protocol(protocol::FramingError::Io(
                io::Error::new(io::ErrorKind::InvalidData, "expected endpoint welcome"),
            )));
        }
        let welcome: EndpointServerWelcome = serde_json::from_str(&data).map_err(|error| {
            ClientError::Protocol(protocol::FramingError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid endpoint welcome: {error}"),
            )))
        })?;
        if let Some(error) = welcome.error {
            return Err(ClientError::HandshakeRejected {
                version: welcome.generation,
                error: error.message,
            });
        }
        if welcome.generation != ENDPOINT_PROTOCOL_GENERATION
            || welcome.snapshot_codec != SNAPSHOT_CODEC_V1
            || welcome.surface_codec != SURFACE_CODEC_V1
            || welcome.input_codec != INPUT_CODEC_V1
            || welcome.blob_codec != BLOB_CODEC_V1
        {
            return Err(ClientError::HandshakeRejected {
                version: welcome.generation,
                error: "server has no compatible endpoint core; update this machine".into(),
            });
        }
        if (geometry_passive_capability()
            || HOST_GRAPHICS
                .get()
                .is_some_and(|graphics| graphics.0 || graphics.1))
            && !welcome
                .capabilities
                .iter()
                .any(|capability| capability == crate::protocol::endpoint::PASSTHROUGH_CAPABILITY)
        {
            return Err(ClientError::HandshakeRejected {
                version: welcome.generation,
                error: "server does not support requested viewer/graphics capabilities; update this machine".into(),
            });
        }
        info!(
            generation = welcome.generation,
            server_version = %welcome.server_version,
            "endpoint handshake succeeded"
        );
        return Ok(HandshakeResult {
            encoding: RenderEncoding::SemanticFrame,
            endpoint_methods: Some(welcome.methods),
            endpoint_capabilities: Some(welcome.capabilities),
        });
    }

    match welcome {
        ServerMessage::Welcome {
            version,
            encoding,
            error,
        } => {
            if let Some(error) = error {
                return Err(ClientError::HandshakeRejected { version, error });
            }
            if geometry_passive_capability() && version != PROTOCOL_VERSION {
                return Err(ClientError::HandshakeRejected {
                    version,
                    error: "server does not support geometry-passive terminal attach; update this machine".into(),
                });
            }
            info!(version, ?encoding, "handshake succeeded");
            Ok(HandshakeResult {
                encoding,
                endpoint_methods: None,
                endpoint_capabilities: None,
            })
        }
        _ => Err(ClientError::Protocol(protocol::FramingError::Io(
            io::Error::new(io::ErrorKind::InvalidData, "expected Welcome message"),
        ))),
    }
}

static HOST_GRAPHICS: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();

pub(super) fn initialize_host_graphics(kitty_graphics_enabled: bool) {
    HOST_GRAPHICS.get_or_init(|| {
        (
            sixel_graphics_capability(kitty_graphics_enabled),
            iip_graphics_capability(),
        )
    });
}

/// Primary Device Attributes request for the outer terminal.
const HOST_DA1_QUERY: &[u8] = b"\x1b[c";

/// How long the pre-handshake DA1 probe waits for the outer terminal's
/// reply before assuming no Sixel support.
#[cfg(unix)]
const DA1_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Environment override for the Sixel capability declared in Hello:
/// `1` forces it on, `0` forces it off, anything else defers to the probe.
pub(super) const FORCE_SIXEL_ENV_VAR: &str = "HERDR_FORCE_SIXEL";

/// Resolves the `sixel_graphics` value for this client's Hello.
///
/// `HERDR_FORCE_SIXEL=1`/`0` wins outright. Otherwise the outer terminal
/// is probed with DA1 only when client-side Kitty graphics are disabled
/// (a Kitty-capable outer terminal takes the native replay path instead).
pub(super) fn sixel_graphics_capability(kitty_graphics_enabled: bool) -> bool {
    match std::env::var(FORCE_SIXEL_ENV_VAR).ok().as_deref() {
        Some("1") => return true,
        Some("0") => return false,
        _ => {}
    }
    if kitty_graphics_enabled {
        return false;
    }
    probe_outer_terminal_sixel()
}

/// Environment override for the IIP capability declared in Hello:
/// `1` forces it on, `0` forces it off. There is no probe — IIP support
/// is not discoverable via DA1, so the capability defaults to off.
pub(super) const FORCE_IIP_ENV_VAR: &str = "HERDR_FORCE_IIP";

/// Resolves the `iip_graphics` value for this client's Hello.
///
/// `HERDR_FORCE_IIP=1`/`0` wins outright; without the override the
/// capability is off (env-only, no probe).
pub(super) fn iip_graphics_capability() -> bool {
    matches!(std::env::var(FORCE_IIP_ENV_VAR).ok().as_deref(), Some("1"))
}

/// Environment opt-in for the geometry-passive viewer mode declared in
/// Hello: `1` marks this client as a passive viewer that never becomes the
/// foreground client and never drives pane sizing. Env-only, no probe.
pub(super) const VIEWER_ENV_VAR: &str = "HERDR_VIEWER";

/// Resolves the `geometry_passive` value for this client's Hello.
///
/// `HERDR_VIEWER=1` opts in; anything else (including unset) is a normal
/// foreground-eligible client.
pub(super) fn geometry_passive_capability() -> bool {
    matches!(std::env::var(VIEWER_ENV_VAR).ok().as_deref(), Some("1"))
}

/// Parses an accumulated DA1 reply (`ESC [ ? Ps ; ... c`).
///
/// Returns `Some(true)` when the attribute list contains `4` (Sixel),
/// `Some(false)` for a complete reply without it, and `None` while no
/// complete reply has arrived yet.
pub(super) fn parse_da1_sixel_reply(buffer: &[u8]) -> Option<bool> {
    let mut index = 0;
    while let Some(start) = buffer[index..]
        .windows(3)
        .position(|window| window == b"\x1b[?")
    {
        let params_start = index + start + 3;
        let mut end = params_start;
        while end < buffer.len() && matches!(buffer[end], b'0'..=b'9' | b';') {
            end += 1;
        }
        if end >= buffer.len() {
            return None;
        }
        if buffer[end] == b'c' {
            let mut fields = buffer[params_start..end].split(|byte| *byte == b';');
            // The first parameter is the device class; Sixel is attribute 4
            // in the remainder.
            let _class = fields.next();
            return Some(fields.any(|field| field == b"4"));
        }
        index = end.max(params_start);
    }
    None
}

/// Probes the outer terminal's DA1 attributes for Sixel support.
///
/// Runs before the handshake (and before the client's terminal setup), so
/// it briefly enables raw mode itself and restores it. Any timeout, probe
/// failure, or non-TTY stdio reports no Sixel support. A reply arriving
/// after the timeout is consumed and dropped later by the raw input
/// framer's unsupported-sequence handling.
#[cfg(unix)]
fn probe_outer_terminal_sixel() -> bool {
    use std::io::IsTerminal;
    let stdin = io::stdin();
    if !stdin.is_terminal() || !io::stdout().is_terminal() {
        return false;
    }
    if crossterm::terminal::enable_raw_mode().is_err() {
        return false;
    }
    let sixel = run_da1_probe(&stdin);
    let _ = crossterm::terminal::disable_raw_mode();
    tracing::debug!(sixel, "probed outer terminal DA1 for sixel support");
    sixel
}

#[cfg(windows)]
fn probe_outer_terminal_sixel() -> bool {
    false
}

#[cfg(unix)]
fn run_da1_probe(stdin: &io::Stdin) -> bool {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    let mut stdout = io::stdout();
    if stdout
        .write_all(HOST_DA1_QUERY)
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return false;
    }

    let deadline = std::time::Instant::now() + DA1_PROBE_TIMEOUT;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match crate::client::input::poll_read_ready(
            stdin.as_raw_fd(),
            remaining.as_millis().min(i32::MAX as u128) as i32,
        ) {
            Some(true) => {}
            _ => return false,
        }
        let Ok(read) = stdin.lock().read(&mut chunk) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(sixel) = parse_da1_sixel_reply(&buffer) {
            return sixel;
        }
        if buffer.len() > 4096 {
            return false;
        }
    }
}
