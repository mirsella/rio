#[cfg(feature = "pty")]
pub mod capi;

pub mod key;
mod render_state;

pub use key::{
    encode as encode_key, EncodeContext, Key, KeyAction, KeyEvent, KittyFlags, Modifiers,
};
pub use render_state::{RenderState, SurfaceSnapshot, ViewportSelection};
pub use rio_vt::clipboard::ClipboardType;
pub use rio_vt::config::colors::term::TermColors;
pub use rio_vt::config::colors::{AnsiColor, ColorRgb, NamedColor};
pub use rio_vt::crosswords::grid::row::Row;
pub use rio_vt::crosswords::pos::Column;
pub use rio_vt::crosswords::square::{Extras, Square};
pub use rio_vt::crosswords::style::{Style, StyleFlags};
pub use rio_vt::grapheme_lut::cluster_width;
pub use rio_vt::selection::SelectionRange;

use rio_vt::ansi::graphics::{KittyPlacement, VirtualPlacement};
pub use rio_vt::ansi::CursorShape;
pub use rio_vt::crosswords::pos::Side;
use rio_vt::crosswords::pos::{Column as PosColumn, Line, Pos};
use rio_vt::crosswords::{Crosswords, Mode};
use rio_vt::event::sync::FairMutex;
#[cfg(feature = "pty")]
use rio_vt::event::Msg;
#[cfg(feature = "pty")]
use rio_vt::event::WindowSize;
use rio_vt::event::{EventListener, InputBudget, InputBudgetError, RioEvent, WindowId};
#[cfg(feature = "pty")]
use rio_vt::performer::Machine;
use rio_vt::selection::{Selection, SelectionType};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::error::Error;
#[cfg(feature = "pty")]
use std::sync::atomic::AtomicU8;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(all(feature = "pty", target_os = "windows"))]
use teletypewriter::create_pty;
#[cfg(all(feature = "pty", not(target_os = "windows")))]
use teletypewriter::{create_pty_with_spawn, create_pty_with_spawn_clear_env};

pub type SurfaceId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Word,
    Line,
    Block,
}

impl SelectionKind {
    fn to_type(self) -> SelectionType {
        match self {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
            SelectionKind::Block => SelectionType::Block,
        }
    }
}

struct GridSize {
    rows: usize,
    cols: usize,
    cell_width: f32,
    cell_height: f32,
}

impl GridSize {
    /// Cell metrics come from the host's pixel size; graphics protocols
    /// (kitty image placements) need them to map pixels onto cells, so a
    /// zero pixel size would silently drop every placement.
    fn new(cols: usize, rows: usize, pixel_width: u16, pixel_height: u16) -> Self {
        let cell = |pixels: u16, cells: usize| {
            if pixels == 0 || cells == 0 {
                0.
            } else {
                pixels as f32 / cells as f32
            }
        };
        Self {
            rows,
            cols,
            cell_width: cell(pixel_width, cols),
            cell_height: cell(pixel_height, rows),
        }
    }
}

impl rio_vt::crosswords::grid::Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }

    fn square_width(&self) -> f32 {
        self.cell_width
    }

    fn square_height(&self) -> f32 {
        self.cell_height
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    SetTitle {
        title: String,
        subtitle: Option<String>,
    },
    RingBell,
    CursorBlinkingChange,
    /// OSC 9;4 progress (ConEmu numbering): 0 remove, 1 set, 2 error,
    /// 3 indeterminate, 4 paused. `value` is 0-100 where the state
    /// carries one.
    Progress {
        state: u8,
        value: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputError {
    WouldBlock,
    Disconnected,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WouldBlock => formatter.write_str("PTY input queue is full"),
            Self::Disconnected => formatter.write_str("PTY input queue is disconnected"),
        }
    }
}

impl Error for InputError {}

/// A copy of active terminal graphics metadata for a session frontend.
/// Process-local timestamps are deliberately omitted; frame sequences and
/// image identity are the cache keys across a process boundary.
#[derive(Debug, Clone)]
pub struct GraphicsSnapshot {
    pub kitty_images: Vec<(u32, rio_graphics::GraphicData)>,
    pub kitty_placements: Vec<((u32, u32), KittyPlacement)>,
    pub kitty_virtual_placements: Vec<((u32, u32), VirtualPlacement)>,
    pub atlas_placements: Vec<rio_vt::ansi::graphics::AtlasPlacement>,
}

/// The parser hands atlas pixels to its event listener because the terminal
/// drops them after `UpdateGraphics` is emitted. Keep those pixels at the
/// surface boundary until a renderer consumes them. This is keyed, bounded
/// storage rather than an event mailbox: retransmitting one image replaces
/// its old bytes and removals release them.
const MAX_RETAINED_GRAPHICS_BYTES: usize = 320 * 1024 * 1024;
const MAX_RETAINED_GRAPHICS_ITEMS: usize = 4096;

#[derive(Default)]
pub(crate) struct GraphicsUpdateStore {
    atlas: HashMap<u64, rio_graphics::GraphicData>,
    removed: HashSet<u64>,
    bytes: usize,
    over_budget: bool,
    removals_over_budget: bool,
}

impl GraphicsUpdateStore {
    fn merge(&mut self, queues: rio_vt::ansi::graphics::UpdateQueues) {
        for graphic in queues.pending {
            let key = rio_graphics::atlas_image_key(graphic.id.get());
            let old_bytes = self.atlas.get(&key).map_or(0, |old| old.pixels.len());
            if !self.atlas.contains_key(&key)
                && self.atlas.len() >= MAX_RETAINED_GRAPHICS_ITEMS
            {
                self.over_budget = true;
                continue;
            }
            let Some(bytes) = self
                .bytes
                .checked_sub(old_bytes)
                .and_then(|bytes| bytes.checked_add(graphic.pixels.len()))
            else {
                self.over_budget = true;
                continue;
            };
            if bytes > MAX_RETAINED_GRAPHICS_BYTES {
                self.over_budget = true;
                continue;
            }
            self.bytes = bytes;
            self.atlas.insert(key, graphic);
            self.removed.remove(&key);
        }

        // Kitty uploads remain authoritative in Crosswords::kitty_images and
        // are copied by the atomic render snapshot. Dropping this duplicate
        // queue avoids retaining every retransmission twice.
        drop(queues.pending_images);

        for key in queues.remove_queue {
            if let Some(graphic) = self.atlas.remove(&key) {
                self.bytes =
                    self.bytes
                        .checked_sub(graphic.pixels.len())
                        .unwrap_or_else(|| {
                            self.over_budget = true;
                            0
                        });
            }
            if !self.removed.contains(&key)
                && self.removed.len() >= MAX_RETAINED_GRAPHICS_ITEMS
            {
                self.over_budget = true;
                self.removals_over_budget = true;
                continue;
            }
            self.removed.insert(key);
        }
    }

    fn take(&mut self) -> Option<rio_vt::ansi::graphics::UpdateQueues> {
        if self.atlas.is_empty() && self.removed.is_empty() {
            return None;
        }
        self.bytes = 0;
        let mut pending = self
            .atlas
            .drain()
            .map(|(_, graphic)| graphic)
            .collect::<Vec<_>>();
        pending.sort_by_key(|graphic| graphic.id.get());
        let mut remove_queue = self.removed.drain().collect::<Vec<_>>();
        remove_queue.sort_unstable();
        Some(rio_vt::ansi::graphics::UpdateQueues {
            pending,
            pending_images: Vec::new(),
            remove_queue,
        })
    }

    fn take_with_over_budget(
        &mut self,
    ) -> (Option<rio_vt::ansi::graphics::UpdateQueues>, bool, bool) {
        let over_budget = self.over_budget;
        let removals_over_budget = self.removals_over_budget;
        let updates = self.take();
        self.over_budget = false;
        self.removals_over_budget = false;
        (updates, over_budget, removals_over_budget)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphicsSnapshotError {
    pub required_bytes: usize,
    pub limit_bytes: usize,
}

impl std::fmt::Display for GraphicsSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "terminal graphics require {} bytes, exceeding the {} byte snapshot budget",
            self.required_bytes, self.limit_bytes
        )
    }
}

impl Error for GraphicsSnapshotError {}

fn active_graphics_bytes_locked(
    terminal: &Crosswords<Listener>,
    limit_bytes: usize,
) -> Result<usize, GraphicsSnapshotError> {
    let mut required_bytes = 0usize;
    for image in terminal.graphics.kitty_images.values() {
        required_bytes = required_bytes.checked_add(image.data.pixels.len()).ok_or(
            GraphicsSnapshotError {
                required_bytes: usize::MAX,
                limit_bytes,
            },
        )?;
        if required_bytes > limit_bytes {
            return Err(GraphicsSnapshotError {
                required_bytes,
                limit_bytes,
            });
        }
    }
    Ok(required_bytes)
}

fn graphics_snapshot_locked(terminal: &Crosswords<Listener>) -> GraphicsSnapshot {
    GraphicsSnapshot {
        kitty_images: terminal
            .graphics
            .kitty_images
            .iter()
            .map(|(id, image)| (*id, image.data.clone()))
            .collect(),
        kitty_placements: terminal
            .graphics
            .kitty_placements
            .iter()
            .map(|(key, placement)| (*key, placement.clone()))
            .collect(),
        kitty_virtual_placements: terminal
            .graphics
            .kitty_virtual_placements
            .iter()
            .map(|(key, placement)| (*key, placement.clone()))
            .collect(),
        atlas_placements: terminal.graphics.atlas_placements.clone(),
    }
}

fn atlas_keys_locked(terminal: &Crosswords<Listener>) -> Vec<u64> {
    terminal
        .graphics
        .atlas_key_refs
        .keys()
        .chain(
            terminal
                .graphics
                .kitty_inactive_screen
                .atlas_key_refs
                .keys(),
        )
        .copied()
        .collect()
}

/// `Send + Sync` everywhere threads exist. On wasm there is one thread and
/// delegates hold JS callbacks (which are `!Send`), so the bound relaxes to
/// nothing rather than forcing unsafe impls on the embedder.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync> MaybeSendSync for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSendSync {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSendSync for T {}

pub trait SurfaceDelegate: MaybeSendSync + 'static {
    fn wakeup(&self, surface: SurfaceId);
    fn action(&self, _surface: SurfaceId, _action: Action) {}
    fn clipboard_write(&self, _surface: SurfaceId, _kind: ClipboardType, _text: String) {}
    /// A terminal requested clipboard/selection contents. The formatter is
    /// deliberately kept in-process; session transports turn this callback
    /// into a bounded request ID before it crosses a wire.
    fn clipboard_load(
        &self,
        _surface: SurfaceId,
        _route: SurfaceId,
        _kind: ClipboardType,
        _format: Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) {
    }
    fn color_request(
        &self,
        _surface: SurfaceId,
        _route: SurfaceId,
        _index: usize,
        _format: Arc<dyn Fn(ColorRgb) -> String + Send + Sync>,
    ) {
    }
    fn text_area_size_request(
        &self,
        _surface: SurfaceId,
        _route: SurfaceId,
        _format: Arc<dyn Fn(rio_vt::event::WindowSize) -> String + Send + Sync>,
    ) {
    }
    fn glyph_protocol_query(&self, _surface: SurfaceId, _route: SurfaceId, _cp: u32) {}
    fn desktop_notification(&self, _surface: SurfaceId, _title: String, _body: String) {}
    fn color_change(
        &self,
        _surface: SurfaceId,
        _route: SurfaceId,
        _index: usize,
        _color: Option<ColorRgb>,
    ) {
    }
    fn close_surface(&self, _surface: SurfaceId) {}
    fn child_exited(&self, _surface: SurfaceId, _status: Option<i32>) {}
    /// Bytes the terminal wants delivered to the child process. Only called
    /// on non-`pty` builds, where the host owns the transport (a WebSocket
    /// to a real shell, an in-page demo interpreter, ...); with a PTY the
    /// bytes go straight to it and this never fires.
    fn output(&self, _surface: SurfaceId, _bytes: &[u8]) {}
}

#[derive(Clone)]
pub(crate) struct Listener {
    surface_id: SurfaceId,
    delegate: Arc<dyn SurfaceDelegate>,
    graphics_updates: Arc<Mutex<GraphicsUpdateStore>>,
    #[cfg(feature = "pty")]
    pty_writer: Arc<Mutex<Option<corcovado::channel::Sender<Msg>>>>,
    #[cfg(feature = "pty")]
    input_budget: Option<InputBudget>,
    #[cfg(feature = "pty")]
    input_error: Arc<AtomicU8>,
}

impl Listener {
    fn dispatch(&self, event: RioEvent) {
        match event {
            RioEvent::TerminalDamaged(_)
            | RioEvent::Render
            | RioEvent::RenderRoute(_) => {
                self.delegate.wakeup(self.surface_id);
            }
            RioEvent::Title(_, title) => {
                self.delegate.action(
                    self.surface_id,
                    Action::SetTitle {
                        title,
                        subtitle: None,
                    },
                );
            }
            RioEvent::Bell => {
                self.delegate.action(self.surface_id, Action::RingBell);
            }
            RioEvent::CursorBlinkingChange | RioEvent::CursorBlinkingChangeOnRoute(_) => {
                self.delegate
                    .action(self.surface_id, Action::CursorBlinkingChange);
            }
            RioEvent::ClipboardStore(kind, text) => {
                self.delegate.clipboard_write(self.surface_id, kind, text);
            }
            RioEvent::PtyWrite(route_id, text) => {
                if route_id != self.surface_id {
                    tracing::error!(
                        surface = self.surface_id,
                        route = route_id,
                        "discarding PTY reply addressed to another surface"
                    );
                    self.delegate.wakeup(self.surface_id);
                    return;
                }
                #[cfg(feature = "pty")]
                if let Some(channel) = self.pty_writer.lock().unwrap().as_ref() {
                    let input: Cow<'static, [u8]> = Cow::Owned(text.into_bytes());
                    let result = if let Some(budget) = &self.input_budget {
                        match budget.try_reserve(input.len()) {
                            Ok(reservation) => channel
                                .send(Msg::InputBounded { input, reservation })
                                .map_err(|_| InputError::Disconnected),
                            Err(InputBudgetError::WouldBlock) => {
                                Err(InputError::WouldBlock)
                            }
                        }
                    } else {
                        channel
                            .send(Msg::Input(input))
                            .map_err(|_| InputError::Disconnected)
                    };
                    if let Err(error) = result {
                        let code = match error {
                            InputError::WouldBlock => 1,
                            InputError::Disconnected => 2,
                        };
                        let _ = self.input_error.compare_exchange(
                            0,
                            code,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        self.delegate.wakeup(self.surface_id);
                    }
                }
                #[cfg(not(feature = "pty"))]
                self.delegate.output(self.surface_id, text.as_bytes());
            }
            RioEvent::ClipboardLoad(route_id, kind, format) => {
                self.delegate
                    .clipboard_load(self.surface_id, route_id, kind, format);
            }
            RioEvent::ColorRequest(route_id, index, format) => {
                self.delegate
                    .color_request(self.surface_id, route_id, index, format);
            }
            RioEvent::TextAreaSizeRequest(route_id, format) => {
                self.delegate
                    .text_area_size_request(self.surface_id, route_id, format);
            }
            RioEvent::GlyphProtocolQuery { route_id, cp } => {
                self.delegate
                    .glyph_protocol_query(self.surface_id, route_id, cp);
            }
            RioEvent::DesktopNotification { title, body } => {
                self.delegate
                    .desktop_notification(self.surface_id, title, body);
            }
            RioEvent::ColorChange(route_id, index, color) => {
                self.delegate
                    .color_change(self.surface_id, route_id, index, color);
                // Color changes alter the passive frame/palette even when the
                // parser did not emit a separate terminal-damage event.
                self.delegate.wakeup(self.surface_id);
            }
            RioEvent::CloseTerminal(_) | RioEvent::Exit => {
                self.delegate.close_surface(self.surface_id);
            }
            RioEvent::ChildExited(_, status) => {
                self.delegate.child_exited(self.surface_id, status);
            }
            RioEvent::UpdateGraphics { queues, .. } => {
                self.graphics_updates.lock().unwrap().merge(queues);
                self.delegate.wakeup(self.surface_id);
            }
            RioEvent::ProgressReport(report) => {
                use rio_vt::event::ProgressState;
                let state = match report.state {
                    ProgressState::Remove => 0,
                    ProgressState::Set => 1,
                    ProgressState::Error => 2,
                    ProgressState::Indeterminate => 3,
                    ProgressState::Pause => 4,
                };
                self.delegate.action(
                    self.surface_id,
                    Action::Progress {
                        state,
                        value: report.progress.unwrap_or(0),
                    },
                );
            }
            _ => {}
        }
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: RioEvent, _id: WindowId) {
        self.dispatch(event);
    }

    fn send_event_with_high_priority(&self, event: RioEvent, _id: WindowId) {
        self.dispatch(event);
    }
}

#[derive(Debug, Clone)]
pub struct SurfaceDesc {
    pub shell: Option<String>,
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
    pub scrollback: usize,
    /// Optional child environment. With `clear_environment`, this replaces
    /// the worker environment instead of overlaying it.
    pub environment: Option<Vec<(String, String)>>,
    pub clear_environment: bool,
    /// Optional bound for bytes queued between the host and the PTY. `None`
    /// preserves the historical unbounded embedder behavior.
    pub input_queue_limit: Option<usize>,
}

impl Default for SurfaceDesc {
    fn default() -> Self {
        Self {
            shell: None,
            args: Vec::new(),
            working_dir: None,
            cols: 80,
            rows: 24,
            pixel_width: 720,
            pixel_height: 432,
            scrollback: 10_000,
            environment: None,
            clear_environment: false,
            input_queue_limit: None,
        }
    }
}

pub struct Engine {
    delegate: Arc<dyn SurfaceDelegate>,
    next_surface_id: AtomicUsize,
}

impl Engine {
    pub fn new(delegate: Arc<dyn SurfaceDelegate>) -> Self {
        Self {
            delegate,
            next_surface_id: AtomicUsize::new(1),
        }
    }

    pub fn create_surface(
        &self,
        desc: &SurfaceDesc,
    ) -> Result<Surface, Box<dyn Error + Send + Sync>> {
        Surface::new(self, desc)
    }
}

/// Translate the terminal's kitty keyboard flags into the encoder's own set.
/// Only the flags that change what a key produces are carried across; see the
/// note in [`key`] about the two that are not implemented.
fn kitty_flags(modes: rio_vt::ansi::KeyboardModes) -> key::KittyFlags {
    use rio_vt::ansi::KeyboardModes;
    let mut flags = key::KittyFlags::empty();
    if modes.contains(KeyboardModes::DISAMBIGUATE_ESC_CODES) {
        flags |= key::KittyFlags::DISAMBIGUATE;
    }
    if modes.contains(KeyboardModes::REPORT_EVENT_TYPES) {
        flags |= key::KittyFlags::REPORT_EVENT_TYPES;
    }
    if modes.contains(KeyboardModes::REPORT_ALL_KEYS_AS_ESC) {
        flags |= key::KittyFlags::REPORT_ALL_AS_ESC;
    }
    flags
}

/// Scheme-prefixed URL pattern for plain-text link detection. The engine
/// is regex-automata (no lookbehind), so trailing prose punctuation is
/// trimmed afterwards by `trailing_url_punctuation`.
const URL_REGEX: &str = "(?:https://|http://|mailto:|ftp://|file:|ssh://|ssh:|git://|tel:|magnet:|ipfs://|ipns://|gemini://|gopher://|news:)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>\"\\s{|}\\^⟨⟩`]+";

/// How many trailing characters of a URL match are prose punctuation
/// rather than URL: `.,;:!?'"` always, and a closing paren/bracket only
/// when its opener is not part of the match.
fn trailing_url_punctuation(text: &str) -> usize {
    let mut len = text.len();
    let mut trimmed = 0;
    while let Some(c) = text[..len].chars().last() {
        let cut = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => text[..len].matches('(').count() < text[..len].matches(')').count(),
            ']' => text[..len].matches('[').count() < text[..len].matches(']').count(),
            _ => false,
        };
        if !cut {
            break;
        }
        len -= c.len_utf8();
        trimmed += 1;
    }
    trimmed
}

pub struct Surface {
    id: SurfaceId,
    alt_is_meta: std::sync::atomic::AtomicBool,
    terminal: Arc<FairMutex<Crosswords<Listener>>>,
    graphics_updates: Arc<Mutex<GraphicsUpdateStore>>,
    /// Compiled URL detector, built on first hover. Hit-testing runs per
    /// pointer event, so the four lazy DFAs must not be rebuilt each time.
    url_regex: std::sync::Mutex<Option<rio_vt::crosswords::search::RegexSearch>>,
    /// VT parser for host-injected output. Persistent so escape sequences
    /// split across `inject_output` calls resume mid-sequence instead of
    /// mis-parsing (and so its sync buffer is allocated once, not per
    /// write).
    processor: std::sync::Mutex<Option<rio_vt::performer::handler::Processor>>,
    /// Non-pty transport: `write` hands the bytes to the delegate instead
    /// of a PTY channel.
    #[cfg(not(feature = "pty"))]
    delegate: Arc<dyn SurfaceDelegate>,
    #[cfg(feature = "pty")]
    channel: corcovado::channel::Sender<Msg>,
    #[cfg(feature = "pty")]
    shell_pid: u32,
    #[cfg(all(feature = "pty", not(target_os = "windows")))]
    main_fd: std::os::fd::RawFd,
    #[cfg(all(feature = "pty", not(target_os = "windows")))]
    child_terminator: teletypewriter::ChildTerminator,
    #[cfg(feature = "pty")]
    reap_child_on_drop: bool,
    #[cfg(feature = "pty")]
    input_budget: Option<InputBudget>,
    #[cfg(feature = "pty")]
    input_error: Arc<AtomicU8>,
    #[cfg(feature = "pty")]
    _io_thread: Option<
        std::thread::JoinHandle<(
            Machine<teletypewriter::Pty, Listener>,
            rio_vt::performer::State,
        )>,
    >,
}

/// Encode one mouse report. SGR (`CSI < b ; x ; y M`) when the program
/// asked for it, else the original X10 form, whose coordinates are
/// offset by 32 and cannot exceed 223 without the UTF-8 extension.
fn mouse_report(
    button: u8,
    col: u16,
    row: u16,
    pressed: bool,
    sgr: bool,
    utf8: bool,
) -> Vec<u8> {
    let x = col.saturating_add(1);
    let y = row.saturating_add(1);
    if sgr {
        // SGR keeps the real button and marks a release with lowercase `m`.
        let end = if pressed { 'M' } else { 'm' };
        return format!("\x1b[<{button};{x};{y}{end}").into_bytes();
    }
    // The X10/normal form has no release code: the button field collapses to
    // 3, but the modifier/motion bits (bit 3 and up) are kept, matching xterm.
    let encoded = if pressed { button } else { button | 3 };
    let mut out = vec![0x1b, b'[', b'M', 32u8.saturating_add(encoded)];
    for value in [x, y] {
        if utf8 && value >= 95 {
            // Two-byte UTF-8 for the extended range.
            let encoded = char::from_u32(32 + value as u32).unwrap_or('\u{20}');
            let mut buffer = [0u8; 4];
            out.extend_from_slice(encoded.encode_utf8(&mut buffer).as_bytes());
        } else {
            out.push(32u8.saturating_add(value.min(223) as u8));
        }
    }
    out
}

fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let filtered = text.replace(['\x1b', '\x03', '\u{9b}'], "");
        format!("\x1b[200~{filtered}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

impl Surface {
    fn new(
        engine: &Engine,
        desc: &SurfaceDesc,
    ) -> Result<Surface, Box<dyn Error + Send + Sync>> {
        let id = engine.next_surface_id.fetch_add(1, Ordering::SeqCst);
        let graphics_updates = Arc::new(Mutex::new(GraphicsUpdateStore::default()));
        #[cfg(feature = "pty")]
        let pty_writer = Arc::new(Mutex::new(None));
        let listener = Listener {
            surface_id: id,
            delegate: engine.delegate.clone(),
            graphics_updates: Arc::clone(&graphics_updates),
            #[cfg(feature = "pty")]
            pty_writer: pty_writer.clone(),
            #[cfg(feature = "pty")]
            input_budget: desc.input_queue_limit.map(InputBudget::new),
            #[cfg(feature = "pty")]
            input_error: Arc::new(AtomicU8::new(0)),
        };

        let terminal = Crosswords::new(
            GridSize::new(
                desc.cols as usize,
                desc.rows as usize,
                desc.pixel_width,
                desc.pixel_height,
            ),
            CursorShape::Block,
            listener.clone(),
            WindowId::from(id as u64),
            id,
            desc.scrollback,
        );
        // On wasm the delegate (and so the whole graph) is single-threaded
        // by design; Arc stays because the pty build shares it with the IO
        // thread and the API is one type on every target.
        #[allow(clippy::arc_with_non_send_sync)]
        let terminal = Arc::new(FairMutex::new(terminal));

        #[cfg(not(feature = "pty"))]
        {
            Ok(Surface {
                id,
                // Terminals default alt to meta; the host may override it.
                alt_is_meta: std::sync::atomic::AtomicBool::new(true),
                terminal,
                graphics_updates,
                url_regex: std::sync::Mutex::new(None),
                processor: std::sync::Mutex::new(None),
                delegate: engine.delegate.clone(),
            })
        }

        #[cfg(feature = "pty")]
        {
            // No shell in the descriptor means "whatever the user's default is",
            // which teletypewriter resolves (and starts as a login shell).
            let shell = desc.shell.as_deref();

            // The child inherits the host process's environment, which for GUI
            // hosts has no TERM at all (or a stale one). Resolve it the way rio
            // does: prefer rio's terminfo when it's installed, else fall back to
            // the universally known xterm-256color so local prompts and remote
            // ssh sessions both keep working.
            #[cfg(not(target_os = "windows"))]
            let env = {
                let terminfo = match (
                    teletypewriter::terminfo_exists_with_environment(
                        "xterm-rio",
                        desc.environment.as_deref(),
                        desc.clear_environment,
                    ),
                    teletypewriter::terminfo_exists_with_environment(
                        "rio",
                        desc.environment.as_deref(),
                        desc.clear_environment,
                    ),
                ) {
                    (true, _) => "xterm-rio",
                    (false, true) => "rio",
                    (false, false) => "xterm-256color",
                };
                Some(vec![
                    ("TERM".to_string(), terminfo.to_string()),
                    ("COLORTERM".to_string(), "truecolor".to_string()),
                ])
            };

            #[cfg(not(target_os = "windows"))]
            let pty = {
                let mut env = env;
                if let Some(extra) = desc.environment.clone() {
                    env.get_or_insert_with(Vec::new).extend(extra);
                }
                let result = if desc.clear_environment {
                    create_pty_with_spawn_clear_env(
                        shell,
                        desc.args.clone(),
                        &desc.working_dir,
                        env.unwrap_or_default(),
                        desc.cols,
                        desc.rows,
                        desc.pixel_width,
                        desc.pixel_height,
                    )
                } else {
                    create_pty_with_spawn(
                        shell,
                        desc.args.clone(),
                        &desc.working_dir,
                        env,
                        desc.cols,
                        desc.rows,
                        desc.pixel_width,
                        desc.pixel_height,
                    )
                };
                result.map_err(|err| Box::new(err) as Box<dyn Error + Send + Sync>)?
            };

            #[cfg(target_os = "windows")]
            let pty = create_pty(
                shell,
                desc.args.clone(),
                &desc.working_dir,
                desc.environment.clone(),
                desc.cols,
                desc.rows,
            )
            .map_err(|err| Box::new(err) as Box<dyn Error + Send + Sync>)?;

            let input_budget = listener.input_budget.clone();
            let input_error = Arc::clone(&listener.input_error);
            #[cfg(not(target_os = "windows"))]
            let shell_pid = pty.child.pid as u32;
            #[cfg(target_os = "windows")]
            let shell_pid = pty.child_watcher().pid().map(|pid| pid.get()).unwrap_or(0);
            #[cfg(not(target_os = "windows"))]
            let main_fd = pty.child.id;

            let machine = Machine::new(
                Arc::clone(&terminal),
                pty,
                listener,
                WindowId::from(id as u64),
                id,
            )
            .map_err(|err| std::io::Error::other(err.to_string()))?;
            let channel = machine.channel();
            *pty_writer.lock().unwrap() = Some(channel.clone());
            let io_thread = machine.spawn();

            Ok(Surface {
                id,
                // Terminals default alt to meta; the host may override it.
                alt_is_meta: std::sync::atomic::AtomicBool::new(true),
                terminal,
                graphics_updates,
                url_regex: std::sync::Mutex::new(None),
                processor: std::sync::Mutex::new(None),
                channel,
                shell_pid,
                #[cfg(not(target_os = "windows"))]
                main_fd,
                reap_child_on_drop: desc.clear_environment,
                input_budget,
                input_error,
                _io_thread: Some(io_thread),
            })
        }
    }

    pub fn id(&self) -> SurfaceId {
        self.id
    }

    /// Reports and clears an input enqueue failure from a parser-generated
    /// terminal response. Worker hosts use this to surface a bounded-queue
    /// failure instead of silently dropping the response.
    #[cfg(feature = "pty")]
    pub fn take_input_error(&self) -> Option<InputError> {
        match self
            .input_error
            .swap(0, std::sync::atomic::Ordering::AcqRel)
        {
            0 => None,
            1 => Some(InputError::WouldBlock),
            2 => Some(InputError::Disconnected),
            code => panic!("invalid PTY input error code: {code}"),
        }
    }

    #[cfg(feature = "pty")]
    fn enqueue_input(&self, bytes: Cow<'static, [u8]>) -> Result<(), InputError> {
        let message = if let Some(budget) = &self.input_budget {
            let reservation = budget
                .try_reserve(bytes.len())
                .map_err(|InputBudgetError::WouldBlock| InputError::WouldBlock)?;
            Msg::InputBounded {
                input: bytes,
                reservation,
            }
        } else {
            Msg::Input(bytes)
        };
        self.channel
            .send(message)
            .map_err(|_| InputError::Disconnected)
    }

    /// Fallible PTY input enqueue. A worker configures an input queue budget;
    /// default embedders keep the historical unbounded behavior through
    /// [`Surface::write`].
    pub fn try_write<B: Into<Cow<'static, [u8]>>>(
        &self,
        bytes: B,
    ) -> Result<(), InputError> {
        self.try_write_response(bytes)?;

        // Input snaps the view back to the live screen and drops any
        // selection, the scroll-to-bottom / clear-on-typing convention
        // (only reached when a key actually produced PTY bytes).
        {
            use rio_vt::crosswords::grid::Scroll;
            let mut term = self.terminal.lock();
            if term.display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
            }
            if term.selection.is_some() {
                term.selection = None;
            }
        }
        Ok(())
    }

    /// Enqueue a terminal-generated reply through the input budget without
    /// changing the user's viewport or selection.
    pub fn try_write_response<B: Into<Cow<'static, [u8]>>>(
        &self,
        bytes: B,
    ) -> Result<(), InputError> {
        let bytes = bytes.into();
        #[cfg(feature = "pty")]
        self.enqueue_input(bytes)?;
        #[cfg(not(feature = "pty"))]
        self.delegate.output(self.id, &bytes);

        Ok(())
    }

    pub fn write<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) {
        let _ = self.try_write(bytes);
    }

    pub fn text(&self, text: &str) {
        self.write(text.as_bytes().to_vec());
    }

    /// Paste text the way terminals do: when the program asked for
    /// bracketed paste (mode 2004) the text is sent verbatim inside
    /// ESC[200~/ESC[201~ markers, minus ESC, ETX and the 8-bit CSI so
    /// the payload can never close the bracket early and inject
    /// keystrokes; otherwise newlines are normalized to CR, what the
    /// Enter key produces.
    pub fn try_paste(&self, text: &str) -> Result<(), InputError> {
        if text.is_empty() {
            return Ok(());
        }
        let bracketed = self.terminal.lock().mode().contains(Mode::BRACKETED_PASTE);
        self.try_write(encode_paste(text, bracketed))
    }

    pub fn paste(&self, text: &str) {
        let _ = self.try_paste(text);
    }

    /// A stable, C-friendly view of the terminal modes an embedder needs
    /// for input decisions it makes on its own (touch scrolling, key bars).
    /// Bit 0 mouse reporting, bit 1 application cursor keys, bit 2
    /// alternate screen, bit 3 bracketed paste.
    pub fn mode_bits(&self) -> u32 {
        let mode = self.terminal.lock().mode();
        let mut bits = 0;
        if mode.intersects(Mode::MOUSE_MODE) {
            bits |= 1;
        }
        if mode.contains(Mode::APP_CURSOR) {
            bits |= 1 << 1;
        }
        if mode.contains(Mode::ALT_SCREEN) {
            bits |= 1 << 2;
        }
        if mode.contains(Mode::BRACKETED_PASTE) {
            bits |= 1 << 3;
        }
        bits
    }

    /// Whether alt acts as meta, prefixing with ESC, instead of letting the
    /// platform's text through. See [`key::EncodeContext::alt_is_meta`].
    pub fn set_alt_is_meta(&self, enabled: bool) {
        self.alt_is_meta.store(enabled, Ordering::Relaxed);
    }

    pub fn alt_is_meta(&self) -> bool {
        self.alt_is_meta.load(Ordering::Relaxed)
    }

    /// Apply host cursor defaults without exposing the authoritative terminal
    /// to the embedder. Parser-issued cursor modes can still override these
    /// values afterward.
    pub fn set_cursor_style(&self, shape: CursorShape, blinking: bool) {
        let mut terminal = self.terminal.lock();
        terminal.cursor_shape = shape;
        terminal.default_cursor_shape = shape;
        terminal.blinking_cursor = blinking;
        terminal.mark_fully_damaged();
    }

    /// Configure the default for grapheme cluster processing (DEC
    /// private mode 2027). On by default; embedders whose renderers
    /// assume legacy wcwidth cell layout can turn it off. Applied
    /// immediately and restored by RIS; a program's DECSET/DECRST
    /// still wins at runtime.
    pub fn set_grapheme_clustering(&self, enabled: bool) {
        self.terminal.lock().set_grapheme_clustering(enabled);
    }

    pub fn try_key(&self, event: &KeyEvent) -> Result<bool, InputError> {
        // The encoding depends on terminal state the embedder does not track,
        // which is the reason this lives here and not in the host.
        let ctx = {
            let terminal = self.terminal.lock();
            key::EncodeContext {
                app_cursor: terminal.mode().contains(Mode::APP_CURSOR),
                kitty: kitty_flags(terminal.keyboard_mode()),
                modify_other_keys: terminal.modify_other_keys(),
                alt_is_meta: self.alt_is_meta.load(Ordering::Relaxed),
            }
        };
        match key::encode(event, &ctx) {
            Some(bytes) => {
                self.try_write(bytes)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn key(&self, event: &KeyEvent) -> bool {
        self.try_key(event).unwrap_or(false)
    }

    /// Report a host focus transition only when the application enabled DEC
    /// mode 1004. The bytes use the same bounded input path as key reports.
    pub fn try_focus(&self, focused: bool) -> Result<bool, InputError> {
        if !self.terminal.lock().mode().contains(Mode::FOCUS_IN_OUT) {
            return Ok(false);
        }
        self.try_write_response(if focused { b"\x1b[I" } else { b"\x1b[O" }.to_vec())?;
        Ok(true)
    }

    pub fn vi_mode(&self) -> bool {
        self.terminal.lock().mode().contains(Mode::VI)
    }

    pub fn set_vi_mode(&self, enabled: bool) -> bool {
        let mut terminal = self.terminal.lock();
        let current = terminal.mode().contains(Mode::VI);
        if current != enabled {
            terminal.toggle_vi_mode();
            true
        } else {
            false
        }
    }

    pub fn toggle_vi_mode(&self) -> bool {
        self.terminal.lock().toggle_vi_mode();
        true
    }

    pub fn vi_motion(&self, motion: rio_vt::crosswords::vi_mode::ViMotion) -> bool {
        let mut terminal = self.terminal.lock();
        if !terminal.mode().contains(Mode::VI) {
            return false;
        }
        terminal.vi_motion(motion);
        true
    }

    pub fn vi_scroll(&self, delta_lines: i32) -> bool {
        let mut terminal = self.terminal.lock();
        if !terminal.mode().contains(Mode::VI) {
            return false;
        }
        terminal.vi_scroll(delta_lines);
        true
    }

    pub fn vi_goto(&self, line: i32, column: usize) -> bool {
        let mut terminal = self.terminal.lock();
        if !terminal.mode().contains(Mode::VI) {
            return false;
        }
        terminal.vi_goto_pos(Pos::new(Line(line), PosColumn(column)));
        true
    }

    pub fn scroll_to_prompt(&self, forward: bool) {
        self.terminal.lock().scroll_to_prompt(forward);
    }

    pub fn scroll_to_top(&self) {
        use rio_vt::crosswords::grid::Scroll;
        self.terminal.lock().scroll_display(Scroll::Top);
    }

    pub fn scroll_to_bottom(&self) {
        use rio_vt::crosswords::grid::Scroll;
        self.terminal.lock().scroll_display(Scroll::Bottom);
    }

    pub fn clear_saved_history(&self) {
        self.terminal.lock().clear_saved_history();
    }

    /// Select the entire retained grid, including scrollback. This is an
    /// authoritative terminal operation so selection text and rendering see
    /// the same range.
    pub fn select_all(&self) {
        use rio_vt::crosswords::grid::Dimensions;
        let mut terminal = self.terminal.lock();
        let start = Pos::new(terminal.grid.topmost_line(), PosColumn(0));
        let end = Pos::new(terminal.grid.bottommost_line(), terminal.grid.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        selection.include_all();
        terminal.selection = Some(selection);
        terminal.mark_fully_damaged();
    }

    /// Move the viewport and update a dragging selection endpoint while the
    /// terminal lock is held. The pointer coordinates are relative to the
    /// resulting viewport, matching the GUI selection-scroll tick.
    pub fn selection_autoscroll(
        &self,
        delta_lines: i32,
        viewport_line: i32,
        col: usize,
        side: Side,
    ) -> bool {
        use rio_vt::crosswords::grid::{Dimensions, Scroll};
        let mut terminal = self.terminal.lock();
        if terminal.selection.is_none() {
            return false;
        }
        terminal.scroll_display(Scroll::Delta(delta_lines));
        let history = terminal.history_size() as i32;
        let bottom = terminal.grid.bottommost_line().0;
        let line =
            (viewport_line - terminal.display_offset() as i32).clamp(-history, bottom);
        let column = col.min(terminal.grid.last_column().0);
        if let Some(selection) = &mut terminal.selection {
            selection.update(Pos::new(Line(line), PosColumn(column)), side);
            terminal.mark_fully_damaged();
            true
        } else {
            false
        }
    }

    pub fn display_offset(&self) -> usize {
        self.terminal.lock().display_offset()
    }

    pub fn columns(&self) -> usize {
        self.terminal.lock().columns()
    }

    pub fn screen_lines(&self) -> usize {
        self.terminal.lock().screen_lines()
    }

    pub fn restore_display_offset(&self, target: usize) {
        use rio_vt::crosswords::grid::Scroll;
        let mut terminal = self.terminal.lock();
        let current = terminal.display_offset();
        let delta = target as i64 - current as i64;
        if delta != 0 {
            terminal.scroll_display(Scroll::Delta(delta as i32));
        }
    }

    pub fn scroll_to_pos(&self, line: i32, column: usize) -> bool {
        let mut terminal = self.terminal.lock();
        let before = terminal.display_offset();
        terminal.scroll_to_pos(Pos::new(Line(line), PosColumn(column)));
        before != terminal.display_offset()
    }

    pub fn search_next(
        &self,
        regex: &mut rio_vt::crosswords::search::RegexSearch,
        origin: Pos,
        direction: rio_vt::crosswords::pos::Direction,
        side: Side,
        max_lines: Option<usize>,
    ) -> Option<(Pos, Pos)> {
        let terminal = self.terminal.lock();
        terminal
            .search_next(regex, origin, direction, side, max_lines)
            .map(|matched| (*matched.start(), *matched.end()))
    }

    pub fn cursor_position(&self) -> (i32, u16) {
        let cursor = self.terminal.lock().cursor();
        (cursor.pos.row.0, cursor.pos.col.0 as u16)
    }

    /// Return the authoritative vi cursor position without applying the
    /// viewport offset used by the rendered cursor.
    pub fn vi_cursor_position(&self) -> (i32, u16) {
        let terminal = self.terminal.lock();
        let cursor = terminal.vi_cursor_pos();
        (cursor.row.0, cursor.col.0 as u16)
    }

    pub fn color(&self, index: usize) -> Option<ColorRgb> {
        if index >= rio_vt::config::colors::term::COUNT {
            return None;
        }
        self.terminal.lock().colors()[index].map(ColorRgb::from_color_arr)
    }

    pub fn resize(&self, cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) {
        self.terminal.lock().resize(GridSize::new(
            cols as usize,
            rows as usize,
            pixel_width,
            pixel_height,
        ));
        #[cfg(feature = "pty")]
        let _ = self.channel.send(Msg::Resize(WindowSize {
            rows,
            cols,
            width: pixel_width,
            height: pixel_height,
        }));
    }

    /// A wheel scroll, dispatched the way terminals do it: the program
    /// running in the terminal gets first claim.
    ///
    /// Three cases, in order:
    /// mouse reporting on, so the wheel is a mouse event; the alternate
    /// screen with alternate-scroll on, where there is no scrollback to
    /// move so the wheel becomes cursor keys and pagers scroll; and
    /// otherwise the host's scrollback view. Holding shift always means
    /// "give me the scrollback", overriding the first two.
    ///
    /// `lines` is positive for scrolling up (towards history). `col` and
    /// `row` are the cell under the pointer, needed by mouse reports.
    /// Returns true when the program consumed it, false when the
    /// scrollback moved instead.
    pub fn try_scroll_wheel(
        &self,
        lines: i32,
        col: u16,
        row: u16,
        mods: Modifiers,
    ) -> Result<bool, InputError> {
        if lines == 0 {
            return Ok(false);
        }
        let (mouse_mode, alt_screen, alt_scroll, app_cursor, sgr, utf8) = {
            let terminal = self.terminal.lock();
            let mode = terminal.mode();
            (
                mode.intersects(Mode::MOUSE_MODE),
                mode.contains(Mode::ALT_SCREEN),
                mode.contains(Mode::ALTERNATE_SCROLL),
                mode.contains(Mode::APP_CURSOR),
                mode.contains(Mode::SGR_MOUSE),
                mode.contains(Mode::UTF8_MOUSE),
            )
        };
        let shift = mods.contains(Modifiers::SHIFT);

        if mouse_mode && !shift {
            // Wheel buttons are 64 (up) and 65 (down), with the modifier
            // bits every mouse report carries.
            let mut button = if lines > 0 { 64 } else { 65 };
            if mods.contains(Modifiers::SHIFT) {
                button += 4;
            }
            if mods.contains(Modifiers::ALT) {
                button += 8;
            }
            if mods.contains(Modifiers::CTRL) {
                button += 16;
            }
            let mut out = Vec::new();
            for _ in 0..lines.abs() {
                out.extend_from_slice(&mouse_report(button, col, row, true, sgr, utf8));
            }
            self.try_write(out)?;
            return Ok(true);
        }

        if alt_screen && alt_scroll && !shift {
            let up = lines > 0;
            let seq: &[u8] = match (app_cursor, up) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1bOB",
                (false, true) => b"\x1b[A",
                (false, false) => b"\x1b[B",
            };
            let mut out = Vec::with_capacity(seq.len() * lines.unsigned_abs() as usize);
            for _ in 0..lines.abs() {
                out.extend_from_slice(seq);
            }
            self.try_write(out)?;
            return Ok(true);
        }

        self.scroll(lines);
        Ok(false)
    }

    pub fn scroll_wheel(&self, lines: i32, col: u16, row: u16, mods: Modifiers) -> bool {
        self.try_scroll_wheel(lines, col, row, mods)
            .unwrap_or(false)
    }

    /// Report a mouse button press/release to the program when it asked for
    /// mouse events (DEC 1000/1002/1003, or X10/9). `button` is 0=left,
    /// 1=middle, 2=right. Returns true when a report was written, so the host
    /// should not start a local selection. Returns false when no program
    /// is grabbing the mouse, or shift is held to force a local selection
    /// (shift-to-bypass).
    pub fn try_mouse_button(
        &self,
        col: u16,
        row: u16,
        button: u8,
        pressed: bool,
        mods: Modifiers,
    ) -> Result<bool, InputError> {
        let (mouse_mode, x10, sgr, utf8) = {
            let mode = self.terminal.lock().mode();
            (
                mode.intersects(Mode::MOUSE_MODE),
                mode.contains(Mode::MOUSE_REPORT_X10),
                mode.contains(Mode::SGR_MOUSE),
                mode.contains(Mode::UTF8_MOUSE),
            )
        };
        if !mouse_mode || mods.contains(Modifiers::SHIFT) {
            return Ok(false);
        }
        // X10 (mode 9) reports only presses of the three main buttons, with
        // no modifiers and no release.
        if x10 && (!pressed || button > 2) {
            return Ok(false);
        }
        let mut encoded = button;
        if !x10 {
            if mods.contains(Modifiers::ALT) {
                encoded += 8;
            }
            if mods.contains(Modifiers::CTRL) {
                encoded += 16;
            }
        }
        self.try_write(mouse_report(encoded, col, row, pressed, sgr, utf8))?;
        Ok(true)
    }

    pub fn mouse_button(
        &self,
        col: u16,
        row: u16,
        button: u8,
        pressed: bool,
        mods: Modifiers,
    ) -> bool {
        self.try_mouse_button(col, row, button, pressed, mods)
            .unwrap_or(false)
    }

    /// Report pointer motion for button-event (1002, a button held) and
    /// any-event (1003, bare motion) modes. `button` is 0/1/2 for the button
    /// held during a drag, or 3 when none is held. Returns true when a report
    /// was written.
    pub fn try_mouse_motion(
        &self,
        col: u16,
        row: u16,
        button: u8,
        mods: Modifiers,
    ) -> Result<bool, InputError> {
        let (drag, motion, sgr, utf8) = {
            let mode = self.terminal.lock().mode();
            (
                mode.contains(Mode::MOUSE_DRAG),
                mode.contains(Mode::MOUSE_MOTION),
                mode.contains(Mode::SGR_MOUSE),
                mode.contains(Mode::UTF8_MOUSE),
            )
        };
        if mods.contains(Modifiers::SHIFT) {
            return Ok(false);
        }
        // 1002 reports motion only while a button is down; 1003 reports all.
        let wanted = if button >= 3 { motion } else { drag || motion };
        if !wanted {
            return Ok(false);
        }
        // The motion bit (32) rides on top of the button.
        let mut encoded = button.saturating_add(32);
        if mods.contains(Modifiers::ALT) {
            encoded += 8;
        }
        if mods.contains(Modifiers::CTRL) {
            encoded += 16;
        }
        self.try_write(mouse_report(encoded, col, row, true, sgr, utf8))?;
        Ok(true)
    }

    pub fn mouse_motion(&self, col: u16, row: u16, button: u8, mods: Modifiers) -> bool {
        self.try_mouse_motion(col, row, button, mods)
            .unwrap_or(false)
    }

    pub fn scroll(&self, delta_lines: i32) {
        use rio_vt::crosswords::grid::Scroll;
        self.terminal
            .lock()
            .scroll_display(Scroll::Delta(delta_lines));
    }

    /// Begin a selection. `side` says which half of the cell the pointer
    /// is on, which is what decides whether that cell is inside the
    /// selection; assuming a side makes cells at the ends of a drag
    /// unreachable in one direction.
    pub fn selection_begin(
        &self,
        viewport_line: i32,
        col: usize,
        kind: SelectionKind,
        side: Side,
    ) {
        let mut term = self.terminal.lock();
        let offset = term.display_offset() as i32;
        let pos = Pos::new(Line(viewport_line - offset), PosColumn(col));
        term.selection = Some(Selection::new(kind.to_type(), pos, side));
        term.mark_fully_damaged();
    }

    pub fn selection_update(&self, viewport_line: i32, col: usize, side: Side) {
        let mut term = self.terminal.lock();
        let offset = term.display_offset() as i32;
        let pos = Pos::new(Line(viewport_line - offset), PosColumn(col));
        if let Some(selection) = &mut term.selection {
            selection.update(pos, side);
            term.mark_fully_damaged();
        }
    }

    pub fn selection_clear(&self) {
        let mut term = self.terminal.lock();
        if term.selection.take().is_some() {
            term.mark_fully_damaged();
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.terminal.lock().selection_to_string()
    }

    pub fn selection_text_bounded(
        &self,
        max_bytes: usize,
    ) -> Result<Option<String>, rio_vt::crosswords::SelectionTextError> {
        self.terminal.lock().selection_to_string_bounded(max_bytes)
    }

    /// The shell's current working directory: OSC 7 when the shell reports
    /// it, otherwise the OS's view of the foreground process's cwd (so it
    /// works without any shell integration). Used by session persistence to
    /// restore each surface in the directory it was left in.
    pub fn working_dir(&self) -> Option<String> {
        let reported = self
            .terminal
            .lock()
            .current_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        if reported.is_some() {
            return reported;
        }
        #[cfg(all(feature = "pty", not(target_os = "windows")))]
        {
            teletypewriter::foreground_process_path(self.main_fd, self.shell_pid)
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        }
        #[cfg(any(not(feature = "pty"), target_os = "windows"))]
        None
    }

    /// Lines currently held in scrollback (the ring, not counting
    /// eviction). Search coordinates are relative to the top of this ring.
    pub fn history_size(&self) -> usize {
        self.terminal.lock().history_size()
    }

    /// Serialize scrollback + screen to a byte stream that reconstructs
    /// content, SGR styling, and OSC 8 hyperlinks when replayed into a
    /// same-width terminal (`inject_output`). The style state machine is
    /// reset-then-set per change: verbose but unambiguous.
    pub fn serialize(&self) -> String {
        use rio_vt::crosswords::grid::{Dimensions, GridSquare};
        use rio_vt::crosswords::square::{CellFlags, Wide};

        let term = self.terminal.lock();
        let grid = &term.grid;
        let cols = grid.columns();
        let history = term.history_size() as i32;
        let rows = term.screen_lines() as i32;

        // Trim trailing all-empty screen rows, like dump().
        let mut last = rows - 1;
        'trim: while last > -history {
            let row = &grid[Line(last)];
            for col in 0..cols {
                let square = row[PosColumn(col)];
                if !square.is_empty() {
                    break 'trim;
                }
            }
            last -= 1;
        }

        let mut out = String::new();
        let mut style = Style::default();
        let mut link: Option<String> = None;
        out.push_str("\x1b[0m");

        for line in -history..=last {
            let row = &grid[Line(line)];
            let wrapped = row[PosColumn(cols - 1)]
                .cell_flags()
                .contains(CellFlags::WRAPLINE);

            // Within a row, drop the trailing run of default empty cells
            // (unless the row wraps, where every cell is content).
            let mut end = cols;
            if !wrapped {
                while end > 0 {
                    let square = row[PosColumn(end - 1)];
                    if !square.is_empty() {
                        break;
                    }
                    end -= 1;
                }
            }

            for col in 0..end {
                let square = row[PosColumn(col)];
                match square.wide() {
                    Wide::Spacer | Wide::LeadingSpacer => continue,
                    _ => {}
                }

                let cell_style = grid.style_of(&square);
                if cell_style != style {
                    rio_vt::crosswords::formatter::write_sgr(&mut out, &cell_style);
                    style = cell_style;
                }

                let cell_link = square
                    .extras_id_checked()
                    .and_then(|id| grid.extras_table.get(id))
                    .and_then(|extras| extras.hyperlink.as_ref())
                    .map(|h| h.uri().to_string());
                if cell_link != link {
                    match &cell_link {
                        Some(uri) => {
                            out.push_str("\x1b]8;;");
                            out.push_str(uri);
                            out.push_str("\x1b\\");
                        }
                        None => out.push_str("\x1b]8;;\x1b\\"),
                    }
                    link = cell_link;
                }

                let c = square.c();
                out.push(if c == '\0' { ' ' } else { c });
                if let Some(extras) = square
                    .extras_id_checked()
                    .and_then(|id| grid.extras_table.get(id))
                {
                    for z in &extras.zerowidth {
                        out.push(*z);
                    }
                }
            }

            if !wrapped && line < last {
                out.push_str("\r\n");
            }
        }

        if link.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
        out.push_str("\x1b[0m");
        out
    }

    /// All regex matches across scrollback + screen, top to bottom, as
    /// `(start_line, start_col, end_line, end_col)` with lines relative to
    /// the top of the scrollback ring. `None` when the pattern is invalid.
    pub fn search(&self, pattern: &str, max: usize) -> Option<Vec<(u32, u16, u32, u16)>> {
        use rio_vt::crosswords::pos::Direction;
        use rio_vt::crosswords::search::{RegexIter, RegexSearch};

        let mut regex = RegexSearch::new(pattern).ok()?;
        let term = self.terminal.lock();
        let history = term.history_size() as i32;
        let rows = term.screen_lines() as i32;
        let cols = term.columns();
        let start = Pos::new(Line(-history), PosColumn(0));
        let end = Pos::new(Line(rows - 1), PosColumn(cols - 1));

        let mut matches = Vec::new();
        for m in RegexIter::new(start, end, Direction::Right, &term, &mut regex) {
            let (s, e) = (m.start(), m.end());
            matches.push((
                (s.row.0 + history) as u32,
                s.col.0 as u16,
                (e.row.0 + history) as u32,
                e.col.0 as u16,
            ));
            if matches.len() >= max {
                break;
            }
        }
        Some(matches)
    }

    /// The pid of the program this surface spawned (the shell, or the
    /// configured `shell` program), for identity and diagnostics. Do not
    /// signal it on teardown: dropping the surface already hangs up the
    /// process group, and the reader thread escalates to SIGKILL and
    /// reaps, so a host-side killpg would race that escalation and can
    /// hit a recycled pid; a host that wants an extra signal must use
    /// [`Surface::hangup_child`], which goes through the lifecycle
    /// guard. On Windows it is the conpty child's process id; 0 if the
    /// pid was unavailable.
    #[cfg(feature = "pty")]
    pub fn child_pid(&self) -> u32 {
        self.shell_pid
    }

    /// Hang up the child's process group through the lifecycle guard:
    /// a no-op once the child was reaped, so a host can signal on its
    /// own teardown without racing the reader thread's escalation or
    /// hitting a recycled pid. Returns false when delivery failed.
    #[cfg(all(feature = "pty", not(target_os = "windows")))]
    pub fn hangup_child(&self) -> bool {
        self.child_terminator.hangup().is_ok()
    }

    /// The foreground process's name (the program the user is running
    /// right now: `claude`, `vim`, or the shell itself), from the kernel.
    /// Hosts use it to tell what a pane is running without any shell
    /// integration.
    #[cfg(feature = "pty")]
    pub fn foreground_process_name(&self) -> String {
        #[cfg(not(target_os = "windows"))]
        {
            teletypewriter::foreground_process_name(self.main_fd, self.shell_pid)
        }
        #[cfg(target_os = "windows")]
        String::new()
    }

    /// Inject bytes into the terminal's DISPLAY (the VT parser), as if they
    /// came from the child process — NOT into the PTY input. Used to replay
    /// saved scrollback on restore; the shell never sees these bytes, so it
    /// can't execute them. Bytes are plain output (convert `\n` to `\r\n`
    /// upstream if you want proper line starts).
    ///
    /// The parser is persistent: an escape sequence split across two
    /// calls resumes where it left off, exactly like PTY chunking.
    pub fn inject_output(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut guard = self.processor.lock().unwrap();
        let processor = guard.get_or_insert_with(Default::default);
        let mut term = self.terminal.lock();
        processor.advance(&mut *term, bytes);
    }

    /// Plain-text URL under a viewport cell, with the run to underline on
    /// the hovered row: `(uri, start_col, end_col)`. Detection is regex
    /// over the logical (unwrapped) line containing the cell, so wrapped
    /// URLs resolve whole. Hit-test shaped: call it from pointer events;
    /// OSC 8 links should be checked first, they are explicit.
    pub fn url_at(&self, viewport_line: u16, col: u16) -> Option<(String, u16, u16)> {
        use rio_vt::crosswords::pos::Direction;
        use rio_vt::crosswords::search::{RegexIter, RegexSearch};
        use rio_vt::crosswords::square::CellFlags;

        // A fully wrapped scrollback (one endless logical line) must not
        // turn a hover into a whole-buffer regex scan.
        const MAX_WRAP_SCAN: i32 = 100;

        let mut guard = self.url_regex.lock().unwrap();
        let regex = match guard.as_mut() {
            Some(regex) => regex,
            None => guard.insert(
                RegexSearch::new(URL_REGEX).expect("static URL pattern compiles"),
            ),
        };

        let term = self.terminal.lock();
        let cols = term.columns();
        let rows = term.screen_lines() as i32;
        let history = term.history_size() as i32;
        let display_offset = term.display_offset() as i32;
        let grid_line = viewport_line as i32 - display_offset;
        if grid_line >= rows || col as usize >= cols {
            return None;
        }
        let point = Pos::new(Line(grid_line), PosColumn(col as usize));

        let wraps = |line: i32| {
            term.grid[Line(line)][PosColumn(cols - 1)]
                .cell_flags()
                .contains(CellFlags::WRAPLINE)
        };
        let mut start_line = grid_line;
        while start_line > -history
            && grid_line - start_line < MAX_WRAP_SCAN
            && wraps(start_line - 1)
        {
            start_line -= 1;
        }
        let mut end_line = grid_line;
        while end_line < rows - 1
            && end_line - grid_line < MAX_WRAP_SCAN
            && wraps(end_line)
        {
            end_line += 1;
        }

        let from = Pos::new(Line(start_line), PosColumn(0));
        let to = Pos::new(Line(end_line), PosColumn(cols - 1));
        for m in RegexIter::new(from, to, Direction::Right, &term, regex) {
            let (s, mut e) = (*m.start(), *m.end());
            if point < s {
                break;
            }
            let text = term.bounds_to_string(s, e);
            // The pattern is greedy about trailing punctuation; prose is
            // not: `https://rio.dev,` links without the comma, and a paren
            // only belongs to the URL when it was opened inside it.
            let trim = trailing_url_punctuation(&text);
            for _ in 0..trim {
                if e.col.0 == 0 {
                    e.row.0 -= 1;
                    e.col = PosColumn(cols - 1);
                } else {
                    e.col.0 -= 1;
                }
            }
            if point > e {
                continue;
            }
            let uri: String = {
                let chars = text.chars().count() - trim;
                text.chars().take(chars).collect()
            };
            let start_col = if s.row.0 < grid_line {
                0
            } else {
                s.col.0 as u16
            };
            let end_col = if e.row.0 > grid_line {
                (cols - 1) as u16
            } else {
                e.col.0 as u16
            };
            return Some((uri, start_col, end_col));
        }
        None
    }

    /// Dump the whole buffer (scrollback + screen) to plain text, so a
    /// frontend can persist it and replay it as inert scrollback on
    /// restore. Trailing blank rows are trimmed by `bounds_to_string`.
    pub fn dump(&self) -> String {
        let term = self.terminal.lock();
        let rows = term.screen_lines() as i32;
        let cols = term.columns();
        if rows == 0 || cols == 0 {
            return String::new();
        }
        let history = term.history_size() as i32;
        // Scrollback lives in negative line coordinates above the screen.
        let start = Pos::new(Line(-history), PosColumn(0));
        let end = Pos::new(Line(rows - 1), PosColumn(cols - 1));
        term.bounds_to_string(start, end)
    }

    pub(crate) fn terminal(&self) -> Arc<FairMutex<Crosswords<Listener>>> {
        self.terminal.clone()
    }
}

#[cfg(feature = "pty")]
impl Drop for Surface {
    fn drop(&mut self) {
        let _ = self.channel.send(Msg::Shutdown);
        if self.reap_child_on_drop {
            // Session workers opt into deterministic PTY ownership teardown;
            // the legacy/default embedder path leaves its JoinHandle detached
            // and remains non-blocking.
            if let Some(io_thread) = self._io_thread.take() {
                let _ = io_thread.join();
            }
            #[cfg(not(target_os = "windows"))]
            let _ = teletypewriter::reap_child(self.shell_pid as _);
        }
    }
}

#[cfg(all(test, feature = "pty", not(target_os = "windows")))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    struct CountingDelegate {
        wakeups: AtomicUsize,
    }

    impl SurfaceDelegate for CountingDelegate {
        fn wakeup(&self, _surface: SurfaceId) {
            self.wakeups.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ActionRecorder {
        actions: Mutex<Vec<Action>>,
    }

    impl SurfaceDelegate for ActionRecorder {
        fn wakeup(&self, _surface: SurfaceId) {}
        fn action(&self, _surface: SurfaceId, action: Action) {
            self.actions.lock().unwrap().push(action);
        }
    }

    // The wheel means different things to different programs. A pager on
    // the alternate screen wants cursor keys (there is no scrollback to
    // move), a mouse-aware program wants a mouse report, and a plain
    // shell wants the scrollback view. Shift always means the last one.
    #[test]
    fn wheel_becomes_cursor_keys_on_the_alternate_screen() {
        let engine = Engine::new(Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        }));
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");

        // Alternate screen + alternate scroll, as pagers and TUIs set it.
        surface.inject_output(b"\x1b[?1049h\x1b[?1007h");
        assert!(surface.scroll_wheel(3, 0, 0, Modifiers::empty()));

        // Application cursor mode swaps CSI for SS3.
        surface.inject_output(b"\x1b[?1h");
        assert!(surface.scroll_wheel(-1, 0, 0, Modifiers::empty()));

        // Shift is the user asking for the scrollback regardless.
        assert!(!surface.scroll_wheel(3, 0, 0, Modifiers::SHIFT));
    }

    /// The measurement entry through the C ABI: count plus width via
    /// out-param, and null tolerance on both pointers.
    #[test]
    fn cluster_width_through_c_abi() {
        let cps: [u32; 3] = [0x1F468, 0x200D, 0x1F33E];
        let mut width: u8 = 0xFF;
        let len = unsafe { capi::rio_cluster_width(cps.as_ptr(), cps.len(), &mut width) };
        assert_eq!((len, width), (3, 2));

        // The width out-param is optional.
        let len = unsafe {
            capi::rio_cluster_width(cps.as_ptr(), cps.len(), core::ptr::null_mut())
        };
        assert_eq!(len, 3);

        // A null or empty buffer measures as nothing.
        let mut width: u8 = 0xFF;
        let len = unsafe { capi::rio_cluster_width(core::ptr::null(), 5, &mut width) };
        assert_eq!((len, width), (0, 0));
    }

    #[test]
    fn wheel_becomes_a_mouse_report_when_the_program_asks() {
        let engine = Engine::new(Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        }));
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");

        surface.inject_output(b"\x1b[?1000h\x1b[?1006h");
        assert!(surface.scroll_wheel(1, 4, 2, Modifiers::empty()));
        assert!(!surface.scroll_wheel(1, 4, 2, Modifiers::SHIFT));
    }

    #[test]
    fn wheel_scrolls_the_view_in_a_plain_shell() {
        let engine = Engine::new(Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        }));
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");
        let mut state = RenderState::new(&surface);

        let mut text = String::new();
        for i in 0..80 {
            text.push_str(&format!("line {i}\r\n"));
        }
        surface.inject_output(text.as_bytes());

        assert!(!surface.scroll_wheel(5, 0, 0, Modifiers::empty()));
        state.update();
        assert!(state.display_offset() > 0, "the view should have moved");
    }

    // The wasm renderer draws a cell's full cluster text (base +
    // attached codepoints) when the wire flag says one exists; the
    // accessor is the source of that text.
    #[test]
    fn render_state_exposes_cluster_text() {
        let engine = Engine::new(Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        }));
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");
        let mut state = RenderState::new(&surface);

        // Mode 2027 on, then a ZWJ emoji and a decomposed accent.
        surface
            .inject_output("\x1b[?2027h\u{1F9D1}\u{200D}\u{1F33E}e\u{301}x".as_bytes());
        state.update();

        assert_eq!(
            state.cell_cluster_text(0, 0).as_deref(),
            Some("\u{1F9D1}\u{200D}\u{1F33E}")
        );
        assert_eq!(state.cell_cluster_text(0, 2).as_deref(), Some("e\u{301}"));
        // Plain cells report nothing.
        assert_eq!(state.cell_cluster_text(0, 3), None);
    }

    // SGR is the modern form; the X10 fallback offsets by 32.
    #[test]
    fn mouse_reports_encode_both_forms() {
        // SGR press keeps the button and ends in `M`; release ends in `m`.
        assert_eq!(
            mouse_report(64, 4, 2, true, true, false),
            b"\x1b[<64;5;3M".to_vec()
        );
        assert_eq!(
            mouse_report(0, 4, 2, false, true, false),
            b"\x1b[<0;5;3m".to_vec()
        );
        // X10 press offsets by 32; release collapses to button 3.
        assert_eq!(
            mouse_report(65, 0, 0, true, false, false),
            vec![0x1b, b'[', b'M', 32 + 65, 33, 33]
        );
        assert_eq!(
            mouse_report(0, 0, 0, false, false, false),
            vec![0x1b, b'[', b'M', 32 + 3, 33, 33]
        );
        // A legacy release keeps the modifier bits: ctrl+left release is
        // 16 | 3 = 19, not a bare 3.
        assert_eq!(
            mouse_report(16, 0, 0, false, false, false),
            vec![0x1b, b'[', b'M', 32 + 19, 33, 33]
        );
    }

    // Clicks and drags report only when the program asked; shift bypasses to
    // a local selection, and motion waits for 1002/1003.
    #[test]
    fn clicks_report_when_the_program_asks() {
        let engine = Engine::new(Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        }));
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");

        // Nothing grabs the mouse yet.
        assert!(!surface.mouse_button(4, 2, 0, true, Modifiers::empty()));

        surface.inject_output(b"\x1b[?1000h\x1b[?1006h");
        assert!(surface.mouse_button(4, 2, 0, true, Modifiers::empty()));
        assert!(surface.mouse_button(4, 2, 0, false, Modifiers::empty()));
        // Shift forces a local selection instead of a report.
        assert!(!surface.mouse_button(4, 2, 0, true, Modifiers::SHIFT));
        // 1000 is click-only: motion isn't wanted until 1002/1003.
        assert!(!surface.mouse_motion(5, 2, 0, Modifiers::empty()));

        surface.inject_output(b"\x1b[?1002h");
        assert!(surface.mouse_motion(5, 2, 0, Modifiers::empty()));
    }

    // Dragging right to left has to be able to reach the first column.
    // The side says which half of the cell the pointer is on; with it
    // hardcoded to the right, column 0 could never be included because
    // the drag would have to pass a point left of the screen.
    #[test]
    fn a_backwards_drag_reaches_the_first_column() {
        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate);
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");

        surface.inject_output(b"\x1b[H\x1b[2JABCDEF");

        // Press inside the right half of column 5, drag left to column 0.
        surface.selection_begin(0, 5, SelectionKind::Simple, Side::Right);
        surface.selection_update(0, 0, Side::Left);
        let text = surface.selection_text().unwrap_or_default();
        assert!(
            text.starts_with('A'),
            "backwards drag should include column 0, got {text:?}"
        );
    }

    // OSC 8 links must resolve through the pulled render state: URI under
    // a cell, and the row-run a renderer underlines on hover.
    #[test]
    fn osc8_links_resolve_from_render_state() {
        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate);
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");
        let mut state = RenderState::new(&surface);

        surface.inject_output(
            b"\x1b[2J\x1b[Hpre \x1b]8;;https://rioterm.com\x1b\\rio link\x1b]8;;\x1b\\ post",
        );
        state.update();

        assert_eq!(state.link_at(0, 0), None);
        assert_eq!(state.link_at(0, 4), Some("https://rioterm.com"));
        assert_eq!(state.link_at(0, 11), Some("https://rioterm.com"));
        assert_eq!(state.link_at(0, 13), None);
        assert_eq!(state.link_run(0, 6), Some((4, 11)));
        assert_eq!(state.link_run(0, 0), None);
    }

    // Bracketed paste sends the text verbatim minus ESC/ETX, so a
    // malicious payload cannot close the bracket and inject keystrokes;
    // unbracketed paste normalizes newlines to CR.
    #[test]
    fn paste_encoding_is_injection_safe() {
        assert_eq!(
            encode_paste("one\ntwo", true),
            b"\x1b[200~one\ntwo\x1b[201~".to_vec()
        );
        assert_eq!(
            encode_paste("a\x1b[201~rm -rf /\x03", true),
            b"\x1b[200~a[201~rm -rf /\x1b[201~".to_vec()
        );
        assert_eq!(
            encode_paste("a\u{9b}201~oops", true),
            b"\x1b[200~a201~oops\x1b[201~".to_vec()
        );
        assert_eq!(
            encode_paste("one\r\ntwo\nthree", false),
            b"one\rtwo\rthree".to_vec()
        );
    }

    // mode_bits mirrors the private modes programs toggle at runtime.
    #[test]
    fn mode_bits_track_private_modes() {
        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate);
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");

        assert_eq!(surface.mode_bits() & (1 << 3), 0);
        surface.inject_output(b"\x1b[?2004h");
        assert_eq!(surface.mode_bits() & (1 << 3), 1 << 3);
        surface.inject_output(b"\x1b[?1h\x1b[?1049h\x1b[?1000h");
        assert_eq!(surface.mode_bits(), 0b1111);
    }

    // paste() keys off live terminal state: markers go to the child only
    // once the program turned mode 2004 on. The sleeper child never reads,
    // so what lands in the grid is the tty line discipline's echo, which
    // renders the ESC of each marker as ^[ (ECHOCTL).
    #[test]
    fn paste_brackets_when_the_program_asks() {
        let surface = quiet_surface(60, 10);
        let mut state = RenderState::new(&surface);
        std::thread::sleep(Duration::from_millis(150));

        let grid_rows = |state: &mut RenderState, needle: &str| {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                state.update();
                let rows: Vec<String> =
                    (0..state.lines()).map(|i| state.text_row(i)).collect();
                if rows.iter().any(|row| row.contains(needle)) {
                    return rows;
                }
                if Instant::now() >= deadline {
                    panic!("echo of {needle:?} never reached the grid: {rows:?}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        };

        surface.paste("plain\n");
        let rows = grid_rows(&mut state, "plain");
        assert!(
            !rows.iter().any(|row| row.contains("^[[200~")),
            "unbracketed paste must not emit markers: {rows:?}"
        );

        surface.inject_output(b"\x1b[?2004h");
        surface.paste("wrapped");
        grid_rows(&mut state, "^[[200~wrapped^[[201~");
    }

    // A child that never writes, so buffer contents are exactly what the
    // test injected and comparisons can't race the shell prompt. Spawned
    // via /bin/sh, the one binary the nix build sandbox provides.
    fn quiet_surface(cols: u16, rows: u16) -> Surface {
        let engine = Engine::new(Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        }));
        engine
            .create_surface(&SurfaceDesc {
                shell: Some("/bin/sh".to_string()),
                args: vec!["-c".to_string(), "sleep 300".to_string()],
                cols,
                rows,
                ..SurfaceDesc::default()
            })
            .expect("spawn sleeper")
    }

    // serialize() must reconstruct content, styling, and hyperlinks when
    // replayed into a fresh same-width terminal. Serializing the replica
    // again is the equality check: identical bytes means identical state.
    #[test]
    fn serialize_round_trips_styles_and_links() {
        let surface = quiet_surface(40, 10);
        surface.inject_output(
            b"\x1b[31mred\x1b[0m plain \x1b[1;4;38;5;208morange\x1b[0m\r\n\
              \x1b[48;2;10;20;30mrgb bg\x1b[0m \
              \x1b]8;;https://rioterm.com\x1b\\link\x1b]8;;\x1b\\\r\n\
              \x1b[5mslow\x1b[0m \x1b[6mfast\x1b[0m\r\n\
              wide: \xe4\xbd\xa0\xe5\xa5\xbd",
        );
        let first = surface.serialize();
        assert!(first.contains("\x1b]8;;https://rioterm.com\x1b\\"));
        assert!(first.contains(";5m"), "missing slow blink: {first:?}");
        assert!(first.contains(";6m"), "missing rapid blink: {first:?}");

        let replica = quiet_surface(40, 10);
        replica.inject_output(first.as_bytes());
        assert_eq!(replica.dump(), surface.dump());
        assert_eq!(replica.serialize(), first);
    }

    // The C ABI marks cells carrying an explicit SGR 58 underline color
    // and hands the color out; unmarked cells take the glyph's color.
    #[test]
    fn capi_reports_underline_color() {
        use crate::capi::{
            rio_render_state_cell, rio_render_state_cell_underline_color,
            RIO_CELL_HAS_UNDERLINE_COLOR, RIO_COLOR_INDEXED, RIO_COLOR_NONE,
            RIO_COLOR_RGB,
        };

        let surface = quiet_surface(10, 3);
        surface.inject_output(b"\x1b[4;58;2;10;20;30ma\x1b[0mb\x1b[4;58;5;196mc\x1b[0m");
        let mut state = RenderState::new(&surface);
        state.update();

        let marked = unsafe { rio_render_state_cell(&state, 0, 0) };
        assert_ne!(marked.style_flags & RIO_CELL_HAS_UNDERLINE_COLOR, 0);
        let color = unsafe { rio_render_state_cell_underline_color(&state, 0, 0) };
        assert_eq!(color.kind, RIO_COLOR_RGB);
        assert_eq!((color.r, color.g, color.b), (10, 20, 30));

        let plain = unsafe { rio_render_state_cell(&state, 0, 1) };
        assert_eq!(plain.style_flags & RIO_CELL_HAS_UNDERLINE_COLOR, 0);
        // The NONE sentinel promises zeroed value and rgb.
        let absent = unsafe { rio_render_state_cell_underline_color(&state, 0, 1) };
        assert_eq!(absent.kind, RIO_COLOR_NONE);
        assert_eq!(absent.value, 0);
        assert_eq!((absent.r, absent.g, absent.b), (0, 0, 0));

        // Indexed colors keep their form and still resolve rgb (256-color
        // cube index 196 is pure red).
        let indexed = unsafe { rio_render_state_cell_underline_color(&state, 0, 2) };
        assert_eq!(indexed.kind, RIO_COLOR_INDEXED);
        assert_eq!(indexed.value, 196);
        assert_eq!((indexed.r, indexed.g, indexed.b), (255, 0, 0));

        // A NULL state and an out-of-range cell also answer NONE, and a
        // missing cell's fg/bg carry the NONE kind.
        let null =
            unsafe { rio_render_state_cell_underline_color(std::ptr::null(), 0, 0) };
        assert_eq!(null.kind, RIO_COLOR_NONE);
        let oob = unsafe { rio_render_state_cell_underline_color(&state, 99, 99) };
        assert_eq!(oob.kind, RIO_COLOR_NONE);
        let missing = unsafe { rio_render_state_cell(&state, 99, 99) };
        assert_eq!(missing.fg.kind, RIO_COLOR_NONE);
        assert_eq!(missing.bg.kind, RIO_COLOR_NONE);
    }

    // Scrollback rows come first, and wrapped rows are emitted without a
    // newline so the replica re-wraps them at the same width.
    #[test]
    fn serialize_covers_scrollback_and_rewraps() {
        let surface = quiet_surface(20, 5);
        for i in 0..12 {
            surface.inject_output(format!("history line {i}\r\n").as_bytes());
        }
        surface.inject_output(b"abcdefghijklmnopqrstuvwxyz");
        assert!(surface.history_size() > 0);
        let first = surface.serialize();

        let replica = quiet_surface(20, 5);
        replica.inject_output(first.as_bytes());
        assert_eq!(replica.dump(), surface.dump());
        assert_eq!(replica.history_size(), surface.history_size());
        assert_eq!(replica.serialize(), first);
    }

    // Search coordinates are ring-relative: line 0 is the top of the
    // scrollback, so hits stay valid however the viewport is scrolled.
    #[test]
    fn search_reports_ring_relative_coordinates() {
        let surface = quiet_surface(40, 5);
        for i in 0..8 {
            surface.inject_output(format!("filler {i}\r\n").as_bytes());
        }
        surface.inject_output(b"needle at last");

        let fillers = surface.search("filler", 100).expect("valid pattern");
        assert_eq!(fillers.len(), 8);
        assert_eq!(fillers[0].0, 0);
        assert_eq!(fillers[7].0, 7);

        let matches = surface.search("needle", 10).expect("valid pattern");
        assert_eq!(matches.len(), 1);
        let (start_line, start_col, end_line, end_col) = matches[0];
        assert_eq!(start_line as usize, surface.history_size() + 4);
        assert_eq!(start_col, 0);
        assert_eq!(end_line, start_line);
        assert_eq!(end_col, 5);

        assert_eq!(surface.search("filler", 3).unwrap().len(), 3);
        assert!(surface.search("[", 10).is_none());
    }

    // The parser must survive chunk boundaries: transports split output
    // arbitrarily, including mid-escape-sequence. A discarded parser
    // would print the tail of the sequence as literal text.
    #[test]
    fn escape_sequences_survive_chunk_boundaries() {
        let surface = quiet_surface(40, 5);
        // Split an SGR sequence in the middle: "\x1b[31m" + "red".
        surface.inject_output(b"\x1b[3");
        surface.inject_output(b"1mred");
        let dump = surface.dump();
        assert_eq!(dump.trim_end(), "red", "no literal escape tail: {dump:?}");
        // The style applied: serialize carries the red foreground (the
        // default bg is covered by the leading reset).
        assert!(surface.serialize().contains("\x1b[0;31m"));

        // Split inside an OSC title too.
        surface.inject_output(b"\x1b]0;hel");
        surface.inject_output(b"lo\x07after");
        assert!(surface.dump().contains("redafter"));
    }

    // DECTCEM and a scrolled viewport both hide the cursor from
    // renderers; showing it again restores visibility.
    #[test]
    fn cursor_visibility_tracks_dectcem_and_scroll() {
        let surface = quiet_surface(40, 5);
        let mut state = RenderState::new(&surface);

        state.update();
        assert!(state.cursor_visible());

        surface.inject_output(b"\x1b[?25l");
        state.update();
        assert!(!state.cursor_visible());

        surface.inject_output(b"\x1b[?25h");
        state.update();
        assert!(state.cursor_visible());

        for i in 0..20 {
            surface.inject_output(format!("line {i}\r\n").as_bytes());
        }
        surface.scroll(5);
        state.update();
        assert!(!state.cursor_visible(), "scrolled view has no live cursor");
        surface.scroll(-100);
        state.update();
        assert!(state.cursor_visible());
    }

    // Plain-text URLs resolve under the pointer, prose punctuation stays
    // prose, and non-link text misses.
    #[test]
    fn urls_resolve_under_the_pointer() {
        let surface = quiet_surface(60, 5);
        surface.inject_output(b"see https://rio.dev, or (http://a.b/c).");

        let (uri, start, end) = surface.url_at(0, 10).expect("hover on the url");
        assert_eq!(uri, "https://rio.dev");
        assert_eq!((start, end), (4, 18));
        // The comma after the URL is prose, not link.
        assert!(surface.url_at(0, 19).is_none());
        // Parenthesized URL: closing paren and period trimmed.
        let (uri, start, end) = surface.url_at(0, 30).expect("hover in parens");
        assert_eq!(uri, "http://a.b/c");
        assert_eq!((start, end), (25, 36));
        // Plain words miss.
        assert!(surface.url_at(0, 0).is_none());
    }

    // A pipe ends a URL: `curl https://x.dev|jq` must not swallow the
    // pipeline into the link.
    #[test]
    fn urls_stop_at_pipes() {
        let surface = quiet_surface(60, 5);
        surface.inject_output(b"curl https://x.dev|jq .");
        let (uri, start, end) = surface.url_at(0, 10).expect("hover on url");
        assert_eq!(uri, "https://x.dev");
        assert_eq!((start, end), (5, 17));
        assert!(surface.url_at(0, 18).is_none());
    }

    // Hit-testing takes the scroll position into account: viewport
    // coordinates keep resolving after the view moves into history.
    #[test]
    fn urls_resolve_while_scrolled() {
        let surface = quiet_surface(40, 5);
        surface.inject_output(b"https://rio.dev/x first\r\n");
        for i in 0..8 {
            surface.inject_output(format!("filler {i}\r\n").as_bytes());
        }
        assert!(surface.url_at(0, 3).is_none(), "url is above the viewport");
        surface.scroll(100);
        let (uri, start, end) = surface.url_at(0, 3).expect("scrolled to top");
        assert_eq!(uri, "https://rio.dev/x");
        assert_eq!((start, end), (0, 16));
    }

    // A URL that wraps across rows resolves whole from either row, with
    // per-row run bounds for hover underlining.
    #[test]
    fn urls_span_wrapped_rows() {
        let surface = quiet_surface(20, 5);
        surface.inject_output(b"x https://example.com/abcdef end");

        let (uri, start, end) = surface.url_at(1, 3).expect("hover on second row");
        assert_eq!(uri, "https://example.com/abcdef");
        assert_eq!((start, end), (0, 7));
        let (uri, start, end) = surface.url_at(0, 5).expect("hover on first row");
        assert_eq!(uri, "https://example.com/abcdef");
        assert_eq!((start, end), (2, 19));
        // Past the URL on the second row is plain text again.
        assert!(surface.url_at(1, 10).is_none());
    }

    // A scheme URL mid-line followed by a parenthesized note: the hover
    // run must sit exactly under the URL, not drift into the scheme or
    // the note.
    #[test]
    fn urls_underline_alignment_mid_line() {
        let surface = quiet_surface(90, 5);
        surface.inject_output(
            b"Dev server is listening at http://localhost:1313/ (bind address 127.0.0.1)",
        );

        // "Dev server is listening at " is 27 cells, so the URL occupies
        // cols 27..=48 and the note after it is prose.
        for col in [27, 31, 35, 48] {
            let (uri, start, end) = surface
                .url_at(0, col)
                .unwrap_or_else(|| panic!("hover at col {col} misses the url"));
            assert_eq!(uri, "http://localhost:1313/");
            assert_eq!((start, end), (27, 48), "hover at col {col}");
        }
        assert!(surface.url_at(0, 26).is_none(), "space before the scheme");
        assert!(surface.url_at(0, 50).is_none(), "note after the url");
    }

    // OSC 9;4 (ConEmu progress) must reach the embedder as an action:
    // set with a value, then remove.
    #[test]
    fn progress_reports_reach_the_delegate() {
        let delegate = Arc::new(ActionRecorder {
            actions: Mutex::new(Vec::new()),
        });
        let engine = Engine::new(delegate.clone());
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");

        surface.inject_output(b"\x1b]9;4;1;42\x07");
        surface.inject_output(b"\x1b]9;4;0\x07");

        let actions = delegate.actions.lock().unwrap();
        let progress: Vec<&Action> = actions
            .iter()
            .filter(|a| matches!(a, Action::Progress { .. }))
            .collect();
        assert_eq!(
            progress,
            vec![
                &Action::Progress {
                    state: 1,
                    value: 42
                },
                &Action::Progress { state: 0, value: 0 },
            ]
        );
    }

    #[test]
    fn drives_a_real_shell_and_reads_cells() {
        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate.clone());
        let desc = SurfaceDesc::default();
        let surface = engine.create_surface(&desc).expect("spawn shell");
        let mut state = RenderState::new(&surface);

        std::thread::sleep(Duration::from_millis(400));
        surface.text("printf '%s%s\\n' li brio-gate\r");

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut found = false;
        while Instant::now() < deadline {
            state.update();
            let lines = state.lines();
            if (0..lines).any(|i| state.text_row(i).contains("librio-gate")) {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        if !found {
            let rows: Vec<String> = (0..6).map(|i| state.text_row(i)).collect();
            panic!(
                "expected shell output in grid; wakeups={} rows={:?}",
                delegate.wakeups.load(Ordering::SeqCst),
                rows
            );
        }
        assert!(delegate.wakeups.load(Ordering::SeqCst) > 0);

        surface.selection_begin(0, 0, SelectionKind::Simple, Side::Left);
        surface.selection_update(0, 9, Side::Right);
        let text = surface.selection_text().expect("selection text");
        assert!(!text.is_empty());
        state.update();
        assert!(state.selection().is_some());
        surface.selection_clear();
        assert!(surface.selection_text().is_none());
    }

    // Typing while scrolled into history must snap the view back to the
    // live screen (and hide-cursor logic keys off the same offset).
    #[test]
    fn input_scrolls_back_to_live_screen() {
        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate);
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");
        let mut state = RenderState::new(&surface);

        // Three screens of output builds scrollback to scroll into.
        let mut text = String::new();
        for i in 0..72 {
            text.push_str(&format!("line {i}\r\n"));
        }
        surface.inject_output(text.as_bytes());
        surface.scroll(10);
        state.update();
        assert!(
            state.display_offset() > 0,
            "scroll(10) should enter history"
        );

        surface.write(b"x".to_vec());
        state.update();
        assert_eq!(state.display_offset(), 0, "input should snap to bottom");
    }

    // Erase fills (EL/ED with a colored bg, htop's header bar, `clear`)
    // produce bg-only cells that encode the color inline instead of a
    // style id; the snapshot accessors must decode them, not read the
    // color bits as a style-table index.
    #[test]
    fn erase_fills_resolve_inline_bg() {
        use rio_vt::config::colors::{AnsiColor, ColorRgb};

        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate);
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");
        let mut state = RenderState::new(&surface);

        // Rows 5-6 (below any shell prompt): green bg + erase-to-EOL,
        // then a truecolor bg + erase-whole-line.
        surface.inject_output(
            b"\x1b[6;1H\x1b[42mA\x1b[K\r\n\x1b[48;2;9;8;7m\x1b[2KB\x1b[0m",
        );
        state.update();

        let last = state.columns() - 1;
        let el_fill = state.style_at(5, last, state.square(5, last).unwrap());
        assert_eq!(el_fill.bg, AnsiColor::Indexed(2));

        let el2_fill = state.style_at(6, last, state.square(6, last).unwrap());
        assert_eq!(el2_fill.bg, AnsiColor::Spec(ColorRgb { r: 9, g: 8, b: 7 }));

        // Snapshot text renders the fills as trimmable spaces, not NULs.
        assert_eq!(state.text_row(5), "A");
        assert_eq!(state.text_row(6), "B");
    }

    // A kitty graphics transmit-and-display (a=T) must surface through
    // the render-state snapshot: placement geometry resolved against the
    // viewport, image dimensions, and an RGBA copy for the renderer.
    #[test]
    fn kitty_image_reaches_render_state() {
        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate);
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");
        let mut state = RenderState::new(&surface);

        // 2x2 RGBA (red, green, blue, white) placed at row 6, col 5.
        let pixels: [u8; 16] = [
            0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, //
            0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        surface.inject_output(
            b"\x1b[6;5H\x1b_Gf=32,s=2,v=2,i=7,a=T;/wAA/wD/AP8AAP///////w==\x1b\\",
        );
        state.update();

        assert_eq!(state.kitty_count(), 1);
        let (image_id, z_index, geometry) = state
            .kitty_geometry(0, 8.0, 16.0)
            .expect("placement in view");
        assert_eq!(image_id, 7);
        assert_eq!(z_index, 0);
        assert_eq!(geometry.x, 4.0 * 8.0);
        assert_eq!(geometry.y, 5.0 * 16.0);
        assert_eq!(geometry.width, 2.0);
        assert_eq!(geometry.height, 2.0);
        assert_eq!(geometry.source_rect, [0.0, 0.0, 1.0, 1.0]);

        let (width, height, _stamp) = state.kitty_image_info(7).expect("stored image");
        assert_eq!((width, height), (2, 2));

        let mut buf = [0u8; 16];
        assert_eq!(state.kitty_image_rgba(7, &mut buf), 16);
        assert_eq!(buf, pixels);

        // Too-small buffers are refused rather than partially filled.
        let mut small = [0u8; 4];
        assert_eq!(state.kitty_image_rgba(7, &mut small), 0);
    }

    // Same sequence the live repro used: deep scrollback, clear, then a
    // kitty transmit+display. The placement must resolve on screen.
    #[test]
    fn kitty_geometry_survives_scrollback() {
        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate);
        // A tiny scrollback forces ring eviction: kitty dest_rows count
        // evicted lines too, so the viewport math must include them or
        // every placement in a long-lived session drifts off-screen.
        let surface = engine
            .create_surface(&SurfaceDesc {
                scrollback: 16,
                ..SurfaceDesc::default()
            })
            .expect("spawn shell");
        let mut state = RenderState::new(&surface);

        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!("line {i}\r\n"));
        }
        surface.inject_output(text.as_bytes());
        surface.inject_output(b"\x1b[2J\x1b[H");
        surface
            .inject_output(b"\x1b_Gf=32,s=2,v=2,i=7,a=T;/wAA/wD/AP8AAP///////w==\x1b\\");
        state.update();

        assert_eq!(state.kitty_count(), 1);
        let (image_id, _z, geometry) = state
            .kitty_geometry(0, 8.0, 16.0)
            .expect("placement visible after scrollback");
        assert_eq!(image_id, 7);
        assert_eq!(geometry.y, 0.0);
    }

    // Virtual placements (`U=1`): the image is registered but only drawn
    // where the application prints U+10EEEE placeholder cells whose fg
    // color + combining diacritics say which image/row/column each cell
    // shows. This is what `kitten icat --unicode-placeholder` (and yazi
    // under a multiplexer) emits.
    #[test]
    fn kitty_virtual_placeholders_resolve_runs() {
        use rio_vt::ansi::kitty_virtual::encode_placeholder;

        let delegate = Arc::new(CountingDelegate {
            wakeups: AtomicUsize::new(0),
        });
        let engine = Engine::new(delegate);
        let surface = engine
            .create_surface(&SurfaceDesc::default())
            .expect("spawn shell");
        let mut state = RenderState::new(&surface);

        // 2x2 RGBA transmitted as a virtual placement spanning 2 cols x 1
        // row, then a run of two placeholder cells (image row 0, cols 0-1)
        // with fg palette index 7 = image id 7.
        let mut text = String::from(
            "\x1b[2J\x1b[H\x1b_Gf=32,s=2,v=2,i=7,a=T,U=1,c=2,r=1;/wAA/wD/AP8AAP///////w==\x1b\\",
        );
        text.push_str("\x1b[4;3H\x1b[38;5;7m");
        text.push_str(&encode_placeholder(0, 0, None));
        text.push_str(&encode_placeholder(0, 1, None));
        text.push_str("\x1b[39m");
        surface.inject_output(text.as_bytes());
        state.update();

        assert_eq!(state.kitty_count(), 1);
        let (image_id, z_index, geometry) = state
            .kitty_geometry(0, 8.0, 16.0)
            .expect("run resolves to geometry");
        assert_eq!(image_id, 7);
        assert_eq!(z_index, 0, "virtual placements use Kitty's default z-index");
        // Placement box: 2 cols x 1 row of 8x16 cells = 16x16 px; the 2x2
        // image aspect-fits to exactly 16x16, and the run starts at cell
        // (row 3, col 2), i.e. pixel (16, 48).
        assert_eq!(geometry.x, 16.0);
        assert_eq!(geometry.y, 48.0);
        assert_eq!(geometry.width, 16.0);
        assert_eq!(geometry.height, 16.0);
        assert_eq!(geometry.source_rect, [0.0, 0.0, 1.0, 1.0]);

        // The RGBA copy path serves virtual images the same way.
        let mut buf = [0u8; 16];
        assert_eq!(state.kitty_image_rgba(7, &mut buf), 16);
    }
}
