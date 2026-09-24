//! Passive session ownership for a terminal pane.
//!
//! The GUI never owns a parser or a grid.  A small background pump owns the
//! authenticated `SessionClient`, serializes commands onto the worker, and
//! publishes validated frames for the renderer.  `RemoteView` contains only a
//! cache of that published data; methods which look like terminal operations
//! enqueue a worker command and never mutate an authoritative terminal.

use super::renderable::{PendingUpdate, RenderableContent};
use rio_backend::ansi::CursorShape;
use rio_backend::clipboard::ClipboardType;
use rio_backend::config::colors::{AnsiColor, ColorRgb, NamedColor};
use rio_backend::crosswords::grid::row::Row;
use rio_backend::crosswords::grid::{Dimensions, Scroll};
use rio_backend::crosswords::pos::{Column, CursorState, Line, Pos};
use rio_backend::crosswords::square::{CellFlags, Extras, Hyperlink, Square, Wide};
use rio_backend::crosswords::style::{Style, StyleFlags};
use rio_backend::crosswords::Mode;
use rio_backend::error::{RioError, RioErrorLevel, RioErrorType};
use rio_backend::event::{EventListener, RioEvent, WindowId, WindowTarget};
use rio_backend::selection::SelectionRange;
use rio_session::protocol::{
    CellContentFrame, CellFrame, ColorFrame, FrameUpdate, FullFrame, RowFrame,
    SearchDirection as WireSearchDirection, SearchMatch, SearchNavigation,
    SelectionFrame, SelectionKind as WireSelectionKind,
    SelectionSide as WireSelectionSide, SessionCommand, SessionDescriptor, SessionEvent,
    SessionReply, StyleFrame, ViMotion as WireViMotion,
};
#[cfg(unix)]
use rio_session::readiness::Readiness;
use rio_session::{PreparedSessionAttachment, SessionClient, SessionError, SessionSpec};
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::ops::{Index, IndexMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
#[cfg(all(test, unix))]
use std::time::Duration;

const COMMAND_QUEUE_SIZE: usize = 256;
const EVENT_QUEUE_SIZE: usize = 64;
const FRAME_UPDATE_QUEUE_SIZE: usize = 64;

type SelectionTextResult = (ClipboardType, Option<String>, bool);

#[cfg(unix)]
type CommandWakeup = Readiness;

#[cfg(not(unix))]
struct CommandWakeup;

fn new_command_wakeup() -> Result<Arc<CommandWakeup>, SessionError> {
    #[cfg(unix)]
    {
        Ok(Arc::new(CommandWakeup::new()?))
    }
    #[cfg(not(unix))]
    {
        Ok(Arc::new(CommandWakeup))
    }
}

struct CommandChannel {
    sender: Option<mpsc::SyncSender<PumpCommand>>,
    wakeup: Arc<CommandWakeup>,
}

impl Drop for CommandChannel {
    fn drop(&mut self) {
        // Disconnect the channel before waking the pump so it cannot observe
        // an empty queue while the last sender is still being destroyed.
        let sender = self.sender.take();
        drop(sender);
        #[cfg(unix)]
        self.wakeup.signal();
    }
}

#[derive(Clone)]
struct CommandSender {
    channel: Arc<CommandChannel>,
}

impl CommandSender {
    fn new(sender: mpsc::SyncSender<PumpCommand>, wakeup: Arc<CommandWakeup>) -> Self {
        Self {
            channel: Arc::new(CommandChannel {
                sender: Some(sender),
                wakeup,
            }),
        }
    }

    fn try_send(
        &self,
        command: PumpCommand,
    ) -> Result<(), mpsc::TrySendError<PumpCommand>> {
        let result = self
            .channel
            .sender
            .as_ref()
            .expect("command sender missing")
            .try_send(command);
        if result.is_ok() {
            #[cfg(unix)]
            self.signal();
        }
        result
    }

    #[cfg(all(unix, test))]
    fn send(&self, command: PumpCommand) -> Result<(), mpsc::SendError<PumpCommand>> {
        let result = self
            .channel
            .sender
            .as_ref()
            .expect("command sender missing")
            .send(command);
        if result.is_ok() {
            self.signal();
        }
        result
    }

    #[cfg(unix)]
    fn signal(&self) {
        self.channel.wakeup.signal();
    }
}

#[derive(Debug)]
enum PumpCommand {
    Terminal(SessionCommand),
    SelectionText {
        target: ClipboardType,
        copy_to_clipboard: bool,
    },
}

enum PumpStartup {
    Frame(Box<FullFrame>),
    Sequence(u64),
    Snapshot,
}

#[derive(Debug, Default)]
struct SelectionTextQueue {
    // Includes queued/in-flight requests and unread replies.
    outstanding: usize,
    replies: VecDeque<SelectionTextResult>,
}

#[derive(Debug, Default)]
struct FrameMailbox {
    pending_full: Option<FullFrame>,
    updates: VecDeque<FrameUpdate>,
}

#[derive(Debug)]
enum SessionStatus {
    Running(Option<String>),
    Closed(Option<String>),
}

#[derive(Debug)]
struct SessionState {
    frames: Mutex<FrameMailbox>,
    events: Mutex<VecDeque<SessionEvent>>,
    status: Mutex<SessionStatus>,
    descriptor: Mutex<Option<SessionDescriptor>>,
    selection_text: Mutex<SelectionTextQueue>,
    search_navigation: Mutex<Option<SearchNavigation>>,
    search_matches: Mutex<Option<Vec<SearchMatch>>>,
    pump_done: AtomicBool,
    frame_resync_logged: AtomicBool,
    had_frame: AtomicBool,
}

impl SessionState {
    fn new() -> Self {
        Self {
            frames: Mutex::new(FrameMailbox::default()),
            events: Mutex::new(VecDeque::new()),
            status: Mutex::new(SessionStatus::Running(None)),
            descriptor: Mutex::new(None),
            selection_text: Mutex::new(SelectionTextQueue::default()),
            search_navigation: Mutex::new(None),
            search_matches: Mutex::new(None),
            pump_done: AtomicBool::new(false),
            frame_resync_logged: AtomicBool::new(false),
            had_frame: AtomicBool::new(false),
        }
    }

    fn fail(&self, error: SessionError) {
        *self.status.lock().expect("session status lock poisoned") =
            SessionStatus::Closed(Some(error.to_string()));
    }

    fn record_error(&self, error: SessionError) {
        if let SessionStatus::Running(slot) =
            &mut *self.status.lock().expect("session status lock poisoned")
        {
            *slot = Some(error.to_string());
        }
    }

    fn error(&self) -> Option<String> {
        match &*self.status.lock().expect("session status lock poisoned") {
            SessionStatus::Running(error) | SessionStatus::Closed(error) => error.clone(),
        }
    }

    fn had_frame(&self) -> bool {
        self.had_frame.load(Ordering::Acquire)
    }

    fn mark_had_frame(&self) {
        self.had_frame.store(true, Ordering::Release);
    }

    fn publish_frame(&self, frame: FullFrame) {
        self.publish_frame_update(FrameUpdate::Full(frame));
    }

    fn publish_frame_update(&self, update: FrameUpdate) -> bool {
        let Ok(mut mailbox) = self.frames.lock() else {
            return false;
        };
        match update {
            FrameUpdate::Full(frame) => {
                mailbox.updates.clear();
                mailbox.pending_full = Some(frame);
                self.frame_resync_logged.store(false, Ordering::Release);
            }
            update => {
                if mailbox.updates.len() >= FRAME_UPDATE_QUEUE_SIZE {
                    return false;
                }
                mailbox.updates.push_back(update);
            }
        }
        drop(mailbox);
        // Both full snapshots and deltas confirm recovery; rejected updates do not.
        self.mark_had_frame();
        if let SessionStatus::Running(error) =
            &mut *self.status.lock().expect("session status lock poisoned")
        {
            *error = None;
        }
        true
    }

    fn take_frame_updates(&self) -> (Option<FullFrame>, Vec<FrameUpdate>) {
        self.frames
            .lock()
            .map(|mut mailbox| {
                (
                    mailbox.pending_full.take(),
                    mailbox.updates.drain(..).collect(),
                )
            })
            .unwrap_or_default()
    }

    fn log_frame_resync_once(&self, base_sequence: u64, error: &SessionError) {
        if self
            .frame_resync_logged
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            tracing::warn!(
                base_sequence,
                error = %error,
                "session frame delta recovery requested"
            );
        }
    }

    fn publish_descriptor(&self, descriptor: SessionDescriptor) {
        if let Ok(mut current) = self.descriptor.lock() {
            *current = Some(descriptor);
        }
    }

    fn publish_event(&self, event: SessionEvent) {
        if let SessionEvent::Closed = event {
            let mut status = self.status.lock().expect("session status lock poisoned");
            if let SessionStatus::Running(error) = &mut *status {
                *status = SessionStatus::Closed(error.take());
            }
        }
        if let Ok(mut events) = self.events.lock() {
            match &event {
                SessionEvent::Title { .. } => {
                    events.retain(|queued| !matches!(queued, SessionEvent::Title { .. }));
                }
                SessionEvent::Progress { .. } => {
                    events.retain(|queued| {
                        !matches!(queued, SessionEvent::Progress { .. })
                    });
                }
                SessionEvent::ColorChange {
                    route_id, index, ..
                } => {
                    events.retain(|queued| {
                        !matches!(
                            queued,
                            SessionEvent::ColorChange {
                                route_id: queued_route,
                                index: queued_index,
                                ..
                            } if queued_route == route_id && queued_index == index
                        )
                    });
                }
                _ => {}
            }
            if events.len() >= EVENT_QUEUE_SIZE {
                // SessionClient already applies the protocol's bounded queue
                // policy.  This second queue is only a GUI wakeup handoff;
                // preserve terminal requests/close by evicting the oldest
                // ordinary event and record pressure for the pane.
                if let Some(index) = events.iter().position(|queued| {
                    !matches!(
                        queued,
                        SessionEvent::ClipboardLoad { .. }
                            | SessionEvent::ColorRequest { .. }
                            | SessionEvent::TextAreaSizeRequest { .. }
                            | SessionEvent::GlyphProtocolQuery { .. }
                            | SessionEvent::DesktopNotification { .. }
                            | SessionEvent::ChildExited { .. }
                            | SessionEvent::ClipboardOverflow
                            | SessionEvent::RequestRefused { .. }
                            | SessionEvent::RequestExpired { .. }
                            | SessionEvent::ColorChange { .. }
                            | SessionEvent::Closed
                    )
                }) {
                    events.remove(index);
                } else {
                    drop(events);
                    self.fail(SessionError::Protocol(
                        "session event handoff is full".to_string(),
                    ));
                    return;
                }
            }
            events.push_back(event);
        }
    }
}

/// A cloneable command/event handle retained by a GUI context.
#[derive(Clone)]
pub struct SessionHandle {
    commands: CommandSender,
    state: Arc<SessionState>,
    window_id: Arc<Mutex<WindowId>>,
    // Serializes command admission and the pump's empty-queue/close decision.
    closed: Arc<Mutex<bool>>,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field(
                "closed",
                &*self.closed.lock().expect("session admission lock poisoned"),
            )
            .finish_non_exhaustive()
    }
}

impl SessionHandle {
    pub fn spawn<T>(
        spec: SessionSpec,
        event_proxy: T,
        route_id: usize,
        window_id: WindowId,
    ) -> Result<Self, SessionError>
    where
        T: EventListener + Clone + Send + 'static,
    {
        let state = Arc::new(SessionState::new());
        let (commands, receiver) = mpsc::sync_channel(COMMAND_QUEUE_SIZE);
        let state_for_thread = Arc::clone(&state);
        let window_for_thread = Arc::new(Mutex::new(window_id));
        let window_for_thread_clone = Arc::clone(&window_for_thread);
        let closed = Arc::new(Mutex::new(false));
        let closed_for_thread = Arc::clone(&closed);
        let command_wakeup = new_command_wakeup()?;
        let command_wakeup_for_thread = Arc::clone(&command_wakeup);
        let commands = CommandSender::new(commands, Arc::clone(&command_wakeup));
        let listener = event_proxy.with_window_target(WindowTarget::dynamic(window_id));

        thread::Builder::new()
            .name(format!("rio-session-{route_id}"))
            .spawn(move || {
                let client = std::env::current_exe()
                    .map_err(SessionError::from)
                    .and_then(|worker| {
                        SessionClient::spawn_with_worker_path(spec, worker)
                    });
                let client = match client {
                    Ok(client) => client,
                    Err(error) => {
                        let window_id = load_window_id(&window_for_thread_clone);
                        report_startup_failure(&listener, window_id, route_id, &error);
                        state_for_thread.fail(error);
                        state_for_thread.pump_done.store(true, Ordering::Release);
                        listener.send_event(RioEvent::RenderRoute(route_id), window_id);
                        return;
                    }
                };
                let startup = {
                    #[cfg(unix)]
                    {
                        match client
                            .take_initial_frame()
                            .expect("session initial frame lock poisoned")
                        {
                            Some(frame) => PumpStartup::Frame(Box::new(frame)),
                            None => PumpStartup::Snapshot,
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        PumpStartup::Snapshot
                    }
                };
                let client = Arc::new(client);
                state_for_thread.publish_descriptor(client.descriptor().clone());
                let pump = SessionPump {
                    client,
                    receiver,
                    state: state_for_thread,
                    event_proxy: listener,
                    route_id,
                    window_id: window_for_thread_clone,
                    closed: closed_for_thread,
                    command_wakeup: command_wakeup_for_thread,
                };
                pump.run(startup);
            })
            .map_err(SessionError::from)?;

        Ok(Self {
            commands,
            state,
            window_id: window_for_thread,
            closed,
        })
    }

    /// Build a passive, disconnected view used only by existing unit tests
    /// that explicitly request a dead context. Production context creation
    /// never calls this path.
    pub fn disconnected_with_error(error: SessionError) -> Self {
        let (commands, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let command_wakeup =
            new_command_wakeup().expect("failed to create session command wakeup");
        let state = Arc::new(SessionState::new());
        state.fail(error);
        state.pump_done.store(true, Ordering::Release);
        Self {
            commands: CommandSender::new(commands, command_wakeup),
            state,
            window_id: Arc::new(Mutex::new(WindowId::from(0))),
            closed: Arc::new(Mutex::new(true)),
        }
    }

    pub fn enqueue(&self, command: SessionCommand) -> Result<(), SessionError> {
        if matches!(command, SessionCommand::Close) {
            self.close();
            return Ok(());
        }
        if matches!(command, SessionCommand::SelectionText) {
            let error = SessionError::Invalid(
                "selection text needs a clipboard destination".into(),
            );
            self.state
                .record_error(SessionError::Invalid(error.to_string()));
            return Err(error);
        }
        self.enqueue_command(PumpCommand::Terminal(command))
    }

    fn enqueue_command(&self, command: PumpCommand) -> Result<(), SessionError> {
        let result = {
            let closed = self.closed.lock().expect("session admission lock poisoned");
            if *closed {
                return Err(SessionError::Detached);
            }
            self.commands.try_send(command)
        }
        .map_err(|error| match error {
            mpsc::TrySendError::Full(_) => {
                SessionError::Invalid("session command queue is full".to_string())
            }
            mpsc::TrySendError::Disconnected(_) => SessionError::Detached,
        });
        if let Err(error) = &result {
            self.state
                .record_error(SessionError::Invalid(error.to_string()));
        }
        result
    }

    pub fn record_command_error(&self, error: SessionError) {
        self.state.record_error(error);
    }

    pub fn close(&self) {
        // Close cannot be rejected by a full data queue. The pump drains all
        // admitted commands before issuing it, even if every sender is gone.
        let should_wake = {
            let mut closed = self.closed.lock().expect("session admission lock poisoned");
            if *closed {
                false
            } else {
                *closed = true;
                true
            }
        };
        if should_wake {
            #[cfg(unix)]
            self.commands.signal();
        }
    }

    pub fn pump_done(&self) -> bool {
        self.state.pump_done.load(Ordering::Acquire)
    }

    pub fn rebind_window(&self, window_id: WindowId) {
        if let Ok(mut current) = self.window_id.lock() {
            *current = window_id;
        }
    }

    pub fn error(&self) -> Option<String> {
        self.state.error()
    }

    pub fn descriptor(&self) -> Option<SessionDescriptor> {
        self.state
            .descriptor
            .lock()
            .ok()
            .and_then(|descriptor| descriptor.clone())
    }

    /// Install an already committed attachment into the same bounded pump
    /// used by locally spawned sessions. `initial_frame` was prepared before
    /// ownership changed, so the target can build its view without a second
    /// snapshot or a parser/grid replica.
    pub fn from_client<T>(
        client: SessionClient,
        initial_sequence: u64,
        event_proxy: T,
        route_id: usize,
        window_id: WindowId,
    ) -> Self
    where
        T: EventListener + Clone + Send + 'static,
    {
        let state = Arc::new(SessionState::new());
        state.publish_descriptor(client.descriptor().clone());
        let (commands, receiver) = mpsc::sync_channel(COMMAND_QUEUE_SIZE);
        let window_for_thread = Arc::new(Mutex::new(window_id));
        let window_for_thread_clone = Arc::clone(&window_for_thread);
        let closed = Arc::new(Mutex::new(false));
        let closed_for_thread = Arc::clone(&closed);
        let command_wakeup =
            new_command_wakeup().expect("failed to create session command wakeup");
        let command_wakeup_for_thread = Arc::clone(&command_wakeup);
        let commands = CommandSender::new(commands, Arc::clone(&command_wakeup));
        let listener = event_proxy.with_window_target(WindowTarget::dynamic(window_id));
        let state_for_thread = Arc::clone(&state);
        let client = Arc::new(client);
        thread::Builder::new()
            .name(format!("rio-session-{route_id}"))
            .spawn(move || {
                SessionPump {
                    client,
                    receiver,
                    state: state_for_thread,
                    event_proxy: listener,
                    route_id,
                    window_id: window_for_thread_clone,
                    closed: closed_for_thread,
                    command_wakeup: command_wakeup_for_thread,
                }
                .run(PumpStartup::Sequence(initial_sequence));
            })
            .expect("failed to start imported session pump");
        Self {
            commands,
            state,
            window_id: window_for_thread,
            closed,
        }
    }

    /// Prepare an attachment without committing it. The returned frame can be
    /// decoded while the source remains the owner; call `commit_prepared` only
    /// after the destination renderer is ready.
    pub fn prepare_attach(attachment: PreparedSessionAttachment) -> PreparedSession {
        PreparedSession {
            initial_frame: Some(attachment.initial_frame().clone()),
            had_active_owner: attachment.had_active_owner(),
            attachment,
        }
    }

    pub fn take_events(&self) -> Vec<SessionEvent> {
        self.state
            .events
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default()
    }

    fn take_frame_updates(&self) -> (Option<FullFrame>, Vec<FrameUpdate>) {
        self.state.take_frame_updates()
    }

    fn request_selection_text(
        &self,
        target: ClipboardType,
        copy_to_clipboard: bool,
    ) -> Result<(), SessionError> {
        let mut queue = self
            .state
            .selection_text
            .lock()
            .expect("selection reply lock poisoned");
        if queue.outstanding == EVENT_QUEUE_SIZE {
            let error = SessionError::Invalid("selection reply handoff is full".into());
            self.state
                .record_error(SessionError::Invalid(error.to_string()));
            return Err(error);
        }
        self.enqueue_command(PumpCommand::SelectionText {
            target,
            copy_to_clipboard,
        })?;
        queue.outstanding += 1;
        Ok(())
    }

    pub fn take_selection_text(&self) -> Option<SelectionTextResult> {
        let mut queue = self
            .state
            .selection_text
            .lock()
            .expect("selection reply lock poisoned");
        let reply = queue.replies.pop_front()?;
        queue.outstanding -= 1;
        Some(reply)
    }

    fn take_search_navigation(&self) -> Option<SearchNavigation> {
        self.state
            .search_navigation
            .lock()
            .ok()
            .and_then(|mut navigation| navigation.take())
    }

    fn take_search_matches(&self) -> Option<Vec<SearchMatch>> {
        self.state
            .search_matches
            .lock()
            .ok()
            .and_then(|mut matches| matches.take())
    }
}

struct SessionPump<T: EventListener + Clone + Send + 'static> {
    client: Arc<SessionClient>,
    receiver: mpsc::Receiver<PumpCommand>,
    state: Arc<SessionState>,
    event_proxy: T,
    route_id: usize,
    window_id: Arc<Mutex<WindowId>>,
    closed: Arc<Mutex<bool>>,
    command_wakeup: Arc<CommandWakeup>,
}

impl<T: EventListener + Clone + Send + 'static> SessionPump<T> {
    fn run(self, startup: PumpStartup) {
        let mut frame_sequence = match startup {
            PumpStartup::Frame(frame) => {
                let sequence = frame.sequence;
                self.state.publish_frame(*frame);
                sequence
            }
            PumpStartup::Sequence(sequence) => {
                // Transfer targets build their view from a prepared frame,
                // so a later pump failure must not look like a stillborn tab.
                self.state.mark_had_frame();
                sequence
            }
            PumpStartup::Snapshot => match self.client.snapshot() {
                Ok(frame) => {
                    let sequence = frame.sequence;
                    self.state.publish_frame(frame);
                    sequence
                }
                Err(error) => {
                    if self.handle_error(error) {
                        return;
                    }
                    0
                }
            },
        };
        self.notify();

        loop {
            #[cfg(unix)]
            self.command_wakeup.clear();
            loop {
                let command = {
                    let closed =
                        self.closed.lock().expect("session admission lock poisoned");
                    match self.receiver.try_recv() {
                        Ok(command) => command,
                        Err(_) if *closed => PumpCommand::Terminal(SessionCommand::Close),
                        Err(mpsc::TryRecvError::Empty) => break,
                        // Last GUI owner dropped the handle: detach, never Close.
                        Err(mpsc::TryRecvError::Disconnected) => return,
                    }
                };
                let (command, selection) = match command {
                    PumpCommand::Terminal(command) => (command, None),
                    PumpCommand::SelectionText {
                        target,
                        copy_to_clipboard,
                    } => (
                        SessionCommand::SelectionText,
                        Some((target, copy_to_clipboard)),
                    ),
                };
                let close = matches!(command, SessionCommand::Close);
                match self.client.command(command) {
                    Ok(reply) => {
                        self.handle_reply(reply, selection, &mut frame_sequence);
                        self.notify();
                    }
                    Err(error) => {
                        if selection.is_some() {
                            self.state
                                .selection_text
                                .lock()
                                .expect("selection reply lock poisoned")
                                .outstanding -= 1;
                        }
                        // Never retry Close, including an uncertain result.
                        if self.handle_error(error) || close {
                            return;
                        }
                        continue;
                    }
                }
                if close {
                    self.notify();
                    return;
                }
            }

            match self.client.poll_event() {
                Ok(Some(SessionEvent::FrameReady)) => {
                    if let Err(error) =
                        self.publish_incremental_frame(&mut frame_sequence)
                    {
                        if self.handle_error(error) {
                            return;
                        }
                    }
                    self.notify();
                }
                Ok(Some(event)) => {
                    self.state.publish_event(event);
                    self.notify();
                }
                Ok(None) => {
                    #[cfg(unix)]
                    if let Err(error) =
                        self.client.wait_for_activity(&self.command_wakeup)
                    {
                        self.fail(error);
                        return;
                    }
                    #[cfg(not(unix))]
                    unreachable!("session workers are unsupported on non-Unix");
                }
                Err(error) => {
                    self.fail(error);
                    return;
                }
            }
        }
    }

    fn publish_incremental_frame(
        &self,
        base_sequence: &mut u64,
    ) -> Result<(), SessionError> {
        let update = match self.client.snapshot_since(*base_sequence) {
            Ok(update) => update,
            Err(error) => {
                self.state.log_frame_resync_once(*base_sequence, &error);
                FrameUpdate::Full(self.client.snapshot()?)
            }
        };

        match update {
            FrameUpdate::Full(frame) => {
                *base_sequence = frame.sequence;
                self.state.publish_frame(frame);
            }
            FrameUpdate::Delta(delta) => {
                *base_sequence = delta.sequence;
                if !self.state.publish_frame_update(FrameUpdate::Delta(delta)) {
                    tracing::warn!(
                        route_id = self.route_id,
                        "session frame update queue full; requesting full snapshot"
                    );
                    let frame = self.client.snapshot()?;
                    *base_sequence = frame.sequence;
                    self.state.publish_frame(frame);
                }
            }
        }
        Ok(())
    }

    fn handle_reply(
        &self,
        reply: SessionReply,
        selection: Option<(ClipboardType, bool)>,
        base_sequence: &mut u64,
    ) {
        match reply {
            SessionReply::Frame(frame) => {
                *base_sequence = frame.sequence;
                self.state.publish_frame(frame);
            }
            SessionReply::SelectionText(text) => {
                let (target, copy_to_clipboard) =
                    selection.expect("selection request needs its destination");
                self.state
                    .selection_text
                    .lock()
                    .expect("selection reply lock poisoned")
                    .replies
                    .push_back((target, text, copy_to_clipboard));
            }
            SessionReply::SearchNavigation(navigation) => {
                if let Ok(mut slot) = self.state.search_navigation.lock() {
                    *slot = Some(navigation);
                }
            }
            SessionReply::SearchMatches(matches) => {
                if let Ok(mut slot) = self.state.search_matches.lock() {
                    *slot = Some(matches);
                }
            }
            SessionReply::Closed
            | SessionReply::Accepted
            | SessionReply::NoChange
            | SessionReply::ChildPid(_) => {}
            SessionReply::FrameUpdate(update) => {
                let next_sequence = match &update {
                    FrameUpdate::Full(frame) => frame.sequence,
                    FrameUpdate::Delta(delta) => delta.sequence,
                };
                if self.state.publish_frame_update(update) {
                    *base_sequence = next_sequence;
                } else {
                    match self.client.snapshot() {
                        Ok(frame) => {
                            *base_sequence = frame.sequence;
                            self.state.publish_frame(frame);
                        }
                        Err(error) => self.state.record_error(error),
                    }
                }
            }
        }
    }

    /// Snapshot budget refusals are recoverable just like command rejections:
    /// keep the attachment so input can clear the offending terminal state.
    fn handle_error(&self, error: SessionError) -> bool {
        let fatal = !recoverable_command_rejection(&error, self.client.is_poisoned());
        if fatal {
            self.fail(error);
        } else {
            self.state.record_error(error);
            self.notify();
        }
        fatal
    }

    /// End the pump on a fatal error: name a stillborn tab, record the
    /// failure, and wake the window for a final render.
    fn fail(&self, error: SessionError) {
        self.report_if_stillborn(&error);
        self.state.fail(error);
        self.notify();
    }

    /// A session that died before its first frame would leave a blank tab
    /// with dead input: report the reason instead of failing silently.
    fn report_if_stillborn(&self, error: &SessionError) {
        if self.state.had_frame() {
            return;
        }
        report_startup_failure(&self.event_proxy, self.window_id(), self.route_id, error);
    }

    fn window_id(&self) -> WindowId {
        load_window_id(&self.window_id)
    }

    fn notify(&self) {
        let window_id = self.window_id();
        self.event_proxy
            .send_event(RioEvent::RenderRoute(self.route_id), window_id);
    }
}

impl<T: EventListener + Clone + Send + 'static> Drop for SessionPump<T> {
    fn drop(&mut self) {
        self.state.pump_done.store(true, Ordering::Release);
        // Reap an already-exited worker without blocking. Every pump exit
        // funnels through here, so closed tabs stop leaking zombies until
        // the window exits; a still-running worker (for example a detached
        // transfer target) is left alone.
        #[cfg(unix)]
        let _ = self.client.reap_worker();
    }
}

/// Read the current window id, falling back to window zero when the slot
/// lock is poisoned.
fn load_window_id(slot: &Mutex<WindowId>) -> WindowId {
    slot.lock()
        .map(|window_id| *window_id)
        .unwrap_or_else(|_| WindowId::from(0))
}

/// Report an async session-startup failure the same way synchronous
/// context creation does: a tab that cannot open must say why.
fn report_startup_failure<T: EventListener>(
    event_proxy: &T,
    window_id: WindowId,
    route_id: usize,
    error: &SessionError,
) {
    tracing::error!(route_id, "could not open a new tab: {error}");
    event_proxy.send_event(
        RioEvent::ReportToAssistant(RioError {
            report: RioErrorType::InitializationError(format!(
                "could not open a new tab: {error}"
            )),
            level: RioErrorLevel::Error,
        }),
        window_id,
    );
}

fn recoverable_command_rejection(error: &SessionError, poisoned: bool) -> bool {
    !poisoned
        && matches!(
            error,
            SessionError::Invalid(_) | SessionError::Unsupported(_)
        )
}

pub struct PreparedSession {
    attachment: PreparedSessionAttachment,
    initial_frame: Option<FullFrame>,
    had_active_owner: bool,
}

impl PreparedSession {
    pub fn initial_frame(&self) -> &FullFrame {
        self.initial_frame
            .as_ref()
            .expect("prepared session frame was already taken")
    }

    pub(crate) fn take_initial_frame(&mut self) -> FullFrame {
        self.initial_frame
            .take()
            .expect("prepared session frame was already taken")
    }

    pub fn had_active_owner(&self) -> bool {
        self.had_active_owner
    }

    pub fn commit(self) -> Result<SessionClient, SessionError> {
        self.attachment.commit()
    }
}

#[derive(Clone, Debug, Default)]
pub struct PassiveCursor {
    pub pos: Pos,
}

/// A renderer-facing row cache. Only viewport rows are available in a
/// `FullFrame`; indexing history lines deliberately clamps to the nearest
/// cached row rather than pretending that the GUI has a history replica.
#[derive(Clone, Debug)]
pub struct PassiveGrid {
    pub rows: Vec<Row<Square>>,
    pub row_styles: Vec<Vec<Style>>,
    pub cursor: PassiveCursor,
    pub columns: usize,
    pub lines: usize,
    pub history: usize,
    pub display_offset: usize,
    extras: FxHashMap<u16, Extras>,
    extras_by_value: FxHashMap<Extras, u16>,
    styles_by_value: FxHashMap<Style, u16>,
    next_extra_id: usize,
}

pub(crate) struct RenderBuffers {
    pub(crate) rows: Vec<Row<Square>>,
    pub(crate) row_styles: Vec<Vec<Style>>,
    pub(crate) extras: FxHashMap<u16, Extras>,
}

struct FrameDecodeState {
    columns: usize,
    extras: FxHashMap<u16, Extras>,
    extras_by_value: FxHashMap<Extras, u16>,
    styles_by_value: FxHashMap<Style, u16>,
    next_extra_id: usize,
}

impl FrameDecodeState {
    fn new(columns: usize) -> Self {
        let mut styles_by_value = FxHashMap::default();
        styles_by_value.insert(Style::default(), 0);
        Self {
            columns,
            extras: FxHashMap::default(),
            extras_by_value: FxHashMap::default(),
            styles_by_value,
            next_extra_id: 1,
        }
    }

    fn from_grid(grid: &PassiveGrid) -> Self {
        Self {
            columns: grid.columns,
            extras: grid.extras.clone(),
            extras_by_value: grid.extras_by_value.clone(),
            styles_by_value: grid.styles_by_value.clone(),
            next_extra_id: grid.next_extra_id,
        }
    }

    fn install_into(self, grid: &mut PassiveGrid) {
        grid.extras = self.extras;
        grid.extras_by_value = self.extras_by_value;
        grid.styles_by_value = self.styles_by_value;
        grid.next_extra_id = self.next_extra_id;
    }
}

impl PassiveGrid {
    fn new(columns: usize, lines: usize) -> Self {
        let lines = lines.max(1);
        let mut styles_by_value = FxHashMap::default();
        styles_by_value.insert(Style::default(), 0);
        Self {
            rows: (0..lines).map(|_| Row::new(columns.max(1))).collect(),
            row_styles: (0..lines)
                .map(|_| vec![Style::default(); columns.max(1)])
                .collect(),
            cursor: PassiveCursor::default(),
            columns: columns.max(1),
            lines,
            history: 0,
            display_offset: 0,
            extras: FxHashMap::default(),
            extras_by_value: FxHashMap::default(),
            styles_by_value,
            next_extra_id: 1,
        }
    }

    fn row_index(&self, line: Line) -> usize {
        let index = line.0.saturating_add(self.display_offset as i32);
        usize::try_from(index)
            .ok()
            .filter(|index| *index < self.rows.len())
            .unwrap_or_else(|| {
                if index.is_negative() {
                    0
                } else {
                    self.rows.len() - 1
                }
            })
    }

    pub fn cell_text(&self, pos: Pos) -> std::vec::IntoIter<char> {
        let row = self.row_index(pos.row);
        let Some(square) = self.rows.get(row).and_then(|row| row.inner.get(pos.col.0))
        else {
            return Vec::new().into_iter();
        };
        let mut text = vec![square.c()];
        if let Some(id) = square.extras_id_checked() {
            if let Some(extras) = self.extras.get(&id) {
                text.extend(extras.zerowidth.iter().copied());
            }
        }
        text.into_iter()
    }

    fn hyperlink(&self, pos: Pos) -> Option<Hyperlink> {
        let row = self.row_index(pos.row);
        let square = self.rows.get(row)?.inner.get(pos.col.0)?;
        square
            .extras_id_checked()
            .and_then(|id| self.extras.get(&id))
            .and_then(|extras| extras.hyperlink.clone())
    }

    pub(crate) fn take_render_buffers(&mut self) -> RenderBuffers {
        RenderBuffers {
            rows: std::mem::take(&mut self.rows),
            row_styles: std::mem::take(&mut self.row_styles),
            extras: std::mem::take(&mut self.extras),
        }
    }

    pub(crate) fn restore_render_buffers(&mut self, buffers: RenderBuffers) {
        self.rows = buffers.rows;
        self.row_styles = buffers.row_styles;
        self.extras = buffers.extras;
    }
}

impl Dimensions for PassiveGrid {
    fn total_lines(&self) -> usize {
        self.history + self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }

    fn history_size(&self) -> usize {
        self.history
    }
}

impl Index<Line> for PassiveGrid {
    type Output = Row<Square>;

    fn index(&self, line: Line) -> &Self::Output {
        &self.rows[self.row_index(line)]
    }
}

impl IndexMut<Line> for PassiveGrid {
    fn index_mut(&mut self, line: Line) -> &mut Self::Output {
        let index = self.row_index(line);
        &mut self.rows[index]
    }
}

/// Passive terminal view used by existing renderer and interaction code.
/// Every mutating method below sends a command to the worker; the fields are
/// only the most recently published frame and are never authoritative.
pub struct RemoteView {
    pub grid: PassiveGrid,
    pub cursor_shape: CursorShape,
    pub default_cursor_shape: CursorShape,
    pub blinking_cursor: bool,
    pub selection_range: Option<SelectionRange>,
    pub vi_mode_cursor: PassiveCursor,
    pub window_id: WindowId,
    pub title: String,
    pub current_directory: Option<std::path::PathBuf>,
    pub colors: rio_backend::config::colors::term::TermColors,
    mode_bits: Mode,
    session: Option<SessionHandle>,
    frame: Option<FullFrame>,
    graphics_dirty: bool,
    pending_frame_damage: rio_backend::event::TerminalDamage,
    delta_resync_logged: bool,
    search_active: bool,
    search_navigation: Option<SearchNavigation>,
    search_matches: Option<Vec<SearchMatch>>,
    decode_error: Option<String>,
}

impl std::fmt::Debug for RemoteView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteView")
            .field(
                "sequence",
                &self.frame.as_ref().map_or(0, |frame| frame.sequence),
            )
            .field("columns", &self.grid.columns)
            .field("lines", &self.grid.lines)
            .finish()
    }
}

impl RemoteView {
    pub fn from_frame(frame: FullFrame, window_id: WindowId) -> Self {
        let mut view = Self::new(
            None,
            window_id,
            frame.columns as usize,
            frame.lines as usize,
        );
        if let Err(error) = view.apply_frame(frame) {
            view.decode_error = Some(error);
        }
        view
    }

    pub fn install_session(&mut self, session: SessionHandle) {
        self.session = Some(session);
    }

    pub fn detach_session(&mut self) {
        self.session = None;
    }

    pub(crate) fn frame_sequence(&self) -> u64 {
        self.frame
            .as_ref()
            .expect("remote view has no cached frame")
            .sequence
    }

    pub(crate) fn take_render_graphics(
        &mut self,
    ) -> rio_session::protocol::GraphicsFrame {
        self.frame
            .as_mut()
            .map(|frame| std::mem::take(&mut frame.graphics))
            .unwrap_or_default()
    }

    pub(crate) fn restore_render_graphics(
        &mut self,
        graphics: rio_session::protocol::GraphicsFrame,
    ) {
        if let Some(frame) = &mut self.frame {
            frame.graphics = graphics;
        }
    }

    pub(crate) fn graphics_dirty(&self) -> bool {
        self.graphics_dirty
    }

    pub(crate) fn mark_graphics_clean(&mut self) {
        self.graphics_dirty = false;
    }

    pub fn decoder_error(&self) -> Option<&str> {
        self.decode_error.as_deref()
    }

    pub fn new(
        session: Option<SessionHandle>,
        window_id: WindowId,
        columns: usize,
        lines: usize,
    ) -> Self {
        Self {
            grid: PassiveGrid::new(columns, lines),
            cursor_shape: CursorShape::Block,
            default_cursor_shape: CursorShape::Block,
            blinking_cursor: false,
            selection_range: None,
            vi_mode_cursor: PassiveCursor::default(),
            window_id,
            title: String::new(),
            current_directory: None,
            colors: Default::default(),
            mode_bits: Mode::empty(),
            session,
            frame: None,
            graphics_dirty: false,
            pending_frame_damage: rio_backend::event::TerminalDamage::Noop,
            delta_resync_logged: false,
            search_active: false,
            search_navigation: None,
            search_matches: None,
            decode_error: None,
        }
    }

    pub fn refresh(&mut self) {
        let Some((navigation, matches, (frame, updates))) =
            self.session.as_ref().map(|session| {
                (
                    session.take_search_navigation(),
                    session.take_search_matches(),
                    session.take_frame_updates(),
                )
            })
        else {
            return;
        };
        if let Some(navigation) = navigation {
            self.search_navigation = Some(navigation);
        }
        if let Some(matches) = matches {
            self.search_matches = Some(matches);
        }
        if let Some(frame) = frame {
            let current_sequence =
                self.frame.as_ref().map_or(0, |current| current.sequence);
            if frame.sequence != current_sequence {
                if let Err(error) = self.apply_frame(frame) {
                    self.decode_error = Some(error.clone());
                    if let Some(session) = self.session.as_ref() {
                        session.record_command_error(SessionError::Invalid(error));
                    }
                } else {
                    self.decode_error = None;
                    self.delta_resync_logged = false;
                }
            }
        }
        for update in updates {
            match self.apply_frame_update(update) {
                Ok(()) => {
                    self.decode_error = None;
                    self.delta_resync_logged = false;
                }
                Err(error) => {
                    self.decode_error = Some(error.clone());
                    if !self.delta_resync_logged {
                        tracing::warn!(
                            error = %error,
                            "cached session frame delta did not match; requesting full snapshot"
                        );
                        self.delta_resync_logged =
                            self.session.as_ref().is_some_and(|session| {
                                session.enqueue(SessionCommand::Snapshot).is_ok()
                            });
                    }
                    break;
                }
            }
        }
    }

    pub fn refresh_renderable(&mut self, content: &mut RenderableContent) {
        self.refresh();
        content.term_colors = self.colors;
        content.display_offset = self.grid.display_offset;
        content.columns = self.grid.columns;
        content.screen_lines = self.grid.lines;
        content.history_size = self.grid.history;
        content.blinking_cursor = self.blinking_cursor;
        content.cursor.state = self.cursor();
        content.selection_range = self.selection_range;
        content.session_error = self
            .decode_error
            .clone()
            .or_else(|| self.session.as_ref().and_then(SessionHandle::error));
        let frame_damage = std::mem::replace(
            &mut self.pending_frame_damage,
            rio_backend::event::TerminalDamage::Noop,
        );
        content.frame_damage =
            PendingUpdate::merge_terminal_damages(content.frame_damage, frame_damage);
    }

    fn apply_frame(&mut self, frame: FullFrame) -> Result<(), String> {
        if frame.columns == 0 || frame.lines == 0 {
            return Err("passive frame dimensions must be non-zero".into());
        }
        if frame.rows.len() != usize::from(frame.lines) {
            return Err("passive frame row count does not match its dimensions".into());
        }
        let graphics_changed = self
            .frame
            .as_ref()
            .is_none_or(|current| current.graphics != frame.graphics);
        let columns = frame.columns as usize;
        let lines = frame.lines as usize;
        let mut grid = PassiveGrid::new(columns, lines);
        let mut decode_state = FrameDecodeState::new(grid.columns);
        for (row_index, source) in frame.rows.iter().enumerate() {
            let (row, styles) = Self::decode_row(&mut decode_state, source)?;
            grid.rows[row_index] = row;
            grid.row_styles[row_index] = styles;
        }
        decode_state.install_into(&mut grid);
        self.grid = grid;
        self.update_frame_metadata(&frame);
        self.frame = Some(frame);
        self.graphics_dirty |= graphics_changed;
        self.pending_frame_damage = PendingUpdate::merge_terminal_damages(
            self.pending_frame_damage,
            rio_backend::event::TerminalDamage::Full,
        );
        Ok(())
    }

    fn decode_row(
        state: &mut FrameDecodeState,
        source: &RowFrame,
    ) -> Result<(Row<Square>, Vec<Style>), String> {
        let columns = state.columns;
        if source.cells.len() != columns
            || source.styles.len() != columns
            || source.extras.len() != columns
        {
            return Err("passive frame row dimensions do not match".into());
        }
        let mut row = Row::new(columns);
        let mut row_styles = Vec::with_capacity(columns);
        for (column, cell) in source.cells.iter().enumerate() {
            let mut square = decode_cell(cell);
            let is_codepoint = matches!(&cell.content, CellContentFrame::Codepoint(_));
            if is_codepoint {
                if let Some(extra) = source.extras.get(column).and_then(Option::as_ref) {
                    let extras = decode_extras(extra);
                    let id = if let Some(id) = state.extras_by_value.get(&extras) {
                        *id
                    } else {
                        if state.next_extra_id > usize::from(u16::MAX) {
                            return Err(
                                "passive frame extras table exceeds u16 capacity".into(),
                            );
                        }
                        let id = u16::try_from(state.next_extra_id).map_err(|_| {
                            "passive frame extras ID overflow".to_string()
                        })?;
                        state.next_extra_id += 1;
                        state.extras_by_value.insert(extras.clone(), id);
                        state.extras.insert(id, extras);
                        id
                    };
                    square.set_extras_id(Some(id));
                    row.has_extras = true;
                }
            }
            let style = decode_style(&source.styles[column]);
            row_styles.push(style);
            let style_id = if let Some(id) = state.styles_by_value.get(&style) {
                *id
            } else {
                let next_id = state.styles_by_value.len();
                if next_id > usize::from(u16::MAX) {
                    return Err("passive frame style ID overflow".into());
                }
                let id = u16::try_from(next_id)
                    .map_err(|_| "passive frame style ID overflow".to_string())?;
                state.styles_by_value.insert(style, id);
                id
            };
            if is_codepoint {
                square.set_style_id(style_id);
            }
            if style != Style::default() {
                row.has_styles = true;
            }
            row.inner[column] = square;
        }
        row.kitty_virtual_placeholder = source.kitty_virtual_placeholder;
        row.dirty = true;
        Ok((row, row_styles))
    }

    fn update_frame_metadata(&mut self, frame: &FullFrame) {
        self.grid.cursor.pos = Pos::new(
            Line(i32::from(frame.cursor.line)),
            Column(frame.cursor.column as usize),
        );
        self.vi_mode_cursor.pos = Pos::new(
            Line(i32::from(frame.cursor.line) - frame.display_offset as i32),
            Column(frame.cursor.column as usize),
        );
        self.blinking_cursor = frame.cursor.blinking;
        self.cursor_shape = cursor_shape(frame.cursor.shape);
        self.default_cursor_shape = self.cursor_shape;
        self.mode_bits = Mode::from_bits_truncate(frame.modes);
        self.title = frame.title.clone();
        self.current_directory =
            frame.working_dir.as_deref().map(std::path::PathBuf::from);
        self.colors = decode_colors(&frame.colors);
        self.selection_range =
            selection_range(frame.selection.as_ref(), frame.display_offset);
        self.grid.history = frame.history_size as usize;
        self.grid.display_offset = frame.display_offset as usize;
    }

    fn apply_frame_update(&mut self, update: FrameUpdate) -> Result<(), String> {
        match update {
            FrameUpdate::Full(frame) => self.apply_frame(frame),
            FrameUpdate::Delta(delta) => {
                let Some(current) = self.frame.take() else {
                    return Err("received a frame delta without a cached frame".into());
                };
                let cached_sequence = current.sequence;
                if current.sequence != delta.base_sequence {
                    self.frame = Some(current);
                    return Err(format!(
                        "frame delta base {} does not match cached {}",
                        delta.base_sequence, cached_sequence
                    ));
                }
                if current.columns != delta.columns || current.lines != delta.lines {
                    self.frame = Some(current);
                    return Err("frame delta dimensions require a full snapshot".into());
                }
                let old_selection = self.selection_range;
                let old_display_offset = self.grid.display_offset;
                let old_history = self.grid.history;
                let old_colors = self.colors;
                let old_alternate_screen = current.alternate_screen;
                let changed_row_count = delta.rows.len();
                let mut next = current;
                let mut decode_state = FrameDecodeState::from_grid(&self.grid);
                let mut decoded_rows = Vec::with_capacity(delta.rows.len());
                for changed in &delta.rows {
                    let row_index = usize::from(changed.line);
                    if row_index >= self.grid.rows.len() {
                        self.frame = Some(next);
                        return Err("frame delta row is outside the cached grid".into());
                    }
                    let (row, styles) =
                        match Self::decode_row(&mut decode_state, &changed.row) {
                            Ok(decoded) => decoded,
                            Err(error) => {
                                self.frame = Some(next);
                                return Err(error);
                            }
                        };
                    decoded_rows.push((row_index, row, styles));
                }
                if let Err(error) = FrameUpdate::Delta(delta).apply_to(&mut next) {
                    self.frame = Some(next);
                    return Err(error.to_string());
                }
                let alternate_changed = old_alternate_screen != next.alternate_screen;
                decode_state.install_into(&mut self.grid);
                for (row_index, row, styles) in decoded_rows {
                    self.grid.rows[row_index] = row;
                    self.grid.row_styles[row_index] = styles;
                }
                self.update_frame_metadata(&next);
                self.frame = Some(next);
                let selection_changed = old_selection != self.selection_range;
                let viewport_changed = old_display_offset != self.grid.display_offset
                    || old_history != self.grid.history;
                let colors_changed = old_colors != self.colors;
                let needs_row_rebuild = selection_changed
                    || viewport_changed
                    || colors_changed
                    || alternate_changed;
                if needs_row_rebuild {
                    for row in &mut self.grid.rows {
                        row.dirty = true;
                    }
                }
                let damage = if needs_row_rebuild || changed_row_count != 0 {
                    rio_backend::event::TerminalDamage::Partial
                } else {
                    rio_backend::event::TerminalDamage::CursorOnly
                };
                self.pending_frame_damage = PendingUpdate::merge_terminal_damages(
                    self.pending_frame_damage,
                    damage,
                );
                Ok(())
            }
        }
    }

    fn enqueue(&self, command: SessionCommand) {
        if let Some(session) = &self.session {
            if let Err(error) = session.enqueue(command) {
                // The owning context exposes this error through the passive
                // render cache; never fall back to a local terminal.
                session.record_command_error(error);
            }
        }
    }

    pub fn session(&self) -> Option<&SessionHandle> {
        self.session.as_ref()
    }

    pub fn mode(&self) -> Mode {
        self.mode_bits
    }

    pub fn display_offset(&self) -> usize {
        self.grid.display_offset
    }

    pub fn history_size(&self) -> usize {
        self.grid.history
    }

    pub fn columns(&self) -> usize {
        self.grid.columns
    }

    pub fn screen_lines(&self) -> usize {
        self.grid.lines
    }

    pub fn total_lines(&self) -> usize {
        self.grid.total_lines()
    }

    pub fn bottommost_line(&self) -> Line {
        self.grid.bottommost_line()
    }

    pub fn topmost_line(&self) -> Line {
        self.grid.topmost_line()
    }

    pub fn last_column(&self) -> Column {
        self.grid.last_column()
    }

    pub fn cursor(&self) -> CursorState {
        CursorState {
            pos: self.grid.cursor.pos,
            content: self.cursor_shape,
        }
    }

    pub fn vi_cursor_position(&self) -> Pos {
        self.vi_mode_cursor.pos
    }

    pub fn resize_to(&mut self, size: rio_backend::event::WindowSize) {
        self.enqueue(SessionCommand::Resize {
            columns: size.cols,
            lines: size.rows,
            pixel_width: size.width,
            pixel_height: size.height,
        });
    }

    pub fn set_cursor_style(&mut self, shape: CursorShape, blinking: bool) {
        let shape = match shape {
            CursorShape::Block => 0,
            CursorShape::Underline => 1,
            CursorShape::Beam => 2,
            CursorShape::Hidden => 3,
        };
        self.enqueue(SessionCommand::SetCursorStyle { shape, blinking });
    }

    pub fn scroll_display(&mut self, scroll: Scroll) {
        match scroll {
            Scroll::Delta(delta_lines) => {
                self.enqueue(SessionCommand::Scroll { delta_lines })
            }
            Scroll::PageUp => self.enqueue(SessionCommand::Scroll {
                delta_lines: self.screen_lines() as i32,
            }),
            Scroll::PageDown => self.enqueue(SessionCommand::Scroll {
                delta_lines: -(self.screen_lines() as i32),
            }),
            Scroll::Top => self.enqueue(SessionCommand::ScrollTop),
            Scroll::Bottom => self.enqueue(SessionCommand::ScrollBottom),
        }
    }

    pub fn paste(&mut self, text: String, bracketed: bool) {
        if bracketed {
            self.enqueue(SessionCommand::Paste(text));
        } else {
            self.enqueue(SessionCommand::Write(text.into_bytes()));
        }
    }

    pub fn focus(&mut self, focused: bool) {
        self.enqueue(SessionCommand::Focus { focused });
    }

    pub fn mouse_wheel(&mut self, lines: i32, point: Pos, modifiers: u8) {
        let (Ok(column), Ok(line)) =
            (u16::try_from(point.col.0), u16::try_from(point.row.0))
        else {
            return;
        };
        self.enqueue(SessionCommand::MouseWheel {
            lines,
            column,
            line,
            modifiers,
        });
    }

    pub fn mouse_button(&mut self, point: Pos, button: u8, pressed: bool, modifiers: u8) {
        let (Ok(column), Ok(line)) =
            (u16::try_from(point.col.0), u16::try_from(point.row.0))
        else {
            return;
        };
        self.enqueue(SessionCommand::MouseButton {
            column,
            line,
            button,
            pressed,
            modifiers,
        });
    }

    pub fn mouse_motion(&mut self, point: Pos, button: u8, modifiers: u8) {
        let (Ok(column), Ok(line)) =
            (u16::try_from(point.col.0), u16::try_from(point.row.0))
        else {
            return;
        };
        self.enqueue(SessionCommand::MouseMotion {
            column,
            line,
            button,
            modifiers,
        });
    }

    pub fn selection_begin(
        &mut self,
        ty: rio_backend::selection::SelectionType,
        point: Pos,
        side: rio_backend::crosswords::pos::Side,
    ) {
        let kind = match ty {
            rio_backend::selection::SelectionType::Simple => WireSelectionKind::Simple,
            rio_backend::selection::SelectionType::Block => WireSelectionKind::Block,
            rio_backend::selection::SelectionType::Semantic => WireSelectionKind::Word,
            rio_backend::selection::SelectionType::Lines => WireSelectionKind::Line,
        };
        let side = match side {
            rio_backend::crosswords::pos::Side::Left => WireSelectionSide::Left,
            rio_backend::crosswords::pos::Side::Right => WireSelectionSide::Right,
        };
        self.enqueue(SessionCommand::SelectionBegin {
            line: point.row.0,
            column: point.col.0,
            kind,
            side,
        });
    }

    pub fn selection_update(
        &mut self,
        point: Pos,
        side: rio_backend::crosswords::pos::Side,
    ) {
        let side = match side {
            rio_backend::crosswords::pos::Side::Left => WireSelectionSide::Left,
            rio_backend::crosswords::pos::Side::Right => WireSelectionSide::Right,
        };
        self.enqueue(SessionCommand::SelectionUpdate {
            line: point.row.0,
            column: point.col.0,
            side,
        });
    }

    pub fn selection_autoscroll(
        &mut self,
        delta_lines: i32,
        point: Pos,
        side: rio_backend::crosswords::pos::Side,
    ) {
        let side = match side {
            rio_backend::crosswords::pos::Side::Left => WireSelectionSide::Left,
            rio_backend::crosswords::pos::Side::Right => WireSelectionSide::Right,
        };
        self.enqueue(SessionCommand::SelectionAutoScroll {
            delta_lines,
            line: point.row.0,
            column: point.col.0,
            side,
        });
    }

    pub fn select_all(&mut self) {
        self.enqueue(SessionCommand::SelectAll);
    }

    pub fn clear_selection(&mut self) {
        self.enqueue(SessionCommand::SelectionClear);
    }

    pub fn vi_scroll(&mut self, delta_lines: i32) {
        self.enqueue(SessionCommand::ViScroll { delta_lines });
    }

    pub fn vi_motion(&mut self, motion: rio_backend::crosswords::vi_mode::ViMotion) {
        if let Some(motion) = wire_vi_motion(motion) {
            self.enqueue(SessionCommand::ViMotion(motion));
        }
    }

    pub fn vi_goto_pos(&mut self, pos: Pos) {
        self.enqueue(SessionCommand::ViGoto {
            line: pos.row.0,
            column: pos.col.0 as u16,
        });
    }

    pub fn scroll_to_pos(&mut self, pos: Pos) {
        self.enqueue(SessionCommand::ViGoto {
            line: pos.row.0,
            column: pos.col.0 as u16,
        });
    }

    pub fn scroll_to_prompt(&mut self, forward: bool) {
        self.enqueue(SessionCommand::ScrollToPrompt { forward });
    }

    pub fn clear_saved_history(&mut self) {
        self.enqueue(SessionCommand::ClearSavedHistory);
    }

    pub fn set_vi_mode(&mut self, enabled: bool) {
        self.enqueue(SessionCommand::SetViMode(enabled));
    }

    pub fn request_selection_text(
        &mut self,
        target: rio_backend::clipboard::ClipboardType,
    ) {
        self.request_selection_text_with_copy(target, false);
    }

    pub fn request_selection_text_with_copy(
        &mut self,
        target: rio_backend::clipboard::ClipboardType,
        copy_to_clipboard: bool,
    ) {
        if let Some(session) = self.session.as_ref() {
            if let Err(error) = session.request_selection_text(target, copy_to_clipboard)
            {
                session.record_command_error(error);
            }
        }
    }

    pub fn take_selection_text(
        &mut self,
    ) -> Option<(rio_backend::clipboard::ClipboardType, Option<String>, bool)> {
        self.session.as_ref()?.take_selection_text()
    }

    pub fn passive_selection_range(&self) -> Option<SelectionRange> {
        self.selection_range
    }

    pub fn begin_search(
        &mut self,
        pattern: String,
        origin: Pos,
        direction: rio_backend::crosswords::pos::Direction,
        side: rio_backend::crosswords::pos::Side,
        max_lines: Option<usize>,
    ) {
        if self.search_active {
            self.enqueue(SessionCommand::SearchNext);
            return;
        }
        let Ok(origin_display_offset) = u32::try_from(self.display_offset()) else {
            return;
        };
        let origin_line = origin.row.0;
        let Ok(origin_column) = origin.col.0.try_into() else {
            return;
        };
        self.search_active = true;
        self.search_navigation = None;
        self.enqueue(SessionCommand::SearchBegin {
            pattern,
            origin_line,
            origin_column,
            origin_display_offset,
            direction: match direction {
                rio_backend::crosswords::pos::Direction::Right => {
                    WireSearchDirection::Forward
                }
                rio_backend::crosswords::pos::Direction::Left => {
                    WireSearchDirection::Backward
                }
            },
            side: match side {
                rio_backend::crosswords::pos::Side::Left => WireSelectionSide::Left,
                rio_backend::crosswords::pos::Side::Right => WireSelectionSide::Right,
            },
            max_lines: max_lines.and_then(|lines| u32::try_from(lines).ok()),
        });
    }

    pub fn next_search(&mut self) {
        if self.search_active {
            self.enqueue(SessionCommand::SearchNext);
        }
    }

    pub fn search_matches(&mut self, pattern: &str, max_matches: usize) {
        self.search_matches = None;
        self.enqueue(SessionCommand::Search {
            pattern: pattern.to_owned(),
            max_matches,
        });
    }

    pub fn take_search_matches(&mut self) -> Option<Vec<std::ops::RangeInclusive<Pos>>> {
        self.refresh();
        self.search_matches.take().map(|matches| {
            matches
                .into_iter()
                .map(|matched| {
                    let history = self.grid.history as i32;
                    let start_line = matched.start_line as i32 - history;
                    let end_line = matched.end_line as i32 - history;
                    Pos::new(Line(start_line), Column(matched.start_column as usize))
                        ..=Pos::new(Line(end_line), Column(matched.end_column as usize))
                })
                .collect()
        })
    }

    pub fn is_search_active(&self) -> bool {
        self.search_active
    }

    pub fn cancel_search(&mut self) {
        if self.search_active {
            self.search_active = false;
            self.search_navigation = None;
            self.enqueue(SessionCommand::SearchCancel);
        }
    }

    pub fn take_search_navigation(&mut self) -> Option<SearchNavigation> {
        self.refresh();
        self.search_navigation.take()
    }

    pub fn cell_hyperlink(&self, point: Pos) -> Option<Hyperlink> {
        self.grid.hyperlink(point)
    }
}

impl Dimensions for RemoteView {
    fn total_lines(&self) -> usize {
        self.grid.total_lines()
    }

    fn screen_lines(&self) -> usize {
        self.grid.lines
    }

    fn columns(&self) -> usize {
        self.grid.columns
    }

    fn history_size(&self) -> usize {
        self.grid.history
    }
}

fn cursor_shape(value: u8) -> CursorShape {
    match value {
        1 => CursorShape::Underline,
        2 => CursorShape::Beam,
        3 => CursorShape::Hidden,
        _ => CursorShape::Block,
    }
}

fn selection_range(
    selection: Option<&SelectionFrame>,
    display_offset: u32,
) -> Option<SelectionRange> {
    // Session snapshots clip selection to the viewport. The grid renderer
    // compares selection rows with terminal coordinates (visible row - offset).
    selection.map(|selection| SelectionRange {
        start: Pos::new(
            Line(i32::from(selection.start_line) - display_offset as i32),
            Column(selection.start_column as usize),
        ),
        end: Pos::new(
            Line(i32::from(selection.end_line) - display_offset as i32),
            Column(selection.end_column as usize),
        ),
        is_block: selection.block,
    })
}

fn decode_cell(cell: &CellFrame) -> Square {
    let mut square = match cell.content {
        CellContentFrame::Codepoint(codepoint) => {
            Square::from_char(char::from_u32(codepoint).unwrap_or(' '))
        }
        CellContentFrame::Palette(index) => {
            let mut square = Square::default();
            square.set_bg_palette(index);
            square
        }
        CellContentFrame::Rgb { r, g, b } => {
            let mut square = Square::default();
            square.set_bg_rgb(r, g, b);
            square
        }
    };
    square.set_wide(match cell.wide {
        1 => Wide::Wide,
        2 => Wide::Spacer,
        3 => Wide::LeadingSpacer,
        _ => Wide::Narrow,
    });
    square.set_cell_flags(CellFlags::from_bits_truncate(cell.flags));
    square
}

fn decode_extras(extra: &rio_session::protocol::ExtrasFrame) -> Extras {
    Extras {
        zerowidth: extra
            .zero_width
            .iter()
            .filter_map(|codepoint| char::from_u32(*codepoint))
            .collect(),
        hyperlink: extra
            .hyperlink
            .as_ref()
            .map(|link| Hyperlink::new(Some(link.id.as_str()), link.uri.as_str())),
    }
}

fn decode_style(style: &StyleFrame) -> Style {
    Style {
        fg: decode_color(&style.foreground),
        bg: decode_color(&style.background),
        underline_color: style.underline.as_ref().map(decode_color),
        flags: StyleFlags::from_bits_truncate(style.flags),
    }
}

fn decode_color(color: &ColorFrame) -> AnsiColor {
    match color {
        ColorFrame::Indexed(index) => AnsiColor::Indexed(*index),
        ColorFrame::Rgb { r, g, b } => AnsiColor::Spec(ColorRgb {
            r: *r,
            g: *g,
            b: *b,
        }),
        ColorFrame::Named(name) => AnsiColor::Named(named_color(*name)),
    }
}

fn named_color(value: u16) -> NamedColor {
    match value {
        0 => NamedColor::Black,
        1 => NamedColor::Red,
        2 => NamedColor::Green,
        3 => NamedColor::Yellow,
        4 => NamedColor::Blue,
        5 => NamedColor::Magenta,
        6 => NamedColor::Cyan,
        7 => NamedColor::White,
        8 => NamedColor::LightBlack,
        9 => NamedColor::LightRed,
        10 => NamedColor::LightGreen,
        11 => NamedColor::LightYellow,
        12 => NamedColor::LightBlue,
        13 => NamedColor::LightMagenta,
        14 => NamedColor::LightCyan,
        15 => NamedColor::LightWhite,
        256 => NamedColor::Foreground,
        257 => NamedColor::Background,
        258 => NamedColor::Cursor,
        259 => NamedColor::DimBlack,
        260 => NamedColor::DimRed,
        261 => NamedColor::DimGreen,
        262 => NamedColor::DimYellow,
        263 => NamedColor::DimBlue,
        264 => NamedColor::DimMagenta,
        265 => NamedColor::DimCyan,
        266 => NamedColor::DimWhite,
        267 => NamedColor::LightForeground,
        268 => NamedColor::DimForeground,
        _ => NamedColor::Foreground,
    }
}

fn decode_colors(
    colors: &[Option<[f32; 4]>],
) -> rio_backend::config::colors::term::TermColors {
    let mut result = rio_backend::config::colors::term::TermColors::default();
    for (index, color) in colors.iter().enumerate().take(269) {
        result[index] = *color;
    }
    result
}

fn wire_vi_motion(
    motion: rio_backend::crosswords::vi_mode::ViMotion,
) -> Option<WireViMotion> {
    use rio_backend::crosswords::vi_mode::ViMotion as Local;
    Some(match motion {
        Local::Up => WireViMotion::Up,
        Local::Down => WireViMotion::Down,
        Local::Left => WireViMotion::Left,
        Local::Right => WireViMotion::Right,
        Local::First => WireViMotion::First,
        Local::Last => WireViMotion::Last,
        Local::FirstOccupied => WireViMotion::FirstOccupied,
        Local::High => WireViMotion::High,
        Local::Middle => WireViMotion::Middle,
        Local::Low => WireViMotion::Low,
        Local::SemanticLeft => WireViMotion::SemanticLeft,
        Local::SemanticRight => WireViMotion::SemanticRight,
        Local::SemanticLeftEnd => WireViMotion::SemanticLeftEnd,
        Local::SemanticRightEnd => WireViMotion::SemanticRightEnd,
        Local::WordLeft => WireViMotion::WordLeft,
        Local::WordRight => WireViMotion::WordRight,
        Local::WordLeftEnd => WireViMotion::WordLeftEnd,
        Local::WordRightEnd => WireViMotion::WordRightEnd,
        Local::Bracket => WireViMotion::Bracket,
        Local::ParagraphUp => WireViMotion::ParagraphUp,
        Local::ParagraphDown => WireViMotion::ParagraphDown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_backend::crosswords::square::ContentTag;
    use rio_session::protocol::{
        CellContentFrame, CellFrame, ColorFrame, CursorFrame, FrameDelta, GraphicsFrame,
        RowFrame, RowUpdate, SelectionFrame, StyleFrame,
    };

    #[cfg(unix)]
    fn review_home() -> String {
        let root = std::env::var_os("RIO_ACCEPT_ARTIFACT_ROOT")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .map(|home| home.join("dev/rio-agent-artifacts"))
            })
            .unwrap_or_else(|| std::path::PathBuf::from("rio-agent-artifacts"));
        let path = root.join(format!("session-review-home-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(
            &path,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        path.to_string_lossy().into_owned()
    }

    fn frame(columns: u16, lines: u16, rows: Vec<RowFrame>) -> FullFrame {
        FullFrame {
            sequence: 1,
            columns,
            lines,
            rows,
            display_offset: 0,
            history_size: 0,
            lines_evicted: 0,
            alternate_screen: false,
            modes: 0,
            cursor: CursorFrame {
                line: 0,
                column: 0,
                visible: true,
                blinking: false,
                shape: 0,
            },
            selection: None,
            colors: vec![None; 269],
            graphics: GraphicsFrame::default(),
            title: String::new(),
            working_dir: None,
        }
    }

    fn delta(
        columns: u16,
        lines: u16,
        sequence: u64,
        rows: Vec<RowUpdate>,
    ) -> FrameDelta {
        FrameDelta {
            base_sequence: sequence - 1,
            sequence,
            columns,
            lines,
            rows,
            display_offset: 0,
            history_size: 0,
            lines_evicted: 0,
            alternate_screen: false,
            modes: 0,
            cursor: CursorFrame {
                line: 0,
                column: 0,
                visible: true,
                blinking: false,
                shape: 0,
            },
            selection: None,
            colors: vec![None; 269],
            title: String::new(),
            working_dir: None,
        }
    }

    #[test]
    fn malformed_full_frame_row_count_is_reported() {
        let view = RemoteView::from_frame(frame(1, 1, Vec::new()), WindowId::from(0));

        assert_eq!(
            view.decoder_error(),
            Some("passive frame row count does not match its dimensions")
        );
    }

    #[test]
    fn frame_delta_cursor_only_does_not_rebuild_rows() {
        let mut view = RemoteView::from_frame(
            frame(
                2,
                1,
                vec![RowFrame {
                    cells: vec![
                        CellFrame {
                            content: CellContentFrame::Codepoint('a' as u32),
                            wide: 0,
                            flags: 0,
                        },
                        CellFrame {
                            content: CellContentFrame::Codepoint('b' as u32),
                            wide: 0,
                            flags: 0,
                        },
                    ],
                    styles: vec![default_style(), default_style()],
                    extras: vec![None, None],
                    kitty_virtual_placeholder: false,
                    text: "ab".into(),
                }],
            ),
            WindowId::from(0),
        );
        let original_row = view.grid.rows[0].clone();
        for row in &mut view.grid.rows {
            row.dirty = false;
        }
        view.pending_frame_damage = rio_backend::event::TerminalDamage::Noop;
        let mut update = delta(2, 1, 2, Vec::new());
        update.cursor.column = 1;
        update.cursor.visible = false;
        assert!(view.apply_frame_update(FrameUpdate::Delta(update)).is_ok());
        assert_eq!(
            view.pending_frame_damage,
            rio_backend::event::TerminalDamage::CursorOnly
        );
        assert_eq!(view.grid.rows[0], original_row);
    }

    #[test]
    fn frame_delta_updates_only_changed_rows_and_preserves_interned_data() {
        let base_row = RowFrame {
            cells: vec![CellFrame {
                content: CellContentFrame::Codepoint('a' as u32),
                wide: 0,
                flags: 0,
            }],
            styles: vec![default_style()],
            extras: vec![Some(rio_session::protocol::ExtrasFrame {
                zero_width: vec![0x301],
                hyperlink: Some(rio_session::protocol::HyperlinkFrame {
                    id: "example".into(),
                    uri: "https://example.com".into(),
                }),
            })],
            kitty_virtual_placeholder: false,
            text: "a\u{301}".into(),
        };
        let other_row = RowFrame {
            cells: vec![CellFrame {
                content: CellContentFrame::Codepoint('b' as u32),
                wide: 0,
                flags: 0,
            }],
            styles: vec![default_style()],
            extras: vec![None],
            kitty_virtual_placeholder: false,
            text: "b".into(),
        };
        let mut view = RemoteView::from_frame(
            frame(1, 2, vec![base_row.clone(), other_row]),
            WindowId::from(0),
        );
        view.pending_frame_damage = rio_backend::event::TerminalDamage::Noop;
        for row in &mut view.grid.rows {
            row.dirty = false;
        }
        let original_extra_id = view.grid.rows[0].inner[0].extras_id_checked();
        let original_other_row = view.grid.rows[1].clone();
        let changed_row = RowFrame {
            cells: vec![CellFrame {
                content: CellContentFrame::Codepoint('c' as u32),
                wide: 0,
                flags: 0,
            }],
            styles: vec![StyleFrame {
                flags: StyleFlags::BOLD.bits(),
                ..default_style()
            }],
            extras: vec![None],
            kitty_virtual_placeholder: false,
            text: "c".into(),
        };
        let update = delta(
            1,
            2,
            2,
            vec![RowUpdate {
                line: 1,
                row: changed_row,
            }],
        );
        assert!(view.apply_frame_update(FrameUpdate::Delta(update)).is_ok());
        assert_eq!(
            view.grid.rows[0].inner[0].extras_id_checked(),
            original_extra_id
        );
        assert_eq!(view.grid.rows[1].inner[0].c(), 'c');
        assert_eq!(view.grid.rows[1].inner[0].style_id(), 1);
        assert_eq!(
            view.grid.rows[1].inner[0].wide(),
            original_other_row.inner[0].wide()
        );
        assert_eq!(
            view.grid
                .extras
                .get(&original_extra_id.unwrap())
                .unwrap()
                .hyperlink
                .as_ref()
                .unwrap()
                .uri(),
            "https://example.com"
        );
        assert_eq!(
            view.pending_frame_damage,
            rio_backend::event::TerminalDamage::Partial
        );
        assert!(!view.grid.rows[0].dirty);
        assert!(view.grid.rows[1].dirty);
    }

    #[test]
    fn decoded_hyperlink_preserves_explicit_id() {
        let extra = rio_session::protocol::ExtrasFrame {
            zero_width: Vec::new(),
            hyperlink: Some(rio_session::protocol::HyperlinkFrame {
                id: "shared-target".into(),
                uri: "https://example.com".into(),
            }),
        };

        let hyperlink = decode_extras(&extra).hyperlink.unwrap();
        assert_eq!(hyperlink.id(), "shared-target");
        assert_eq!(hyperlink.uri(), "https://example.com");
    }

    #[test]
    fn frame_delta_selection_invalidates_all_rows() {
        let mut view = RemoteView::from_frame(
            frame(
                1,
                2,
                vec![
                    RowFrame {
                        cells: vec![CellFrame {
                            content: CellContentFrame::Codepoint('a' as u32),
                            wide: 0,
                            flags: 0,
                        }],
                        styles: vec![default_style()],
                        extras: vec![None],
                        kitty_virtual_placeholder: false,
                        text: "a".into(),
                    },
                    RowFrame {
                        cells: vec![CellFrame {
                            content: CellContentFrame::Codepoint('b' as u32),
                            wide: 0,
                            flags: 0,
                        }],
                        styles: vec![default_style()],
                        extras: vec![None],
                        kitty_virtual_placeholder: false,
                        text: "b".into(),
                    },
                ],
            ),
            WindowId::from(0),
        );
        for row in &mut view.grid.rows {
            row.dirty = false;
        }
        view.pending_frame_damage = rio_backend::event::TerminalDamage::Noop;
        let mut update = delta(1, 2, 2, Vec::new());
        update.selection = Some(SelectionFrame {
            start_line: 0,
            start_column: 0,
            end_line: 1,
            end_column: 0,
            block: false,
        });

        assert!(view.apply_frame_update(FrameUpdate::Delta(update)).is_ok());
        assert_eq!(
            view.pending_frame_damage,
            rio_backend::event::TerminalDamage::Partial
        );
        assert!(view.grid.rows.iter().all(|row| row.dirty));
    }

    #[test]
    fn scrolled_selection_highlights_viewport_rows_in_full_and_delta_frames() {
        let rows = (0..3)
            .map(|_| RowFrame {
                cells: vec![
                    CellFrame {
                        content: CellContentFrame::Codepoint('a' as u32),
                        wide: 0,
                        flags: 0,
                    };
                    4
                ],
                styles: vec![default_style(); 4],
                extras: vec![None; 4],
                kitty_virtual_placeholder: false,
                text: "a".into(),
            })
            .collect();
        let mut snapshot = frame(4, 3, rows);
        snapshot.display_offset = 20;
        snapshot.history_size = 20;
        snapshot.selection = Some(SelectionFrame {
            start_line: 0,
            start_column: 1,
            end_line: 1,
            end_column: 2,
            block: false,
        });
        let mut view = RemoteView::from_frame(snapshot, WindowId::from(0));
        assert_eq!(view.decoder_error(), None);
        let selected_row = |view: &RemoteView, y| {
            rio_grid::row_selection_for(
                view.selection_range,
                y,
                4,
                view.grid.display_offset as i32,
            )
            .map(|interval| (interval.lo, interval.hi))
        };
        assert_eq!(selected_row(&view, 0), Some((1, 3)));
        assert_eq!(selected_row(&view, 1), Some((0, 2)));
        assert_eq!(selected_row(&view, 2), None);

        let mut update = delta(4, 3, 2, Vec::new());
        update.display_offset = 25;
        update.history_size = 25;
        update.selection = Some(SelectionFrame {
            start_line: 1,
            start_column: 0,
            end_line: 2,
            end_column: 2,
            block: false,
        });
        assert!(view.apply_frame_update(FrameUpdate::Delta(update)).is_ok());
        assert_eq!(selected_row(&view, 0), None);
        assert_eq!(selected_row(&view, 1), Some((0, 3)));
        assert_eq!(selected_row(&view, 2), Some((0, 2)));
    }

    #[test]
    fn frame_delta_base_mismatch_preserves_cached_frame_for_resync() {
        let mut view = RemoteView::from_frame(
            frame(
                1,
                1,
                vec![RowFrame {
                    cells: vec![CellFrame {
                        content: CellContentFrame::Codepoint('a' as u32),
                        wide: 0,
                        flags: 0,
                    }],
                    styles: vec![default_style()],
                    extras: vec![None],
                    kitty_virtual_placeholder: false,
                    text: "a".into(),
                }],
            ),
            WindowId::from(0),
        );
        view.pending_frame_damage = rio_backend::event::TerminalDamage::Noop;
        let mut update = delta(1, 1, 3, Vec::new());
        update.base_sequence = 2;

        assert!(view.apply_frame_update(FrameUpdate::Delta(update)).is_err());
        assert_eq!(view.frame.as_ref().unwrap().sequence, 1);
        assert_eq!(view.grid.rows[0].inner[0].c(), 'a');
        assert_eq!(
            view.pending_frame_damage,
            rio_backend::event::TerminalDamage::Noop
        );
    }

    #[test]
    fn delta_resync_retries_after_snapshot_queue_pressure() {
        let (handle, receiver) = selection_test_handle(1);
        handle
            .enqueue(SessionCommand::Write(b"occupied".to_vec()))
            .unwrap();
        let mut view = RemoteView::from_frame(
            frame(
                1,
                1,
                vec![RowFrame {
                    cells: vec![CellFrame {
                        content: CellContentFrame::Codepoint('a' as u32),
                        wide: 0,
                        flags: 0,
                    }],
                    styles: vec![default_style()],
                    extras: vec![None],
                    kitty_virtual_placeholder: false,
                    text: "a".into(),
                }],
            ),
            WindowId::from(0),
        );
        view.install_session(handle.clone());

        let publish_mismatched_delta = || {
            let mut update = delta(1, 1, 2, Vec::new());
            update.base_sequence = 2;
            handle
                .state
                .publish_frame_update(FrameUpdate::Delta(update));
        };

        publish_mismatched_delta();
        view.refresh();
        assert!(!view.delta_resync_logged);
        assert!(matches!(
            receiver.try_recv(),
            Ok(PumpCommand::Terminal(SessionCommand::Write(_)))
        ));

        publish_mismatched_delta();
        view.refresh();
        assert!(matches!(
            receiver.try_recv(),
            Ok(PumpCommand::Terminal(SessionCommand::Snapshot))
        ));
    }

    #[test]
    fn frame_delta_decode_failure_preserves_cached_frame() {
        let mut view = RemoteView::from_frame(
            frame(
                1,
                1,
                vec![RowFrame {
                    cells: vec![CellFrame {
                        content: CellContentFrame::Codepoint('a' as u32),
                        wide: 0,
                        flags: 0,
                    }],
                    styles: vec![default_style()],
                    extras: vec![None],
                    kitty_virtual_placeholder: false,
                    text: "a".into(),
                }],
            ),
            WindowId::from(0),
        );
        view.pending_frame_damage = rio_backend::event::TerminalDamage::Noop;
        let invalid = delta(
            1,
            1,
            2,
            vec![RowUpdate {
                line: 0,
                row: RowFrame {
                    cells: vec![
                        CellFrame {
                            content: CellContentFrame::Codepoint('b' as u32),
                            wide: 0,
                            flags: 0,
                        },
                        CellFrame {
                            content: CellContentFrame::Codepoint('c' as u32),
                            wide: 0,
                            flags: 0,
                        },
                    ],
                    styles: vec![default_style(), default_style()],
                    extras: vec![None, None],
                    kitty_virtual_placeholder: false,
                    text: "bc".into(),
                },
            }],
        );

        assert!(view
            .apply_frame_update(FrameUpdate::Delta(invalid))
            .is_err());
        assert_eq!(view.frame.as_ref().unwrap().sequence, 1);
        assert_eq!(view.grid.rows[0].inner[0].c(), 'a');
        assert_eq!(
            view.pending_frame_damage,
            rio_backend::event::TerminalDamage::Noop
        );
    }

    #[test]
    fn frame_damage_merges_across_publications_until_render_consumes_it() {
        let mut view = RemoteView::from_frame(
            frame(
                1,
                1,
                vec![RowFrame {
                    cells: vec![CellFrame {
                        content: CellContentFrame::Codepoint('a' as u32),
                        wide: 0,
                        flags: 0,
                    }],
                    styles: vec![default_style()],
                    extras: vec![None],
                    kitty_virtual_placeholder: false,
                    text: "a".into(),
                }],
            ),
            WindowId::from(0),
        );
        let mut content = RenderableContent::new(Default::default());
        content.frame_damage = rio_backend::event::TerminalDamage::CursorOnly;
        view.pending_frame_damage = rio_backend::event::TerminalDamage::Noop;
        let mut cursor_update = delta(1, 1, 2, Vec::new());
        cursor_update.cursor.visible = false;
        view.apply_frame_update(FrameUpdate::Delta(cursor_update))
            .unwrap();
        view.refresh_renderable(&mut content);
        assert_eq!(
            content.frame_damage,
            rio_backend::event::TerminalDamage::CursorOnly
        );

        let row = RowFrame {
            cells: vec![CellFrame {
                content: CellContentFrame::Codepoint('b' as u32),
                wide: 0,
                flags: 0,
            }],
            styles: vec![default_style()],
            extras: vec![None],
            kitty_virtual_placeholder: false,
            text: "b".into(),
        };
        view.apply_frame_update(FrameUpdate::Delta(delta(
            1,
            1,
            3,
            vec![RowUpdate { line: 0, row }],
        )))
        .unwrap();
        view.refresh_renderable(&mut content);
        assert_eq!(
            content.frame_damage,
            rio_backend::event::TerminalDamage::Partial
        );
        assert_eq!(view.grid.rows[0].inner[0].c(), 'b');
    }

    #[test]
    fn only_unpoisoned_command_rejections_keep_the_pump_alive() {
        for error in [
            SessionError::Invalid("bad input".into()),
            SessionError::Unsupported("effect".into()),
        ] {
            assert!(recoverable_command_rejection(&error, false));
            assert!(!recoverable_command_rejection(&error, true));
        }
        for error in [
            SessionError::Detached,
            SessionError::WorkerExited,
            SessionError::Protocol("bad reply".into()),
            SessionError::Codec("decode".into()),
            SessionError::Io(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        ] {
            assert!(!recoverable_command_rejection(&error, false));
        }
    }

    #[test]
    fn history_search_matches_keep_negative_terminal_coordinates() {
        let mut view = RemoteView::new(None, WindowId::from(0), 80, 24);
        view.grid.history = 20;
        view.search_matches = Some(vec![SearchMatch {
            start_line: 8,
            start_column: 2,
            end_line: 9,
            end_column: 5,
        }]);
        let matches = view.take_search_matches().unwrap();
        assert_eq!(
            matches,
            vec![Pos::new(Line(-12), Column(2))..=Pos::new(Line(-11), Column(5))]
        );
    }

    #[test]
    fn command_sender_disconnects_after_its_last_owner_drops() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let wakeup =
            new_command_wakeup().expect("failed to create session command wakeup");
        let first = CommandSender::new(sender, wakeup);
        let second = first.clone();

        drop(first);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        drop(second);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_last_sender_drops_wake_a_blocked_waiter() {
        use std::os::fd::{AsFd, AsRawFd};
        use std::sync::Barrier;

        for _ in 0..16 {
            let (sender, receiver) = mpsc::sync_channel(1);
            let wakeup =
                new_command_wakeup().expect("failed to create session command wakeup");
            let waiter_wakeup = Arc::clone(&wakeup);
            let owner = CommandSender::new(sender, wakeup);
            let first = owner.clone();
            let second = owner.clone();
            drop(owner);

            let barrier = Arc::new(Barrier::new(3));
            let first_barrier = Arc::clone(&barrier);
            let first_thread = thread::spawn(move || {
                first_barrier.wait();
                drop(first);
            });
            let second_barrier = Arc::clone(&barrier);
            let second_thread = thread::spawn(move || {
                second_barrier.wait();
                drop(second);
            });
            barrier.wait();

            let mut poll_fds = [libc::pollfd {
                fd: waiter_wakeup.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let ready = rio_session::readiness::wait(
                &mut poll_fds,
                Some(std::time::Instant::now() + Duration::from_secs(1)),
            )
            .unwrap();
            assert_eq!(ready, 1);
            assert!(rio_session::readiness::is_readable(poll_fds[0].revents));
            waiter_wakeup.clear();
            first_thread.join().unwrap();
            second_thread.join().unwrap();
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ));
        }
    }

    #[test]
    fn close_preserves_admitted_commands_when_queue_is_full() {
        let (handle, receiver) = selection_test_handle(1);
        handle.enqueue(SessionCommand::Snapshot).unwrap();
        handle.close();
        assert!(matches!(
            handle.enqueue(SessionCommand::Snapshot),
            Err(SessionError::Detached)
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(PumpCommand::Terminal(SessionCommand::Snapshot))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    fn selection_test_handle(
        capacity: usize,
    ) -> (SessionHandle, mpsc::Receiver<PumpCommand>) {
        let (commands, receiver) = mpsc::sync_channel(capacity);
        let command_wakeup =
            new_command_wakeup().expect("failed to create session command wakeup");
        (
            SessionHandle {
                commands: CommandSender::new(commands, command_wakeup),
                state: Arc::new(SessionState::new()),
                window_id: Arc::new(Mutex::new(WindowId::from(1))),
                closed: Arc::new(Mutex::new(false)),
            },
            receiver,
        )
    }

    #[test]
    fn paste_commands_preserve_raw_input_and_delegate_bracketed_paste() {
        let (handle, receiver) = selection_test_handle(5);
        let mut view = RemoteView::new(Some(handle), WindowId::from(1), 80, 24);

        view.paste("\x7f".into(), false);
        view.paste("\x1b[A".into(), false);
        view.paste("\x1b]1337;SetMark\x07".into(), false);
        view.paste("line\nraw".into(), false);
        view.paste("line\ntext".into(), true);

        for expected in [
            SessionCommand::Write(b"\x7f".to_vec()),
            SessionCommand::Write(b"\x1b[A".to_vec()),
            SessionCommand::Write(b"\x1b]1337;SetMark\x07".to_vec()),
            SessionCommand::Write(b"line\nraw".to_vec()),
            SessionCommand::Paste("line\ntext".into()),
        ] {
            match receiver.try_recv().unwrap() {
                PumpCommand::Terminal(command) => assert_eq!(command, expected),
                PumpCommand::SelectionText { .. } => {
                    panic!("unexpected selection command")
                }
            }
        }
    }

    #[test]
    fn selection_handoff_reserves_unread_replies_and_rejects_pressure() {
        let (handle, receiver) = selection_test_handle(COMMAND_QUEUE_SIZE);
        for _ in 0..EVENT_QUEUE_SIZE {
            handle
                .request_selection_text(ClipboardType::Selection, true)
                .unwrap();
            let PumpCommand::SelectionText {
                target,
                copy_to_clipboard,
            } = receiver.try_recv().unwrap()
            else {
                panic!("missing selection command")
            };
            handle
                .state
                .selection_text
                .lock()
                .unwrap()
                .replies
                .push_back((target, Some("retained".into()), copy_to_clipboard));
        }
        assert!(matches!(
            handle.request_selection_text(ClipboardType::Clipboard, false),
            Err(SessionError::Invalid(_))
        ));
        assert!(handle
            .error()
            .unwrap()
            .contains("selection reply handoff is full"));
        assert_eq!(
            handle.take_selection_text(),
            Some((ClipboardType::Selection, Some("retained".into()), true))
        );
        handle
            .request_selection_text(ClipboardType::Clipboard, false)
            .unwrap();
        for _ in 1..EVENT_QUEUE_SIZE {
            assert_eq!(
                handle.take_selection_text(),
                Some((ClipboardType::Selection, Some("retained".into()), true))
            );
        }
        assert!(handle.take_selection_text().is_none());
        assert_eq!(handle.state.selection_text.lock().unwrap().outstanding, 1);
    }

    #[test]
    fn rejected_selection_enqueue_does_not_reserve_reply_capacity() {
        let (handle, receiver) = selection_test_handle(1);
        handle.enqueue(SessionCommand::Snapshot).unwrap();
        assert!(handle
            .request_selection_text(ClipboardType::Clipboard, false)
            .is_err());
        assert_eq!(handle.state.selection_text.lock().unwrap().outstanding, 0);
        receiver.try_recv().unwrap();
        handle
            .request_selection_text(ClipboardType::Selection, true)
            .unwrap();
        assert_eq!(handle.state.selection_text.lock().unwrap().outstanding, 1);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires worker-only RIO_TEST_BINARY and private runtime directory"]
    fn review_close_drains_full_queue_once_after_last_handle_drops() {
        use rio_backend::event::VoidListener;
        for _ in 0..4 {
            let client = Arc::new(review_worker());
            wait_for_review_condition(|| client.snapshot().unwrap().title == "ready");
            let (handle, receiver) = selection_test_handle(1);
            handle
                .request_selection_text(ClipboardType::Selection, true)
                .unwrap();
            assert!(
                handle.enqueue(SessionCommand::Snapshot).is_err(),
                "queue must be full"
            );
            let other = handle.clone();
            handle.close();
            other.close();
            other.enqueue(SessionCommand::Close).unwrap();
            assert!(matches!(
                handle.enqueue(SessionCommand::Snapshot),
                Err(SessionError::Detached)
            ));
            let state = Arc::clone(&handle.state);
            let pump = SessionPump {
                client: Arc::clone(&client),
                receiver,
                state: Arc::clone(&state),
                event_proxy: VoidListener,
                route_id: 1,
                window_id: Arc::clone(&handle.window_id),
                closed: Arc::clone(&handle.closed),
                command_wakeup: Arc::clone(&handle.commands.channel.wakeup),
            };
            // No sender or queue slot is needed to retain explicit close intent.
            drop(other);
            drop(handle);
            let (done, finished) = mpsc::sync_channel(1);
            let thread = thread::spawn(move || {
                pump.run(PumpStartup::Snapshot);
                done.send(()).unwrap();
            });
            finished
                .recv_timeout(Duration::from_secs(5))
                .expect("close pump did not finish");
            thread.join().unwrap();
            let results = state.selection_text.lock().unwrap();
            assert_eq!(results.outstanding, 1);
            assert_eq!(
                results.replies.front(),
                Some(&(ClipboardType::Selection, None, true))
            );
            assert_eq!(
                results.replies.len(),
                1,
                "accepted reply must precede Close"
            );
            drop(results);
            wait_for_review_condition(|| !client.descriptor().endpoint.exists());
            assert!(client.wait_worker().unwrap().unwrap().success());
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires worker-only RIO_TEST_BINARY and private runtime directory"]
    fn review_overlapping_selection_replies_keep_destinations_and_copy_flags() {
        use rio_backend::event::VoidListener;
        let client = Arc::new(review_worker());
        wait_for_review_condition(|| client.snapshot().unwrap().title == "ready");
        client.write(b"clipboard-request-text".to_vec()).unwrap();
        wait_for_review_condition(|| {
            client
                .snapshot()
                .unwrap()
                .rows
                .iter()
                .any(|row| row.text.contains("clipboard-request-text"))
        });
        let (handle, receiver) = selection_test_handle(COMMAND_QUEUE_SIZE);
        let mut view = RemoteView::new(Some(handle.clone()), WindowId::from(1), 80, 24);
        handle.enqueue(SessionCommand::SelectAll).unwrap();
        view.request_selection_text_with_copy(ClipboardType::Clipboard, false);
        view.request_selection_text_with_copy(ClipboardType::Selection, true);
        handle.enqueue(SessionCommand::SelectionClear).unwrap();
        view.request_selection_text_with_copy(ClipboardType::Selection, false);
        let pump = SessionPump {
            client: Arc::clone(&client),
            receiver,
            state: Arc::clone(&handle.state),
            event_proxy: VoidListener,
            route_id: 1,
            window_id: Arc::clone(&handle.window_id),
            closed: Arc::clone(&handle.closed),
            command_wakeup: Arc::clone(&handle.commands.channel.wakeup),
        };
        let thread = thread::spawn(move || pump.run(PumpStartup::Snapshot));
        // All replies must be retained before the GUI consumes any of them.
        wait_for_review_condition(|| {
            handle.state.selection_text.lock().unwrap().replies.len() == 3
        });
        let first = view.take_selection_text().unwrap();
        assert_eq!(first.0, ClipboardType::Clipboard);
        assert!(!first.2);
        assert!(first.1.as_ref().unwrap().contains("clipboard-request-text"));
        assert_eq!(
            view.take_selection_text(),
            Some((ClipboardType::Selection, first.1, true))
        );
        assert_eq!(
            view.take_selection_text(),
            Some((ClipboardType::Selection, None, false))
        );
        assert!(view.take_selection_text().is_none());
        assert_eq!(handle.state.selection_text.lock().unwrap().outstanding, 0);
        handle.close();
        wait_for_review_condition(|| !client.descriptor().endpoint.exists());
        thread.join().unwrap();
        assert!(client.wait_worker().unwrap().unwrap().success());
    }

    #[cfg(unix)]
    fn review_worker() -> SessionClient {
        SessionClient::spawn_with_worker_path(
            SessionSpec {
                shell: Some("/bin/sh".into()),
                args: vec![
                    "-c".into(),
                    r"stty raw -echo; printf '\033]2;ready\007'; cat".into(),
                ],
                environment: vec![
                    rio_session::protocol::EnvVar::new("HOME", review_home()),
                    rio_session::protocol::EnvVar::new("PATH", "/usr/bin:/bin"),
                    rio_session::protocol::EnvVar::new("TERM", "xterm-rio"),
                ],
                ..SessionSpec::default()
            },
            std::env::var_os("RIO_TEST_BINARY").expect("set worker-only RIO_TEST_BINARY"),
        )
        .unwrap()
    }

    #[cfg(unix)]
    #[track_caller]
    fn wait_for_review_condition(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "worker condition timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires worker-only RIO_TEST_BINARY and private runtime directory"]
    fn review_snapshot_rejection_recovers_and_last_sender_detaches_pump() {
        use rio_backend::event::VoidListener;
        let client = Arc::new(review_worker());
        wait_for_review_condition(|| client.snapshot().unwrap().title == "ready");
        let pid = client.child_pid().unwrap();
        // Retained RGBA pixels exceed the session's per-image snapshot budget.
        client
            .write(b"\x1bPq\"1;1;1536;1536#1;2;100;0;0~\x1b\\".to_vec())
            .unwrap();
        wait_for_review_condition(|| {
            matches!(client.snapshot(), Err(SessionError::Unsupported(_)))
        });
        let state = Arc::new(SessionState::new());
        let (commands, receiver) = mpsc::sync_channel(COMMAND_QUEUE_SIZE);
        let command_wakeup =
            new_command_wakeup().expect("failed to create session command wakeup");
        let commands = CommandSender::new(commands, Arc::clone(&command_wakeup));
        let pump = SessionPump {
            client: Arc::clone(&client),
            receiver,
            state: Arc::clone(&state),
            event_proxy: VoidListener,
            route_id: 1,
            window_id: Arc::new(Mutex::new(WindowId::from(1))),
            closed: Arc::new(Mutex::new(false)),
            command_wakeup,
        };
        let (done, finished) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            pump.run(PumpStartup::Snapshot);
            done.send(()).unwrap();
        });
        wait_for_review_condition(|| state.error().is_some());
        assert!(matches!(
            *state.status.lock().unwrap(),
            SessionStatus::Running(_)
        ));
        // Explicit child output resets all graphics, including offscreen spans.
        commands
            .send(PumpCommand::Terminal(SessionCommand::Write(
                b"\x1bc".to_vec(),
            )))
            .unwrap();
        wait_for_review_condition(|| state.frames.lock().unwrap().pending_full.is_some());
        assert!(state.error().is_none());
        assert_eq!(client.child_pid().unwrap(), pid);
        drop(commands);
        finished
            .recv_timeout(Duration::from_secs(2))
            .expect("pump must stop when last sender drops");
        thread.join().unwrap();
        assert_eq!(
            client.child_pid().unwrap(),
            pid,
            "pump drop must not close the worker"
        );
        client.close().unwrap();
        assert!(client.wait_worker().unwrap().unwrap().success());
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires worker-only RIO_TEST_BINARY and private runtime directory"]
    fn review_prepared_context_resizes_only_after_commit() {
        use rio_backend::event::VoidListener;
        let owner = review_worker();
        let before = owner.snapshot().unwrap();
        let prepared = SessionHandle::prepare_attach(
            SessionClient::prepare_attach(owner.descriptor().clone()).unwrap(),
        );
        let dimension = crate::layout::ContextDimension {
            columns: 40,
            lines: 10,
            width: 320.0,
            height: 160.0,
            ..Default::default()
        };
        let mut context = super::super::create_prepared_context::<VoidListener>(
            prepared,
            WindowId::from(1),
            1,
            1,
            dimension,
        );
        assert_eq!(owner.snapshot().unwrap().columns, before.columns);
        context.commit_pending_session(VoidListener).unwrap();
        wait_for_review_condition(|| {
            context
                .terminal
                .lock()
                .session()
                .unwrap()
                .state
                .frames
                .lock()
                .unwrap()
                .pending_full
                .as_ref()
                .is_some_and(|frame| frame.columns == 40 && frame.lines == 10)
        });
        assert_eq!(
            context
                .terminal
                .lock()
                .session()
                .unwrap()
                .descriptor()
                .unwrap()
                .session_id,
            owner.descriptor().session_id
        );
        drop(context);
        wait_for_review_condition(|| !owner.descriptor().endpoint.exists());
        assert!(owner.wait_worker().unwrap().unwrap().success());
    }

    fn default_style() -> StyleFrame {
        StyleFrame {
            foreground: ColorFrame::Named(256),
            background: ColorFrame::Named(257),
            underline: None,
            flags: 0,
        }
    }

    #[test]
    fn background_cells_keep_inline_background_and_styles_are_interned() {
        let bold = StyleFrame {
            flags: StyleFlags::BOLD.bits(),
            ..default_style()
        };
        let cells = vec![
            CellFrame {
                content: CellContentFrame::Palette(7),
                wide: 0,
                flags: 0,
            },
            CellFrame {
                content: CellContentFrame::Rgb { r: 1, g: 2, b: 3 },
                wide: 0,
                flags: 0,
            },
            CellFrame {
                content: CellContentFrame::Codepoint('a' as u32),
                wide: 0,
                flags: 0,
            },
            CellFrame {
                content: CellContentFrame::Codepoint('b' as u32),
                wide: 0,
                flags: 0,
            },
        ];
        let styles = vec![default_style(), default_style(), default_style(), bold];
        let row = RowFrame {
            cells,
            styles,
            extras: vec![None, None, None, None],
            kitty_virtual_placeholder: false,
            text: "ab".into(),
        };
        let view = RemoteView::from_frame(frame(4, 1, vec![row]), WindowId::from(0));
        let first = &view.grid.rows[0].inner;
        assert_eq!(first[0].content_tag(), ContentTag::BgPalette);
        assert_eq!(first[0].bg_palette_index(), 7);
        assert_eq!(first[1].content_tag(), ContentTag::BgRgb);
        assert_eq!(first[1].bg_rgb(), (1, 2, 3));
        assert_eq!(first[2].style_id(), 0);
        assert_eq!(first[3].style_id(), 1);
    }

    #[test]
    fn passive_view_owns_the_published_frame_and_keeps_interaction_data() {
        let snapshot = frame(
            1,
            1,
            vec![RowFrame {
                cells: vec![CellFrame {
                    content: CellContentFrame::Codepoint('x' as u32),
                    wide: 0,
                    flags: 0,
                }],
                styles: vec![default_style()],
                extras: vec![Some(rio_session::protocol::ExtrasFrame {
                    zero_width: vec![0x301],
                    hyperlink: Some(rio_session::protocol::HyperlinkFrame {
                        id: "example".into(),
                        uri: "https://example.com".into(),
                    }),
                })],
                kitty_virtual_placeholder: false,
                text: "x\u{301}".into(),
            }],
        );
        let mut view = RemoteView::new(None, WindowId::from(0), 1, 1);
        view.apply_frame(snapshot).unwrap();
        let mut content = RenderableContent::new(Default::default());
        view.decode_error = Some("visible renderer error".into());
        view.refresh_renderable(&mut content);

        let pos = Pos::new(Line(0), Column(0));
        assert_eq!(view.grid.cell_text(pos).collect::<String>(), "x\u{301}");
        assert!(view.grid.hyperlink(pos).is_some());
        assert_eq!(content.screen_lines, 1);
        assert_eq!(
            content.session_error.as_deref(),
            Some("visible renderer error")
        );
        assert_eq!(view.frame.as_ref().unwrap().rows[0].text, "x\u{301}");
    }

    #[test]
    fn excessive_distinct_extras_are_reported_without_aliasing() {
        let columns = 1024usize;
        let mut rows = Vec::with_capacity(64);
        for row_index in 0..64 {
            let mut cells = Vec::with_capacity(columns);
            let mut styles = Vec::with_capacity(columns);
            let mut extras = Vec::with_capacity(columns);
            for column in 0..columns {
                let index = row_index * columns + column;
                cells.push(CellFrame {
                    content: CellContentFrame::Codepoint('x' as u32),
                    wide: 0,
                    flags: 0,
                });
                styles.push(default_style());
                extras.push(Some(rio_session::protocol::ExtrasFrame {
                    zero_width: vec![],
                    hyperlink: Some(rio_session::protocol::HyperlinkFrame {
                        id: index.to_string(),
                        uri: format!("https://{index}.invalid"),
                    }),
                }));
            }
            rows.push(RowFrame {
                cells,
                styles,
                extras,
                kitty_virtual_placeholder: false,
                text: String::new(),
            });
        }
        let view =
            RemoteView::from_frame(frame(columns as u16, 64, rows), WindowId::from(0));
        assert!(view
            .decode_error
            .as_deref()
            .is_some_and(|error| error.contains("extras")));
    }

    #[test]
    fn published_frame_clears_recoverable_command_error() {
        let state = SessionState::new();
        state.record_error(SessionError::Invalid("temporary command failure".into()));
        state.publish_frame(frame(
            1,
            1,
            vec![RowFrame {
                cells: vec![CellFrame {
                    content: CellContentFrame::Codepoint('x' as u32),
                    wide: 0,
                    flags: 0,
                }],
                styles: vec![default_style()],
                extras: vec![None],
                kitty_virtual_placeholder: false,
                text: "x".into(),
            }],
        ));
        assert!(state.error().is_none());

        state.record_error(SessionError::Invalid("temporary command failure".into()));
        assert!(state.publish_frame_update(FrameUpdate::Full(frame(
            2,
            1,
            vec![RowFrame {
                cells: vec![CellFrame {
                    content: CellContentFrame::Codepoint('y' as u32),
                    wide: 0,
                    flags: 0,
                }],
                styles: vec![default_style()],
                extras: vec![None],
                kitty_virtual_placeholder: false,
                text: "y".into(),
            }],
        ))));
        assert!(state.error().is_none());

        state.record_error(SessionError::Invalid("temporary command failure".into()));
        assert!(state.publish_frame_update(FrameUpdate::Delta(delta(2, 1, 2, vec![]))));
        assert!(state.error().is_none());

        for sequence in 3..(FRAME_UPDATE_QUEUE_SIZE as u64 + 2) {
            assert!(state.publish_frame_update(FrameUpdate::Delta(delta(
                2,
                1,
                sequence,
                vec![]
            ))));
        }
        state.record_error(SessionError::Invalid("retry needed".into()));
        assert!(!state.publish_frame_update(FrameUpdate::Delta(delta(
            2,
            1,
            FRAME_UPDATE_QUEUE_SIZE as u64 + 2,
            vec![]
        ))));
        assert!(state.error().is_some());

        state.take_frame_updates();
        state.fail(SessionError::Protocol("worker disconnected".into()));
        let failure = state.error();
        assert!(state.publish_frame_update(FrameUpdate::Delta(delta(2, 1, 2, vec![]))));
        state.record_error(SessionError::Invalid("late command rejection".into()));
        state.publish_event(SessionEvent::Closed);
        assert_eq!(state.error(), failure);
    }

    #[test]
    fn closed_session_preserves_its_final_diagnostic() {
        for initial_error in [None, Some("last command error")] {
            let state = SessionState::new();
            if let Some(error) = initial_error {
                state.record_error(SessionError::Invalid(error.into()));
            }
            let diagnostic = state.error();
            state.publish_event(SessionEvent::Closed);
            state.record_error(SessionError::Detached);
            assert!(state.publish_frame_update(FrameUpdate::Delta(delta(
                1,
                1,
                2,
                vec![]
            ))));
            assert_eq!(state.error(), diagnostic);
        }
    }

    #[test]
    fn first_published_frame_marks_session_as_started() {
        let state = SessionState::new();
        assert!(!state.had_frame());
        state.publish_frame(frame(1, 1, Vec::new()));
        assert!(state.had_frame());
        // A later fatal error must not clear the marker: the stillborn
        // report gate only cares whether anything was ever displayed.
        state.fail(SessionError::Protocol("worker disconnected".into()));
        assert!(state.had_frame());
    }

    #[test]
    fn startup_failure_report_names_the_tab_failure() {
        #[derive(Clone, Default)]
        struct RecordingListener {
            events: Arc<Mutex<Vec<RioEvent>>>,
        }

        impl EventListener for RecordingListener {
            fn send_event(&self, event: RioEvent, _window_id: WindowId) {
                self.events.lock().unwrap().push(event);
            }
        }

        let listener = RecordingListener::default();
        report_startup_failure(
            &listener,
            WindowId::from(3),
            7,
            &SessionError::Invalid("socket closed".into()),
        );
        let events = listener.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let RioEvent::ReportToAssistant(error) = &events[0] else {
            panic!("startup failure must report to the assistant");
        };
        assert!(matches!(error.level, RioErrorLevel::Error));
        let RioErrorType::InitializationError(message) = &error.report else {
            panic!("startup failure must be an initialization error");
        };
        assert!(
            message.contains("could not open a new tab"),
            "unexpected message: {message}"
        );
        assert!(
            message.contains("socket closed"),
            "unexpected message: {message}"
        );
    }
}
