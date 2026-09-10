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
    use crate::{SessionError, PREPARED_ATTACHMENT_TIMEOUT};
    use librio::{
        Action, Engine, SelectionKind as RioSelectionKind, Side, Surface,
        SurfaceDelegate, SurfaceDesc,
    };
    use rio_vt::crosswords::pos::{Column as PosColumn, Direction, Line, Pos};
    use rio_vt::crosswords::vi_mode::ViMotion as RioViMotion;
    use std::collections::VecDeque;
    use std::fs;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
    const CONNECTION_TIMEOUT: Duration = Duration::from_millis(250);
    const IDLE_RETENTION: Duration = Duration::from_secs(300);
    const MAX_CONNECTIONS: usize = 8;
    const TERMINAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn release_request_slot(slots: &AtomicUsize) {
        let previous = slots.fetch_sub(1, Ordering::AcqRel);
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

    struct PendingRequest {
        request: TerminalRequest,
        expires_at: Instant,
    }

    fn terminal_request_event(
        request: &TerminalRequest,
    ) -> Result<SessionEvent, SessionError> {
        let request_id = request.id();
        let protocol_route_id = u64::try_from(request.route_id()).map_err(|_| {
            SessionError::invalid("terminal route id exceeds protocol range")
        })?;
        match request {
            TerminalRequest::ClipboardLoad { kind, .. } => {
                Ok(SessionEvent::ClipboardLoad {
                    request_id,
                    route_id: protocol_route_id,
                    kind: *kind as u8,
                })
            }
            TerminalRequest::ColorRequest { index, .. } => {
                Ok(SessionEvent::ColorRequest {
                    request_id,
                    route_id: protocol_route_id,
                    index: (*index).try_into().map_err(|_| {
                        SessionError::invalid(
                            "color request index exceeds protocol range",
                        )
                    })?,
                })
            }
            TerminalRequest::TextAreaSizeRequest { .. } => {
                Ok(SessionEvent::TextAreaSizeRequest {
                    request_id,
                    route_id: protocol_route_id,
                })
            }
            TerminalRequest::GlyphProtocolQuery { cp, .. } => {
                Ok(SessionEvent::GlyphProtocolQuery {
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
        TerminalRequest(TerminalRequest),
        RequestRefused {
            request_id: u64,
            kind: RequestKind,
            reason: RequestRefusalReason,
        },
    }

    #[derive(Default)]
    struct DeferredNotifications {
        title: Option<String>,
        progress: Option<(u8, u8)>,
        bell: bool,
        cursor_blinking: bool,
        clipboard_overflow: bool,
        child_exited: Option<Option<i32>>,
        closed: bool,
        terminal_requests: VecDeque<TerminalRequest>,
        request_refused: VecDeque<(u64, RequestKind, RequestRefusalReason)>,
        desktop_notifications: VecDeque<(String, String)>,
        color_changes: VecDeque<(usize, usize, Option<librio::ColorRgb>)>,
    }

    impl DeferredNotifications {
        fn drain_into(&mut self, notifications: &mut Vec<Notification>) {
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
            if self.clipboard_overflow {
                self.clipboard_overflow = false;
                notifications.push(Notification::ClipboardOverflow);
            }
            if let Some(status) = self.child_exited.take() {
                notifications.push(Notification::ChildExited(status));
            }
            if self.closed {
                self.closed = false;
                notifications.push(Notification::Closed);
            }
            notifications.extend(
                self.terminal_requests
                    .drain(..)
                    .map(Notification::TerminalRequest),
            );
            notifications.extend(self.request_refused.drain(..).map(
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

    #[derive(Clone)]
    struct Delegate {
        sender: SyncSender<Notification>,
        critical_sender: SyncSender<Notification>,
        wakeup_pending: Arc<AtomicBool>,
        deferred: Arc<Mutex<DeferredNotifications>>,
        next_request_id: Arc<AtomicU64>,
        pending_requests: Arc<AtomicUsize>,
    }

    impl Delegate {
        fn allocate_request(&self) -> Option<u64> {
            let reserved = self.pending_requests.try_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |current| (current < MAX_PENDING_REQUESTS).then_some(current + 1),
            );
            if reserved.is_err() {
                return None;
            }
            match self
                .next_request_id
                .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
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
                .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
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
            self.send_critical(Notification::RequestRefused {
                request_id,
                kind,
                reason,
            });
        }

        fn send_request(&self, request: TerminalRequest) {
            let kind = request.kind();
            let request_id = request.id();
            self.wakeup_pending.store(true, Ordering::Release);
            if let Err(error) =
                self.sender.try_send(Notification::TerminalRequest(request))
            {
                let notification = match error {
                    mpsc::TrySendError::Full(notification)
                    | mpsc::TrySendError::Disconnected(notification) => notification,
                };
                let Notification::TerminalRequest(request) = notification else {
                    unreachable!("terminal request sender returned another notification")
                };
                if let Ok(mut deferred) = self.deferred.lock() {
                    if deferred.terminal_requests.len() < MAX_PENDING_REQUESTS {
                        deferred.terminal_requests.push_back(request);
                        return;
                    }
                }
                self.release_request();
                self.request_refused(request_id, kind, RequestRefusalReason::Capacity);
            }
        }

        fn send(&self, notification: Notification) {
            self.wakeup_pending.store(true, Ordering::Release);
            if let Err(error) = self.sender.try_send(notification) {
                let notification = match error {
                    mpsc::TrySendError::Full(notification)
                    | mpsc::TrySendError::Disconnected(notification) => notification,
                };
                if let Ok(mut deferred) = self.deferred.lock() {
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
                        Notification::ClipboardStore { .. }
                        | Notification::ClipboardOverflow => {
                            deferred.clipboard_overflow = true;
                        }
                        Notification::Closed | Notification::ChildExited(_) => {}
                        Notification::Desktop { title, body } => {
                            if deferred.desktop_notifications.len() < MAX_PENDING_REQUESTS
                            {
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
                            } else if deferred.color_changes.len() < MAX_PENDING_REQUESTS
                            {
                                deferred
                                    .color_changes
                                    .push_back((route_id, index, color));
                            }
                        }
                        Notification::TerminalRequest(request) => {
                            let request_id = request.id();
                            let kind = request.kind();
                            if deferred.terminal_requests.len() < MAX_PENDING_REQUESTS {
                                deferred.terminal_requests.push_back(request);
                            } else {
                                self.release_request();
                                Self::defer_request_refusal(
                                    &mut deferred,
                                    request_id,
                                    kind,
                                    RequestRefusalReason::Capacity,
                                );
                            }
                        }
                        Notification::RequestRefused {
                            request_id,
                            kind,
                            reason,
                        } => Self::defer_request_refusal(
                            &mut deferred,
                            request_id,
                            kind,
                            reason,
                        ),
                    }
                }
            }
        }

        fn send_critical(&self, notification: Notification) {
            self.wakeup_pending.store(true, Ordering::Release);
            if let Err(error) = self.critical_sender.try_send(notification) {
                let notification = match error {
                    mpsc::TrySendError::Full(notification)
                    | mpsc::TrySendError::Disconnected(notification) => notification,
                };
                if let Ok(mut deferred) = self.deferred.lock() {
                    match notification {
                        Notification::ChildExited(status) => {
                            deferred.child_exited = Some(status);
                        }
                        Notification::Closed => deferred.closed = true,
                        Notification::RequestRefused {
                            request_id,
                            kind,
                            reason,
                        } => Self::defer_request_refusal(
                            &mut deferred,
                            request_id,
                            kind,
                            reason,
                        ),
                        _ => {}
                    }
                }
            }
        }
    }

    impl SurfaceDelegate for Delegate {
        fn wakeup(&self, _surface: librio::SurfaceId) {
            self.wakeup_pending.store(true, Ordering::Release);
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
            let Some(request_id) = self.allocate_request() else {
                let Some(request_id) = self.fresh_request_id() else {
                    return;
                };
                self.request_refused(
                    request_id,
                    RequestKind::ClipboardLoad,
                    RequestRefusalReason::Capacity,
                );
                return;
            };
            self.send_request(TerminalRequest::ClipboardLoad {
                request_id,
                route_id,
                kind,
                format,
            });
        }

        fn color_request(
            &self,
            _surface: librio::SurfaceId,
            route_id: librio::SurfaceId,
            index: usize,
            format: Arc<dyn Fn(librio::ColorRgb) -> String + Send + Sync>,
        ) {
            let Some(request_id) = self.allocate_request() else {
                let Some(request_id) = self.fresh_request_id() else {
                    return;
                };
                self.request_refused(
                    request_id,
                    RequestKind::ColorRequest,
                    RequestRefusalReason::Capacity,
                );
                return;
            };
            self.send_request(TerminalRequest::ColorRequest {
                request_id,
                route_id,
                index,
                format,
            });
        }

        fn text_area_size_request(
            &self,
            _surface: librio::SurfaceId,
            route_id: librio::SurfaceId,
            format: Arc<dyn Fn(rio_vt::event::WindowSize) -> String + Send + Sync>,
        ) {
            let Some(request_id) = self.allocate_request() else {
                let Some(request_id) = self.fresh_request_id() else {
                    return;
                };
                self.request_refused(
                    request_id,
                    RequestKind::TextAreaSizeRequest,
                    RequestRefusalReason::Capacity,
                );
                return;
            };
            self.send_request(TerminalRequest::TextAreaSizeRequest {
                request_id,
                route_id,
                format,
            });
        }

        fn glyph_protocol_query(
            &self,
            _surface: librio::SurfaceId,
            route_id: librio::SurfaceId,
            cp: u32,
        ) {
            let Some(request_id) = self.allocate_request() else {
                let Some(request_id) = self.fresh_request_id() else {
                    return;
                };
                self.request_refused(
                    request_id,
                    RequestKind::GlyphProtocolQuery,
                    RequestRefusalReason::Capacity,
                );
                return;
            };
            self.send_request(TerminalRequest::GlyphProtocolQuery {
                request_id,
                route_id,
                cp,
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
            self.send_critical(Notification::Closed);
        }

        fn child_exited(&self, _surface: librio::SurfaceId, status: Option<i32>) {
            self.send_critical(Notification::ChildExited(status.map(exit_code)));
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
        pending_requests: Vec<PendingRequest>,
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
            spec.validate()?;
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
            let surface = Engine::new(delegate.clone())
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
                pending_requests: Vec::new(),
                pending_request_slots: Arc::clone(&delegate.pending_requests),
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
            let Some(index) = self
                .pending_requests
                .iter()
                .position(|pending| pending.request.id() == request_id)
            else {
                return Err(SessionError::invalid(
                    "terminal request is unknown or expired",
                ));
            };
            if self.pending_requests[index].request.kind() != kind {
                return Err(SessionError::invalid(
                    "terminal response does not match its request",
                ));
            }
            let expected_route =
                u64::try_from(self.pending_requests[index].request.route_id()).map_err(
                    |_| SessionError::invalid("terminal route id exceeds protocol range"),
                )?;
            if expected_route != route_id {
                return Err(SessionError::invalid(
                    "terminal response does not match its route",
                ));
            }
            if self.pending_requests[index].expires_at <= Instant::now() {
                self.complete_request(index);
                push_event(self, SessionEvent::RequestExpired { request_id, kind });
                return Err(SessionError::invalid("terminal request has expired"));
            }
            Ok(index)
        }

        fn complete_request(&mut self, index: usize) {
            self.pending_requests.swap_remove(index);
            release_request_slot(&self.pending_request_slots);
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
            let matched = match matched {
                Some((start, end)) => Some(SearchMatch {
                    start_line: u32::try_from(
                        start.row.0.checked_add(history).ok_or_else(|| {
                            SessionError::invalid(
                                "search match is outside the session range",
                            )
                        })?,
                    )
                    .map_err(|_| {
                        SessionError::invalid("search match is outside the session range")
                    })?,
                    start_column: u16::try_from(start.col.0).map_err(|_| {
                        SessionError::invalid("search match is outside the session range")
                    })?,
                    end_line: u32::try_from(end.row.0.checked_add(history).ok_or_else(
                        || {
                            SessionError::invalid(
                                "search match is outside the session range",
                            )
                        },
                    )?)
                    .map_err(|_| {
                        SessionError::invalid("search match is outside the session range")
                    })?,
                    end_column: u16::try_from(end.col.0).map_err(|_| {
                        SessionError::invalid("search match is outside the session range")
                    })?,
                }),
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
            command.validate()?;
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
                    frame.validate()?;
                    SessionReply::Frame(frame)
                }
                SessionCommand::SnapshotSince { base_sequence } => {
                    let update = self
                        .snapshots
                        .snapshot_since(&self.surface, base_sequence)?;
                    update.validate()?;
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
                } => {
                    let pending = self.pending_request(
                        request_id,
                        RequestKind::ClipboardLoad,
                        route_id,
                    )?;
                    let TerminalRequest::ClipboardLoad { format, .. } =
                        &self.pending_requests[pending].request
                    else {
                        unreachable!("pending request kind was checked")
                    };
                    let bytes = Self::response_bytes(format(&text))?;
                    self.surface
                        .try_write_response(bytes)
                        .map_err(input_error)?;
                    self.complete_request(pending);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::ColorResponse {
                    request_id,
                    route_id,
                    color,
                } => {
                    let Some([r, g, b]) = color else {
                        return Err(SessionError::unsupported(
                            "unset terminal colors need a host fallback",
                        ));
                    };
                    let pending = self.pending_request(
                        request_id,
                        RequestKind::ColorRequest,
                        route_id,
                    )?;
                    let TerminalRequest::ColorRequest { format, .. } =
                        &self.pending_requests[pending].request
                    else {
                        unreachable!("pending request kind was checked")
                    };
                    let bytes =
                        Self::response_bytes(format(librio::ColorRgb { r, g, b }))?;
                    self.surface
                        .try_write_response(bytes)
                        .map_err(input_error)?;
                    self.complete_request(pending);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::TextAreaSizeResponse {
                    request_id,
                    route_id,
                    rows,
                    columns,
                    pixel_width,
                    pixel_height,
                } => {
                    let pending = self.pending_request(
                        request_id,
                        RequestKind::TextAreaSizeRequest,
                        route_id,
                    )?;
                    let TerminalRequest::TextAreaSizeRequest { format, .. } =
                        &self.pending_requests[pending].request
                    else {
                        unreachable!("pending request kind was checked")
                    };
                    let bytes =
                        Self::response_bytes(format(rio_vt::event::WindowSize {
                            rows,
                            cols: columns,
                            width: pixel_width,
                            height: pixel_height,
                        }))?;
                    self.surface
                        .try_write_response(bytes)
                        .map_err(input_error)?;
                    self.complete_request(pending);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
                SessionCommand::GlyphProtocolResponse {
                    request_id,
                    route_id,
                    status,
                } => {
                    let pending = self.pending_request(
                        request_id,
                        RequestKind::GlyphProtocolQuery,
                        route_id,
                    )?;
                    let TerminalRequest::GlyphProtocolQuery { cp, .. } =
                        &self.pending_requests[pending].request
                    else {
                        unreachable!("pending request kind was checked")
                    };
                    let bytes = Self::response_bytes(
                        rio_vt::ansi::glyph_protocol::format_query_response(
                            *cp,
                            glyph_status(status),
                        ),
                    )?;
                    self.surface
                        .try_write_response(bytes)
                        .map_err(input_error)?;
                    self.complete_request(pending);
                    self.frame_pending = true;
                    SessionReply::Accepted
                }
            };
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
    }

    struct Shared {
        gate: Mutex<()>,
        runtime: Mutex<Option<Runtime>>,
        active: Mutex<Option<Active>>,
        notifications: Mutex<Receiver<Notification>>,
        critical_notifications: Mutex<Receiver<Notification>>,
        next_generation: AtomicU64,
        connection_count: AtomicUsize,
        closing: AtomicBool,
        close_pending: AtomicBool,
        idle_since: Mutex<Option<Instant>>,
        connections: Mutex<Vec<(RawFd, UnixStream)>>,
        session_id: SessionId,
        capability: [u8; 32],
        delegate: Arc<Delegate>,
    }

    struct ConnectionGuard {
        shared: Arc<Shared>,
        fd: RawFd,
    }

    impl Drop for ConnectionGuard {
        fn drop(&mut self) {
            self.shared.unregister_connection(self.fd);
            self.shared.release_connection_slot();
        }
    }

    impl Shared {
        fn new(
            session_id: SessionId,
            capability: [u8; 32],
            receiver: Receiver<Notification>,
            critical_receiver: Receiver<Notification>,
            delegate: Arc<Delegate>,
        ) -> Self {
            Self {
                gate: Mutex::new(()),
                runtime: Mutex::new(None),
                active: Mutex::new(None),
                notifications: Mutex::new(receiver),
                critical_notifications: Mutex::new(critical_receiver),
                next_generation: AtomicU64::new(1),
                connection_count: AtomicUsize::new(0),
                closing: AtomicBool::new(false),
                close_pending: AtomicBool::new(false),
                idle_since: Mutex::new(Some(Instant::now())),
                connections: Mutex::new(Vec::new()),
                session_id,
                capability,
                delegate,
            }
        }

        fn set_idle(&self, idle: bool) {
            if let Ok(mut since) = self.idle_since.lock() {
                *since = if idle { Some(Instant::now()) } else { None };
            }
        }

        fn expired(&self) -> bool {
            self.idle_since
                .lock()
                .ok()
                .and_then(|since| *since)
                .is_some_and(|since| since.elapsed() >= IDLE_RETENTION)
        }

        fn refresh_idle_after_terminal_event(&self) {
            let Ok(_gate) = self.gate.lock() else {
                return;
            };
            let detached = self
                .active
                .lock()
                .ok()
                .is_some_and(|active| active.is_none());
            if detached {
                self.set_idle(true);
            }
        }

        fn register_connection(
            &self,
            stream: &UnixStream,
        ) -> Result<RawFd, SessionError> {
            let copy = stream.try_clone()?;
            // Keep the registry's fd alive until the guard unregisters it. The
            // handler's stream can be dropped before its guard, so using that
            // fd as the key would allow reuse to unregister a newer peer.
            let fd = copy.as_raw_fd();
            self.connections
                .lock()
                .map_err(|_| SessionError::protocol("worker connection lock poisoned"))?
                .push((fd, copy));
            Ok(fd)
        }

        fn unregister_connection(&self, fd: RawFd) {
            if let Ok(mut connections) = self.connections.lock() {
                if let Some(index) =
                    connections.iter().position(|(value, _)| *value == fd)
                {
                    connections.swap_remove(index);
                }
            }
        }

        fn close_connection(&self, fd: RawFd) {
            if let Ok(connections) = self.connections.lock() {
                if let Some((_, stream)) =
                    connections.iter().find(|(value, _)| *value == fd)
                {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }

        fn close_connections(&self) {
            if let Ok(connections) = self.connections.lock() {
                for (_, stream) in connections.iter() {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }

        fn release_connection_slot(&self) {
            if self
                .connection_count
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_sub(1)
                })
                .is_err()
            {
                self.closing.store(true, Ordering::Release);
            }
        }

        fn next_generation(&self) -> Result<u64, SessionError> {
            let mut current = self.next_generation.load(Ordering::Acquire);
            loop {
                if current == 0 || current == u64::MAX {
                    return Err(SessionError::protocol(
                        "attachment generation exhausted",
                    ));
                }
                match self.next_generation.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(current),
                    Err(next) => current = next,
                }
            }
        }
    }

    pub fn run(
        endpoint: PathBuf,
        session_id: SessionId,
        capability: [u8; 32],
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
        let guard = EndpointGuard::bind(endpoint)?;
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_REQUESTS);
        let (critical_sender, critical_receiver) = mpsc::sync_channel(8);
        let delegate = Arc::new(Delegate {
            sender,
            critical_sender,
            wakeup_pending: Arc::new(AtomicBool::new(false)),
            deferred: Arc::new(Mutex::new(DeferredNotifications::default())),
            next_request_id: Arc::new(AtomicU64::new(1)),
            pending_requests: Arc::new(AtomicUsize::new(0)),
        });
        let shared = Arc::new(Shared::new(
            session_id,
            capability,
            receiver,
            critical_receiver,
            delegate,
        ));

        let mut connection_threads: Vec<thread::JoinHandle<()>> = Vec::new();
        let result = loop {
            if shared.closing.load(Ordering::Acquire) {
                break Ok(());
            }
            if let Err(error) = drain_notifications(&shared) {
                break Err(error);
            }
            {
                let _gate = shared.gate.lock().expect("worker state lock poisoned");
                if shared.expired() {
                    // Fence new commits before leaving the loop. A commit that
                    // won the gate first has already cancelled idle retention.
                    shared.closing.store(true, Ordering::Release);
                    break Ok(());
                }
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
            match guard.listener.accept() {
                Ok((stream, _)) => {
                    let accepted = shared.connection_count.try_update(
                        Ordering::AcqRel,
                        Ordering::Acquire,
                        |count| (count < MAX_CONNECTIONS).then_some(count + 1),
                    );
                    if accepted.is_err() {
                        drop(stream);
                        continue;
                    }
                    let fd = match shared.register_connection(&stream) {
                        Ok(fd) => fd,
                        Err(error) => {
                            shared.release_connection_slot();
                            drop(stream);
                            break Err(error);
                        }
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
                            shared.release_connection_slot();
                            break Err(SessionError::Io(error));
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
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
        if let Some(spec) = &hello.3 {
            if let Err(error) = spec.validate() {
                let _ = send_error(&mut stream, error_code(&error), &error.to_string());
                return Ok(());
            }
        }

        let generation =
            match establish_attachment(&shared, &mut stream, connection_fd, hello.3) {
                Ok(generation) => generation,
                Err(error) => {
                    if !matches!(error, SessionError::Io(_) | SessionError::Codec(_)) {
                        let _ = send_error(
                            &mut stream,
                            error_code(&error),
                            &error.to_string(),
                        );
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
        let result = run_attachment(&shared, &mut stream, generation);
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
    ) -> Result<u64, SessionError> {
        let current_generation =
            {
                let _gate = shared
                    .gate
                    .lock()
                    .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
                if shared.closing.load(Ordering::Acquire)
                    || shared.close_pending.load(Ordering::Acquire)
                {
                    return Err(SessionError::Detached);
                }
                let active = shared.active.lock().map_err(|_| {
                    SessionError::protocol("worker attachment lock poisoned")
                })?;
                let mut runtime = shared.runtime.lock().map_err(|_| {
                    SessionError::protocol("worker runtime lock poisoned")
                })?;
                if runtime.is_none() {
                    let spec = match spec {
                        Some(spec) => spec,
                        None => {
                            return Err(SessionError::invalid(
                                "first attachment needs a session spec",
                            ));
                        }
                    };
                    let new_runtime = Runtime::new(spec, Arc::clone(&shared.delegate))?;
                    *runtime = Some(new_runtime);
                }
                active.as_ref().map(|active| active.generation)
            };

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
        let frame =
            {
                let _gate = shared
                    .gate
                    .lock()
                    .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
                if shared.closing.load(Ordering::Acquire)
                    || shared.close_pending.load(Ordering::Acquire)
                {
                    return Err(SessionError::Detached);
                }
                let active = shared.active.lock().map_err(|_| {
                    SessionError::protocol("worker attachment lock poisoned")
                })?;
                if let Some(active) = active.as_ref() {
                    if Some(active.generation) != current_generation {
                        return Err(SessionError::protocol("stale attachment claim"));
                    }
                }
                let mut runtime = shared.runtime.lock().map_err(|_| {
                    SessionError::protocol("worker runtime lock poisoned")
                })?;
                let runtime = runtime
                    .as_mut()
                    .ok_or_else(|| SessionError::protocol("session runtime missing"))?;
                let frame = runtime.snapshots.full_frame(&runtime.surface)?;
                frame.validate()?;
                frame
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
        read_prepared_commit(stream, generation, Instant::now())?;

        let old_fd = {
            let _gate = shared
                .gate
                .lock()
                .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
            if shared.closing.load(Ordering::Acquire)
                || shared.close_pending.load(Ordering::Acquire)
            {
                return Err(SessionError::Detached);
            }
            let mut active = shared
                .active
                .lock()
                .map_err(|_| SessionError::protocol("worker attachment lock poisoned"))?;
            if let Some(active) = active.as_ref() {
                if Some(active.generation) != current_generation {
                    return Err(SessionError::protocol("stale attachment commit"));
                }
            }
            let old_fd = active.replace(Active {
                generation,
                connection_fd,
            });
            if let Some(runtime) = shared
                .runtime
                .lock()
                .map_err(|_| SessionError::protocol("worker runtime lock poisoned"))?
                .as_mut()
            {
                runtime.frame_pending = true;
            }
            shared.set_idle(false);
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
    ) -> Result<(), SessionError> {
        let mut last_request_id = 0;
        loop {
            if !is_active(shared, generation) {
                return Ok(());
            }
            drain_notifications(shared)?;
            flush_events(shared, stream, generation)?;

            match read_client_message(stream)? {
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
                        let _gate = shared.gate.lock().map_err(|_| {
                            SessionError::protocol("worker state lock poisoned")
                        })?;
                        if !is_active(shared, generation)
                            || command_generation != generation
                        {
                            (Err(SessionError::protocol("stale attachment")), false, true)
                        } else {
                            let result = shared
                                .runtime
                                .lock()
                                .map_err(|_| {
                                    SessionError::protocol("worker runtime lock poisoned")
                                })?
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
                            if let Err(error) = reply.validate() {
                                send_error(
                                    stream,
                                    error_code(&error),
                                    &error.to_string(),
                                )?;
                                return Ok(());
                            }
                            if close {
                                // Reply first: the client must observe explicit close before
                                // the worker tears down its tracked sockets.
                                let write_result = codec::write_frame_until(
                                    stream,
                                    &ServerMessage::Reply { request_id, reply },
                                    Instant::now() + HANDSHAKE_TIMEOUT,
                                );
                                shared.closing.store(true, Ordering::Release);
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
    ) -> Result<Option<ClientMessage>, SessionError> {
        let mut poll_fd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(stream),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready =
            unsafe { libc::poll(&mut poll_fd, 1, CONNECTION_TIMEOUT.as_millis() as i32) };
        if ready == 0 {
            return Ok(None);
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(error.into());
        }
        let message = codec::read_frame_until(stream, Instant::now() + HANDSHAKE_TIMEOUT);
        stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
        message.map(Some)
    }

    fn is_active(shared: &Shared, generation: u64) -> bool {
        shared.active.lock().ok().is_some_and(|active| {
            active
                .as_ref()
                .is_some_and(|value| value.generation == generation)
        })
    }

    fn detach(shared: &Shared, generation: u64) {
        // Serialize clearing ownership and arming retention with replacement
        // commits, which disable retention under this same gate.
        let _gate = shared.gate.lock().expect("worker state lock poisoned");
        let detached = if let Ok(mut active) = shared.active.lock() {
            if active
                .as_ref()
                .is_some_and(|value| value.generation == generation)
            {
                *active = None;
                true
            } else {
                false
            }
        } else {
            false
        };
        if detached {
            if let Ok(mut runtime) = shared.runtime.lock() {
                if let Some(runtime) = runtime.as_mut() {
                    let requests = runtime
                        .pending_requests
                        .iter()
                        .filter_map(|pending| {
                            terminal_request_event(&pending.request).ok()
                        })
                        .collect::<Vec<_>>();
                    for event in requests {
                        push_event(runtime, event);
                    }
                    if let Some(status) = runtime.child_exit_status {
                        push_event(runtime, SessionEvent::ChildExited { status });
                    }
                    if runtime.terminal_closed {
                        push_event(runtime, SessionEvent::Closed);
                    }
                }
            }
            shared.set_idle(true);
        }
    }

    fn drain_notifications(shared: &Shared) -> Result<(), SessionError> {
        let mut notifications = Vec::new();
        {
            let receiver = shared.notifications.lock().map_err(|_| {
                SessionError::protocol("worker notification lock poisoned")
            })?;
            let critical_receiver =
                shared.critical_notifications.lock().map_err(|_| {
                    SessionError::protocol("worker critical notification lock poisoned")
                })?;
            loop {
                let notification = match critical_receiver.try_recv() {
                    Ok(notification) => Some(notification),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                        receiver.try_recv().ok()
                    }
                };
                let Some(notification) = notification else {
                    break;
                };
                notifications.push(notification);
            }
        }
        if let Ok(mut deferred) = shared.delegate.deferred.lock() {
            deferred.drain_into(&mut notifications);
        }
        let terminal_event = {
            let mut runtime = shared
                .runtime
                .lock()
                .map_err(|_| SessionError::protocol("worker runtime lock poisoned"))?;
            let Some(runtime) = runtime.as_mut() else {
                return Ok(());
            };
            expire_requests(runtime);
            if shared.delegate.wakeup_pending.swap(false, Ordering::AcqRel) {
                runtime.frame_pending = true;
            }
            let mut terminal_event = false;
            for notification in notifications {
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
                        push_event(runtime, event);
                    }
                    Notification::ClipboardStore { kind, text } => {
                        push_event(
                            runtime,
                            SessionEvent::ClipboardStore {
                                kind: kind as u8,
                                text,
                            },
                        );
                    }
                    Notification::Closed => {
                        runtime.terminal_closed = true;
                        terminal_event = true;
                        push_event(runtime, SessionEvent::Closed);
                    }
                    Notification::ChildExited(status) => {
                        runtime.child_exit_status = Some(status);
                        runtime.frame_pending = true;
                        terminal_event = true;
                        push_event(runtime, SessionEvent::ChildExited { status });
                    }
                    Notification::ClipboardOverflow => {
                        push_event(runtime, SessionEvent::ClipboardOverflow);
                    }
                    Notification::TerminalRequest(request) => {
                        let request_id = request.id();
                        let kind = request.kind();
                        let event = match terminal_request_event(&request) {
                            Ok(event) => event,
                            Err(_) => {
                                shared.delegate.release_request();
                                push_event(
                                    runtime,
                                    SessionEvent::RequestRefused {
                                        request_id,
                                        kind,
                                        reason: RequestRefusalReason::Unsupported,
                                    },
                                );
                                continue;
                            }
                        };
                        if runtime.pending_requests.len() >= MAX_PENDING_REQUESTS {
                            push_event(
                                runtime,
                                SessionEvent::RequestRefused {
                                    request_id,
                                    kind,
                                    reason: RequestRefusalReason::Capacity,
                                },
                            );
                            shared.delegate.release_request();
                            continue;
                        }
                        if push_event(runtime, event) {
                            runtime.pending_requests.push(PendingRequest {
                                request,
                                expires_at: Instant::now() + TERMINAL_REQUEST_TIMEOUT,
                            });
                        } else {
                            shared.delegate.release_request();
                            if !push_event(
                                runtime,
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
                    Notification::RequestRefused {
                        request_id,
                        kind,
                        reason,
                    } => {
                        push_event(
                            runtime,
                            SessionEvent::RequestRefused {
                                request_id,
                                kind,
                                reason,
                            },
                        );
                    }
                    Notification::Desktop { title, body } => {
                        push_event(
                            runtime,
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
                        push_event(
                            runtime,
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
            terminal_event
        };
        if terminal_event {
            shared.refresh_idle_after_terminal_event();
        }
        Ok(())
    }

    fn expire_requests(runtime: &mut Runtime) {
        let now = Instant::now();
        let mut expired = Vec::new();
        let mut pending = Vec::with_capacity(runtime.pending_requests.len());
        for request in runtime.pending_requests.drain(..) {
            if request.expires_at <= now {
                expired.push((request.request.id(), request.request.kind()));
                release_request_slot(&runtime.pending_request_slots);
            } else {
                pending.push(request);
            }
        }
        runtime.pending_requests = pending;
        for (request_id, kind) in expired {
            push_event(runtime, SessionEvent::RequestExpired { request_id, kind });
        }
    }

    fn push_event(runtime: &mut Runtime, mut event: SessionEvent) -> bool {
        if let Some(request_id) = terminal_request_event_id(&event) {
            if runtime
                .pending_events
                .iter()
                .any(|pending| terminal_request_event_id(pending) == Some(request_id))
            {
                return true;
            }
        }
        if matches!(
            event,
            SessionEvent::Title { .. } | SessionEvent::Progress { .. }
        ) {
            runtime.pending_events.retain(|pending| {
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
            runtime.pending_events.retain(|pending| {
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
        ) && runtime.pending_events.iter().any(|pending| {
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
            && runtime
                .pending_events
                .iter()
                .any(|pending| matches!(pending, SessionEvent::FrameReady))
        {
            return true;
        }
        if runtime.pending_events.len() >= MAX_PENDING_REQUESTS {
            let removable = runtime
                .pending_events
                .iter()
                .position(|pending| !is_critical(pending));
            if let Some(index) = removable {
                let _ = runtime.pending_events.remove(index);
            } else if matches!(event, SessionEvent::ClipboardStore { .. }) {
                if runtime
                    .pending_events
                    .iter()
                    .any(|pending| matches!(pending, SessionEvent::ClipboardOverflow))
                {
                    return true;
                }
                event = SessionEvent::ClipboardOverflow;
            } else {
                if matches!(
                    event,
                    SessionEvent::Closed | SessionEvent::ChildExited { .. }
                ) {
                    runtime.pending_events.clear();
                    runtime.pending_events.push_back(event);
                    return true;
                }
                return false;
            }
        }
        runtime.pending_events.push_back(event);
        true
    }

    fn is_critical(event: &SessionEvent) -> bool {
        matches!(
            event,
            SessionEvent::ChildExited { .. }
                | SessionEvent::ClipboardOverflow
                | SessionEvent::ClipboardLoad { .. }
                | SessionEvent::ColorRequest { .. }
                | SessionEvent::TextAreaSizeRequest { .. }
                | SessionEvent::GlyphProtocolQuery { .. }
                | SessionEvent::ColorChange { .. }
                | SessionEvent::RequestRefused { .. }
                | SessionEvent::RequestExpired { .. }
                | SessionEvent::DesktopNotification { .. }
                | SessionEvent::Closed
        )
    }

    fn terminal_request_event_id(event: &SessionEvent) -> Option<u64> {
        match event {
            SessionEvent::ClipboardLoad { request_id, .. }
            | SessionEvent::ColorRequest { request_id, .. }
            | SessionEvent::TextAreaSizeRequest { request_id, .. }
            | SessionEvent::GlyphProtocolQuery { request_id, .. } => Some(*request_id),
            _ => None,
        }
    }

    fn flush_events(
        shared: &Shared,
        stream: &mut UnixStream,
        generation: u64,
    ) -> Result<(), SessionError> {
        let mut messages = Vec::new();
        {
            let _gate = shared
                .gate
                .lock()
                .map_err(|_| SessionError::protocol("worker state lock poisoned"))?;
            if !is_active(shared, generation) {
                return Ok(());
            }
            let mut runtime = shared
                .runtime
                .lock()
                .map_err(|_| SessionError::protocol("worker runtime lock poisoned"))?;
            let Some(runtime) = runtime.as_mut() else {
                return Ok(());
            };
            if runtime.frame_pending {
                runtime.frame_pending = false;
                push_event(runtime, SessionEvent::FrameReady);
            }
            messages.extend(
                runtime
                    .pending_events
                    .drain(..)
                    .map(|event| ServerMessage::Event { generation, event }),
            );
        }
        let critical = messages
            .iter()
            .filter_map(|message| match message {
                ServerMessage::Event { event, .. } if is_critical(event) => {
                    Some(event.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Err(error) = messages.iter().try_for_each(ServerMessage::validate) {
            requeue_critical(shared, critical);
            return Err(error);
        }
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let result = messages
            .iter()
            .try_for_each(|message| codec::write_frame_until(stream, message, deadline));
        if result.is_err() || !is_active(shared, generation) {
            requeue_critical(shared, critical);
        }
        result
    }

    fn requeue_critical(shared: &Shared, events: Vec<SessionEvent>) {
        let Ok(_gate) = shared.gate.lock() else {
            return;
        };
        let Ok(mut runtime) = shared.runtime.lock() else {
            return;
        };
        if let Some(runtime) = runtime.as_mut() {
            for event in events {
                push_event(runtime, event);
            }
        }
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
    }

    impl EndpointGuard {
        fn bind(endpoint: PathBuf) -> Result<Self, SessionError> {
            let parent = endpoint
                .parent()
                .ok_or_else(|| SessionError::invalid("session endpoint has no parent"))?;
            let parent_metadata = fs::symlink_metadata(parent)?;
            let uid = unsafe { libc::geteuid() };
            if !parent_metadata.is_dir()
                || parent_metadata.uid() != uid
                || parent_metadata.mode() & 0o077 != 0
            {
                return Err(SessionError::protocol(
                    "session endpoint directory is not private",
                ));
            }
            let previous_umask = unsafe { libc::umask(0o177) };
            let listener_result = UnixListener::bind(&endpoint);
            unsafe { libc::umask(previous_umask) };
            let listener = listener_result?;
            listener.set_nonblocking(true)?;
            let metadata = match fs::symlink_metadata(&endpoint) {
                Ok(metadata) => metadata,
                Err(error) => {
                    let _ = fs::remove_file(&endpoint);
                    return Err(error.into());
                }
            };
            if !metadata.file_type().is_socket()
                || metadata.uid() != uid
                || metadata.mode() & 0o777 != 0o600
            {
                let _ = fs::remove_file(&endpoint);
                return Err(SessionError::protocol("session endpoint is not private"));
            }
            Ok(Self { listener, endpoint })
        }
    }

    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.endpoint);
            if let Some(parent) = self.endpoint.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }

    pub fn parse_args(
        args: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> Result<(PathBuf, SessionId), SessionError> {
        let mut args = args.into_iter();
        let mut endpoint = None;
        let mut session_id = None;
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
        Ok((endpoint, session_id))
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
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_REQUESTS);
        let (critical_sender, critical_receiver) = mpsc::sync_channel(8);
        let delegate = Arc::new(Delegate {
            sender,
            critical_sender,
            wakeup_pending: Arc::new(AtomicBool::new(false)),
            deferred: Arc::new(Mutex::new(DeferredNotifications::default())),
            next_request_id: Arc::new(AtomicU64::new(1)),
            pending_requests: Arc::new(AtomicUsize::new(0)),
        });
        let shared = Arc::new(Shared::new(
            SessionId([1; 16]),
            [1; 32],
            receiver,
            critical_receiver,
            delegate,
        ));
        *shared.active.lock().unwrap() = Some(Active {
            generation: 1,
            connection_fd: -1,
        });
        shared.set_idle(false);
        let gate = shared.gate.lock().unwrap();
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
        *shared.active.lock().unwrap() = Some(Active {
            generation: 2,
            connection_fd: -1,
        });
        shared.set_idle(false);
        drop(gate);
        thread.join().unwrap();
        assert!(shared.idle_since.lock().unwrap().is_none());
        assert!(is_active(&shared, 2));
        detach(&shared, 2);
        assert!(shared.idle_since.lock().unwrap().is_some());
        assert!(!shared.expired());
        *shared.idle_since.lock().unwrap() = Some(Instant::now() - IDLE_RETENTION);
        assert!(shared.expired());
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
        let (endpoint, session_id) = unix::parse_args(std::env::args_os().skip(1))?;
        let capability = unix::read_capability()?;
        unix::run(endpoint, session_id, capability)
    }
    #[cfg(not(unix))]
    Err(crate::SessionError::unsupported(
        "session workers are currently supported only on Unix",
    ))
}
