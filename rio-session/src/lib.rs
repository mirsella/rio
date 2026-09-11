//! Durable per-terminal sessions for Rio.
//!
//! A session owns one PTY and one VT parser.  The GUI is a client of that
//! session and never parses PTY bytes or mutates a local terminal replica.

pub mod codec;
pub mod protocol;
#[cfg(unix)]
mod snapshot;
pub mod worker;

pub use protocol::{
    CellContentFrame, CellFrame, ClientMessage, FrameDelta, FrameUpdate, FullFrame,
    GlyphStatus, KeyAction, KeyCode, KeyInput, RequestKind, RequestRefusalReason,
    RowUpdate, SearchDirection, SearchMatch, SearchNavigation, SearchOrigin,
    SelectionKind, SelectionSide, ServerMessage, SessionCommand, SessionDescriptor,
    SessionEvent, SessionId, SessionReply, SessionSpec, ViMotion,
};

#[cfg(unix)]
use protocol::PROTOCOL_VERSION;
use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::io;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
const SESSION_TRANSPORT_SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
));

#[cfg(unix)]
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// Maximum authenticated renderer-readiness wait after the worker sends the
/// initial frame. This does not extend handshake/frame I/O deadlines or renew
/// detached-session retention. Callers must leave time for commit delivery.
pub const PREPARED_ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum SessionError {
    Io(io::Error),
    Codec(String),
    Protocol(String),
    Invalid(String),
    Unsupported(String),
    Random(String),
    Detached,
    WorkerExited,
}

impl SessionError {
    pub(crate) fn codec(message: impl Into<String>) -> Self {
        Self::Codec(message.into())
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    pub(crate) fn random(error: getrandom::Error) -> Self {
        Self::Random(error.to_string())
    }
}

impl Display for SessionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, formatter),
            Self::Codec(message)
            | Self::Protocol(message)
            | Self::Invalid(message)
            | Self::Unsupported(message)
            | Self::Random(message) => formatter.write_str(message),
            Self::Detached => formatter.write_str("session is detached"),
            Self::WorkerExited => formatter.write_str("session worker exited"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<io::Error> for SessionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(unix)]
struct ClientConnection {
    stream: std::os::unix::net::UnixStream,
    generation: u64,
    ready_pending: bool,
}

#[cfg(unix)]
struct ConnectedSession {
    stream: std::os::unix::net::UnixStream,
    generation: u64,
    initial_frame: FullFrame,
    had_active_owner: bool,
}

#[cfg(unix)]
struct WorkerCleanup {
    child: Option<std::process::Child>,
    endpoint_dir: PathBuf,
    recovery_path: Option<PathBuf>,
    active: bool,
}

#[cfg(unix)]
impl WorkerCleanup {
    fn into_child(mut self) -> std::process::Child {
        let child = self
            .child
            .take()
            .expect("worker cleanup child must be installed");
        self.active = false;
        child
    }
}

#[cfg(unix)]
impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(path) = self.recovery_path.take() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.endpoint_dir);
    }
}

/// An authenticated session attachment whose ownership has not been committed.
///
/// The worker continues serving the current owner while this value exists. A
/// destination can decode and prepare its initial frame before calling
/// [`PreparedSessionAttachment::commit`]. Dropping it closes only the pending
/// connection and leaves the current owner attached.
/// The worker accepts `Commit` only within [`PREPARED_ATTACHMENT_TIMEOUT`]
/// after sending the initial frame. Keep user selection outside this window;
/// prepare again when the destination has been chosen, not on an uncertain commit.
#[cfg(unix)]
pub struct PreparedSessionAttachment {
    descriptor: SessionDescriptor,
    connection: ConnectedSession,
}

#[cfg(not(unix))]
pub struct PreparedSessionAttachment;

#[cfg(not(unix))]
impl PreparedSessionAttachment {
    pub fn descriptor(&self) -> &SessionDescriptor {
        panic!("prepared session attachments are unsupported on this platform")
    }

    pub fn generation(&self) -> u64 {
        0
    }

    pub fn initial_frame(&self) -> &FullFrame {
        panic!("prepared session attachments are unsupported on this platform")
    }

    pub fn had_active_owner(&self) -> bool {
        false
    }

    pub fn commit(self) -> Result<SessionClient, SessionError> {
        Err(SessionError::unsupported(
            "prepared session attachments are currently supported only on Unix",
        ))
    }
}

#[cfg(unix)]
impl std::fmt::Debug for PreparedSessionAttachment {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedSessionAttachment")
            .field("session_id", &self.descriptor.session_id)
            .field("generation", &self.connection.generation)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl PreparedSessionAttachment {
    pub fn descriptor(&self) -> &SessionDescriptor {
        &self.descriptor
    }

    pub fn generation(&self) -> u64 {
        self.connection.generation
    }

    /// Whether the worker had another committed attachment while this one
    /// was prepared. Committing such an attachment is an explicit takeover.
    pub fn had_active_owner(&self) -> bool {
        self.connection.had_active_owner
    }

    pub fn initial_frame(&self) -> &FullFrame {
        &self.connection.initial_frame
    }

    /// Commit ownership after the destination has prepared its view.
    ///
    /// The write has a three-second I/O deadline, but must reach the worker
    /// before its preparation deadline. Success means the write completed;
    /// the first command or event poll validates the deferred `Ready` within
    /// its own three-second I/O deadline. An error after writing can leave
    /// ownership uncertain; this method does not retry or roll back ownership.
    pub fn commit(self) -> Result<SessionClient, SessionError> {
        let mut connection = self.connection;
        codec::write_frame_until(
            &mut connection.stream,
            &ClientMessage::Commit {
                generation: connection.generation,
            },
            Instant::now() + FRAME_TIMEOUT,
        )?;

        Ok(SessionClient {
            descriptor: self.descriptor,
            connection: Mutex::new(ClientConnection {
                stream: connection.stream,
                generation: connection.generation,
                ready_pending: true,
            }),
            poisoned: AtomicBool::new(false),
            next_request_id: AtomicU64::new(1),
            events: Mutex::new(VecDeque::new()),
            worker: Mutex::new(None),
        })
    }
}

/// A live attachment to one session worker.
///
/// Dropping this handle closes only the attachment.  The worker keeps the PTY
/// and parser alive until an explicit `close` or its bounded idle-retention
/// deadline.
pub struct SessionClient {
    descriptor: SessionDescriptor,
    #[cfg(unix)]
    connection: Mutex<ClientConnection>,
    poisoned: AtomicBool,
    #[cfg(unix)]
    next_request_id: AtomicU64,
    events: Mutex<VecDeque<SessionEvent>>,
    #[cfg(unix)]
    worker: Mutex<Option<std::process::Child>>,
}

impl std::fmt::Debug for SessionClient {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionClient")
            .field("session_id", &self.descriptor.session_id)
            .finish_non_exhaustive()
    }
}

impl SessionClient {
    pub fn spawn(spec: SessionSpec) -> Result<Self, SessionError> {
        #[cfg(unix)]
        {
            let worker = worker_binary()?;
            Self::spawn_with_worker_path(spec, worker)
        }

        #[cfg(not(unix))]
        {
            let _ = spec;
            Err(SessionError::unsupported(
                "session workers are currently supported only on Unix",
            ))
        }
    }

    #[cfg(unix)]
    pub fn spawn_with_worker_path(
        spec: SessionSpec,
        worker_path: impl AsRef<Path>,
    ) -> Result<Self, SessionError> {
        if !SESSION_TRANSPORT_SUPPORTED {
            return Err(SessionError::unsupported(
                "session workers are supported only on Linux and BSD/macOS Unix sockets",
            ));
        }
        spec.validate()?;
        let session_id = SessionId::random()?;
        let capability = random_capability()?;
        let endpoint_dir = endpoint_directory(session_id)?;
        let endpoint = endpoint_dir.join("session.sock");

        let mut cleanup = WorkerCleanup {
            child: None,
            endpoint_dir,
            recovery_path: None,
            active: true,
        };
        let child = std::process::Command::new(worker_path.as_ref())
            .env_clear()
            .arg("--endpoint")
            .arg(&endpoint)
            .arg("--session-id")
            .arg(session_id.hex())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| {
                SessionError::Io(io::Error::other(format!(
                    "start session worker: {error}"
                )))
            })?;
        cleanup.child = Some(child);

        let mut bootstrap = match cleanup
            .child
            .as_mut()
            .expect("worker cleanup child must be installed")
            .stdin
            .take()
        {
            Some(bootstrap) => bootstrap,
            None => {
                return Err(SessionError::protocol(
                    "worker bootstrap pipe is unavailable",
                ))
            }
        };
        if let Err(error) = std::io::Write::write_all(&mut bootstrap, &capability) {
            return Err(error.into());
        }
        drop(bootstrap);

        let descriptor = SessionDescriptor {
            endpoint,
            capability,
            session_id,
        };

        cleanup.recovery_path = Some(descriptor.save_recovery()?);

        let connection = connect_until_ready(&descriptor, Some(spec), true)?;
        let child = cleanup.into_child();

        Ok(Self {
            descriptor,
            connection: Mutex::new(ClientConnection {
                stream: connection.stream,
                generation: connection.generation,
                ready_pending: false,
            }),
            poisoned: AtomicBool::new(false),
            next_request_id: AtomicU64::new(1),
            events: Mutex::new(VecDeque::new()),
            worker: Mutex::new(Some(child)),
        })
    }

    #[cfg(not(unix))]
    pub fn spawn_with_worker_path(
        _spec: SessionSpec,
        _worker_path: impl AsRef<Path>,
    ) -> Result<Self, SessionError> {
        Err(SessionError::unsupported(
            "session workers are currently supported only on Unix",
        ))
    }

    #[cfg(unix)]
    pub fn attach(descriptor: SessionDescriptor) -> Result<Self, SessionError> {
        if !SESSION_TRANSPORT_SUPPORTED {
            return Err(SessionError::unsupported(
                "session workers are supported only on Linux and BSD/macOS Unix sockets",
            ));
        }
        descriptor.validate()?;
        let connection = connect_until_ready(&descriptor, None, true)?;
        Ok(Self {
            descriptor,
            connection: Mutex::new(ClientConnection {
                stream: connection.stream,
                generation: connection.generation,
                ready_pending: false,
            }),
            poisoned: AtomicBool::new(false),
            next_request_id: AtomicU64::new(1),
            events: Mutex::new(VecDeque::new()),
            worker: Mutex::new(None),
        })
    }

    /// Prepare an authenticated attachment without taking ownership from the
    /// current client. The returned initial frame is safe to decode and render
    /// before [`PreparedSessionAttachment::commit`] switches the worker owner.
    #[cfg(unix)]
    pub fn prepare_attach(
        descriptor: SessionDescriptor,
    ) -> Result<PreparedSessionAttachment, SessionError> {
        if !SESSION_TRANSPORT_SUPPORTED {
            return Err(SessionError::unsupported(
                "session workers are supported only on Linux and BSD/macOS Unix sockets",
            ));
        }
        descriptor.validate()?;
        let connection = connect_until_ready(&descriptor, None, false)?;
        Ok(PreparedSessionAttachment {
            descriptor,
            connection,
        })
    }

    #[cfg(not(unix))]
    pub fn prepare_attach(
        _descriptor: SessionDescriptor,
    ) -> Result<PreparedSessionAttachment, SessionError> {
        Err(SessionError::unsupported(
            "session workers are currently supported only on Unix",
        ))
    }

    #[cfg(not(unix))]
    pub fn attach(_descriptor: SessionDescriptor) -> Result<Self, SessionError> {
        Err(SessionError::unsupported(
            "session workers are currently supported only on Unix",
        ))
    }

    pub fn descriptor(&self) -> &SessionDescriptor {
        &self.descriptor
    }

    /// Execute one validated command on the session worker.
    ///
    /// This is intentionally kept as a low-level escape hatch for hosts that
    /// dispatch commands from a background pump. The typed helpers below are
    /// preferable when a command has a stable public shape.
    pub fn command(&self, command: SessionCommand) -> Result<SessionReply, SessionError> {
        let reply_kind = ReplyKind::for_command(&command);
        self.request(command, move |reply| {
            reply_kind.matches(&reply).then_some(reply)
        })
    }

    fn accepted(&self, command: SessionCommand) -> Result<(), SessionError> {
        self.request(command, |reply| {
            matches!(reply, SessionReply::Accepted).then_some(())
        })
    }

    fn changed(&self, command: SessionCommand) -> Result<bool, SessionError> {
        self.request(command, |reply| match reply {
            SessionReply::Accepted => Some(true),
            SessionReply::NoChange => Some(false),
            _ => None,
        })
    }

    fn accepted_or_unchanged(&self, command: SessionCommand) -> Result<(), SessionError> {
        self.request(command, |reply| {
            matches!(reply, SessionReply::Accepted | SessionReply::NoChange).then_some(())
        })
    }

    fn value<T>(
        &self,
        command: SessionCommand,
        decode: impl FnOnce(SessionReply) -> Option<T>,
    ) -> Result<T, SessionError> {
        self.request(command, decode)
    }

    /// Whether this attachment has been made unusable by a detected transport
    /// or protocol failure, detachment, or explicit close.
    ///
    /// Local validation and ordinary worker command rejections do not poison
    /// the attachment. This is not a liveness probe: a peer failure is only
    /// reflected after an I/O operation detects it.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub fn write(&self, bytes: Vec<u8>) -> Result<(), SessionError> {
        self.accepted(SessionCommand::Write(bytes))
    }

    pub fn paste(&self, text: impl Into<String>) -> Result<(), SessionError> {
        self.accepted(SessionCommand::Paste(text.into()))
    }

    /// Encode a key against the worker's current terminal modes before
    /// writing it to the PTY. The wire representation intentionally contains
    /// no platform-specific event types.
    pub fn key(&self, input: KeyInput) -> Result<bool, SessionError> {
        self.changed(SessionCommand::Key(input))
    }

    pub fn focus(&self, focused: bool) -> Result<bool, SessionError> {
        self.changed(SessionCommand::Focus { focused })
    }

    pub fn resize(
        &self,
        columns: u16,
        lines: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), SessionError> {
        self.accepted(SessionCommand::Resize {
            columns,
            lines,
            pixel_width,
            pixel_height,
        })
    }

    pub fn scroll(&self, delta_lines: i32) -> Result<(), SessionError> {
        self.accepted(SessionCommand::Scroll { delta_lines })
    }

    pub fn mouse_wheel(
        &self,
        lines: i32,
        column: u16,
        line: u16,
        modifiers: u8,
    ) -> Result<bool, SessionError> {
        self.changed(SessionCommand::MouseWheel {
            lines,
            column,
            line,
            modifiers,
        })
    }

    pub fn mouse_button(
        &self,
        column: u16,
        line: u16,
        button: u8,
        pressed: bool,
        modifiers: u8,
    ) -> Result<bool, SessionError> {
        self.changed(SessionCommand::MouseButton {
            column,
            line,
            button,
            pressed,
            modifiers,
        })
    }

    pub fn mouse_motion(
        &self,
        column: u16,
        line: u16,
        button: u8,
        modifiers: u8,
    ) -> Result<bool, SessionError> {
        self.changed(SessionCommand::MouseMotion {
            column,
            line,
            button,
            modifiers,
        })
    }

    pub fn selection_begin(
        &self,
        line: i32,
        column: usize,
        kind: SelectionKind,
        side: SelectionSide,
    ) -> Result<(), SessionError> {
        self.accepted(SessionCommand::SelectionBegin {
            line,
            column,
            kind,
            side,
        })
    }

    pub fn selection_update(
        &self,
        line: i32,
        column: usize,
        side: SelectionSide,
    ) -> Result<(), SessionError> {
        self.accepted(SessionCommand::SelectionUpdate { line, column, side })
    }

    pub fn selection_clear(&self) -> Result<(), SessionError> {
        self.accepted(SessionCommand::SelectionClear)
    }

    pub fn select_all(&self) -> Result<(), SessionError> {
        self.accepted(SessionCommand::SelectAll)
    }

    pub fn selection_autoscroll(
        &self,
        delta_lines: i32,
        line: i32,
        column: usize,
        side: SelectionSide,
    ) -> Result<bool, SessionError> {
        self.changed(SessionCommand::SelectionAutoScroll {
            delta_lines,
            line,
            column,
            side,
        })
    }

    pub fn selection_text(&self) -> Result<Option<String>, SessionError> {
        self.value(SessionCommand::SelectionText, |reply| match reply {
            SessionReply::SelectionText(text) => Some(text),
            _ => None,
        })
    }

    pub fn search(
        &self,
        pattern: impl Into<String>,
        max_matches: usize,
    ) -> Result<Vec<protocol::SearchMatch>, SessionError> {
        self.value(
            SessionCommand::Search {
                pattern: pattern.into(),
                max_matches,
            },
            |reply| match reply {
                SessionReply::SearchMatches(matches) => Some(matches),
                _ => None,
            },
        )
    }

    pub fn search_begin(
        &self,
        pattern: impl Into<String>,
        origin: SearchOrigin,
        direction: SearchDirection,
        side: SelectionSide,
        max_lines: Option<u32>,
    ) -> Result<SearchNavigation, SessionError> {
        self.value(
            SessionCommand::SearchBegin {
                pattern: pattern.into(),
                origin_line: origin.line,
                origin_column: origin.column,
                origin_display_offset: origin.display_offset,
                direction,
                side,
                max_lines,
            },
            |reply| match reply {
                SessionReply::SearchNavigation(navigation) => Some(navigation),
                _ => None,
            },
        )
    }

    pub fn search_next(&self) -> Result<SearchNavigation, SessionError> {
        self.value(SessionCommand::SearchNext, |reply| match reply {
            SessionReply::SearchNavigation(navigation) => Some(navigation),
            _ => None,
        })
    }

    pub fn search_cancel(&self) -> Result<SearchNavigation, SessionError> {
        self.value(SessionCommand::SearchCancel, |reply| match reply {
            SessionReply::SearchNavigation(navigation) => Some(navigation),
            _ => None,
        })
    }

    pub fn set_vi_mode(&self, enabled: bool) -> Result<bool, SessionError> {
        self.changed(SessionCommand::SetViMode(enabled))
    }

    pub fn toggle_vi_mode(&self) -> Result<bool, SessionError> {
        self.changed(SessionCommand::ToggleViMode)
    }

    pub fn vi_motion(&self, motion: ViMotion) -> Result<bool, SessionError> {
        self.changed(SessionCommand::ViMotion(motion))
    }

    pub fn vi_scroll(&self, delta_lines: i32) -> Result<bool, SessionError> {
        self.changed(SessionCommand::ViScroll { delta_lines })
    }

    pub fn vi_goto(&self, line: i32, column: u16) -> Result<bool, SessionError> {
        self.changed(SessionCommand::ViGoto { line, column })
    }

    pub fn scroll_to_prompt(&self, forward: bool) -> Result<(), SessionError> {
        self.accepted_or_unchanged(SessionCommand::ScrollToPrompt { forward })
    }

    pub fn scroll_top(&self) -> Result<(), SessionError> {
        self.accepted_or_unchanged(SessionCommand::ScrollTop)
    }

    pub fn scroll_bottom(&self) -> Result<(), SessionError> {
        self.accepted_or_unchanged(SessionCommand::ScrollBottom)
    }

    pub fn clear_saved_history(&self) -> Result<(), SessionError> {
        self.accepted(SessionCommand::ClearSavedHistory)
    }

    pub fn clipboard_response(
        &self,
        request_id: u64,
        route_id: u64,
        text: impl Into<String>,
    ) -> Result<(), SessionError> {
        self.accepted(SessionCommand::ClipboardResponse {
            request_id,
            route_id,
            text: text.into(),
        })
    }

    pub fn color_response(
        &self,
        request_id: u64,
        route_id: u64,
        color: Option<[u8; 3]>,
    ) -> Result<(), SessionError> {
        self.accepted(SessionCommand::ColorResponse {
            request_id,
            route_id,
            color,
        })
    }

    pub fn text_area_size_response(
        &self,
        request_id: u64,
        route_id: u64,
        rows: u16,
        columns: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), SessionError> {
        self.accepted(SessionCommand::TextAreaSizeResponse {
            request_id,
            route_id,
            rows,
            columns,
            pixel_width,
            pixel_height,
        })
    }

    pub fn glyph_protocol_response(
        &self,
        request_id: u64,
        route_id: u64,
        status: GlyphStatus,
    ) -> Result<(), SessionError> {
        self.accepted(SessionCommand::GlyphProtocolResponse {
            request_id,
            route_id,
            status,
        })
    }

    pub fn child_pid(&self) -> Result<u32, SessionError> {
        self.value(SessionCommand::ChildPid, |reply| match reply {
            SessionReply::ChildPid(pid) => Some(pid),
            _ => None,
        })
    }

    pub fn snapshot(&self) -> Result<FullFrame, SessionError> {
        self.value(SessionCommand::Snapshot, |reply| match reply {
            SessionReply::Frame(frame) => Some(frame),
            _ => None,
        })
    }

    pub fn snapshot_since(
        &self,
        base_sequence: u64,
    ) -> Result<FrameUpdate, SessionError> {
        self.value(
            SessionCommand::SnapshotSince { base_sequence },
            |reply| match reply {
                SessionReply::FrameUpdate(update) => Some(update),
                _ => None,
            },
        )
    }

    pub fn set_alt_is_meta(&self, enabled: bool) -> Result<(), SessionError> {
        self.accepted(SessionCommand::SetAltIsMeta(enabled))
    }

    pub fn set_cursor_style(
        &self,
        shape: u8,
        blinking: bool,
    ) -> Result<(), SessionError> {
        self.accepted(SessionCommand::SetCursorStyle { shape, blinking })
    }

    pub fn poll_event(&self) -> Result<Option<SessionEvent>, SessionError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(SessionError::Detached);
        }
        {
            let mut events = self.events.lock().map_err(|_| {
                SessionError::protocol("session event queue lock poisoned")
            })?;
            if let Some(event) = events.pop_front() {
                return Ok(Some(event));
            }
        }
        #[cfg(unix)]
        {
            let mut connection = self.connection.lock().map_err(|_| {
                SessionError::protocol("session connection lock poisoned")
            })?;
            self.ensure_ready(&mut connection)?;
            let mut poll_fd = libc::pollfd {
                fd: std::os::fd::AsRawFd::as_raw_fd(&connection.stream),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll_fd, 1, 1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(None);
                }
                self.poison_stream(&connection.stream);
                return Err(error.into());
            }
            if ready == 0 {
                return Ok(None);
            }
            if poll_fd.revents & libc::POLLNVAL != 0 {
                self.poison_stream(&connection.stream);
                return Err(SessionError::WorkerExited);
            }
            if poll_fd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
                return Ok(None);
            }
            let result: Result<ServerMessage, SessionError> = codec::read_frame_until(
                &mut connection.stream,
                Instant::now() + FRAME_TIMEOUT,
            );
            if let Err(error) = connection
                .stream
                .set_read_timeout(Some(Duration::from_millis(250)))
            {
                self.poison_stream(&connection.stream);
                return Err(error.into());
            }
            match result {
                Ok(message) => {
                    if let Err(error) = message.validate() {
                        self.poison_stream(&connection.stream);
                        return Err(error);
                    }
                    match message {
                        ServerMessage::Event { generation, event } => {
                            if generation != connection.generation {
                                self.poison_stream(&connection.stream);
                                return Err(SessionError::protocol(
                                    "worker returned a stale event generation",
                                ));
                            }
                            Ok(Some(event))
                        }
                        ServerMessage::Error { code, message } => {
                            self.poison_stream(&connection.stream);
                            Err(server_error(code, message))
                        }
                        ServerMessage::Detached => {
                            self.poison_stream(&connection.stream);
                            Err(SessionError::Detached)
                        }
                        _ => {
                            self.poison_stream(&connection.stream);
                            Err(SessionError::protocol(
                                "unexpected message while polling events",
                            ))
                        }
                    }
                }
                Err(error) => {
                    self.poison_stream(&connection.stream);
                    if matches!(
                        &error,
                        SessionError::Io(error)
                            if error.kind() == io::ErrorKind::UnexpectedEof
                    ) {
                        return Err(SessionError::WorkerExited);
                    }
                    Err(error)
                }
            }
        }
        #[cfg(not(unix))]
        {
            Err(SessionError::unsupported(
                "session workers are currently supported only on Unix",
            ))
        }
    }

    #[cfg(unix)]
    pub fn worker_pid(&self) -> Option<u32> {
        self.worker
            .lock()
            .ok()?
            .as_ref()
            .map(std::process::Child::id)
    }

    /// Wait for a worker process this client spawned. This never sends a
    /// signal; callers decide whether the worker should still be running.
    #[cfg(unix)]
    pub fn wait_worker(&self) -> Result<Option<std::process::ExitStatus>, SessionError> {
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| SessionError::protocol("worker lock poisoned"))?;
        worker
            .as_mut()
            .map(std::process::Child::wait)
            .transpose()
            .map_err(Into::into)
    }

    pub fn close(&self) -> Result<(), SessionError> {
        self.request(SessionCommand::Close, |reply| {
            matches!(reply, SessionReply::Closed).then_some(())
        })?;
        self.poisoned.store(true, Ordering::Release);
        self.descriptor.remove_recovery();
        Ok(())
    }

    #[cfg(unix)]
    fn ensure_ready(
        &self,
        connection: &mut ClientConnection,
    ) -> Result<(), SessionError> {
        if !connection.ready_pending {
            return Ok(());
        }

        let message: ServerMessage = match codec::read_frame_until(
            &mut connection.stream,
            Instant::now() + FRAME_TIMEOUT,
        ) {
            Ok(message) => message,
            Err(error) => {
                self.poison_stream(&connection.stream);
                return Err(error);
            }
        };
        if let Err(error) = message.validate() {
            self.poison_stream(&connection.stream);
            return Err(error);
        }
        match message {
            ServerMessage::Ready {
                version,
                session_id,
                generation,
            } if version == PROTOCOL_VERSION
                && session_id == self.descriptor.session_id
                && generation == connection.generation =>
            {
                connection.ready_pending = false;
                Ok(())
            }
            ServerMessage::Error { code, message } => {
                self.poison_stream(&connection.stream);
                Err(server_error(code, message))
            }
            _ => {
                self.poison_stream(&connection.stream);
                Err(SessionError::protocol(
                    "worker did not acknowledge prepared attachment",
                ))
            }
        }
    }

    #[cfg(unix)]
    fn request<T>(
        &self,
        command: SessionCommand,
        decode: impl FnOnce(SessionReply) -> Option<T>,
    ) -> Result<T, SessionError> {
        command.validate()?;
        if self.poisoned.load(Ordering::Acquire) {
            return Err(SessionError::Detached);
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| SessionError::protocol("session connection lock poisoned"))?;
        self.ensure_ready(&mut connection)?;
        let request_id = self
            .next_request_id
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| SessionError::protocol("request id exhausted"))?;

        let generation = connection.generation;
        if let Err(error) = codec::write_frame_until(
            &mut connection.stream,
            &ClientMessage::Command {
                generation,
                request_id,
                command,
            },
            Instant::now() + FRAME_TIMEOUT,
        ) {
            self.poison_stream(&connection.stream);
            return Err(error);
        }

        loop {
            let message: ServerMessage = match codec::read_frame_until(
                &mut connection.stream,
                Instant::now() + FRAME_TIMEOUT,
            ) {
                Ok(message) => message,
                Err(error) => {
                    self.poison_stream(&connection.stream);
                    return Err(error);
                }
            };
            if let Err(error) = message.validate() {
                self.poison_stream(&connection.stream);
                return Err(error);
            }
            match message {
                ServerMessage::Reply {
                    request_id: id,
                    reply,
                } => {
                    if id != request_id {
                        self.poison_stream(&connection.stream);
                        return Err(SessionError::protocol(
                            "worker returned an unexpected request id",
                        ));
                    }
                    match decode(reply) {
                        Some(reply) => return Ok(reply),
                        None => {
                            self.poison_stream(&connection.stream);
                            return Err(SessionError::protocol(
                                "worker returned an unexpected reply",
                            ));
                        }
                    }
                }
                ServerMessage::Event { generation, event } => {
                    if generation != connection.generation {
                        self.poison_stream(&connection.stream);
                        return Err(SessionError::protocol(
                            "worker returned a stale event generation",
                        ));
                    }
                    if let Err(error) = self.queue_event(event) {
                        self.poison_stream(&connection.stream);
                        return Err(error);
                    }
                }
                ServerMessage::Error { code, message } => {
                    if matches!(
                        code,
                        protocol::ErrorCode::BadProtocol
                            | protocol::ErrorCode::BadAuth
                            | protocol::ErrorCode::StaleGeneration
                    ) {
                        self.poison_stream(&connection.stream);
                    }
                    return Err(server_error(code, message));
                }
                ServerMessage::Detached => {
                    self.poison_stream(&connection.stream);
                    return Err(SessionError::Detached);
                }
                _ => {
                    self.poison_stream(&connection.stream);
                    return Err(SessionError::protocol(
                        "unexpected message while waiting for a reply",
                    ));
                }
            }
        }
    }

    #[cfg(unix)]
    fn poison_stream(&self, stream: &std::os::unix::net::UnixStream) {
        self.poisoned.store(true, Ordering::Release);
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    #[cfg(unix)]
    fn queue_event(&self, event: SessionEvent) -> Result<(), SessionError> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| SessionError::protocol("session event queue lock poisoned"))?;
        if matches!(event, SessionEvent::FrameReady)
            && events
                .iter()
                .any(|pending| matches!(pending, SessionEvent::FrameReady))
        {
            return Ok(());
        }
        if let SessionEvent::Title { .. } | SessionEvent::Progress { .. } = event {
            events.retain(|pending| {
                !matches!(
                    (&event, pending),
                    (SessionEvent::Title { .. }, SessionEvent::Title { .. })
                        | (SessionEvent::Progress { .. }, SessionEvent::Progress { .. })
                )
            });
        }
        if let SessionEvent::ColorChange {
            route_id, index, ..
        } = &event
        {
            events.retain(|pending| {
                !matches!(
                    pending,
                    SessionEvent::ColorChange {
                        route_id: pending_route_id,
                        index: pending_index,
                        ..
                    } if pending_route_id == route_id && pending_index == index
                )
            });
        }
        if matches!(
            event,
            SessionEvent::ChildExited { .. }
                | SessionEvent::ClipboardOverflow
                | SessionEvent::Closed
        ) && events.iter().any(|pending| {
            matches!(
                (&event, pending),
                (
                    SessionEvent::ChildExited { .. },
                    SessionEvent::ChildExited { .. }
                ) | (
                    SessionEvent::ClipboardOverflow,
                    SessionEvent::ClipboardOverflow
                ) | (SessionEvent::Closed, SessionEvent::Closed)
            )
        }) {
            return Ok(());
        }
        if events.len() >= protocol::MAX_PENDING_REQUESTS {
            let removable = events.iter().position(|pending| !pending.is_critical());
            if let Some(index) = removable {
                let _ = events.remove(index);
            } else if matches!(event, SessionEvent::ClipboardStore { .. }) {
                if events
                    .iter()
                    .any(|pending| matches!(pending, SessionEvent::ClipboardOverflow))
                {
                    return Ok(());
                }
                // Do not exceed the bounded queue when all retained events
                // are critical; preserve those events instead.
                return Ok(());
            } else if matches!(
                event,
                SessionEvent::Closed | SessionEvent::ChildExited { .. }
            ) {
                // Preserve lifecycle delivery when saturated, mirroring the
                // worker queue: the session is ending, so the exit event
                // replaces older critical state instead of poisoning.
                events.clear();
            } else {
                // Drop additional critical state when saturated, mirroring
                // the worker queue, instead of poisoning the attachment.
                return Ok(());
            }
        }
        events.push_back(event);
        Ok(())
    }

    #[cfg(not(unix))]
    fn request<T>(
        &self,
        _command: SessionCommand,
        _decode: impl FnOnce(SessionReply) -> Option<T>,
    ) -> Result<T, SessionError> {
        Err(SessionError::unsupported(
            "session workers are currently supported only on Unix",
        ))
    }
}

#[derive(Clone, Copy)]
enum ReplyKind {
    Accepted,
    AcceptedOrNoChange,
    Frame,
    FrameUpdate,
    SelectionText,
    SearchMatches,
    SearchNavigation,
    ChildPid,
    Closed,
}

impl ReplyKind {
    fn for_command(command: &SessionCommand) -> Self {
        match command {
            SessionCommand::Write(_)
            | SessionCommand::Paste(_)
            | SessionCommand::Resize { .. }
            | SessionCommand::Scroll { .. }
            | SessionCommand::SelectionBegin { .. }
            | SessionCommand::SelectionUpdate { .. }
            | SessionCommand::SelectionClear
            | SessionCommand::SelectAll
            | SessionCommand::ClearSavedHistory
            | SessionCommand::SetAltIsMeta(_)
            | SessionCommand::SetCursorStyle { .. } => Self::Accepted,
            SessionCommand::Focus { .. } => Self::AcceptedOrNoChange,
            SessionCommand::Key(_)
            | SessionCommand::ScrollToPrompt { .. }
            | SessionCommand::ScrollTop
            | SessionCommand::ScrollBottom
            | SessionCommand::MouseWheel { .. }
            | SessionCommand::MouseButton { .. }
            | SessionCommand::MouseMotion { .. }
            | SessionCommand::SelectionAutoScroll { .. }
            | SessionCommand::SetViMode(_)
            | SessionCommand::ToggleViMode
            | SessionCommand::ViMotion(_)
            | SessionCommand::ViScroll { .. }
            | SessionCommand::ViGoto { .. } => Self::AcceptedOrNoChange,
            SessionCommand::SelectionText => Self::SelectionText,
            SessionCommand::Search { .. } => Self::SearchMatches,
            SessionCommand::SearchBegin { .. }
            | SessionCommand::SearchNext
            | SessionCommand::SearchCancel => Self::SearchNavigation,
            SessionCommand::ChildPid => Self::ChildPid,
            SessionCommand::Snapshot => Self::Frame,
            SessionCommand::SnapshotSince { .. } => Self::FrameUpdate,
            SessionCommand::ClipboardResponse { .. }
            | SessionCommand::ColorResponse { .. }
            | SessionCommand::TextAreaSizeResponse { .. }
            | SessionCommand::GlyphProtocolResponse { .. } => Self::Accepted,
            SessionCommand::Close => Self::Closed,
        }
    }

    fn matches(self, reply: &SessionReply) -> bool {
        match self {
            Self::Accepted => matches!(reply, SessionReply::Accepted),
            Self::AcceptedOrNoChange => {
                matches!(reply, SessionReply::Accepted | SessionReply::NoChange)
            }
            Self::Frame => matches!(reply, SessionReply::Frame(_)),
            Self::FrameUpdate => matches!(reply, SessionReply::FrameUpdate(_)),
            Self::SelectionText => matches!(reply, SessionReply::SelectionText(_)),
            Self::SearchMatches => matches!(reply, SessionReply::SearchMatches(_)),
            Self::SearchNavigation => matches!(reply, SessionReply::SearchNavigation(_)),
            Self::ChildPid => matches!(reply, SessionReply::ChildPid(_)),
            Self::Closed => matches!(reply, SessionReply::Closed),
        }
    }
}

#[cfg(unix)]
fn worker_binary() -> Result<PathBuf, SessionError> {
    if let Some(path) = std::env::var_os("RIO_SESSION_WORKER") {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe()?;
    let parent = current
        .parent()
        .ok_or_else(|| SessionError::protocol("current executable has no parent"))?;
    let direct = parent.join("rio-session-worker");
    if direct.exists() {
        return Ok(direct);
    }
    Ok(parent
        .parent()
        .map(|parent| parent.join("rio-session-worker"))
        .unwrap_or(direct))
}

#[cfg(unix)]
fn random_capability() -> Result<[u8; 32], SessionError> {
    let mut capability = [0; 32];
    getrandom::fill(&mut capability).map_err(SessionError::random)?;
    Ok(capability)
}

#[cfg(unix)]
fn endpoint_directory(session_id: SessionId) -> Result<PathBuf, SessionError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let mut base_builder = std::fs::DirBuilder::new();
    base_builder.recursive(true).mode(0o700).create(&base)?;
    let directory = base.join(format!("rio-session-{}", session_id.hex()));
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(&directory)?;
    let metadata = std::fs::symlink_metadata(&directory)?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        let _ = std::fs::remove_dir(&directory);
        return Err(SessionError::protocol(
            "session endpoint directory is not private",
        ));
    }
    Ok(directory)
}

#[cfg(unix)]
fn connect_until_ready(
    descriptor: &SessionDescriptor,
    spec: Option<SessionSpec>,
    commit: bool,
) -> Result<ConnectedSession, SessionError> {
    use std::os::unix::net::UnixStream;

    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        capability: descriptor.capability,
        session_id: descriptor.session_id,
        spec,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(&descriptor.endpoint) {
            Ok(mut stream) => {
                stream.set_read_timeout(Some(Duration::from_millis(250)))?;
                stream.set_write_timeout(Some(Duration::from_millis(250)))?;
                codec::write_frame_until(
                    &mut stream,
                    &hello,
                    deadline.min(Instant::now() + FRAME_TIMEOUT),
                )?;
                let mut offered_generation = None;
                let mut claimed_generation = None;
                let mut initial_frame = None;
                loop {
                    let message: ServerMessage = codec::read_frame_until(
                        &mut stream,
                        deadline.min(Instant::now() + FRAME_TIMEOUT),
                    )?;
                    message.validate()?;
                    match message {
                        ServerMessage::Ready {
                            version,
                            session_id,
                            generation,
                        } => {
                            if version != PROTOCOL_VERSION
                                || session_id != descriptor.session_id
                                || claimed_generation.is_none()
                                || claimed_generation
                                    .is_some_and(|claimed| claimed != generation)
                            {
                                return Err(SessionError::protocol(
                                    "worker returned an incompatible identity",
                                ));
                            }
                            let initial_frame =
                                initial_frame.take().ok_or_else(|| {
                                    SessionError::protocol(
                                        "worker returned ready without an initial frame",
                                    )
                                })?;
                            return Ok(ConnectedSession {
                                stream,
                                generation,
                                initial_frame,
                                had_active_owner: false,
                            });
                        }
                        ServerMessage::Offer { generation } => {
                            if offered_generation.is_some()
                                || claimed_generation.is_some()
                            {
                                return Err(SessionError::protocol(
                                    "worker sent an offer out of order",
                                ));
                            }
                            offered_generation = Some(generation);
                            codec::write_frame_until(
                                &mut stream,
                                &ClientMessage::Claim { generation },
                                deadline.min(Instant::now() + FRAME_TIMEOUT),
                            )?;
                        }
                        ServerMessage::Claimed { generation } => {
                            if generation == 0 {
                                return Err(SessionError::protocol(
                                    "worker returned an invalid attachment generation",
                                ));
                            }
                            if offered_generation
                                .is_some_and(|offered| generation <= offered)
                            {
                                return Err(SessionError::protocol(
                                    "worker did not advance the offered attachment generation",
                                ));
                            }
                            if claimed_generation.replace(generation).is_some() {
                                return Err(SessionError::protocol(
                                    "worker sent duplicate attachment generations",
                                ));
                            }
                            let frame = loop {
                                let message: ServerMessage = codec::read_frame_until(
                                    &mut stream,
                                    deadline.min(Instant::now() + FRAME_TIMEOUT),
                                )?;
                                message.validate()?;
                                match message {
                                    ServerMessage::Initial {
                                        generation: initial_generation,
                                        frame,
                                    } if initial_generation == generation => break frame,
                                    ServerMessage::Event {
                                        generation: event_generation,
                                        event,
                                    } if event_generation == generation => {
                                        if !matches!(
                                            event,
                                            protocol::SessionEvent::FrameReady
                                        ) {
                                            return Err(SessionError::protocol(
                                                "worker sent an event before the initial frame",
                                            ));
                                        }
                                    }
                                    _ => {
                                        return Err(SessionError::protocol(
                                            "worker did not send the initial frame",
                                        ));
                                    }
                                }
                            };
                            frame.validate()?;
                            if !commit {
                                return Ok(ConnectedSession {
                                    stream,
                                    generation,
                                    initial_frame: frame,
                                    had_active_owner: offered_generation.is_some(),
                                });
                            }
                            initial_frame = Some(frame);
                            codec::write_frame_until(
                                &mut stream,
                                &ClientMessage::Commit { generation },
                                deadline.min(Instant::now() + FRAME_TIMEOUT),
                            )?;
                        }
                        ServerMessage::Initial { .. } => {
                            return Err(SessionError::protocol(
                                "worker sent an initial frame without claiming it",
                            ));
                        }
                        ServerMessage::Event { .. } => {
                            return Err(SessionError::protocol(
                                "worker sent an event before attachment was ready",
                            ));
                        }
                        ServerMessage::Error { code, message } => {
                            return Err(SessionError::protocol(format!(
                                "{code:?}: {message}"
                            )));
                        }
                        ServerMessage::Detached => {
                            return Err(SessionError::Detached);
                        }
                        _ => {
                            return Err(SessionError::protocol(
                                "unexpected handshake message",
                            ))
                        }
                    }
                }
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(SessionError::protocol(format!(
                        "worker did not become ready: {error}"
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn server_error(code: protocol::ErrorCode, message: String) -> SessionError {
    match code {
        protocol::ErrorCode::BadRequest => SessionError::invalid(message),
        protocol::ErrorCode::Unsupported => SessionError::unsupported(message),
        _ => SessionError::protocol(format!("{code:?}: {message}")),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{ClientConnection, ReplyKind, SessionClient, SessionCommand};
    use crate::protocol::{
        SessionDescriptor, SessionEvent, SessionId, MAX_PENDING_REQUESTS,
    };
    use std::collections::VecDeque;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Mutex;

    #[test]
    fn focus_no_change_is_accepted_by_the_pump_classifier() {
        assert!(matches!(
            ReplyKind::for_command(&SessionCommand::Focus { focused: true }),
            ReplyKind::AcceptedOrNoChange
        ));
    }

    #[test]
    fn client_event_queue_stays_bounded_when_critical_is_full() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let client = SessionClient {
            descriptor: SessionDescriptor {
                endpoint: PathBuf::from("/var/empty/session.sock"),
                capability: [1; 32],
                session_id: SessionId([1; 16]),
            },
            connection: Mutex::new(ClientConnection {
                stream,
                generation: 1,
                ready_pending: false,
            }),
            poisoned: AtomicBool::new(false),
            next_request_id: AtomicU64::new(1),
            events: Mutex::new(VecDeque::new()),
            worker: Mutex::new(None),
        };

        for index in 0..MAX_PENDING_REQUESTS {
            client
                .queue_event(SessionEvent::ColorChange {
                    route_id: 1,
                    index: index as u16,
                    color: None,
                })
                .unwrap();
        }
        client
            .queue_event(SessionEvent::ClipboardStore {
                kind: 0,
                text: String::from("overflow"),
            })
            .unwrap();
        // Additional critical state is dropped while saturated, without
        // poisoning the attachment.
        client
            .queue_event(SessionEvent::ColorChange {
                route_id: 1,
                index: u16::MAX,
                color: None,
            })
            .unwrap();
        {
            let events = client.events.lock().unwrap();
            assert_eq!(events.len(), MAX_PENDING_REQUESTS);
            assert!(events.iter().any(|event| matches!(
                event,
                SessionEvent::ColorChange { index: 0, .. }
            )));
        }
        // Lifecycle delivery is preserved: the exit event replaces older
        // critical state instead of poisoning the attachment.
        client
            .queue_event(SessionEvent::ChildExited { status: Some(1) })
            .unwrap();

        let events = client.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events
            .iter()
            .any(|event| matches!(event, SessionEvent::ChildExited { status: Some(1) })));
    }
}
