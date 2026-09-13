//! The standalone per-terminal worker.
//!
//! The worker deliberately has no windowing or renderer dependency.  It owns
//! the PTY, parser, grid, scrollback, and graphics state for exactly one
//! session.  Unix is the first transport; the protocol itself is platform
//! neutral so a named-pipe transport can be added without changing it.

#[cfg(unix)]
mod unix {
    use super::super::snapshot::Snapshotter;
    use crate::codec;
    use crate::protocol::{
        ClientMessage, EnvVar, ErrorCode, KeyAction as WireKeyAction, KeyCode, KeyInput,
        RequestKind, RequestRefusalReason, SearchDirection, SearchMatch, SelectionKind,
        SelectionSide, ServerMessage, SessionCommand, SessionEvent, SessionId,
        SessionReply, SessionSpec, MAX_PENDING_INPUT_BYTES, MAX_PENDING_REQUESTS,
        MAX_SELECTION_LINES, PROTOCOL_VERSION,
    };
    use crate::readiness;
    use crate::{
        cleanup_endpoint_if_owned, cleanup_listener, private_directory_identity,
        queue_event, terminal_request_event_id, SessionError,
        PREPARED_ATTACHMENT_TIMEOUT,
    };
    use librio::{
        Action, Engine, SelectionKind as RioSelectionKind, Side, Surface,
        SurfaceDelegate, SurfaceDesc,
    };
    use rio_vt::crosswords::pos::{Column as PosColumn, Direction, Line, Pos};
    use rio_vt::crosswords::vi_mode::ViMotion as RioViMotion;
    use std::collections::VecDeque;
    #[cfg(test)]
    use std::fs;
    use std::io::{self, Read};
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, RawFd};
    #[cfg(test)]
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    #[cfg(test)]
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
    const CONNECTION_TIMEOUT: Duration = Duration::from_millis(250);
    const IDLE_RETENTION: Duration = Duration::from_secs(300);
    const MAX_CONNECTIONS: usize = 8;
    const TERMINAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn release_request_slot(slots: &AtomicUsize) {
        let previous = slots.fetch_sub(1, Ordering::Relaxed);
        assert!(previous > 0, "terminal request budget underflow");
    }

    enum TerminalRequest {
        ClipboardLoad {
            request_id: u64,
            route_id: usize,
            kind: librio::ClipboardType,
            format: Arc<dyn Fn(&str) -> String + Send + Sync>,
        },
        ColorRequest {
            request_id: u64,
            route_id: usize,
            index: usize,
            format: Arc<dyn Fn(librio::ColorRgb) -> String + Send + Sync>,
        },
        TextAreaSizeRequest {
            request_id: u64,
            route_id: usize,
            format: Arc<dyn Fn(rio_vt::event::WindowSize) -> String + Send + Sync>,
        },
        GlyphProtocolQuery {
            request_id: u64,
            route_id: usize,
            cp: u32,
        },
    }

    enum TerminalReply {
        Request(TerminalRequest),
        PtyWrite(librio::PtyWrite),
    }

    impl TerminalRequest {
        fn id(&self) -> u64 {
            match self {
                Self::ClipboardLoad { request_id, .. }
                | Self::ColorRequest { request_id, .. }
                | Self::TextAreaSizeRequest { request_id, .. }
                | Self::GlyphProtocolQuery { request_id, .. } => *request_id,
            }
        }

        fn kind(&self) -> RequestKind {
            match self {
                Self::ClipboardLoad { .. } => RequestKind::ClipboardLoad,
                Self::ColorRequest { .. } => RequestKind::ColorRequest,
                Self::TextAreaSizeRequest { .. } => RequestKind::TextAreaSizeRequest,
                Self::GlyphProtocolQuery { .. } => RequestKind::GlyphProtocolQuery,
            }
        }

        fn route_id(&self) -> usize {
            match self {
                Self::ClipboardLoad { route_id, .. }
                | Self::ColorRequest { route_id, .. }
                | Self::TextAreaSizeRequest { route_id, .. }
                | Self::GlyphProtocolQuery { route_id, .. } => *route_id,
            }
        }
    }

    enum PendingReply {
        Request {
            request: TerminalRequest,
            expires_at: Instant,
            response: Option<librio::PtyWrite>,
        },
        PtyWrite(librio::PtyWrite),
    }

    fn terminal_request_event(request: &TerminalRequest) -> Option<SessionEvent> {
        let request_id = request.id();
        let protocol_route_id = u64::try_from(request.route_id()).ok()?;
        match request {
            TerminalRequest::ClipboardLoad { kind, .. } => {
                Some(SessionEvent::ClipboardLoad {
                    request_id,
                    route_id: protocol_route_id,
                    kind: *kind as u8,
                })
            }
            TerminalRequest::ColorRequest { index, .. } => {
                Some(SessionEvent::ColorRequest {
                    request_id,
                    route_id: protocol_route_id,
                    index: (*index).try_into().ok()?,
                })
            }
            TerminalRequest::TextAreaSizeRequest { .. } => {
                Some(SessionEvent::TextAreaSizeRequest {
                    request_id,
                    route_id: protocol_route_id,
                })
            }
            TerminalRequest::GlyphProtocolQuery { cp, .. } => {
                Some(SessionEvent::GlyphProtocolQuery {
                    request_id,
                    route_id: protocol_route_id,
                    codepoint: *cp,
                })
            }
        }
    }

    enum Notification {
        Action(Action),
        ClipboardStore {
            kind: librio::ClipboardType,
            text: String,
        },
        ClipboardOverflow,
        Closed,
        ChildExited(Option<i32>),
        Desktop {
            title: String,
            body: String,
        },
        ColorChange {
            route_id: usize,
            index: usize,
            color: Option<librio::ColorRgb>,
        },
        RequestRefused {
            request_id: u64,
            kind: RequestKind,
            reason: RequestRefusalReason,
        },
    }

    impl Notification {
        fn is_critical(&self) -> bool {
            matches!(
                self,
                Self::Closed | Self::ChildExited(_) | Self::RequestRefused { .. }
            )
        }
    }

    #[derive(Default)]
    struct DeferredNotifications {
        queued: VecDeque<Notification>,
        title: Option<String>,
        progress: Option<(u8, u8)>,
        bell: bool,
        cursor_blinking: bool,
        clipboard_store: [Option<String>; 2],
        clipboard_overflow: bool,
        child_exited: Option<Option<i32>>,
        closed: bool,
        terminal_replies: VecDeque<TerminalReply>,
        request_refused: VecDeque<(u64, RequestKind, RequestRefusalReason)>,
        desktop_notifications: VecDeque<(String, String)>,
        color_changes: VecDeque<(usize, usize, Option<librio::ColorRgb>)>,
    }

    impl DeferredNotifications {
        fn has_deferred_notifications(&self) -> bool {
            self.title.is_some()
                || self.progress.is_some()
                || self.bell
                || self.cursor_blinking
                || self.clipboard_store.iter().any(Option::is_some)
                || self.clipboard_overflow
                || self.child_exited.is_some()
                || self.closed
                || !self.request_refused.is_empty()
                || !self.desktop_notifications.is_empty()
                || !self.color_changes.is_empty()
        }

        fn drain_into(
            &mut self,
            critical_notifications: &mut Vec<Notification>,
            notifications: &mut Vec<Notification>,
        ) {
            if let Some(title) = self.title.take() {
                notifications.push(Notification::Action(Action::SetTitle {
                    title,
                    subtitle: None,
                }));
            }
            if let Some((state, value)) = self.progress.take() {
                notifications
                    .push(Notification::Action(Action::Progress { state, value }));
            }
            if self.bell {
                self.bell = false;
                notifications.push(Notification::Action(Action::RingBell));
            }
            if self.cursor_blinking {
                self.cursor_blinking = false;
                notifications.push(Notification::Action(Action::CursorBlinkingChange));
            }
            for (kind, pending) in [
                librio::ClipboardType::Clipboard,
                librio::ClipboardType::Selection,
            ]
            .into_iter()
            .zip(&mut self.clipboard_store)
            {
                if let Some(text) = pending.take() {
                    notifications.push(Notification::ClipboardStore { kind, text });
                }
            }
            if self.clipboard_overflow {
                self.clipboard_overflow = false;
                notifications.push(Notification::ClipboardOverflow);
            }
            if let Some(status) = self.child_exited.take() {
                critical_notifications.push(Notification::ChildExited(status));
            }
            if self.closed {
                self.closed = false;
                critical_notifications.push(Notification::Closed);
            }
            critical_notifications.extend(self.request_refused.drain(..).map(
                |(request_id, kind, reason)| Notification::RequestRefused {
                    request_id,
                    kind,
                    reason,
                },
            ));
            notifications.extend(
                self.desktop_notifications
                    .drain(..)
                    .map(|(title, body)| Notification::Desktop { title, body }),
            );
            notifications.extend(self.color_changes.drain(..).map(
                |(route_id, index, color)| Notification::ColorChange {
                    route_id,
                    index,
                    color,
                },
            ));
        }
    }

    struct Delegate {
        wakeup_pending: AtomicBool,
        wakeup: readiness::Readiness,
        deferred: Mutex<DeferredNotifications>,
        next_request_id: AtomicU64,
        pending_requests: Arc<AtomicUsize>,
    }

    impl Delegate {
        fn allocate_request(&self) -> Option<u64> {
            let reserved = self.pending_requests.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |current| (current < MAX_PENDING_REQUESTS).then_some(current + 1),
            );
            if reserved.is_err() {
                return None;
            }
            match self
                .next_request_id
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    current.checked_add(1)
                })
                .ok()
            {
                Some(request_id) => Some(request_id),
                None => {
                    self.release_request();
                    None
                }
            }
        }

        fn fresh_request_id(&self) -> Option<u64> {
            self.next_request_id
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    current.checked_add(1)
                })
                .ok()
        }

        fn release_request(&self) {
            release_request_slot(&self.pending_requests);
        }

        fn defer_request_refusal(
            deferred: &mut DeferredNotifications,
            request_id: u64,
            kind: RequestKind,
            reason: RequestRefusalReason,
        ) {
            if deferred.request_refused.len() < MAX_PENDING_REQUESTS {
                deferred
                    .request_refused
                    .push_back((request_id, kind, reason));
            }
        }

        fn request_refused(
            &self,
            request_id: u64,
            kind: RequestKind,
            reason: RequestRefusalReason,
        ) {
            self.send(Notification::RequestRefused {
                request_id,
                kind,
                reason,
            });
        }

        fn wake(&self) {
            if !self.wakeup_pending.swap(true, Ordering::Relaxed) {
                self.wakeup.signal();
            }
        }

        fn enqueue_terminal_reply(
            &self,
            reply: TerminalReply,
        ) -> Result<(), librio::InputError> {
            let mut deferred = self
                .deferred
                .lock()
                .map_err(|_| librio::InputError::Disconnected)?;
            if deferred.terminal_replies.len() >= MAX_PENDING_REQUESTS {
                return Err(librio::InputError::WouldBlock);
            }
            deferred.terminal_replies.push_back(reply);
            drop(deferred);
            self.wake();
            Ok(())
        }

        fn send(&self, notification: Notification) {
            if let Ok(mut deferred) = self.deferred.lock() {
                if !deferred.has_deferred_notifications()
                    && deferred.queued.len() < MAX_PENDING_REQUESTS
                {
                    deferred.queued.push_back(notification);
                } else {
                    Self::defer_notification(&mut deferred, notification);
                }
            }
            self.wake();
        }

        fn defer_notification(
            deferred: &mut DeferredNotifications,
            notification: Notification,
        ) {
            match notification {
                Notification::Action(Action::SetTitle { title, .. }) => {
                    deferred.title = Some(title);
                }
                Notification::Action(Action::Progress { state, value }) => {
                    deferred.progress = Some((state, value));
                }
                Notification::Action(Action::RingBell) => deferred.bell = true,
                Notification::Action(Action::CursorBlinkingChange) => {
                    deferred.cursor_blinking = true
                }
                Notification::ClipboardStore { kind, text } => {
                    deferred.clipboard_store[kind as usize] = Some(text);
                }
                Notification::ClipboardOverflow => deferred.clipboard_overflow = true,
                Notification::Closed => deferred.closed = true,
                Notification::ChildExited(status) => deferred.child_exited = Some(status),
                Notification::Desktop { title, body } => {
                    if deferred.desktop_notifications.len() < MAX_PENDING_REQUESTS {
                        deferred.desktop_notifications.push_back((title, body));
                    }
                }
                Notification::ColorChange {
                    route_id,
                    index,
                    color,
                } => {
                    if let Some(change) = deferred.color_changes.iter_mut().find(
                        |(route, pending_index, _)| {
                            *route == route_id && *pending_index == index
                        },
                    ) {
                        change.2 = color;
                    } else if deferred.color_changes.len() < MAX_PENDING_REQUESTS {
                        deferred.color_changes.push_back((route_id, index, color));
                    }
                }
                Notification::RequestRefused {
                    request_id,
                    kind,
                    reason,
                } => Self::defer_request_refusal(deferred, request_id, kind, reason),
            }
        }

        fn send_request(
            &self,
            kind: RequestKind,
            make: impl FnOnce(u64) -> TerminalRequest,
        ) {
            let Some(request_id) = self.allocate_request() else {
                let Some(request_id) = self.fresh_request_id() else {
                    return;
                };
                self.request_refused(request_id, kind, RequestRefusalReason::Capacity);
                return;
            };
            if self
                .enqueue_terminal_reply(TerminalReply::Request(make(request_id)))
                .is_err()
            {
                self.release_request();
                self.request_refused(request_id, kind, RequestRefusalReason::Capacity);
            }
        }
    }

    impl SurfaceDelegate for Delegate {
        fn wakeup(&self, _surface: librio::SurfaceId) {
            self.wake();
        }

        fn action(&self, _surface: librio::SurfaceId, action: Action) {
            if let Action::SetTitle { title, .. } = &action {
                if title.len() > crate::protocol::MAX_STRING_BYTES {
                    return;
                }
            }
            self.send(Notification::Action(action));
        }

        fn clipboard_write(
            &self,
            _surface: librio::SurfaceId,
            kind: librio::ClipboardType,
            text: String,
        ) {
            if text.len() > crate::protocol::MAX_STRING_BYTES {
                self.send(Notification::ClipboardOverflow);
                return;
            }
            self.send(Notification::ClipboardStore { kind, text });
        }

        fn clipboard_load(
            &self,
            _surface: librio::SurfaceId,
            route_id: librio::SurfaceId,
            kind: librio::ClipboardType,
            format: Arc<dyn Fn(&str) -> String + Send + Sync>,
        ) {
            self.send_request(RequestKind::ClipboardLoad, |request_id| {
                TerminalRequest::ClipboardLoad {
                    request_id,
                    route_id,
                    kind,
                    format,
                }
            });
        }

        fn color_request(
            &self,
            _surface: librio::SurfaceId,
            route_id: librio::SurfaceId,
            index: usize,
            format: Arc<dyn Fn(librio::ColorRgb) -> String + Send + Sync>,
        ) {
            self.send_request(RequestKind::ColorRequest, |request_id| {
                TerminalRequest::ColorRequest {
                    request_id,
                    route_id,
                    index,
                    format,
                }
            });
        }

        fn text_area_size_request(
            &self,
            _surface: librio::SurfaceId,
            route_id: librio::SurfaceId,
            format: Arc<dyn Fn(rio_vt::event::WindowSize) -> String + Send + Sync>,
        ) {
            self.send_request(RequestKind::TextAreaSizeRequest, |request_id| {
                TerminalRequest::TextAreaSizeRequest {
                    request_id,
                    route_id,
                    format,
                }
            });
        }

        fn glyph_protocol_query(
            &self,
            _surface: librio::SurfaceId,
            route_id: librio::SurfaceId,
            cp: u32,
        ) {
            self.send_request(RequestKind::GlyphProtocolQuery, |request_id| {
                TerminalRequest::GlyphProtocolQuery {
                    request_id,
                    route_id,
                    cp,
                }
            });
        }

        fn desktop_notification(
            &self,
            _surface: librio::SurfaceId,
            title: String,
            body: String,
        ) {
            if title.len() <= crate::protocol::MAX_STRING_BYTES
                && body.len() <= crate::protocol::MAX_STRING_BYTES
            {
                self.send(Notification::Desktop { title, body });
            }
        }

        fn color_change(
            &self,
            _surface: librio::SurfaceId,
            route_id: librio::SurfaceId,
            index: usize,
            color: Option<librio::ColorRgb>,
        ) {
            self.send(Notification::ColorChange {
                route_id,
                index,
                color,
            });
        }

        fn close_surface(&self, _surface: librio::SurfaceId) {
            self.send(Notification::Closed);
        }

        fn child_exited(&self, _surface: librio::SurfaceId, status: Option<i32>) {
            self.send(Notification::ChildExited(status.map(exit_code)));
        }

        fn pty_write(
            &self,
            _surface: librio::SurfaceId,
            write: librio::PtyWrite,
        ) -> Result<librio::PtyWriteResult, librio::InputError> {
            if self.allocate_request().is_none() {
                return Err(librio::InputError::WouldBlock);
            }
            if let Err(error) =
                self.enqueue_terminal_reply(TerminalReply::PtyWrite(write))
            {
                self.release_request();
                return Err(error);
            }
            Ok(librio::PtyWriteResult::Handled)
        }
    }

    fn exit_code(status: i32) -> i32 {
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            status
        }
    }

    struct Runtime {
        surface: Surface,
        snapshots: Snapshotter,
        pending_events: VecDeque<SessionEvent>,
        pending_replies: VecDeque<PendingReply>,
        pending_request_slots: Arc<AtomicUsize>,
        frame_pending: bool,
        selection_anchor: Option<i32>,
        selection_endpoint: Option<i32>,
        child_exit_status: Option<Option<i32>>,
        terminal_closed: bool,
        search: Option<SearchSession>,
    }

    struct SearchSession {
        regex: rio_vt::crosswords::search::RegexSearch,
        origin: rio_vt::crosswords::pos::Pos,
        origin_display_offset: usize,
        origin_cursor: (i32, u16),
        origin_vi_mode: bool,
        direction: SearchDirection,
        side: SelectionSide,
        max_lines: Option<usize>,
        current: Option<(rio_vt::crosswords::pos::Pos, rio_vt::crosswords::pos::Pos)>,
    }

    impl Runtime {
        fn new(spec: SessionSpec, delegate: Arc<Delegate>) -> Result<Self, SessionError> {
            if let Some(working_dir) = spec.working_dir.as_deref() {
                let path = Path::new(working_dir);
                if !path.is_absolute() || !path.is_dir() {
                    return Err(SessionError::invalid(
                        "session working directory must be an existing absolute directory",
                    ));
                }
            }
            let SessionSpec {
                shell,
                args,
                working_dir,
                environment,
                columns,
                lines,
                pixel_width,
                pixel_height,
                scrollback,
                grapheme_clustering,
            } = spec;
            let environment = environment
                .into_iter()
                .map(EnvVar::into_utf8)
                .collect::<Result<Vec<_>, _>>()?;
            let pending_request_slots = Arc::clone(&delegate.pending_requests);
            let surface = Engine::new(delegate)
                .create_surface(&SurfaceDesc {
                    shell,
                    args,
                    working_dir,
                    cols: columns,
                    rows: lines,
                    pixel_width,
                    pixel_height,
                    scrollback,
                    environment: Some(environment),
                    clear_environment: true,
                    input_queue_limit: Some(MAX_PENDING_INPUT_BYTES),
                })
                .map_err(|error| {
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|error| error.kind() == io::ErrorKind::Unsupported)
                    {
                        return SessionError::unsupported(error.to_string());
                    }
                    SessionError::protocol(format!("create session PTY: {error}"))
                })?;
            surface.set_grapheme_clustering(grapheme_clustering);
            Ok(Self {
                snapshots: Snapshotter::new(&surface),
                surface,
                pending_events: VecDeque::new(),
                frame_pending: true,
                selection_anchor: None,
                selection_endpoint: None,
                child_exit_status: None,
                terminal_closed: false,
                pending_replies: VecDeque::new(),
                pending_request_slots,
                search: None,
            })
        }

        fn validate_command(
            &self,
            command: &SessionCommand,
        ) -> Result<Option<i32>, SessionError> {
            match command {
                SessionCommand::MouseWheel { column, line, .. }
                | SessionCommand::MouseButton { column, line, .. }
                | SessionCommand::MouseMotion { column, line, .. } => {
                    self.validate_pointer(*column, *line)?;
                }
                SessionCommand::SelectionBegin { line, column, .. } => {
                    return self.validate_selection(*line, *column).map(Some);
                }
                SessionCommand::SelectionUpdate { line, column, .. } => {
                    let line = self.validate_selection(*line, *column)?;
                    if let Some(anchor) = self.selection_anchor {
                        if anchor.abs_diff(line) > MAX_SELECTION_LINES as u32 {
                            return Err(SessionError::invalid(
                                "selection range exceeds the session limit",
                            ));
                        }
                    }
                    return Ok(Some(line));
                }
                _ => return Ok(None),
            }
            Ok(None)
        }

        fn pending_request(
            &mut self,
            request_id: u64,
            kind: RequestKind,
            route_id: u64,
        ) -> Result<usize, SessionError> {
            let Some(index) = self.pending_replies.iter().position(|reply| {
                matches!(
                        reply,
                        PendingReply::Request { request, .. }
                            if request.id() == request_id
                )
            }) else {
                return Err(SessionError::invalid(
                    "terminal request is unknown or expired",
                ));
            };
            let PendingReply::Request {
                request,
                expires_at,
                ..
            } = &self.pending_replies[index]
            else {
                unreachable!("pending reply kind was checked")
            };
            if request.kind() != kind {
                return Err(SessionError::invalid(
                    "terminal response does not match its request",
                ));
            }
            let expected_route = u64::try_from(request.route_id()).map_err(|_| {
                SessionError::invalid("terminal route id exceeds protocol range")
            })?;
            if expected_route != route_id {
                return Err(SessionError::invalid(
                    "terminal response does not match its route",
                ));
            }
            if *expires_at <= Instant::now() {
                let Some(PendingReply::Request { .. }) =
                    self.pending_replies.remove(index)
                else {
                    unreachable!("expired request barrier was missing")
                };
                release_request_slot(&self.pending_request_slots);
                remove_terminal_request_event(&mut self.pending_events, request_id);
                queue_event(
                    &mut self.pending_events,
                    SessionEvent::RequestExpired { request_id, kind },
                );
                return Err(SessionError::invalid("terminal request has expired"));
            }
            Ok(index)
        }

        fn accept_terminal_response(
            &mut self,
            request_id: u64,
            kind: RequestKind,
            route_id: u64,
            format: impl FnOnce(&TerminalRequest) -> Result<Vec<u8>, SessionError>,
        ) -> Result<SessionReply, SessionError> {
            let pending = self.pending_request(request_id, kind, route_id)?;
            let bytes = {
                let PendingReply::Request {
                    request, response, ..
                } = &self.pending_replies[pending]
                else {
                    unreachable!("pending reply kind was checked")
                };
                if response.is_some() {
                    return Err(SessionError::invalid(
                        "terminal request already has a response",
                    ));
                }
                format(request)?
            };
            let write = self
                .surface
                .try_reserve_response(bytes)
                .map_err(input_error)?;
            let PendingReply::Request { response, .. } =
                &mut self.pending_replies[pending]
            else {
                unreachable!("pending reply kind was checked")
            };
            *response = Some(write);
            self.frame_pending = true;
            Ok(SessionReply::Accepted)
        }

        fn flush_ordered_replies(&mut self) -> Result<bool, SessionError> {
            let mut flushed = false;
            while let Some(reply) = self.pending_replies.front() {
                if matches!(reply, PendingReply::Request { response: None, .. }) {
                    break;
                }
                let write = match self.pending_replies.pop_front().unwrap() {
                    PendingReply::PtyWrite(write)
                    | PendingReply::Request {
                        response: Some(write),
                        ..
                    } => write,
                    PendingReply::Request { response: None, .. } => {
                        unreachable!("unanswered requests stop the reply queue")
                    }
                };
                let result = self.surface.send_pty_write(write);
                release_request_slot(&self.pending_request_slots);
                result.map_err(input_error)?;
                flushed = true;
            }
            Ok(flushed)
        }

        fn response_bytes(text: String) -> Result<Vec<u8>, SessionError> {
            if text.len() > crate::protocol::MAX_STRING_BYTES {
                return Err(SessionError::invalid(
                    "terminal response exceeds the session limit",
                ));
            }
            Ok(text.into_bytes())
        }

        fn search_navigation_reply(
            &self,
            matched: Option<(Pos, Pos)>,
        ) -> Result<SessionReply, SessionError> {
            let history = i32::try_from(self.surface.history_size()).map_err(|_| {
                SessionError::invalid("search history exceeds the session range")
            })?;
            let protocol_position = |position: Pos| -> Result<(u32, u16), SessionError> {
                let line = position
                    .row
                    .0
                    .checked_add(history)
                    .and_then(|line| u32::try_from(line).ok())
                    .ok_or_else(|| {
                        SessionError::invalid("search match is outside the session range")
                    })?;
                let column = u16::try_from(position.col.0).map_err(|_| {
                    SessionError::invalid("search match is outside the session range")
                })?;
                Ok((line, column))
            };
            let matched = match matched {
                Some((start, end)) => {
                    let (start_line, start_column) = protocol_position(start)?;
                    let (end_line, end_column) = protocol_position(end)?;
                    Some(SearchMatch {
                        start_line,
                        start_column,
                        end_line,
                        end_column,
                    })
                }
                None => None,
            };
            let display_offset =
                u32::try_from(self.surface.display_offset()).map_err(|_| {
                    SessionError::invalid(
                        "search display offset exceeds the session range",
                    )
                })?;
            Ok(SessionReply::SearchNavigation(
                crate::protocol::SearchNavigation {
                    matched,
                    display_offset,
                    vi_mode: self.surface.vi_mode(),
                },
            ))
        }

        fn validate_pointer(&self, column: u16, line: u16) -> Result<(), SessionError> {
            if usize::from(column) >= self.surface.columns()
                || usize::from(line) >= self.surface.screen_lines()
            {
                return Err(SessionError::invalid("pointer is outside the terminal"));
            }
            Ok(())
        }

        fn validate_selection(
            &self,
            line: i32,
            column: usize,
        ) -> Result<i32, SessionError> {
            let offset = i32::try_from(self.surface.display_offset()).map_err(|_| {
                SessionError::invalid(
                    "selection display offset exceeds the supported range",
                )
            })?;
            let line = line.checked_sub(offset).ok_or_else(|| {
                SessionError::invalid("selection line is outside the terminal")
            })?;
            self.validate_internal_selection(line, column)?;
            Ok(line)
        }

        fn validate_internal_selection(
            &self,
            line: i32,
            column: usize,
        ) -> Result<(), SessionError> {
            let history = i32::try_from(self.surface.history_size()).map_err(|_| {
                SessionError::invalid("selection history exceeds the supported range")
            })?;
            let bottom = i32::try_from(self.surface.screen_lines()).map_err(|_| {
                SessionError::invalid("terminal screen lines exceed the supported range")
            })?;
            if line < -history || line >= bottom || column >= self.surface.columns() {
                return Err(SessionError::invalid("selection is outside the terminal"));
            }
            Ok(())
        }

        fn execute(
            &mut self,
            command: SessionCommand,
        ) -> Result<SessionReply, SessionError> {
            if let Some(error) = self.surface.take_input_error() {
                return Err(input_error(error));
            }
            let selection_line = self.validate_command(&command)?;
            let reply = match command {
                SessionCommand::Write(bytes) => {
                    self.surface.try_write(bytes).map_err(input_error)?;
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::Paste(text) => {
                    self.surface.try_paste(&text).map_err(input_error)?;
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::Resize {
                    columns,
                    lines,
                    pixel_width,
                    pixel_height,
                } => {
                    self.surface
                        .resize(columns, lines, pixel_width, pixel_height);
                    self.selection_anchor = None;
                    self.selection_endpoint = None;
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::Scroll { delta_lines } => {
                    self.surface.scroll(delta_lines);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::MouseWheel {
                    lines,
                    column,
                    line,
                    modifiers,
                } => {
                    let consumed = self
                        .surface
                        .try_scroll_wheel(
                            lines,
                            column,
                            line,
                            librio::Modifiers::from_bits_retain(modifiers),
                        )
                        .map_err(input_error)?;
                    self.frame_pending = true;
                    if consumed {
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::MouseButton {
                    column,
                    line,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let consumed = self
                        .surface
                        .try_mouse_button(
                            column,
                            line,
                            button,
                            pressed,
                            librio::Modifiers::from_bits_retain(modifiers),
                        )
                        .map_err(input_error)?;
                    self.frame_pending = true;
                    if consumed {
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::MouseMotion {
                    column,
                    line,
                    button,
                    modifiers,
                } => {
                    let consumed = self
                        .surface
                        .try_mouse_motion(
                            column,
                            line,
                            button,
                            librio::Modifiers::from_bits_retain(modifiers),
                        )
                        .map_err(input_error)?;
                    self.frame_pending = true;
                    if consumed {
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::SelectionBegin {
                    line: viewport_line,
                    column,
                    kind,
                    side,
                } => {
                    let internal_line = selection_line.ok_or_else(|| {
                        SessionError::protocol("selection coordinates were not validated")
                    })?;
                    self.surface.selection_begin(
                        viewport_line,
                        column,
                        selection_kind(kind),
                        selection_side(side),
                    );
                    self.selection_anchor = Some(internal_line);
                    self.selection_endpoint = Some(internal_line);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::SelectionUpdate { line, column, side } => {
                    let internal_line = selection_line.ok_or_else(|| {
                        SessionError::protocol("selection coordinates were not validated")
                    })?;
                    self.surface
                        .selection_update(line, column, selection_side(side));
                    self.selection_endpoint = Some(internal_line);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::SelectionClear => {
                    self.surface.selection_clear();
                    self.selection_anchor = None;
                    self.selection_endpoint = None;
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::SelectionText => {
                    if let (Some(anchor), Some(endpoint)) =
                        (self.selection_anchor, self.selection_endpoint)
                    {
                        self.validate_internal_selection(anchor, 0)?;
                        self.validate_internal_selection(endpoint, 0)?;
                        if anchor.abs_diff(endpoint) > MAX_SELECTION_LINES as u32 {
                            return Err(SessionError::invalid(
                                "selection range exceeds the session limit",
                            ));
                        }
                    }
                    let text = self
                        .surface
                        .selection_text_bounded(crate::protocol::MAX_STRING_BYTES)
                        .map_err(|_| {
                            SessionError::invalid(
                                "selection text exceeds the session limit",
                            )
                        })?;
                    SessionReply::SelectionText(text)
                }
                SessionCommand::Search {
                    pattern,
                    max_matches,
                } => {
                    let matches = self
                        .surface
                        .search(&pattern, max_matches)
                        .ok_or_else(|| SessionError::invalid("invalid search pattern"))?
                        .into_iter()
                        .map(|(start_line, start_column, end_line, end_column)| {
                            crate::protocol::SearchMatch {
                                start_line,
                                start_column,
                                end_line,
                                end_column,
                            }
                        })
                        .collect();
                    SessionReply::SearchMatches(matches)
                }
                SessionCommand::ChildPid => {
                    SessionReply::ChildPid(self.surface.child_pid())
                }
                SessionCommand::Snapshot => {
                    let frame = self.snapshots.full_frame(&self.surface)?;
                    SessionReply::Frame(frame)
                }
                SessionCommand::SnapshotSince { base_sequence } => {
                    let update = self
                        .snapshots
                        .snapshot_since(&self.surface, base_sequence)?;
                    SessionReply::FrameUpdate(update)
                }
                SessionCommand::SetAltIsMeta(enabled) => {
                    self.surface.set_alt_is_meta(enabled);
                    SessionReply::Accepted
                }
                SessionCommand::SetCursorStyle { shape, blinking } => {
                    let shape = match shape {
                        0 => librio::CursorShape::Block,
                        1 => librio::CursorShape::Underline,
                        2 => librio::CursorShape::Beam,
                        3 => librio::CursorShape::Hidden,
                        _ => {
                            return Err(SessionError::unsupported(
                                "unsupported cursor shape",
                            ))
                        }
                    };
                    self.surface.set_cursor_style(shape, blinking);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::Close => SessionReply::Closed,
                SessionCommand::Key(input) => {
                    let changed = self
                        .surface
                        .try_key(&key_event(input))
                        .map_err(input_error)?;
                    self.frame_pending = true;
                    if changed {
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::Focus { focused } => {
                    let changed = self.surface.try_focus(focused).map_err(input_error)?;
                    self.frame_pending |= changed;
                    if changed {
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::SelectAll => {
                    self.surface.select_all();
                    let history =
                        i32::try_from(self.surface.history_size()).map_err(|_| {
                            SessionError::invalid(
                                "selection history exceeds the supported range",
                            )
                        })?;
                    self.selection_anchor = Some(-history);
                    self.selection_endpoint = Some(
                        i32::try_from(self.surface.screen_lines()).map_err(|_| {
                            SessionError::invalid(
                                "terminal screen lines exceed the supported range",
                            )
                        })? - 1,
                    );
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::SelectionAutoScroll {
                    delta_lines,
                    line,
                    column,
                    side,
                } => {
                    let changed = self.surface.selection_autoscroll(
                        delta_lines,
                        line,
                        column,
                        selection_side(side),
                    );
                    if changed {
                        let display_offset = i32::try_from(self.surface.display_offset())
                            .map_err(|_| {
                                SessionError::invalid(
                                    "selection display offset exceeds the supported range",
                                )
                            })?;
                        let history = i32::try_from(self.surface.history_size())
                            .map_err(|_| {
                                SessionError::invalid(
                                    "selection history exceeds the supported range",
                                )
                            })?;
                        let endpoint = line
                            .checked_sub(display_offset)
                            .ok_or_else(|| {
                                SessionError::invalid(
                                    "selection endpoint is outside the terminal",
                                )
                            })?
                            .clamp(
                                -history,
                                i32::try_from(self.surface.screen_lines()).map_err(|_| {
                                    SessionError::invalid(
                                        "terminal screen lines exceed the supported range",
                                    )
                                })? - 1,
                            );
                        self.selection_endpoint = Some(endpoint);
                        self.frame_pending = true;
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::SetViMode(enabled) => {
                    if self.search.is_some() && self.surface.vi_mode() != enabled {
                        return Err(SessionError::unsupported(
                            "vi mode cannot change during search navigation",
                        ));
                    }
                    if self.surface.set_vi_mode(enabled) {
                        self.frame_pending = true;
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::ToggleViMode => {
                    if self.search.is_some() {
                        return Err(SessionError::unsupported(
                            "vi mode cannot change during search navigation",
                        ));
                    }
                    self.surface.toggle_vi_mode();
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::ViMotion(motion) => {
                    if !self.surface.vi_mode() {
                        return Err(SessionError::unsupported(
                            "vi motion requires vi mode to be enabled",
                        ));
                    }
                    self.surface.vi_motion(vi_motion(motion));
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::ViScroll { delta_lines } => {
                    if !self.surface.vi_mode() {
                        return Err(SessionError::unsupported(
                            "vi scrolling requires vi mode to be enabled",
                        ));
                    }
                    self.surface.vi_scroll(delta_lines);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::ViGoto { line, column } => {
                    if !self.surface.vi_mode() {
                        return Err(SessionError::unsupported(
                            "vi cursor movement requires vi mode to be enabled",
                        ));
                    }
                    self.validate_internal_selection(line, usize::from(column))?;
                    self.surface.vi_goto(line, usize::from(column));
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::ScrollToPrompt { forward } => {
                    let before = self.surface.display_offset();
                    self.surface.scroll_to_prompt(forward);
                    let changed = before != self.surface.display_offset();
                    self.frame_pending |= changed;
                    if changed {
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::ScrollTop => {
                    let before = self.surface.display_offset();
                    self.surface.scroll_to_top();
                    let changed = before != self.surface.display_offset();
                    self.frame_pending |= changed;
                    if changed {
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::ScrollBottom => {
                    let before = self.surface.display_offset();
                    self.surface.scroll_to_bottom();
                    let changed = before != self.surface.display_offset();
                    self.frame_pending |= changed;
                    if changed {
                        SessionReply::Accepted
                    } else {
                        SessionReply::NoChange
                    }
                }
                SessionCommand::ClearSavedHistory => {
                    self.surface.clear_saved_history();
                    self.selection_anchor = None;
                    self.selection_endpoint = None;
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::SearchBegin {
                    pattern,
                    origin_line,
                    origin_column,
                    origin_display_offset,
                    direction,
                    side,
                    max_lines,
                } => {
                    if self.search.is_some() {
                        return Err(SessionError::invalid(
                            "search navigation is already active",
                        ));
                    }
                    self.validate_internal_selection(
                        origin_line,
                        usize::from(origin_column),
                    )?;
                    let origin = Pos::new(
                        Line(origin_line),
                        PosColumn(usize::from(origin_column)),
                    );
                    let origin_vi_mode = self.surface.vi_mode();
                    let origin_cursor = if origin_vi_mode {
                        self.surface.vi_cursor_position()
                    } else {
                        self.surface.cursor_position()
                    };
                    let origin_display_offset = usize::try_from(origin_display_offset)
                        .map_err(|_| {
                            SessionError::invalid("search display offset is too large")
                        })?;
                    if origin_display_offset > self.surface.history_size() {
                        return Err(SessionError::invalid(
                            "search display offset is outside the terminal",
                        ));
                    }
                    let mut regex = rio_vt::crosswords::search::RegexSearch::new(
                        &pattern,
                    )
                    .map_err(|_| SessionError::invalid("invalid search pattern"))?;
                    let rio_direction = search_direction(direction);
                    let rio_side = selection_side(side);
                    let matched = self.surface.search_next(
                        &mut regex,
                        origin,
                        rio_direction,
                        rio_side,
                        max_lines.map(|lines| lines as usize),
                    );
                    if let Some((start, _)) = matched {
                        if origin_vi_mode {
                            self.surface.vi_goto(start.row.0, start.col.0);
                        } else {
                            self.surface.scroll_to_pos(start.row.0, start.col.0);
                        }
                        self.frame_pending = true;
                    }
                    self.search = Some(SearchSession {
                        regex,
                        origin,
                        origin_display_offset,
                        origin_cursor,
                        origin_vi_mode,
                        direction,
                        side,
                        max_lines: max_lines.map(|lines| lines as usize),
                        current: matched,
                    });
                    self.search_navigation_reply(matched)?
                }
                SessionCommand::SearchNext => {
                    let search = self.search.as_mut().ok_or_else(|| {
                        SessionError::invalid("search navigation has not begun")
                    })?;
                    if self.surface.vi_mode() != search.origin_vi_mode {
                        return Err(SessionError::unsupported(
                            "vi mode changed while search navigation was active",
                        ));
                    }
                    let origin = match search.current {
                        Some((start, end)) => match search.direction {
                            SearchDirection::Forward => end,
                            SearchDirection::Backward => start,
                        },
                        None => search.origin,
                    };
                    let matched = self.surface.search_next(
                        &mut search.regex,
                        origin,
                        search_direction(search.direction),
                        selection_side(search.side),
                        search.max_lines,
                    );
                    search.current = matched;
                    if let Some((start, _)) = matched {
                        if search.origin_vi_mode {
                            self.surface.vi_goto(start.row.0, start.col.0);
                        } else {
                            self.surface.scroll_to_pos(start.row.0, start.col.0);
                        }
                        self.frame_pending = true;
                    }
                    self.search_navigation_reply(matched)?
                }
                SessionCommand::SearchCancel => {
                    let Some(search) = self.search.as_ref() else {
                        return Err(SessionError::invalid(
                            "search navigation has not begun",
                        ));
                    };
                    if self.surface.vi_mode() != search.origin_vi_mode {
                        return Err(SessionError::unsupported(
                            "vi mode changed while search navigation was active",
                        ));
                    }
                    let search = self.search.take().expect("search state was checked");
                    if search.origin_vi_mode {
                        self.surface.vi_goto(
                            search.origin_cursor.0,
                            usize::from(search.origin_cursor.1),
                        );
                        self.surface
                            .restore_display_offset(search.origin_display_offset);
                    } else {
                        if let Some((start, end)) = search.current {
                            let offset = i32::try_from(self.surface.display_offset()).map_err(
                                |_| {
                                    SessionError::invalid(
                                        "selection display offset exceeds the supported range",
                                    )
                                },
                            )?;
                            self.surface.selection_begin(
                                start.row.0 + offset,
                                start.col.0,
                                RioSelectionKind::Simple,
                                Side::Left,
                            );
                            self.surface.selection_update(
                                end.row.0 + offset,
                                end.col.0,
                                Side::Right,
                            );
                            self.selection_anchor = Some(start.row.0);
                            self.selection_endpoint = Some(end.row.0);
                        }
                    }
                    self.frame_pending = true;
                    self.search_navigation_reply(None)?
                }
                SessionCommand::ClipboardResponse {
                    request_id,
                    route_id,
                    text,
                } => self.accept_terminal_response(
                    request_id,
                    RequestKind::ClipboardLoad,
                    route_id,
                    |request| {
                        let TerminalRequest::ClipboardLoad { format, .. } = request
                        else {
                            unreachable!("pending request kind was checked")
                        };
                        Self::response_bytes(format(&text))
                    },
                )?,
                SessionCommand::ColorResponse {
                    request_id,
                    route_id,
                    color,
                } => self.accept_terminal_response(
                    request_id,
                    RequestKind::ColorRequest,
                    route_id,
                    |request| {
                        let Some([r, g, b]) = color else {
                            return Err(SessionError::unsupported(
                                "unset terminal colors need a host fallback",
                            ));
                        };
                        let TerminalRequest::ColorRequest { format, .. } = request else {
                            unreachable!("pending request kind was checked")
                        };
                        Self::response_bytes(format(librio::ColorRgb { r, g, b }))
                    },
                )?,
                SessionCommand::TextAreaSizeResponse {
                    request_id,
                    route_id,
                    rows,
                    columns,
                    pixel_width,
                    pixel_height,
                } => self.accept_terminal_response(
                    request_id,
                    RequestKind::TextAreaSizeRequest,
                    route_id,
                    |request| {
                        let TerminalRequest::TextAreaSizeRequest { format, .. } = request
                        else {
                            unreachable!("pending request kind was checked")
                        };
                        Self::response_bytes(format(rio_vt::event::WindowSize {
                            rows,
                            cols: columns,
                            width: pixel_width,
                            height: pixel_height,
                        }))
                    },
                )?,
                SessionCommand::GlyphProtocolResponse {
                    request_id,
                    route_id,
                    status,
                } => self.accept_terminal_response(
                    request_id,
                    RequestKind::GlyphProtocolQuery,
                    route_id,
                    |request| {
                        let TerminalRequest::GlyphProtocolQuery { cp, .. } = request
                        else {
                            unreachable!("pending request kind was checked")
                        };
                        Self::response_bytes(
                            rio_vt::ansi::glyph_protocol::format_query_response(
                                *cp,
                                glyph_status(status),
                            ),
                        )
                    },
                )?,
            };
            let _ = self.flush_ordered_replies()?;
            Ok(reply)
        }
    }

    fn input_error(error: librio::InputError) -> SessionError {
        match error {
            librio::InputError::WouldBlock => {
                SessionError::invalid("pending PTY input exceeds the session limit")
            }
            librio::InputError::Disconnected => SessionError::Detached,
        }
    }

    fn selection_kind(kind: SelectionKind) -> RioSelectionKind {
        match kind {
            SelectionKind::Simple => RioSelectionKind::Simple,
            SelectionKind::Word => RioSelectionKind::Word,
            SelectionKind::Line => RioSelectionKind::Line,
            SelectionKind::Block => RioSelectionKind::Block,
        }
    }

    fn selection_side(side: SelectionSide) -> Side {
        match side {
            SelectionSide::Left => Side::Left,
            SelectionSide::Right => Side::Right,
        }
    }

    fn search_direction(direction: SearchDirection) -> Direction {
        match direction {
            SearchDirection::Forward => Direction::Right,
            SearchDirection::Backward => Direction::Left,
        }
    }

    fn vi_motion(motion: crate::protocol::ViMotion) -> RioViMotion {
        match motion {
            crate::protocol::ViMotion::Up => RioViMotion::Up,
            crate::protocol::ViMotion::Down => RioViMotion::Down,
            crate::protocol::ViMotion::Left => RioViMotion::Left,
            crate::protocol::ViMotion::Right => RioViMotion::Right,
            crate::protocol::ViMotion::First => RioViMotion::First,
            crate::protocol::ViMotion::Last => RioViMotion::Last,
            crate::protocol::ViMotion::FirstOccupied => RioViMotion::FirstOccupied,
            crate::protocol::ViMotion::High => RioViMotion::High,
            crate::protocol::ViMotion::Middle => RioViMotion::Middle,
            crate::protocol::ViMotion::Low => RioViMotion::Low,
            crate::protocol::ViMotion::SemanticLeft => RioViMotion::SemanticLeft,
            crate::protocol::ViMotion::SemanticRight => RioViMotion::SemanticRight,
            crate::protocol::ViMotion::SemanticLeftEnd => RioViMotion::SemanticLeftEnd,
            crate::protocol::ViMotion::SemanticRightEnd => RioViMotion::SemanticRightEnd,
            crate::protocol::ViMotion::WordLeft => RioViMotion::WordLeft,
            crate::protocol::ViMotion::WordRight => RioViMotion::WordRight,
            crate::protocol::ViMotion::WordLeftEnd => RioViMotion::WordLeftEnd,
            crate::protocol::ViMotion::WordRightEnd => RioViMotion::WordRightEnd,
            crate::protocol::ViMotion::Bracket => RioViMotion::Bracket,
            crate::protocol::ViMotion::ParagraphUp => RioViMotion::ParagraphUp,
            crate::protocol::ViMotion::ParagraphDown => RioViMotion::ParagraphDown,
        }
    }

    fn glyph_status(
        status: crate::protocol::GlyphStatus,
    ) -> rio_vt::ansi::glyph_protocol::QueryStatus {
        match status {
            crate::protocol::GlyphStatus::Free => {
                rio_vt::ansi::glyph_protocol::QueryStatus::Free
            }
            crate::protocol::GlyphStatus::System => {
                rio_vt::ansi::glyph_protocol::QueryStatus::System
            }
            crate::protocol::GlyphStatus::Glossary => {
                rio_vt::ansi::glyph_protocol::QueryStatus::Glossary
            }
            crate::protocol::GlyphStatus::Both => {
                rio_vt::ansi::glyph_protocol::QueryStatus::Both
            }
        }
    }

    fn key_event(input: KeyInput) -> librio::KeyEvent {
        let key = input.key.map(key_code);
        librio::KeyEvent {
            action: match input.action {
                WireKeyAction::Press => librio::KeyAction::Press,
                WireKeyAction::Repeat => librio::KeyAction::Repeat,
                WireKeyAction::Release => librio::KeyAction::Release,
            },
            key,
            mods: librio::Modifiers::from_bits_retain(input.modifiers),
            consumed_mods: librio::Modifiers::from_bits_retain(input.consumed_modifiers),
            text: input.text,
            composing: input.composing,
        }
    }

    fn key_code(code: KeyCode) -> librio::Key {
        match code {
            KeyCode::Char(value) => librio::Key::Char(value),
            KeyCode::Enter => librio::Key::Enter,
            KeyCode::Tab => librio::Key::Tab,
            KeyCode::Backspace => librio::Key::Backspace,
            KeyCode::Escape => librio::Key::Escape,
            KeyCode::Up => librio::Key::Up,
            KeyCode::Down => librio::Key::Down,
            KeyCode::Left => librio::Key::Left,
            KeyCode::Right => librio::Key::Right,
            KeyCode::Home => librio::Key::Home,
            KeyCode::End => librio::Key::End,
            KeyCode::PageUp => librio::Key::PageUp,
            KeyCode::PageDown => librio::Key::PageDown,
            KeyCode::Insert => librio::Key::Insert,
            KeyCode::Delete => librio::Key::Delete,
            KeyCode::Function(number) => librio::Key::F(number),
            KeyCode::CapsLock => librio::Key::CapsLock,
            KeyCode::ShiftLeft => librio::Key::ShiftLeft,
            KeyCode::ShiftRight => librio::Key::ShiftRight,
            KeyCode::ControlLeft => librio::Key::ControlLeft,
            KeyCode::ControlRight => librio::Key::ControlRight,
            KeyCode::AltLeft => librio::Key::AltLeft,
            KeyCode::AltRight => librio::Key::AltRight,
            KeyCode::SuperLeft => librio::Key::SuperLeft,
            KeyCode::SuperRight => librio::Key::SuperRight,
        }
    }

    struct Active {
        generation: u64,
        connection_fd: RawFd,
        wakeup: Arc<readiness::Readiness>,
    }

    struct Shared {
        state: Mutex<SharedState>,
        next_generation: AtomicU64,
        closing: AtomicBool,
        close_pending: AtomicBool,
        connections: Mutex<Vec<UnixStream>>,
        session_id: SessionId,
        capability: [u8; 32],
        delegate: Arc<Delegate>,
    }

    struct SharedState {
        runtime: Option<Runtime>,
        active: Option<Active>,
        idle_since: Option<Instant>,
    }

    struct ConnectionGuard {
        shared: Arc<Shared>,
        fd: RawFd,
    }

    impl Drop for ConnectionGuard {
        fn drop(&mut self) {
            self.shared.unregister_connection(self.fd);
        }
    }

    impl Shared {
        fn new(
            session_id: SessionId,
            capability: [u8; 32],
            delegate: Arc<Delegate>,
        ) -> Self {
            Self {
                state: Mutex::new(SharedState {
                    runtime: None,
                    active: None,
                    idle_since: Some(Instant::now()),
                }),
                next_generation: AtomicU64::new(1),
                closing: AtomicBool::new(false),
                close_pending: AtomicBool::new(false),
                connections: Mutex::new(Vec::new()),
                session_id,
                capability,
                delegate,
            }
        }

        fn wait_deadline(&self) -> Option<Instant> {
            let state = self.state.lock().expect("worker state lock poisoned");
            let idle_deadline = state.idle_since.map(|since| since + IDLE_RETENTION);
            let request_deadline = state.runtime.as_ref().and_then(|runtime| {
                runtime
                    .pending_replies
                    .iter()
                    .filter_map(|reply| match reply {
                        PendingReply::Request { expires_at, .. } => Some(*expires_at),
                        PendingReply::PtyWrite(_) => None,
                    })
                    .min()
            });
            next_deadline(idle_deadline, request_deadline)
        }

        fn signal_active(&self) {
            let wakeup = self
                .state
                .lock()
                .expect("worker state lock poisoned")
                .active
                .as_ref()
                .map(|active| Arc::clone(&active.wakeup));
            if let Some(wakeup) = wakeup {
                wakeup.signal();
            }
        }

        fn request_close(&self) {
            if !self.closing.swap(true, Ordering::AcqRel) {
                self.delegate.wakeup.signal();
            }
        }

        fn expire_if_idle(&self) -> bool {
            let state = self.state.lock().expect("worker state lock poisoned");
            if state
                .idle_since
                .is_some_and(|since| since.elapsed() >= IDLE_RETENTION)
            {
                self.closing.store(true, Ordering::Release);
                true
            } else {
                false
            }
        }

        fn refresh_idle_after_terminal_event(&self) {
            let mut state = self.state.lock().expect("worker state lock poisoned");
            if state.active.is_none() {
                state.idle_since = Some(Instant::now());
            }
        }

        fn register_connection(
            &self,
            stream: &UnixStream,
        ) -> Result<Option<RawFd>, SessionError> {
            let mut connections = self
                .connections
                .lock()
                .map_err(|_| SessionError::protocol("worker connection lock poisoned"))?;
            if connections.len() >= MAX_CONNECTIONS {
                return Ok(None);
            }
            let copy = stream.try_clone()?;
            // Keep the registry's fd alive until the guard unregisters it. The
            // handler's stream can be dropped before its guard, so using that
            // fd as the key would allow reuse to unregister a newer peer.
            let fd = copy.as_raw_fd();
            connections.push(copy);
            Ok(Some(fd))
        }

        fn unregister_connection(&self, fd: RawFd) {
            if let Ok(mut connections) = self.connections.lock() {
                if let Some(index) = connections
                    .iter()
                    .position(|connection| connection.as_raw_fd() == fd)
                {
                    connections.swap_remove(index);
                }
            }
        }

        fn close_connection(&self, fd: RawFd) {
            if let Ok(connections) = self.connections.lock() {
                if let Some(stream) = connections
                    .iter()
                    .find(|connection| connection.as_raw_fd() == fd)
                {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }

        fn close_connections(&self) {
            if let Ok(connections) = self.connections.lock() {
                for stream in connections.iter() {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }

        fn next_generation(&self) -> Result<u64, SessionError> {
            let mut current = self.next_generation.load(Ordering::Relaxed);
            loop {
                if current == 0 || current == u64::MAX {
                    return Err(SessionError::protocol(
                        "attachment generation exhausted",
                    ));
                }
                match self.next_generation.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Ok(current),
                    Err(next) => current = next,
                }
            }
        }
    }

    fn wait_for_listener(
        listener: &UnixListener,
        wakeup: &readiness::Readiness,
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        let mut poll_fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wakeup.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            let ready = readiness::wait(&mut poll_fds, deadline)?;
            if ready == 0 {
                return Ok(());
            }
            if poll_fds
                .iter()
                .any(|poll_fd| readiness::is_invalid(poll_fd.revents))
            {
                return Err(SessionError::protocol("session readiness fd is invalid"));
            }
            if poll_fds
                .iter()
                .any(|poll_fd| readiness::is_readable(poll_fd.revents))
            {
                return Ok(());
            }
        }
    }

    pub fn run(
        endpoint: PathBuf,
        session_id: SessionId,
        capability: [u8; 32],
        listener_fd: Option<RawFd>,
        endpoint_identity: Option<(u64, u64)>,
    ) -> Result<(), SessionError> {
        if capability.iter().all(|byte| *byte == 0) {
            return Err(SessionError::invalid("worker capability is empty"));
        }
        if !cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd"
        )) {
            return Err(SessionError::unsupported(
                "session workers are supported only on Linux and BSD/macOS Unix sockets",
            ));
        }
        if !endpoint.is_absolute() {
            return Err(SessionError::invalid(
                "session worker endpoint must be an absolute path",
            ));
        }
        let guard = match listener_fd {
            Some(listener_fd) => EndpointGuard::from_fd(
                endpoint,
                listener_fd,
                endpoint_identity.ok_or_else(|| {
                    SessionError::invalid("missing inherited endpoint identity")
                })?,
            )?,
            None => EndpointGuard::bind(endpoint)?,
        };
        let wakeup = readiness::Readiness::new()?;
        let delegate = Arc::new(Delegate {
            wakeup_pending: AtomicBool::new(false),
            wakeup,
            deferred: Mutex::new(DeferredNotifications::default()),
            next_request_id: AtomicU64::new(1),
            pending_requests: Arc::new(AtomicUsize::new(0)),
        });
        let shared = Arc::new(Shared::new(session_id, capability, delegate));

        let mut connection_threads: Vec<thread::JoinHandle<()>> = Vec::new();
        let result = loop {
            shared.delegate.wakeup.clear();
            if shared.closing.load(Ordering::Acquire) {
                break Ok(());
            }
            if let Err(error) = drain_notifications(&shared) {
                break Err(error);
            }
            if shared.expire_if_idle() {
                break Ok(());
            }
            let mut index = 0;
            while index < connection_threads.len() {
                if connection_threads[index].is_finished() {
                    let thread = connection_threads.swap_remove(index);
                    let _ = thread.join();
                } else {
                    index += 1;
                }
            }
            let deadline = shared.wait_deadline();
            if let Err(error) =
                wait_for_listener(&guard.listener, &shared.delegate.wakeup, deadline)
            {
                break Err(error);
            }
            match guard.listener.accept() {
                Ok((stream, _)) => {
                    let fd = match shared.register_connection(&stream) {
                        Ok(Some(fd)) => fd,
                        Ok(None) => continue,
                        Err(error) => break Err(error),
                    };
                    let shared_for_thread = Arc::clone(&shared);
                    match thread::Builder::new()
                        .name("rio-session-connection".into())
                        .spawn(move || {
                            let _guard = ConnectionGuard {
                                shared: shared_for_thread.clone(),
                                fd,
                            };
                            let _ = handle_connection(shared_for_thread, stream, fd);
                        }) {
                        Ok(thread) => connection_threads.push(thread),
                        Err(error) => {
                            shared.unregister_connection(fd);
                            break Err(SessionError::Io(error));
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => break Err(SessionError::Io(error)),
            }
        };
        shared.closing.store(true, Ordering::Release);
        shared.close_connections();
        for thread in connection_threads {
            let _ = thread.join();
        }
        drop(guard);
        result
    }

    fn handle_connection(
        shared: Arc<Shared>,
        mut stream: UnixStream,
        connection_fd: RawFd,
    ) -> Result<(), SessionError> {
        if shared.closing.load(Ordering::Acquire)
            || shared.close_pending.load(Ordering::Acquire)
        {
            return Ok(());
        }
        if !peer_is_current_user(&stream) {
            return Ok(());
        }
        stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
        stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;

        let hello = {
            let message = codec::read_frame_until::<ClientMessage>(
                &mut stream,
                Instant::now() + HANDSHAKE_TIMEOUT,
            )?;
            message.validate()?;
            match message {
                ClientMessage::Hello {
                    version,
                    capability,
                    session_id,
                    spec,
                } => (version, capability, session_id, spec),
                _ => {
                    let _ =
                        send_error(&mut stream, ErrorCode::BadRequest, "expected hello");
                    return Ok(());
                }
            }
        };
        if hello.0 != PROTOCOL_VERSION {
            let _ = send_error(
                &mut stream,
                ErrorCode::BadProtocol,
                "unsupported protocol version",
            );
            return Ok(());
        }
        if hello.2 != shared.session_id {
            let _ = send_error(&mut stream, ErrorCode::BadAuth, "unknown session");
            return Ok(());
        }
        if !capabilities_equal(&hello.1, &shared.capability) {
            let _ = send_error(
                &mut stream,
                ErrorCode::BadAuth,
                "invalid session capability",
            );
            return Ok(());
        }
        let active_wakeup = Arc::new(readiness::Readiness::new()?);
        let generation = match establish_attachment(
            &shared,
            &mut stream,
            connection_fd,
            hello.3,
            Arc::clone(&active_wakeup),
        ) {
            Ok(generation) => generation,
            Err(error) => {
                if !matches!(error, SessionError::Io(_) | SessionError::Codec(_)) {
                    let _ =
                        send_error(&mut stream, error_code(&error), &error.to_string());
                }
                return Err(error);
            }
        };
        if let Err(error) = stream.set_read_timeout(Some(CONNECTION_TIMEOUT)) {
            detach(&shared, generation);
            return Err(error.into());
        }
        if let Err(error) = stream.set_write_timeout(Some(CONNECTION_TIMEOUT)) {
            detach(&shared, generation);
            return Err(error.into());
        }
        let result = run_attachment(&shared, &mut stream, generation, &active_wakeup);
        if !shared.closing.load(Ordering::Acquire) && is_active(&shared, generation) {
            detach(&shared, generation);
        }
        result
    }

    fn establish_attachment(
        shared: &Arc<Shared>,
        stream: &mut UnixStream,
        connection_fd: RawFd,
        spec: Option<SessionSpec>,
        active_wakeup: Arc<readiness::Readiness>,
    ) -> Result<u64, SessionError> {
        let (current_generation, runtime_created) = {
            let mut state = shared
                .state
                .lock()
                .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
            if shared.closing.load(Ordering::Acquire)
                || shared.close_pending.load(Ordering::Acquire)
            {
                return Err(SessionError::Detached);
            }
            let runtime_created = if state.runtime.is_none() {
                let spec = match spec {
                    Some(spec) => spec,
                    None => {
                        return Err(SessionError::invalid(
                            "first attachment needs a session spec",
                        ));
                    }
                };
                let new_runtime = Runtime::new(spec, Arc::clone(&shared.delegate))?;
                state.runtime = Some(new_runtime);
                true
            } else {
                false
            };
            (
                state.active.as_ref().map(|active| active.generation),
                runtime_created,
            )
        };
        if runtime_created && shared.delegate.wakeup_pending.load(Ordering::Relaxed) {
            shared.delegate.wakeup.signal();
        }

        if let Some(current_generation) = current_generation {
            codec::write_frame_until(
                stream,
                &ServerMessage::Offer {
                    generation: current_generation,
                },
                Instant::now() + HANDSHAKE_TIMEOUT,
            )?;
            let claim = codec::read_frame_until::<ClientMessage>(
                stream,
                Instant::now() + HANDSHAKE_TIMEOUT,
            )?;
            claim.validate()?;
            let ClientMessage::Claim { generation } = claim else {
                return Err(SessionError::protocol("attachment claim missing"));
            };
            if generation != current_generation {
                return Err(SessionError::protocol("stale attachment claim"));
            }
        }

        let generation = shared.next_generation()?;
        let frame = {
            let mut state = shared
                .state
                .lock()
                .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
            if shared.closing.load(Ordering::Acquire)
                || shared.close_pending.load(Ordering::Acquire)
            {
                return Err(SessionError::Detached);
            }
            if let Some(active) = state.active.as_ref() {
                if Some(active.generation) != current_generation {
                    return Err(SessionError::protocol("stale attachment claim"));
                }
            }
            let runtime = state
                .runtime
                .as_mut()
                .ok_or_else(|| SessionError::protocol("session runtime missing"))?;
            runtime.snapshots.full_frame(&runtime.surface)?
        };

        codec::write_frame_until(
            stream,
            &ServerMessage::Claimed { generation },
            Instant::now() + HANDSHAKE_TIMEOUT,
        )?;
        codec::write_frame_until(
            stream,
            &ServerMessage::Initial { generation, frame },
            Instant::now() + HANDSHAKE_TIMEOUT,
        )?;
        let initial_sent_at = Instant::now();
        read_prepared_commit(stream, generation, initial_sent_at)?;

        let old_fd = {
            let mut state = shared
                .state
                .lock()
                .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
            if shared.closing.load(Ordering::Acquire)
                || shared.close_pending.load(Ordering::Acquire)
            {
                return Err(SessionError::Detached);
            }
            if let Some(active) = state.active.as_ref() {
                if Some(active.generation) != current_generation {
                    return Err(SessionError::protocol("stale attachment commit"));
                }
            }
            let old_fd = state.active.replace(Active {
                generation,
                connection_fd,
                wakeup: Arc::clone(&active_wakeup),
            });
            if let Some(runtime) = state.runtime.as_mut() {
                runtime.frame_pending = true;
            }
            state.idle_since = None;
            old_fd.map(|active| active.connection_fd)
        };
        if let Some(old_fd) = old_fd {
            shared.close_connection(old_fd);
        }
        // Commit is the ownership point. A lost Ready cannot be rolled back
        // without risking two writers; the client must reconnect and claim the
        // current generation instead.
        let ready = codec::write_frame_until(
            stream,
            &ServerMessage::Ready {
                version: PROTOCOL_VERSION,
                session_id: shared.session_id,
                generation,
            },
            Instant::now() + HANDSHAKE_TIMEOUT,
        );
        if let Err(error) = ready {
            detach(shared, generation);
            return Err(error);
        }
        Ok(generation)
    }

    fn read_prepared_commit(
        stream: &mut UnixStream,
        generation: u64,
        initial_sent_at: Instant,
    ) -> Result<(), SessionError> {
        // Only authenticated peers reach this phase. The absolute deadline
        // cannot be extended by partial writes or renderer progress messages.
        let commit = codec::read_frame_until::<ClientMessage>(
            stream,
            initial_sent_at + PREPARED_ATTACHMENT_TIMEOUT,
        )?;
        commit.validate()?;
        if !matches!(commit, ClientMessage::Commit { generation: value } if value == generation)
        {
            return Err(SessionError::protocol("initial frame commit missing"));
        }
        Ok(())
    }

    #[test]
    fn prepared_commit_deadline_boundary() {
        assert_eq!(PREPARED_ATTACHMENT_TIMEOUT, Duration::from_secs(30));
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        codec::write_frame(&mut writer, &ClientMessage::Commit { generation: 1 })
            .unwrap();
        // Past the short handshake budget, but still inside readiness time.
        read_prepared_commit(&mut reader, 1, Instant::now() - HANDSHAKE_TIMEOUT).unwrap();
        codec::write_frame(&mut writer, &ClientMessage::Commit { generation: 1 })
            .unwrap();
        let boundary = Instant::now() - PREPARED_ATTACHMENT_TIMEOUT;
        assert!(codec::is_timeout(
            &read_prepared_commit(&mut reader, 1, boundary).unwrap_err()
        ));
        // Even a fully queued commit is rejected at expiry; no blocking sleep.
        assert!(codec::is_timeout(
            &read_prepared_commit(&mut reader, 1, boundary - Duration::from_nanos(1))
                .unwrap_err()
        ));
    }

    fn run_attachment(
        shared: &Shared,
        stream: &mut UnixStream,
        generation: u64,
        active_wakeup: &readiness::Readiness,
    ) -> Result<(), SessionError> {
        let mut last_request_id = 0;
        loop {
            if !is_active(shared, generation) {
                return Ok(());
            }
            active_wakeup.clear();
            flush_events(shared, stream, generation)?;

            match read_client_message(stream, active_wakeup)? {
                Some(message @ ClientMessage::Command { .. }) => {
                    if let Err(error) = message.validate() {
                        send_error(stream, error_code(&error), &error.to_string())?;
                        return Ok(());
                    }
                    let ClientMessage::Command {
                        generation: command_generation,
                        request_id,
                        command,
                    } = message
                    else {
                        unreachable!("validated command pattern changed");
                    };
                    if request_id <= last_request_id {
                        send_error(
                            stream,
                            ErrorCode::BadRequest,
                            "request ids must increase on an attachment",
                        )?;
                        return Ok(());
                    }
                    last_request_id = request_id;
                    let (result, close, stale) = {
                        let mut state = shared.state.lock().map_err(|_| {
                            SessionError::protocol("worker state lock poisoned")
                        })?;
                        let active = state
                            .active
                            .as_ref()
                            .is_some_and(|active| active.generation == generation);
                        if !active || command_generation != generation {
                            (Err(SessionError::protocol("stale attachment")), false, true)
                        } else {
                            let result = state
                                .runtime
                                .as_mut()
                                .ok_or_else(|| {
                                    SessionError::protocol("session runtime missing")
                                })?
                                .execute(command);
                            let close = result
                                .as_ref()
                                .is_ok_and(|reply| matches!(reply, SessionReply::Closed));
                            if close {
                                shared.close_pending.store(true, Ordering::Release);
                            }
                            (result, close, false)
                        }
                    };
                    if stale {
                        send_error(
                            stream,
                            ErrorCode::StaleGeneration,
                            "stale attachment",
                        )?;
                        return Ok(());
                    }
                    match result {
                        Ok(reply) => {
                            // Snapshotter validates and size-checks its own trusted frames before
                            // advancing publication state. Other replies still cross this
                            // worker-side contract check before they are written.
                            if !matches!(
                                &reply,
                                &SessionReply::Frame(_) | &SessionReply::FrameUpdate(_)
                            ) {
                                if let Err(error) = reply.validate() {
                                    send_error(
                                        stream,
                                        error_code(&error),
                                        &error.to_string(),
                                    )?;
                                    return Ok(());
                                }
                            }
                            if close {
                                // Reply first: the client must observe explicit close before
                                // the worker tears down its tracked sockets.
                                let write_result = codec::write_frame_until(
                                    stream,
                                    &ServerMessage::Reply { request_id, reply },
                                    Instant::now() + HANDSHAKE_TIMEOUT,
                                );
                                shared.request_close();
                                shared.close_connections();
                                write_result?;
                                return Ok(());
                            }
                            codec::write_frame_until(
                                stream,
                                &ServerMessage::Reply { request_id, reply },
                                Instant::now() + HANDSHAKE_TIMEOUT,
                            )?;
                        }
                        Err(error) => {
                            send_error(stream, error_code(&error), &error.to_string())?;
                        }
                    }
                }
                Some(_) => {
                    send_error(
                        stream,
                        ErrorCode::BadRequest,
                        "unexpected client message",
                    )?;
                    return Ok(());
                }
                None => {}
            }
        }
    }

    fn read_client_message(
        stream: &mut UnixStream,
        active_wakeup: &readiness::Readiness,
    ) -> Result<Option<ClientMessage>, SessionError> {
        let mut poll_fds = [
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: active_wakeup.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            readiness::wait(&mut poll_fds, None)?;
            if readiness::is_invalid(poll_fds[1].revents) {
                return Err(SessionError::protocol("attachment readiness fd is invalid"));
            }
            if readiness::is_invalid(poll_fds[0].revents) {
                return Err(SessionError::WorkerExited);
            }
            if readiness::is_readable(poll_fds[0].revents) {
                let message =
                    codec::read_frame_until(stream, Instant::now() + HANDSHAKE_TIMEOUT);
                stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
                return message.map(Some);
            }
            if readiness::is_readable(poll_fds[1].revents) {
                return Ok(None);
            }
        }
    }

    fn is_active(shared: &Shared, generation: u64) -> bool {
        shared
            .state
            .lock()
            .expect("worker state lock poisoned")
            .active
            .as_ref()
            .is_some_and(|value| value.generation == generation)
    }

    fn detach(shared: &Shared, generation: u64) {
        // Serialize clearing ownership and arming retention with replacement
        // commits, which disable retention under this same state lock.
        let mut state = shared.state.lock().expect("worker state lock poisoned");
        let detached = if state
            .active
            .as_ref()
            .is_some_and(|value| value.generation == generation)
        {
            state.active = None;
            true
        } else {
            false
        };
        if detached {
            if let Some(runtime) = state.runtime.as_mut() {
                expire_requests(runtime);
                let requests = runtime
                    .pending_replies
                    .iter()
                    .filter_map(|reply| match reply {
                        PendingReply::Request {
                            request,
                            response: None,
                            ..
                        } => terminal_request_event(request),
                        PendingReply::PtyWrite(_) => None,
                        PendingReply::Request {
                            response: Some(_), ..
                        } => None,
                    })
                    .collect::<Vec<_>>();
                for event in requests {
                    queue_event(&mut runtime.pending_events, event);
                }
                if let Some(status) = runtime.child_exit_status {
                    queue_event(
                        &mut runtime.pending_events,
                        SessionEvent::ChildExited { status },
                    );
                }
                if runtime.terminal_closed {
                    queue_event(&mut runtime.pending_events, SessionEvent::Closed);
                }
            }
            state.idle_since = Some(Instant::now());
        }
        drop(state);
        if detached {
            shared.delegate.wakeup.signal();
        }
    }

    fn drain_notifications(shared: &Shared) -> Result<bool, SessionError> {
        let (terminal_event, published) = {
            let mut state = shared
                .state
                .lock()
                .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
            let Some(runtime) = state.runtime.as_mut() else {
                return Ok(false);
            };
            let wakeup_pending = shared
                .delegate
                .wakeup_pending
                .swap(false, Ordering::Relaxed);
            let mut critical_notifications = Vec::new();
            let mut notifications = Vec::new();
            let mut deferred = shared.delegate.deferred.lock().map_err(|_| {
                SessionError::protocol("worker deferred notification lock poisoned")
            })?;
            while let Some(notification) = deferred.queued.pop_front() {
                if notification.is_critical() {
                    critical_notifications.push(notification);
                } else {
                    notifications.push(notification);
                }
            }
            deferred.drain_into(&mut critical_notifications, &mut notifications);
            let mut published =
                !critical_notifications.is_empty() || !notifications.is_empty();
            let requests_expired = expire_requests(runtime);
            if requests_expired {
                published = true;
            }
            if wakeup_pending {
                runtime.frame_pending = true;
                published = true;
            }
            let mut terminal_event = requests_expired;
            for notification in critical_notifications.into_iter().chain(notifications) {
                match notification {
                    Notification::Action(action) => {
                        let event = match action {
                            Action::SetTitle { title, .. } => {
                                SessionEvent::Title { title }
                            }
                            Action::RingBell => SessionEvent::Bell,
                            Action::CursorBlinkingChange => {
                                runtime.frame_pending = true;
                                SessionEvent::CursorBlinkingChanged
                            }
                            Action::Progress { state, value } => {
                                SessionEvent::Progress { state, value }
                            }
                        };
                        queue_event(&mut runtime.pending_events, event);
                    }
                    Notification::ClipboardStore { kind, text } => {
                        queue_event(
                            &mut runtime.pending_events,
                            SessionEvent::ClipboardStore {
                                kind: kind as u8,
                                text,
                            },
                        );
                    }
                    Notification::Closed => {
                        runtime.terminal_closed = true;
                        terminal_event = true;
                        queue_event(&mut runtime.pending_events, SessionEvent::Closed);
                    }
                    Notification::ChildExited(status) => {
                        runtime.child_exit_status = Some(status);
                        runtime.frame_pending = true;
                        terminal_event = true;
                        queue_event(
                            &mut runtime.pending_events,
                            SessionEvent::ChildExited { status },
                        );
                    }
                    Notification::ClipboardOverflow => {
                        queue_event(
                            &mut runtime.pending_events,
                            SessionEvent::ClipboardOverflow,
                        );
                    }
                    Notification::RequestRefused {
                        request_id,
                        kind,
                        reason,
                    } => {
                        queue_event(
                            &mut runtime.pending_events,
                            SessionEvent::RequestRefused {
                                request_id,
                                kind,
                                reason,
                            },
                        );
                    }
                    Notification::Desktop { title, body } => {
                        queue_event(
                            &mut runtime.pending_events,
                            SessionEvent::DesktopNotification { title, body },
                        );
                    }
                    Notification::ColorChange {
                        route_id,
                        index,
                        color,
                    } => {
                        let route_id = u64::try_from(route_id).map_err(|_| {
                            SessionError::invalid(
                                "color change route id exceeds protocol range",
                            )
                        })?;
                        let index = index.try_into().map_err(|_| {
                            SessionError::invalid(
                                "color change index exceeds protocol range",
                            )
                        })?;
                        queue_event(
                            &mut runtime.pending_events,
                            SessionEvent::ColorChange {
                                route_id,
                                index,
                                color: color.map(|color| [color.r, color.g, color.b]),
                            },
                        );
                        runtime.frame_pending = true;
                    }
                }
            }
            if !deferred.terminal_replies.is_empty() {
                published = true;
            }
            while let Some(reply) = deferred.terminal_replies.pop_front() {
                match reply {
                    TerminalReply::PtyWrite(write) => {
                        runtime
                            .pending_replies
                            .push_back(PendingReply::PtyWrite(write));
                        terminal_event = true;
                    }
                    TerminalReply::Request(request) => {
                        let request_id = request.id();
                        let kind = request.kind();
                        let Some(event) = terminal_request_event(&request) else {
                            shared.delegate.release_request();
                            queue_event(
                                &mut runtime.pending_events,
                                SessionEvent::RequestRefused {
                                    request_id,
                                    kind,
                                    reason: RequestRefusalReason::Unsupported,
                                },
                            );
                            continue;
                        };
                        if queue_event(&mut runtime.pending_events, event) {
                            runtime.pending_replies.push_back(PendingReply::Request {
                                request,
                                expires_at: Instant::now() + TERMINAL_REQUEST_TIMEOUT,
                                response: None,
                            });
                            terminal_event = true;
                        } else {
                            shared.delegate.release_request();
                            if !queue_event(
                                &mut runtime.pending_events,
                                SessionEvent::RequestRefused {
                                    request_id,
                                    kind,
                                    reason: RequestRefusalReason::Capacity,
                                },
                            ) {
                                eprintln!("rio-session: terminal request {request_id} ({kind:?}) refused because the event queue is full");
                            }
                        }
                    }
                }
            }
            drop(deferred);
            if runtime.flush_ordered_replies()? {
                published = true;
                terminal_event = true;
            }
            (terminal_event, published)
        };
        if terminal_event {
            shared.refresh_idle_after_terminal_event();
        }
        if published {
            shared.signal_active();
        }
        Ok(published)
    }

    fn next_deadline(
        idle_deadline: Option<Instant>,
        request_deadline: Option<Instant>,
    ) -> Option<Instant> {
        match (idle_deadline, request_deadline) {
            (Some(idle), Some(request)) => Some(idle.min(request)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    fn expire_requests(runtime: &mut Runtime) -> bool {
        let now = Instant::now();
        let mut had_expired = false;
        let pending_request_slots = &runtime.pending_request_slots;
        let pending_events = &mut runtime.pending_events;
        runtime.pending_replies.retain(|reply| match reply {
            PendingReply::Request {
                request,
                expires_at,
                ..
            } if *expires_at <= now => {
                release_request_slot(pending_request_slots);
                remove_terminal_request_event(pending_events, request.id());
                queue_event(
                    pending_events,
                    SessionEvent::RequestExpired {
                        request_id: request.id(),
                        kind: request.kind(),
                    },
                );
                had_expired = true;
                false
            }
            _ => true,
        });
        had_expired
    }

    fn remove_terminal_request_event(
        events: &mut VecDeque<SessionEvent>,
        request_id: u64,
    ) {
        events.retain(|event| terminal_request_event_id(event) != Some(request_id));
    }

    fn flush_events(
        shared: &Shared,
        stream: &mut UnixStream,
        generation: u64,
    ) -> Result<(), SessionError> {
        let mut messages = Vec::new();
        let mut retry_frame = false;
        {
            let mut state = shared
                .state
                .lock()
                .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
            if !state
                .active
                .as_ref()
                .is_some_and(|active| active.generation == generation)
            {
                return Ok(());
            }
            let Some(runtime) = state.runtime.as_mut() else {
                return Ok(());
            };
            expire_requests(runtime);
            runtime.flush_ordered_replies()?;
            if runtime.frame_pending {
                if queue_event(&mut runtime.pending_events, SessionEvent::FrameReady) {
                    runtime.frame_pending = false;
                } else {
                    retry_frame = true;
                }
            }
            messages.extend(
                runtime
                    .pending_events
                    .drain(..)
                    .map(|event| ServerMessage::Event { generation, event }),
            );
        }
        if let Err(error) = messages.iter().try_for_each(ServerMessage::validate) {
            requeue_critical(shared, &messages, 0);
            return Err(error);
        }
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        for (index, message) in messages.iter().enumerate() {
            if let Err(error) = codec::write_frame_until(stream, message, deadline) {
                let start = if is_active(shared, generation) {
                    index
                } else {
                    0
                };
                requeue_critical(shared, &messages, start);
                return Err(error);
            }
        }
        if !is_active(shared, generation) {
            requeue_critical(shared, &messages, 0);
        }
        if retry_frame {
            shared.signal_active();
        }
        Ok(())
    }

    fn requeue_critical(shared: &Shared, messages: &[ServerMessage], start: usize) {
        let events = critical_events_from(messages, start);
        if events.is_empty() {
            return;
        }
        let published = {
            let mut state = shared.state.lock().expect("worker state lock poisoned");
            let Some(runtime) = state.runtime.as_mut() else {
                return;
            };
            let mut published = false;
            for event in events {
                published |= queue_event(&mut runtime.pending_events, event);
            }
            published
        };
        if published {
            shared.signal_active();
        }
    }

    fn critical_events_from(
        messages: &[ServerMessage],
        start: usize,
    ) -> Vec<SessionEvent> {
        messages[start..]
            .iter()
            .filter_map(|message| match message {
                ServerMessage::Event { event, .. } if event.is_critical() => {
                    Some(event.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn send_error(
        stream: &mut UnixStream,
        code: ErrorCode,
        message: &str,
    ) -> Result<(), SessionError> {
        let message = if message.len() <= crate::protocol::MAX_STRING_BYTES {
            message.to_owned()
        } else {
            let end = message
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= crate::protocol::MAX_STRING_BYTES)
                .last()
                .unwrap_or_default();
            message[..end].to_owned()
        };
        let response = ServerMessage::Error { code, message };
        response.validate()?;
        codec::write_frame_until(stream, &response, Instant::now() + HANDSHAKE_TIMEOUT)?;
        Ok(())
    }

    fn error_code(error: &SessionError) -> ErrorCode {
        match error {
            SessionError::Unsupported(_) => ErrorCode::Unsupported,
            SessionError::Invalid(_) => ErrorCode::BadRequest,
            SessionError::Codec(_) | SessionError::Protocol(_) => ErrorCode::BadProtocol,
            SessionError::Detached => ErrorCode::StaleGeneration,
            _ => ErrorCode::Internal,
        }
    }

    fn capabilities_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
        let mut difference = 0u8;
        for (left, right) in left.iter().zip(right) {
            difference |= *left ^ *right;
        }
        difference == 0
    }

    fn peer_is_current_user(stream: &UnixStream) -> bool {
        #[cfg(target_os = "linux")]
        {
            let mut peer = libc::ucred {
                pid: 0,
                uid: 0,
                gid: 0,
            };
            let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let result = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut length,
                )
            };
            result == 0 && peer.uid == unsafe { libc::geteuid() }
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd"
        ))]
        {
            let mut euid = 0;
            let mut egid = 0;
            let result =
                unsafe { libc::getpeereid(stream.as_raw_fd(), &mut euid, &mut egid) };
            result == 0 && euid == unsafe { libc::geteuid() }
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd"
        )))]
        true
    }

    struct EndpointGuard {
        listener: UnixListener,
        endpoint: PathBuf,
        endpoint_identity: crate::FileIdentity,
        parent_identity: crate::FileIdentity,
    }

    impl EndpointGuard {
        fn bind(endpoint: PathBuf) -> Result<Self, SessionError> {
            let (listener, endpoint_identity, parent_identity) =
                crate::bind_endpoint(&endpoint)?;
            Ok(Self {
                listener,
                endpoint,
                endpoint_identity,
                parent_identity,
            })
        }

        fn from_fd(
            endpoint: PathBuf,
            fd: RawFd,
            expected_identity: crate::FileIdentity,
        ) -> Result<Self, SessionError> {
            if fd < 0 {
                return Err(SessionError::invalid("session listener fd is negative"));
            }
            // The parent clears close-on-exec only for this inherited listener.
            let listener = unsafe { UnixListener::from_raw_fd(fd) };
            let validation = (|| -> Result<(), SessionError> {
                let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFD) };
                if flags == -1 {
                    return Err(io::Error::last_os_error().into());
                }
                if unsafe {
                    libc::fcntl(
                        listener.as_raw_fd(),
                        libc::F_SETFD,
                        flags | libc::FD_CLOEXEC,
                    )
                } == -1
                {
                    return Err(io::Error::last_os_error().into());
                }
                let address = listener.local_addr()?;
                if address.as_pathname() != Some(endpoint.as_path()) {
                    return Err(SessionError::protocol(
                        "inherited listener does not match the session endpoint",
                    ));
                }
                let mut socket_metadata = unsafe { std::mem::zeroed::<libc::stat>() };
                if unsafe { libc::fstat(listener.as_raw_fd(), &mut socket_metadata) }
                    == -1
                {
                    return Err(io::Error::last_os_error().into());
                }
                if socket_metadata.st_mode as u64 & libc::S_IFMT as u64
                    != libc::S_IFSOCK as u64
                {
                    return Err(SessionError::protocol(
                        "inherited listener does not match the session endpoint",
                    ));
                }
                let mut socket_type = 0;
                let mut option_length =
                    std::mem::size_of_val(&socket_type) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        listener.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_TYPE,
                        (&mut socket_type as *mut libc::c_int).cast(),
                        &mut option_length,
                    )
                } == -1
                    || socket_type != libc::SOCK_STREAM
                {
                    return Err(SessionError::protocol(
                        "inherited listener is not a stream socket",
                    ));
                }
                let mut accepting = 0;
                option_length = std::mem::size_of_val(&accepting) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        listener.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_ACCEPTCONN,
                        (&mut accepting as *mut libc::c_int).cast(),
                        &mut option_length,
                    )
                } == -1
                    || accepting == 0
                {
                    return Err(SessionError::protocol(
                        "inherited fd is not a listening socket",
                    ));
                }
                Ok(())
            })();
            if let Err(error) = validation {
                cleanup_listener(&endpoint, listener, expected_identity);
                return Err(error);
            }
            Self::from_listener(endpoint, listener, expected_identity)
        }

        fn from_listener(
            endpoint: PathBuf,
            listener: UnixListener,
            expected_identity: crate::FileIdentity,
        ) -> Result<Self, SessionError> {
            let parent = match endpoint.parent() {
                Some(parent) => parent,
                None => {
                    cleanup_listener(&endpoint, listener, expected_identity);
                    return Err(SessionError::invalid("session endpoint has no parent"));
                }
            };
            let parent_identity = match private_directory_identity(parent) {
                Ok(identity) => identity,
                Err(error) => {
                    cleanup_listener(&endpoint, listener, expected_identity);
                    return Err(error);
                }
            };
            let endpoint_identity =
                match crate::private_endpoint_identity(&endpoint, expected_identity) {
                    Ok(identity) => identity,
                    Err(error) => {
                        cleanup_listener(&endpoint, listener, expected_identity);
                        return Err(error);
                    }
                };
            if let Err(error) = listener.set_nonblocking(true) {
                cleanup_listener(&endpoint, listener, expected_identity);
                return Err(error.into());
            }
            Ok(Self {
                listener,
                endpoint,
                endpoint_identity,
                parent_identity,
            })
        }
    }

    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            if let Some(parent) = self.endpoint.parent() {
                cleanup_endpoint_if_owned(
                    &self.endpoint,
                    Some(self.endpoint_identity),
                    parent,
                    self.parent_identity,
                );
            }
        }
    }

    pub(super) struct WorkerArgs {
        pub(super) endpoint: PathBuf,
        pub(super) session_id: SessionId,
        pub(super) listener_fd: Option<RawFd>,
        pub(super) endpoint_identity: Option<crate::FileIdentity>,
    }

    pub(super) fn parse_args(
        args: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> Result<WorkerArgs, SessionError> {
        let mut args = args.into_iter();
        let mut endpoint = None;
        let mut session_id = None;
        let mut listener_fd = None;
        let mut endpoint_identity = None;
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--endpoint") => {
                    if endpoint.is_some() {
                        return Err(SessionError::invalid("duplicate worker endpoint"));
                    }
                    endpoint = args.next().map(PathBuf::from)
                }
                Some("--session-id") => {
                    if session_id.is_some() {
                        return Err(SessionError::invalid("duplicate worker session id"));
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| SessionError::invalid("missing session id"))?;
                    session_id = Some(parse_session_id(&value)?);
                }
                Some("--listener-fd") => {
                    if listener_fd.is_some() {
                        return Err(SessionError::invalid(
                            "duplicate worker listener fd",
                        ));
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| SessionError::invalid("missing listener fd"))?;
                    let value = value.to_str().ok_or_else(|| {
                        SessionError::invalid("listener fd is not UTF-8")
                    })?;
                    let value = value
                        .parse::<RawFd>()
                        .map_err(|_| SessionError::invalid("invalid listener fd"))?;
                    listener_fd = Some(value);
                }
                Some("--endpoint-identity") => {
                    if endpoint_identity.is_some() {
                        return Err(SessionError::invalid(
                            "duplicate worker endpoint identity",
                        ));
                    }
                    let value = args.next().ok_or_else(|| {
                        SessionError::invalid("missing endpoint identity")
                    })?;
                    endpoint_identity = Some(parse_endpoint_identity(&value)?);
                }
                _ => return Err(SessionError::invalid("unknown worker argument")),
            }
        }
        let endpoint =
            endpoint.ok_or_else(|| SessionError::invalid("missing worker endpoint"))?;
        if !endpoint.is_absolute() {
            return Err(SessionError::invalid(
                "worker endpoint must be an absolute path",
            ));
        }
        let session_id = session_id
            .ok_or_else(|| SessionError::invalid("missing worker session id"))?;
        if listener_fd.is_some() != endpoint_identity.is_some() {
            return Err(SessionError::invalid(
                "inherited listener requires an endpoint identity",
            ));
        }
        Ok(WorkerArgs {
            endpoint,
            session_id,
            listener_fd,
            endpoint_identity,
        })
    }

    fn parse_endpoint_identity(
        value: &std::ffi::OsStr,
    ) -> Result<crate::FileIdentity, SessionError> {
        let value = value
            .to_str()
            .ok_or_else(|| SessionError::invalid("endpoint identity is not UTF-8"))?;
        let (device, inode) = value
            .split_once(':')
            .ok_or_else(|| SessionError::invalid("invalid endpoint identity"))?;
        let device = device
            .parse()
            .map_err(|_| SessionError::invalid("invalid endpoint device"))?;
        let inode = inode
            .parse()
            .map_err(|_| SessionError::invalid("invalid endpoint inode"))?;
        Ok((device, inode))
    }

    fn parse_session_id(value: &std::ffi::OsStr) -> Result<SessionId, SessionError> {
        let value = value
            .to_str()
            .ok_or_else(|| SessionError::invalid("session id is not UTF-8"))?;
        let value = value.as_bytes();
        if value.len() != 32 {
            return Err(SessionError::invalid("invalid session id"));
        }
        let mut bytes = [0; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let high = hex_digit(value[index * 2])
                .ok_or_else(|| SessionError::invalid("invalid session id"))?;
            let low = hex_digit(value[index * 2 + 1])
                .ok_or_else(|| SessionError::invalid("invalid session id"))?;
            *byte = high * 16 + low;
        }
        Ok(SessionId(bytes))
    }

    fn hex_digit(value: u8) -> Option<u8> {
        match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            b'A'..=b'F' => Some(value - b'A' + 10),
            _ => None,
        }
    }

    #[test]
    fn detach_serializes_retention_with_replacement_commit() {
        let shared = test_shared();
        let mut state = shared.state.lock().unwrap();
        state.active = Some(Active {
            generation: 1,
            connection_fd: -1,
            wakeup: Arc::new(readiness::Readiness::new().unwrap()),
        });
        state.idle_since = None;
        let (started, waiting) = mpsc::sync_channel(1);
        let (done, finished) = mpsc::sync_channel(1);
        let other = shared.clone();
        let thread = thread::spawn(move || {
            started.send(()).unwrap();
            detach(&other, 1);
            done.send(()).unwrap();
        });
        waiting.recv().unwrap();
        assert!(finished.recv_timeout(Duration::from_millis(50)).is_err());
        let mut state = state;
        state.active = Some(Active {
            generation: 2,
            connection_fd: -1,
            wakeup: Arc::new(readiness::Readiness::new().unwrap()),
        });
        state.idle_since = None;
        drop(state);
        thread.join().unwrap();
        assert!(shared.state.lock().unwrap().idle_since.is_none());
        assert!(is_active(&shared, 2));
        detach(&shared, 2);
        assert!(shared.state.lock().unwrap().idle_since.is_some());
        assert!(!shared.expire_if_idle());
        shared.state.lock().unwrap().idle_since = Some(Instant::now() - IDLE_RETENTION);
        assert!(shared.expire_if_idle());
    }

    #[cfg(test)]
    fn test_shared() -> Arc<Shared> {
        let delegate = Arc::new(Delegate {
            wakeup_pending: AtomicBool::new(false),
            wakeup: readiness::Readiness::new().unwrap(),
            deferred: Mutex::new(DeferredNotifications::default()),
            next_request_id: AtomicU64::new(1),
            pending_requests: Arc::new(AtomicUsize::new(0)),
        });
        Arc::new(Shared::new(SessionId([1; 16]), [1; 32], delegate))
    }

    #[cfg(test)]
    fn test_color_request(request_id: u64) -> TerminalRequest {
        TerminalRequest::ColorRequest {
            request_id,
            route_id: 1,
            index: 11,
            format: Arc::new(|color: librio::ColorRgb| {
                format!("color:{}:{}:{}", color.r, color.g, color.b)
            }),
        }
    }

    #[test]
    fn frame_ready_retries_after_critical_events_are_flushed() {
        let shared = test_shared();
        let active_wakeup = Arc::new(readiness::Readiness::new().unwrap());
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        let mut runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        for index in 0..MAX_PENDING_REQUESTS {
            assert!(queue_event(
                &mut runtime.pending_events,
                SessionEvent::ColorChange {
                    route_id: 1,
                    index: index as u16,
                    color: None,
                },
            ));
        }
        runtime.frame_pending = true;
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
            state.active = Some(Active {
                generation: 1,
                connection_fd: stream.as_raw_fd(),
                wakeup: Arc::clone(&active_wakeup),
            });
        }

        flush_events(&shared, &mut stream, 1).unwrap();

        assert!(readiness_is_signaled(&active_wakeup));
    }

    #[test]
    fn detach_does_not_requeue_a_request_with_an_accepted_response() {
        let shared = test_shared();
        let mut runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        let response = runtime.surface.try_reserve_response(vec![b'\n']).unwrap();
        runtime.pending_replies.push_back(PendingReply::Request {
            request: TerminalRequest::GlyphProtocolQuery {
                request_id: 7,
                route_id: 1,
                cp: b'?' as u32,
            },
            expires_at: Instant::now() + TERMINAL_REQUEST_TIMEOUT,
            response: Some(response),
        });
        shared.delegate.pending_requests.store(1, Ordering::Release);
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
            state.active = Some(Active {
                generation: 1,
                connection_fd: -1,
                wakeup: Arc::new(readiness::Readiness::new().unwrap()),
            });
        }

        detach(&shared, 1);

        let state = shared.state.lock().unwrap();
        assert!(!state.runtime.as_ref().unwrap().pending_events.iter().any(
            |event| matches!(
                event,
                SessionEvent::GlyphProtocolQuery { request_id: 7, .. }
            )
        ));
    }

    #[test]
    fn terminal_replies_preserve_fifo_when_responses_are_reversed() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
        }

        let first_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        let second_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        drain_notifications(&shared).unwrap();

        let mut state = shared.state.lock().unwrap();
        let runtime = state.runtime.as_mut().unwrap();
        assert!(matches!(
            runtime.pending_replies.front(),
            Some(PendingReply::Request { request, .. })
                if request.id() == first_id
        ));
        assert!(matches!(
            runtime.pending_replies.get(1),
            Some(PendingReply::Request { request, .. })
                if request.id() == second_id
        ));

        runtime
            .execute(SessionCommand::ColorResponse {
                request_id: second_id,
                route_id: 1,
                color: Some([4, 5, 6]),
            })
            .unwrap();
        assert_eq!(runtime.pending_replies.len(), 2);
        assert!(runtime
            .pending_replies
            .iter()
            .find_map(|reply| match reply {
                PendingReply::Request {
                    request, response, ..
                } if request.id() == second_id => Some(response.is_some()),
                _ => None,
            })
            .unwrap());

        runtime
            .execute(SessionCommand::ColorResponse {
                request_id: first_id,
                route_id: 1,
                color: Some([1, 2, 3]),
            })
            .unwrap();
        assert!(runtime.pending_replies.is_empty());
        assert_eq!(shared.delegate.pending_requests.load(Ordering::Acquire), 0);
    }

    #[test]
    fn terminal_reply_fifo_survives_notification_backpressure() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
        }
        for _ in 0..MAX_PENDING_REQUESTS {
            shared.delegate.send(Notification::Action(Action::RingBell));
        }

        let first_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        let second_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        {
            let deferred = shared.delegate.deferred.lock().unwrap();
            assert_eq!(deferred.terminal_replies.len(), 2);
        }

        drain_notifications(&shared).unwrap();
        let state = shared.state.lock().unwrap();
        let runtime = state.runtime.as_ref().unwrap();
        assert!(matches!(
            runtime.pending_replies.front(),
            Some(PendingReply::Request { request, .. })
                if request.id() == first_id
        ));
        assert!(matches!(
            runtime.pending_replies.get(1),
            Some(PendingReply::Request { request, .. })
                if request.id() == second_id
        ));
    }

    #[test]
    fn terminal_reply_fifo_keeps_raw_and_request_admission_together() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
        }
        for _ in 0..MAX_PENDING_REQUESTS {
            shared.delegate.send(Notification::Action(Action::RingBell));
        }

        let first_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        let raw = {
            let state = shared.state.lock().unwrap();
            state
                .runtime
                .as_ref()
                .unwrap()
                .surface
                .try_reserve_response(b"raw-reply".to_vec())
                .unwrap()
        };
        assert!(matches!(
            SurfaceDelegate::pty_write(&*shared.delegate, 1, raw).unwrap(),
            librio::PtyWriteResult::Handled
        ));
        let second_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        assert_eq!(
            shared
                .delegate
                .deferred
                .lock()
                .unwrap()
                .terminal_replies
                .len(),
            3
        );

        drain_notifications(&shared).unwrap();
        let state = shared.state.lock().unwrap();
        let runtime = state.runtime.as_ref().unwrap();
        assert!(matches!(
            runtime
                .surface
                .try_reserve_response(vec![0; MAX_PENDING_INPUT_BYTES]),
            Err(librio::InputError::WouldBlock)
        ));
        assert!(matches!(
            runtime.pending_replies.front(),
            Some(PendingReply::Request { request, .. })
                if request.id() == first_id
        ));
        assert!(matches!(
            runtime.pending_replies.get(1),
            Some(PendingReply::PtyWrite(_))
        ));
        assert!(matches!(
            runtime.pending_replies.get(2),
            Some(PendingReply::Request { request, .. })
                if request.id() == second_id
        ));
    }

    #[test]
    fn saturated_clipboard_store_preserves_its_payload() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        shared.state.lock().unwrap().runtime = Some(runtime);
        for _ in 0..MAX_PENDING_REQUESTS {
            shared.delegate.send(Notification::Action(Action::RingBell));
        }

        shared.delegate.send(Notification::ClipboardStore {
            kind: librio::ClipboardType::Selection,
            text: String::from("preserve this clipboard"),
        });
        shared.delegate.send(Notification::ClipboardStore {
            kind: librio::ClipboardType::Clipboard,
            text: String::from("old clipboard value"),
        });
        shared.delegate.send(Notification::ClipboardStore {
            kind: librio::ClipboardType::Clipboard,
            text: String::from("new clipboard value"),
        });
        drain_notifications(&shared).unwrap();

        let state = shared.state.lock().unwrap();
        let events = &state.runtime.as_ref().unwrap().pending_events;
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::ClipboardStore { kind: 1, text }
                if text == "preserve this clipboard"
        )));
        let clipboard = events.iter().filter_map(|event| match event {
            SessionEvent::ClipboardStore { kind: 0, text } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(clipboard.collect::<Vec<_>>(), ["new clipboard value"]);
        assert!(!events
            .iter()
            .any(|event| matches!(event, SessionEvent::ClipboardOverflow)));
    }

    #[test]
    fn deferred_notifications_do_not_overtake_newer_queued_notifications() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        shared.state.lock().unwrap().runtime = Some(runtime);
        for _ in 0..MAX_PENDING_REQUESTS {
            shared.delegate.send(Notification::Action(Action::RingBell));
        }

        shared.delegate.send(Notification::Desktop {
            title: String::from("old"),
            body: String::from("old body"),
        });
        shared.delegate.send(Notification::Desktop {
            title: String::from("new"),
            body: String::from("new body"),
        });
        drain_notifications(&shared).unwrap();

        let state = shared.state.lock().unwrap();
        let titles = state
            .runtime
            .as_ref()
            .unwrap()
            .pending_events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::DesktopNotification { title, .. } => Some(title.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(titles, ["old", "new"]);
    }

    #[test]
    fn older_deferred_color_change_precedes_newer_terminal_request() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        shared.state.lock().unwrap().runtime = Some(runtime);

        for _ in 0..MAX_PENDING_REQUESTS {
            shared.delegate.send(Notification::Action(Action::RingBell));
        }
        shared.delegate.send(Notification::ColorChange {
            route_id: 1,
            index: 11,
            color: None,
        });
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        drain_notifications(&shared).unwrap();

        let state = shared.state.lock().unwrap();
        let events = &state.runtime.as_ref().unwrap().pending_events;
        let color_index = events
            .iter()
            .position(|event| {
                matches!(event, SessionEvent::ColorChange { index: 11, .. })
            })
            .unwrap();
        let request_index = events
            .iter()
            .position(|event| matches!(event, SessionEvent::ColorRequest { .. }))
            .unwrap();
        assert!(color_index < request_index);
    }

    #[test]
    fn expired_ordered_request_releases_following_raw_reply() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
        }

        let request_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        let raw = {
            let state = shared.state.lock().unwrap();
            state
                .runtime
                .as_ref()
                .unwrap()
                .surface
                .try_reserve_response(b"raw-reply".to_vec())
                .unwrap()
        };
        assert!(matches!(
            SurfaceDelegate::pty_write(&*shared.delegate, 1, raw).unwrap(),
            librio::PtyWriteResult::Handled
        ));
        drain_notifications(&shared).unwrap();

        let mut state = shared.state.lock().unwrap();
        let runtime = state.runtime.as_mut().unwrap();
        *runtime
            .pending_replies
            .iter_mut()
            .find_map(|reply| match reply {
                PendingReply::Request {
                    request,
                    expires_at,
                    ..
                } if request.id() == request_id => Some(expires_at),
                _ => None,
            })
            .unwrap() = Instant::now() - Duration::from_secs(1);
        assert!(expire_requests(runtime));
        assert_eq!(runtime.pending_replies.len(), 1);
        assert!(matches!(
            runtime.pending_replies.front(),
            Some(PendingReply::PtyWrite(_))
        ));
        assert!(runtime.flush_ordered_replies().unwrap());
        assert!(runtime.pending_replies.is_empty());
        assert_eq!(shared.delegate.pending_requests.load(Ordering::Acquire), 0);
    }

    #[test]
    fn invalid_terminal_response_keeps_request_for_retry() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
        }

        let request_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        shared
            .delegate
            .send_request(RequestKind::ColorRequest, test_color_request);
        let raw = {
            let state = shared.state.lock().unwrap();
            state
                .runtime
                .as_ref()
                .unwrap()
                .surface
                .try_reserve_response(b"raw-reply".to_vec())
                .unwrap()
        };
        assert!(matches!(
            SurfaceDelegate::pty_write(&*shared.delegate, 1, raw).unwrap(),
            librio::PtyWriteResult::Handled
        ));
        drain_notifications(&shared).unwrap();

        let mut state = shared.state.lock().unwrap();
        let runtime = state.runtime.as_mut().unwrap();
        assert!(runtime
            .execute(SessionCommand::ColorResponse {
                request_id,
                route_id: 1,
                color: None,
            })
            .is_err());
        assert_eq!(runtime.pending_replies.len(), 2);
        assert!(matches!(
            runtime.pending_replies.front(),
            Some(PendingReply::Request { request, .. })
                if request.id() == request_id
        ));
        runtime
            .execute(SessionCommand::ColorResponse {
                request_id,
                route_id: 1,
                color: Some([1, 2, 3]),
            })
            .unwrap();
        assert!(runtime.pending_replies.is_empty());
        assert_eq!(shared.delegate.pending_requests.load(Ordering::Acquire), 0);
    }

    #[test]
    fn terminal_request_capacity_refusal_preserves_bound() {
        let shared = test_shared();
        for _ in 0..MAX_PENDING_REQUESTS {
            shared
                .delegate
                .send_request(RequestKind::GlyphProtocolQuery, |request_id| {
                    TerminalRequest::GlyphProtocolQuery {
                        request_id,
                        route_id: 1,
                        cp: b'?' as u32,
                    }
                });
        }
        assert_eq!(
            shared.delegate.pending_requests.load(Ordering::Acquire),
            MAX_PENDING_REQUESTS
        );
        assert_eq!(
            shared
                .delegate
                .deferred
                .lock()
                .unwrap()
                .terminal_replies
                .len(),
            MAX_PENDING_REQUESTS
        );
        let refused_request_id = shared.delegate.next_request_id.load(Ordering::Acquire);
        SurfaceDelegate::glyph_protocol_query(&*shared.delegate, 1, 1, b'!' as u32);
        let notification = shared
            .delegate
            .deferred
            .lock()
            .unwrap()
            .queued
            .pop_front()
            .unwrap();
        assert!(matches!(
            notification,
            Notification::RequestRefused {
                request_id: refused_id,
                kind: RequestKind::GlyphProtocolQuery,
                reason: RequestRefusalReason::Capacity,
            } if refused_id == refused_request_id
        ));
    }

    #[cfg(test)]
    fn readiness_is_signaled(readiness: &readiness::Readiness) -> bool {
        let mut poll_fd = libc::pollfd {
            fd: readiness.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        readiness::wait(std::slice::from_mut(&mut poll_fd), Some(Instant::now()))
            .is_ok_and(|ready| ready == 1)
    }

    #[test]
    fn active_wakeup_follows_canonical_generation() {
        let shared = test_shared();
        let first = Arc::new(readiness::Readiness::new().unwrap());
        let second = Arc::new(readiness::Readiness::new().unwrap());
        shared.state.lock().unwrap().active = Some(Active {
            generation: 1,
            connection_fd: -1,
            wakeup: Arc::clone(&first),
        });

        shared.delegate.wake();
        assert!(readiness_is_signaled(&shared.delegate.wakeup));
        assert!(!readiness_is_signaled(&first));
        shared.delegate.wakeup.clear();
        shared
            .delegate
            .wakeup_pending
            .store(false, Ordering::Release);

        shared.signal_active();
        assert!(readiness_is_signaled(&first));
        first.clear();
        shared.state.lock().unwrap().active = Some(Active {
            generation: 2,
            connection_fd: -1,
            wakeup: Arc::clone(&second),
        });
        shared.signal_active();
        assert!(!readiness_is_signaled(&first));
        assert!(readiness_is_signaled(&second));
    }

    #[test]
    fn wait_deadline_includes_pending_request_expiry() {
        let now = Instant::now();
        let idle = now + Duration::from_secs(30);
        let request = now + TERMINAL_REQUEST_TIMEOUT;
        assert_eq!(next_deadline(Some(idle), Some(request)), Some(request));
        assert_eq!(next_deadline(Some(request), None), Some(request));
    }

    #[test]
    fn expired_request_refreshes_detached_retention() {
        let shared = test_shared();
        let mut runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        runtime.pending_replies.push_back(PendingReply::Request {
            request: TerminalRequest::GlyphProtocolQuery {
                request_id: 7,
                route_id: 1,
                cp: b'?' as u32,
            },
            expires_at: Instant::now() - Duration::from_secs(1),
            response: None,
        });
        runtime
            .pending_events
            .push_back(SessionEvent::GlyphProtocolQuery {
                request_id: 7,
                route_id: 1,
                codepoint: b'?' as u32,
            });
        shared.delegate.pending_requests.store(1, Ordering::Release);
        let before = Instant::now();
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
            state.idle_since = Some(before - Duration::from_secs(1));
        }

        drain_notifications(&shared).unwrap();

        assert!(shared
            .state
            .lock()
            .unwrap()
            .idle_since
            .is_some_and(|idle_since| idle_since >= before));
    }

    #[test]
    fn accepted_request_refreshes_detached_retention() {
        let shared = test_shared();
        let runtime =
            Runtime::new(SessionSpec::default(), Arc::clone(&shared.delegate)).unwrap();
        let before = Instant::now();
        {
            let mut state = shared.state.lock().unwrap();
            state.runtime = Some(runtime);
            state.idle_since = Some(before - Duration::from_secs(1));
        }

        shared
            .delegate
            .send_request(RequestKind::GlyphProtocolQuery, |request_id| {
                TerminalRequest::GlyphProtocolQuery {
                    request_id,
                    route_id: 1,
                    cp: b'?' as u32,
                }
            });
        drain_notifications(&shared).unwrap();

        let state = shared.state.lock().unwrap();
        assert!(state
            .idle_since
            .is_some_and(|idle_since| idle_since >= before));
        assert_eq!(
            state
                .runtime
                .as_ref()
                .unwrap()
                .pending_replies
                .iter()
                .filter(|reply| matches!(reply, PendingReply::Request { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn expired_request_replaces_queued_request_event() {
        let mut events = VecDeque::from([
            SessionEvent::ClipboardLoad {
                request_id: 7,
                route_id: 1,
                kind: 0,
            },
            SessionEvent::Title {
                title: String::from("still pending"),
            },
        ]);
        remove_terminal_request_event(&mut events, 7);
        events.push_back(SessionEvent::RequestExpired {
            request_id: 7,
            kind: RequestKind::ClipboardLoad,
        });

        assert_eq!(events.len(), 2);
        assert!(!events.iter().any(|event| matches!(
            event,
            SessionEvent::ClipboardLoad { request_id: 7, .. }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::RequestExpired { request_id: 7, .. }
        )));
    }

    #[test]
    fn flush_requeues_only_unsent_critical_events() {
        let messages = vec![
            ServerMessage::Event {
                generation: 1,
                event: SessionEvent::ChildExited { status: Some(1) },
            },
            ServerMessage::Event {
                generation: 1,
                event: SessionEvent::Title {
                    title: String::from("sent"),
                },
            },
            ServerMessage::Event {
                generation: 1,
                event: SessionEvent::RequestExpired {
                    request_id: 7,
                    kind: RequestKind::ClipboardLoad,
                },
            },
        ];

        let events = critical_events_from(&messages, 2);
        assert_eq!(
            events,
            vec![SessionEvent::RequestExpired {
                request_id: 7,
                kind: RequestKind::ClipboardLoad,
            }]
        );
    }

    #[test]
    fn endpoint_cleanup_does_not_remove_replacement_socket() {
        let directory = std::env::temp_dir().join(format!(
            "rio-session-endpoint-cleanup-{}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = directory.join("session.sock");
        let guard = EndpointGuard::bind(endpoint.clone()).unwrap();
        fs::remove_file(&endpoint).unwrap();
        let replacement = UnixListener::bind(&endpoint).unwrap();
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600)).unwrap();

        drop(guard);
        assert!(endpoint.exists());

        drop(replacement);
        fs::remove_file(&endpoint).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn inherited_listener_rejects_replaced_endpoint() {
        let directory = std::env::temp_dir().join(format!(
            "rio-session-endpoint-adoption-{}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = directory.join("session.sock");
        let listener = UnixListener::bind(&endpoint).unwrap();
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600)).unwrap();
        let expected_identity = crate::path_identity(&endpoint).unwrap();
        let inherited =
            std::os::fd::IntoRawFd::into_raw_fd(listener.try_clone().unwrap());
        fs::remove_file(&endpoint).unwrap();
        let replacement = UnixListener::bind(&endpoint).unwrap();
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(
            EndpointGuard::from_fd(endpoint.clone(), inherited, expected_identity)
                .is_err()
        );
        assert!(endpoint.exists());

        drop(replacement);
        drop(listener);
        fs::remove_file(&endpoint).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    pub fn read_capability() -> Result<[u8; 32], SessionError> {
        let mut capability = [0; 32];
        let mut input = io::stdin();
        input.read_exact(&mut capability)?;
        let mut extra = [0; 1];
        if input.read(&mut extra)? != 0 {
            return Err(SessionError::invalid(
                "worker capability must contain exactly 32 bytes",
            ));
        }
        if capability.iter().all(|byte| *byte == 0) {
            return Err(SessionError::invalid("worker capability is empty"));
        }
        Ok(capability)
    }
}

pub fn run() -> Result<(), crate::SessionError> {
    #[cfg(unix)]
    {
        let args = unix::parse_args(std::env::args_os().skip(1))?;
        let capability = unix::read_capability()?;
        unix::run(
            args.endpoint,
            args.session_id,
            capability,
            args.listener_fd,
            args.endpoint_identity,
        )
    }
    #[cfg(not(unix))]
    Err(crate::SessionError::unsupported(
        "session workers are currently supported only on Unix",
    ))
}
