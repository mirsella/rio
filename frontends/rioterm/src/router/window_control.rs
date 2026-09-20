//! Authenticated control transport between independently launched Rio windows.
//!
//! The registry contains only endpoint metadata and is private to the current
//! user.  Capabilities are read from that private registry, never put in drag
//! payloads or process arguments.  Terminal session descriptors travel only
//! over the authenticated socket.

#[cfg(unix)]
use rio_backend::event::RioEvent;
use rio_backend::event::{EventListener, WindowId};
#[cfg(unix)]
use rio_session::codec;
#[cfg(unix)]
use rio_session::readiness::{self, Readiness};
use rio_session::SessionDescriptor;
#[cfg(all(feature = "wayland", target_os = "linux"))]
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
#[cfg(unix)]
use std::sync::mpsc::{self, Receiver};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(all(feature = "wayland", target_os = "linux"))]
use std::sync::Mutex;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

const VERSION: u16 = 4;
const MAX_CONNECTIONS: usize = 8;
const MAX_OFFERS: usize = 64;
const MAX_LAYOUT_DEPTH: usize = 64;
const MAX_LAYOUT_NODES: usize = MAX_OFFERS * 4;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
#[cfg(unix)]
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// A response that waits on the user, such as a drag or offer decision.
#[cfg(unix)]
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a discovery probe waits on one peer.
#[cfg(unix)]
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, bincode::Encode, bincode::Decode)]
pub struct WindowEndpoint {
    pub instance: [u8; 16],
    pub process_id: u32,
    pub native_window_id: u64,
    pub endpoint: String,
    pub capability: [u8; 32],
    pub scope: String,
}

impl fmt::Debug for WindowEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowEndpoint")
            .field("instance", &self.instance)
            .field("process_id", &self.process_id)
            .field("native_window_id", &self.native_window_id)
            .field("endpoint", &self.endpoint)
            .field("capability", &"<redacted>")
            .field("scope", &self.scope)
            .finish()
    }
}

impl WindowEndpoint {
    fn validate(&self) -> Result<(), String> {
        if self.instance == [0; 16] || self.capability == [0; 32] {
            return Err("window endpoint identity is empty".into());
        }
        if self.endpoint.len() > MAX_FRAME_BYTES
            || self.endpoint.as_bytes().contains(&0)
            || self.scope.len() > 1024
            || self.scope.as_bytes().contains(&0)
        {
            return Err("window endpoint metadata is invalid".into());
        }
        let endpoint = Path::new(&self.endpoint);
        if !endpoint.is_absolute() {
            return Err("window endpoint path is not absolute".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let parent = endpoint
                .parent()
                .ok_or_else(|| "window endpoint has no parent".to_string())?;
            let metadata = std::fs::symlink_metadata(parent)
                .map_err(|error| format!("window endpoint parent: {error}"))?;
            if !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err("window endpoint parent is not private".into());
            }
            let socket = std::fs::symlink_metadata(endpoint)
                .map_err(|error| format!("window endpoint: {error}"))?;
            if !socket.file_type().is_socket() {
                return Err("window endpoint is not a socket".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, bincode::Encode, bincode::Decode)]
pub struct PaneOffer {
    pub route_id: u64,
    pub tab_id: u64,
    pub layout_rect: [f32; 4],
    pub session: SessionDescriptor,
}

#[derive(Clone, Copy, Debug, bincode::Encode, bincode::Decode, Eq, PartialEq)]
pub enum LayoutDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, bincode::Encode, bincode::Decode)]
pub struct LayoutNodeOffer {
    /// Zero identifies a split container; leaves carry their source route.
    pub route_id: u64,
    pub direction: Option<LayoutDirection>,
    pub flex_grow: f32,
    pub children: Vec<LayoutNodeOffer>,
}

#[derive(Clone, Debug, bincode::Encode, bincode::Decode)]
pub struct TabOffer {
    pub tab_id: u64,
    pub layout: LayoutNodeOffer,
    pub active_route: u64,
}

#[derive(Clone, Debug, bincode::Encode, bincode::Decode)]
pub struct TransferOffer {
    pub transfer_id: [u8; 16],
    pub panes: Vec<PaneOffer>,
    pub tabs: Vec<TabOffer>,
    pub active_pane: u32,
}

#[cfg(unix)]
pub fn new_transfer_id() -> Result<[u8; 16], String> {
    random_bytes::<16>()
}

#[cfg(not(unix))]
pub fn new_transfer_id() -> Result<[u8; 16], String> {
    Err("cross-window control is unsupported on this platform".into())
}

/// Flag marking a detached-window bootstrap child. The transfer identity
/// itself travels over inherited stdin, never via argv.
pub const WINDOW_BOOTSTRAP_FLAG: &str = "--window-bootstrap";

/// Maximum app id length forwarded to a bootstrap child (Wayland app_id /
/// X11 WM_CLASS). Keeps a malformed source value from becoming an invalid
/// child argv entry.
pub const MAX_APP_ID_LEN: usize = 256;

/// Removes every bootstrap flag from process argv. Returns whether the
/// current process is a detached-window bootstrap child.
pub fn take_window_bootstrap_flag(arguments: &mut Vec<std::ffi::OsString>) -> bool {
    let mut found = false;
    let mut is_argv0 = true;
    arguments.retain(|arg| {
        // Never strip argv[0]: it is the executable path, not a flag.
        if is_argv0 {
            is_argv0 = false;
            return true;
        }
        if arg == WINDOW_BOOTSTRAP_FLAG {
            found = true;
            false
        } else {
            true
        }
    });
    found
}

/// Reads the 16-byte transfer identity a bootstrap child receives over stdin.
pub fn read_bootstrap_identity<R: std::io::Read>(
    mut source: R,
) -> Result<[u8; 16], std::io::Error> {
    let mut transfer = [0; 16];
    source.read_exact(&mut transfer)?;
    if transfer == [0; 16] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid window bootstrap identity",
        ));
    }
    Ok(transfer)
}

pub fn validate_bootstrap_app_id(app_id: &str) -> Result<(), String> {
    if app_id.is_empty()
        || app_id.len() > MAX_APP_ID_LEN
        || app_id.as_bytes().contains(&0)
    {
        return Err("window bootstrap app id is invalid".into());
    }
    Ok(())
}

/// Builds the bootstrap child command. The flag stays first; the source
/// window's app id is forwarded so the child reports the same Wayland app_id
/// / X11 WM_CLASS as a manually launched second Rio window and groups with
/// it in the taskbar.
pub fn bootstrap_command(
    executable: &Path,
    app_id: Option<&str>,
) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command.arg(WINDOW_BOOTSTRAP_FLAG);
    if let Some(app_id) = app_id.filter(|id| !id.is_empty()) {
        command.arg("--app-id").arg(app_id);
    }
    command.stdin(std::process::Stdio::piped());
    command
}

impl TransferOffer {
    pub fn pane_route_ids(&self) -> Vec<u64> {
        self.panes.iter().map(|pane| pane.route_id).collect()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.transfer_id == [0; 16] {
            return Err("transfer id is empty".into());
        }
        Self::validate_parts(self.panes.iter(), &self.tabs, self.active_pane).map(|_| ())
    }

    pub(crate) fn validate_parts<'a, I>(
        panes: I,
        tabs: &[TabOffer],
        active_pane: u32,
    ) -> Result<Vec<Vec<u64>>, String>
    where
        I: IntoIterator<Item = &'a PaneOffer> + Clone,
    {
        let pane_count = panes.clone().into_iter().count();
        if pane_count == 0 || pane_count > MAX_OFFERS {
            return Err("transfer pane count is outside the bounded limit".into());
        }
        if tabs.is_empty() || tabs.len() > MAX_OFFERS {
            return Err("transfer tab count is outside the bounded limit".into());
        }
        if usize::try_from(active_pane).map_or(true, |index| index >= pane_count) {
            return Err("active transfer pane is out of range".into());
        }
        let mut tab_ids = HashSet::with_capacity(tabs.len());
        let mut routes = HashSet::with_capacity(pane_count);
        let mut routes_by_tab = Vec::with_capacity(tabs.len());
        for tab in tabs {
            if tab.tab_id == 0 || !tab_ids.insert(tab.tab_id) {
                return Err("transfer tab identity is empty or duplicated".into());
            }
            let mut layout_routes = Vec::new();
            validate_layout_node(&tab.layout, 0, &mut 0, &mut layout_routes)?;
            if tab.active_route == 0 || !layout_routes.contains(&tab.active_route) {
                return Err("active transfer route is not in its tab layout".into());
            }
            let pane_routes: HashSet<_> = panes
                .clone()
                .into_iter()
                .filter(|pane| pane.tab_id == tab.tab_id)
                .map(|pane| pane.route_id)
                .collect();
            if pane_routes.is_empty()
                || pane_routes.len() != layout_routes.len()
                || layout_routes
                    .iter()
                    .any(|route| !pane_routes.contains(route))
            {
                return Err("transfer layout does not match its panes".into());
            }
            routes_by_tab.push(layout_routes);
        }
        for pane in panes {
            if pane.route_id == 0 || pane.tab_id == 0 {
                return Err("transfer pane identity is empty".into());
            }
            if !routes.insert(pane.route_id) || !tab_ids.contains(&pane.tab_id) {
                return Err("transfer pane identity is invalid or duplicated".into());
            }
            if pane
                .layout_rect
                .iter()
                .any(|value| !value.is_finite() || *value < 0.0)
            {
                return Err("transfer pane layout is invalid".into());
            }
            pane.session
                .validate()
                .map_err(|error| format!("invalid pane session: {error}"))?;
        }
        Ok(routes_by_tab)
    }
}

fn validate_layout_node(
    node: &LayoutNodeOffer,
    depth: usize,
    count: &mut usize,
    routes: &mut Vec<u64>,
) -> Result<(), String> {
    if depth > MAX_LAYOUT_DEPTH {
        return Err("transfer layout is too deep".into());
    }
    *count = (*count)
        .checked_add(1)
        .ok_or_else(|| "transfer layout node count overflowed".to_string())?;
    if *count > MAX_LAYOUT_NODES {
        return Err("transfer layout has too many nodes".into());
    }
    if !node.flex_grow.is_finite() || node.flex_grow < 0.0 {
        return Err("transfer layout flex size is invalid".into());
    }
    if node.children.is_empty() {
        if node.route_id == 0 || node.direction.is_some() {
            return Err("transfer layout leaf is invalid".into());
        }
        if routes.contains(&node.route_id) {
            return Err("transfer layout route is duplicated".into());
        }
        routes.push(node.route_id);
        return Ok(());
    }
    if node.route_id != 0 || node.direction.is_none() || node.children.len() < 2 {
        return Err("transfer layout container is invalid".into());
    }
    for child in &node.children {
        validate_layout_node(child, depth + 1, count, routes)?;
    }
    Ok(())
}

#[derive(Clone, Debug, bincode::Encode, bincode::Decode)]
enum Request {
    Hello {
        version: u16,
        capability: [u8; 32],
    },
    Offer {
        offer: TransferOffer,
        target_index: Option<u32>,
    },
    Take([u8; 16]),
    Probe,
    ArmSelection {
        selection_id: [u8; 16],
        source: WindowEndpoint,
    },
    CancelSelection {
        selection_id: [u8; 16],
    },
    SelectionEvent {
        selection_id: [u8; 16],
        target_window: u64,
        target_index: u32,
        clicked: bool,
    },
}

#[derive(Debug)]
pub enum WindowControlEvent {
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    DragResult {
        transfer_id: [u8; 16],
        result: Result<(), String>,
    },
    Probe {
        window_id: WindowId,
        reply: SyncSender<WindowControlResponse>,
    },
    Peers {
        window_id: WindowId,
        targets: Vec<WindowEndpoint>,
    },
    IncomingOffer {
        offer: TransferOffer,
        reply: SyncSender<WindowControlResponse>,
        target_window: Option<u64>,
        target_index: Option<usize>,
    },
    OfferResult {
        transfer_id: [u8; 16],
        result: Result<Vec<u64>, String>,
    },
    ArmSelection {
        window_id: WindowId,
        selection_id: [u8; 16],
        source: WindowEndpoint,
        reply: SyncSender<WindowControlResponse>,
    },
    CancelSelection {
        window_id: WindowId,
        selection_id: [u8; 16],
    },
    Selection {
        selection_id: [u8; 16],
        target_window: u64,
        target_index: u32,
        clicked: bool,
    },
    ArmSelectionResult {
        selection_id: [u8; 16],
        target: WindowEndpoint,
        result: Result<(), String>,
    },
}

#[derive(Clone, Debug, bincode::Encode, bincode::Decode)]
pub enum WindowControlResponse {
    Hello,
    Offer(TransferOffer),
    Committed { routes: Vec<u64> },
    SelectionArmed,
    SelectionCancelled,
    Rejected(String),
}

impl fmt::Display for WindowControlEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            Self::DragResult { .. } => write!(formatter, "foreign drag result"),
            Self::Probe { .. } => write!(formatter, "window probe"),
            Self::Peers { .. } => write!(formatter, "window discovery"),
            Self::IncomingOffer { offer, .. } => {
                write!(
                    formatter,
                    "incoming transfer {}",
                    hex_id(&offer.transfer_id)
                )
            }
            Self::OfferResult {
                transfer_id,
                result,
            } => write!(formatter, "transfer {}: {result:?}", hex_id(transfer_id)),
            Self::ArmSelection { selection_id, .. } => {
                write!(formatter, "arm window selection {}", hex_id(selection_id))
            }
            Self::CancelSelection { selection_id, .. } => {
                write!(
                    formatter,
                    "cancel window selection {}",
                    hex_id(selection_id)
                )
            }
            Self::Selection {
                selection_id,
                clicked,
                ..
            } => write!(
                formatter,
                "window selection {} ({})",
                hex_id(selection_id),
                if *clicked { "clicked" } else { "hovered" }
            ),
            Self::ArmSelectionResult { selection_id, .. } => {
                write!(formatter, "window selection arm {}", hex_id(selection_id))
            }
        }
    }
}

#[cfg(unix)]
pub struct WindowControl {
    descriptor: WindowEndpoint,
    registry_path: PathBuf,
    events: Receiver<WindowControlEvent>,
    event_sender: SyncSender<WindowControlEvent>,
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    published: Arc<Mutex<HashMap<[u8; 16], TransferOffer>>>,
    acceptor_stop: Arc<AtomicBool>,
    acceptor_wakeup: Arc<Readiness>,
    acceptor_stopped: Arc<AtomicBool>,
    acceptor: Option<thread::JoinHandle<()>>,
}

#[cfg(not(unix))]
pub struct WindowControl;

#[cfg(unix)]
impl WindowControl {
    pub fn new<T: EventListener + Clone + Send + 'static>(
        event_proxy: T,
        window_id: WindowId,
    ) -> Result<Self, String> {
        let root = registry_root()?;
        ensure_private_registry(&root)?;
        let acceptor_wakeup = Arc::new(Readiness::new().map_err(|error| {
            format!("create window control acceptor wakeup: {error}")
        })?);

        let instance = random_bytes::<16>()?;
        let capability = random_bytes::<32>()?;
        let socket_path = root.join(format!("w-{}.sock", hex_id(&instance)));
        let registry_path = root.join(format!("w-{}.desc", hex_id(&instance)));
        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| format!("bind window control endpoint: {error}"))?;
        set_private_file(&socket_path)?;
        if let Err(error) = listener.set_nonblocking(true) {
            let _ = std::fs::remove_file(&socket_path);
            return Err(format!("make window control endpoint stoppable: {error}"));
        }

        let descriptor = WindowEndpoint {
            instance,
            process_id: std::process::id(),
            native_window_id: u64::from(window_id),
            endpoint: socket_path.to_string_lossy().into_owned(),
            capability,
            scope: display_scope(),
        };
        descriptor.validate()?;
        if let Err(error) = write_descriptor(&registry_path, &descriptor) {
            let _ = std::fs::remove_file(&socket_path);
            return Err(error);
        }

        let (event_sender, events) = mpsc::sync_channel(MAX_CONNECTIONS * 2);
        #[cfg(all(feature = "wayland", target_os = "linux"))]
        let published = Arc::new(Mutex::new(HashMap::new()));
        let wake_proxy = event_proxy
            .with_window_target(rio_backend::event::WindowTarget::dynamic(window_id));
        let accept_sender = event_sender.clone();
        #[cfg(all(feature = "wayland", target_os = "linux"))]
        let accept_published = published.clone();
        let acceptor_stop = Arc::new(AtomicBool::new(false));
        let acceptor_stopped = Arc::new(AtomicBool::new(false));
        let acceptor_stop_thread = acceptor_stop.clone();
        let acceptor_wakeup_thread = acceptor_wakeup.clone();
        let acceptor_stopped_thread = acceptor_stopped.clone();
        let acceptor = match thread::Builder::new()
            .name("rio-window-control".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    accept_sender,
                    wake_proxy,
                    window_id,
                    capability,
                    #[cfg(all(feature = "wayland", target_os = "linux"))]
                    accept_published,
                    AcceptorState {
                        stop: acceptor_stop_thread,
                        wakeup: acceptor_wakeup_thread,
                        stopped: acceptor_stopped_thread,
                        #[cfg(test)]
                        blocked: None,
                    },
                )
            }) {
            Ok(acceptor) => acceptor,
            Err(error) => {
                let _ = std::fs::remove_file(&registry_path);
                let _ = std::fs::remove_file(&socket_path);
                return Err(format!("start window control endpoint: {error}"));
            }
        };

        Ok(Self {
            descriptor,
            registry_path,
            events,
            event_sender,
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            published,
            acceptor_stop,
            acceptor_wakeup,
            acceptor_stopped,
            acceptor: Some(acceptor),
        })
    }

    pub fn descriptor(&self) -> &WindowEndpoint {
        &self.descriptor
    }

    pub fn poll(&self) -> Vec<WindowControlEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    pub fn discover(scope: &str) -> Vec<WindowEndpoint> {
        let Ok(root) = registry_root() else {
            return Vec::new();
        };
        if !is_private_directory(&root) {
            return Vec::new();
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        entries
            .take(MAX_OFFERS)
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".desc"))
            .filter_map(|entry| read_descriptor(&entry.path()).ok())
            .filter(|descriptor| descriptor.scope == scope)
            .collect()
    }

    pub fn offer_async<T: EventListener + Clone + Send + 'static>(
        &self,
        target: WindowEndpoint,
        offer: TransferOffer,
        target_index: Option<usize>,
        event_proxy: T,
        window_id: WindowId,
    ) -> Result<(), String> {
        target.validate()?;
        offer.validate()?;
        let target_index = target_index
            .map(u32::try_from)
            .transpose()
            .map_err(|_| "window transfer target index is too large".to_string())?;
        if target.instance == self.descriptor.instance {
            return Err("a window cannot receive its own transfer".into());
        }
        let sender = self.event_sender.clone();
        let listener = event_proxy
            .with_window_target(rio_backend::event::WindowTarget::dynamic(window_id));
        thread::Builder::new()
            .name("rio-window-offer".into())
            .spawn(move || {
                let result = send_offer(&target, &offer, target_index);
                let _ = sender.try_send(WindowControlEvent::OfferResult {
                    transfer_id: offer.transfer_id,
                    result,
                });
                listener.send_event(RioEvent::Render, window_id);
            })
            .map_err(|error| format!("start window transfer: {error}"))?;
        Ok(())
    }

    pub fn launch_and_offer_async<T: EventListener + Clone + Send + 'static>(
        &self,
        offer: TransferOffer,
        event_proxy: T,
        window_id: WindowId,
        app_id: Option<String>,
    ) -> Result<(), String> {
        offer.validate()?;
        if let Some(app_id) = app_id.as_deref() {
            validate_bootstrap_app_id(app_id)?;
        }
        let sender = self.event_sender.clone();
        let listener = event_proxy
            .with_window_target(rio_backend::event::WindowTarget::dynamic(window_id));
        let scope = self.descriptor.scope.clone();
        thread::Builder::new()
            .name("rio-window-launch".into())
            .spawn(move || {
                let result =
                    Self::blocking_launch_and_offer(&scope, &offer, app_id.as_deref());
                let _ = sender.try_send(WindowControlEvent::OfferResult {
                    transfer_id: offer.transfer_id,
                    result,
                });
                listener.send_event(RioEvent::Render, window_id);
            })
            .map_err(|error| format!("start Rio window launch: {error}"))?;
        Ok(())
    }

    /// Reap a detached window without holding up the commit result: the
    /// commit is already known, and the source tab must be removed as soon
    /// as the target takes ownership. Waiting here instead would leave a
    /// frozen ghost tab in the source window until the new window exits.
    fn reap_detached_window(child: std::process::Child) {
        if let Err(error) =
            thread::Builder::new()
                .name("rio-window-reap".into())
                .spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                })
        {
            tracing::warn!(%error, "detached Rio window reaper failed to start");
        }
    }

    #[cfg(unix)]
    fn blocking_launch_and_offer(
        scope: &str,
        offer: &TransferOffer,
        app_id: Option<&str>,
    ) -> Result<Vec<u64>, String> {
        let mut child = Self::spawn_bootstrap_child(app_id)?;
        let process_id = child.id();
        if let Err(error) = Self::write_bootstrap_identity(&mut child, &offer.transfer_id)
        {
            // An empty bootstrap window has no session to preserve.
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let result = Self::await_bootstrap_commit(scope, process_id, offer);
        if result.is_ok() {
            Self::reap_detached_window(child);
        } else {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }

    #[cfg(unix)]
    fn spawn_bootstrap_child(
        app_id: Option<&str>,
    ) -> Result<std::process::Child, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("find Rio executable: {error}"))?;
        bootstrap_command(&executable, app_id)
            .spawn()
            .map_err(|error| format!("launch Rio window: {error}"))
    }

    #[cfg(unix)]
    fn write_bootstrap_identity(
        child: &mut std::process::Child,
        transfer_id: &[u8; 16],
    ) -> Result<(), String> {
        use std::io::Write;
        let mut bootstrap = child
            .stdin
            .take()
            .ok_or_else(|| "bootstrap window stdin is unavailable".to_owned())?;
        bootstrap
            .write_all(transfer_id)
            .map_err(|error| format!("bootstrap window: {error}"))
    }

    #[cfg(unix)]
    fn await_bootstrap_commit(
        scope: &str,
        process_id: u32,
        offer: &TransferOffer,
    ) -> Result<Vec<u64>, String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(target) = WindowControl::discover(scope)
                .into_iter()
                .find(|target| target.process_id == process_id)
            {
                return send_offer(&target, offer, None);
            }
            if std::time::Instant::now() >= deadline {
                return Err("new Rio window did not publish a control endpoint".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn discover_peers_async<T: EventListener + Clone + Send + 'static>(
        &self,
        listener: T,
        window_id: WindowId,
    ) -> Result<(), String> {
        let own = self.descriptor.instance;
        let scope = self.descriptor.scope.clone();
        let sender = self.event_sender.clone();
        thread::Builder::new()
            .name("rio-window-discovery".into())
            .spawn(move || {
                let targets = Self::discover(&scope)
                    .into_iter()
                    .filter(|target| {
                        if target.instance == own {
                            return false;
                        }
                        let probe = || -> Result<bool, Box<dyn std::error::Error>> {
                            let mut stream = UnixStream::connect(&target.endpoint)?;
                            stream.set_nonblocking(true)?;
                            let deadline = || Instant::now() + PROBE_TIMEOUT;
                            codec::write_frame_until(
                                &mut stream,
                                &Request::Hello {
                                    version: VERSION,
                                    capability: target.capability,
                                },
                                deadline(),
                            )?;
                            if !matches!(
                                codec::read_frame_until(&mut stream, deadline())?,
                                WindowControlResponse::Hello
                            ) {
                                return Ok(false);
                            }
                            codec::write_frame_until(
                                &mut stream,
                                &Request::Probe,
                                deadline(),
                            )?;
                            Ok(matches!(
                                codec::read_frame_until(&mut stream, deadline())?,
                                WindowControlResponse::Hello
                            ))
                        };
                        probe().unwrap_or(false)
                    })
                    .collect();
                let _ = sender.try_send(WindowControlEvent::Peers { window_id, targets });
                listener.send_event(RioEvent::Render, window_id);
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn arm_selection_async<T: EventListener + Clone + Send + 'static>(
        &self,
        target: WindowEndpoint,
        selection_id: [u8; 16],
        source: WindowEndpoint,
        event_proxy: T,
        window_id: WindowId,
    ) -> Result<(), String> {
        target.validate()?;
        source.validate()?;
        if target.instance == self.descriptor.instance
            || source.instance != self.descriptor.instance
        {
            return Err("window selection endpoint is invalid".into());
        }
        if selection_id == [0; 16] {
            return Err("window selection id is empty".into());
        }
        let sender = self.event_sender.clone();
        let listener = event_proxy
            .with_window_target(rio_backend::event::WindowTarget::dynamic(window_id));
        thread::Builder::new()
            .name("rio-window-selection-arm".into())
            .spawn(move || {
                tracing::info!(
                    target_window = target.native_window_id,
                    "sending native merge target arm request"
                );
                let result = send_arm_selection(&target, selection_id, &source);
                match &result {
                    Ok(()) => tracing::info!(
                        target_window = target.native_window_id,
                        "native merge target arm acknowledged"
                    ),
                    Err(error) => tracing::warn!(
                        target_window = target.native_window_id,
                        %error,
                        "native merge target arm request failed"
                    ),
                }
                let _ = sender.try_send(WindowControlEvent::ArmSelectionResult {
                    selection_id,
                    target,
                    result,
                });
                listener.send_event(RioEvent::Render, window_id);
            })
            .map_err(|error| format!("start window selection: {error}"))?;
        Ok(())
    }

    pub fn cancel_selection_async(
        &self,
        target: WindowEndpoint,
        selection_id: [u8; 16],
    ) -> Result<(), String> {
        target.validate()?;
        if selection_id == [0; 16] {
            return Err("window selection id is empty".into());
        }
        thread::Builder::new()
            .name("rio-window-selection-cancel".into())
            .spawn(move || {
                let _ = send_cancel_selection(&target, selection_id);
            })
            .map_err(|error| format!("start window selection cleanup: {error}"))?;
        Ok(())
    }

    pub fn send_selection_event_async(
        &self,
        source: WindowEndpoint,
        selection_id: [u8; 16],
        target_index: usize,
        clicked: bool,
    ) -> Result<(), String> {
        source.validate()?;
        let target_window = self.descriptor.native_window_id;
        let target_index = u32::try_from(target_index)
            .map_err(|_| "window selection target index is too large".to_string())?;
        thread::Builder::new()
            .name("rio-window-selection-event".into())
            .spawn(move || {
                let _ = send_selection_event(
                    &source,
                    selection_id,
                    target_window,
                    target_index,
                    clicked,
                );
            })
            .map_err(|error| format!("start window selection event: {error}"))?;
        Ok(())
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub fn publish_drag_offer(&self, offer: TransferOffer) -> Result<(), String> {
        offer.validate()?;
        let mut published = self
            .published
            .lock()
            .map_err(|_| "window offer registry is poisoned".to_string())?;
        if published.len() >= MAX_OFFERS && !published.contains_key(&offer.transfer_id) {
            return Err("window offer registry is full".into());
        }
        published.insert(offer.transfer_id, offer);
        Ok(())
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub fn withdraw_drag_offer(&self, transfer_id: [u8; 16]) {
        if let Ok(mut published) = self.published.lock() {
            published.remove(&transfer_id);
        }
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub fn take_drag_offer_async<T: EventListener + Clone + Send + 'static>(
        &self,
        transfer_id: [u8; 16],
        target_window_id: u64,
        target_index: usize,
        event_proxy: T,
        window_id: WindowId,
    ) -> Result<(), String> {
        if transfer_id == [0; 16] {
            return Err("empty drag transfer token".into());
        }
        let own_instance = self.descriptor.instance;
        let scope = self.descriptor.scope.clone();
        let sender = self.event_sender.clone();
        let listener = event_proxy
            .with_window_target(rio_backend::event::WindowTarget::dynamic(window_id));
        thread::Builder::new()
            .name("rio-window-drag-take".into())
            .spawn(move || {
                let result: Result<(), String> = (|| {
                    for target in Self::discover(&scope) {
                        if target.instance == own_instance {
                            continue;
                        }
                        if let Ok(Some((offer, mut stream))) =
                            take_offer(&target, transfer_id)
                        {
                            let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
                            sender
                                .try_send(WindowControlEvent::IncomingOffer {
                                    offer,
                                    reply: reply_sender,
                                    target_window: Some(target_window_id),
                                    target_index: Some(target_index),
                                })
                                .map_err(|_| {
                                    "window control queue is full".to_string()
                                })?;
                            listener.send_event(RioEvent::Render, window_id);
                            let response =
                                reply_receiver.recv_timeout(REPLY_TIMEOUT).map_err(
                                    |_| "drag transfer preparation timed out".to_string(),
                                )?;
                            codec::write_frame_until(
                                &mut stream,
                                &response,
                                Instant::now() + REPLY_TIMEOUT,
                            )
                            .map_err(|error| error.to_string())?;
                            return match response {
                                WindowControlResponse::Committed { routes }
                                    if !routes.is_empty() =>
                                {
                                    Ok(())
                                }
                                WindowControlResponse::Rejected(error) => Err(error),
                                _ => {
                                    Err("drag transfer did not commit any sessions"
                                        .into())
                                }
                            };
                        }
                    }
                    Err("no Rio window owns the drag transfer".into())
                })();
                let _ = sender.try_send(WindowControlEvent::DragResult {
                    transfer_id,
                    result,
                });
                listener.send_event(RioEvent::Render, window_id);
            })
            .map_err(|error| format!("start foreign drag transfer: {error}"))?;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for WindowControl {
    fn drop(&mut self) {
        self.acceptor_stop.store(true, Ordering::Release);
        self.acceptor_wakeup.signal();
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
            if !self.acceptor_stopped.load(Ordering::Acquire) {
                tracing::error!(
                    "window control acceptor exited without reporting shutdown"
                );
            }
        }
        let _ = std::fs::remove_file(&self.registry_path);
        let _ = std::fs::remove_file(&self.descriptor.endpoint);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rio_backend::event::VoidListener;
    use rio_session::{SessionDescriptor, SessionId};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn test_artifact_dir(prefix: &str) -> PathBuf {
        let root = std::env::var_os("RIO_ACCEPT_ARTIFACT_ROOT")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join("dev/rio-agent-artifacts"))
            })
            .unwrap_or_else(|| PathBuf::from("rio-agent-artifacts"));
        let path = root.join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            hex_id(&random_bytes::<8>().unwrap())
        ));
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn session_descriptor() -> SessionDescriptor {
        let root = test_artifact_dir("rio-window-control-test");
        SessionDescriptor {
            endpoint: root.join("session.sock"),
            capability: [7; 32],
            session_id: SessionId([9; 16]),
        }
    }

    #[test]
    fn authenticated_probe_requires_native_window_confirmation() {
        let control = WindowControl::new(VoidListener, WindowId::from(73)).unwrap();
        let endpoint = control.descriptor().clone();
        let probe = thread::spawn(move || {
            let mut stream = UnixStream::connect(&endpoint.endpoint).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            codec::write_frame(
                &mut stream,
                &Request::Hello {
                    version: VERSION,
                    capability: endpoint.capability,
                },
            )
            .unwrap();
            assert!(matches!(
                codec::read_frame(&mut stream).unwrap(),
                WindowControlResponse::Hello
            ));
            codec::write_frame(&mut stream, &Request::Probe).unwrap();
            assert!(matches!(
                codec::read_frame(&mut stream).unwrap(),
                WindowControlResponse::Rejected(_)
            ));
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut answered = false;
        while std::time::Instant::now() < deadline && !answered {
            for event in control.poll() {
                if let WindowControlEvent::Probe { window_id, reply } = event {
                    assert_eq!(window_id, WindowId::from(73));
                    reply
                        .send(WindowControlResponse::Rejected("window is gone".into()))
                        .unwrap();
                    answered = true;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        probe.join().unwrap();
        assert!(answered);
    }

    #[test]
    fn authenticated_two_process_shape_offer_preserves_requested_slots() {
        let first = WindowControl::new(VoidListener, WindowId::from(1)).unwrap();
        let second = WindowControl::new(VoidListener, WindowId::from(2)).unwrap();
        let offer = TransferOffer {
            transfer_id: [3; 16],
            panes: vec![PaneOffer {
                route_id: 41,
                tab_id: 42,
                layout_rect: [0.0, 0.0, 1.0, 1.0],
                session: session_descriptor(),
            }],
            tabs: vec![TabOffer {
                tab_id: 42,
                layout: LayoutNodeOffer {
                    route_id: 41,
                    direction: None,
                    flex_grow: 1.0,
                    children: Vec::new(),
                },
                active_route: 41,
            }],
            active_pane: 0,
        };
        for (target_index, transfer_id) in [(0, [3; 16]), (1, [4; 16]), (2, [5; 16])] {
            let mut offer = offer.clone();
            offer.transfer_id = transfer_id;
            first
                .offer_async(
                    second.descriptor().clone(),
                    offer,
                    Some(target_index),
                    VoidListener,
                    WindowId::from(1),
                )
                .unwrap();
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut received_indexes = HashSet::new();
        let mut results = 0;
        while std::time::Instant::now() < deadline && results < 3 {
            for event in second.poll() {
                if let WindowControlEvent::IncomingOffer {
                    reply,
                    target_index,
                    ..
                } = event
                {
                    received_indexes.insert(target_index.unwrap());
                    reply
                        .send(WindowControlResponse::Committed { routes: vec![41] })
                        .unwrap();
                }
            }
            for event in first.poll() {
                if let WindowControlEvent::OfferResult { result: value, .. } = event {
                    assert_eq!(value.unwrap(), vec![41]);
                    results += 1;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(received_indexes, [0, 1, 2].into_iter().collect());
        assert_eq!(results, 3);
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    #[test]
    fn published_drag_offer_returns_commit_to_source() {
        let source = WindowControl::new(VoidListener, WindowId::from(11)).unwrap();
        let target = WindowControl::new(VoidListener, WindowId::from(12)).unwrap();
        let offer = TransferOffer {
            transfer_id: [4; 16],
            panes: vec![PaneOffer {
                route_id: 51,
                tab_id: 52,
                layout_rect: [0.0, 0.0, 1.0, 1.0],
                session: session_descriptor(),
            }],
            tabs: vec![TabOffer {
                tab_id: 52,
                layout: LayoutNodeOffer {
                    route_id: 51,
                    direction: None,
                    flex_grow: 1.0,
                    children: Vec::new(),
                },
                active_route: 51,
            }],
            active_pane: 0,
        };
        source.publish_drag_offer(offer).unwrap();
        target
            .take_drag_offer_async([4; 16], 12, 0, VoidListener, WindowId::from(12))
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut result = None;
        while std::time::Instant::now() < deadline {
            for event in target.poll() {
                if let WindowControlEvent::IncomingOffer {
                    reply,
                    target_window,
                    ..
                } = event
                {
                    assert_eq!(target_window, Some(12));
                    reply
                        .send(WindowControlResponse::Committed { routes: vec![51] })
                        .unwrap();
                }
            }
            for event in source.poll() {
                if let WindowControlEvent::OfferResult { result: value, .. } = event {
                    result = Some(value);
                }
            }
            if result.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(result.unwrap().unwrap(), vec![51]);
        assert!(source.published.lock().unwrap().get(&[4; 16]).is_none());
    }

    #[test]
    fn drop_joins_acceptor_after_signaling_shutdown() {
        let control = WindowControl::new(VoidListener, WindowId::from(13)).unwrap();
        let stopped = control.acceptor_stopped.clone();
        drop(control);
        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn blocked_acceptor_stops_on_readiness_signal() {
        let root = test_artifact_dir("rio-window-control-blocked");
        let endpoint = root.join("control.sock");
        let listener = UnixListener::bind(&endpoint).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (sender, _receiver) = mpsc::sync_channel(1);
        let wakeup = Arc::new(Readiness::new().unwrap());
        let blocked = Arc::new(Readiness::new().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let acceptor = {
            let wakeup = wakeup.clone();
            let blocked = blocked.clone();
            let stop = stop.clone();
            let stopped = stopped.clone();
            thread::spawn(move || {
                accept_loop(
                    listener,
                    sender,
                    VoidListener,
                    WindowId::from(14),
                    [0; 32],
                    #[cfg(all(feature = "wayland", target_os = "linux"))]
                    Arc::new(Mutex::new(HashMap::new())),
                    AcceptorState {
                        stop,
                        wakeup,
                        stopped,
                        blocked: Some(blocked),
                    },
                )
            })
        };

        let mut poll_fds = [libc::pollfd {
            fd: blocked.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let ready = readiness::wait(
            &mut poll_fds,
            Some(std::time::Instant::now() + Duration::from_secs(2)),
        )
        .unwrap();
        blocked.clear();
        stop.store(true, Ordering::Release);
        wakeup.signal();
        acceptor.join().unwrap();

        assert_eq!(ready, 1);
        assert!(stopped.load(Ordering::Acquire));
        let _ = fs::remove_file(endpoint);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bootstrap_child_shares_source_window_identity() {
        use std::ffi::OsString;
        use std::io::Cursor;

        let exe = Path::new("/usr/bin/rio");
        let plain: Vec<OsString> = bootstrap_command(exe, None)
            .get_args()
            .map(ToOwned::to_owned)
            .collect();
        assert_eq!(plain, vec![OsString::from(WINDOW_BOOTSTRAP_FLAG)]);

        let grouped: Vec<OsString> = bootstrap_command(exe, Some("Rio"))
            .get_args()
            .map(ToOwned::to_owned)
            .collect();
        assert_eq!(
            grouped,
            vec![
                OsString::from(WINDOW_BOOTSTRAP_FLAG),
                OsString::from("--app-id"),
                OsString::from("Rio"),
            ]
        );

        let mut argv = vec![
            OsString::from("rio"),
            OsString::from(WINDOW_BOOTSTRAP_FLAG),
            OsString::from("--app-id"),
            OsString::from("Rio"),
        ];
        assert!(super::take_window_bootstrap_flag(&mut argv));
        assert_eq!(
            argv,
            vec![
                OsString::from("rio"),
                OsString::from("--app-id"),
                OsString::from("Rio")
            ]
        );

        assert!(super::validate_bootstrap_app_id("Rio").is_ok());
        assert!(super::validate_bootstrap_app_id("").is_err());

        let identity = super::read_bootstrap_identity(Cursor::new([7; 16])).unwrap();
        assert_eq!(identity, [7; 16]);
        assert!(super::read_bootstrap_identity(Cursor::new([0; 16])).is_err());
    }

    #[test]
    fn endpoint_debug_redacts_capability() {
        let endpoint_path =
            test_artifact_dir("rio-window-control-debug").join("control.sock");
        let endpoint = WindowEndpoint {
            instance: [1; 16],
            process_id: 1,
            native_window_id: 1,
            endpoint: endpoint_path.to_string_lossy().into_owned(),
            capability: [0xab; 32],
            scope: "test".into(),
        };
        let debug = format!("{endpoint:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("abab"));
        let encoded =
            bincode::encode_to_vec(&endpoint, bincode::config::standard()).unwrap();
        let (decoded, _): (WindowEndpoint, _) =
            bincode::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(decoded.process_id, endpoint.process_id);
        assert_eq!(decoded.native_window_id, endpoint.native_window_id);
    }

    #[test]
    fn descriptor_symlink_is_rejected() {
        let root = test_artifact_dir("rio-window-control-symlink");
        let target = root.join("target");
        let link = root.join("link.desc");
        fs::write(&target, b"not a descriptor").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_descriptor(&link).is_err());
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(not(unix))]
impl WindowControl {
    pub fn new<T>(_event_proxy: T, _window_id: WindowId) -> Result<Self, String> {
        Err("cross-window control is unsupported on this platform".into())
    }

    pub fn discover_peers_async<T>(
        &self,
        _listener: T,
        _window_id: WindowId,
    ) -> Result<(), String> {
        Err("cross-window control is unsupported on this platform".into())
    }

    pub fn poll(&self) -> Vec<WindowControlEvent> {
        Vec::new()
    }

    pub fn discover(_scope: &str) -> Vec<WindowEndpoint> {
        Vec::new()
    }

    pub fn offer_async<T: EventListener + Clone + Send + 'static>(
        &self,
        _target: WindowEndpoint,
        _offer: TransferOffer,
        _target_index: Option<usize>,
        _event_proxy: T,
        _window_id: WindowId,
    ) -> Result<(), String> {
        Err("cross-window control is unsupported on this platform".into())
    }

    pub fn launch_and_offer_async<T: EventListener + Clone + Send + 'static>(
        &self,
        _offer: TransferOffer,
        _event_proxy: T,
        _window_id: WindowId,
        _app_id: Option<String>,
    ) -> Result<(), String> {
        Err("cross-window control is unsupported on this platform".into())
    }

    pub fn publish_drag_offer(&self, _offer: TransferOffer) -> Result<(), String> {
        Err("cross-window control is unsupported on this platform".into())
    }

    pub fn withdraw_drag_offer(&self, _transfer_id: [u8; 16]) {}

    pub fn take_drag_offer_async<T: EventListener + Clone + Send + 'static>(
        &self,
        _transfer_id: [u8; 16],
        _target_window_id: u64,
        _target_index: usize,
        _event_proxy: T,
        _window_id: WindowId,
    ) -> Result<(), String> {
        Err("cross-window control is unsupported on this platform".into())
    }

    pub fn arm_selection_async<T: EventListener + Clone + Send + 'static>(
        &self,
        _target: WindowEndpoint,
        _selection_id: [u8; 16],
        _source: WindowEndpoint,
        _event_proxy: T,
        _window_id: WindowId,
    ) -> Result<(), String> {
        Err("cross-window control is unsupported on this platform".into())
    }

    pub fn cancel_selection_async(
        &self,
        _target: WindowEndpoint,
        _selection_id: [u8; 16],
    ) -> Result<(), String> {
        Err("cross-window control is unsupported on this platform".into())
    }

    pub fn send_selection_event_async(
        &self,
        _source: WindowEndpoint,
        _selection_id: [u8; 16],
        _target_index: usize,
        _clicked: bool,
    ) -> Result<(), String> {
        Err("cross-window control is unsupported on this platform".into())
    }
}

#[cfg(unix)]
struct AcceptorState {
    stop: Arc<AtomicBool>,
    wakeup: Arc<Readiness>,
    stopped: Arc<AtomicBool>,
    #[cfg(test)]
    blocked: Option<Arc<Readiness>>,
}

#[cfg(unix)]
fn accept_loop<T: EventListener + Clone + Send + 'static>(
    listener: UnixListener,
    sender: SyncSender<WindowControlEvent>,
    event_proxy: T,
    window_id: WindowId,
    expected_capability: [u8; 32],
    #[cfg(all(feature = "wayland", target_os = "linux"))] published: Arc<
        Mutex<HashMap<[u8; 16], TransferOffer>>,
    >,
    state: AcceptorState,
) {
    'acceptor: loop {
        if state.stop.load(Ordering::Acquire) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let sender = sender.clone();
                let event_proxy = event_proxy.clone();
                #[cfg(all(feature = "wayland", target_os = "linux"))]
                let published = published.clone();
                let _ = thread::Builder::new()
                    .name("rio-window-control-peer".into())
                    .spawn(move || {
                        handle_connection(
                            stream,
                            sender,
                            event_proxy,
                            window_id,
                            expected_capability,
                            #[cfg(all(feature = "wayland", target_os = "linux"))]
                            published,
                        )
                    });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => loop {
                #[cfg(test)]
                if let Some(blocked) = &state.blocked {
                    blocked.signal();
                }
                let mut poll_fds = [
                    libc::pollfd {
                        fd: listener.as_fd().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: state.wakeup.as_fd().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                if let Err(error) = readiness::wait(&mut poll_fds, None) {
                    tracing::debug!(%error, "window control accept wait failed");
                    break 'acceptor;
                }
                let wakeup_revents = poll_fds[1].revents;
                if readiness::is_invalid(wakeup_revents) {
                    tracing::debug!("window control accept wakeup became unusable");
                    break 'acceptor;
                }
                if readiness::is_readable(wakeup_revents)
                    && wakeup_revents & libc::POLLIN != 0
                {
                    state.wakeup.clear();
                    if state.stop.load(Ordering::Acquire) {
                        break 'acceptor;
                    }
                }
                if wakeup_revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    tracing::debug!("window control accept wakeup became unusable");
                    break 'acceptor;
                }
                let listener_revents = poll_fds[0].revents;
                if readiness::is_invalid(listener_revents) {
                    tracing::debug!("window control listener became invalid");
                    break 'acceptor;
                }
                if listener_revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    tracing::debug!("window control listener became unusable");
                    break 'acceptor;
                }
                if readiness::is_readable(listener_revents)
                    && listener_revents & libc::POLLIN != 0
                {
                    break;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::debug!(%error, "window control accept failed");
                break;
            }
        }
    }
    state.stopped.store(true, Ordering::Release);
}

#[cfg(unix)]
fn handle_connection<T: EventListener + Clone + Send + 'static>(
    mut stream: UnixStream,
    sender: SyncSender<WindowControlEvent>,
    event_proxy: T,
    window_id: WindowId,
    expected_capability: [u8; 32],
    #[cfg(all(feature = "wayland", target_os = "linux"))] published: Arc<
        Mutex<HashMap<[u8; 16], TransferOffer>>,
    >,
) {
    // BSD accept() already hands over a nonblocking socket, and the codec polls
    // between transfers, so a nonblocking stream is what both expect.
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    fn read_request(stream: &mut UnixStream) -> Option<Request> {
        match codec::read_frame_until(stream, Instant::now() + IO_TIMEOUT) {
            Ok(request) => Some(request),
            Err(error) => {
                tracing::debug!(%error, "window control request failed");
                None
            }
        }
    }
    fn send_response(stream: &mut UnixStream, response: &WindowControlResponse) -> bool {
        match codec::write_frame_until(stream, response, Instant::now() + IO_TIMEOUT) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(%error, "window control response failed");
                false
            }
        }
    }
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let Request::Hello {
        version,
        capability: presented_capability,
    } = request
    else {
        return;
    };
    if version != VERSION || presented_capability != expected_capability {
        return;
    }
    if !send_response(&mut stream, &WindowControlResponse::Hello) {
        return;
    }
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    match &request {
        Request::ArmSelection {
            selection_id,
            source,
        } => {
            tracing::info!(
                target_window = ?window_id,
                "received native merge target arm request"
            );
            if source.validate().is_err() || *selection_id == [0; 16] {
                send_response(
                    &mut stream,
                    &WindowControlResponse::Rejected(
                        "invalid window selection request".into(),
                    ),
                );
                return;
            }
            let (reply, response) = mpsc::sync_channel(1);
            if sender
                .try_send(WindowControlEvent::ArmSelection {
                    window_id,
                    selection_id: *selection_id,
                    source: source.clone(),
                    reply,
                })
                .is_err()
            {
                send_response(
                    &mut stream,
                    &WindowControlResponse::Rejected("target busy".into()),
                );
                return;
            }
            tracing::info!(target_window = ?window_id, "queued native merge target arm request");
            event_proxy.send_event(RioEvent::Render, window_id);
            let response = response.recv_timeout(IO_TIMEOUT).unwrap_or_else(|_| {
                WindowControlResponse::Rejected("target did not arm selection".into())
            });
            tracing::info!(target_window = ?window_id, "native merge target arm response ready");
            send_response(&mut stream, &response);
            return;
        }
        Request::CancelSelection { selection_id } => {
            let _ = sender.try_send(WindowControlEvent::CancelSelection {
                window_id,
                selection_id: *selection_id,
            });
            event_proxy.send_event(RioEvent::Render, window_id);
            send_response(&mut stream, &WindowControlResponse::SelectionCancelled);
            return;
        }
        Request::SelectionEvent {
            selection_id,
            target_window,
            target_index,
            clicked,
        } => {
            if *selection_id == [0; 16]
                || sender
                    .try_send(WindowControlEvent::Selection {
                        selection_id: *selection_id,
                        target_window: *target_window,
                        target_index: *target_index,
                        clicked: *clicked,
                    })
                    .is_err()
            {
                send_response(
                    &mut stream,
                    &WindowControlResponse::Rejected("source busy".into()),
                );
                return;
            }
            event_proxy.send_event(RioEvent::Render, window_id);
            send_response(&mut stream, &WindowControlResponse::Hello);
            return;
        }
        _ => {}
    }
    let (offer, target_index) = match request {
        Request::Offer {
            offer,
            target_index,
        } => {
            let target_index = match target_index.map(usize::try_from).transpose() {
                Ok(target_index) => target_index,
                Err(_) => {
                    send_response(
                        &mut stream,
                        &WindowControlResponse::Rejected(
                            "transfer target index is not representable".into(),
                        ),
                    );
                    return;
                }
            };
            (offer, target_index)
        }
        #[cfg(all(feature = "wayland", target_os = "linux"))]
        Request::Take(transfer_id) => {
            let offer = published
                .lock()
                .ok()
                .and_then(|mut offers| offers.remove(&transfer_id));
            let Some(offer) = offer else {
                send_response(
                    &mut stream,
                    &WindowControlResponse::Rejected(
                        "drag transfer is unavailable".into(),
                    ),
                );
                return;
            };
            if offer.validate().is_err()
                || !send_response(&mut stream, &WindowControlResponse::Offer(offer))
            {
                return;
            }
            // The user answers a drag, so this waits much longer than a probe.
            let result = match codec::read_frame_until::<WindowControlResponse>(
                &mut stream,
                Instant::now() + REPLY_TIMEOUT,
            ) {
                Ok(WindowControlResponse::Committed { routes }) => Ok(routes),
                Ok(WindowControlResponse::Rejected(reason)) => Err(reason),
                Ok(_) => Err("target returned an invalid drag commit response".into()),
                Err(error) => Err(error.to_string()),
            };
            tracing::info!(
                committed = result.is_ok(),
                "authenticated drag response received by source endpoint"
            );
            let _ = sender.try_send(WindowControlEvent::OfferResult {
                transfer_id,
                result,
            });
            event_proxy.send_event(RioEvent::Render, window_id);
            return;
        }
        #[cfg(not(all(feature = "wayland", target_os = "linux")))]
        Request::Take(_) => {
            send_response(
                &mut stream,
                &WindowControlResponse::Rejected("drag transfer is unavailable".into()),
            );
            return;
        }
        Request::Probe => {
            let (reply, response) = mpsc::sync_channel(1);
            if sender
                .try_send(WindowControlEvent::Probe { window_id, reply })
                .is_ok()
            {
                event_proxy.send_event(RioEvent::Render, window_id);
                if let Ok(response) = response.recv_timeout(IO_TIMEOUT) {
                    send_response(&mut stream, &response);
                }
            }
            return;
        }
        Request::Hello { .. } => return,
        Request::ArmSelection { .. }
        | Request::CancelSelection { .. }
        | Request::SelectionEvent { .. } => return,
    };
    if offer.validate().is_err() {
        send_response(
            &mut stream,
            &WindowControlResponse::Rejected("invalid transfer".into()),
        );
        return;
    }
    let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
    if sender
        .try_send(WindowControlEvent::IncomingOffer {
            offer,
            reply: reply_sender,
            target_window: Some(u64::from(window_id)),
            target_index,
        })
        .is_err()
    {
        send_response(
            &mut stream,
            &WindowControlResponse::Rejected("target busy".into()),
        );
        return;
    }
    event_proxy.send_event(RioEvent::Render, window_id);
    let response = reply_receiver
        .recv_timeout(REPLY_TIMEOUT)
        .unwrap_or_else(|_| {
            WindowControlResponse::Rejected("target did not prepare transfer".into())
        });
    send_response(&mut stream, &response);
}

/// Sends one authenticated request and reads the target's response.
#[cfg(unix)]
fn exchange(
    target: &WindowEndpoint,
    request: &Request,
    reply_timeout: Duration,
) -> Result<WindowControlResponse, String> {
    let mut stream = authenticated_stream(target)?;
    codec::write_frame_until(&mut stream, request, Instant::now() + IO_TIMEOUT)
        .map_err(|error| error.to_string())?;
    codec::read_frame_until::<WindowControlResponse>(
        &mut stream,
        Instant::now() + reply_timeout,
    )
    .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn send_offer(
    target: &WindowEndpoint,
    offer: &TransferOffer,
    target_index: Option<u32>,
) -> Result<Vec<u64>, String> {
    match exchange(
        target,
        &Request::Offer {
            offer: offer.clone(),
            target_index,
        },
        REPLY_TIMEOUT,
    )? {
        WindowControlResponse::Committed { routes } => Ok(routes),
        WindowControlResponse::Rejected(reason) => Err(reason),
        _ => Err("target returned an invalid transfer response".into()),
    }
}

#[cfg(unix)]
fn authenticated_stream(endpoint: &WindowEndpoint) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(&endpoint.endpoint)
        .map_err(|error| format!("connect window control endpoint: {error}"))?;
    stream
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    codec::write_frame_until(
        &mut stream,
        &Request::Hello {
            version: VERSION,
            capability: endpoint.capability,
        },
        Instant::now() + IO_TIMEOUT,
    )
    .map_err(|error| error.to_string())?;
    match codec::read_frame_until::<WindowControlResponse>(
        &mut stream,
        Instant::now() + IO_TIMEOUT,
    )
    .map_err(|error| error.to_string())?
    {
        WindowControlResponse::Hello => Ok(stream),
        _ => Err("window control authentication was rejected".into()),
    }
}

#[cfg(unix)]
fn send_arm_selection(
    target: &WindowEndpoint,
    selection_id: [u8; 16],
    source: &WindowEndpoint,
) -> Result<(), String> {
    match exchange(
        target,
        &Request::ArmSelection {
            selection_id,
            source: source.clone(),
        },
        IO_TIMEOUT,
    )? {
        WindowControlResponse::SelectionArmed => Ok(()),
        WindowControlResponse::Rejected(reason) => Err(reason),
        _ => Err("target returned an invalid selection response".into()),
    }
}

#[cfg(unix)]
fn send_cancel_selection(
    target: &WindowEndpoint,
    selection_id: [u8; 16],
) -> Result<(), String> {
    match exchange(
        target,
        &Request::CancelSelection { selection_id },
        IO_TIMEOUT,
    )? {
        WindowControlResponse::SelectionCancelled => Ok(()),
        WindowControlResponse::Rejected(reason) => Err(reason),
        _ => Err("target returned an invalid selection cleanup response".into()),
    }
}

#[cfg(unix)]
fn send_selection_event(
    source: &WindowEndpoint,
    selection_id: [u8; 16],
    target_window: u64,
    target_index: u32,
    clicked: bool,
) -> Result<(), String> {
    match exchange(
        source,
        &Request::SelectionEvent {
            selection_id,
            target_window,
            target_index,
            clicked,
        },
        IO_TIMEOUT,
    )? {
        WindowControlResponse::Hello => Ok(()),
        WindowControlResponse::Rejected(reason) => Err(reason),
        _ => Err("source returned an invalid selection event response".into()),
    }
}

#[cfg(all(feature = "wayland", target_os = "linux"))]
fn take_offer(
    target: &WindowEndpoint,
    transfer_id: [u8; 16],
) -> Result<Option<(TransferOffer, UnixStream)>, String> {
    let mut stream = authenticated_stream(target)?;
    codec::write_frame_until(
        &mut stream,
        &Request::Take(transfer_id),
        Instant::now() + IO_TIMEOUT,
    )
    .map_err(|error| error.to_string())?;
    match codec::read_frame_until::<WindowControlResponse>(
        &mut stream,
        Instant::now() + IO_TIMEOUT,
    )
    .map_err(|error| error.to_string())?
    {
        WindowControlResponse::Offer(offer) => {
            offer.validate()?;
            Ok(Some((offer, stream)))
        }
        WindowControlResponse::Rejected(reason)
            if reason == "drag transfer is unavailable" =>
        {
            Ok(None)
        }
        WindowControlResponse::Rejected(reason) => Err(reason),
        _ => Err("source returned an invalid drag response".into()),
    }
}

#[cfg(unix)]
fn registry_root() -> Result<PathBuf, String> {
    let base = dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .ok_or_else(|| "no private runtime directory is available".to_string())?;
    Ok(base.join("rio").join("windows"))
}

#[cfg(unix)]
fn display_scope() -> String {
    std::env::var("WAYLAND_DISPLAY")
        .or_else(|_| std::env::var("DISPLAY"))
        .or_else(|_| std::env::var("SWAYSOCK"))
        .unwrap_or_else(|_| "default".into())
}

#[cfg(unix)]
fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("generate window capability: {error}"))?;
    Ok(bytes)
}

fn hex_id(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(unix)]
fn write_descriptor(path: &Path, descriptor: &WindowEndpoint) -> Result<(), String> {
    descriptor.validate()?;
    let bytes = codec::encode(descriptor).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("window descriptor exceeds the bounded limit".into());
    }
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(path)
        .map_err(|error| format!("write window descriptor: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write window descriptor: {error}"))
}

#[cfg(unix)]
fn read_descriptor(path: &Path) -> Result<WindowEndpoint, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("window descriptor is not private".into());
    }
    if metadata.len() > MAX_FRAME_BYTES as u64 {
        return Err("window descriptor is too large".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_FRAME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("window descriptor is too large".into());
    }
    let descriptor: WindowEndpoint =
        codec::decode(&bytes).map_err(|error| error.to_string())?;
    descriptor.validate()?;
    Ok(descriptor)
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect window registry: {error}"))?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("window registry is not private".into());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("protect window registry: {error}"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect window registry: {error}"))?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("window registry is not private".into());
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_private_registry(root: &Path) -> Result<(), String> {
    let parent = root
        .parent()
        .ok_or_else(|| "window registry has no parent".to_string())?;
    for directory in [parent, root] {
        match std::fs::create_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create window registry: {error}")),
        }
        set_private_directory(directory)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("protect window control file: {error}"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect window control file: {error}"))?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err("window control file is not private".into());
    }
    Ok(())
}

#[cfg(unix)]
fn is_private_directory(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
}
