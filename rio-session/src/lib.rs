//! Durable per-terminal sessions for Rio.
//!
//! A session owns one PTY and one VT parser.  The GUI is a client of that
//! session and never parses PTY bytes or mutates a local terminal replica.

pub mod codec;
pub mod protocol;
#[cfg(unix)]
pub mod readiness;
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
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
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

/// Listening socket name inside a session's private directory.
#[cfg(unix)]
pub(crate) const SESSION_SOCKET_FILE: &str = "session.sock";

#[cfg(unix)]
type FileIdentity = (u64, u64);

#[cfg(unix)]
type EndpointBinding = (UnixListener, FileIdentity, FileIdentity);

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
    next_request_id: u64,
    initial_frame: Option<FullFrame>,
}

#[cfg(unix)]
struct ConnectedSession {
    stream: std::os::unix::net::UnixStream,
    generation: u64,
    initial_frame: FullFrame,
    had_active_owner: bool,
}

#[cfg(unix)]
struct RecoveryCleanup {
    path: PathBuf,
    file: std::fs::File,
}

#[cfg(unix)]
struct WorkerCleanup {
    child: Option<std::process::Child>,
    endpoint: PathBuf,
    endpoint_dir: PathBuf,
    endpoint_identity: Option<FileIdentity>,
    endpoint_dir_identity: FileIdentity,
    recovery: Option<RecoveryCleanup>,
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
        if let Some(recovery) = self.recovery.take() {
            let _ = protocol::remove_file_if_open_file(&recovery.path, &recovery.file);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        cleanup_endpoint_if_owned(
            &self.endpoint,
            self.endpoint_identity,
            &self.endpoint_dir,
            self.endpoint_dir_identity,
        );
    }
}

#[cfg(unix)]
pub(crate) fn queue_event(
    events: &mut VecDeque<SessionEvent>,
    event: SessionEvent,
) -> bool {
    if terminal_request_event_id(&event).is_some_and(|request_id| {
        events
            .iter()
            .any(|pending| terminal_request_event_id(pending) == Some(request_id))
    }) {
        return true;
    }
    if matches!(
        event,
        SessionEvent::Title { .. } | SessionEvent::Progress { .. }
    ) {
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
            ) | (SessionEvent::Closed, SessionEvent::Closed)
                | (
                    SessionEvent::ClipboardOverflow,
                    SessionEvent::ClipboardOverflow
                )
        )
    }) {
        return true;
    }
    if matches!(event, SessionEvent::FrameReady)
        && events
            .iter()
            .any(|pending| matches!(pending, SessionEvent::FrameReady))
    {
        return true;
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
                return true;
            }
            // Keep the queue bounded when every slot already contains a
            // critical event. Existing lifecycle/request events win over an
            // additional overflow notification.
            return false;
        } else if matches!(
            event,
            SessionEvent::Closed | SessionEvent::ChildExited { .. }
        ) {
            // Preserve lifecycle delivery when saturated: the session is
            // ending, so the exit event replaces older critical state.
            events.clear();
            events.push_back(event);
            return true;
        } else {
            // Drop additional critical state when saturated instead of
            // exceeding the bounded queue.
            return false;
        }
    }
    events.push_back(event);
    true
}

#[cfg(unix)]
pub(crate) fn terminal_request_event_id(event: &SessionEvent) -> Option<u64> {
    match event {
        SessionEvent::ClipboardLoad { request_id, .. }
        | SessionEvent::ColorRequest { request_id, .. }
        | SessionEvent::TextAreaSizeRequest { request_id, .. }
        | SessionEvent::GlyphProtocolQuery { request_id, .. } => Some(*request_id),
        _ => None,
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
    recovery_file: Option<std::fs::File>,
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
        let activity_wakeup = readiness::Readiness::new()?;
        let mut connection = self.connection;
        codec::write_frame_until(
            &mut connection.stream,
            &ClientMessage::Commit {
                generation: connection.generation,
            },
            Instant::now() + FRAME_TIMEOUT,
        )?;

        Ok(SessionClient::from_parts(
            self.descriptor,
            ClientConnection {
                stream: connection.stream,
                generation: connection.generation,
                ready_pending: true,
                next_request_id: 1,
                initial_frame: None,
            },
            activity_wakeup,
            None,
            self.recovery_file,
        ))
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
    events: Mutex<VecDeque<SessionEvent>>,
    #[cfg(unix)]
    activity_wakeup: readiness::Readiness,
    #[cfg(unix)]
    worker: Mutex<Option<std::process::Child>>,
    #[cfg(unix)]
    recovery_file: Option<std::fs::File>,
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
    #[cfg(unix)]
    fn from_parts(
        descriptor: SessionDescriptor,
        connection: ClientConnection,
        activity_wakeup: readiness::Readiness,
        worker: Option<std::process::Child>,
        recovery_file: Option<std::fs::File>,
    ) -> Self {
        Self {
            descriptor,
            connection: Mutex::new(connection),
            poisoned: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
            activity_wakeup,
            worker: Mutex::new(worker),
            recovery_file,
        }
    }

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
        let activity_wakeup = readiness::Readiness::new()?;
        let (endpoint_dir, endpoint_dir_identity) = endpoint_directory(session_id)?;
        let endpoint = endpoint_dir.join(SESSION_SOCKET_FILE);

        let mut cleanup = WorkerCleanup {
            child: None,
            endpoint: endpoint.clone(),
            endpoint_dir,
            endpoint_identity: None,
            endpoint_dir_identity,
            recovery: None,
            active: true,
        };
        let (listener, endpoint_identity, _) = bind_endpoint(&endpoint)?;
        cleanup.endpoint_identity = Some(endpoint_identity);
        let listener = listener_for_child(listener)?;
        let listener_fd = listener.as_raw_fd();
        let child = unsafe {
            let mut command = std::process::Command::new(worker_path.as_ref());
            command
                .env_clear()
                .arg("--endpoint")
                .arg(&endpoint)
                .arg("--session-id")
                .arg(session_id.hex())
                .arg("--endpoint-identity")
                .arg(format!("{}:{}", endpoint_identity.0, endpoint_identity.1))
                .arg("--listener-fd")
                .arg(listener_fd.to_string())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                // The worker reports startup and connection failures on its
                // stderr, the only visible side of a failed handshake.
                .stderr(std::process::Stdio::inherit())
                .pre_exec(move || {
                    let flags = libc::fcntl(listener_fd, libc::F_GETFD);
                    if flags == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::fcntl(listener_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC)
                        == -1
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            command.spawn()
        }
        .map_err(|error| {
            SessionError::Io(io::Error::other(format!("start session worker: {error}")))
        })?;
        drop(listener);
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

        let (recovery_path, recovery_file) = descriptor.save_recovery_with_file()?;
        cleanup.recovery = Some(RecoveryCleanup {
            path: recovery_path,
            file: recovery_file,
        });

        let connection = connect_until_ready(&descriptor, Some(spec), true)?;
        let recovery_file = cleanup.recovery.take().map(|recovery| recovery.file);
        let child = cleanup.into_child();

        Ok(Self::from_parts(
            descriptor,
            ClientConnection {
                stream: connection.stream,
                generation: connection.generation,
                ready_pending: false,
                next_request_id: 1,
                initial_frame: Some(connection.initial_frame),
            },
            activity_wakeup,
            Some(child),
            recovery_file,
        ))
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
        let recovery_file = descriptor.open_recovery_file();
        let activity_wakeup = readiness::Readiness::new()?;
        let connection = connect_until_ready(&descriptor, None, true)?;
        Ok(Self::from_parts(
            descriptor,
            ClientConnection {
                stream: connection.stream,
                generation: connection.generation,
                ready_pending: false,
                next_request_id: 1,
                initial_frame: Some(connection.initial_frame),
            },
            activity_wakeup,
            None,
            recovery_file,
        ))
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
        let recovery_file = descriptor.open_recovery_file();
        let connection = connect_until_ready(&descriptor, None, false)?;
        Ok(PreparedSessionAttachment {
            descriptor,
            connection,
            recovery_file,
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

    /// Take the validated frame received during the attach handshake. A
    /// committed local session also emits `FrameReady`, which reconciles this
    /// baseline without an initial snapshot request.
    #[cfg(unix)]
    pub fn take_initial_frame(&self) -> Result<Option<FullFrame>, SessionError> {
        self.connection
            .lock()
            .map(|mut connection| connection.initial_frame.take())
            .map_err(|_| SessionError::protocol("session connection lock poisoned"))
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

    /// Wait until the worker transport or a caller-owned local wakeup is
    /// readable. This method does not consume a wire frame or the local
    /// wakeup; clear the local wakeup before rechecking the caller's queue.
    #[cfg(unix)]
    pub fn wait_for_activity(
        &self,
        wakeup: &readiness::Readiness,
    ) -> Result<(), SessionError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(SessionError::Detached);
        }

        self.activity_wakeup.clear();
        let stream_fd = {
            let mut connection = self.connection.lock().map_err(|_| {
                SessionError::protocol("session connection lock poisoned")
            })?;
            self.ensure_ready(&mut connection)?;
            connection.stream.as_raw_fd()
        };
        let events = self
            .events
            .lock()
            .map_err(|_| SessionError::protocol("session event queue lock poisoned"))?;
        if !events.is_empty() {
            return Ok(());
        }
        drop(events);

        let mut poll_fds = [
            libc::pollfd {
                fd: stream_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wakeup.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.activity_wakeup.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            if let Err(error) = readiness::wait(&mut poll_fds, None) {
                self.poison_connection();
                return Err(error.into());
            }
            if readiness::is_invalid(poll_fds[0].revents) {
                self.poison_connection();
                return Err(SessionError::WorkerExited);
            }
            if readiness::is_invalid(poll_fds[1].revents) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "session activity wakeup is invalid",
                )
                .into());
            }
            if readiness::is_invalid(poll_fds[2].revents) {
                return Err(SessionError::protocol(
                    "session activity queue wakeup is invalid",
                ));
            }
            if poll_fds
                .iter()
                .any(|poll_fd| readiness::is_readable(poll_fd.revents))
            {
                return Ok(());
            }
        }
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
        #[cfg(unix)]
        self.activity_wakeup.clear();
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
            if let Some(event) = self
                .events
                .lock()
                .map_err(|_| SessionError::protocol("session event queue lock poisoned"))?
                .pop_front()
            {
                return Ok(Some(event));
            }
            let mut poll_fd = libc::pollfd {
                fd: std::os::fd::AsRawFd::as_raw_fd(&connection.stream),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = match readiness::wait(
                std::slice::from_mut(&mut poll_fd),
                Some(Instant::now()),
            ) {
                Ok(ready) => ready,
                Err(error) => {
                    self.poison_stream(&connection.stream);
                    return Err(error.into());
                }
            };
            if ready == 0 {
                return Ok(None);
            }
            if readiness::is_invalid(poll_fd.revents) {
                self.poison_stream(&connection.stream);
                return Err(SessionError::WorkerExited);
            }
            if !readiness::is_readable(poll_fd.revents) {
                return Ok(None);
            }
            let result = Self::read_server_message(
                &mut connection.stream,
                Instant::now() + FRAME_TIMEOUT,
            );
            match result {
                Ok(message) => match message {
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
                },
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
        self.lock_worker()
            .ok()?
            .as_ref()
            .map(std::process::Child::id)
    }

    /// Lock the spawned worker child, mapping poisoning to a protocol error.
    #[cfg(unix)]
    fn lock_worker(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<std::process::Child>>, SessionError>
    {
        self.worker
            .lock()
            .map_err(|_| SessionError::protocol("worker lock poisoned"))
    }

    /// Wait for a worker process this client spawned. This never sends a
    /// signal; callers decide whether the worker should still be running.
    #[cfg(unix)]
    pub fn wait_worker(&self) -> Result<Option<std::process::ExitStatus>, SessionError> {
        self.lock_worker()?
            .as_mut()
            .map(std::process::Child::wait)
            .transpose()
            .map_err(Into::into)
    }

    /// Reap the worker if it already exited, without blocking. The session
    /// pump calls this when it goes away so every closed tab does not leak
    /// a zombie until its window exits; a still-running worker (for example
    /// a detached transfer target) is left alone.
    #[cfg(unix)]
    pub fn reap_worker(&self) -> Result<Option<std::process::ExitStatus>, SessionError> {
        self.lock_worker()?
            .as_mut()
            .map(std::process::Child::try_wait)
            .transpose()
            .map_err(SessionError::from)
            .map(|status| status.flatten())
    }

    pub fn close(&self) -> Result<(), SessionError> {
        self.request(SessionCommand::Close, |reply| {
            if matches!(reply, SessionReply::Closed) {
                self.poisoned.store(true, Ordering::Release);
                Some(())
            } else {
                None
            }
        })?;
        #[cfg(unix)]
        if let Some(file) = self.recovery_file.as_ref() {
            self.descriptor.remove_recovery_if_file(file);
        }
        Ok(())
    }

    #[cfg(unix)]
    fn read_server_message(
        stream: &mut std::os::unix::net::UnixStream,
        deadline: Instant,
    ) -> Result<ServerMessage, SessionError> {
        let message: ServerMessage = codec::read_frame_until(stream, deadline)?;
        message.validate()?;
        Ok(message)
    }

    #[cfg(unix)]
    fn ensure_ready(
        &self,
        connection: &mut ClientConnection,
    ) -> Result<(), SessionError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(SessionError::Detached);
        }
        if !connection.ready_pending {
            return Ok(());
        }

        let message: ServerMessage = match Self::read_server_message(
            &mut connection.stream,
            Instant::now() + FRAME_TIMEOUT,
        ) {
            Ok(message) => message,
            Err(error) => {
                self.poison_stream(&connection.stream);
                return Err(error);
            }
        };
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
        let request_id = connection.next_request_id;
        connection.next_request_id = request_id
            .checked_add(1)
            .ok_or_else(|| SessionError::protocol("request id exhausted"))?;

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
            let message: ServerMessage = match Self::read_server_message(
                &mut connection.stream,
                Instant::now() + FRAME_TIMEOUT,
            ) {
                Ok(message) => message,
                Err(error) => {
                    self.poison_stream(&connection.stream);
                    return Err(error);
                }
            };
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
    fn poison_connection(&self) {
        self.poisoned.store(true, Ordering::Release);
        if let Ok(connection) = self.connection.lock() {
            let _ = connection.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    #[cfg(unix)]
    fn queue_event(&self, event: SessionEvent) -> Result<(), SessionError> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| SessionError::protocol("session event queue lock poisoned"))?;
        let queued = queue_event(&mut events, event);
        drop(events);
        if queued {
            self.activity_wakeup.signal();
        }
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
fn endpoint_directory(
    session_id: SessionId,
) -> Result<(PathBuf, FileIdentity), SessionError> {
    use std::os::unix::fs::DirBuilderExt;

    let configured_base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let base = endpoint_base(&configured_base, session_id)?;
    let mut base_builder = std::fs::DirBuilder::new();
    base_builder.recursive(true).mode(0o700).create(&base)?;
    let directory = session_directory(&base, session_id);
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(&directory)?;
    let identity = match private_directory_identity(&directory) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = std::fs::remove_dir(&directory);
            return Err(error);
        }
    };
    Ok((directory, identity))
}

#[cfg(unix)]
fn session_directory(base: &Path, session_id: SessionId) -> PathBuf {
    base.join(format!("rio-session-{}", session_id.hex()))
}

/// Pick the directory that keeps the session socket within `SUN_LEN`.
///
/// `XDG_RUNTIME_DIR` is preferred, but a long base silently produced
/// unusable sockets, so fall back to `/var/tmp` when the path would not fit.
#[cfg(unix)]
fn endpoint_base(
    configured_base: &Path,
    session_id: SessionId,
) -> Result<PathBuf, SessionError> {
    let fits = |base: &Path| {
        unix_socket_path_fits(
            &session_directory(base, session_id).join(SESSION_SOCKET_FILE),
        )
    };

    if fits(configured_base) {
        return Ok(configured_base.to_owned());
    }

    let fallback = Path::new("/var/tmp");
    if fits(fallback) {
        tracing::warn!(
            "session runtime directory is too long for a Unix socket; using a short private directory"
        );
        return Ok(fallback.to_owned());
    }

    Err(SessionError::invalid(
        "no usable Unix socket directory is available",
    ))
}

#[cfg(unix)]
fn unix_socket_path_fits(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let capacity = std::mem::size_of::<libc::sockaddr_un>()
        - std::mem::offset_of!(libc::sockaddr_un, sun_path);
    let bytes = path.as_os_str().as_bytes();
    !bytes.contains(&0) && bytes.len() < capacity
}

#[cfg(unix)]
fn path_identity(path: &Path) -> io::Result<FileIdentity> {
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn private_directory_identity(path: &Path) -> Result<FileIdentity, SessionError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(SessionError::protocol(
            "session endpoint directory is not private",
        ));
    }
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn is_private_file(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
}

#[cfg(unix)]
fn private_endpoint_identity(
    path: &Path,
    expected: FileIdentity,
) -> Result<FileIdentity, SessionError> {
    use std::os::unix::fs::FileTypeExt;

    let metadata = std::fs::symlink_metadata(path)?;
    let identity = (metadata.dev(), metadata.ino());
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || identity != expected
    {
        return Err(SessionError::protocol(
            "session endpoint is not a private socket",
        ));
    }
    Ok(identity)
}

#[cfg(unix)]
fn remove_file_if_identity(path: &Path, identity: FileIdentity) -> bool {
    path_identity(path).is_ok_and(|current| current == identity)
        && std::fs::remove_file(path).is_ok()
}

#[cfg(unix)]
fn endpoint_is_absent(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

#[cfg(unix)]
fn cleanup_endpoint_if_owned(
    endpoint: &Path,
    endpoint_identity: Option<FileIdentity>,
    endpoint_dir: &Path,
    endpoint_dir_identity: FileIdentity,
) {
    let removed_endpoint = endpoint_identity
        .is_some_and(|identity| remove_file_if_identity(endpoint, identity));
    if (removed_endpoint || endpoint_is_absent(endpoint))
        && path_identity(endpoint_dir).ok() == Some(endpoint_dir_identity)
    {
        let _ = std::fs::remove_dir(endpoint_dir);
    }
}

#[cfg(unix)]
fn cleanup_listener(endpoint: &Path, listener: UnixListener, identity: FileIdentity) {
    drop(listener);
    let _ = remove_file_if_identity(endpoint, identity);
}

#[cfg(unix)]
fn listener_for_child(listener: UnixListener) -> Result<UnixListener, SessionError> {
    if listener.as_raw_fd() > 2 {
        return Ok(listener);
    }
    let fd = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd == -1 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { UnixListener::from_raw_fd(fd) })
}

#[cfg(unix)]
fn bind_endpoint(endpoint: &Path) -> Result<EndpointBinding, SessionError> {
    use std::os::unix::fs::PermissionsExt;

    let parent = endpoint.parent().ok_or_else(|| {
        SessionError::protocol("session endpoint has no parent directory")
    })?;
    let parent_identity = private_directory_identity(parent)?;

    let listener = UnixListener::bind(endpoint)?;
    let endpoint_identity = path_identity(endpoint)?;
    if let Err(error) =
        std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
    {
        cleanup_listener(endpoint, listener, endpoint_identity);
        return Err(error.into());
    }
    if let Err(error) = listener.set_nonblocking(true) {
        cleanup_listener(endpoint, listener, endpoint_identity);
        return Err(error.into());
    }

    let endpoint_identity = match private_endpoint_identity(endpoint, endpoint_identity) {
        Ok(identity) => identity,
        Err(error) => {
            cleanup_listener(endpoint, listener, endpoint_identity);
            return Err(error);
        }
    };
    Ok((listener, endpoint_identity, parent_identity))
}

#[cfg(unix)]
fn connect_with_deadline(
    endpoint: &Path,
    deadline: Instant,
) -> Result<std::os::unix::net::UnixStream, SessionError> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;

    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "session connection deadline exceeded",
        )
        .into());
    }
    let path = endpoint.as_os_str().as_bytes();
    let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    if path.contains(&0) {
        return Err(SessionError::invalid("session endpoint contains NUL"));
    }
    if path.len() >= address.sun_path.len() {
        return Err(SessionError::invalid(
            "session endpoint is too long for Unix sockets",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (destination, source) in address.sun_path.iter_mut().zip(path.iter().copied()) {
        *destination = source as libc::c_char;
    }
    let address_length = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(path.len() + 1)
        .ok_or_else(|| SessionError::invalid("session endpoint address is too long"))?;
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        address.sun_len = u8::try_from(address_length)
            .map_err(|_| SessionError::invalid("session endpoint address is too long"))?;
    }

    #[cfg(target_os = "linux")]
    let fd = {
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if fd != -1 {
            fd
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINVAL) {
                return Err(error.into());
            }
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) }
        }
    };
    #[cfg(not(target_os = "linux"))]
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error().into());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error().into());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    stream.set_nonblocking(true)?;
    let address = &address as *const libc::sockaddr_un as *const libc::sockaddr;
    let address_length = libc::socklen_t::try_from(address_length)
        .map_err(|_| SessionError::invalid("session endpoint address is too long"))?;
    loop {
        let result = unsafe { libc::connect(fd, address, address_length) };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        let Some(code) = error.raw_os_error() else {
            return Err(error.into());
        };
        if code == libc::EISCONN {
            break;
        }
        if code == libc::EINTR {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "session connection deadline exceeded",
                )
                .into());
            }
            continue;
        }
        if !matches!(code, libc::EINPROGRESS | libc::EALREADY | libc::EAGAIN) {
            return Err(error.into());
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        if readiness::wait(std::slice::from_mut(&mut poll_fd), Some(deadline))? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "session connection deadline exceeded",
            )
            .into());
        }
        if readiness::is_invalid(poll_fd.revents) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session connection fd is invalid",
            )
            .into());
        }
        if poll_fd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) == 0 {
            continue;
        }
        let mut socket_error = 0;
        let mut option_length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut libc::c_int).cast(),
                &mut option_length,
            )
        } == -1
        {
            return Err(io::Error::last_os_error().into());
        }
        if socket_error != 0 {
            return Err(io::Error::from_raw_os_error(socket_error).into());
        }
        break;
    }
    // Frame deadlines poll the socket, so it must never block a transfer.
    stream.set_nonblocking(true)?;
    Ok(stream)
}

#[cfg(unix)]
fn connect_until_ready(
    descriptor: &SessionDescriptor,
    spec: Option<SessionSpec>,
    commit: bool,
) -> Result<ConnectedSession, SessionError> {
    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        capability: descriptor.capability,
        session_id: descriptor.session_id,
        spec,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = connect_with_deadline(&descriptor.endpoint, deadline)
        .map_err(|error| codec::step_error("connect", error))?;
    codec::write_frame_until(
        &mut stream,
        &hello,
        deadline.min(Instant::now() + FRAME_TIMEOUT),
    )
    .map_err(|error| codec::step_error("send hello", error))?;
    let mut offered_generation = None;
    let mut claimed_generation = None;
    let mut initial_frame = None;
    loop {
        let message: ServerMessage = codec::read_frame_until(
            &mut stream,
            deadline.min(Instant::now() + FRAME_TIMEOUT),
        )
        .map_err(|error| codec::step_error("read handshake frame", error))?;
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
                    || claimed_generation.is_some_and(|claimed| claimed != generation)
                {
                    return Err(SessionError::protocol(
                        "worker returned an incompatible identity",
                    ));
                }
                let initial_frame = initial_frame.take().ok_or_else(|| {
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
                if offered_generation.is_some() || claimed_generation.is_some() {
                    return Err(SessionError::protocol(
                        "worker sent an offer out of order",
                    ));
                }
                offered_generation = Some(generation);
                codec::write_frame_until(
                    &mut stream,
                    &ClientMessage::Claim { generation },
                    deadline.min(Instant::now() + FRAME_TIMEOUT),
                )
                .map_err(|error| codec::step_error("send claim", error))?;
            }
            ServerMessage::Claimed { generation } => {
                if generation == 0 {
                    return Err(SessionError::protocol(
                        "worker returned an invalid attachment generation",
                    ));
                }
                if offered_generation.is_some_and(|offered| generation <= offered) {
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
                    )
                    .map_err(|error| codec::step_error("read initial frame", error))?;
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
                            if !matches!(event, protocol::SessionEvent::FrameReady) {
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
                )
                .map_err(|error| codec::step_error("send commit", error))?;
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
                return Err(SessionError::protocol(format!("{code:?}: {message}")));
            }
            ServerMessage::Detached => {
                return Err(SessionError::Detached);
            }
            _ => return Err(SessionError::protocol("unexpected handshake message")),
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
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    #[test]
    fn focus_no_change_is_accepted_by_the_pump_classifier() {
        assert!(matches!(
            ReplyKind::for_command(&SessionCommand::Focus { focused: true }),
            ReplyKind::AcceptedOrNoChange
        ));
    }

    #[test]
    fn serialized_operations_reject_a_poisoned_client_after_waiting() {
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
                next_request_id: 1,
                initial_frame: None,
            }),
            poisoned: AtomicBool::new(true),
            events: Mutex::new(VecDeque::new()),
            activity_wakeup: super::readiness::Readiness::new().unwrap(),
            worker: Mutex::new(None),
            recovery_file: None,
        };

        let mut connection = client.connection.lock().unwrap();
        assert!(matches!(
            client.ensure_ready(&mut connection),
            Err(crate::SessionError::Detached)
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
                next_request_id: 1,
                initial_frame: None,
            }),
            poisoned: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
            activity_wakeup: super::readiness::Readiness::new().unwrap(),
            worker: Mutex::new(None),
            recovery_file: None,
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

    #[test]
    fn child_listener_is_moved_above_stdio_fds() {
        if std::env::var_os("RIO_LISTENER_FD_TEST_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("child_listener_is_moved_above_stdio_fds")
                .env("RIO_LISTENER_FD_TEST_CHILD", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let path = std::env::temp_dir()
            .join(format!("rio-session-listener-fd-{}", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        let source_fd =
            unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 20) };
        assert!(source_fd >= 0);
        drop(listener);

        struct RestoreStdin(i32);

        impl Drop for RestoreStdin {
            fn drop(&mut self) {
                if self.0 >= 0 {
                    unsafe {
                        assert!(libc::dup2(self.0, 0) >= 0);
                        libc::close(self.0);
                    }
                } else {
                    unsafe {
                        libc::close(0);
                    }
                }
            }
        }

        let saved_stdin = unsafe { libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 20) };
        let _restore_stdin = RestoreStdin(saved_stdin);
        assert!(unsafe { libc::dup2(source_fd, 0) } >= 0);
        unsafe {
            libc::close(source_fd);
        }
        let low_listener = unsafe { UnixListener::from_raw_fd(0) };
        let child_listener = super::listener_for_child(low_listener).unwrap();
        assert!(child_listener.as_raw_fd() > 2);
        drop(child_listener);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn long_runtime_directory_uses_a_short_socket_base() {
        let configured = PathBuf::from(format!("/{}", "runtime".repeat(20)));
        assert_eq!(
            super::endpoint_base(&configured, crate::protocol::SessionId([7; 16]))
                .unwrap(),
            PathBuf::from("/var/tmp")
        );
    }

    #[test]
    fn worker_cleanup_removes_empty_directory_after_worker_removes_socket() {
        let directory = std::env::temp_dir()
            .join(format!("rio-session-worker-cleanup-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let endpoint = directory.join(super::SESSION_SOCKET_FILE);
        let listener = UnixListener::bind(&endpoint).unwrap();
        let endpoint_identity = super::path_identity(&endpoint).unwrap();
        let endpoint_dir_identity = super::path_identity(&directory).unwrap();
        drop(listener);
        std::fs::remove_file(&endpoint).unwrap();

        drop(super::WorkerCleanup {
            child: None,
            endpoint,
            endpoint_dir: directory.clone(),
            endpoint_identity: Some(endpoint_identity),
            endpoint_dir_identity,
            recovery: None,
            active: true,
        });

        assert!(!directory.exists());
    }
}
