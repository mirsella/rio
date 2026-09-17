use crate::context::session::SessionHandle;
use crate::event::{ClickState, EventPayload, EventProxy, RioEvent, RioEventType};
use crate::ime::Preedit;
use crate::renderer::utils::update_colors_based_on_theme;
use crate::router::{routes::RoutePath, Router};
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::screen::touch::on_touch;
use crate::watcher::configuration_file_updates;
#[cfg(all(
    feature = "audio",
    not(target_os = "macos"),
    not(target_os = "windows")
))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use raw_window_handle::HasDisplayHandle;
use rio_backend::clipboard::{Clipboard, ClipboardType};
use rio_backend::config::colors::{ColorRgb, NamedColor};
use rio_session::protocol::{
    GlyphStatus, SessionCommand, SessionDescriptor, SessionEvent,
};
use rio_window::application::ApplicationHandler;
use rio_window::event::{
    ElementState, Ime, MouseButton, MouseScrollDelta, StartCause, TouchPhase, WindowEvent,
};
use rio_window::event_loop::ActiveEventLoop;
use rio_window::event_loop::ControlFlow;
use rio_window::event_loop::{DeviceEvents, EventLoop};
#[cfg(target_os = "macos")]
use rio_window::platform::macos::ActiveEventLoopExtMacOS;
#[cfg(target_os = "macos")]
use rio_window::platform::macos::WindowExtMacOS;
use rio_window::window::WindowId;
use rio_window::window::{CursorIcon, Fullscreen};
use std::collections::HashSet;
use std::error::Error;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

#[cfg(all(feature = "wayland", target_os = "linux"))]
use crate::tab_drag::{
    Command as TabDragCommand, Event as TabDragEvent, Lifecycle as TabDragLifecycle,
    OwnerRoute, TabDrag, Transition as TabDragTransition,
};
#[cfg(all(feature = "wayland", target_os = "linux"))]
use std::collections::VecDeque;
#[cfg(all(feature = "wayland", target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DeferredTabDragClose {
    Terminal(usize),
    Window(rio_backend::event::WindowId),
}

#[cfg(all(feature = "wayland", target_os = "linux"))]
struct ForeignDrag {
    window: WindowId,
    offer: crate::tab_drag::PlatformOfferId,
    index: usize,
    dropped: bool,
    selected_move: bool,
    receiving: bool,
    token: Option<[u8; 16]>,
}

fn clipboard_type(kind: u8) -> Option<ClipboardType> {
    match kind {
        0 => Some(ClipboardType::Clipboard),
        1 => Some(ClipboardType::Selection),
        _ => None,
    }
}

#[cfg(all(feature = "wayland", target_os = "linux"))]
// ponytail: fixed grace period; a compositor target-enter signal is the precise replacement.
const TAB_DETACH_GRACE_PERIOD: Duration = Duration::from_millis(750);

const MERGE_SELECTION_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(all(feature = "wayland", target_os = "linux"))]
fn deferred_window_close<D, O>(
    drag: &TabDrag<D, O>,
    window_id: WindowId,
) -> Option<DeferredTabDragClose> {
    if drag.source_window == window_id
        || drag
            .hover
            .as_ref()
            .is_some_and(|hover| hover.target_window == window_id)
    {
        Some(DeferredTabDragClose::Window(window_id.into()))
    } else {
        None
    }
}

#[cfg(all(feature = "wayland", target_os = "linux"))]
fn is_valid_tab_drag_target<D, O>(drag: &TabDrag<D, O>, window_id: WindowId) -> bool {
    drag.owner
        .as_ref()
        .is_none_or(|owner| owner.window_id != window_id)
}

pub struct Application<'a> {
    config: rio_backend::config::Config,
    event_proxy: EventProxy,
    router: Router<'a>,
    window_control: Option<crate::router::window_control::WindowControl>,
    incoming_prepared_sender: SyncSender<PreparedIncoming>,
    incoming_prepared: Receiver<PreparedIncoming>,
    pending_prepared: Vec<PendingPrepared>,
    pending_outgoing: Vec<PendingOutgoing>,
    exit_after_transfer: bool,
    exit_after_session_closes: bool,
    session_close_deadline: Option<Instant>,
    pending_session_closes: Vec<SessionHandle>,
    session_preparations: std::sync::Arc<()>,
    ready_session_imports: Vec<(rio_backend::event::WindowId, Vec<usize>)>,
    scheduler: Scheduler,
    app_id: Option<String>,
    global_hotkey: Option<crate::global_hotkey::GlobalHotkeys>,
    merge_window_source: Option<rio_backend::event::WindowId>,
    merge_target: Option<(rio_backend::event::WindowId, usize)>,
    pending_merge_selection: Option<PendingMergeSelection>,
    armed_merge_selection: Option<ArmedMergeSelection>,
    recovery_targets: Option<(rio_backend::event::WindowId, Vec<RecoveryCandidate>)>,
    recovery_probe_sender: SyncSender<RecoveryProbeResult>,
    recovery_probe: Receiver<RecoveryProbeResult>,
    recovery_probe_state: Option<RecoveryProbeState>,
    next_recovery_probe_id: u64,
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    tab_drag: Option<TabDrag>,
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    foreign_drag: Option<ForeignDrag>,
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    retained_tab_drag_owner: Option<crate::router::Route<'a>>,
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    deferred_tab_drag_closes: Vec<DeferredTabDragClose>,
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    tab_drag_detach_deadline: Option<Instant>,
    drop_marker_window: Option<rio_backend::event::WindowId>,
    /// Frontmost app when the quake window was shown, re-activated
    /// when it hides so focus returns where the user was.
    #[cfg(target_os = "macos")]
    quake_previous_app: Option<i32>,
}

struct PreparedIncoming {
    offer: crate::router::window_control::TransferOffer,
    prepared: Result<Vec<crate::context::session::PreparedSession>, String>,
    reply: SyncSender<crate::router::window_control::WindowControlResponse>,
    target_window: Option<u64>,
    target_index: Option<usize>,
}

struct PendingPrepared {
    routes: Vec<PendingPreparedRoute>,
    ready: HashSet<usize>,
    window_id: rio_backend::event::WindowId,
    deadline: Instant,
    completion: PendingPreparedCompletion,
}

#[derive(Debug)]
struct PendingPreparedRoute {
    source_route: Option<u64>,
    target_route: usize,
}

enum PendingPreparedCompletion {
    Incoming {
        reply: SyncSender<crate::router::window_control::WindowControlResponse>,
    },
    Recovery,
}

struct PendingOutgoing {
    transfer_id: [u8; 16],
    source_window: rio_backend::event::WindowId,
    source_routes: Vec<u64>,
}

struct PendingMergeSelection {
    selection_id: [u8; 16],
    source_window: rio_backend::event::WindowId,
    targets: Vec<crate::router::window_control::WindowEndpoint>,
    armed: usize,
    resolved: HashSet<[u8; 16]>,
    deadline: Instant,
}

struct ArmedMergeSelection {
    selection_id: [u8; 16],
    source: crate::router::window_control::WindowEndpoint,
    target_window: rio_backend::event::WindowId,
    hover: MergeHoverState,
    deadline: Instant,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MergeHoverState {
    index: Option<usize>,
}

impl MergeHoverState {
    fn set(&mut self, index: usize) {
        self.index = Some(index);
    }

    fn clear(&mut self) {
        self.index = None;
    }

    fn clicked_index(&self) -> Option<usize> {
        self.index
    }
}

#[inline]
fn merge_target_index_is_valid(
    index: usize,
    tab_count: usize,
    has_capacity: bool,
) -> bool {
    index <= tab_count && has_capacity
}

fn transfer_exit_ready(
    transferred: bool,
    windows: usize,
    pending: usize,
    bootstrap: bool,
) -> bool {
    transferred && windows == 0 && pending == 0 && !bootstrap
}

fn current_route_if_contexts_remain<
    T: rio_backend::event::EventListener + Clone + Send + 'static,
>(
    context_manager: &crate::context::ContextManager<T>,
) -> Option<usize> {
    (!context_manager.is_empty()).then(|| context_manager.current_route())
}

fn validate_committed_routes(
    source_routes: &[u64],
    committed_routes: Vec<u64>,
) -> Result<Vec<usize>, String> {
    if committed_routes.is_empty() {
        return Err("target committed no source routes".into());
    }
    let source_routes: HashSet<_> = source_routes.iter().copied().collect();
    let mut seen = HashSet::with_capacity(committed_routes.len());
    committed_routes
        .into_iter()
        .map(|route_id| {
            if !source_routes.contains(&route_id) {
                return Err(format!(
                    "target committed a route not included in the offer: {route_id}"
                ));
            }
            if !seen.insert(route_id) {
                return Err(format!("target committed route more than once: {route_id}"));
            }
            usize::try_from(route_id).map_err(|_| {
                format!("target committed an unrepresentable route: {route_id}")
            })
        })
        .collect()
}

#[cfg(test)]
mod outgoing_transfer_tests {
    use super::validate_committed_routes;

    #[test]
    fn committed_routes_must_be_offered_unique_and_nonempty() {
        assert_eq!(
            validate_committed_routes(&[11, 22], vec![22]).unwrap(),
            vec![22]
        );
        assert!(validate_committed_routes(&[11, 22], vec![]).is_err());
        assert!(validate_committed_routes(&[11, 22], vec![33]).is_err());
        assert!(validate_committed_routes(&[11, 22], vec![11, 11]).is_err());
    }

    #[test]
    fn committed_routes_reject_large_unknown_ids() {
        assert!(validate_committed_routes(&[11, 22], vec![u64::MAX]).is_err());
    }
}

#[cfg(test)]
mod pending_transfer_tests {
    use super::current_route_if_contexts_remain;
    use crate::context::ContextManager;
    use rio_backend::event::{VoidListener, WindowId};

    #[test]
    fn empty_transfer_target_has_no_route_to_redraw() {
        let mut context_manager =
            ContextManager::start_with_capacity(1, VoidListener {}, WindowId::from(0))
                .unwrap();
        context_manager
            .extract_grid(0)
            .expect("the bootstrap grid should be removable");

        assert_eq!(current_route_if_contexts_remain(&context_manager), None);
    }
}

#[cfg(test)]
mod merge_hover_tests {
    use super::{merge_target_index_is_valid, MergeHoverState};

    #[test]
    fn arm_starts_unhighlighted_and_click_requires_hover() {
        let mut hover = MergeHoverState::default();
        assert_eq!(hover.clicked_index(), None);

        hover.set(2);
        assert_eq!(hover.clicked_index(), Some(2));

        hover.clear();
        assert_eq!(hover.clicked_index(), None);
    }

    #[test]
    fn native_target_accepts_beginning_middle_end_and_rejects_stale() {
        for index in 0..=2 {
            assert!(merge_target_index_is_valid(index, 2, true));
        }
        assert!(!merge_target_index_is_valid(3, 2, true));
        assert!(!merge_target_index_is_valid(2, 2, false));
    }
}

struct RecoveryCandidate {
    descriptor: SessionDescriptor,
    label: String,
}

impl RecoveryCandidate {
    fn from_probe(
        descriptor: SessionDescriptor,
        prepared: crate::context::session::PreparedSession,
    ) -> Self {
        let label = Application::recovery_label(&prepared);
        // Drop the short-lived claim now; user interaction has no time limit.
        Self { descriptor, label }
    }
}

struct RecoveryProbeState {
    id: u64,
    source_window: rio_backend::event::WindowId,
    remaining: usize,
    candidates: Vec<RecoveryCandidate>,
}

#[cfg(all(feature = "wayland", target_os = "linux"))]
fn pointer_outside_surface(
    position: rio_window::dpi::PhysicalPosition<f64>,
    size: rio_window::dpi::PhysicalSize<u32>,
) -> bool {
    position.x < 0.0
        || position.y < 0.0
        || position.x >= f64::from(size.width)
        || position.y >= f64::from(size.height)
}

#[cfg(all(test, unix))]
mod recovery_tests {
    use super::*;

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    #[test]
    fn implicit_grab_motion_detects_exit_before_clamping() {
        use rio_window::dpi::{PhysicalPosition as P, PhysicalSize};
        let size = PhysicalSize::new(800, 490);
        assert!(!pointer_outside_surface(P::new(799.0, 20.0), size));
        assert!(pointer_outside_surface(P::new(900.0, 20.0), size));
        assert!(pointer_outside_surface(P::new(-1.0, 20.0), size));
        assert!(pointer_outside_surface(P::new(400.0, -1.0), size));
        assert!(pointer_outside_surface(P::new(400.0, 490.0), size));
    }

    #[test]
    fn transferred_gui_exits_only_after_windows_and_preparations_are_gone() {
        assert!(transfer_exit_ready(true, 0, 0, false));
        assert!(!transfer_exit_ready(false, 0, 0, false));
        assert!(!transfer_exit_ready(true, 1, 0, false));
        assert!(!transfer_exit_ready(true, 0, 1, false));
        assert!(!transfer_exit_ready(true, 0, 0, true));
    }

    #[test]
    #[ignore = "requires RIO_TEST_BINARY and a private XDG_RUNTIME_DIR"]
    fn recovery_candidate_outlives_probe_commit_deadline() {
        use rio_session::{SessionClient, SessionSpec};
        let binary = std::env::var_os("RIO_TEST_BINARY").expect("set RIO_TEST_BINARY");
        let owner = SessionClient::spawn_with_worker_path(
            SessionSpec {
                shell: Some("/bin/sh".into()),
                args: vec![
                    "-c".into(),
                    "while IFS= read -r line; do printf 'ACK:%s\\n' \"$line\"; done"
                        .into(),
                ],
                ..SessionSpec::default()
            },
            binary,
        )
        .unwrap();
        let pid = owner.child_pid().unwrap();
        let descriptor = owner.descriptor().clone();
        let prepared = crate::context::session::SessionHandle::prepare_attach(
            SessionClient::prepare_attach(descriptor.clone()).unwrap(),
        );
        let candidate = RecoveryCandidate::from_probe(descriptor, prepared);
        std::thread::sleep(std::time::Duration::from_secs(4));
        let recovered = SessionClient::prepare_attach(candidate.descriptor)
            .unwrap()
            .commit()
            .unwrap();
        let recovered_pid = recovered.child_pid().unwrap();
        recovered.write(b"after-picker-delay\n".to_vec()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let seen = loop {
            let frame = recovered.snapshot().unwrap();
            if frame
                .rows
                .iter()
                .any(|row| row.text.contains("ACK:after-picker-delay"))
            {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        recovered.close().unwrap();
        assert_eq!(pid, recovered_pid);
        assert!(seen, "fresh input must reach the original shell");
    }
}

struct RecoveryProbeResult {
    id: u64,
    source_window: rio_backend::event::WindowId,
    prepared: Result<crate::context::session::PreparedSession, String>,
    descriptor: Option<SessionDescriptor>,
}

impl<'a> Application<'a> {
    pub fn new<'app>(
        config: rio_backend::config::Config,
        config_error: Option<rio_backend::config::ConfigError>,
        event_loop: &EventLoop<EventPayload>,
        app_id: Option<String>,
    ) -> Application<'app> {
        // SAFETY: Since this takes a pointer to the winit event loop, it MUST be dropped first,
        // which is done in `exiting`.
        let clipboard =
            unsafe { Clipboard::new(event_loop.display_handle().unwrap().as_raw()) };

        let mut router = Router::new(config.fonts.to_owned(), clipboard);
        if let Some(error) = config_error {
            router.propagate_error_to_next_route(error.into());
        }

        let proxy = event_loop.create_proxy();
        let event_proxy = EventProxy::new(proxy.clone());
        let _ = configuration_file_updates(
            rio_backend::config::config_dir_path(),
            event_proxy.clone(),
        );
        let scheduler = Scheduler::new(proxy);
        let (incoming_prepared_sender, incoming_prepared) = mpsc::sync_channel(8);
        let (recovery_probe_sender, recovery_probe) = mpsc::sync_channel(8);
        event_loop.listen_device_events(DeviceEvents::Never);

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        event_loop.set_confirm_before_quit(config.confirm_before_quit);

        rio_notifier::request_authorization();

        Application {
            config,
            event_proxy,
            router,
            window_control: None,
            incoming_prepared_sender,
            incoming_prepared,
            pending_prepared: Vec::new(),
            pending_outgoing: Vec::new(),
            exit_after_transfer: false,
            exit_after_session_closes: false,
            session_close_deadline: None,
            pending_session_closes: Vec::new(),
            session_preparations: std::sync::Arc::new(()),
            ready_session_imports: Vec::new(),
            scheduler,
            app_id,
            global_hotkey: None,
            merge_window_source: None,
            merge_target: None,
            pending_merge_selection: None,
            armed_merge_selection: None,
            recovery_targets: None,
            recovery_probe_sender,
            recovery_probe,
            recovery_probe_state: None,
            next_recovery_probe_id: 1,
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            tab_drag: None,
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            foreign_drag: None,
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            retained_tab_drag_owner: None,
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            deferred_tab_drag_closes: Vec::new(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            tab_drag_detach_deadline: None,
            drop_marker_window: None,
            #[cfg(target_os = "macos")]
            quake_previous_app: None,
        }
    }

    fn skip_window_event(event: &WindowEvent) -> bool {
        matches!(
            event,
            WindowEvent::KeyboardInput {
                is_synthetic: true,
                ..
            } | WindowEvent::ActivationTokenDone { .. }
                | WindowEvent::DoubleTapGesture { .. }
                | WindowEvent::TouchpadPressure { .. }
                | WindowEvent::RotationGesture { .. }
                | WindowEvent::PinchGesture { .. }
                | WindowEvent::AxisMotion { .. }
                | WindowEvent::PanGesture { .. }
                | WindowEvent::HoveredFileCancelled
                | WindowEvent::Destroyed
                | WindowEvent::HoveredFile(_)
                | WindowEvent::Moved(_)
        )
    }

    fn handle_audio_bell(&mut self) {
        #[cfg(target_os = "macos")]
        {
            // Use system bell sound on macOS
            unsafe {
                #[link(name = "AppKit", kind = "framework")]
                extern "C" {
                    fn NSBeep();
                }
                NSBeep();
            }
        }

        #[cfg(target_os = "windows")]
        {
            // Use MessageBeep on Windows with MB_OK (0x00000000) for default beep
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(0x00000000);
            }
        }

        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        {
            #[cfg(feature = "audio")]
            {
                std::thread::spawn(|| {
                    if let Err(e) = play_bell_sound() {
                        tracing::warn!("Failed to play bell sound: {}", e);
                    }
                });
            }
            #[cfg(not(feature = "audio"))]
            {
                tracing::debug!("Audio bell requested but audio feature is not enabled");
            }
        }
    }

    fn handle_desktop_notification(&self, title: &str, body: &str) {
        rio_notifier::send_notification(title, body);
    }

    pub fn run(
        &mut self,
        event_loop: EventLoop<EventPayload>,
    ) -> Result<(), Box<dyn Error>> {
        let result = event_loop.run_app(self);
        result.map_err(Into::into)
    }
}

impl<'a> Application<'a> {
    /// Register a system-wide hotkey for every `ToggleQuake` binding
    /// in the config, so the quake window opens while Rio is
    /// unfocused. No-op when quake is not bound; pure Wayland has no
    /// global hotkey API, the compositor keybinding + a regular
    /// binding cover it there.
    fn setup_quake_hotkey(&mut self) {
        // Drop any previous manager first: registering a chord the old
        // manager still holds fails on Windows and X11.
        self.global_hotkey = None;
        self.global_hotkey = crate::global_hotkey::setup(
            self.event_proxy.clone(),
            &self.config.bindings.keys,
        );
    }

    /// The monitor the quake window should drop down on: the one
    /// under the mouse cursor where the platform can tell us, the
    /// primary monitor otherwise.
    fn quake_monitor(
        &self,
        event_loop: &ActiveEventLoop,
    ) -> Option<rio_window::monitor::MonitorHandle> {
        event_loop
            .cursor_monitor()
            .or_else(|| event_loop.primary_monitor())
    }

    /// Anchor the quake window to the top of `monitor`, horizontally
    /// centered, sized by the configured percentages, then show it.
    fn show_quake_window(
        &mut self,
        id: rio_backend::event::WindowId,
        event_loop: &ActiveEventLoop,
    ) {
        #[cfg(target_os = "macos")]
        {
            self.quake_previous_app =
                rio_window::platform::macos::frontmost_application_pid();
        }

        let Some(route) = self.router.routes.get(&id) else {
            return;
        };
        let window = &route.window.winit_window;
        if let Some(monitor) = self.quake_monitor(event_loop) {
            let msize = monitor.size();
            let mpos = monitor.position();
            let width = (msize.width as f32
                * self.config.window.quake_width_percentage.clamp(0.1, 1.0))
                as u32;
            let height = (msize.height as f32
                * self.config.window.quake_height_percentage.clamp(0.1, 1.0))
                as u32;
            let x = mpos.x + (msize.width.saturating_sub(width) / 2) as i32;
            #[cfg(target_os = "macos")]
            {
                let scale = monitor.scale_factor();
                let size: rio_window::dpi::LogicalSize<f64> =
                    rio_window::dpi::PhysicalSize::new(width, height).to_logical(scale);
                let pos: rio_window::dpi::LogicalPosition<f64> =
                    rio_window::dpi::PhysicalPosition::new(x, mpos.y).to_logical(scale);
                let _ = window.request_inner_size(size);
                window.set_outer_position(pos);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = window.request_inner_size(rio_window::dpi::PhysicalSize::new(
                    width, height,
                ));
                window.set_outer_position(rio_window::dpi::PhysicalPosition::new(
                    x, mpos.y,
                ));
            }
        }
        window.set_visible(true);
        window.focus_window();
    }

    /// Show, focus or hide the quake window; create it on first use.
    fn toggle_quake_window(&mut self, event_loop: &ActiveEventLoop) {
        let quake_id = self
            .router
            .quake_window_id
            .filter(|id| self.router.routes.contains_key(id));

        let Some(id) = quake_id else {
            self.router.quake_window_id = None;
            self.router.create_quake_window(
                event_loop,
                self.event_proxy.clone(),
                &self.config,
            );
            if let Some(id) = self.router.quake_window_id {
                self.show_quake_window(id, event_loop);
            }
            return;
        };

        if let Some(route) = self.router.routes.get_mut(&id) {
            let window = &route.window.winit_window;
            let visible = window.is_visible().unwrap_or(true);
            if !visible {
                self.show_quake_window(id, event_loop);
            } else if window.has_focus() {
                window.set_visible(false);
                #[cfg(target_os = "macos")]
                if let Some(pid) = self.quake_previous_app.take() {
                    rio_window::platform::macos::activate_application(pid);
                }
            } else {
                self.show_quake_window(id, event_loop);
            }
        }
    }

    fn close_terminal_at(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: rio_backend::event::WindowId,
        route_id: usize,
    ) {
        let mut remove_window = false;
        let mut closing_sessions = Vec::new();
        if let Some(route) = self.router.routes.get_mut(&window_id) {
            closing_sessions = route.window.screen.context_manager.session_handles();
            route.window.screen.discard_routes([route_id]);
            remove_window = route
                .window
                .screen
                .context_manager
                .should_close_context_manager(
                    route_id,
                    &mut route.window.screen.sugarloaf,
                );
            if !remove_window {
                let size = route.window.winit_window.inner_size();
                route.window.screen.refresh_after_tab_transfer(size);
                route.request_redraw();
            }
        }
        self.scheduler.unschedule_window(route_id);
        if remove_window {
            self.pending_session_closes.extend(closing_sessions);
            self.clear_merge_if_window(window_id);
            self.router.remove_route(window_id);
            if self.router.routes.is_empty() {
                self.defer_exit_until_session_closes();
            }
        }
    }

    fn defer_exit_until_session_closes(&mut self) {
        self.exit_after_session_closes = true;
        self.session_close_deadline = Some(Instant::now() + Duration::from_secs(5));
    }

    fn poll_deferred_session_exit(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if !self.exit_after_session_closes {
            return false;
        }

        self.pending_session_closes
            .retain(|session| !session.pump_done());
        if self.pending_session_closes.is_empty() {
            self.exit_after_session_closes = false;
            self.session_close_deadline = None;
            event_loop.exit();
            return true;
        }

        if self
            .session_close_deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            tracing::warn!(
                pending = self.pending_session_closes.len(),
                "session workers did not acknowledge GUI close before exit deadline"
            );
            self.exit_after_session_closes = false;
            self.session_close_deadline = None;
            event_loop.exit();
            return true;
        }

        true
    }

    fn restore_route_transfer(
        route: &mut crate::router::Route<'_>,
        index: usize,
        transfer: crate::screen::ScreenTransfer,
    ) -> Result<(), crate::screen::ScreenTransfer> {
        let size = route.window.winit_window.inner_size();
        route.window.screen.insert_transfer(index, transfer, size)?;
        route.request_redraw();
        Ok(())
    }

    fn route_from_transfer(
        &self,
        window: rio_window::window::Window,
        transfer: crate::screen::ScreenTransfer,
    ) -> Result<crate::router::Route<'a>, crate::screen::ScreenTransferFailure> {
        let window = crate::router::RouteWindow::from_transfer(
            window,
            self.event_proxy.clone(),
            &self.config,
            &self.router.font_library,
            transfer,
        )?;
        Ok(crate::router::Route::new(RoutePath::Terminal, window))
    }

    fn track_outgoing_offer(
        &mut self,
        transfer_id: [u8; 16],
        source_window: rio_backend::event::WindowId,
        source_routes: Vec<u64>,
    ) {
        self.pending_outgoing.push(PendingOutgoing {
            transfer_id,
            source_window,
            source_routes,
        });
    }

    fn move_tab_to_new_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        source_id: rio_backend::event::WindowId,
        tab_id: crate::layout::TabId,
    ) -> Option<rio_backend::event::WindowId> {
        let Some(original_index) = self
            .router
            .routes
            .get(&source_id)
            .and_then(|route| route.window.screen.context_manager.tab_index(tab_id))
        else {
            tracing::warn!(?tab_id, "tab disappeared before detaching");
            return None;
        };

        if let Some(control) = self.window_control.as_ref() {
            let transfer_id = match crate::router::window_control::new_transfer_id() {
                Ok(id) => id,
                Err(error) => {
                    tracing::warn!(%error, "could not allocate cross-window transfer id");
                    return None;
                }
            };
            let offer = match self.router.routes.get(&source_id).and_then(|route| {
                route
                    .window
                    .screen
                    .context_manager
                    .transfer_offer(original_index, transfer_id)
                    .ok()
            }) {
                Some(offer) => offer,
                None => {
                    tracing::warn!("session is not ready for cross-window transfer");
                    return None;
                }
            };
            let source_routes = offer.pane_route_ids();
            if let Err(error) =
                control.launch_and_offer_async(offer, self.event_proxy.clone(), source_id)
            {
                tracing::warn!(%error, "could not launch target Rio window");
                return None;
            }
            self.track_outgoing_offer(transfer_id, source_id, source_routes);
            return None;
        }

        let bare = match crate::router::RouteWindow::create(
            event_loop,
            &self.config,
            "Rio",
            None,
            self.app_id.as_deref(),
        ) {
            Ok(window) => window,
            Err(error) => {
                tracing::warn!(%error, "could not create a window for the current tab");
                return None;
            }
        };

        let (original_index, transfer, source_empty) = {
            let source = self.router.routes.get_mut(&source_id)?;
            let Some(transfer) = source.window.screen.extract_transfer(original_index)
            else {
                tracing::error!("current tab disappeared before extraction");
                return None;
            };
            let source_empty = source.window.screen.context_manager.is_empty();
            if !source_empty {
                source
                    .window
                    .screen
                    .refresh_after_tab_transfer(source.window.winit_window.inner_size());
                source.request_redraw();
            }
            (original_index, transfer, source_empty)
        };
        let route = match self.route_from_transfer(bare, transfer) {
            Ok(route) => route,
            Err(failure) => {
                tracing::warn!(error = %failure, "could not map a new window for the current tab");
                let source = self
                    .router
                    .routes
                    .get_mut(&source_id)
                    .expect("tab transfer source disappeared during synchronous window construction");
                Self::restore_route_transfer(source, original_index, failure.transfer)
                    .unwrap_or_else(|_| {
                        panic!("tab transfer source rejected its synchronously extracted tab")
                    });
                return None;
            }
        };
        let destination_id = self.router.install_normal_route(route);

        if source_empty {
            self.clear_merge_if_window(source_id);
            self.router.remove_route(source_id);
        }
        if let Some(destination) = self.router.routes.get_mut(&destination_id) {
            destination.window.winit_window.focus_window();
            destination.request_redraw();
        }
        Some(destination_id)
    }

    fn perform_close_requested(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: rio_backend::event::WindowId,
    ) {
        if self.config.confirm_before_quit
            && !cfg!(any(target_os = "macos", target_os = "windows"))
        {
            if let Some(route) = self.router.routes.get_mut(&window_id) {
                route.confirm_quit();
            }
            return;
        }
        let closing_sessions = if let Some(route) = self.router.routes.get(&window_id) {
            for route_id in route.window.screen.context_manager.route_ids() {
                self.scheduler.unschedule_window(route_id);
            }
            route.window.screen.context_manager.session_handles()
        } else {
            Vec::new()
        };
        self.clear_merge_if_window(window_id);
        self.pending_session_closes.extend(closing_sessions);
        self.router.remove_route(window_id);
        if self.router.routes.is_empty() {
            self.defer_exit_until_session_closes();
        }
    }
}

#[cfg(all(feature = "wayland", target_os = "linux"))]
impl<'a> Application<'a> {
    fn start_external_drag(
        &mut self,
        event_loop: &ActiveEventLoop,
        source_id: rio_backend::event::WindowId,
    ) -> bool {
        use rio_window::platform::wayland::WindowExtWayland;

        if self.tab_drag.is_some() {
            return false;
        }
        let Some(route) = self.router.routes.get_mut(&source_id) else {
            return false;
        };
        if route.path != RoutePath::Terminal
            || !route.window.screen.mouse.left_button_state.is_pressed()
        {
            return false;
        }
        let supports_toplevel_drag = route.window.winit_window.supports_toplevel_drag();

        let scale = route.window.screen.sugarloaf.scale_factor();
        route
            .window
            .screen
            .handle_tab_drag_move(route.window.screen.mouse.x as f32 / scale, true);
        let Some(handoff) = route
            .window
            .screen
            .renderer
            .island
            .as_mut()
            .and_then(crate::renderer::island::Island::take_external_drag_handoff)
        else {
            return false;
        };

        let whole_window = matches!(
            &handoff,
            crate::renderer::island::ExternalDragHandoff::Window { .. }
        );
        let tab_id = match &handoff {
            crate::renderer::island::ExternalDragHandoff::Tab { tab_id, .. } => {
                Some(*tab_id)
            }
            crate::renderer::island::ExternalDragHandoff::Window { .. } => None,
        };
        if !supports_toplevel_drag {
            if let Some(island) = route.window.screen.renderer.island.as_mut() {
                island.cancel_drag();
            }
            if let Some(tab_id) = tab_id {
                self.detach_tab_from_window(event_loop, source_id, tab_id);
                return true;
            }
            return false;
        }
        let tab_count = route.window.screen.context_manager.len();
        let (source_index, transition) = match handoff {
            crate::renderer::island::ExternalDragHandoff::Tab { tab_id, .. } => {
                let Some(source_index) =
                    route.window.screen.context_manager.tab_index(tab_id)
                else {
                    tracing::warn!("dragged tab disappeared before external handoff");
                    if let Some(island) = route.window.screen.renderer.island.as_mut() {
                        island.cancel_drag();
                    }
                    return false;
                };
                (
                    source_index,
                    TabDrag::begin(
                        route.window.winit_window.id(),
                        tab_id,
                        source_index,
                        tab_count,
                    ),
                )
            }
            crate::renderer::island::ExternalDragHandoff::Window { .. } => {
                let source_index = route.window.screen.context_manager.current_index();
                let Some(tab_id) = route
                    .window
                    .screen
                    .context_manager
                    .current_grid_opt()
                    .map(|grid| grid.id())
                else {
                    if let Some(island) = route.window.screen.renderer.island.as_mut() {
                        island.cancel_drag();
                    }
                    return false;
                };
                (
                    source_index,
                    TabDrag::begin_window(
                        route.window.winit_window.id(),
                        tab_id,
                        tab_count,
                        source_index,
                    ),
                )
            }
        };

        match transition {
            Ok(transition) => {
                route.window.screen.set_transfer_source_marker(source_index);
                if whole_window {
                    route.window.screen.set_window_overlay(Some(
                        crate::renderer::WindowOverlay::MergeSource,
                    ));
                }
                if let Some(island) = route.window.screen.renderer.island.as_mut() {
                    island.cancel_drag();
                }
                self.run_tab_drag_transition(event_loop, transition);
                if self
                    .tab_drag
                    .as_ref()
                    .is_some_and(|drag| !drag.whole_window)
                {
                    self.schedule_tab_detach();
                }
                true
            }
            Err(error) => {
                tracing::debug!(%error, "could not start external tab drag");
                if let Some(island) = route.window.screen.renderer.island.as_mut() {
                    island.cancel_drag();
                }
                if let Some(tab_id) = tab_id {
                    self.detach_tab_from_window(event_loop, source_id, tab_id);
                    return true;
                }
                false
            }
        }
    }

    fn detach_tab_from_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        source_window: rio_backend::event::WindowId,
        tab_id: crate::layout::TabId,
    ) {
        use rio_window::platform::wayland::WindowExtWayland;

        let Some(destination_id) =
            self.move_tab_to_new_window(event_loop, source_window, tab_id)
        else {
            return;
        };
        let Some(route) = self.router.routes.get(&destination_id) else {
            return;
        };
        let result = if route.window.winit_window.supports_toplevel_drag() {
            route
                .window
                .winit_window
                .drag_window_from_active_grab(u64::from(source_window))
        } else {
            route.window.winit_window.drag_window()
        };
        if let Err(error) = result {
            tracing::debug!(%error, "could not continue detached tab drag");
        }
    }

    fn run_tab_drag_transition(
        &mut self,
        event_loop: &ActiveEventLoop,
        transition: TabDragTransition,
    ) {
        assert!(
            self.tab_drag.is_none(),
            "tab drag reducer state must be taken first"
        );
        let TabDragTransition {
            mut state,
            commands,
        } = transition;
        let mut commands: VecDeque<_> = commands.into();
        while let Some(command) = commands.pop_front() {
            let immediate = self.execute_tab_drag_command(&state, command);
            let Some(event) = immediate else {
                continue;
            };
            let transition = state.reduce(event);
            state = transition.state;
            commands.extend(transition.commands);
        }
        self.tab_drag = Some(state);
        self.finish_terminal_tab_drag(event_loop);
    }

    fn dispatch_tab_drag_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: TabDragEvent,
    ) {
        let Some(state) = self.tab_drag.take() else {
            return;
        };
        self.run_tab_drag_transition(event_loop, state.reduce(event));
    }

    fn execute_tab_drag_command(
        &mut self,
        state: &TabDrag,
        command: TabDragCommand,
    ) -> Option<TabDragEvent> {
        use rio_window::platform::wayland::WindowExtWayland;

        match command {
            TabDragCommand::PrepareSource {
                payload,
                frame_grab,
            } => {
                let source: rio_backend::event::WindowId = state.source_window.into();
                let Some(route) = self.router.routes.get(&source) else {
                    return Some(TabDragEvent::PrepareFailed);
                };
                let result = route
                    .window
                    .winit_window
                    .prepare_toplevel_drag(payload, frame_grab);
                Some(match result {
                    Ok(drag_id) => TabDragEvent::Prepared(drag_id),
                    Err(error) => {
                        tracing::debug!(%error, "could not prepare Wayland tab drag");
                        TabDragEvent::PrepareFailed
                    }
                })
            }
            TabDragCommand::StartSourceOwner => {
                let drag_id = state.drag_id.expect("started owner requires drag");
                let source: rio_backend::event::WindowId = state.source_window.into();
                let Some(route) = self.router.routes.get(&source) else {
                    return Some(TabDragEvent::OwnerStartFailed(drag_id));
                };
                let owner = OwnerRoute {
                    window_id: state.source_window,
                    tab_id: state.tab_id,
                    route_ids: if state.whole_window {
                        route.window.screen.context_manager.route_ids()
                    } else {
                        route
                            .window
                            .screen
                            .context_manager
                            .current_grid()
                            .route_ids()
                    },
                };
                let mut source_routes = None;
                if let Some(control) = self.window_control.as_ref() {
                    let offer = if state.whole_window {
                        route
                            .window
                            .screen
                            .context_manager
                            .transfer_window_offer(state.token.as_bytes())
                    } else {
                        route
                            .window
                            .screen
                            .context_manager
                            .transfer_offer(state.original_index, state.token.as_bytes())
                    };
                    let offer = match offer {
                        Ok(offer) => offer,
                        Err(error) => {
                            tracing::warn!(%error, "could not publish foreign tab drag offer");
                            return Some(TabDragEvent::OwnerStartFailed(drag_id));
                        }
                    };
                    let offer_routes = offer.pane_route_ids();
                    if let Err(error) = control.publish_drag_offer(offer) {
                        tracing::warn!(%error, "could not publish foreign tab drag offer");
                        return Some(TabDragEvent::OwnerStartFailed(drag_id));
                    }
                    source_routes = Some(offer_routes);
                }
                let result = route.window.winit_window.start_toplevel_drag(drag_id);
                Some(match result {
                    Ok(()) => {
                        if let Some(source_routes) = source_routes {
                            self.track_outgoing_offer(
                                state.token.as_bytes(),
                                source,
                                source_routes,
                            );
                        }
                        TabDragEvent::OwnerStarted { drag_id, owner }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not start source tab drag owner");
                        TabDragEvent::OwnerStartFailed(drag_id)
                    }
                })
            }
            TabDragCommand::AcceptOffer => {
                let hover = state.hover.expect("accepted offer requires hover");
                let target = hover.target_window.into();
                let Some(route) = self.router.routes.get(&target) else {
                    return Some(TabDragEvent::TargetRejected);
                };
                let result = route
                    .window
                    .winit_window
                    .accept_toplevel_drag_offer(hover.offer_id);
                if let Err(error) = result {
                    tracing::debug!(%error, "could not accept tab drag offer");
                    return Some(TabDragEvent::TargetRejected);
                }
                None
            }
            TabDragCommand::RejectOffer {
                offer_id,
                target_window,
            } => {
                self.reject_tab_drag_offer(offer_id, target_window);
                None
            }
            TabDragCommand::ReceiveData => {
                let hover = state.hover.expect("received offer requires hover");
                let target = hover.target_window.into();
                let Some(route) = self.router.routes.get(&target) else {
                    return Some(TabDragEvent::DataFailed(hover.offer_id));
                };
                let result = route
                    .window
                    .winit_window
                    .receive_toplevel_drag_offer(hover.offer_id);
                let Err(error) = result else {
                    return None;
                };
                tracing::debug!(%error, "could not receive tab drag data");
                if let Err(cancel_error) = route
                    .window
                    .winit_window
                    .cancel_toplevel_drag_offer(hover.offer_id)
                {
                    tracing::debug!(%cancel_error, "could not destroy failed tab drag offer");
                }
                Some(TabDragEvent::DataFailed(hover.offer_id))
            }
            TabDragCommand::MoveOwnerToTarget => {
                let owner = state.owner.as_ref().expect("target move requires owner");
                let hover = state.hover.expect("target move requires hover");
                if let Some(control) = self.window_control.as_ref() {
                    let target: rio_backend::event::WindowId = hover.target_window.into();
                    if !self.router.routes.contains_key(&target) {
                        return Some(
                            match control.take_drag_offer_async(
                                state.token.as_bytes(),
                                u64::from(target),
                                hover.index,
                                self.event_proxy.clone(),
                                target,
                            ) {
                                Ok(()) => TabDragEvent::TargetCommitted,
                                Err(error) => {
                                    tracing::debug!(%error, "could not request foreign drag offer");
                                    TabDragEvent::TargetRejected
                                }
                            },
                        );
                    }
                }
                Some(
                    if if state.whole_window {
                        self.move_window_owner_to_target(
                            owner,
                            hover.target_window,
                            hover.index,
                        )
                    } else {
                        self.move_owner_to_target(owner, hover.target_window, hover.index)
                    } {
                        TabDragEvent::TargetCommitted
                    } else {
                        TabDragEvent::TargetRejected
                    },
                )
            }
            TabDragCommand::FinishOffer => {
                let hover = state.hover.expect("finished offer requires hover");
                let target = hover.target_window.into();
                if let Some(route) = self.router.routes.get(&target) {
                    if let Err(error) = route
                        .window
                        .winit_window
                        .finish_toplevel_drag_offer(hover.offer_id)
                    {
                        tracing::warn!(%error, "could not finish committed tab drag offer");
                        if let Err(cancel_error) = route
                            .window
                            .winit_window
                            .cancel_toplevel_drag_offer(hover.offer_id)
                        {
                            tracing::debug!(%cancel_error, "could not destroy failed tab drag offer");
                        }
                        return Some(TabDragEvent::OfferCancelled(hover.offer_id));
                    }
                } else {
                    tracing::error!(
                        "committed tab drag target disappeared before offer finish"
                    );
                    return Some(TabDragEvent::OfferCancelled(hover.offer_id));
                }
                None
            }
            TabDragCommand::CancelOffer => {
                if let Some(hover) = state.hover {
                    if let Some(route) =
                        self.router.routes.get(&hover.target_window.into())
                    {
                        if let Err(error) = route
                            .window
                            .winit_window
                            .cancel_toplevel_drag_offer(hover.offer_id)
                        {
                            tracing::debug!(%error, "could not cancel tab drag offer");
                        }
                    }
                    return Some(TabDragEvent::OfferCancelled(hover.offer_id));
                }
                None
            }
            TabDragCommand::CancelSource => {
                let drag_id = state.drag_id.expect("cancelled source requires drag");
                let source: rio_backend::event::WindowId = state.source_window.into();
                if let Some(route) = self.router.routes.get(&source) {
                    if let Err(error) =
                        route.window.winit_window.cancel_toplevel_drag(drag_id)
                    {
                        tracing::debug!(%error, "could not cancel tab drag source");
                    }
                }
                None
            }
        }
    }

    fn reject_tab_drag_offer(
        &self,
        offer_id: rio_window::platform::wayland::ToplevelDragOfferId,
        target_window: WindowId,
    ) {
        use rio_window::platform::wayland::WindowExtWayland;

        if let Some(route) = self.router.routes.get(&target_window.into()) {
            if let Err(error) = route
                .window
                .winit_window
                .reject_toplevel_drag_offer(offer_id)
            {
                tracing::debug!(%error, "could not reject tab drag offer");
            }
        }
    }

    fn cancel_tab_drag_offer(
        &self,
        offer_id: rio_window::platform::wayland::ToplevelDragOfferId,
        target_window: WindowId,
    ) {
        use rio_window::platform::wayland::WindowExtWayland;

        if let Some(route) = self.router.routes.get(&target_window.into()) {
            if let Err(error) = route
                .window
                .winit_window
                .cancel_toplevel_drag_offer(offer_id)
            {
                tracing::debug!(%error, "could not cancel tab drag offer");
            }
        }
    }

    fn retain_tab_drag_owner(&mut self, owner_id: rio_backend::event::WindowId) {
        assert!(
            self.retained_tab_drag_owner.is_none(),
            "only one tab drag owner may await protocol completion"
        );
        self.clear_merge_if_window(owner_id);
        self.retained_tab_drag_owner = Some(
            self.router
                .remove_route(owner_id)
                .expect("committed drag owner must remain registered"),
        );
    }

    fn move_owner_to_target(
        &mut self,
        owner: &OwnerRoute,
        target_window: WindowId,
        index: usize,
    ) -> bool {
        if owner.window_id == target_window {
            tracing::error!("source tab drag owner was selected as its own target");
            return false;
        }
        let owner_id: rio_backend::event::WindowId = owner.window_id.into();
        let target_id: rio_backend::event::WindowId = target_window.into();
        let [Some(owner_route), Some(target_route)] =
            self.router.routes.get_disjoint_mut([&owner_id, &target_id])
        else {
            return false;
        };
        let moved = {
            let Some(owner_index) = owner_route
                .window
                .screen
                .context_manager
                .tab_index(owner.tab_id)
            else {
                return false;
            };
            let transfer = owner_route
                .window
                .screen
                .extract_transfer(owner_index)
                .expect("validated owner tab disappeared");
            if transfer.route_ids() != owner.route_ids {
                Self::restore_route_transfer(owner_route, owner_index, transfer)
                    .unwrap_or_else(|_| panic!("owner rejected its own tab transfer"));
                return false;
            }
            match Self::restore_route_transfer(target_route, index, transfer) {
                Ok(()) => true,
                Err(transfer) => {
                    Self::restore_route_transfer(owner_route, owner_index, transfer)
                        .unwrap_or_else(|_| {
                            panic!("owner changed during synchronous target insertion")
                        });
                    false
                }
            }
        };
        if moved {
            if let Some(source) = self.router.routes.get_mut(&owner_id) {
                source.window.screen.clear_transfer_source_marker();
                if !source.window.screen.context_manager.is_empty() {
                    let size = source.window.winit_window.inner_size();
                    source.window.screen.refresh_after_tab_transfer(size);
                    source.request_redraw();
                }
            }
            let source_empty = self
                .router
                .routes
                .get(&owner_id)
                .is_some_and(|route| route.window.screen.context_manager.is_empty());
            if source_empty {
                self.retain_tab_drag_owner(owner_id);
            }
        }
        moved
    }

    fn move_window_owner_to_target(
        &mut self,
        owner: &OwnerRoute,
        target_window: WindowId,
        index: usize,
    ) -> bool {
        if owner.window_id == target_window {
            return false;
        }
        let owner_id: rio_backend::event::WindowId = owner.window_id.into();
        let target_id: rio_backend::event::WindowId = target_window.into();
        if !self.router.routes.get(&owner_id).is_some_and(|route| {
            route.path == RoutePath::Terminal
                && owner.route_ids == route.window.screen.context_manager.route_ids()
        }) {
            return false;
        }
        let moved = self.transfer_window_to_target(owner_id, target_id, index);
        if moved {
            self.retain_tab_drag_owner(owner_id);
        }
        moved
    }

    fn tab_drag_target_index(
        &self,
        target_window: WindowId,
        position: rio_window::dpi::LogicalPosition<f64>,
        whole_window: bool,
        tab_count: usize,
    ) -> Option<usize> {
        let target_id: rio_backend::event::WindowId = target_window.into();
        self.router
            .routes
            .get(&target_id)
            .filter(|route| route.path == RoutePath::Terminal)
            .and_then(|route| {
                if whole_window {
                    let index = route.window.screen.context_manager.len();
                    return route
                        .window
                        .screen
                        .context_manager
                        .can_insert_grids(index, tab_count)
                        .then_some(index);
                }

                let scale = route.window.screen.sugarloaf.scale_factor() as f64;
                route
                    .window
                    .screen
                    .tab_drop_index(position.x * scale, position.y * scale)
                    .filter(|index| {
                        route.window.screen.context_manager.can_insert_grid(*index)
                    })
            })
    }

    fn handle_toplevel_drag_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: rio_window::platform::wayland::ToplevelDragEvent,
    ) {
        use rio_window::platform::wayland::ToplevelDragEvent;

        if let ToplevelDragEvent::FrameDrag {
            seat_id,
            pointer_id,
            ..
        } = event
        {
            if self.tab_drag.is_some() {
                if let Some(route) = self.router.routes.get(&(window_id.into())) {
                    route
                        .window
                        .winit_window
                        .forget_frame_drag_for_pointer(seat_id, pointer_id);
                }
                return;
            }
            let source_window: rio_backend::event::WindowId = window_id.into();
            let Some(route) = self.router.routes.get(&source_window) else {
                return;
            };
            if route.path != RoutePath::Terminal {
                let _ = route.window.winit_window.drag_window_from_frame_grab(
                    u64::from(source_window),
                    seat_id,
                    pointer_id,
                );
                route
                    .window
                    .winit_window
                    .forget_frame_drag_for_pointer(seat_id, pointer_id);
                return;
            }
            let (tab_id, tab_count, original_index) = {
                let screen = &route.window.screen;
                let Some(grid) = screen.context_manager.current_grid_opt() else {
                    let _ = route.window.winit_window.drag_window_from_frame_grab(
                        u64::from(source_window),
                        seat_id,
                        pointer_id,
                    );
                    route
                        .window
                        .winit_window
                        .forget_frame_drag_for_pointer(seat_id, pointer_id);
                    return;
                };
                (
                    grid.id(),
                    screen.context_manager.len(),
                    screen.context_manager.current_index(),
                )
            };
            let transition = match TabDrag::begin_window_from_frame(
                route.window.winit_window.id(),
                tab_id,
                tab_count,
                original_index,
                seat_id,
                pointer_id,
            ) {
                Ok(transition) => transition,
                Err(error) => {
                    tracing::debug!(%error, "could not start whole-window drag");
                    let _ = route.window.winit_window.drag_window_from_frame_grab(
                        u64::from(source_window),
                        seat_id,
                        pointer_id,
                    );
                    route
                        .window
                        .winit_window
                        .forget_frame_drag_for_pointer(seat_id, pointer_id);
                    return;
                }
            };
            self.run_tab_drag_transition(event_loop, transition);
            if self.tab_drag.is_some() {
                if let Some(route) = self.router.routes.get_mut(&source_window) {
                    route
                        .window
                        .screen
                        .set_transfer_source_marker(original_index);
                    route.window.screen.set_window_overlay(Some(
                        crate::renderer::WindowOverlay::MergeSource,
                    ));
                    route.request_redraw();
                }
            }
            return;
        }

        let target_window = window_id;
        if self.tab_drag.is_none() {
            self.handle_foreign_drag(target_window, event);
            return;
        }
        let restore_marker = matches!(
            &event,
            ToplevelDragEvent::SourceActionsChanged {
                move_supported: true,
                ..
            } | ToplevelDragEvent::SelectedActionChanged {
                selected_move: true,
                ..
            }
        );
        let event = match event {
            ToplevelDragEvent::FrameDrag { .. } => {
                unreachable!("frame drags are handled above")
            }
            ToplevelDragEvent::Entered { offer_id, position }
            | ToplevelDragEvent::Motion { offer_id, position } => {
                let valid_target = self
                    .tab_drag
                    .as_ref()
                    .is_some_and(|drag| is_valid_tab_drag_target(drag, target_window));
                let whole_window =
                    self.tab_drag.as_ref().is_some_and(|drag| drag.whole_window);
                let tab_count = self.tab_drag.as_ref().map_or(0, |drag| drag.tab_count);
                let index = valid_target
                    .then(|| {
                        self.tab_drag_target_index(
                            target_window,
                            position,
                            whole_window,
                            tab_count,
                        )
                    })
                    .flatten();
                let target_id: rio_backend::event::WindowId = target_window.into();
                let Some(index) = index else {
                    self.clear_drop_marker();
                    if self.tab_drag.as_ref().is_some_and(|drag| {
                        drag.hover.is_some_and(|hover| hover.offer_id == offer_id)
                    }) {
                        self.dispatch_tab_drag_event(
                            event_loop,
                            TabDragEvent::TargetRejected,
                        );
                        if self.tab_drag.as_ref().is_some_and(|drag| {
                            !drag.whole_window && drag.hover.is_none()
                        }) {
                            self.schedule_tab_detach();
                        }
                    } else {
                        self.reject_tab_drag_offer(offer_id, target_window);
                    }
                    return;
                };
                self.tab_drag_detach_deadline = None;
                let entering = self.tab_drag.as_ref().is_none_or(|drag| {
                    drag.hover.is_none_or(|hover| hover.offer_id != offer_id)
                });
                self.dispatch_tab_drag_event(
                    event_loop,
                    if entering {
                        TabDragEvent::Enter {
                            offer_id,
                            target_window,
                            index,
                        }
                    } else {
                        TabDragEvent::Motion { offer_id, index }
                    },
                );
                if self.tab_drag.as_ref().is_some_and(|drag| {
                    drag.hover.is_some_and(|hover| {
                        hover.offer_id == offer_id && hover.target_window == target_window
                    })
                }) {
                    self.set_drop_marker(target_id, index);
                }
                return;
            }
            ToplevelDragEvent::Left { offer_id } => {
                let active_offer = self.tab_drag_owns_offer(offer_id);
                if active_offer {
                    self.clear_drop_marker();
                }
                if active_offer
                    && self
                        .tab_drag
                        .as_ref()
                        .is_some_and(|drag| !drag.whole_window)
                {
                    self.schedule_tab_detach();
                }
                TabDragEvent::Leave(offer_id)
            }
            ToplevelDragEvent::Dropped { offer_id } => {
                if self.tab_drag_owns_offer(offer_id) {
                    self.clear_drop_marker();
                    TabDragEvent::Drop(offer_id)
                } else {
                    self.cancel_tab_drag_offer(offer_id, target_window);
                    return;
                }
            }
            ToplevelDragEvent::SourceActionsChanged {
                offer_id,
                move_supported,
            } => {
                if !move_supported && self.tab_drag_owns_offer(offer_id) {
                    self.clear_drop_marker();
                }
                TabDragEvent::SourceActionsChanged {
                    offer_id,
                    move_supported,
                }
            }
            ToplevelDragEvent::SelectedActionChanged {
                offer_id,
                selected_move,
            } => {
                if !selected_move && self.tab_drag_owns_offer(offer_id) {
                    self.clear_drop_marker();
                }
                TabDragEvent::SelectedActionChanged {
                    offer_id,
                    selected_move,
                }
            }
            ToplevelDragEvent::DataReady { offer_id, data } => {
                TabDragEvent::DataReady { offer_id, data }
            }
            ToplevelDragEvent::OfferDataFailed { offer_id } => {
                TabDragEvent::DataFailed(offer_id)
            }
            ToplevelDragEvent::OfferCancelled { offer_id } => {
                if self.tab_drag_owns_offer(offer_id) {
                    self.clear_drop_marker();
                }
                TabDragEvent::OfferCancelled(offer_id)
            }
            ToplevelDragEvent::Finished { drag_id } => {
                if self.tab_drag.as_ref().is_some_and(|drag| {
                    drag.drag_id == Some(drag_id) && drag.hover.is_none()
                }) {
                    // Foreign completion has no local hover. Wait for the
                    // authenticated commit response before removing source routes.
                    self.dispatch_tab_drag_event(
                        event_loop,
                        TabDragEvent::ForeignFinished(drag_id),
                    );
                    return;
                }
                TabDragEvent::SourceFinished(drag_id)
            }
            ToplevelDragEvent::Cancelled { drag_id } => {
                if self
                    .tab_drag
                    .as_ref()
                    .is_some_and(|drag| drag.drag_id == Some(drag_id))
                {
                    self.clear_drop_marker();
                }
                TabDragEvent::SourceCancelled(drag_id)
            }
        };
        self.dispatch_tab_drag_event(event_loop, event);
        if restore_marker {
            if let Some((target_window, index)) =
                self.tab_drag.as_ref().and_then(|drag| {
                    drag.hover
                        .filter(|hover| !hover.dropped)
                        .map(|hover| (hover.target_window, hover.index))
                })
            {
                self.set_drop_marker(target_window.into(), index);
            }
        }
    }

    fn finish_terminal_tab_drag(&mut self, event_loop: &ActiveEventLoop) {
        let Some(drag) = self.tab_drag.as_ref() else {
            return;
        };
        if !matches!(
            drag.lifecycle,
            TabDragLifecycle::Complete(_) | TabDragLifecycle::Cancelled
        ) {
            return;
        }
        let whole_window = drag.whole_window;
        let source_window = drag.source_window.into();
        let frame_grab = drag.frame_grab();
        let detached_tab = (!drag.whole_window
            && matches!(
                drag.lifecycle,
                TabDragLifecycle::Complete(crate::tab_drag::Outcome::Outside)
                    | TabDragLifecycle::Cancelled
            ))
        .then_some(drag.tab_id);
        let resume_native_window_drag = whole_window
            && matches!(
                drag.lifecycle,
                TabDragLifecycle::Complete(crate::tab_drag::Outcome::Outside)
                    | TabDragLifecycle::Complete(crate::tab_drag::Outcome::RolledBack)
                    | TabDragLifecycle::Cancelled
            );
        let owner_route_ids = drag
            .owner
            .as_ref()
            .map(|owner| owner.route_ids.clone())
            .unwrap_or_default();
        let withdraw_foreign_offer = !matches!(
            drag.lifecycle,
            TabDragLifecycle::Complete(crate::tab_drag::Outcome::Moved)
        );
        let transfer_token = drag.token.as_bytes();
        if withdraw_foreign_offer {
            if let Some(control) = self.window_control.as_ref() {
                control.withdraw_drag_offer(transfer_token);
            }
        }
        self.reset_tab_drag_input(source_window);
        for route_id in owner_route_ids {
            self.scheduler.unschedule_window(route_id);
        }
        self.clear_drop_marker();
        if let Some(route) = self.router.routes.get_mut(&source_window) {
            route.window.screen.clear_transfer_source_marker();
            if whole_window {
                route.window.screen.set_window_overlay(None);
            }
        } else if let Some(route) = self.retained_tab_drag_owner.as_mut() {
            route.window.screen.clear_transfer_source_marker();
            if whole_window {
                route.window.screen.set_window_overlay(None);
            }
        }
        self.tab_drag = None;
        self.tab_drag_detach_deadline = None;
        if let Some(tab_id) = detached_tab {
            self.detach_tab_from_window(event_loop, source_window, tab_id);
        }
        if resume_native_window_drag {
            if let Some(route) = self.router.routes.get(&source_window) {
                let result = frame_grab.map_or_else(
                    || {
                        route
                            .window
                            .winit_window
                            .drag_window_from_active_grab(u64::from(source_window))
                    },
                    |(seat_id, pointer_id)| {
                        route.window.winit_window.drag_window_from_frame_grab(
                            u64::from(source_window),
                            seat_id,
                            pointer_id,
                        )
                    },
                );
                if let Err(error) = result {
                    tracing::debug!(%error, "could not resume native window drag");
                }
            }
        }
        let closes = std::mem::take(&mut self.deferred_tab_drag_closes);
        if let Some(owner) = self.retained_tab_drag_owner.take() {
            let close_source = closes.iter().any(|close| {
                matches!(close, DeferredTabDragClose::Window(window_id) if *window_id == source_window)
            });
            if close_source {
                self.router.install_normal_route(owner);
            }
        }
        for close in closes {
            match close {
                DeferredTabDragClose::Terminal(route_id) => {
                    if let Some(window_id) =
                        self.router.routes.iter().find_map(|(window_id, route)| {
                            route
                                .window
                                .screen
                                .context_manager
                                .contains_route_id(route_id)
                                .then_some(*window_id)
                        })
                    {
                        self.close_terminal_at(event_loop, window_id, route_id);
                    }
                }
                DeferredTabDragClose::Window(window_id) => {
                    self.perform_close_requested(event_loop, window_id);
                }
            }
        }
    }

    fn reset_tab_drag_input(&mut self, source_window: rio_backend::event::WindowId) {
        let route_id = if let Some(route) = self.router.routes.get_mut(&source_window) {
            route.window.screen.reset_drag_input()
        } else if let Some(route) = self.retained_tab_drag_owner.as_mut() {
            route.window.screen.reset_drag_input()
        } else {
            None
        };
        if let Some(route_id) = route_id {
            self.scheduler
                .unschedule(TimerId::new(Topic::SelectionScrolling, route_id));
        }
    }

    fn defer_tab_drag_close(&mut self, close: DeferredTabDragClose) {
        if !self.deferred_tab_drag_closes.contains(&close) {
            self.deferred_tab_drag_closes.push(close);
        }
    }

    fn schedule_tab_detach(&mut self) {
        // Foreign target events belong to the other GUI. Absence of a local
        // hover is not evidence of an outside drop; wait for source cancellation.
        if self.window_control.is_some() {
            return;
        }
        self.tab_drag_detach_deadline = Some(Instant::now() + TAB_DETACH_GRACE_PERIOD);
    }

    fn handle_foreign_drag(
        &mut self,
        window: WindowId,
        event: rio_window::platform::wayland::ToplevelDragEvent,
    ) {
        use rio_window::platform::wayland::{ToplevelDragEvent as E, WindowExtWayland};
        let event_name = match &event {
            E::Entered { .. } => "entered",
            E::Motion { .. } => "motion",
            E::Dropped { .. } => "dropped",
            E::SelectedActionChanged { .. } => "selected-action",
            E::Left { .. } => "left",
            E::DataReady { .. } => "data-ready",
            E::OfferCancelled { .. } => "offer-cancelled",
            E::OfferDataFailed { .. } => "offer-data-failed",
            _ => "other",
        };
        tracing::info!(?window, event = event_name, "foreign Wayland drag event");
        let mut failed = false;
        match event {
            E::Entered { offer_id, position } | E::Motion { offer_id, position } => {
                let index = self.tab_drag_target_index(window, position, false, 1);
                if self.foreign_drag.as_ref().is_some_and(|drag| drag.dropped) {
                    return;
                }
                let Some(index) = index else {
                    self.reject_tab_drag_offer(offer_id, window);
                    self.foreign_drag = None;
                    self.clear_drop_marker();
                    return;
                };
                if self
                    .foreign_drag
                    .as_ref()
                    .is_none_or(|drag| drag.offer != offer_id || drag.window != window)
                {
                    let Some(route) = self.router.routes.get(&window.into()) else {
                        return;
                    };
                    if route
                        .window
                        .winit_window
                        .accept_toplevel_drag_offer(offer_id)
                        .is_err()
                    {
                        return;
                    }
                    self.foreign_drag = Some(ForeignDrag {
                        window,
                        offer: offer_id,
                        index,
                        dropped: false,
                        selected_move: false,
                        receiving: false,
                        token: None,
                    });
                }
                self.foreign_drag.as_mut().unwrap().index = index;
                self.set_drop_marker(window.into(), index);
            }
            E::Dropped { offer_id } => {
                if let Some(drag) = self
                    .foreign_drag
                    .as_mut()
                    .filter(|drag| drag.offer == offer_id && drag.window == window)
                {
                    drag.dropped = true;
                }
            }
            E::SelectedActionChanged {
                offer_id,
                selected_move,
            } => {
                if let Some(drag) = self
                    .foreign_drag
                    .as_mut()
                    .filter(|drag| drag.offer == offer_id && drag.window == window)
                {
                    drag.selected_move = selected_move;
                }
            }
            E::Left { offer_id } => {
                if self
                    .foreign_drag
                    .as_ref()
                    .is_some_and(|drag| drag.offer == offer_id && !drag.dropped)
                {
                    failed = true;
                }
            }
            E::OfferCancelled { offer_id } | E::OfferDataFailed { offer_id } => {
                failed = self
                    .foreign_drag
                    .as_ref()
                    .is_some_and(|drag| drag.offer == offer_id);
            }
            E::DataReady { offer_id, data } => {
                if let Some(drag) = self.foreign_drag.as_mut().filter(|drag| {
                    drag.offer == offer_id
                        && drag.window == window
                        && drag.dropped
                        && drag.receiving
                        && drag.token.is_none()
                }) {
                    if let Ok(token) = <[u8; 16]>::try_from(data.as_slice()) {
                        drag.token = Some(token);
                        failed = self.window_control.as_ref().is_none_or(|control| {
                            control
                                .take_drag_offer_async(
                                    token,
                                    u64::from(window),
                                    drag.index,
                                    self.event_proxy.clone(),
                                    window.into(),
                                )
                                .is_err()
                        });
                    } else {
                        failed = true;
                    }
                }
            }
            _ => {}
        }
        if let Some(drag) = self.foreign_drag.as_mut() {
            if drag.dropped && drag.selected_move && !drag.receiving {
                drag.receiving = true;
                failed |= self.router.routes.get(&window.into()).is_none_or(|route| {
                    route
                        .window
                        .winit_window
                        .receive_toplevel_drag_offer(drag.offer)
                        .is_err()
                });
            }
        }
        if failed {
            if let Some(drag) = self.foreign_drag.take() {
                self.cancel_tab_drag_offer(drag.offer, drag.window);
            }
            self.clear_drop_marker();
        }
    }
}

impl<'a> Application<'a> {
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    fn transfer_window_to_target(
        &mut self,
        source_id: rio_backend::event::WindowId,
        target_id: rio_backend::event::WindowId,
        index: usize,
    ) -> bool {
        if source_id == target_id {
            return false;
        }
        let [Some(source), Some(target)] = self
            .router
            .routes
            .get_disjoint_mut([&source_id, &target_id])
        else {
            return false;
        };
        if source.path != RoutePath::Terminal || target.path != RoutePath::Terminal {
            return false;
        }
        let source_size = source.window.winit_window.inner_size();
        let target_size = target.window.winit_window.inner_size();
        let Some(transfer) = source.window.screen.extract_window_transfer() else {
            return false;
        };
        match target
            .window
            .screen
            .insert_window_transfer(index, transfer, target_size)
        {
            Ok(()) => true,
            Err(transfer) => {
                source
                    .window
                    .screen
                    .insert_window_transfer(0, transfer, source_size)
                    .unwrap_or_else(|_| panic!("window transfer rollback failed"));
                false
            }
        }
    }

    fn transfer_tab_to_target(
        &mut self,
        source_id: rio_backend::event::WindowId,
        target_id: rio_backend::event::WindowId,
        index: usize,
    ) -> bool {
        if source_id == target_id {
            return false;
        }
        let [Some(source), Some(target)] = self
            .router
            .routes
            .get_disjoint_mut([&source_id, &target_id])
        else {
            return false;
        };
        if source.path != RoutePath::Terminal || target.path != RoutePath::Terminal {
            return false;
        }

        let source_index = source.window.screen.context_manager.current_index();
        let source_size = source.window.winit_window.inner_size();
        let target_size = target.window.winit_window.inner_size();
        let Some(transfer) = source.window.screen.extract_transfer(source_index) else {
            return false;
        };
        match target
            .window
            .screen
            .insert_transfer(index, transfer, target_size)
        {
            Ok(()) => {
                if !source.window.screen.context_manager.is_empty() {
                    source.window.screen.refresh_after_tab_transfer(source_size);
                    source.request_redraw();
                }
                true
            }
            Err(transfer) => {
                source
                    .window
                    .screen
                    .insert_transfer(source_index, transfer, source_size)
                    .unwrap_or_else(|_| panic!("tab transfer rollback failed"));
                false
            }
        }
    }

    fn merge_window_into_target(
        &mut self,
        source_id: rio_backend::event::WindowId,
        target_id: rio_backend::event::WindowId,
        index: Option<usize>,
    ) {
        let index = index.or_else(|| {
            self.router
                .routes
                .get(&target_id)
                .filter(|route| route.path == RoutePath::Terminal)
                .map(|route| route.window.screen.context_manager.len())
        });
        let Some(index) = index else { return };
        if self.transfer_tab_to_target(source_id, target_id, index) {
            self.clear_merge_if_window(source_id);
            let source_empty = self
                .router
                .routes
                .get(&source_id)
                .is_some_and(|route| route.window.screen.context_manager.is_empty());
            if source_empty {
                self.router.remove_route(source_id);
            }
            if let Some(target) = self.router.routes.get(&target_id) {
                target.window.winit_window.focus_window();
            }
        }
    }

    fn start_foreign_window_merge(
        &mut self,
        source_id: rio_backend::event::WindowId,
        target: crate::router::window_control::WindowEndpoint,
        target_index: usize,
    ) -> Result<(), String> {
        let transfer_id = crate::router::window_control::new_transfer_id()?;
        let source_index = self
            .router
            .routes
            .get(&source_id)
            .ok_or_else(|| "source window disappeared before merge".to_string())?
            .window
            .screen
            .context_manager
            .current_index();
        let offer = self
            .router
            .routes
            .get(&source_id)
            .ok_or_else(|| "source window disappeared before merge".to_string())?
            .window
            .screen
            .context_manager
            .transfer_offer(source_index, transfer_id)?;
        let source_routes = offer.pane_route_ids();
        let control = self
            .window_control
            .as_ref()
            .ok_or_else(|| "cross-window control is unavailable".to_string())?;
        control.offer_async(
            target,
            offer,
            Some(target_index),
            self.event_proxy.clone(),
            source_id,
        )?;
        self.track_outgoing_offer(transfer_id, source_id, source_routes);
        Ok(())
    }

    fn recovery_label(prepared: &crate::context::session::PreparedSession) -> String {
        let frame = prepared.initial_frame();
        let source = if !frame.title.trim().is_empty() {
            frame.title.as_str()
        } else {
            frame.working_dir.as_deref().unwrap_or("unnamed session")
        };
        let mut label: String = source
            .chars()
            .filter(|character| !character.is_control())
            .take(64)
            .collect();
        if label.is_empty() {
            label = "unnamed session".into();
        }
        if prepared.had_active_owner() {
            format!("Recover \"{label}\" (active owner; takeover)")
        } else {
            format!("Recover \"{label}\"")
        }
    }

    fn begin_recovery_probes(
        &mut self,
        source_window: rio_backend::event::WindowId,
        descriptors: Vec<SessionDescriptor>,
    ) {
        let descriptors: Vec<_> = descriptors.into_iter().take(8).collect();
        if descriptors.is_empty() {
            return;
        }
        while self.recovery_probe.try_recv().is_ok() {}
        let probe_id = self.next_recovery_probe_id;
        self.next_recovery_probe_id = self.next_recovery_probe_id.wrapping_add(1).max(1);
        let sender = self.recovery_probe_sender.clone();
        let event_proxy = self.event_proxy.clone();
        self.recovery_probe_state = Some(RecoveryProbeState {
            id: probe_id,
            source_window,
            remaining: descriptors.len(),
            candidates: Vec::new(),
        });
        for descriptor in descriptors {
            let sender = sender.clone();
            let event_proxy = event_proxy.clone();
            let spawn_result = std::thread::Builder::new()
                .name("rio-session-recovery-probe".into())
                .spawn(move || {
                    let prepared =
                        rio_session::SessionClient::prepare_attach(descriptor.clone())
                            .map(crate::context::session::SessionHandle::prepare_attach)
                            .map_err(|error| error.to_string());
                    let _ = sender.try_send(RecoveryProbeResult {
                        id: probe_id,
                        source_window,
                        prepared,
                        descriptor: Some(descriptor),
                    });
                    rio_backend::event::EventListener::send_event(
                        &event_proxy,
                        RioEvent::Render,
                        source_window,
                    );
                });
            if spawn_result.is_err() {
                let _ = self.recovery_probe_sender.try_send(RecoveryProbeResult {
                    id: probe_id,
                    source_window,
                    prepared: Err("could not start recovery probe".into()),
                    descriptor: None,
                });
            }
        }
    }

    fn begin_merge_window_action(
        &mut self,
        source_id: rio_backend::event::WindowId,
    ) -> bool {
        let Some(control) = self.window_control.as_ref() else {
            return false;
        };
        control
            .discover_peers_async(self.event_proxy.clone(), source_id)
            .is_ok()
    }

    fn arm_merge_source(&mut self, source_id: rio_backend::event::WindowId) -> bool {
        let Some(route) = self.router.routes.get_mut(&source_id) else {
            return false;
        };
        if route.path != RoutePath::Terminal
            || route.window.screen.context_manager.is_empty()
        {
            return false;
        }
        let index = route.window.screen.context_manager.current_index();
        route.window.screen.set_transfer_source_marker(index);
        self.merge_window_source = Some(source_id);
        self.set_window_overlay(
            source_id,
            Some(crate::renderer::WindowOverlay::MergeSource),
        );
        true
    }

    fn begin_recovery_action(&mut self, source_id: rio_backend::event::WindowId) -> bool {
        let descriptors = SessionDescriptor::discover_recovery()
            .unwrap_or_default()
            .into_iter()
            .filter(|descriptor| {
                !self.router.routes.values().any(|route| {
                    route
                        .window
                        .screen
                        .context_manager
                        .owns_session_id(descriptor.session_id)
                })
            })
            .collect::<Vec<_>>();
        if descriptors.is_empty() {
            self.show_merge_error(source_id, "no saved sessions are available".into());
            return false;
        }
        self.begin_recovery_probes(source_id, descriptors);
        if let Some(route) = self.router.routes.get_mut(&source_id) {
            // Keep recovery's saved-session list separate from live-window
            // targeting. The empty list is only a loading state.
            route.window.screen.begin_recovery_targets(Vec::new());
            route.request_redraw();
            true
        } else {
            self.recovery_probe_state = None;
            false
        }
    }

    fn arm_discovered_merge_targets(
        &mut self,
        source_id: rio_backend::event::WindowId,
        targets: Vec<crate::router::window_control::WindowEndpoint>,
    ) -> bool {
        if self.merge_window_source != Some(source_id) {
            return false;
        }
        if targets.is_empty() {
            tracing::debug!(
                source_window = ?source_id,
                "no foreign Rio windows discovered; retaining local target mode"
            );
            return true;
        }
        tracing::info!(
            source_window = ?source_id,
            foreign_targets = targets.len(),
            "merge targets discovered"
        );
        #[cfg(unix)]
        self.arm_foreign_merge_selection(source_id, targets);
        if let Some(route) = self.router.routes.get_mut(&source_id) {
            route.request_redraw();
            true
        } else {
            false
        }
    }

    fn select_recovery_target(
        &mut self,
        source_id: rio_backend::event::WindowId,
        target_index: usize,
    ) -> bool {
        let Some((pending_source, candidates)) = self.recovery_targets.take() else {
            return false;
        };
        if pending_source != source_id {
            self.recovery_targets = Some((pending_source, candidates));
            return false;
        }
        let Some(candidate) = candidates.get(target_index) else {
            self.recovery_targets = Some((pending_source, candidates));
            return false;
        };
        let candidate = RecoveryCandidate {
            descriptor: candidate.descriptor.clone(),
            label: candidate.label.clone(),
        };
        tracing::info!(source_window = ?source_id, target_index, "recovery target selected");
        self.recovery_probe_state = None;
        while self.recovery_probe.try_recv().is_ok() {}
        if let Some(route) = self.router.routes.get_mut(&source_id) {
            route.window.screen.clear_merge_ui();
            route.request_redraw();
        }
        let sender = self.recovery_probe_sender.clone();
        let listener = self.event_proxy.clone();
        let preparation = self.session_preparations.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("rio-session-recover".into())
            .spawn(move || {
                let prepared =
                    rio_session::SessionClient::prepare_attach(candidate.descriptor)
                        .map(crate::context::session::SessionHandle::prepare_attach)
                        .map_err(|error| error.to_string());
                let _ = sender.try_send(RecoveryProbeResult {
                    id: 0,
                    source_window: source_id,
                    prepared,
                    descriptor: None,
                });
                drop(preparation);
                rio_backend::event::EventListener::send_event(
                    &listener,
                    RioEvent::Render,
                    source_id,
                );
            })
        {
            self.show_merge_error(source_id, error.to_string());
            return false;
        }
        true
    }

    fn show_merge_error(
        &mut self,
        source_id: rio_backend::event::WindowId,
        error: String,
    ) {
        tracing::warn!(%error, "session recovery/merge failed");
        if let Some(route_id) = self
            .router
            .routes
            .get(&source_id)
            .map(|route| route.window.screen.context_manager.current_route())
        {
            self.show_session_error(source_id, route_id, error);
        }
    }

    fn start_recovery_import(
        &mut self,
        window_id: rio_backend::event::WindowId,
        prepared: crate::context::session::PreparedSession,
    ) -> Result<(), String> {
        let Some(route) = self.router.routes.get_mut(&window_id) else {
            return Err("target window disappeared before recovery".into());
        };
        let dimension = route.window.screen.ctx().current().dimension;
        let size = route.window.winit_window.inner_size();
        let route_id = route
            .window
            .screen
            .context_manager
            .insert_prepared_pane(
                prepared,
                crate::context::next_rich_text_id(),
                dimension,
            )
            .map_err(|_| "target context capacity is full".to_string())?;
        route.window.screen.refresh_after_tab_transfer(size);
        tracing::info!(
            window_id = ?window_id,
            route_id,
            "recovery session inserted; waiting for direct frame readiness"
        );
        self.pending_prepared.push(PendingPrepared {
            routes: vec![PendingPreparedRoute {
                source_route: None,
                target_route: route_id,
            }],
            ready: HashSet::new(),
            window_id,
            deadline: Instant::now() + Duration::from_secs(25),
            completion: PendingPreparedCompletion::Recovery,
        });
        route.request_overlay_redraw();
        self.event_proxy
            .send_event(RioEventType::Rio(RioEvent::Render), window_id);
        Ok(())
    }

    fn clear_merge_window(&mut self) {
        let mut palette_windows = HashSet::new();
        if let Some(source_id) = self.merge_window_source {
            palette_windows.insert(source_id);
        }
        if let Some(pending) = self.pending_merge_selection.as_ref() {
            palette_windows.insert(pending.source_window);
        }
        if let Some((source_id, _)) = self.recovery_targets.as_ref() {
            palette_windows.insert(*source_id);
        }
        if let Some(state) = self.recovery_probe_state.as_ref() {
            palette_windows.insert(state.source_window);
        }
        self.cancel_pending_merge_selection();
        self.clear_armed_merge_selection_and_notify();
        self.recovery_targets = None;
        self.recovery_probe_state = None;
        while self.recovery_probe.try_recv().is_ok() {}
        if let Some(source_id) = self.merge_window_source.take() {
            if let Some(route) = self.router.routes.get_mut(&source_id) {
                route.window.screen.clear_transfer_source_marker();
            }
            self.set_window_overlay(source_id, None);
        }
        self.clear_merge_target();
        for window_id in palette_windows {
            if let Some(route) = self.router.routes.get_mut(&window_id) {
                route.window.screen.clear_merge_ui();
                route.request_redraw();
            }
        }
    }

    fn cancel_pending_merge_selection(&mut self) {
        let Some(pending) = self.pending_merge_selection.take() else {
            return;
        };
        if let Some(control) = self.window_control.as_ref() {
            for target in pending.targets {
                let _ = control.cancel_selection_async(target, pending.selection_id);
            }
        }
    }

    fn clear_armed_merge_selection(&mut self) {
        let Some(armed) = self.armed_merge_selection.take() else {
            return;
        };
        self.set_window_overlay(armed.target_window, None);
        if self.drop_marker_window == Some(armed.target_window) {
            self.clear_drop_marker();
        }
    }

    fn clear_armed_merge_selection_and_notify(&mut self) {
        let Some(armed) = self.armed_merge_selection.as_ref() else {
            return;
        };
        let source = armed.source.clone();
        let selection_id = armed.selection_id;
        self.clear_armed_merge_selection();
        if let Some(control) = self.window_control.as_ref() {
            let _ = control.cancel_selection_async(source, selection_id);
        }
    }

    #[cfg(unix)]
    fn arm_foreign_merge_selection(
        &mut self,
        source_window: rio_backend::event::WindowId,
        targets: Vec<crate::router::window_control::WindowEndpoint>,
    ) {
        if targets.is_empty() {
            return;
        }
        let Some(source) = self
            .window_control
            .as_ref()
            .map(|control| control.descriptor().clone())
        else {
            return;
        };
        let Ok(selection_id) = crate::router::window_control::new_transfer_id() else {
            tracing::warn!("could not create native merge selection id");
            return;
        };
        let target_count = targets.len();
        self.pending_merge_selection = Some(PendingMergeSelection {
            selection_id,
            source_window,
            targets,
            armed: 0,
            resolved: HashSet::new(),
            deadline: Instant::now() + MERGE_SELECTION_TIMEOUT,
        });
        tracing::info!(
            source_window = ?source_window,
            target_count,
            "arming native merge targets"
        );
        self.merge_window_source = Some(source_window);
        if let Some(route) = self.router.routes.get_mut(&source_window) {
            let index = route.window.screen.context_manager.current_index();
            route.window.screen.set_transfer_source_marker(index);
        }
        self.set_window_overlay(
            source_window,
            Some(crate::renderer::WindowOverlay::MergeSource),
        );
        let pending = self
            .pending_merge_selection
            .as_ref()
            .expect("native merge selection was not installed");
        if let Some(control) = self.window_control.as_ref() {
            for target in &pending.targets {
                if let Err(error) = control.arm_selection_async(
                    target.clone(),
                    selection_id,
                    source.clone(),
                    self.event_proxy.clone(),
                    source_window,
                ) {
                    tracing::debug!(%error, "could not arm native merge target");
                }
            }
        }
    }

    fn clear_merge_if_window(&mut self, window_id: rio_backend::event::WindowId) {
        if self.merge_state_uses_window(window_id) {
            self.clear_merge_window();
        }
    }

    fn merge_state_uses_window(&self, window_id: rio_backend::event::WindowId) -> bool {
        self.merge_source_uses_window(window_id)
            || self
                .merge_target
                .is_some_and(|(target_id, _)| target_id == window_id)
            || self
                .armed_merge_selection
                .as_ref()
                .is_some_and(|armed| armed.target_window == window_id)
    }

    fn merge_source_uses_window(&self, window_id: rio_backend::event::WindowId) -> bool {
        self.merge_window_source == Some(window_id)
            || self
                .pending_merge_selection
                .as_ref()
                .is_some_and(|pending| pending.source_window == window_id)
            || self
                .recovery_targets
                .as_ref()
                .is_some_and(|(source_id, _)| *source_id == window_id)
            || self
                .recovery_probe_state
                .as_ref()
                .is_some_and(|state| state.source_window == window_id)
    }

    fn clear_merge_target(&mut self) {
        if let Some((target_id, _)) = self.merge_target.take() {
            self.set_window_overlay(target_id, None);
        }
        self.clear_drop_marker();
    }

    fn update_merge_target(
        &mut self,
        target_window: rio_backend::event::WindowId,
        position: rio_window::dpi::PhysicalPosition<f64>,
    ) {
        let Some(source_id) = self.merge_window_source else {
            return;
        };
        if source_id == target_window {
            self.clear_merge_target();
            return;
        }
        let source_ready = self
            .router
            .routes
            .get(&source_id)
            .filter(|route| route.path == RoutePath::Terminal)
            .is_some_and(|route| !route.window.screen.context_manager.is_empty());
        if !source_ready {
            self.clear_merge_target();
            return;
        }
        let Some(route) = self
            .router
            .routes
            .get(&target_window)
            .filter(|route| route.path == RoutePath::Terminal)
        else {
            self.clear_merge_target();
            return;
        };
        let count = route.window.screen.context_manager.len();
        let index = route
            .window
            .screen
            .tab_drop_index(position.x, position.y)
            .unwrap_or(count);
        if !route
            .window
            .screen
            .context_manager
            .can_insert_grids(index, 1)
        {
            self.clear_merge_target();
            return;
        }
        if self
            .merge_target
            .is_some_and(|(current_target, _)| current_target != target_window)
        {
            self.clear_merge_target();
        }
        self.merge_target = Some((target_window, index));
        self.set_drop_marker(target_window, index);
        self.set_window_overlay(
            target_window,
            Some(crate::renderer::WindowOverlay::MergeTarget),
        );
    }

    fn set_window_overlay(
        &mut self,
        window_id: rio_backend::event::WindowId,
        overlay: Option<crate::renderer::WindowOverlay>,
    ) {
        if let Some(route) = self.router.routes.get_mut(&window_id) {
            let changed = route.window.screen.set_window_overlay(overlay);
            tracing::info!(window_id = ?window_id, changed, "window overlay state updated");
            if changed {
                route.request_redraw();
            }
        }
    }

    fn clear_drop_marker(&mut self) {
        let Some(window_id) = self.drop_marker_window.take() else {
            return;
        };
        if let Some(route) = self.router.routes.get_mut(&window_id) {
            if route.window.screen.clear_tab_drop_marker() {
                route.request_redraw();
            }
        }
    }

    fn set_drop_marker(&mut self, window_id: rio_backend::event::WindowId, index: usize) {
        if self.drop_marker_window != Some(window_id) {
            self.clear_drop_marker();
        }
        if let Some(route) = self.router.routes.get_mut(&window_id) {
            if route.window.screen.set_tab_drop_marker(index) {
                route.request_redraw();
            }
            self.drop_marker_window = Some(window_id);
        }
    }

    fn clear_armed_merge_hover(&mut self, window_id: rio_backend::event::WindowId) {
        if let Some(armed) = self.armed_merge_selection.as_mut() {
            if armed.target_window == window_id {
                armed.hover.clear();
            }
        }
        self.set_window_overlay(window_id, None);
        if self.drop_marker_window == Some(window_id) {
            self.clear_drop_marker();
        }
    }

    fn valid_merge_target_index(
        &self,
        window_id: rio_backend::event::WindowId,
        index: usize,
    ) -> bool {
        let Some(route) = self.router.routes.get(&window_id) else {
            return false;
        };
        if route.path != RoutePath::Terminal
            || route.window.screen.context_manager.is_empty()
        {
            return false;
        }
        let count = route.window.screen.context_manager.len();
        merge_target_index_is_valid(
            index,
            count,
            route
                .window
                .screen
                .context_manager
                .can_insert_grids(index, 1),
        )
    }

    fn merge_target_index_at(
        &self,
        window_id: rio_backend::event::WindowId,
        position: Option<rio_window::dpi::PhysicalPosition<f64>>,
    ) -> Option<usize> {
        let route = self.router.routes.get(&window_id)?;
        if route.path != RoutePath::Terminal
            || route.window.screen.context_manager.is_empty()
        {
            return None;
        }
        let count = route.window.screen.context_manager.len();
        let index = position
            .and_then(|position| {
                route.window.screen.tab_drop_index(position.x, position.y)
            })
            .unwrap_or(count);
        self.valid_merge_target_index(window_id, index)
            .then_some(index)
    }

    fn handle_armed_merge_window_event(
        &mut self,
        window_id: rio_backend::event::WindowId,
        event: &WindowEvent,
    ) -> bool {
        let Some(armed) = self.armed_merge_selection.as_ref() else {
            return false;
        };
        if armed.target_window != window_id {
            return false;
        }
        let selection_id = armed.selection_id;
        let source = armed.source.clone();
        let send_selection = |application: &mut Self, index: usize, clicked: bool| {
            if let Some(control) = application.window_control.as_ref() {
                let _ = control.send_selection_event_async(
                    source.clone(),
                    selection_id,
                    index,
                    clicked,
                );
            }
        };
        match event {
            WindowEvent::CursorEntered { .. } => {
                let Some(index) = self.merge_target_index_at(window_id, None) else {
                    self.clear_armed_merge_hover(window_id);
                    return true;
                };
                if let Some(armed) = self.armed_merge_selection.as_mut() {
                    armed.hover.set(index);
                }
                self.set_window_overlay(
                    window_id,
                    Some(crate::renderer::WindowOverlay::MergeTarget),
                );
                self.set_drop_marker(window_id, index);
                tracing::info!(
                    target_window = ?window_id,
                    target_index = index,
                    "native merge target pointer entered"
                );
                send_selection(self, index, false);
                true
            }
            WindowEvent::CursorLeft { .. } => {
                self.clear_armed_merge_hover(window_id);
                true
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(index) = self.merge_target_index_at(window_id, Some(*position))
                else {
                    self.clear_armed_merge_hover(window_id);
                    return true;
                };
                let changed = self
                    .armed_merge_selection
                    .as_ref()
                    .is_none_or(|armed| armed.hover.index != Some(index));
                self.set_window_overlay(
                    window_id,
                    Some(crate::renderer::WindowOverlay::MergeTarget),
                );
                self.set_drop_marker(window_id, index);
                if changed {
                    if let Some(armed) = self.armed_merge_selection.as_mut() {
                        armed.hover.set(index);
                    }
                    tracing::info!(
                        target_window = ?window_id,
                        target_index = index,
                        "native merge target pointer moved"
                    );
                    send_selection(self, index, false);
                }
                true
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let Some(index) = self
                    .armed_merge_selection
                    .as_ref()
                    .and_then(|armed| armed.hover.clicked_index())
                else {
                    tracing::debug!(
                        target_window = ?window_id,
                        "ignoring merge click without a hovered target"
                    );
                    return true;
                };
                if !self.valid_merge_target_index(window_id, index) {
                    self.clear_armed_merge_hover(window_id);
                    return true;
                }
                tracing::info!(
                    target_window = ?window_id,
                    target_index = index,
                    "native merge target clicked"
                );
                send_selection(self, index, true);
                self.clear_armed_merge_selection();
                true
            }
            WindowEvent::MouseInput { .. } => true,
            _ => false,
        }
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    fn tab_drag_owns_offer(
        &self,
        offer_id: rio_window::platform::wayland::ToplevelDragOfferId,
    ) -> bool {
        self.tab_drag.as_ref().is_some_and(|drag| {
            drag.hover.is_some_and(|hover| hover.offer_id == offer_id)
        })
    }
}

impl Application<'_> {
    fn ensure_window_control(&mut self, window_id: rio_backend::event::WindowId) {
        if self.window_control.is_some() {
            return;
        }
        match crate::router::window_control::WindowControl::new(
            self.event_proxy.clone(),
            window_id,
        ) {
            Ok(control) => self.window_control = Some(control),
            Err(error) => tracing::warn!(%error, "cross-window control unavailable"),
        }
    }

    fn poll_recovery_probes(&mut self) {
        let recovery_source = self
            .recovery_probe_state
            .as_ref()
            .map(|state| state.source_window)
            .or_else(|| self.recovery_targets.as_ref().map(|(source, _)| *source));
        if let Some(source_window) = recovery_source {
            let picker_open =
                self.router.routes.get(&source_window).is_some_and(|route| {
                    route.window.screen.renderer.command_palette.is_enabled()
                });
            if !picker_open {
                self.recovery_probe_state = None;
                self.recovery_targets = None;
                while self.recovery_probe.try_recv().is_ok() {}
                if let Some(route) = self.router.routes.get_mut(&source_window) {
                    route.window.screen.clear_merge_ui();
                    route.request_redraw();
                }
                return;
            }
        }
        let recovery_open = self.recovery_probe_state.as_ref().is_some_and(|state| {
            self.router
                .routes
                .get(&state.source_window)
                .is_some_and(|route| {
                    route.window.screen.renderer.command_palette.is_enabled()
                })
        });
        if self.recovery_probe_state.is_some() && !recovery_open {
            self.recovery_probe_state = None;
            while self.recovery_probe.try_recv().is_ok() {}
            return;
        }
        while let Ok(result) = self.recovery_probe.try_recv() {
            if result.id == 0 {
                let outcome = result.prepared.and_then(|prepared| {
                    self.start_recovery_import(result.source_window, prepared)
                });
                if let Err(error) = outcome {
                    self.show_merge_error(result.source_window, error);
                }
                continue;
            }
            let Some(state) = self.recovery_probe_state.as_mut() else {
                continue;
            };
            if state.id != result.id || state.source_window != result.source_window {
                continue;
            }
            state.remaining = state.remaining.saturating_sub(1);
            if let Ok(prepared) = result.prepared {
                // A probe's commit deadline must not span user interaction.
                if let Some(descriptor) = result.descriptor {
                    state
                        .candidates
                        .push(RecoveryCandidate::from_probe(descriptor, prepared));
                }
            }
            if state.remaining != 0 {
                continue;
            }
            let state = self
                .recovery_probe_state
                .take()
                .expect("probe state exists");
            let still_open =
                self.router
                    .routes
                    .get(&state.source_window)
                    .is_some_and(|route| {
                        route.window.screen.renderer.command_palette.is_enabled()
                    });
            if !still_open {
                continue;
            }
            let labels = state
                .candidates
                .iter()
                .map(|candidate| candidate.label.clone())
                .collect::<Vec<_>>();
            if state.candidates.is_empty() {
                self.show_merge_error(
                    state.source_window,
                    "no recoverable sessions could be prepared".into(),
                );
                if let Some(route) = self.router.routes.get_mut(&state.source_window) {
                    route.window.screen.clear_merge_ui();
                    route.request_redraw();
                }
                continue;
            }
            tracing::info!(
                source_window = ?state.source_window,
                recovery_targets = state.candidates.len(),
                "saved session recovery targets discovered"
            );
            self.recovery_targets = Some((state.source_window, state.candidates));
            if let Some(route) = self.router.routes.get_mut(&state.source_window) {
                route.window.screen.begin_recovery_targets(labels);
                route.request_redraw();
            }
        }
    }

    fn expire_merge_selection(&mut self) {
        let now = Instant::now();
        let pending_expired = self
            .pending_merge_selection
            .as_ref()
            .is_some_and(|pending| pending.deadline <= now);
        let armed_expired = self
            .armed_merge_selection
            .as_ref()
            .is_some_and(|armed| armed.deadline <= now);
        if pending_expired || armed_expired {
            self.clear_merge_window();
        }
    }

    fn handle_arm_selection(
        &mut self,
        window_id: rio_backend::event::WindowId,
        selection_id: [u8; 16],
        source: crate::router::window_control::WindowEndpoint,
        reply: SyncSender<crate::router::window_control::WindowControlResponse>,
    ) {
        let valid_source = {
            #[cfg(unix)]
            {
                self.window_control.as_ref().is_some_and(|control| {
                    control.descriptor().instance != source.instance
                })
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        let has_terminal = self.router.routes.get(&window_id).is_some_and(|route| {
            route.path == RoutePath::Terminal
                && !route.window.screen.context_manager.is_empty()
        });
        let has_capacity = self.router.routes.get(&window_id).is_some_and(|route| {
            let count = route.window.screen.context_manager.len();
            route
                .window
                .screen
                .context_manager
                .can_insert_grids(count, 1)
        });
        if !valid_source || !has_terminal || !has_capacity {
            tracing::warn!(
                target_window = ?window_id,
                valid_source,
                has_terminal,
                has_capacity,
                "rejecting native merge target arm"
            );
            let _ = reply.try_send(
                crate::router::window_control::WindowControlResponse::Rejected(
                    "target window is unavailable".into(),
                ),
            );
            return;
        }
        if let Some(armed) = self.armed_merge_selection.as_ref() {
            if armed.selection_id != selection_id {
                let _ = reply.try_send(
                    crate::router::window_control::WindowControlResponse::Rejected(
                        "target already has a merge selection".into(),
                    ),
                );
                return;
            }
        }
        self.clear_armed_merge_selection();
        self.armed_merge_selection = Some(ArmedMergeSelection {
            selection_id,
            source,
            target_window: window_id,
            hover: MergeHoverState::default(),
            deadline: Instant::now() + MERGE_SELECTION_TIMEOUT,
        });
        tracing::info!(target_window = ?window_id, "native merge target armed");
        let _ = reply.try_send(
            crate::router::window_control::WindowControlResponse::SelectionArmed,
        );
    }

    fn handle_arm_selection_result(
        &mut self,
        selection_id: [u8; 16],
        target: crate::router::window_control::WindowEndpoint,
        result: Result<(), String>,
    ) {
        if let Err(error) = &result {
            tracing::warn!(%error, "native merge target arm failed");
        }
        let Some(pending) = self.pending_merge_selection.as_mut() else {
            if result.is_ok() {
                if let Some(control) = self.window_control.as_ref() {
                    let _ = control.cancel_selection_async(target, selection_id);
                }
            }
            return;
        };
        if pending.selection_id != selection_id {
            if result.is_ok() {
                if let Some(control) = self.window_control.as_ref() {
                    let _ = control.cancel_selection_async(target, selection_id);
                }
            }
            return;
        }
        if !pending.resolved.insert(target.instance) {
            return;
        }
        if result.is_ok() {
            pending.armed += 1;
        } else {
            tracing::debug!(
                target = target.native_window_id,
                error = ?result,
                "native merge target did not arm"
            );
        }
        let all_resolved = pending.resolved.len() == pending.targets.len();
        let no_target_armed = pending.armed == 0;
        if all_resolved && no_target_armed {
            self.clear_merge_window();
        }
    }

    fn handle_selection_event(
        &mut self,
        selection_id: [u8; 16],
        target_window: u64,
        target_index: u32,
        clicked: bool,
    ) {
        let Ok(target_index) = usize::try_from(target_index) else {
            return;
        };
        let Some(pending) = self.pending_merge_selection.as_ref() else {
            return;
        };
        if pending.selection_id != selection_id {
            return;
        }
        let Some(target) = pending
            .targets
            .iter()
            .find(|target| target.native_window_id == target_window)
            .cloned()
        else {
            return;
        };
        if !clicked {
            return;
        }
        let source_window = pending.source_window;
        self.clear_merge_window();
        if let Err(error) =
            self.start_foreign_window_merge(source_window, target, target_index)
        {
            self.show_merge_error(source_window, error);
        }
    }

    fn poll_window_control(&mut self, event_loop: &ActiveEventLoop) {
        self.poll_recovery_probes();
        self.expire_merge_selection();
        self.expire_pending_prepared();
        let ready = std::mem::take(&mut self.ready_session_imports);
        for (window_id, routes) in ready {
            tracing::info!(
                ?window_id,
                ready_routes = ?routes,
                pending_prepared = self.pending_prepared.len(),
                "processing direct-ready session imports"
            );
            self.finish_ready_prepared(window_id, routes);
        }

        let events = self
            .window_control
            .as_ref()
            .map(crate::router::window_control::WindowControl::poll)
            .unwrap_or_default();
        for event in events {
            match event {
                #[cfg(all(feature = "wayland", target_os = "linux"))]
                crate::router::window_control::WindowControlEvent::DragResult {
                    transfer_id,
                    result,
                } => {
                    #[cfg(all(feature = "wayland", target_os = "linux"))]
                    if self
                        .foreign_drag
                        .as_ref()
                        .is_some_and(|drag| drag.token == Some(transfer_id))
                    {
                        use rio_window::platform::wayland::WindowExtWayland;
                        let drag = self.foreign_drag.take().unwrap();
                        if let Some(route) = self.router.routes.get(&drag.window.into()) {
                            if result.is_ok() {
                                if let Err(error) = route
                                    .window
                                    .winit_window
                                    .finish_toplevel_drag_offer(drag.offer)
                                {
                                    tracing::warn!(%error, "committed foreign drag finish failed");
                                }
                            } else {
                                let _ = route
                                    .window
                                    .winit_window
                                    .cancel_toplevel_drag_offer(drag.offer);
                            }
                        }
                        self.clear_drop_marker();
                        if let Err(error) = result {
                            self.show_merge_error(drag.window.into(), error);
                        }
                    }
                }
                crate::router::window_control::WindowControlEvent::Probe {
                    window_id,
                    reply,
                } => {
                    let response = if self.router.routes.contains_key(&window_id) {
                        crate::router::window_control::WindowControlResponse::Hello
                    } else {
                        crate::router::window_control::WindowControlResponse::Rejected(
                            "window is gone".into(),
                        )
                    };
                    let _ = reply.try_send(response);
                }
                crate::router::window_control::WindowControlEvent::Peers {
                    window_id,
                    targets,
                } => {
                    self.arm_discovered_merge_targets(window_id, targets);
                }
                crate::router::window_control::WindowControlEvent::IncomingOffer {
                    offer,
                    reply,
                    target_window,
                    target_index,
                } => {
                    let sender = self.incoming_prepared_sender.clone();
                    let preparation_reply = reply.clone();
                    let preparation = self.session_preparations.clone();
                    let event_proxy = self.event_proxy.clone();
                    let wake_window = target_window
                        .map(rio_backend::event::WindowId::from)
                        .or_else(|| self.router.routes.keys().next().copied());
                    let preparation = std::thread::Builder::new()
                        .name("rio-window-prepare".into())
                        .spawn(move || {
                            let prepared = offer
                                .panes
                                .iter()
                                .map(|pane| {
                                    rio_session::SessionClient::prepare_attach(
                                        pane.session.clone(),
                                    )
                                    .map(crate::context::session::SessionHandle::prepare_attach)
                                    .map_err(|error| error.to_string())
                                })
                                .collect::<Result<Vec<_>, _>>();
                            let _ = sender.try_send(PreparedIncoming {
                                offer,
                                prepared,
                                reply,
                                target_window,
                                target_index,
                            });
                            drop(preparation);
                            if let Some(window_id) = wake_window {
                                rio_backend::event::EventListener::send_event(
                                    &event_proxy,
                                    RioEvent::Render,
                                    window_id,
                                );
                            }
                        });
                    if let Err(error) = preparation {
                        let _ = preparation_reply.try_send(
                            crate::router::window_control::WindowControlResponse::Rejected(
                                format!("start transfer preparation: {error}"),
                            ),
                        );
                    }
                }
                crate::router::window_control::WindowControlEvent::OfferResult {
                    transfer_id,
                    result,
                } => {
                    tracing::info!(
                        committed = result.is_ok(),
                        "received foreign drag commit result"
                    );
                    self.finish_outgoing_transfer(event_loop, transfer_id, result)
                }
                crate::router::window_control::WindowControlEvent::ArmSelection {
                    window_id,
                    selection_id,
                    source,
                    reply,
                } => {
                    tracing::info!(target_window = ?window_id, "processing native merge target arm");
                    self.handle_arm_selection(window_id, selection_id, source, reply)
                }
                crate::router::window_control::WindowControlEvent::CancelSelection {
                    window_id,
                    selection_id,
                } => {
                    if self
                        .pending_merge_selection
                        .as_ref()
                        .is_some_and(|pending| pending.selection_id == selection_id)
                    {
                        self.clear_merge_window();
                    } else if self
                        .armed_merge_selection
                        .as_ref()
                        .is_some_and(|armed| {
                            armed.target_window == window_id
                                && armed.selection_id == selection_id
                        })
                    {
                        self.clear_armed_merge_selection();
                    }
                }
                crate::router::window_control::WindowControlEvent::Selection {
                    selection_id,
                    target_window,
                    target_index,
                    clicked,
                } => self.handle_selection_event(
                    selection_id,
                    target_window,
                    target_index,
                    clicked,
                ),
                crate::router::window_control::WindowControlEvent::ArmSelectionResult {
                    selection_id,
                    target,
                    result,
                } => self.handle_arm_selection_result(selection_id, target, result),
            }
        }

        while let Ok(prepared) = self.incoming_prepared.try_recv() {
            self.install_incoming_transfer(prepared);
        }
    }

    fn expire_pending_prepared(&mut self) {
        let now = Instant::now();
        let mut expired = Vec::new();
        let mut index = 0;
        while index < self.pending_prepared.len() {
            if self.pending_prepared[index].deadline <= now {
                expired.push(self.pending_prepared.remove(index));
            } else {
                index += 1;
            }
        }
        for pending in expired {
            let routes: Vec<_> = pending
                .routes
                .iter()
                .map(|route| route.target_route)
                .collect();
            let recovery =
                matches!(&pending.completion, PendingPreparedCompletion::Recovery);
            let current_route = self
                .remove_pending_prepared_routes(pending.window_id, &routes)
                .filter(|_| recovery);
            match pending.completion {
                PendingPreparedCompletion::Incoming { reply } => {
                    let _ = reply.try_send(
                        crate::router::window_control::WindowControlResponse::Rejected(
                            "target renderer readiness timed out".into(),
                        ),
                    );
                }
                PendingPreparedCompletion::Recovery => {
                    if let Some(route_id) = current_route {
                        self.show_session_error(
                            pending.window_id,
                            route_id,
                            "recovery renderer readiness timed out".into(),
                        );
                    }
                }
            }
        }
    }

    fn install_incoming_transfer(&mut self, incoming: PreparedIncoming) {
        let window_id = match incoming.target_window {
            Some(target_window) => {
                let window_id = rio_backend::event::WindowId::from(target_window);
                if self
                    .router
                    .routes
                    .get(&window_id)
                    .is_some_and(|route| route.path == RoutePath::Terminal)
                {
                    Some(window_id)
                } else {
                    None
                }
            }
            None => self
                .router
                .get_focused_route()
                .or_else(|| self.router.routes.keys().next().copied()),
        };
        let Some(window_id) = window_id else {
            let _ = incoming.reply.try_send(
                crate::router::window_control::WindowControlResponse::Rejected(
                    "target has no terminal window".into(),
                ),
            );
            return;
        };
        let prepared = match incoming.prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = incoming.reply.try_send(
                    crate::router::window_control::WindowControlResponse::Rejected(error),
                );
                return;
            }
        };
        let Some(route) = self.router.routes.get_mut(&window_id) else {
            let _ = incoming.reply.try_send(
                crate::router::window_control::WindowControlResponse::Rejected(
                    "target route disappeared".into(),
                ),
            );
            return;
        };
        if route
            .window
            .screen
            .context_manager
            .bootstrap_transfer
            .is_some_and(|(id, _)| id != incoming.offer.transfer_id)
        {
            let _ = incoming.reply.try_send(
                crate::router::window_control::WindowControlResponse::Rejected(
                    "unexpected bootstrap transfer".into(),
                ),
            );
            return;
        }
        let dimension = route.window.screen.ctx().current().dimension;
        let size = route.window.winit_window.inner_size();
        let source_routes: Vec<_> = incoming
            .offer
            .panes
            .iter()
            .map(|pane| pane.route_id)
            .collect();
        let prepared = incoming.offer.panes.into_iter().zip(prepared).collect();
        let target_routes = route.window.screen.context_manager.insert_prepared_tabs(
            prepared,
            incoming.offer.tabs,
            incoming.offer.active_pane as usize,
            incoming.target_index,
            dimension,
        );
        let Ok(target_routes) = target_routes else {
            let _ = incoming.reply.try_send(
                crate::router::window_control::WindowControlResponse::Rejected(
                    "target context capacity or insertion index is invalid".into(),
                ),
            );
            return;
        };
        if let Some((_, placeholder)) = route
            .window
            .screen
            .context_manager
            .bootstrap_transfer
            .take()
        {
            route
                .window
                .screen
                .context_manager
                .remove_transferred_routes(
                    &[placeholder],
                    &mut route.window.screen.sugarloaf,
                );
            route.window.screen.discard_routes([placeholder]);
        }
        route.window.screen.refresh_after_tab_transfer(size);
        let routes = source_routes
            .into_iter()
            .zip(target_routes)
            .map(|(source_route, target_route)| PendingPreparedRoute {
                source_route: Some(source_route),
                target_route,
            })
            .collect();
        self.pending_prepared.push(PendingPrepared {
            routes,
            ready: HashSet::new(),
            window_id,
            deadline: Instant::now() + Duration::from_secs(25),
            completion: PendingPreparedCompletion::Incoming {
                reply: incoming.reply,
            },
        });
        route.request_overlay_redraw();
        // Installation is drained from about_to_wait, after this iteration's
        // native redraw dispatch. Wake another iteration for the first frame.
        self.event_proxy
            .send_event(RioEventType::Rio(RioEvent::Render), window_id);
    }

    fn finish_outgoing_transfer(
        &mut self,
        event_loop: &ActiveEventLoop,
        transfer_id: [u8; 16],
        result: Result<Vec<u64>, String>,
    ) {
        let Some(index) = self
            .pending_outgoing
            .iter()
            .position(|pending| pending.transfer_id == transfer_id)
        else {
            tracing::warn!(
                "received foreign drag result without a pending outgoing transfer"
            );
            return;
        };
        let pending = self.pending_outgoing.remove(index);
        let routes = match result {
            Ok(routes) => match validate_committed_routes(&pending.source_routes, routes)
            {
                Ok(routes) => routes,
                Err(error) => {
                    tracing::warn!(%error, "cross-window transfer returned an invalid commit");
                    return self
                        .report_outgoing_transfer_error(pending.source_window, error);
                }
            },
            Err(error) => {
                tracing::warn!(%error, "cross-window transfer rejected");
                return self.report_outgoing_transfer_error(pending.source_window, error);
            }
        };
        tracing::info!(
            source_window = ?pending.source_window,
            routes = ?routes,
            "applying committed foreign drag routes"
        );
        if let Some(route) = self.router.routes.get_mut(&pending.source_window) {
            route
                .window
                .screen
                .context_manager
                .remove_transferred_routes(&routes, &mut route.window.screen.sugarloaf);
            route.window.screen.discard_routes(routes.iter().copied());
            if route.window.screen.context_manager.is_empty() {
                tracing::info!(
                    source_window = ?pending.source_window,
                    "foreign drag emptied source window"
                );
                self.clear_merge_if_window(pending.source_window);
                self.router.remove_route(pending.source_window);
                self.exit_after_transfer = true;
            } else {
                let size = route.window.winit_window.inner_size();
                route.window.screen.refresh_after_tab_transfer(size);
                tracing::info!(
                    remaining_routes = ?route.window.screen.context_manager.route_ids(),
                    "foreign drag left source routes"
                );
                route.request_overlay_redraw();
            }
        } else {
            tracing::warn!(
                source_window = ?pending.source_window,
                "foreign drag source window disappeared before route removal"
            );
        }
        #[cfg(all(feature = "wayland", target_os = "linux"))]
        self.dispatch_tab_drag_event(
            event_loop,
            TabDragEvent::ForeignCommitted(transfer_id),
        );
    }

    fn report_outgoing_transfer_error(
        &mut self,
        window_id: rio_backend::event::WindowId,
        error: String,
    ) {
        if let Some(route_id) = self
            .router
            .routes
            .get(&window_id)
            .map(|route| route.window.screen.context_manager.current_route())
        {
            self.show_session_error(window_id, route_id, error);
        }
    }

    fn commit_prepared_route(
        &mut self,
        window_id: rio_backend::event::WindowId,
        route_id: usize,
        kind: &str,
    ) -> Result<(), String> {
        let event_proxy = self.event_proxy.clone();
        let Some(route) = self.router.routes.get_mut(&window_id) else {
            return Err(format!("{kind} target window disappeared before commit"));
        };
        let Some(context) = route
            .window
            .screen
            .context_manager
            .get_by_route_id(route_id)
        else {
            return Err(format!("{kind} target route disappeared"));
        };
        if let Some(error) = context.terminal.lock().decoder_error() {
            return Err(format!("cannot decode {kind} session frame: {error}"));
        }
        context
            .commit_pending_session(event_proxy)
            .map_err(|error| error.to_string())
    }

    fn remove_pending_prepared_routes(
        &mut self,
        window_id: rio_backend::event::WindowId,
        routes: &[usize],
    ) -> Option<usize> {
        let current_route = {
            let route = self.router.routes.get_mut(&window_id)?;
            route
                .window
                .screen
                .context_manager
                .remove_transferred_routes(routes, &mut route.window.screen.sugarloaf);
            route.window.screen.discard_routes(routes.iter().copied());
            let current_route =
                current_route_if_contexts_remain(&route.window.screen.context_manager);
            if current_route.is_some() {
                let size = route.window.winit_window.inner_size();
                route.window.screen.refresh_after_tab_transfer(size);
                route.request_redraw();
            }
            current_route
        };
        if current_route.is_none() {
            self.clear_merge_if_window(window_id);
            self.router.remove_route(window_id);
        }
        current_route
    }

    fn finish_ready_prepared(
        &mut self,
        window_id: rio_backend::event::WindowId,
        ready: Vec<usize>,
    ) {
        for route_id in &ready {
            for pending in &mut self.pending_prepared {
                if pending.window_id == window_id
                    && pending
                        .routes
                        .iter()
                        .any(|route| route.target_route == *route_id)
                {
                    pending.ready.insert(*route_id);
                }
            }
        }

        let mut index = 0;
        while index < self.pending_prepared.len() {
            if self.pending_prepared[index].ready.len()
                != self.pending_prepared[index].routes.len()
            {
                index += 1;
                continue;
            }
            let pending = self.pending_prepared.remove(index);
            match pending.completion {
                PendingPreparedCompletion::Incoming { reply } => {
                    self.commit_ready_incoming(
                        window_id,
                        ready.as_slice(),
                        pending.routes,
                        reply,
                    );
                }
                PendingPreparedCompletion::Recovery => {
                    self.commit_ready_recovery(window_id, pending.routes);
                }
            }
        }
    }

    fn commit_ready_incoming(
        &mut self,
        window_id: rio_backend::event::WindowId,
        ready: &[usize],
        routes: Vec<PendingPreparedRoute>,
        reply: SyncSender<crate::router::window_control::WindowControlResponse>,
    ) {
        tracing::info!(
            ?window_id,
            ready_routes = ?ready,
            pending_routes = ?routes,
            "committing direct-ready session imports"
        );
        let mut committed = Vec::new();
        let mut committed_targets = HashSet::new();
        let mut error = None;
        if self.router.routes.contains_key(&window_id) {
            for route in &routes {
                let source_route = route
                    .source_route
                    .expect("incoming prepared route has no source route");
                match self.commit_prepared_route(
                    window_id,
                    route.target_route,
                    "prepared",
                ) {
                    Ok(()) => {
                        committed.push(source_route);
                        committed_targets.insert(route.target_route);
                    }
                    Err(error_value) => {
                        error = Some(error_value);
                        break;
                    }
                }
            }
            if error.is_some() {
                let uncommitted: Vec<_> = routes
                    .iter()
                    .filter(|route| !committed_targets.contains(&route.target_route))
                    .map(|route| route.target_route)
                    .collect();
                self.remove_pending_prepared_routes(window_id, &uncommitted);
            }
            if error.is_none() {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.request_redraw();
                }
            }
        } else {
            error = Some("target window disappeared before commit".into());
        }
        if let Some(error) = error {
            tracing::warn!(
                ?window_id,
                committed_routes = ?committed,
                %error,
                "direct-ready session import commit failed"
            );
            if committed.is_empty() {
                let _ = reply.try_send(
                    crate::router::window_control::WindowControlResponse::Rejected(error),
                );
            } else {
                let _ = reply.try_send(
                    crate::router::window_control::WindowControlResponse::Committed {
                        routes: committed,
                    },
                );
            }
        } else {
            tracing::info!(
                ?window_id,
                committed_routes = ?committed,
                "direct-ready session imports committed"
            );
            let _ = reply.try_send(
                crate::router::window_control::WindowControlResponse::Committed {
                    routes: committed,
                },
            );
        }
    }

    fn commit_ready_recovery(
        &mut self,
        window_id: rio_backend::event::WindowId,
        routes: Vec<PendingPreparedRoute>,
    ) {
        let [route] = routes.as_slice() else {
            unreachable!("recovery has exactly one prepared route")
        };
        let target_route = route.target_route;
        let error = self
            .commit_prepared_route(window_id, target_route, "recovered")
            .err();
        let current_route = self
            .router
            .routes
            .get(&window_id)
            .map(|route| route.window.screen.context_manager.current_route());
        if error.is_some() {
            self.remove_pending_prepared_routes(window_id, &[target_route]);
        } else if let Some(route) = self.router.routes.get_mut(&window_id) {
            route.request_redraw();
        }
        if let Some(error) = error {
            if let Some(route_id) = current_route {
                self.show_session_error(window_id, route_id, error);
            }
        }
    }

    fn resumed(&mut self, _active_event_loop: &ActiveEventLoop) {}

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause != StartCause::Init
            && cause != StartCause::CreateWindow
            && cause != StartCause::MacOSReopen
        {
            return;
        }

        if cause == StartCause::MacOSReopen && !self.router.routes.is_empty() {
            // Reopen (dock click) with every window minimized should
            // restore one; otherwise clicking the dock icon does
            // nothing at all.
            let all_minimized = self
                .router
                .routes
                .values()
                .all(|route| route.window.winit_window.is_minimized() == Some(true));
            if all_minimized {
                if let Some(route) = self.router.routes.values().next() {
                    route.window.winit_window.set_minimized(false);
                    route.window.winit_window.focus_window();
                }
            }
            return;
        }

        #[cfg(all(
            any(feature = "x11", feature = "wayland"),
            unix,
            not(any(target_os = "redox", target_family = "wasm", target_os = "macos"))
        ))]
        if cause == StartCause::Init
            && self.config.adaptive_colors.is_some()
            && self.config.force_theme.is_none()
        {
            use rio_window::platform::linux::ActiveEventLoopExtLinux;
            event_loop.start_system_theme_monitor();
        }

        let theme = self
            .config
            .force_theme
            .map(|t| t.to_window_theme())
            .or_else(|| event_loop.system_theme());
        update_colors_based_on_theme(&mut self.config, theme);

        self.router.create_window(
            event_loop,
            self.event_proxy.clone(),
            &self.config,
            None,
            self.app_id.as_deref(),
        );
        if let Some(window_id) = self.router.routes.keys().next().copied() {
            self.ensure_window_control(window_id);
        }

        if cause == StartCause::Init {
            self.setup_quake_hotkey();
        }

        // Schedule title updates every 2s
        let timer_id = TimerId::new(Topic::UpdateTitles, 0);
        if !self.scheduler.scheduled(timer_id) {
            self.scheduler.schedule(
                EventPayload::new(RioEventType::Rio(RioEvent::UpdateTitles), unsafe {
                    rio_backend::event::WindowId::from(
                        rio_window::window::WindowId::dummy(),
                    )
                }),
                Duration::from_secs(2),
                true,
                timer_id,
            );
        }

        tracing::info!("Initialisation complete");
    }
}

impl Application<'_> {
    fn process_session_events(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: rio_backend::event::WindowId,
        route_id: usize,
    ) {
        let events = self
            .router
            .routes
            .get_mut(&window_id)
            .and_then(|route| {
                route
                    .window
                    .screen
                    .context_manager
                    .get_by_route_id(route_id)
                    .and_then(|context| {
                        context
                            .terminal
                            .lock()
                            .session()
                            .map(SessionHandle::take_events)
                    })
            })
            .unwrap_or_default();

        for event in events {
            self.handle_session_event(event_loop, window_id, route_id, event);
        }
        while let Some((kind, text, copy_to_clipboard)) =
            self.router.routes.get_mut(&window_id).and_then(|route| {
                route
                    .window
                    .screen
                    .context_manager
                    .get_by_route_id(route_id)
                    .and_then(|context| context.terminal.lock().take_selection_text())
            })
        {
            if let Some(text) = text {
                self.router.clipboard.set(kind, text);
                if copy_to_clipboard {
                    let text = self.router.clipboard.get(kind).to_string();
                    self.router.clipboard.set(ClipboardType::Clipboard, text);
                }
            }
        }
        if let Some(route) = self.router.routes.get_mut(&window_id) {
            route.window.screen.sync_session_search(route_id);
        }
    }

    fn handle_session_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: rio_backend::event::WindowId,
        route_id: usize,
        event: SessionEvent,
    ) {
        match event {
            SessionEvent::FrameReady => {
                unreachable!("frame-ready events are consumed by the session pump")
            }
            SessionEvent::Title { title } => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if let Some(context) = route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                    {
                        context.terminal.lock().title = title.clone();
                    }
                    if route.window.screen.ctx().current_route() == route_id {
                        route.set_window_title(&title);
                    }
                }
            }
            SessionEvent::Bell => {
                if self.config.bell.audio {
                    self.handle_audio_bell();
                }
            }
            SessionEvent::CursorBlinkingChanged => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if let Some(context) = route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                    {
                        context
                            .renderable_content
                            .pending_update
                            .set_terminal_damage(
                                rio_backend::event::TerminalDamage::CursorOnly,
                            );
                    }
                    route.request_redraw();
                }
            }
            SessionEvent::Progress { state, value } => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if let Some(island) = &mut route.window.screen.renderer.island {
                        let state = match state {
                            0 => rio_backend::event::ProgressState::Remove,
                            1 => rio_backend::event::ProgressState::Set,
                            2 => rio_backend::event::ProgressState::Error,
                            3 => rio_backend::event::ProgressState::Indeterminate,
                            4 => rio_backend::event::ProgressState::Pause,
                            _ => return,
                        };
                        island.set_progress_report(rio_backend::event::ProgressReport {
                            state,
                            progress: Some(value),
                        });
                        route.request_redraw();
                    }
                }
            }
            SessionEvent::ClipboardStore { kind, text } => {
                let Some(kind) = clipboard_type(kind) else {
                    return;
                };
                let focused = self
                    .router
                    .routes
                    .get(&window_id)
                    .is_some_and(|route| route.window.is_focused);
                if focused {
                    self.router.clipboard.set(kind, text);
                }
            }
            SessionEvent::ChildExited { .. } => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.request_redraw();
                }
            }
            SessionEvent::ClipboardOverflow => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.report_error(&rio_backend::error::RioError {
                        report: rio_backend::error::RioErrorType::InitializationError(
                            "terminal clipboard response exceeded the session limit"
                                .into(),
                        ),
                        level: rio_backend::error::RioErrorLevel::Warning,
                    });
                    route.request_redraw();
                }
            }
            SessionEvent::ClipboardLoad {
                request_id,
                route_id: worker_route_id,
                kind,
            } => {
                let Some(kind) = clipboard_type(kind) else {
                    return;
                };
                let text = self.router.clipboard.get(kind).to_string();
                self.send_session_command(
                    window_id,
                    route_id,
                    SessionCommand::ClipboardResponse {
                        request_id,
                        route_id: worker_route_id,
                        text,
                    },
                );
            }
            SessionEvent::ColorRequest {
                request_id,
                route_id: worker_route_id,
                index,
            } => {
                let renderer_color = self
                    .router
                    .routes
                    .get(&window_id)
                    .map(|route| route.window.screen.renderer.colors[index as usize]);
                let color = {
                    let Some(route) = self.router.routes.get_mut(&window_id) else {
                        return;
                    };
                    let Some(context) = route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                    else {
                        return;
                    };
                    context.terminal.lock().colors[index as usize]
                        .map(ColorRgb::from_color_arr)
                        .or_else(|| renderer_color.map(ColorRgb::from_color_arr))
                        .map(|color| [color.r, color.g, color.b])
                };
                self.send_session_command(
                    window_id,
                    route_id,
                    SessionCommand::ColorResponse {
                        request_id,
                        route_id: worker_route_id,
                        color,
                    },
                );
            }
            SessionEvent::TextAreaSizeRequest {
                request_id,
                route_id: worker_route_id,
            } => {
                let size = {
                    let Some(route) = self.router.routes.get_mut(&window_id) else {
                        return;
                    };
                    let Some(context) = route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                    else {
                        return;
                    };
                    crate::renderer::utils::terminal_dimensions(&context.dimension)
                };
                self.send_session_command(
                    window_id,
                    route_id,
                    SessionCommand::TextAreaSizeResponse {
                        request_id,
                        route_id: worker_route_id,
                        rows: size.rows,
                        columns: size.cols,
                        pixel_width: size.width,
                        pixel_height: size.height,
                    },
                );
            }
            SessionEvent::GlyphProtocolQuery {
                request_id,
                route_id: worker_route_id,
                codepoint,
            } => {
                let Some(route) = self.router.routes.get(&window_id) else {
                    return;
                };
                let library = route.window.screen.sugarloaf.font_library();
                let in_glossary = library
                    .glyph_registry_for(route_id)
                    .is_some_and(|registry| registry.contains(codepoint));
                let in_system = library.covers_codepoint(codepoint);
                let status = match (in_glossary, in_system) {
                    (true, true) => GlyphStatus::Both,
                    (true, false) => GlyphStatus::Glossary,
                    (false, true) => GlyphStatus::System,
                    (false, false) => GlyphStatus::Free,
                };
                self.send_session_command(
                    window_id,
                    route_id,
                    SessionCommand::GlyphProtocolResponse {
                        request_id,
                        route_id: worker_route_id,
                        status,
                    },
                );
            }
            SessionEvent::DesktopNotification { title, body } => {
                self.handle_desktop_notification(&title, &body);
            }
            SessionEvent::ColorChange {
                route_id: _worker_route_id,
                index,
                color,
            } => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if let Some(context) = route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                    {
                        use crate::context::renderable::BackgroundState;
                        if index == NamedColor::Foreground as u16 + 1 {
                            context.renderable_content.background = Some(match color {
                                Some([r, g, b]) => {
                                    BackgroundState::Set(ColorRgb { r, g, b }.to_wgpu())
                                }
                                None => BackgroundState::Reset,
                            });
                        }
                        context
                            .renderable_content
                            .pending_update
                            .set_terminal_damage(
                                rio_backend::event::TerminalDamage::Full,
                            );
                    }
                    route.request_redraw();
                }
            }
            SessionEvent::RequestRefused {
                request_id,
                kind,
                reason,
            } => {
                self.show_session_error(
                    window_id,
                    route_id,
                    format!(
                        "terminal request {request_id} ({kind:?}) refused: {reason:?}"
                    ),
                );
            }
            SessionEvent::RequestExpired { request_id, kind } => {
                self.show_session_error(
                    window_id,
                    route_id,
                    format!("terminal request {request_id} ({kind:?}) expired"),
                );
            }
            SessionEvent::Closed => {
                self.close_terminal_at(event_loop, window_id, route_id);
            }
        }
    }

    fn send_session_command(
        &mut self,
        window_id: rio_backend::event::WindowId,
        route_id: usize,
        command: SessionCommand,
    ) {
        let Some(route) = self.router.routes.get_mut(&window_id) else {
            return;
        };
        let Some(context) = route
            .window
            .screen
            .context_manager
            .get_by_route_id(route_id)
        else {
            return;
        };
        let result = context
            .terminal
            .lock()
            .session()
            .map(|session| session.enqueue(command));
        if let Some(Err(error)) = result {
            context.renderable_content.session_error = Some(error.to_string());
            context
                .renderable_content
                .pending_update
                .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
        }
    }

    fn show_session_error(
        &mut self,
        window_id: rio_backend::event::WindowId,
        route_id: usize,
        error: String,
    ) {
        if let Some(route) = self.router.routes.get_mut(&window_id) {
            if let Some(context) = route
                .window
                .screen
                .context_manager
                .get_by_route_id(route_id)
            {
                context.renderable_content.session_error = Some(error);
                context
                    .renderable_content
                    .pending_update
                    .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
            }
            route.request_redraw();
        }
    }
}

impl ApplicationHandler<EventPayload> for Application<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        Application::resumed(self, event_loop);
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        Application::new_events(self, event_loop, cause);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: EventPayload) {
        let target = event.target.clone();
        let window_id = event.window_id();
        #[cfg(all(feature = "wayland", target_os = "linux"))]
        if matches!(
            &event.payload,
            RioEventType::Rio(RioEvent::CreateConfigEditor)
        ) && self.tab_drag.as_ref().is_some_and(|drag| {
            rio_backend::event::WindowId::from(drag.source_window) == window_id
                || drag.owner.as_ref().is_some_and(|owner| {
                    rio_backend::event::WindowId::from(owner.window_id) == window_id
                })
        }) {
            return;
        }
        // Prepared attachments must produce their first frame before commit,
        // even when the source window covers the destination.
        let preparing_session = self
            .pending_prepared
            .iter()
            .any(|pending| pending.window_id == window_id);
        if preparing_session
            && matches!(
                &event.payload,
                RioEventType::Rio(RioEvent::Render | RioEvent::RenderRoute(_))
            )
        {
            if let Some(route) = self.router.routes.get_mut(&window_id) {
                let ready = route.window.screen.prepare_session_imports();
                tracing::info!(
                    window_id = ?window_id,
                    ready_routes = ?ready,
                    "prepared pending session imports"
                );
                if !ready.is_empty() {
                    self.ready_session_imports.push((window_id, ready));
                }
            }
        }
        match event.payload {
            RioEventType::Rio(RioEvent::Render) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    // Skip rendering for unfocused windows if configured
                    if !preparing_session
                        && self.config.renderer.disable_unfocused_render
                        && !route.window.is_focused
                    {
                        return;
                    }

                    // Skip rendering for occluded windows if configured, unless we need to render after occlusion
                    if !preparing_session
                        && self.config.renderer.disable_occluded_render
                        && route.window.is_occluded
                        && !route.window.needs_render_after_occlusion
                    {
                        return;
                    }

                    // Clear the one-time render flag if it was set
                    if route.window.needs_render_after_occlusion {
                        route.window.needs_render_after_occlusion = false;
                    }

                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::RenderRoute(route_id)) => {
                // SessionPump delivers all worker-originated events through
                // the existing route wakeup. Drain them before applying the
                // render throttling policy so callback requests are answered
                // even for an unfocused or occluded pane.
                self.process_session_events(event_loop, window_id, route_id);
                if self.config.renderer.strategy.is_event_based() {
                    if let Some(route) = self.router.routes.get_mut(&window_id) {
                        // Skip rendering for unfocused windows if configured
                        if !preparing_session
                            && self.config.renderer.disable_unfocused_render
                            && !route.window.is_focused
                        {
                            if route.window.screen.renderer.scrollbar.needs_redraw() {
                                route.request_redraw();
                            }
                            return;
                        }

                        // Skip rendering for occluded windows if configured, unless we need to render after occlusion
                        if !preparing_session
                            && self.config.renderer.disable_occluded_render
                            && route.window.is_occluded
                            && !route.window.needs_render_after_occlusion
                        {
                            return;
                        }

                        // Clear the one-time render flag if it was set
                        if route.window.needs_render_after_occlusion {
                            route.window.needs_render_after_occlusion = false;
                        }

                        // Mark the renderable content as needing to render
                        if let Some(context) =
                            route.window.screen.ctx_mut().get_by_route_id(route_id)
                        {
                            context.renderable_content.pending_update.set_dirty();
                        }

                        // Check if we need to throttle based on timing
                        if let Some(wait_duration) = route.window.wait_until() {
                            // We need to wait before rendering again
                            let timer_id = TimerId::new(Topic::RenderRoute, route_id);
                            let event = EventPayload::new(
                                RioEventType::Rio(RioEvent::Render),
                                target.clone(),
                            );

                            // Only schedule if not already scheduled
                            if !self.scheduler.scheduled(timer_id) {
                                self.scheduler.schedule(
                                    event,
                                    wait_duration,
                                    false,
                                    timer_id,
                                );
                            }
                        } else {
                            // We can render immediately
                            route.request_redraw();
                        }
                    }
                }
            }

            RioEventType::Rio(RioEvent::TerminalDamaged(route_id)) => {
                if self.config.renderer.strategy.is_event_based() {
                    if let Some(route) = self.router.routes.get_mut(&window_id) {
                        if self.config.renderer.disable_unfocused_render
                            && !route.window.is_focused
                        {
                            return;
                        }
                        if self.config.renderer.disable_occluded_render
                            && route.window.is_occluded
                            && !route.window.needs_render_after_occlusion
                        {
                            return;
                        }

                        if let Some(context) =
                            route.window.screen.ctx_mut().get_by_route_id(route_id)
                        {
                            // Just mark dirty — damage will be extracted from
                            // the terminal when the renderer locks it.
                            context.renderable_content.pending_update.set_dirty();
                            route.request_redraw();
                        }
                    }
                }
            }
            RioEventType::Rio(RioEvent::UpdateGraphics { route_id, queues }) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    // A batch the VT thread queued before the user
                    // closed its tab or split would otherwise land
                    // under a route nothing will ever release.
                    if route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                        .is_none()
                    {
                        return;
                    }

                    // Process graphics directly in sugarloaf
                    let sugarloaf = &mut route.window.screen.sugarloaf;

                    // Removals arrive as final image keys (atlas refs
                    // dropped off scrollback, kitty evictions) and free
                    // both the pixel store and the cached GPU texture. Apply
                    // them first so a same-frame re-upload wins below.
                    for key in queues.remove_queue {
                        sugarloaf.remove_image(rio_backend::sugarloaf::GraphicKey::new(
                            route_id, key,
                        ));
                    }

                    // Atlas graphics (sixel/iTerm2) share the per-image
                    // texture store with kitty images, in a disjoint key
                    // namespace.
                    for graphic_data in queues.pending {
                        let key = rio_backend::sugarloaf::GraphicKey::new(
                            route_id,
                            crate::renderer::atlas_image_key(graphic_data.id.get()),
                        );
                        sugarloaf.image_data.insert(
                            key,
                            rio_backend::sugarloaf::GraphicDataEntry::from_graphic_data(
                                graphic_data,
                            ),
                        );
                    }

                    // Image textures (kitty) → separate store, no clone
                    for (image_id, graphic_data) in queues.pending_images {
                        sugarloaf.image_data.insert(
                            rio_backend::sugarloaf::GraphicKey::new(
                                route_id,
                                crate::renderer::kitty_image_key(image_id),
                            ),
                            rio_backend::sugarloaf::GraphicDataEntry::from_graphic_data(
                                graphic_data,
                            ),
                        );
                    }

                    // Mark the panel dirty: the renderer skips non-dirty
                    // panels, so a bare redraw after the pixels arrive
                    // would no-op and leave the image blank until the
                    // next unrelated damage.
                    if let Some(context) =
                        route.window.screen.ctx_mut().get_by_route_id(route_id)
                    {
                        context.renderable_content.pending_update.set_dirty();
                    }

                    // Request a redraw to display the updated graphics
                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::PrepareUpdateConfig) => {
                let timer_id = TimerId::new(Topic::UpdateConfig, 0);
                let event = EventPayload::new(
                    RioEventType::Rio(RioEvent::UpdateConfig),
                    target.clone(),
                );

                if !self.scheduler.scheduled(timer_id) {
                    self.scheduler.schedule(
                        event,
                        Duration::from_millis(250),
                        false,
                        timer_id,
                    );
                }
            }
            RioEventType::Rio(RioEvent::ReportToAssistant(error)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.report_error(&error);
                }
            }
            RioEventType::Rio(RioEvent::UpdateConfig) => {
                // A config.toml typo saved mid-edit must not reset the
                // live config (fonts, colors, bindings) to defaults: keep
                // running on the current one, surface the error, and pick
                // up the next successful save. Startup differs — there is
                // nothing live to keep, so main's load still falls back to
                // defaults.
                let config = match rio_backend::config::Config::try_load() {
                    Ok(config) => config,
                    Err(error) => {
                        tracing::warn!(
                            "config.toml failed to parse; keeping the previous config: {error:?}"
                        );
                        for route in self.router.routes.values_mut() {
                            route.report_error(&error.to_owned().into());
                            route.request_redraw();
                        }
                        return;
                    }
                };
                let has_font_updates = self.config.fonts != config.fonts;
                let has_binding_updates = self.config.bindings != config.bindings;

                let font_library_errors = if has_font_updates {
                    self.router.font_library.reload(config.fonts.to_owned())
                } else {
                    None
                };

                self.config = config;

                // Dropping the old manager unregisters its hotkeys, so
                // ToggleQuake binding edits apply without restarting.
                if has_binding_updates {
                    self.setup_quake_hotkey();
                }

                let mut has_checked_adaptive_colors = false;
                for route in self.router.routes.values_mut() {
                    route.clear_errors();
                    // Apply system theme to ensure colors are consistent
                    if !has_checked_adaptive_colors {
                        let system_theme = event_loop.system_theme();
                        let theme = self
                            .config
                            .force_theme
                            .map(|t| t.to_window_theme())
                            .or(system_theme);
                        update_colors_based_on_theme(&mut self.config, theme);
                        has_checked_adaptive_colors = true;
                    }

                    if has_font_updates {
                        if let Some(ref err) = font_library_errors {
                            route
                                .window
                                .screen
                                .context_manager
                                .report_error_fonts_not_found(
                                    err.fonts_not_found.clone(),
                                );
                        }
                    }

                    route.update_config(
                        &self.config,
                        &self.router.font_library,
                        has_font_updates,
                    );
                    route.window.configure_window(&self.config);

                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::Exit | RioEvent::Quit) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if self.config.confirm_before_quit {
                        route.confirm_quit();
                    } else {
                        route.quit();
                    }
                }
            }
            RioEventType::Rio(RioEvent::GlyphProtocolInstalled {
                route_id,
                registry,
            }) => {
                if let Some(route) = self.router.routes.get(&window_id) {
                    route
                        .window
                        .screen
                        .sugarloaf
                        .font_library()
                        .install_glyph_registry(route_id, registry);
                }
            }
            RioEventType::Rio(RioEvent::GlyphProtocolQuery { route_id, cp }) => {
                tracing::warn!(
                    route_id,
                    cp,
                    "ignored legacy glyph query; session workers use SessionEvent::GlyphProtocolQuery"
                );
            }
            RioEventType::Rio(RioEvent::CloseTerminal(route_id)) => {
                #[cfg(all(feature = "wayland", target_os = "linux"))]
                if self.tab_drag.as_ref().is_some_and(|drag| {
                    deferred_window_close(drag, window_id.into()).is_some()
                        || drag
                            .owner
                            .as_ref()
                            .is_some_and(|owner| owner.route_ids.contains(&route_id))
                }) {
                    self.defer_tab_drag_close(DeferredTabDragClose::Terminal(route_id));
                    return;
                }
                self.close_terminal_at(event_loop, window_id, route_id);
            }
            RioEventType::Rio(RioEvent::CursorBlinkingChange) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::CursorBlinkingChangeOnRoute(route_id)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if route_id == route.window.screen.ctx().current_route() {
                        // Cursor blink toggles the cursor sprite (a
                        // separate quad), not cell content — so we
                        // signal `CursorOnly` and the GPU emit skips
                        // per-row rebuild while the cursor uniform
                        // updates downstream.
                        route
                            .window
                            .screen
                            .ctx_mut()
                            .current_mut()
                            .renderable_content
                            .pending_update
                            .set_terminal_damage(
                                rio_backend::event::TerminalDamage::CursorOnly,
                            );

                        route.request_redraw();
                    }
                }
            }
            RioEventType::Rio(RioEvent::ProgressReport(report)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if let Some(island) = &mut route.window.screen.renderer.island {
                        island.set_progress_report(report);
                        route.request_redraw();
                    }
                }
            }
            RioEventType::Rio(RioEvent::SelectionScrollTick) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.screen.selection_scroll_tick();
                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::Bell) => {
                // Handle audio bell
                if self.config.bell.audio {
                    self.handle_audio_bell();
                }
            }
            RioEventType::Rio(RioEvent::DesktopNotification { title, body }) => {
                self.handle_desktop_notification(&title, &body);
            }
            RioEventType::Rio(RioEvent::PrepareRender(millis)) => {
                if let Some(route) = self.router.routes.get(&window_id) {
                    let timer_id = TimerId::new(
                        Topic::Render,
                        route.window.screen.ctx().current_route(),
                    );
                    let event = EventPayload::new(
                        RioEventType::Rio(RioEvent::Render),
                        target.clone(),
                    );

                    if !self.scheduler.scheduled(timer_id) {
                        self.scheduler.schedule(
                            event,
                            Duration::from_millis(millis),
                            false,
                            timer_id,
                        );
                    }
                }
            }
            RioEventType::Rio(RioEvent::PrepareRenderOnRoute(millis, route_id)) => {
                let timer_id = TimerId::new(Topic::ScheduledRenderRoute, route_id);
                let event = EventPayload::new(
                    RioEventType::Rio(RioEvent::RenderRoute(route_id)),
                    target.clone(),
                );

                if !self.scheduler.scheduled(timer_id) {
                    self.scheduler.schedule(
                        event,
                        Duration::from_millis(millis),
                        false,
                        timer_id,
                    );
                }
            }
            RioEventType::Rio(RioEvent::BlinkCursor(millis, route_id)) => {
                let timer_id = TimerId::new(Topic::CursorBlinking, route_id);
                let event = EventPayload::new(
                    RioEventType::Rio(RioEvent::CursorBlinkingChangeOnRoute(route_id)),
                    target,
                );

                if !self.scheduler.scheduled(timer_id) {
                    self.scheduler.schedule(
                        event,
                        Duration::from_millis(millis),
                        false,
                        timer_id,
                    );
                }
            }
            RioEventType::Rio(RioEvent::Title(route_id, title)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if route.window.screen.context_manager.current().route_id == route_id
                    {
                        route.set_window_title(&title);
                    }
                }
            }
            RioEventType::Rio(RioEvent::UpdateTitles) => {
                self.router.update_titles();
            }
            RioEventType::Rio(RioEvent::MouseCursorDirty) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.screen.reset_mouse();
                }
            }
            RioEventType::Rio(RioEvent::Scroll(scroll)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    let mut terminal = route
                        .window
                        .screen
                        .context_manager
                        .current_mut()
                        .terminal
                        .lock();
                    terminal.scroll_display(scroll);
                    drop(terminal);
                    route.window.screen.refresh_hints_after_scroll();
                    route
                        .window
                        .winit_window
                        .set_cursor(route.window.screen.mouse_cursor_icon());
                }
            }
            RioEventType::Rio(RioEvent::ClipboardLoad(
                route_id,
                clipboard_type,
                format,
            )) => {
                let _ = (route_id, clipboard_type, format, window_id);
                tracing::warn!(
                    "ignored legacy clipboard request; session workers use SessionEvent"
                );
            }
            RioEventType::Rio(RioEvent::ClipboardStore(clipboard_type, content)) => {
                let Router {
                    routes, clipboard, ..
                } = &mut self.router;
                if let Some(route) = routes.get_mut(&window_id) {
                    if route.window.is_focused {
                        clipboard.set(clipboard_type, content);
                    }
                }
            }
            RioEventType::Rio(RioEvent::PtyWrite(route_id, text)) => {
                let _ = (route_id, text, window_id);
                tracing::warn!(
                    "ignored legacy PTY reply; session workers own terminal responses"
                );
            }
            RioEventType::Rio(RioEvent::TextAreaSizeRequest(route_id, format)) => {
                let _ = (route_id, format, window_id);
                tracing::warn!(
                    "ignored legacy size request; session workers use SessionEvent"
                );
            }
            RioEventType::Rio(RioEvent::ColorRequest(route_id, index, format)) => {
                let _ = (route_id, index, format, window_id);
                tracing::warn!(
                    "ignored legacy color request; session workers use SessionEvent"
                );
            }
            RioEventType::Rio(RioEvent::CreateWindow) => {
                self.router.create_window(
                    event_loop,
                    self.event_proxy.clone(),
                    &self.config,
                    None,
                    self.app_id.as_deref(),
                );
            }
            RioEventType::Rio(RioEvent::MoveCurrentTabToNewWindow) => {
                let Some(tab_id) = self.router.routes.get(&window_id).and_then(|route| {
                    route
                        .window
                        .screen
                        .context_manager
                        .current_grid_opt()
                        .map(|grid| grid.id())
                }) else {
                    tracing::warn!("current tab disappeared before detaching");
                    return;
                };
                let _ = self.move_tab_to_new_window(event_loop, window_id, tab_id);
            }
            RioEventType::Rio(RioEvent::MergeWindow) => {
                let selected_recovery_target = self
                    .router
                    .routes
                    .get_mut(&window_id)
                    .and_then(|route| route.window.screen.take_recovery_target());
                if let Some(target_index) = selected_recovery_target {
                    let _ = self.select_recovery_target(window_id, target_index);
                    return;
                }
                let recovery_requested =
                    self.router.routes.get_mut(&window_id).is_some_and(|route| {
                        route.window.screen.take_recovery_action_request()
                    });
                self.clear_merge_window();
                if recovery_requested {
                    let _ = self.begin_recovery_action(window_id);
                    return;
                }
                if !self.arm_merge_source(window_id) {
                    return;
                }
                let _ = self.begin_merge_window_action(window_id);
            }
            RioEventType::Rio(RioEvent::ToggleQuake) => {
                self.toggle_quake_window(event_loop);
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::CreateNativeTab(working_dir_overwrite)) => {
                if let Some(route) = self.router.routes.get(&window_id) {
                    // This case happens only for native tabs
                    // every time that a new tab is created through context
                    // it also reaches for the foreground process path if
                    // config.use_current_path is true
                    // For these case we need to make a workaround
                    let config = if working_dir_overwrite.is_some() {
                        rio_backend::config::Config {
                            working_dir: working_dir_overwrite,
                            ..self.config.clone()
                        }
                    } else {
                        self.config.clone()
                    };

                    self.router.create_native_tab(
                        event_loop,
                        self.event_proxy.clone(),
                        &config,
                        Some(&route.window.winit_window.tabbing_identifier()),
                        None,
                    );
                }
            }
            RioEventType::Rio(RioEvent::CreateConfigEditor) => {
                if self.config.navigation.open_config_with_split {
                    self.router.open_config_split(&self.config);
                } else {
                    self.router.open_config_window(
                        event_loop,
                        self.event_proxy.clone(),
                        &self.config,
                    );
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::CloseWindow) => {
                if let Some(route) = self.router.routes.get(&window_id) {
                    for route_id in route.window.screen.context_manager.route_ids() {
                        self.scheduler.unschedule_window(route_id);
                    }
                }
                let closing_sessions = self
                    .router
                    .routes
                    .get(&window_id)
                    .map(|route| route.window.screen.context_manager.session_handles())
                    .unwrap_or_default();
                self.clear_merge_if_window(window_id);
                self.pending_session_closes.extend(closing_sessions);
                self.router.remove_route(window_id);
                if self.router.routes.is_empty() && !self.config.confirm_before_quit {
                    self.defer_exit_until_session_closes();
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::SelectNativeTabByIndex(tab_index)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.winit_window.select_tab_at_index(tab_index);
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::SelectNativeTabLast) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route
                        .window
                        .winit_window
                        .select_tab_at_index(route.window.winit_window.num_tabs() - 1);
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::SelectNativeTabNext) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.winit_window.select_next_tab();
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::SelectNativeTabPrev) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.winit_window.select_previous_tab();
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::Hide) => {
                event_loop.hide_application();
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::HideOtherApplications) => {
                event_loop.hide_other_applications();
            }
            RioEventType::Rio(RioEvent::Minimize(set_minimize)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.winit_window.set_minimized(set_minimize);
                }
            }
            RioEventType::Rio(RioEvent::ToggleFullScreen) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    match route.window.winit_window.fullscreen() {
                        None => route
                            .window
                            .winit_window
                            .set_fullscreen(Some(Fullscreen::Borderless(None))),
                        _ => route.window.winit_window.set_fullscreen(None),
                    }
                }
            }
            RioEventType::Rio(RioEvent::ToggleAppearanceTheme) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    use rio_backend::config::theme::AppearanceTheme;
                    let current = self
                        .config
                        .force_theme
                        .or_else(|| {
                            route
                                .window
                                .winit_window
                                .theme()
                                .map(AppearanceTheme::from_window_theme)
                        })
                        .unwrap_or(AppearanceTheme::Dark);
                    let toggled = current.toggled();
                    self.config.force_theme = Some(toggled);
                    update_colors_based_on_theme(
                        &mut self.config,
                        Some(toggled.to_window_theme()),
                    );
                    route.window.screen.update_config(
                        &self.config,
                        &self.router.font_library,
                        false,
                    );
                    route.window.configure_window(&self.config);
                }
            }
            RioEventType::Rio(RioEvent::ColorChange(route_id, index, color)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    let screen = &mut route.window.screen;
                    // Background color is index 1 relative to NamedColor::Foreground
                    if index == NamedColor::Foreground as usize + 1 {
                        if let Some(context) =
                            screen.context_manager.get_by_route_id(route_id)
                        {
                            use crate::context::renderable::BackgroundState;
                            context.renderable_content.background = Some(match color {
                                Some(c) => BackgroundState::Set(c.to_wgpu()),
                                None => BackgroundState::Reset,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    #[cfg(target_os = "macos")]
    fn open_urls(&mut self, active_event_loop: &ActiveEventLoop, urls: Vec<String>) {
        if !self.config.navigation.is_native() {
            let config = &self.config;
            for url in urls {
                self.router.create_window(
                    active_event_loop,
                    self.event_proxy.clone(),
                    config,
                    Some(url),
                    self.app_id.as_deref(),
                );
            }
            return;
        }

        let mut tab_id = None;

        // In case only have one window
        for (_, route) in self.router.routes.iter() {
            if tab_id.is_none() {
                tab_id = Some(route.window.winit_window.tabbing_identifier());
            }

            if route.window.is_focused {
                tab_id = Some(route.window.winit_window.tabbing_identifier());
                break;
            }
        }

        if tab_id.is_some() {
            let config = &self.config;
            for url in urls {
                self.router.create_native_tab(
                    active_event_loop,
                    self.event_proxy.clone(),
                    config,
                    tab_id.as_deref(),
                    Some(url),
                );
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // Ignore all events we do not care about.
        if Self::skip_window_event(&event) {
            return;
        }

        #[cfg(all(feature = "wayland", target_os = "linux"))]
        let event = match event {
            WindowEvent::ToplevelDrag(drag_event) => {
                self.handle_toplevel_drag_event(event_loop, window_id, drag_event);
                return;
            }
            event => event,
        };

        #[cfg(all(feature = "wayland", target_os = "linux"))]
        if matches!(event, WindowEvent::CloseRequested) {
            let deferred = self
                .tab_drag
                .as_ref()
                .and_then(|drag| deferred_window_close(drag, window_id));
            if let Some(deferred) = deferred {
                self.defer_tab_drag_close(deferred);
                return;
            }
        }

        #[cfg(all(feature = "wayland", target_os = "linux"))]
        if self
            .tab_drag
            .as_ref()
            .is_some_and(|drag| deferred_window_close(drag, window_id).is_some())
            && matches!(
                event,
                WindowEvent::KeyboardInput { .. } | WindowEvent::MouseInput { .. }
            )
        {
            return;
        }

        // The event loop keys on rio-window's id; the router keys on the
        // core's `WindowId`. Convert once at this boundary.
        let window_id: rio_backend::event::WindowId = window_id.into();

        if self.handle_armed_merge_window_event(window_id, &event) {
            return;
        }

        if matches!(
            &event,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            }
        ) {
            if let Some(source_id) = self.merge_window_source {
                let index = self
                    .merge_target
                    .filter(|(target, _)| *target == window_id)
                    .map(|(_, index)| index);
                self.clear_merge_window();
                if source_id != window_id {
                    self.merge_window_into_target(source_id, window_id, index);
                    return;
                }
            }
        }

        if matches!(&event, WindowEvent::CloseRequested) {
            self.clear_merge_if_window(window_id);
            self.perform_close_requested(event_loop, window_id);
            return;
        }

        if let WindowEvent::CursorMoved { position, .. } = &event {
            self.update_merge_target(window_id, *position);
        }

        // Escape normally belongs to the command palette. Once native merge
        // targeting is armed, it must also cancel the authenticated target so
        // the palette cannot hide while the remote overlay remains active.
        let escape_pressed = matches!(
            &event,
            WindowEvent::KeyboardInput {
                event: key_event,
                ..
            } if key_event.state == ElementState::Pressed
                && key_event.logical_key
                    == rio_window::keyboard::Key::Named(
                        rio_window::keyboard::NamedKey::Escape,
                    )
        );
        if escape_pressed && self.merge_state_uses_window(window_id) {
            self.clear_merge_window();
            if let Some(route) = self.router.routes.get_mut(&window_id) {
                route.request_redraw();
            }
            return;
        }

        let route = match self.router.routes.get_mut(&window_id) {
            Some(window) => window,
            None => return,
        };

        if route.path == RoutePath::Terminal
            && route.window.screen.context_manager.is_empty()
            && matches!(
                &event,
                WindowEvent::CursorMoved { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::KeyboardInput { .. }
            )
        {
            if let WindowEvent::MouseInput {
                state: ElementState::Released,
                button,
                ..
            } = &event
            {
                match button {
                    MouseButton::Left => {
                        route.window.screen.mouse.left_button_state =
                            ElementState::Released
                    }
                    MouseButton::Middle => {
                        route.window.screen.mouse.middle_button_state =
                            ElementState::Released
                    }
                    MouseButton::Right => {
                        route.window.screen.mouse.right_button_state =
                            ElementState::Released
                    }
                    _ => {}
                }
                if let Some(island) = route.window.screen.renderer.island.as_mut() {
                    island.cancel_drag();
                }
                route.window.screen.renderer.scrollbar.end_drag();
                route.window.screen.resize_state = None;
            }
            return;
        }

        match event {
            WindowEvent::ModifiersChanged(modifiers) => {
                route.window.screen.set_modifiers(modifiers);

                // Hint mods (cmd on macOS) are pressed with the pointer
                // already parked over the link, and `CursorMoved` only
                // recomputes hints when the pointer crosses a cell
                // boundary. Without refreshing here the link is never
                // highlighted, so the click that follows has nothing to
                // activate. The set half only runs with the pointer inside
                // the text area (a clamped chrome position must not light
                // up a link it is not over), but the clear half always
                // runs: a highlight left behind would outlive its modifier
                // and hijack the next plain click.
                if route.path == RoutePath::Terminal
                    && (if route.window.screen.mouse.inside_text_area {
                        route.window.screen.update_highlighted_hints()
                    } else {
                        route.window.screen.clear_highlighted_hint()
                    })
                {
                    route
                        .window
                        .winit_window
                        .set_cursor(route.window.screen.mouse_cursor_icon());
                    route.window.screen.context_manager.request_render();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if route.path != RoutePath::Terminal
                    || route.window.screen.renderer.confirm_quit.is_active()
                {
                    #[cfg(target_os = "macos")]
                    if state == ElementState::Pressed
                        && button == MouseButton::Left
                        && route.window.screen.allow_manual_dragging
                    {
                        if route
                            .window
                            .screen
                            .tab_bar_contains_y(route.window.screen.mouse.y)
                        {
                            let _ = route.window.winit_window.drag_window();
                        }
                    }
                    if state == ElementState::Pressed {
                        let _ = route.window.screen.take_chrome_press();
                    } else if state == ElementState::Released
                        && button == MouseButton::Left
                    {
                        route.window.screen.mouse.left_button_state =
                            ElementState::Released;
                        // A release swallowed here must also drop the hint
                        // latch, or a later chrome-consumed press would
                        // release against a hint it never landed on.
                        route.window.screen.mouse.hint_click_latched = None;
                        if let Some(ref mut island) = route.window.screen.renderer.island
                        {
                            island.cancel_drag();
                        }
                        route.window.screen.renderer.scrollbar.end_drag();
                        route.window.screen.resize_state = None;
                    }
                    return;
                }

                if self.config.hide_cursor_when_typing {
                    route.window.winit_window.set_cursor_visible(true);
                }

                match button {
                    MouseButton::Left => {
                        route.window.screen.mouse.left_button_state = state
                    }
                    MouseButton::Middle => {
                        route.window.screen.mouse.middle_button_state = state
                    }
                    MouseButton::Right => {
                        route.window.screen.mouse.right_button_state = state
                    }
                    _ => (),
                }

                match state {
                    ElementState::Pressed => {
                        // Calculate time since the last click to handle double/triple clicks.
                        // Do this early so island clicks can use the click state
                        let now = Instant::now();
                        let elapsed =
                            now - route.window.screen.mouse.last_click_timestamp;
                        route.window.screen.mouse.last_click_timestamp = now;

                        let threshold = crate::constants::MULTI_CLICK_THRESHOLD;
                        let mouse = &route.window.screen.mouse;
                        route.window.screen.mouse.click_state = match mouse.click_state {
                            // Reset click state if button has changed.
                            _ if button != mouse.last_click_button => {
                                route.window.screen.mouse.last_click_button = button;
                                ClickState::Click
                            }
                            ClickState::Click if elapsed < threshold => {
                                ClickState::DoubleClick
                            }
                            ClickState::DoubleClick if elapsed < threshold => {
                                ClickState::TripleClick
                            }
                            _ => ClickState::Click,
                        };

                        let chrome_press = route.window.screen.take_chrome_press();

                        if route.dismiss_assistant() {
                            route.request_redraw();
                            return;
                        }

                        if let MouseButton::Left = button {
                            // Check if clicking on a panel border to start resize
                            {
                                let mx = route.window.screen.mouse.x as f32;
                                let my = route.window.screen.mouse.y as f32;
                                let grid =
                                    route.window.screen.context_manager.current_grid();
                                if let Some(border) = grid.find_border_at_position(mx, my)
                                {
                                    let start_pos = match border.direction {
                                        crate::layout::BorderDirection::Vertical => mx,
                                        crate::layout::BorderDirection::Horizontal => my,
                                    };
                                    let size_a = grid.get_panel_size(
                                        border.left_or_top,
                                        border.direction,
                                    );
                                    let size_b = grid.get_panel_size(
                                        border.right_or_bottom,
                                        border.direction,
                                    );
                                    route.window.screen.resize_state =
                                        Some(crate::layout::ResizeState {
                                            border,
                                            start_pos,
                                            original_sizes: (size_a, size_b),
                                        });
                                    return;
                                }
                            }

                            if route
                                .window
                                .screen
                                .handle_palette_click(&mut self.router.clipboard)
                            {
                                route.request_redraw();
                                return;
                            }

                            if route
                                .window
                                .screen
                                .handle_search_click(&mut self.router.clipboard)
                            {
                                route.request_redraw();
                                return;
                            }

                            let handled_by_island =
                                route.window.screen.handle_island_click(
                                    &route.window.winit_window,
                                    &mut self.router.clipboard,
                                    false,
                                    chrome_press,
                                );

                            if handled_by_island {
                                route.request_redraw();
                                return;
                            }

                            #[cfg(target_os = "macos")]
                            if route.window.screen.allow_manual_dragging {
                                if route
                                    .window
                                    .screen
                                    .tab_bar_contains_y(route.window.screen.mouse.y)
                                {
                                    route
                                        .window
                                        .screen
                                        .start_window_drag(&route.window.winit_window);
                                }
                            }

                            if route.window.screen.handle_scrollbar_click() {
                                route.request_redraw();
                                return;
                            }
                        } else if let MouseButton::Right = button {
                            let handled_by_island =
                                route.window.screen.handle_island_click(
                                    &route.window.winit_window,
                                    &mut self.router.clipboard,
                                    true,
                                    chrome_press,
                                );

                            if handled_by_island {
                                route.request_redraw();
                                return;
                            }
                        }

                        // Always try panel switching first: if the click
                        // targets a different panel, switch to it regardless
                        // of mouse mode (e.g. neovim capturing clicks).
                        //
                        // A left click on a highlighted hint bypasses mouse
                        // reporting the same way shift does: the hint's mods
                        // are held, so the user is following the link, not
                        // clicking inside the application. The hint itself is
                        // latched for the release handler: re-evaluating
                        // there would split a press from its release when
                        // the modifier changes mid-click, and the release
                        // must know which hint the press landed on.
                        let latched = if button == MouseButton::Left {
                            route.window.screen.highlighted_hint().cloned()
                        } else {
                            None
                        };
                        let hint_click = latched.is_some();
                        if button == MouseButton::Left {
                            route.window.screen.mouse.hint_click_latched = latched;
                        }

                        if route.window.screen.select_current_based_on_mouse() {
                            route.request_redraw();
                        } else if !route.window.screen.modifiers.state().shift_key()
                            && !hint_click
                            && route.window.screen.mouse_mode()
                        {
                            // Process mouse press before bindings to update the `click_state`.
                            route.window.screen.mouse.click_state = ClickState::None;

                            let code = match button {
                                MouseButton::Left => 0,
                                MouseButton::Middle => 1,
                                MouseButton::Right => 2,
                                // Can't properly report more than three buttons..
                                MouseButton::Back
                                | MouseButton::Forward
                                | MouseButton::Other(_) => return,
                            };

                            route
                                .window
                                .screen
                                .mouse_report(code, ElementState::Pressed);

                            route.window.screen.process_mouse_bindings(
                                button,
                                &mut self.router.clipboard,
                            );
                        } else {
                            // Load mouse point, treating message bar and padding as the closest square.
                            let display_offset = route.window.screen.display_offset();

                            if let MouseButton::Left = button {
                                let pos =
                                    route.window.screen.mouse_position(display_offset);
                                route
                                    .window
                                    .screen
                                    .on_left_click(pos, &mut self.router.clipboard);
                            }

                            route.request_redraw();
                        }
                        route
                            .window
                            .screen
                            .process_mouse_bindings(button, &mut self.router.clipboard);
                    }
                    ElementState::Released => {
                        // Stop selection auto-scroll on button release.
                        if let MouseButton::Left | MouseButton::Right = button {
                            let scroll_timer_id =
                                route.window.screen.ctx().current_route();
                            let timer_id =
                                TimerId::new(Topic::SelectionScrolling, scroll_timer_id);
                            self.scheduler.unschedule(timer_id);
                        }

                        if button == MouseButton::Left
                            && route
                                .window
                                .screen
                                .renderer
                                .island
                                .as_ref()
                                .is_some_and(|i| i.is_dragging())
                        {
                            let started = route.window.screen.handle_tab_drag_release();
                            if started {
                                route.request_redraw();
                                return;
                            }
                        }

                        if route.window.screen.renderer.scrollbar.is_dragging() {
                            route.window.screen.handle_scrollbar_release();
                            route.request_redraw();
                            return;
                        }

                        if route.window.screen.resize_state.is_some() {
                            route.window.screen.resize_state = None;
                            route.window.winit_window.set_cursor(CursorIcon::Default);
                            return;
                        }

                        // Consume the press handler's latched hint so press
                        // and release always take the same path, even when
                        // the hint modifier changed mid-click. The
                        // application never sees one without the other.
                        let latched_hint = if button == MouseButton::Left {
                            route.window.screen.mouse.hint_click_latched.take()
                        } else {
                            None
                        };
                        let hint_click = latched_hint.is_some();

                        if !route.window.screen.modifiers.state().shift_key()
                            && !hint_click
                            && route.window.screen.mouse_mode()
                        {
                            let code = match button {
                                MouseButton::Left => 0,
                                MouseButton::Middle => 1,
                                MouseButton::Right => 2,
                                // Can't properly report more than three buttons.
                                MouseButton::Back
                                | MouseButton::Forward
                                | MouseButton::Other(_) => return,
                            };
                            route
                                .window
                                .screen
                                .mouse_report(code, ElementState::Released);
                            return;
                        }

                        // Releasing a drag selection copies it (with
                        // copy-on-select) and must not activate a hint
                        // sitting under the release point; hints fire on
                        // plain clicks only, when no selection exists.
                        if route.window.screen.selection_is_empty() {
                            if button == MouseButton::Left {
                                // Only a latched press opens a link, and only
                                // when the release lands on the same span the
                                // press did. Mouse mode never turns the drag
                                // into a selection, so without the span check
                                // a press on one link released over another
                                // would open the wrong one; and a press the
                                // chrome consumed (which never latches) must
                                // not open a highlight it never touched. The
                                // latched match is what executes: a modifier
                                // change mid-click can swap which hint config
                                // the same span resolves to.
                                if let Some(latched) = latched_hint {
                                    let same_span = route
                                        .window
                                        .screen
                                        .highlighted_hint()
                                        .is_some_and(|h| {
                                            h.text == latched.text
                                                && h.start == latched.start
                                                && h.end == latched.end
                                        });
                                    if same_span {
                                        route.window.screen.open_latched_hint(
                                            latched,
                                            &mut self.router.clipboard,
                                        );
                                        // Paint the cleared highlight now: an
                                        // action that steals no focus (Copy)
                                        // schedules no frame of its own.
                                        route
                                            .window
                                            .screen
                                            .context_manager
                                            .request_render();
                                    }
                                }
                            }
                        } else if matches!(button, MouseButton::Left | MouseButton::Right)
                        {
                            route.window.screen.copy_selection_on_pointer_release(
                                self.config.copy_on_select,
                                &mut self.router.clipboard,
                            );
                        }
                    }
                }
            }

            WindowEvent::CursorLeft { .. } => {
                #[cfg(all(feature = "wayland", target_os = "linux"))]
                let start_external_drag = route.path == RoutePath::Terminal
                    && route.window.screen.mouse.left_button_state.is_pressed()
                    && route
                        .window
                        .screen
                        .renderer
                        .island
                        .as_ref()
                        .is_some_and(|island| island.is_dragging());
                let clear_merge_target = self
                    .merge_target
                    .is_some_and(|(target, _)| target == window_id);
                if route.window.screen.clear_close_button_hover() {
                    route.request_redraw();
                }
                #[cfg(all(feature = "wayland", target_os = "linux"))]
                if clear_merge_target || start_external_drag {
                    if clear_merge_target {
                        self.clear_merge_target();
                    }
                    if start_external_drag {
                        self.start_external_drag(event_loop, window_id);
                    }
                }
                #[cfg(not(all(feature = "wayland", target_os = "linux")))]
                if clear_merge_target {
                    self.clear_merge_target();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                #[cfg(all(feature = "wayland", target_os = "linux"))]
                if route.path == RoutePath::Terminal
                    && route.window.screen.mouse.left_button_state.is_pressed()
                    && route
                        .window
                        .screen
                        .renderer
                        .island
                        .as_ref()
                        .is_some_and(|island| island.is_dragging())
                {
                    let size = route.window.winit_window.inner_size();
                    if pointer_outside_surface(position, size) {
                        // Wayland's implicit button grab suppresses Leave until
                        // release. Start DnD from out-of-surface motion while the
                        // press serial is still valid, before clamping positions.
                        self.start_external_drag(event_loop, window_id);
                        return;
                    }
                }
                if self.config.hide_cursor_when_typing {
                    route.window.winit_window.set_cursor_visible(true);
                }

                let layout = route.window.screen.sugarloaf.window_size();

                // Keep f64 precision all the way to the cell-grid
                // divide. The old `as usize` cast here dropped
                // subpixel info from HiDPI events.
                let x = position.x.clamp(0.0, (layout.width as i32 - 1) as f64);
                let y = position.y.clamp(0.0, (layout.height as i32 - 1) as f64);

                route.window.screen.mouse.x = x;
                route.window.screen.mouse.y = y;
                route.window.screen.mouse.raw_y = position.y;

                if route.path != RoutePath::Terminal
                    || route.window.screen.renderer.confirm_quit.is_active()
                {
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    return;
                }

                // Handle command palette hover
                if route.window.screen.renderer.command_palette.is_enabled() {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    let win_w = route.window.screen.sugarloaf.window_size().width;
                    let mx = x as f32 / scale;
                    let my = y as f32 / scale;
                    if route
                        .window
                        .screen
                        .renderer
                        .command_palette
                        .hover(mx, my, win_w, scale)
                    {
                        route.request_overlay_redraw();
                    }
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    return;
                }

                // Handle search overlay hover
                if route.window.screen.renderer.search.is_active() {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    let win_w = route.window.screen.sugarloaf.window_size().width;
                    let mx = x as f32 / scale;
                    let my = y as f32 / scale;
                    if route
                        .window
                        .screen
                        .renderer
                        .search
                        .hover(mx, my, win_w, scale)
                    {
                        // UI-only change (hover highlight). `set_dirty`
                        // passes `Renderer::run`'s per-context gate;
                        // the inner damage match hits
                        // `(None, None) => TerminalDamage::Noop` so
                        // no rows rebuild. The search overlay itself
                        // is drawn unconditionally after the per-context
                        // loop in `Renderer::run`.
                        route
                            .window
                            .screen
                            .ctx_mut()
                            .current_mut()
                            .renderable_content
                            .pending_update
                            .set_dirty();
                        route.request_redraw();
                    }
                }

                if route.window.screen.mouse.left_button_state == ElementState::Pressed
                    && route
                        .window
                        .screen
                        .renderer
                        .island
                        .as_ref()
                        .is_some_and(|i| i.is_dragging())
                {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    route
                        .window
                        .screen
                        .handle_tab_drag_move(x as f32 / scale, false);
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    route.request_redraw();
                    return;
                }

                if route.window.screen.update_close_button_hover(x, y) {
                    route.request_redraw();
                }

                // Only force the default cursor while the island is
                // visible — when it's hidden (hide_if_single + single
                // tab on macOS) the band at the top has no tabs to
                // hover, and the I-beam from the terminal grid below
                // should stay during top-edge drags.
                if route.window.screen.tab_bar_contains_y(y) {
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    return;
                }

                // Handle scrollbar drag
                if route.window.screen.renderer.scrollbar.is_dragging() {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    let mouse_y = y as f32 / scale;
                    route.window.screen.handle_scrollbar_drag(mouse_y);
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    route.request_redraw();
                    return;
                }

                // Handle panel border resize
                if route.window.screen.resize_state.is_some() {
                    let state = route.window.screen.resize_state.unwrap();
                    let current_pos = match state.border.direction {
                        crate::layout::BorderDirection::Vertical => x as f32,
                        crate::layout::BorderDirection::Horizontal => y as f32,
                    };
                    let delta = current_pos - state.start_pos;
                    let border = state.border;
                    let original_sizes = state.original_sizes;
                    route
                        .window
                        .screen
                        .context_manager
                        .current_grid_mut()
                        .resize_border(&border, original_sizes, delta);
                    // Dragging a split divider displaces panel origins;
                    // that is layout, not cursor travel.
                    route.window.screen.renderer.trail_cursor.snap();
                    let cursor = match border.direction {
                        crate::layout::BorderDirection::Vertical => CursorIcon::ColResize,
                        crate::layout::BorderDirection::Horizontal => {
                            CursorIcon::RowResize
                        }
                    };
                    route.window.winit_window.set_cursor(cursor);
                    route.window.screen.context_manager.request_render();
                    route.request_redraw();
                    return;
                }

                // Check if hovering over a panel border
                {
                    let grid = route.window.screen.context_manager.current_grid();
                    if let Some(border) = grid.find_border_at_position(x as f32, y as f32)
                    {
                        let cursor = match border.direction {
                            crate::layout::BorderDirection::Vertical => {
                                CursorIcon::ColResize
                            }
                            crate::layout::BorderDirection::Horizontal => {
                                CursorIcon::RowResize
                            }
                        };
                        route.window.winit_window.set_cursor(cursor);
                        route.window.screen.mouse.on_border = true;
                        return;
                    }
                }

                // Check if hovering over scrollbar
                if route.window.screen.is_hovering_scrollbar() {
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    return;
                }

                // Track leaving a border to force cursor reset below
                let was_on_border = route.window.screen.mouse.on_border;
                route.window.screen.mouse.on_border = false;

                let lmb_pressed =
                    route.window.screen.mouse.left_button_state == ElementState::Pressed;
                let rmb_pressed =
                    route.window.screen.mouse.right_button_state == ElementState::Pressed;

                let has_selection = !route.window.screen.selection_is_empty();
                if has_selection && (lmb_pressed || rmb_pressed) {
                    // Only start the timer when the mouse enters the scroll
                    // zone. Once running, the tick reads mouse.raw_y each
                    // iteration so it keeps scrolling after CursorMoved
                    // stops (mouse left window). Cancelled on button release.
                    let delta = route.window.screen.selection_scroll_delta(position.y);
                    if delta != 0 {
                        let scroll_timer_id = route.window.screen.ctx().current_route();
                        let scroll_target =
                            route.window.screen.ctx().current().window_target.clone();
                        let timer_id =
                            TimerId::new(Topic::SelectionScrolling, scroll_timer_id);
                        if !self.scheduler.scheduled(timer_id) {
                            let event = EventPayload::new(
                                RioEventType::Rio(RioEvent::SelectionScrollTick),
                                scroll_target,
                            );
                            self.scheduler.schedule(
                                event,
                                Duration::from_millis(15),
                                true,
                                timer_id,
                            );
                        }
                    }
                }

                let display_offset = route.window.screen.display_offset();
                let point = route.window.screen.mouse_position(display_offset);

                // Compare *cell* coordinates, not pixel coordinates, so
                // subpixel HiDPI jitter inside the same cell doesn't
                // re-fire hint / OSC-8 / hyperlink work every event.
                let prev_cell = route.window.screen.mouse.last_cell;
                let cell_changed = prev_cell != Some(point);
                route.window.screen.mouse.last_cell = Some(point);

                let inside_text_area = route.window.screen.contains_point(x, y);
                let square_side = route.window.screen.side_by_pos(x);

                // If the cursor hasn't changed cells, do nothing.
                // Force update when transitioning off a border so the cursor resets.
                if !cell_changed
                    && !was_on_border
                    && route.window.screen.mouse.square_side == square_side
                    && route.window.screen.mouse.inside_text_area == inside_text_area
                {
                    return;
                }

                // Skip hint/hyperlink highlighting during active selection
                // drag to avoid unnecessary terminal locks and regex matching.
                let is_selecting = (lmb_pressed || rmb_pressed)
                    && (route.window.screen.modifiers.state().shift_key()
                        || !route.window.screen.mouse_mode());

                if !is_selecting {
                    let hint_changed = route.window.screen.update_highlighted_hints();
                    route
                        .window
                        .winit_window
                        .set_cursor(route.window.screen.mouse_cursor_icon());

                    if hint_changed {
                        route.window.screen.context_manager.request_render();
                    }
                }

                route.window.screen.mouse.inside_text_area = inside_text_area;
                route.window.screen.mouse.square_side = square_side;

                if is_selecting {
                    route.window.screen.update_selection(point, square_side);
                    route.window.screen.context_manager.request_render();
                } else if cell_changed && route.window.screen.has_mouse_motion_and_drag()
                {
                    if lmb_pressed {
                        // A latched hint click hides its press and release
                        // from the application; a drag report leaking out
                        // mid-click would arrive with no press around it.
                        if route.window.screen.mouse.hint_click_latched.is_none() {
                            route.window.screen.mouse_report(32, ElementState::Pressed);
                        }
                    } else if route.window.screen.mouse.middle_button_state
                        == ElementState::Pressed
                    {
                        route.window.screen.mouse_report(33, ElementState::Pressed);
                    } else if route.window.screen.mouse.right_button_state
                        == ElementState::Pressed
                    {
                        route.window.screen.mouse_report(34, ElementState::Pressed);
                    } else if route.window.screen.has_mouse_motion() {
                        route.window.screen.mouse_report(35, ElementState::Pressed);
                    }
                }
            }

            WindowEvent::MouseWheel { delta, phase, .. } => {
                if route.path != RoutePath::Terminal
                    || route.window.screen.renderer.confirm_quit.is_active()
                {
                    return;
                }

                if self.config.hide_cursor_when_typing {
                    route.window.winit_window.set_cursor_visible(true);
                }

                match delta {
                    MouseScrollDelta::LineDelta(columns, lines) => {
                        // One wheel notch is one line/column. Convert
                        // with the cell size: scroll() divides the
                        // accumulated pixels by it, and converting
                        // with font_size (smaller than a cell) made
                        // single notches floor to zero lines (#1350).
                        let cell =
                            route.window.screen.ctx().current().dimension.dimension;
                        if cell.width > 0.0 && cell.height > 0.0 {
                            let new_scroll_px_x = columns * cell.width;
                            let new_scroll_px_y = lines * cell.height;
                            route
                                .window
                                .screen
                                .scroll(new_scroll_px_x as f64, new_scroll_px_y as f64);
                        }
                    }
                    MouseScrollDelta::PixelDelta(mut lpos) => {
                        match phase {
                            TouchPhase::Started => {
                                // Reset offset to zero.
                                route.window.screen.mouse.accumulated_scroll =
                                    Default::default();
                            }
                            TouchPhase::Moved => {
                                // When the angle between (x, 0) and (x, y) is lower than ~25 degrees
                                // (cosine is larger that 0.9) we consider this scrolling as horizontal.
                                if lpos.x.abs() / lpos.x.hypot(lpos.y) > 0.9 {
                                    lpos.y = 0.;
                                } else {
                                    lpos.x = 0.;
                                }

                                route.window.screen.scroll(lpos.x, lpos.y);
                            }
                            _ => (),
                        }
                    }
                }

                route
                    .window
                    .winit_window
                    .set_cursor(route.window.screen.mouse_cursor_icon());
                route.request_redraw();
            }

            WindowEvent::KeyboardInput {
                is_synthetic: false,
                event: key_event,
                ..
            } => {
                if route.has_key_wait(&key_event, &mut self.router.clipboard) {
                    if route.path != RoutePath::Terminal
                        && key_event.state == ElementState::Released
                    {
                        // Scheduler must be cleaned after leave the terminal route
                        self.scheduler.unschedule(TimerId::new(
                            Topic::Render,
                            route.window.screen.ctx().current_route(),
                        ));
                    }
                    return;
                }

                route.window.screen.context_manager.set_last_typing();
                route
                    .window
                    .screen
                    .process_key_event(&key_event, &mut self.router.clipboard);
                route
                    .window
                    .winit_window
                    .set_cursor(route.window.screen.mouse_cursor_icon());
                // `process_key_event` used to call `self.render()` for
                // local-only keystrokes (VI mode, search input, hint
                // mode). Now it just marks `pending_update.set_dirty()`
                // through `mark_dirty`. Request a redraw so the next
                // vsync fires `RedrawRequested` — PTY-bound keystrokes
                // also flow through here but their render is idempotent
                // with the PTY-damage-driven redraw.
                route.request_redraw();

                if key_event.state == ElementState::Released
                    && self.config.hide_cursor_when_typing
                {
                    route.window.winit_window.set_cursor_visible(false);
                }
            }

            WindowEvent::Ime(ime) => {
                if route.dismiss_assistant() {
                    route.request_redraw();
                }

                match ime {
                    // The matching keyboard event already handles hint input; do not let its
                    // IME commit reach paste and reset the scrollback position.
                    Ime::Commit(_) if route.window.screen.hint_state.is_active() => {}
                    Ime::Commit(text) => {
                        // Don't use bracketed paste for single char input.
                        route.window.screen.paste(&text, text.chars().count() > 1);
                    }
                    Ime::Preedit(text, cursor_offset) => {
                        let preedit = if text.is_empty() {
                            None
                        } else {
                            Some(Preedit::new(text, cursor_offset.map(|offset| offset.0)))
                        };

                        if route.window.screen.context_manager.current().ime.preedit()
                            != preedit.as_ref()
                        {
                            route
                                .window
                                .screen
                                .context_manager
                                .current_mut()
                                .ime
                                .set_preedit(preedit);
                            route.request_redraw();
                        }
                    }
                    Ime::Enabled => {
                        route
                            .window
                            .screen
                            .context_manager
                            .current_mut()
                            .ime
                            .set_enabled(true);
                    }
                    Ime::Disabled => {
                        let had_preedit = route
                            .window
                            .screen
                            .context_manager
                            .current()
                            .ime
                            .preedit()
                            .is_some();
                        route
                            .window
                            .screen
                            .context_manager
                            .current_mut()
                            .ime
                            .set_enabled(false);
                        if had_preedit {
                            route.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::Touch(touch) => {
                on_touch(route, touch, &mut self.router.clipboard);
            }

            WindowEvent::Focused(focused) => {
                if self.config.hide_cursor_when_typing {
                    route.window.winit_window.set_cursor_visible(true);
                }

                let focus_changed = route.window.is_focused != focused;
                route.window.is_focused = focused;

                // Focus is a cheap checkpoint to catch backing-scale changes
                // whose ScaleFactorChanged never arrived (sleep/wake display
                // reconfiguration).
                if focused
                    && route
                        .window
                        .screen
                        .reconcile_scale(&route.window.winit_window)
                {
                    route.window.update_vblank_interval();
                    route.request_redraw();
                } else if focus_changed {
                    route.request_redraw();
                }

                route.window.screen.on_focus_change(focused);
            }

            WindowEvent::Occluded(occluded) => {
                let was_occluded = route.window.is_occluded;
                route.window.is_occluded = occluded;

                // If window was occluded and is now visible, mark for one-time render
                if was_occluded && !occluded {
                    route.window.needs_render_after_occlusion = true;
                    // Same checkpoint as focus: the un-occlusion after wake
                    // is often the first event the window receives.
                    if route
                        .window
                        .screen
                        .reconcile_scale(&route.window.winit_window)
                    {
                        route.window.update_vblank_interval();
                    }
                    // An idle terminal produces no PTY traffic to trigger the
                    // deferred post-occlusion render; request it directly.
                    route.request_redraw();
                }
            }

            WindowEvent::ThemeChanged(new_theme) => {
                if self.config.force_theme.is_some() {
                    return;
                }
                update_colors_based_on_theme(&mut self.config, Some(new_theme));
                route.window.screen.update_config(
                    &self.config,
                    &self.router.font_library,
                    false,
                );
                route.window.configure_window(&self.config);
                route.request_redraw();
            }

            WindowEvent::DroppedFile(path) => {
                if route.dismiss_assistant() {
                    route.request_redraw();
                }

                let path = crate::platform::shell_escape(&path.to_string_lossy());
                route.window.screen.paste(&(path + " "), true);
            }

            WindowEvent::Resized(new_size) => {
                if new_size.width == 0 || new_size.height == 0 {
                    return;
                }

                route.window.screen.resize(new_size);
                route.request_redraw();
            }

            WindowEvent::ScaleFactorChanged {
                inner_size_writer: _,
                scale_factor,
            } => {
                let scale = scale_factor as f32;
                route
                    .window
                    .screen
                    .set_scale(scale, route.window.winit_window.inner_size());
                route.window.update_vblank_interval();
                route.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                route.begin_render();

                match route.path {
                    RoutePath::Welcome => {
                        route.window.screen.render_welcome();
                    }
                    RoutePath::Terminal => {
                        if let Some(window_update) = route.window.screen.render() {
                            use crate::context::renderable::{
                                BackgroundState, WindowUpdate,
                            };
                            match window_update {
                                WindowUpdate::Background(bg_state) => {
                                    // for now setting this as allowed because it fails on linux builds
                                    #[allow(unused_variables)]
                                    let bg_color = match bg_state {
                                        BackgroundState::Set(color) => color,
                                        BackgroundState::Reset => {
                                            self.config.colors.background.1
                                        }
                                    };

                                    #[cfg(target_os = "macos")]
                                    {
                                        route.window.winit_window.set_background_color(
                                            bg_color.r, bg_color.g, bg_color.b,
                                            bg_color.a,
                                        );
                                    }

                                    #[cfg(target_os = "windows")]
                                    {
                                        use rio_window::platform::windows::WindowExtWindows;
                                        route
                                            .window
                                            .winit_window
                                            .set_title_bar_background_color(
                                                bg_color.r, bg_color.g, bg_color.b,
                                                bg_color.a,
                                            );
                                    }
                                }
                            }
                        }

                        let ready_imports =
                            route.window.screen.take_ready_session_imports();
                        if !ready_imports.is_empty() {
                            self.ready_session_imports.push((window_id, ready_imports));
                        }

                        // Update IME cursor position after rendering to ensure it's current
                        route.window.screen.update_ime_cursor_position_if_needed(
                            &route.window.winit_window,
                        );
                    }
                }

                #[cfg(target_os = "windows")]
                if !route.window.initial_frame_rendered {
                    // Keep the native window cloaked until the first complete
                    // render has been submitted. Uncloaking earlier can expose
                    // the blank native surface during GPU/PTY startup.
                    use rio_window::platform::windows::WindowExtWindows;
                    route.window.winit_window.set_cloaked(false);
                    route.window.initial_frame_rendered = true;
                }

                // let duration = start.elapsed();
                // println!("Time elapsed in render() is: {:?}", duration);
                // }

                // Game mode = unlocked framerate, so keep the event loop
                // spinning. Every other case is vsync-paced: a
                // `request_redraw` tells winit to deliver
                // `RedrawRequested` at the next platform vsync, and the
                // OS parks the thread until that event arrives. Busy-
                // polling between vsyncs here would burn CPU without
                // delivering more frames.
                if self.config.renderer.strategy.is_game() {
                    route.request_redraw();
                    event_loop.set_control_flow(ControlFlow::Poll);
                } else {
                    if route
                        .window
                        .screen
                        .ctx()
                        .current()
                        .renderable_content
                        .pending_update
                        .is_dirty()
                    {
                        route.request_redraw();
                    }
                    event_loop.set_control_flow(ControlFlow::Wait);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.poll_window_control(event_loop);

        if self.poll_deferred_session_exit(event_loop) {
            return;
        }

        // Transferred contexts were disarmed before removal: exiting the GUI
        // must not send Close to their workers. Bootstrap windows remain routes.
        if self.exit_after_transfer {
            tracing::info!(
                windows = self.router.routes.len(),
                pending_prepared = self.pending_prepared.len(),
                pending_outgoing = self.pending_outgoing.len(),
                session_preparations =
                    std::sync::Arc::strong_count(&self.session_preparations),
                "checking transferred GUI exit"
            );
        }
        if transfer_exit_ready(
            self.exit_after_transfer,
            self.router.routes.len(),
            self.pending_prepared.len()
                + self.pending_outgoing.len()
                + std::sync::Arc::strong_count(&self.session_preparations)
                - 1,
            crate::WINDOW_BOOTSTRAP.lock().unwrap().is_some(),
        ) {
            event_loop.exit();
            return;
        }

        #[cfg(all(feature = "wayland", target_os = "linux"))]
        if self
            .tab_drag_detach_deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            self.tab_drag_detach_deadline = None;
            if self
                .tab_drag
                .as_ref()
                .is_some_and(|drag| !drag.whole_window && drag.hover.is_none())
            {
                self.dispatch_tab_drag_event(event_loop, TabDragEvent::Detach);
            }
        }

        let scheduler_deadline = self
            .scheduler
            .update()
            .into_iter()
            .chain(self.pending_prepared.iter().map(|pending| pending.deadline))
            .min();
        #[cfg(all(feature = "wayland", target_os = "linux"))]
        let detach_deadline = self.tab_drag_detach_deadline;
        #[cfg(not(all(feature = "wayland", target_os = "linux")))]
        let detach_deadline = None;
        let control_flow = match (scheduler_deadline, detach_deadline) {
            (Some(scheduler), Some(detach)) => {
                ControlFlow::WaitUntil(scheduler.min(detach))
            }
            (Some(instant), None) | (None, Some(instant)) => {
                ControlFlow::WaitUntil(instant)
            }
            (None, None) => ControlFlow::Wait,
        };
        event_loop.set_control_flow(control_flow);
    }

    fn open_config(&mut self, event_loop: &ActiveEventLoop) {
        if self.config.navigation.open_config_with_split {
            self.router.open_config_split(&self.config);
        } else {
            self.router.open_config_window(
                event_loop,
                self.event_proxy.clone(),
                &self.config,
            );
        }
    }

    fn hook_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        key: &rio_window::event::KeyEvent,
        modifiers: &rio_window::event::Modifiers,
    ) {
        let window_id = match self.router.get_focused_route() {
            Some(window_id) => window_id,
            None => return,
        };

        #[cfg(all(feature = "wayland", target_os = "linux"))]
        if self.tab_drag.as_ref().is_some_and(|drag| {
            rio_backend::event::WindowId::from(drag.source_window) == window_id
                || drag.owner.as_ref().is_some_and(|owner| {
                    rio_backend::event::WindowId::from(owner.window_id) == window_id
                })
        }) {
            return;
        }

        let route = match self.router.routes.get_mut(&window_id) {
            Some(window) => window,
            None => return,
        };

        // For menu-triggered events, we need to temporarily set the correct modifiers
        // since menu events don't trigger ModifiersChanged events.
        let original_modifiers = route.window.screen.modifiers;

        // Use the modifiers passed from the menu action
        route.window.screen.set_modifiers(*modifiers);

        // Process the key event
        route
            .window
            .screen
            .process_key_event(key, &mut self.router.clipboard);

        // Restore the original modifiers
        route.window.screen.set_modifiers(original_modifiers);
    }

    // Emitted when the event loop is being shut down.
    // This is irreversible - if this event is emitted, it is guaranteed to be the last event that gets emitted.
    // You generally want to treat this as an “do on quit” event.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Ensure that all the windows are dropped, so the destructors for
        // Renderer and contexts ran.
        self.router.routes.clear();

        // SAFETY: The clipboard must be dropped before the event loop, so
        // replace it with a safe no-op placeholder.
        self.router.clipboard = Clipboard::new_nop();

        // `process::exit` skips field destructors; stop the acceptor explicitly.
        self.window_control.take();

        std::process::exit(0);
    }
}

#[cfg(all(test, feature = "wayland", target_os = "linux"))]
mod tab_drag_executor_tests {
    use super::*;
    use crate::layout::TabId;

    #[test]
    fn close_deferral_matches_source_and_target_only() {
        let state =
            TabDrag::<u8, u8>::begin(WindowId::from(11), TabId::for_test(23), 1, 3)
                .unwrap()
                .state
                .reduce(TabDragEvent::Prepared(7))
                .state
                .reduce(TabDragEvent::OwnerStarted {
                    drag_id: 7,
                    owner: OwnerRoute {
                        window_id: WindowId::from(11),
                        tab_id: TabId::for_test(23),
                        route_ids: vec![4, 9],
                    },
                })
                .state;

        assert_eq!(
            deferred_window_close(&state, WindowId::from(11)),
            Some(DeferredTabDragClose::Window(
                rio_backend::event::WindowId::from(WindowId::from(11))
            ))
        );
        assert_eq!(deferred_window_close(&state, WindowId::from(44)), None);
        assert!(!is_valid_tab_drag_target(&state, WindowId::from(11)));
        assert!(is_valid_tab_drag_target(&state, WindowId::from(44)));

        let state = state
            .reduce(TabDragEvent::Enter {
                offer_id: 3,
                target_window: WindowId::from(22),
                index: 0,
            })
            .state;
        assert_eq!(
            deferred_window_close(&state, WindowId::from(22)),
            Some(DeferredTabDragClose::Window(
                rio_backend::event::WindowId::from(WindowId::from(22))
            ))
        );
    }
}

#[cfg(all(
    feature = "audio",
    not(target_os = "macos"),
    not(target_os = "windows")
))]
fn play_bell_sound() -> Result<(), Box<dyn Error>> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("No output device available")?;

    let config = device.default_output_config()?;

    match config.sample_format() {
        cpal::SampleFormat::F32 => run_bell::<f32>(&device, &config.into()),
        cpal::SampleFormat::I16 => run_bell::<i16>(&device, &config.into()),
        cpal::SampleFormat::U16 => run_bell::<u16>(&device, &config.into()),
        _ => Err("Unsupported sample format".into()),
    }
}

#[cfg(all(
    feature = "audio",
    not(target_os = "macos"),
    not(target_os = "windows")
))]
fn run_bell<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
) -> Result<(), Box<dyn Error>>
where
    T: cpal::Sample + cpal::SizedSample + cpal::FromSample<f32>,
{
    let sample_rate = config.sample_rate.0 as f32;
    let channels = config.channels as usize;
    let duration_secs = crate::constants::BELL_DURATION.as_secs_f32();
    let total_samples = (sample_rate * duration_secs) as usize;

    let mut sample_clock = 0f32;
    let mut samples_played = 0usize;

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                if samples_played >= total_samples {
                    for sample in frame.iter_mut() {
                        *sample = T::from_sample(0.0);
                    }
                } else {
                    let value = (sample_clock * 440.0 * 2.0 * std::f32::consts::PI
                        / sample_rate)
                        .sin()
                        * 0.2;
                    for sample in frame.iter_mut() {
                        *sample = T::from_sample(value);
                    }
                    sample_clock += 1.0;
                    samples_played += 1;
                }
            }
        },
        |err| tracing::error!("Audio stream error: {}", err),
        None,
    )?;

    stream.play()?;
    std::thread::sleep(crate::constants::BELL_DURATION);

    Ok(())
}
