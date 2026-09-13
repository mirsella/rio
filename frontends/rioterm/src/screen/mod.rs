// MIT License
// Copyright 2022-present Raphael Amorim
//
// The functions (including comments) and logic of process_key_event, build_key_sequence, process_mouse_bindings, copy_selection, start_selection, update_selection_scrolling,
// side_by_pos, on_left_click, paste, sgr_mouse_report, mouse_report, normal_mouse_report, scroll,
// were retired from https://github.com/alacritty/alacritty/blob/c39c3c97f1a1213418c3629cc59a1d46e34070e0/alacritty/src/input.rs
// which is licensed under Apache 2.0 license.

pub mod touch;

use crate::bindings::{
    Action as Act, BindingKey, BindingMode, FontSizeAction, MouseBinding, SearchAction,
    ViAction,
};
use crate::context;
use crate::context::renderable::{Cursor, RenderableContent};
use crate::context::{next_rich_text_id, process_open_url, ContextManager, GridTransfer};
use crate::crosswords::{
    grid::Scroll,
    pos::{Column, Pos, Side},
    vi_mode::ViMotion,
    Mode,
};
use crate::hints::HintState;
use crate::layout::ContextDimension;
use crate::mouse::{calculate_mouse_position, Mouse};
use crate::renderer::island::{self, TabStripLayout};
use crate::renderer::{utils::padding_top_from_config, Renderer};
use crate::selection::SelectionType;
use core::fmt::Debug;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use rio_backend::clipboard::Clipboard;
use rio_backend::clipboard::ClipboardType;
use rio_backend::config::layout::Margin;
use rio_backend::config::renderer::Backend;
use rio_backend::crosswords::pos::{Boundary, CursorState, Direction, Line};
use rio_backend::error::{RioError, RioErrorLevel, RioErrorType};
use rio_backend::event::{ClickState, EventProxy, SearchState};
use rio_backend::sugarloaf::{
    layout::RootStyle, Sugarloaf, SugarloafBackend, SugarloafErrors, SugarloafRenderer,
    SugarloafWindow, SugarloafWindowSize,
};
use rio_session::protocol::{
    KeyAction as SessionKeyAction, KeyCode as SessionKeyCode, KeyInput, SessionCommand,
};
use rio_window::event::ElementState;
use rio_window::event::Modifiers;
use rio_window::event::MouseButton;
#[cfg(target_os = "macos")]
use rio_window::keyboard::ModifiersKeyState;
use rio_window::keyboard::{Key, ModifiersState, NamedKey};
use rio_window::platform::modifier_supplement::KeyEventExtModifierSupplement;
use rio_window::window::CursorIcon;
use std::error::Error;
use std::ffi::OsStr;
use touch::TouchPurpose;

/// Maximum number of lines for the blocking search while still typing the search regex.
const MAX_SEARCH_WHILE_TYPING: Option<usize> = Some(1000);

/// Maximum number of search terms stored in the history.
const MAX_SEARCH_HISTORY_SIZE: usize = 255;

fn wire_mouse_modifiers(modifiers: ModifiersState) -> u8 {
    let mut bits = 0;
    if modifiers.shift_key() {
        bits |= 1;
    }
    if modifiers.control_key() {
        bits |= 1 << 1;
    }
    if modifiers.alt_key() {
        bits |= 1 << 2;
    }
    if modifiers.super_key() {
        bits |= 1 << 3;
    }
    bits
}

fn session_key_input(
    key: &rio_window::event::KeyEvent,
    modifiers: ModifiersState,
    alt_is_meta: bool,
) -> Option<KeyInput> {
    let action = match key.state {
        ElementState::Pressed if key.repeat => SessionKeyAction::Repeat,
        ElementState::Pressed => SessionKeyAction::Press,
        ElementState::Released => SessionKeyAction::Release,
    };
    let mut modifier_bits = 0;
    if modifiers.shift_key() {
        modifier_bits |= 1;
    }
    if modifiers.control_key() {
        modifier_bits |= 1 << 1;
    }
    if modifiers.alt_key() {
        modifier_bits |= 1 << 2;
    }
    if modifiers.super_key() {
        modifier_bits |= 1 << 3;
    }

    let key_without_modifiers = key.key_without_modifiers();
    let wire_key = match key_without_modifiers {
        Key::Character(value) => {
            let mut chars = value.chars();
            let character = chars.next()?;
            if chars.next().is_some() {
                None
            } else {
                Some(SessionKeyCode::Char(character))
            }
        }
        Key::Named(named) => named_key_code(named),
        Key::Dead(Some(character)) => Some(SessionKeyCode::Char(character)),
        Key::Dead(None) | Key::Unidentified(_) => None,
    };

    let text = key
        .text_with_all_modifiers()
        .or(key.text.as_deref())
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let consumed_modifiers = if modifiers.alt_key() && !alt_is_meta {
        1 << 2
    } else {
        0
    };

    Some(KeyInput {
        action,
        key: wire_key,
        modifiers: modifier_bits,
        consumed_modifiers,
        text,
        composing: false,
    })
}

fn named_key_code(key: NamedKey) -> Option<SessionKeyCode> {
    Some(match key {
        NamedKey::Enter => SessionKeyCode::Enter,
        NamedKey::Tab => SessionKeyCode::Tab,
        NamedKey::Backspace => SessionKeyCode::Backspace,
        NamedKey::Escape => SessionKeyCode::Escape,
        NamedKey::ArrowUp => SessionKeyCode::Up,
        NamedKey::ArrowDown => SessionKeyCode::Down,
        NamedKey::ArrowLeft => SessionKeyCode::Left,
        NamedKey::ArrowRight => SessionKeyCode::Right,
        NamedKey::Home => SessionKeyCode::Home,
        NamedKey::End => SessionKeyCode::End,
        NamedKey::PageUp => SessionKeyCode::PageUp,
        NamedKey::PageDown => SessionKeyCode::PageDown,
        NamedKey::Insert => SessionKeyCode::Insert,
        NamedKey::Delete => SessionKeyCode::Delete,
        NamedKey::CapsLock => SessionKeyCode::CapsLock,
        NamedKey::Shift => SessionKeyCode::ShiftLeft,
        NamedKey::Control => SessionKeyCode::ControlLeft,
        NamedKey::Alt | NamedKey::AltGraph => SessionKeyCode::AltLeft,
        NamedKey::Super | NamedKey::Meta => SessionKeyCode::SuperLeft,
        NamedKey::F1 => SessionKeyCode::Function(1),
        NamedKey::F2 => SessionKeyCode::Function(2),
        NamedKey::F3 => SessionKeyCode::Function(3),
        NamedKey::F4 => SessionKeyCode::Function(4),
        NamedKey::F5 => SessionKeyCode::Function(5),
        NamedKey::F6 => SessionKeyCode::Function(6),
        NamedKey::F7 => SessionKeyCode::Function(7),
        NamedKey::F8 => SessionKeyCode::Function(8),
        NamedKey::F9 => SessionKeyCode::Function(9),
        NamedKey::F10 => SessionKeyCode::Function(10),
        NamedKey::F11 => SessionKeyCode::Function(11),
        NamedKey::F12 => SessionKeyCode::Function(12),
        NamedKey::F13 => SessionKeyCode::Function(13),
        NamedKey::F14 => SessionKeyCode::Function(14),
        NamedKey::F15 => SessionKeyCode::Function(15),
        NamedKey::F16 => SessionKeyCode::Function(16),
        NamedKey::F17 => SessionKeyCode::Function(17),
        NamedKey::F18 => SessionKeyCode::Function(18),
        NamedKey::F19 => SessionKeyCode::Function(19),
        NamedKey::F20 => SessionKeyCode::Function(20),
        NamedKey::F21 => SessionKeyCode::Function(21),
        NamedKey::F22 => SessionKeyCode::Function(22),
        NamedKey::F23 => SessionKeyCode::Function(23),
        NamedKey::F24 => SessionKeyCode::Function(24),
        NamedKey::F25 => SessionKeyCode::Function(25),
        NamedKey::F26 => SessionKeyCode::Function(26),
        NamedKey::F27 => SessionKeyCode::Function(27),
        NamedKey::F28 => SessionKeyCode::Function(28),
        NamedKey::F29 => SessionKeyCode::Function(29),
        NamedKey::F30 => SessionKeyCode::Function(30),
        NamedKey::F31 => SessionKeyCode::Function(31),
        NamedKey::F32 => SessionKeyCode::Function(32),
        NamedKey::F33 => SessionKeyCode::Function(33),
        NamedKey::F34 => SessionKeyCode::Function(34),
        NamedKey::F35 => SessionKeyCode::Function(35),
        NamedKey::Space => SessionKeyCode::Char(' '),
        _ => return None,
    })
}

pub struct Screen<'screen> {
    bindings: crate::bindings::KeyBindings,
    mouse_bindings: Vec<MouseBinding>,
    pub modifiers: Modifiers,
    pub mouse: Mouse,
    pub touchpurpose: TouchPurpose,
    pub search_state: SearchState,
    pub hint_state: HintState,
    pub renderer: Renderer,
    pub sugarloaf: Sugarloaf<'screen>,
    pub context_manager: context::ContextManager<EventProxy>,
    last_ime_cursor_pos: Option<(f32, f32)>,
    hints_config: Vec<std::rc::Rc<rio_backend::config::hints::Hint>>,
    /// Hint regexes compiled on first use, keyed by pattern. Hover
    /// hit-testing runs on every mouse move; recompiling the URL
    /// pattern each time is measurable jank.
    hint_regex_cache:
        std::cell::RefCell<std::collections::HashMap<String, std::rc::Rc<onig::Regex>>>,
    /// The viewport cell and modifiers of the last hover-hint probe
    /// that found nothing. Mouse events arrive per pixel; re-probing
    /// the same cell would re-extract and re-scan the logical line for
    /// every one of them. Viewport coordinates so the check needs no
    /// terminal lock, and only an unchanged probe that was not over a
    /// link is skippable, so text changing under a shown underline
    /// still refreshes it. Reset on wheel scroll and highlight clears.
    last_hint_probe: Option<(Pos, rio_window::keyboard::ModifiersState)>,
    pub resize_state: Option<crate::layout::ResizeState>,
    #[cfg(target_os = "macos")]
    pub allow_manual_dragging: bool,
    last_chrome_press: Option<ChromePress>,
    last_close_press: Option<(std::time::Instant, f32)>,
    pub grids: rustc_hash::FxHashMap<usize, rio_backend::sugarloaf::grid::GridRenderer>,
    pub grid_rasterizer: rio_grid::GridGlyphRasterizer,
    ready_session_imports: Vec<usize>,
    recovery_target: Option<usize>,
    recovery_action_requested: bool,
}

pub struct ChromePress {
    window_origin: Option<rio_window::dpi::PhysicalPosition<i32>>,
    at: std::time::Instant,
}

impl ChromePress {
    fn validates_double_click(
        &self,
        window_origin: Option<rio_window::dpi::PhysicalPosition<i32>>,
    ) -> bool {
        self.at.elapsed() <= crate::constants::MULTI_CLICK_THRESHOLD
            && self.window_origin == window_origin
    }
}

pub struct ScreenWindowProperties {
    pub size: rio_window::dpi::PhysicalSize<u32>,
    pub scale: f64,
    pub raw_window_handle: RawWindowHandle,
    pub raw_display_handle: RawDisplayHandle,
    pub window_id: rio_window::window::WindowId,
}

type RouteGraphics = rustc_hash::FxHashMap<
    rio_backend::sugarloaf::GraphicKey,
    rio_backend::sugarloaf::GraphicDataEntry,
>;

pub struct ScreenTransfer {
    grid: GridTransfer<EventProxy>,
    graphics: RouteGraphics,
}

#[cfg(all(feature = "wayland", target_os = "linux"))]
pub struct WindowTransfer {
    tabs: Vec<ScreenTransfer>,
    active_index: usize,
}

impl ScreenTransfer {
    pub fn id(&self) -> crate::layout::TabId {
        self.grid.id()
    }

    pub fn route_ids(&self) -> Vec<usize> {
        self.grid.route_ids()
    }
}

#[cfg_attr(not(all(feature = "wayland", target_os = "linux")), allow(dead_code))]
pub struct ScreenTransferFailure {
    pub(crate) transfer: ScreenTransfer,
    pub(crate) message: String,
}

impl std::fmt::Debug for ScreenTransferFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScreenTransferFailure")
            .field("tab_id", &self.transfer.id())
            .field("route_ids", &self.transfer.route_ids())
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for ScreenTransferFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ScreenTransferFailure {}

enum ScreenContext {
    Fresh(Option<String>),
    Transfer(ScreenTransfer),
}

#[inline]
fn window_should_be_opaque(config: &rio_backend::config::Config) -> bool {
    config.window.opacity >= 1.0 && !config.window.blur.is_glass()
}

fn route_ids_belong_to(
    graphics_routes: impl IntoIterator<Item = usize>,
    route_ids: &[usize],
) -> bool {
    graphics_routes
        .into_iter()
        .all(|route_id| route_ids.contains(&route_id))
}

fn scaled_margin_for_tabs(
    navigation: &rio_backend::config::navigation::Navigation,
    margin: Margin,
    macos_use_unified_titlebar: bool,
    tab_count: usize,
    scale: f32,
) -> Margin {
    Margin::new(
        padding_top_from_config(
            navigation,
            margin.top,
            tab_count,
            macos_use_unified_titlebar,
        ) * scale,
        margin.right * scale,
        margin.bottom * scale,
        margin.left * scale,
    )
}

enum ScreenBuildFailure {
    Fresh(Box<dyn Error>),
    Transfer(ScreenTransferFailure),
}

impl ScreenBuildFailure {
    fn into_error(self) -> Box<dyn Error> {
        match self {
            Self::Fresh(error) => error,
            Self::Transfer(error) => Box::new(error),
        }
    }

    fn into_transfer(self) -> ScreenTransferFailure {
        match self {
            Self::Transfer(error) => error,
            Self::Fresh(_) => {
                unreachable!("transfer construction returned a fresh error")
            }
        }
    }
}

impl Screen<'_> {
    pub fn new<'screen>(
        window_properties: ScreenWindowProperties,
        config: &rio_backend::config::Config,
        event_proxy: EventProxy,
        font_library: &rio_backend::sugarloaf::font::FontLibrary,
        open_url: Option<String>,
    ) -> Result<Screen<'screen>, Box<dyn Error>> {
        Self::build(
            window_properties,
            config,
            event_proxy,
            font_library,
            ScreenContext::Fresh(open_url),
        )
        .map_err(ScreenBuildFailure::into_error)
    }

    pub fn from_transfer<'screen>(
        window_properties: ScreenWindowProperties,
        config: &rio_backend::config::Config,
        event_proxy: EventProxy,
        font_library: &rio_backend::sugarloaf::font::FontLibrary,
        transfer: ScreenTransfer,
    ) -> Result<Screen<'screen>, ScreenTransferFailure> {
        Self::build(
            window_properties,
            config,
            event_proxy,
            font_library,
            ScreenContext::Transfer(transfer),
        )
        .map_err(ScreenBuildFailure::into_transfer)
    }

    fn build<'screen>(
        window_properties: ScreenWindowProperties,
        config: &rio_backend::config::Config,
        event_proxy: EventProxy,
        font_library: &rio_backend::sugarloaf::font::FontLibrary,
        screen_context: ScreenContext,
    ) -> Result<Screen<'screen>, ScreenBuildFailure> {
        let size = window_properties.size;
        let scale = window_properties.scale;
        let raw_window_handle = window_properties.raw_window_handle;
        let raw_display_handle = window_properties.raw_display_handle;
        let window_id = window_properties.window_id;
        let padding_y_top = padding_top_from_config(
            &config.navigation,
            config.margin.top,
            1,
            config.window.macos_use_unified_titlebar,
        );

        let padding_y_bottom = config.margin.bottom;
        let sugarloaf_layout =
            RootStyle::new(scale as f32, config.fonts.size, config.line_height);

        let mut sugarloaf_errors: Option<SugarloafErrors> = None;

        let sugarloaf_window = SugarloafWindow {
            handle: raw_window_handle,
            display: raw_display_handle,
            scale: scale as f32,
            size: SugarloafWindowSize {
                width: size.width as f32,
                height: size.height as f32,
            },
        };

        let backend = if config.renderer.use_cpu {
            SugarloafBackend::Cpu
        } else {
            // `wgpu_backend` (see build.rs): rioterm's own `wgpu`
            // feature, or Windows, where sugarloaf and rio-backend get
            // the feature through the target-specific dependency
            // override so the wgpu code paths always exist. Without the
            // Windows half, default builds there silently fall back to
            // the CPU rasterizer.
            match config.renderer.backend {
                // `Backend::Vulkan` from the user config means the
                // native ash backend on Linux. Other OSes fall through
                // to the wgpu Vulkan path when wgpu is available;
                // otherwise we degrade to CPU rasterizer.
                #[cfg(target_os = "linux")]
                Backend::Vulkan => SugarloafBackend::Vulkan,
                #[cfg(all(not(target_os = "linux"), wgpu_backend))]
                Backend::Vulkan => SugarloafBackend::Wgpu(wgpu::Backends::VULKAN),
                #[cfg(all(not(target_os = "linux"), not(wgpu_backend)))]
                Backend::Vulkan => SugarloafBackend::Cpu,
                #[cfg(target_os = "macos")]
                Backend::Metal => SugarloafBackend::Metal,
                #[cfg(all(wgpu_backend, target_arch = "wasm32"))]
                Backend::Webgpu => SugarloafBackend::Wgpu(
                    wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
                ),
                #[cfg(all(wgpu_backend, not(target_arch = "wasm32")))]
                Backend::Webgpu => SugarloafBackend::Wgpu(wgpu::Backends::all()),
                #[cfg(not(wgpu_backend))]
                Backend::Webgpu => SugarloafBackend::Cpu,
            }
        };

        let sugarloaf_renderer = SugarloafRenderer {
            backend,
            colorspace: config.window.colorspace.to_sugarloaf_colorspace(),
            // The exact predicate the rest of the frontend uses: glass
            // blur forces the window bg alpha to 0 regardless of opacity,
            // so it needs an alpha-carrying surface exactly like
            // `window.opacity < 1` does. Fixed at surface creation; a
            // live-reloaded opacity change takes effect on restart.
            prefer_alpha_capable_adapter: !window_should_be_opaque(config),
        };

        let mut sugarloaf: Sugarloaf = match Sugarloaf::new(
            sugarloaf_window,
            sugarloaf_renderer,
            font_library,
            sugarloaf_layout,
        ) {
            Ok(instance) => instance,
            Err(instance_with_errors) => {
                sugarloaf_errors = Some(instance_with_errors.errors);
                instance_with_errors.instance
            }
        };

        #[cfg(wgpu_backend)]
        sugarloaf.update_filters(config.renderer.filters.as_slice());

        let mut renderer = Renderer::new(config);

        let bindings = crate::bindings::default_key_bindings(config);

        let is_native = config.navigation.is_native();

        let open_url = match &screen_context {
            ScreenContext::Fresh(open_url) => open_url.as_deref(),
            ScreenContext::Transfer(_) => None,
        };
        let (shell, working_dir) = process_open_url(
            config.shell.to_owned(),
            config.working_dir.to_owned(),
            config.editor.to_owned(),
            open_url,
        );

        let context_manager_config = context::ContextManagerConfig {
            #[cfg(test)]
            dead_pty: false,
            cwd: config.navigation.current_working_directory,
            shell,
            working_dir,
            is_native,
            // When navigation does not contain any color rule
            // does not make sense fetch for foreground process names/path
            should_update_title_extra: !config.navigation.color_automation.is_empty(),
            split_color: config.colors.split,
            panel: config.panel,
            title: config.title.clone(),
            keyboard: config.keyboard.clone(),
            scrollback_history_limit: config.scrollback_history_limit,
            grapheme_clustering: config.grapheme_clustering,
        };

        let rich_text_id = next_rich_text_id();
        let margin = Margin::new(
            padding_y_top,
            config.margin.right,
            padding_y_bottom,
            config.margin.left,
        );
        let scaled_margin = Margin::new(
            padding_y_top * scale as f32,
            config.margin.right * scale as f32,
            padding_y_bottom * scale as f32,
            config.margin.left * scale as f32,
        );
        let (text_dimensions, cell_metrics) = sugarloaf.compute_cell_metrics(
            config.fonts.size,
            config.line_height,
            scale as f32,
        );
        let context_dimension = ContextDimension::build(
            size.width as f32,
            size.height as f32,
            text_dimensions,
            cell_metrics,
            config.line_height,
            config.fonts.size,
            margin,
        );

        let cursor = Cursor {
            content: config.cursor.shape.into(),
            content_ref: config.cursor.shape.into(),
            state: CursorState::new(config.cursor.shape.into()),
            is_ime_enabled: false,
        };

        let (context_manager, graphics, transferred) = match screen_context {
            ScreenContext::Fresh(_) => {
                let context_manager = context::ContextManager::start(
                    (&cursor, config.cursor.blinking),
                    event_proxy,
                    window_id.into(),
                    rich_text_id,
                    context_manager_config,
                    context_dimension,
                    scaled_margin,
                    sugarloaf_errors,
                )
                .map_err(ScreenBuildFailure::Fresh)?;
                (context_manager, None, false)
            }
            ScreenContext::Transfer(ScreenTransfer { grid, graphics }) => {
                if !route_ids_belong_to(
                    graphics.keys().map(|graphic| graphic.route_id),
                    &grid.route_ids(),
                ) {
                    return Err(ScreenBuildFailure::Transfer(ScreenTransferFailure {
                        transfer: ScreenTransfer { grid, graphics },
                        message: "transferred graphics contain an unrelated route".into(),
                    }));
                }

                let context_manager = ContextManager::from_transfer(
                    grid,
                    event_proxy,
                    window_id.into(),
                    context_manager_config,
                );
                (context_manager, Some(graphics), true)
            }
        };

        if let Some(graphics) = graphics {
            sugarloaf.image_data = graphics;
        }

        sugarloaf.set_window_opaque(window_should_be_opaque(config));
        sugarloaf.set_background_color(Some(renderer.dynamic_background.1));

        if let Some(image) = &config.window.background_image {
            if let Err(message) = sugarloaf.set_background_image(image) {
                renderer.assistant.set_error(RioError {
                    level: RioErrorLevel::Warning,
                    report: RioErrorType::BackgroundImageLoadFailure(message),
                });
            }
        } else {
            sugarloaf.clear_background_image();
        }

        let mut screen = Screen {
            search_state: SearchState::default(),
            hint_state: HintState::new(config.hints.alphabet.clone()),
            hints_config: config
                .hints
                .rules
                .iter()
                .map(|h| std::rc::Rc::new(h.clone()))
                .collect(),
            hint_regex_cache: Default::default(),
            last_hint_probe: None,
            mouse_bindings: crate::bindings::default_mouse_bindings(),
            modifiers: Modifiers::default(),
            context_manager,
            sugarloaf,
            mouse: Mouse::new(config.scroll.multiplier, config.scroll.divider),
            touchpurpose: TouchPurpose::default(),
            renderer,
            bindings,
            last_ime_cursor_pos: None,
            resize_state: None,
            #[cfg(target_os = "macos")]
            allow_manual_dragging: config.navigation.is_enabled(),
            last_chrome_press: None,
            last_close_press: None,
            grids: rustc_hash::FxHashMap::default(),
            grid_rasterizer: rio_grid::GridGlyphRasterizer::new(),
            ready_session_imports: Vec::new(),
            recovery_target: None,
            recovery_action_requested: false,
        };
        if transferred {
            screen.refresh_after_tab_transfer(size);
        }
        Ok(screen)
    }

    /// Extract compositor images for several routes.
    fn extract_routes_graphics(
        &mut self,
        route_ids: impl IntoIterator<Item = usize>,
    ) -> RouteGraphics {
        let route_ids: rustc_hash::FxHashSet<_> = route_ids.into_iter().collect();
        self.sugarloaf
            .extract_routes_graphics(route_ids.iter().copied())
    }

    pub(crate) fn discard_routes(&mut self, route_ids: impl IntoIterator<Item = usize>) {
        let route_ids: Vec<_> = route_ids.into_iter().collect();
        self.grids
            .retain(|route_id, _| !route_ids.contains(route_id));
        for route_id in &route_ids {
            self.sugarloaf
                .font_library()
                .remove_glyph_registry(*route_id);
        }
        drop(self.extract_routes_graphics(route_ids));
    }

    /// Extract a tab and all renderer state owned by its split routes.
    pub fn extract_transfer(&mut self, index: usize) -> Option<ScreenTransfer> {
        let grid = self.context_manager.extract_grid(index)?;
        let route_ids = grid.route_ids();
        for route_id in &route_ids {
            self.grids.remove(route_id);
        }
        let graphics = self.extract_routes_graphics(route_ids);
        Some(ScreenTransfer { grid, graphics })
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub fn extract_window_transfer(&mut self) -> Option<WindowTransfer> {
        let count = self.context_manager.len();
        if count == 0 {
            return None;
        }
        let active_index = self.context_manager.current_index();
        let tabs = (0..count)
            .map(|_| {
                self.extract_transfer(0)
                    .expect("window tab disappeared during extraction")
            })
            .collect();
        Some(WindowTransfer { tabs, active_index })
    }

    /// Transactionally insert a tab and its renderer-owned graphics.
    pub fn insert_transfer(
        &mut self,
        index: usize,
        transfer: ScreenTransfer,
        size: rio_window::dpi::PhysicalSize<u32>,
    ) -> Result<(), ScreenTransfer> {
        let ScreenTransfer { grid, graphics } = transfer;
        let route_ids = grid.route_ids();
        if !route_ids_belong_to(graphics.keys().map(|key| key.route_id), &route_ids)
            || self
                .sugarloaf
                .image_data
                .keys()
                .any(|key| route_ids.contains(&key.route_id))
        {
            return Err(ScreenTransfer { grid, graphics });
        }
        if let Err(graphics) = self.sugarloaf.insert_routes_graphics(graphics) {
            return Err(ScreenTransfer { grid, graphics });
        }
        if let Err(grid) = self.context_manager.insert_grid(index, grid) {
            let graphics = self
                .sugarloaf
                .extract_routes_graphics(route_ids.iter().copied());
            return Err(ScreenTransfer { grid, graphics });
        }
        self.refresh_after_tab_transfer(size);
        Ok(())
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub fn insert_window_transfer(
        &mut self,
        index: usize,
        transfer: WindowTransfer,
        size: rio_window::dpi::PhysicalSize<u32>,
    ) -> Result<(), WindowTransfer> {
        if transfer.active_index >= transfer.tabs.len()
            || !self
                .context_manager
                .can_insert_grids(index, transfer.tabs.len())
        {
            return Err(transfer);
        }

        let WindowTransfer { tabs, active_index } = transfer;
        let mut inserted = 0;
        let mut remaining = tabs.into_iter();
        while let Some(tab) = remaining.next() {
            match self.insert_transfer(index + inserted, tab, size) {
                Ok(()) => inserted += 1,
                Err(tab) => {
                    let mut tabs = Vec::with_capacity(inserted + 1);
                    for _ in 0..inserted {
                        tabs.push(
                            self.extract_transfer(index).expect(
                                "inserted window tab disappeared during rollback",
                            ),
                        );
                    }
                    tabs.push(tab);
                    tabs.extend(remaining);
                    return Err(WindowTransfer { tabs, active_index });
                }
            }
        }
        self.context_manager.set_current(index + active_index);
        Ok(())
    }

    pub fn refresh_after_tab_transfer(
        &mut self,
        size: rio_window::dpi::PhysicalSize<u32>,
    ) {
        self.sugarloaf.resize(size.width, size.height);
        let scale = self.sugarloaf.scale_factor();
        let scaled_margin = scaled_margin_for_tabs(
            &self.renderer.navigation,
            self.renderer.margin,
            self.renderer.macos_use_unified_titlebar,
            self.context_manager.len(),
            scale,
        );
        for grid in self.context_manager.contexts_mut() {
            grid.width = size.width as f32;
            grid.height = size.height as f32;
            grid.update_scaled_margin(scaled_margin);
            grid.update_scale(scale);
            for context in grid.contexts_mut().values_mut() {
                let context = context.context_mut();
                context.dimension.update_scale(scale);
                context
                    .renderable_content
                    .pending_update
                    .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
            }
            grid.update_dimensions(&mut self.sugarloaf);
        }
        self.resize_all_contexts();
        self.mark_dirty();
        // Tab-strip displacement is layout, not cursor travel.
        self.renderer.trail_cursor.snap();
    }

    #[inline]
    pub fn ctx(&self) -> &ContextManager<EventProxy> {
        &self.context_manager
    }

    #[inline]
    pub fn ctx_mut(&mut self) -> &mut ContextManager<EventProxy> {
        &mut self.context_manager
    }

    #[inline]
    pub fn mark_dirty(&mut self) {
        self.context_manager
            .current_mut()
            .renderable_content
            .pending_update
            .set_dirty();
    }

    #[inline]
    pub fn set_modifiers(&mut self, modifiers: Modifiers) {
        self.modifiers = modifiers;
    }

    #[inline]
    pub fn search_active(&self) -> bool {
        self.search_state.history_index.is_some()
    }

    #[inline]
    pub fn reset_mouse(&mut self) {
        self.mouse.accumulated_scroll = crate::mouse::AccumulatedScroll::default();
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub(crate) fn reset_drag_input(&mut self) -> Option<usize> {
        self.mouse.left_button_state = ElementState::Released;
        self.mouse.middle_button_state = ElementState::Released;
        self.mouse.right_button_state = ElementState::Released;
        self.mouse.on_border = false;
        if let Some(island) = self.renderer.island.as_mut() {
            island.cancel_drag();
        }
        self.handle_scrollbar_release();
        self.resize_state = None;
        (!self.context_manager.is_empty()).then(|| self.context_manager.current_route())
    }

    #[inline]
    pub fn select_current_based_on_mouse(&mut self) -> bool {
        if self
            .context_manager
            .current_grid_mut()
            .select_current_based_on_mouse(&self.mouse)
        {
            // The focusing click never reaches on_left_click, so a
            // selection left behind in the target panel would
            // drag-extend from its stale anchor; drop it on switch.
            self.clear_selection();
            return true;
        }
        false
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    fn start_wayland_window_drag(
        &mut self,
        window: &rio_window::window::Window,
        x_unscaled: f32,
        layout: &TabStripLayout,
    ) {
        use rio_window::platform::wayland::WindowExtWayland;

        if !window.supports_toplevel_drag() {
            return;
        }
        if let Some(island) = self.renderer.island.as_mut() {
            let Some(grid) = self.context_manager.current_grid_opt() else {
                return;
            };
            island.start_window_drag(
                grid.id(),
                x_unscaled - layout.left_margin,
                x_unscaled,
            );
        }
    }

    #[inline]
    pub fn mouse_position(&self, display_offset: usize) -> Pos {
        let current_grid = self.context_manager.current_grid();
        let (context, margin) = current_grid.current_context_with_computed_dimension();
        let context_dimension = context.dimension;
        calculate_mouse_position(
            &self.mouse,
            display_offset,
            (context_dimension.columns, context_dimension.lines),
            margin.left,
            margin.top,
            (
                context_dimension.cell.cell_width,
                context_dimension.cell.cell_height,
            ),
        )
    }

    #[inline]
    pub fn touch_purpose(&mut self) -> &mut TouchPurpose {
        &mut self.touchpurpose
    }

    /// update_config is triggered in any configuration file update
    #[inline]
    pub fn update_config(
        &mut self,
        config: &rio_backend::config::Config,
        font_library: &rio_backend::sugarloaf::font::FontLibrary,
        should_update_font_library: bool,
    ) {
        let num_tabs = self.ctx().len();
        let padding_y_top = padding_top_from_config(
            &config.navigation,
            config.margin.top,
            num_tabs,
            config.window.macos_use_unified_titlebar,
        );
        let padding_y_bottom = config.margin.bottom;
        let scale = self.sugarloaf.scale_factor();

        if should_update_font_library {
            self.sugarloaf.update_font(font_library);
        }
        let s = self.sugarloaf.style_mut();
        s.font_size = config.fonts.size;
        s.line_height = config.line_height;

        // `wgpu_backend`, not the bare feature: default Windows builds
        // run the wgpu path too, and skipping this made `[renderer]
        // filters` edits require a restart there.
        #[cfg(wgpu_backend)]
        self.sugarloaf
            .update_filters(config.renderer.filters.as_slice());

        // Rebuild bindings so `[bindings]` edits live-reload like the
        // rest of the config instead of waiting for a new window.
        self.bindings = crate::bindings::default_key_bindings(config);

        let old_island = self.renderer.island.take();
        let was_focused = self.renderer.is_window_focused;
        self.renderer = Renderer::new(config);
        self.renderer.is_window_focused = was_focused;
        if self.renderer.island.is_some() {
            self.renderer.island = Some(old_island.unwrap_or_else(island::Island::new));
        }
        for context_grid in self.context_manager.contexts_mut() {
            context_grid.update_line_height(config.line_height);

            context_grid.update_scaled_margin(Margin::new(
                padding_y_top * scale,
                config.margin.right * scale,
                padding_y_bottom * scale,
                config.margin.left * scale,
            ));

            // Update per-panel font size and line height BEFORE
            // update_dimensions — the recompute reads from these
            // fields. `rebaseline_font_size` also re-anchors the
            // "reset" target so the next change_font_size(Reset)
            // returns to the new config size.
            for current_context in context_grid.contexts_mut().values_mut() {
                let current_context = current_context.context_mut();
                current_context
                    .dimension
                    .rebaseline_font_size(config.fonts.size);
                current_context.dimension.line_height = config.line_height;
            }

            context_grid.update_dimensions(&mut self.sugarloaf);

            for current_context in context_grid.contexts_mut().values_mut() {
                let current_context = current_context.context_mut();
                current_context.renderable_content =
                    RenderableContent::from_cursor_config(&config.cursor);
                let shape = config.cursor.shape;
                current_context
                    .terminal
                    .lock()
                    .set_cursor_style(shape, config.cursor.blinking);
            }
        }

        self.mouse
            .set_multiplier_and_divider(config.scroll.multiplier, config.scroll.divider);

        // Update keyboard config in context manager
        self.context_manager.config.keyboard = config.keyboard.clone();

        // Re-evaluate the opaque flag — toggling `window.opacity` /
        // `window.blur` at runtime should flip the compositor mode.
        self.sugarloaf
            .set_window_opaque(window_should_be_opaque(config));

        self.sugarloaf
            .set_background_color(Some(self.renderer.dynamic_background.1));

        if let Some(image) = &config.window.background_image {
            if let Err(message) = self.sugarloaf.set_background_image(image) {
                self.renderer.assistant.set_error(RioError {
                    level: RioErrorLevel::Warning,
                    report: RioErrorType::BackgroundImageLoadFailure(message),
                });
            }
        } else {
            self.sugarloaf.clear_background_image();
        }

        self.resize_all_contexts();
        self.mark_dirty();
    }

    #[inline]
    pub fn change_font_size(&mut self, action: FontSizeAction) {
        let dim = &mut self.context_manager.current_mut().dimension;
        let changed = match action {
            FontSizeAction::Increase => dim.increase_font_size(),
            FontSizeAction::Decrease => dim.decrease_font_size(),
            FontSizeAction::Reset => dim.reset_font_size(),
        };
        if !changed {
            return;
        }

        self.context_manager
            .current_grid_mut()
            .update_dimensions(&mut self.sugarloaf);

        self.mark_dirty();
        self.resize_all_contexts();
        // Reflowed cursor displacement is layout, not travel.
        self.renderer.trail_cursor.snap();
    }

    #[inline]
    pub fn resize(&mut self, new_size: rio_window::dpi::PhysicalSize<u32>) -> &mut Self {
        if self
            .context_manager
            .current()
            .renderable_content
            .selection_range
            .is_some()
        {
            self.clear_selection();
        }
        self.refresh_after_tab_transfer(new_size);

        self
    }

    /// Re-read the window's live scale factor and re-run the rescale path
    /// when it diverged from the one being rendered with. Display
    /// reconfiguration during sleep/wake can change the backing scale
    /// without a `ScaleFactorChanged` ever being delivered (the macOS
    /// producer de-dupes on the numeric value and wake notifications
    /// coalesce), so cheap checkpoints call this instead of trusting
    /// event delivery. Returns whether a rescale ran.
    pub fn reconcile_scale(&mut self, winit_window: &rio_window::window::Window) -> bool {
        let live_scale = winit_window.scale_factor() as f32;
        if live_scale > 0.0
            && (live_scale - self.sugarloaf.scale_factor()).abs() > f32::EPSILON
        {
            self.set_scale(live_scale, winit_window.inner_size());
            return true;
        }
        false
    }

    #[inline]
    pub fn set_scale(
        &mut self,
        new_scale: f32,
        new_size: rio_window::dpi::PhysicalSize<u32>,
    ) -> &mut Self {
        self.sugarloaf.rescale(new_scale);
        self.refresh_after_tab_transfer(new_size);

        self
    }

    #[inline]
    pub fn resize_all_contexts(&mut self) {
        // whenever a resize update happens: it will stored in
        // the next layout, so once the messenger.send_resize triggers
        // the wakeup from pty it will also trigger a sugarloaf.render()
        // and then eventually a render with the new layout computation.
        for context_grid in self.context_manager.contexts_mut() {
            for context in context_grid.contexts_mut().values_mut() {
                let ctx = context.context_mut();
                let winsize = crate::renderer::utils::terminal_dimensions(&ctx.dimension);
                ctx.terminal.lock().resize_to(winsize);
            }
        }
    }

    #[inline]
    pub fn scroll_bottom_when_cursor_not_visible(&mut self) {
        {
            let current = self.ctx_mut().current_mut();
            if current.terminal.lock().display_offset() != 0 {
                current.terminal.lock().scroll_display(Scroll::Bottom);
            }
        }
        self.refresh_hints_after_scroll();
    }

    #[inline]
    pub fn mouse_mode(&self) -> bool {
        let mode = self.get_mode();
        mode.intersects(Mode::MOUSE_MODE) && !mode.contains(Mode::VI)
    }

    #[inline]
    pub fn display_offset(&self) -> usize {
        let terminal = self.ctx().current().terminal.lock();
        terminal.display_offset()
    }

    #[inline]
    pub fn get_mode(&self) -> Mode {
        let terminal = self.ctx().current().terminal.lock();
        terminal.mode()
    }

    pub fn scroll_page(&mut self, up: bool) {
        let rich_text_id;
        {
            let current = self.context_manager.current_mut();
            rich_text_id = current.rich_text_id;
            let mut terminal = current.terminal.lock();
            let scroll_lines = terminal.screen_lines() as i32 * if up { 1 } else { -1 };
            terminal.vi_scroll(scroll_lines);
            terminal.scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
        }
        self.refresh_selection_range();
        self.renderer.scrollbar.notify_scroll(rich_text_id);
        self.refresh_hints_after_scroll();
        self.mark_dirty();
    }

    pub fn scroll_half_page(&mut self, up: bool) {
        let rich_text_id;
        {
            let current = self.context_manager.current_mut();
            rich_text_id = current.rich_text_id;
            let mut terminal = current.terminal.lock();
            let half = terminal.screen_lines() as i32 / 2;
            let scroll_lines = if up { half } else { -half };
            terminal.vi_scroll(scroll_lines);
            terminal.scroll_display(Scroll::Delta(scroll_lines));
        }
        self.refresh_selection_range();
        self.renderer.scrollbar.notify_scroll(rich_text_id);
        self.refresh_hints_after_scroll();
        self.mark_dirty();
    }

    /// Sync the cached selection range after a scroll moved the vi cursor
    /// or clamped it back into the viewport.
    fn refresh_selection_range(&mut self) {
        let selection_range = {
            let context = self.context_manager.current_mut();
            context.terminal.lock().passive_selection_range()
        };
        self.context_manager
            .current_mut()
            .set_selection(selection_range);
    }

    pub fn refresh_hints_after_scroll(&mut self) {
        let quick_select_active = self.hint_state.is_active();
        if quick_select_active {
            {
                let terminal = self.context_manager.current().terminal.lock();
                self.hint_state.refresh_matches(&*terminal);
            }
            self.update_hint_state();
        }

        if quick_select_active || self.update_highlighted_hints() {
            self.mark_dirty();
        }
    }

    #[inline]
    pub fn process_key_event(
        &mut self,
        key: &rio_window::event::KeyEvent,
        clipboard: &mut Clipboard,
    ) {
        if self.context_manager.current().ime.preedit().is_some() {
            return;
        }

        let mode = self.get_mode();
        let mods = self.modifiers.state();

        if key.state == ElementState::Released {
            if mode.contains(Mode::VI)
                || self.search_active()
                || self.hint_state.is_active()
            {
                return;
            }
            let text = key.text_with_all_modifiers().unwrap_or_default();
            let alt_is_meta = self.alt_send_esc(key, text);
            if let Some(input) = session_key_input(key, mods, alt_is_meta) {
                let _ = self
                    .ctx_mut()
                    .current_mut()
                    .terminal
                    .lock()
                    .session()
                    .map(|session| session.enqueue(SessionCommand::Key(input)));
            }
            return;
        }

        // All key bindings are disabled while a hint is being selected (like Alacritty)
        if self.hint_state.is_active() {
            // Handle special keys first
            match key.logical_key {
                rio_window::keyboard::Key::Named(
                    rio_window::keyboard::NamedKey::Escape,
                ) => {
                    self.hint_state.stop();
                    self.update_hint_state();
                    self.mark_dirty();
                    return;
                }
                rio_window::keyboard::Key::Named(
                    rio_window::keyboard::NamedKey::Backspace,
                ) => {
                    let terminal = self.context_manager.current().terminal.lock();
                    self.hint_state.keyboard_input(&*terminal, '\x08');
                    drop(terminal);
                    self.update_hint_state();
                    self.mark_dirty();
                    return;
                }
                rio_window::keyboard::Key::Named(
                    rio_window::keyboard::NamedKey::PageUp,
                ) => {
                    self.scroll_page(true);
                    return;
                }
                rio_window::keyboard::Key::Named(
                    rio_window::keyboard::NamedKey::PageDown,
                ) => {
                    self.scroll_page(false);
                    return;
                }
                _ => {}
            }

            // Handle text input
            let text = key.text_with_all_modifiers().unwrap_or_default();
            for character in text.chars() {
                let terminal = self.context_manager.current().terminal.lock();
                if let Some((hint_match, paste)) =
                    self.hint_state.keyboard_input(&*terminal, character)
                {
                    drop(terminal);
                    self.execute_hint_action(&hint_match, clipboard, paste);
                    self.update_hint_state();
                    self.mark_dirty();
                    return;
                }
                drop(terminal);
            }
            self.update_hint_state();
            self.mark_dirty();
            return;
        }

        if self.process_key_bindings(key, &mode, mods, clipboard) {
            return;
        }

        let text = key.text_with_all_modifiers().unwrap_or_default();

        if self.search_active() {
            for character in text.chars() {
                self.search_input(character);
            }

            self.mark_dirty();
            return;
        }

        // Vi mode on its own doesn't have any input, the search input was done before.
        if mode.contains(Mode::VI) {
            return;
        }

        let alt_is_meta = self.alt_send_esc(key, text);
        if let Some(input) = session_key_input(key, mods, alt_is_meta) {
            self.scroll_bottom_when_cursor_not_visible();
            self.clear_selection();
            let _ = self
                .ctx_mut()
                .current_mut()
                .terminal
                .lock()
                .session()
                .map(|session| session.enqueue(SessionCommand::Key(input)));
        }
    }

    #[inline]
    pub fn process_mouse_bindings(
        &mut self,
        button: MouseButton,
        clipboard: &mut Clipboard,
    ) {
        let mode = self.get_mode();
        let binding_mode = BindingMode::new(&mode, self.search_active());
        let mouse_mode = self.mouse_mode();
        let mods = self.modifiers.state();

        for i in 0..self.mouse_bindings.len() {
            let mut binding = self.mouse_bindings[i].clone();

            // Require shift for all modifiers when mouse mode is active.
            if mouse_mode {
                binding.mods |= ModifiersState::SHIFT;
            }

            if binding.is_triggered_by(binding_mode.to_owned(), mods, &button)
                && binding.action == Act::PasteSelection
            {
                let content = clipboard.get(ClipboardType::Selection);
                self.paste(&content, true);
            }
        }
    }

    pub fn process_key_bindings(
        &mut self,
        key: &rio_window::event::KeyEvent,
        mode: &Mode,
        mods: ModifiersState,
        clipboard: &mut Clipboard,
    ) -> bool {
        let search_active = self.search_active();
        let binding_mode = BindingMode::new(mode, search_active);
        let mut ignore_chars = None;

        for i in 0..self.bindings.len() {
            let binding = &self.bindings[i];
            let trigger = &binding.trigger;
            let action = binding.action.clone();

            // We don't want the key without modifier, because it means something else most of
            // the time. However what we want is to manually lowercase the character to account
            // for both small and capital letters on regular characters at the same time.
            let logical_key = if let Key::Character(ch) = key.logical_key.as_ref() {
                // Match `Alt` bindings without `Alt` being applied, otherwise they use the
                // composed chars, which are not intuitive to bind.
                //
                // On Windows, the `Ctrl + Alt` mangles `logical_key` to unidentified values, thus
                // preventing them from being used in bindings
                //
                // For more see https://github.com/rust-windowing/winit/issues/2945.
                // if (cfg!(target_os = "macos") || (cfg!(windows) && mods.control_key()))
                // && mods.alt_key()
                if (mods.shift_key() || mods.alt_key())
                    || mods.alt_key() && (cfg!(windows) && mods.control_key())
                {
                    key.key_without_modifiers()
                } else {
                    Key::Character(ch.to_lowercase().into())
                }
            } else {
                key.logical_key.clone()
            };

            let key_match = match (&trigger, logical_key) {
                (BindingKey::Scancode(_), _) => BindingKey::Scancode(key.physical_key),
                (_, code) => BindingKey::Keycode {
                    key: code,
                    location: key.location,
                },
            };

            if binding.is_triggered_by(binding_mode.to_owned(), mods, &key_match) {
                *ignore_chars.get_or_insert(true) &= action != Act::ReceiveChar;

                match &action {
                    Act::Run(program) => self.exec(program.program(), program.args()),
                    Act::Esc(s) => {
                        self.paste(s, false);
                    }
                    Act::Paste => {
                        let content = clipboard.get(ClipboardType::Clipboard);
                        self.paste(&content, true);
                    }
                    Act::ClearSelection => {
                        self.clear_selection();
                    }
                    Act::PasteSelection => {
                        let content = clipboard.get(ClipboardType::Selection);
                        self.paste(&content, true);
                    }
                    Act::Copy => {
                        self.yank_selection(clipboard);
                    }
                    Act::SelectAll => {
                        self.select_all();
                    }
                    Act::Hint(hint_config) => {
                        self.start_hint_mode(hint_config.clone());
                    }
                    Act::SearchForward => {
                        self.start_search(Direction::Right);
                        self.mark_dirty();
                    }
                    Act::SearchBackward => {
                        self.start_search(Direction::Left);
                        self.mark_dirty();
                    }
                    Act::Search(SearchAction::SearchConfirm) => {
                        self.confirm_search(clipboard);
                        self.mark_dirty();
                    }
                    Act::Search(SearchAction::SearchCancel) => {
                        self.cancel_search(clipboard);
                        self.mark_dirty();
                    }
                    Act::Search(SearchAction::SearchClear) => {
                        let direction = self.search_state.direction;
                        self.cancel_search(clipboard);
                        self.start_search(direction);
                        self.mark_dirty();
                    }
                    Act::Search(SearchAction::SearchFocusNext) => {
                        self.advance_search_origin(self.search_state.direction);
                        self.mark_dirty();
                    }
                    Act::Search(SearchAction::SearchFocusPrevious) => {
                        let direction = self.search_state.direction.opposite();
                        self.advance_search_origin(direction);
                        self.mark_dirty();
                    }
                    Act::Search(SearchAction::SearchDeleteWord) => {
                        self.search_pop_word();
                        self.mark_dirty();
                    }
                    Act::Search(SearchAction::SearchHistoryPrevious) => {
                        self.search_history_previous();
                        self.mark_dirty();
                    }
                    Act::Search(SearchAction::SearchHistoryNext) => {
                        self.search_history_next();
                        self.mark_dirty();
                    }
                    Act::ToggleViMode => {
                        self.toggle_vi_mode();
                    }
                    Act::ViMotion(motion) => {
                        self.context_manager
                            .current_mut()
                            .terminal
                            .lock()
                            .vi_motion(*motion);
                        self.refresh_selection_range();
                        self.mark_dirty();
                    }
                    Act::Vi(ViAction::CenterAroundViCursor) => {
                        let scroll_lines = {
                            let terminal = self.context_manager.current().terminal.lock();
                            let target = -(terminal.display_offset() as i32)
                                + terminal.screen_lines() as i32 / 2
                                - 1;
                            target - terminal.vi_cursor_position().row.0
                        };
                        self.context_manager
                            .current_mut()
                            .terminal
                            .lock()
                            .scroll_display(Scroll::Delta(scroll_lines));
                        self.refresh_selection_range();
                        self.refresh_hints_after_scroll();
                        self.mark_dirty();
                    }
                    Act::Vi(ViAction::ToggleNormalSelection) => {
                        self.toggle_selection(
                            SelectionType::Simple,
                            Side::Left,
                            clipboard,
                        );
                        self.context_manager
                            .current_mut()
                            .renderable_content
                            .pending_update
                            .set_terminal_damage(
                                rio_backend::event::TerminalDamage::Full,
                            );
                        self.mark_dirty();
                    }
                    Act::Vi(ViAction::ToggleLineSelection) => {
                        self.toggle_selection(
                            SelectionType::Lines,
                            Side::Left,
                            clipboard,
                        );
                        self.context_manager
                            .current_mut()
                            .renderable_content
                            .pending_update
                            .set_terminal_damage(
                                rio_backend::event::TerminalDamage::Full,
                            );
                        self.mark_dirty();
                    }
                    Act::Vi(ViAction::ToggleBlockSelection) => {
                        self.toggle_selection(
                            SelectionType::Block,
                            Side::Left,
                            clipboard,
                        );
                        self.context_manager
                            .current_mut()
                            .renderable_content
                            .pending_update
                            .set_terminal_damage(
                                rio_backend::event::TerminalDamage::Full,
                            );
                        self.mark_dirty();
                    }
                    Act::Vi(ViAction::ToggleSemanticSelection) => {
                        self.toggle_selection(
                            SelectionType::Semantic,
                            Side::Left,
                            clipboard,
                        );
                        self.context_manager
                            .current_mut()
                            .renderable_content
                            .pending_update
                            .set_terminal_damage(
                                rio_backend::event::TerminalDamage::Full,
                            );
                        self.mark_dirty();
                    }
                    Act::SplitRight => {
                        self.split_right();
                    }
                    Act::SplitDown => {
                        self.split_down();
                    }
                    Act::MoveDividerUp => {
                        // User wants divider to move up visually, which means expanding the bottom split
                        self.move_divider_down();
                    }
                    Act::MoveDividerDown => {
                        // User wants divider to move down visually, which means expanding the top split
                        self.move_divider_up();
                    }
                    Act::MoveDividerLeft => {
                        self.move_divider_left();
                    }
                    Act::MoveDividerRight => {
                        self.move_divider_right();
                    }
                    Act::ConfigEditor => {
                        self.context_manager.switch_to_settings();
                    }
                    Act::WindowCreateNew => {
                        self.context_manager.create_new_window();
                    }
                    Act::MoveCurrentTabToNewWindow => {
                        self.context_manager.move_current_tab_to_new_window();
                    }
                    Act::MergeWindow => {
                        self.context_manager.merge_window();
                    }
                    Act::ToggleQuake => {
                        self.context_manager.toggle_quake();
                    }
                    Act::CloseCurrentSplitOrTab => {
                        self.close_split_or_tab(clipboard);
                    }
                    Act::TabCreateNew => {
                        self.create_tab(clipboard);
                    }
                    Act::TabCloseCurrent => {
                        self.close_tab(clipboard);
                    }
                    Act::TabCloseUnfocused => {
                        self.clear_selection();
                        self.cancel_search(clipboard);
                        if self.ctx().len() <= 1 {
                            return true;
                        }
                        let removed = self
                            .context_manager
                            .close_unfocused_tabs(&mut self.sugarloaf);
                        self.discard_routes(removed);
                        if let Some(ref mut island) = self.renderer.island {
                            island.dismiss_color_picker();
                        }
                        self.refresh_current_layout();
                    }
                    Act::Quit => {
                        self.context_manager.quit();
                    }
                    Act::IncreaseFontSize => {
                        self.change_font_size(FontSizeAction::Increase);
                    }
                    Act::DecreaseFontSize => {
                        self.change_font_size(FontSizeAction::Decrease);
                    }
                    Act::ResetFontSize => {
                        self.change_font_size(FontSizeAction::Reset);
                    }
                    Act::ScrollToPrevPrompt => {
                        let rtid = self.ctx().current().rich_text_id;
                        self.context_manager
                            .current_mut()
                            .terminal
                            .lock()
                            .scroll_to_prompt(false);
                        self.refresh_hints_after_scroll();
                        self.renderer.scrollbar.notify_scroll(rtid);
                        self.mark_dirty();
                    }
                    Act::ScrollToNextPrompt => {
                        let rtid = self.ctx().current().rich_text_id;
                        self.context_manager
                            .current_mut()
                            .terminal
                            .lock()
                            .scroll_to_prompt(true);
                        self.refresh_hints_after_scroll();
                        self.renderer.scrollbar.notify_scroll(rtid);
                        self.mark_dirty();
                    }
                    Act::ScrollPageUp => {
                        self.scroll_page(true);
                    }
                    Act::ScrollPageDown => {
                        self.scroll_page(false);
                    }
                    Act::ScrollHalfPageUp => {
                        self.scroll_half_page(true);
                    }
                    Act::ScrollHalfPageDown => {
                        self.scroll_half_page(false);
                    }
                    Act::ScrollToTop => {
                        let rtid = self.ctx().current().rich_text_id;
                        {
                            let mut terminal =
                                self.context_manager.current_mut().terminal.lock();
                            terminal.scroll_display(Scroll::Top);
                            let top = Pos::new(terminal.topmost_line(), Column(0));
                            terminal.vi_goto_pos(top);
                            terminal.vi_motion(ViMotion::FirstOccupied);
                        }
                        self.refresh_selection_range();
                        self.refresh_hints_after_scroll();
                        self.renderer.scrollbar.notify_scroll(rtid);
                        self.mark_dirty();
                    }
                    Act::ScrollToBottom => {
                        let rtid = self.ctx().current().rich_text_id;
                        {
                            let mut terminal =
                                self.context_manager.current_mut().terminal.lock();
                            terminal.scroll_display(Scroll::Bottom);
                            let bottom = Pos::new(
                                terminal.bottommost_line(),
                                terminal.last_column(),
                            );
                            terminal.vi_goto_pos(bottom);
                            terminal.vi_motion(ViMotion::FirstOccupied);
                            terminal.vi_motion(ViMotion::FirstOccupied);
                        }
                        self.refresh_selection_range();
                        self.refresh_hints_after_scroll();
                        self.renderer.scrollbar.notify_scroll(rtid);
                        self.mark_dirty();
                    }
                    Act::Scroll(delta) => {
                        let rtid = self.ctx().current().rich_text_id;
                        self.context_manager
                            .current_mut()
                            .terminal
                            .lock()
                            .scroll_display(Scroll::Delta(*delta));
                        self.refresh_selection_range();
                        self.refresh_hints_after_scroll();
                        self.renderer.scrollbar.notify_scroll(rtid);
                        self.mark_dirty();
                    }
                    Act::ClearHistory => {
                        self.context_manager
                            .current_mut()
                            .terminal
                            .lock()
                            .clear_saved_history();
                        self.mark_dirty();
                    }
                    Act::ToggleFullscreen => self.context_manager.toggle_full_screen(),
                    Act::ToggleAppearanceTheme => {
                        self.context_manager.toggle_appearance_theme();
                    }
                    Act::OpenCommandPalette => {
                        // One-way "open": the action never closes an
                        // already-visible palette. Users close it via
                        // Esc (handled inside the palette's own key
                        // dispatcher in `router::mod`). Idempotent —
                        // re-firing while the palette is already open
                        // must NOT wipe the user's in-progress query.
                        if !self.renderer.command_palette.is_enabled() {
                            self.renderer.command_palette.set_enabled(true);
                            self.mark_dirty();
                        }
                    }
                    Act::Minimize => {
                        self.context_manager.minimize();
                    }
                    Act::Hide => {
                        self.context_manager.hide();
                    }
                    #[cfg(target_os = "macos")]
                    Act::HideOtherApplications => {
                        self.context_manager.hide_other_apps();
                    }
                    Act::SelectNextSplit => {
                        self.cancel_search(clipboard);
                        self.context_manager.select_next_split();
                        self.mark_dirty();
                    }
                    Act::SelectPrevSplit => {
                        self.cancel_search(clipboard);
                        self.context_manager.select_prev_split();
                        self.mark_dirty();
                    }
                    Act::SelectNextSplitOrTab => {
                        self.cancel_search(clipboard);
                        self.clear_selection();
                        let old_index = self.context_manager.current_index();
                        self.context_manager.switch_to_next_split_or_tab();
                        self.context_manager
                            .clear_context_overlays(&mut self.sugarloaf, old_index);
                        self.mark_dirty();
                    }
                    Act::SelectPrevSplitOrTab => {
                        self.cancel_search(clipboard);
                        self.clear_selection();
                        let old_index = self.context_manager.current_index();
                        self.context_manager.switch_to_prev_split_or_tab();
                        self.context_manager
                            .clear_context_overlays(&mut self.sugarloaf, old_index);
                        self.mark_dirty();
                    }
                    Act::SelectTab(tab_index) => {
                        let old_index = self.context_manager.current_index();
                        self.context_manager.select_tab(*tab_index);
                        self.context_manager
                            .clear_context_overlays(&mut self.sugarloaf, old_index);
                        self.cancel_search(clipboard);
                        self.mark_dirty();
                    }
                    Act::SelectLastTab => {
                        self.cancel_search(clipboard);
                        let old_index = self.context_manager.current_index();
                        self.context_manager.select_last_tab();
                        self.context_manager
                            .clear_context_overlays(&mut self.sugarloaf, old_index);
                        self.mark_dirty();
                    }
                    Act::SelectNextTab => {
                        self.cancel_search(clipboard);
                        self.clear_selection();
                        let old_index = self.context_manager.current_index();
                        self.context_manager.switch_to_next();
                        self.context_manager
                            .clear_context_overlays(&mut self.sugarloaf, old_index);
                        self.mark_dirty();
                    }
                    Act::MoveCurrentTabToPrev => {
                        self.cancel_search(clipboard);
                        self.clear_selection();
                        let old_index = self.context_manager.current_index();
                        self.context_manager.move_current_to_prev();
                        let new_index = self.context_manager.current_index();
                        self.context_manager
                            .clear_context_overlays(&mut self.sugarloaf, old_index);
                        let tab_width =
                            self.island_tab_layout(self.context_manager.len()).tab_width;
                        if let Some(ref mut island) = self.renderer.island {
                            island.remap_tab_swap(old_index, new_index, tab_width);
                        }
                        self.mark_dirty();
                    }
                    Act::MoveCurrentTabToNext => {
                        self.cancel_search(clipboard);
                        self.clear_selection();
                        let old_index = self.context_manager.current_index();
                        self.context_manager.move_current_to_next();
                        let new_index = self.context_manager.current_index();
                        self.context_manager
                            .clear_context_overlays(&mut self.sugarloaf, old_index);
                        let tab_width =
                            self.island_tab_layout(self.context_manager.len()).tab_width;
                        if let Some(ref mut island) = self.renderer.island {
                            island.remap_tab_swap(old_index, new_index, tab_width);
                        }
                        self.mark_dirty();
                    }
                    Act::SelectPrevTab => {
                        self.cancel_search(clipboard);
                        self.clear_selection();
                        let old_index = self.context_manager.current_index();
                        self.context_manager.switch_to_prev();
                        self.context_manager
                            .clear_context_overlays(&mut self.sugarloaf, old_index);
                        self.mark_dirty();
                    }
                    Act::ReceiveChar | Act::None => (),
                    _ => (),
                }
            }
        }

        // Hint activation always consumes its key, even with an overlapping `ReceiveChar`.
        ignore_chars.unwrap_or(false) || self.hint_state.is_active()
    }

    pub fn split_right_with_config(&mut self, config: rio_backend::config::Config) {
        let rich_text_id = next_rich_text_id();
        self.context_manager
            .split_from_config(rich_text_id, false, config);

        self.mark_dirty();
    }

    pub fn split_right(&mut self) {
        let rich_text_id = next_rich_text_id();
        self.context_manager.split(rich_text_id, false);

        self.mark_dirty();
    }

    pub fn split_down(&mut self) {
        let rich_text_id = next_rich_text_id();
        self.context_manager.split(rich_text_id, true);

        self.mark_dirty();
    }

    pub fn move_divider_up(&mut self) {
        let amount = 20.0; // Default movement amount
        if self.context_manager.move_divider_up(amount) {
            self.mark_dirty();
            // Divider displacement is layout, not cursor travel.
            self.renderer.trail_cursor.snap();
        }
    }

    pub fn move_divider_down(&mut self) {
        let amount = 20.0; // Default movement amount
        if self.context_manager.move_divider_down(amount) {
            self.mark_dirty();
            // Divider displacement is layout, not cursor travel.
            self.renderer.trail_cursor.snap();
        }
    }

    pub fn move_divider_left(&mut self) {
        let amount = 40.0; // Default movement amount
        if self.context_manager.move_divider_left(amount) {
            self.mark_dirty();
            // Divider displacement is layout, not cursor travel.
            self.renderer.trail_cursor.snap();
        }
    }

    pub fn move_divider_right(&mut self) {
        let amount = 40.0; // Default movement amount
        if self.context_manager.move_divider_right(amount) {
            self.mark_dirty();
            // Divider displacement is layout, not cursor travel.
            self.renderer.trail_cursor.snap();
        }
    }

    pub fn create_tab(&mut self, clipboard: &mut Clipboard) {
        let redirect = true;

        let old_index = self.context_manager.current_index();

        let rich_text_id = next_rich_text_id();
        self.context_manager.add_context(redirect, rich_text_id);
        self.context_manager
            .clear_context_overlays(&mut self.sugarloaf, old_index);

        self.cancel_search(clipboard);
        self.refresh_current_layout();
    }

    pub fn close_split_or_tab(&mut self, clipboard: &mut Clipboard) {
        if self.context_manager.current_grid().len() > 1 {
            self.clear_selection();
            self.discard_routes([self.context_manager.current().route_id]);
            self.context_manager
                .remove_current_grid(&mut self.sugarloaf);
            self.mark_dirty();
        } else {
            self.close_tab(clipboard);
        }
    }

    pub fn close_tab(&mut self, clipboard: &mut Clipboard) {
        self.clear_selection();
        let had_multiple_tabs = self.context_manager.len() > 1;
        if had_multiple_tabs {
            let route_ids = self.context_manager.current_grid().route_ids();
            self.discard_routes(route_ids);
        }
        self.context_manager
            .close_current_context(&mut self.sugarloaf);
        if let Some(ref mut island) = self.renderer.island {
            island.dismiss_color_picker();
        }

        self.cancel_search(clipboard);
        if had_multiple_tabs {
            self.refresh_current_layout();
        } else {
            self.mark_dirty();
        }
    }

    fn refresh_current_layout(&mut self) {
        let size = self.sugarloaf.window_size();
        self.refresh_after_tab_transfer(rio_window::dpi::PhysicalSize::new(
            size.width.round() as u32,
            size.height.round() as u32,
        ));
    }

    #[inline]
    fn search_pop_word(&mut self) {
        if let Some(regex) = self.search_state.regex_mut() {
            *regex = regex.trim_end().to_owned();
            regex.truncate(regex.rfind(' ').map_or(0, |i| i + 1));
            self.update_search();
        }
    }

    /// Go to the previous regex in the search history.
    #[inline]
    fn search_history_previous(&mut self) {
        let index = match &mut self.search_state.history_index {
            None => return,
            Some(index) if *index + 1 >= self.search_state.history.len() => return,
            Some(index) => index,
        };

        *index += 1;
        self.update_search();
    }

    /// Go to the previous regex in the search history.
    #[inline]
    fn search_history_next(&mut self) {
        let index = match &mut self.search_state.history_index {
            Some(0) | None => return,
            Some(index) => index,
        };

        *index -= 1;
        self.update_search();
    }

    #[inline]
    fn advance_search_origin(&mut self, direction: Direction) {
        let remote_search = self.ctx().current().terminal.lock().session().is_some();
        if remote_search {
            let focused_match = self.search_state.focused_match.clone();
            let search_active = {
                let terminal = self.context_manager.current().terminal.lock();
                terminal.is_search_active()
            };
            if let Some(focused_match) = focused_match {
                let terminal = self.context_manager.current().terminal.lock();
                self.search_state.origin = match direction {
                    Direction::Right => {
                        focused_match.end().add(&*terminal, Boundary::None, 1)
                    }
                    Direction::Left => {
                        focused_match.start().sub(&*terminal, Boundary::None, 1)
                    }
                };
            }
            if search_active {
                self.context_manager
                    .current_mut()
                    .terminal
                    .lock()
                    .cancel_search();
            }
            self.search_state.display_offset_delta = 0;
            self.search_state.direction = direction;
            self.goto_match(None);
            return;
        }

        // Use focused match as new search origin if available.
        if let Some(focused_match) = &self.search_state.focused_match {
            let mut terminal = self.context_manager.current_mut().terminal.lock();
            let new_origin = match direction {
                Direction::Right => {
                    focused_match.end().add(&*terminal, Boundary::None, 1)
                }
                Direction::Left => {
                    focused_match.start().sub(&*terminal, Boundary::None, 1)
                }
            };

            terminal.scroll_to_pos(new_origin);
            drop(terminal);

            self.search_state.display_offset_delta = 0;
            self.search_state.origin = new_origin;
        }

        // Search for the next match using the supplied direction.
        let search_direction =
            std::mem::replace(&mut self.search_state.direction, direction);
        self.goto_match(None);
        self.search_state.direction = search_direction;

        // If we found a match, we set the search origin right in front of it to make sure that
        // after modifications to the regex the search is started without moving the focused match
        // around.
        let focused_match = match &self.search_state.focused_match {
            Some(focused_match) => focused_match,
            None => return,
        };

        // Set new origin to the left/right of the match, depending on search direction.
        let new_origin = match self.search_state.direction {
            Direction::Right => *focused_match.start(),
            Direction::Left => *focused_match.end(),
        };

        let mut terminal = self.context_manager.current_mut().terminal.lock();

        // Store the search origin with display offset by checking how far we need to scroll to it.
        let old_display_offset = terminal.display_offset() as i32;
        terminal.scroll_to_pos(new_origin);
        let new_display_offset = terminal.display_offset() as i32;
        self.search_state.display_offset_delta = new_display_offset - old_display_offset;

        // Store origin and scroll back to the match.
        terminal.scroll_display(Scroll::Delta(-self.search_state.display_offset_delta));
        drop(terminal);
        self.search_state.origin = new_origin;
        self.refresh_hints_after_scroll();
    }

    /// Whether we should send `ESC` due to `Alt` being pressed.
    fn alt_send_esc(&mut self, key: &rio_window::event::KeyEvent, text: &str) -> bool {
        #[cfg(not(target_os = "macos"))]
        let alt_send_esc = self.modifiers.state().alt_key();

        #[cfg(target_os = "macos")]
        let alt_send_esc = {
            let option_as_alt = &self.renderer.option_as_alt;
            self.modifiers.state().alt_key()
                && (option_as_alt == "both"
                    || (option_as_alt == "left"
                        && self.modifiers.lalt_state() == ModifiersKeyState::Pressed)
                    || (option_as_alt == "right"
                        && self.modifiers.ralt_state() == ModifiersKeyState::Pressed))
        };

        match key.logical_key {
            Key::Named(named) => {
                if named.to_text().is_some() {
                    alt_send_esc
                } else {
                    // Treat `Alt` as modifier for named keys without text, like ArrowUp.
                    self.modifiers.state().alt_key()
                }
            }
            _ => alt_send_esc && text.chars().count() == 1,
        }
    }

    pub fn copy_selection(
        &mut self,
        ty: ClipboardType,
        clipboard: &mut Clipboard,
    ) -> bool {
        let _ = clipboard;
        self.context_manager
            .current_mut()
            .terminal
            .lock()
            .request_selection_text(ty);
        false
    }

    fn yank_selection(&mut self, clipboard: &mut Clipboard) {
        let vi_mode = self.get_mode().contains(Mode::VI);
        self.copy_selection(ClipboardType::Clipboard, clipboard);
        if vi_mode {
            self.set_vi_mode(false);
        }
    }

    fn toggle_vi_mode(&mut self) {
        let vi_mode_enabled = self
            .context_manager
            .current()
            .terminal
            .lock()
            .mode()
            .contains(Mode::VI);
        self.set_vi_mode(!vi_mode_enabled);
    }

    fn set_vi_mode(&mut self, enabled: bool) {
        {
            let context = self.context_manager.current_mut();
            context.terminal.lock().set_vi_mode(enabled);
            context
                .renderable_content
                .pending_update
                .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
        }
        self.renderer.set_vi_mode(enabled);
        self.mark_dirty();
    }

    pub fn copy_selection_on_pointer_release(
        &mut self,
        copy_on_select: bool,
        clipboard: &mut Clipboard,
    ) {
        self.context_manager
            .current_mut()
            .terminal
            .lock()
            .request_selection_text_with_copy(ClipboardType::Selection, copy_on_select);
        let _ = clipboard;
    }

    #[inline]
    pub fn select_all(&mut self) {
        {
            let current = self.context_manager.current_mut();
            current.terminal.lock().select_all();
        }
        self.context_manager.request_render();
    }

    #[inline]
    pub fn clear_selection(&mut self) {
        // Clear the selection on the terminal.
        self.context_manager
            .current_mut()
            .terminal
            .lock()
            .clear_selection();
    }

    #[inline]
    fn start_selection(
        &mut self,
        ty: SelectionType,
        point: Pos,
        side: Side,
        clipboard: &mut Clipboard,
    ) {
        self.copy_selection(ClipboardType::Selection, clipboard);
        {
            let current = self.context_manager.current_mut();
            current.terminal.lock().selection_begin(ty, point, side);
        }

        // Request render to ensure it shows immediately
        self.context_manager.request_render();
    }

    #[inline]
    fn toggle_selection(
        &mut self,
        ty: SelectionType,
        side: Side,
        clipboard: &mut Clipboard,
    ) {
        let _ = clipboard;
        let current = self.context_manager.current_mut();
        let terminal = current.terminal.lock();
        if terminal.selection_range.is_some() {
            drop(terminal);
            self.clear_selection();
        } else {
            let point = terminal.vi_cursor_position();
            drop(terminal);
            self.start_selection(ty, point, side, clipboard);
        }
    }

    #[inline]
    pub fn update_selection(&mut self, mut pos: Pos, side: Side) {
        let is_search_active = self.search_active();
        {
            let current = self.context_manager.current_mut();
            let mut terminal = current.terminal.lock();
            pos.row = std::cmp::min(pos.row, terminal.bottommost_line());
            if terminal.selection_range.is_none() {
                return;
            }
            if terminal.mode().contains(Mode::VI) && !is_search_active {
                terminal.vi_goto_pos(pos);
            }
            terminal.selection_update(pos, side);
        }

        // Request render to ensure it shows immediately
        self.context_manager.request_render();
    }

    #[inline]
    /// Update hint highlighting based on mouse position and modifiers
    pub fn update_highlighted_hints(&mut self) -> bool {
        // Check if any hint configuration has matching modifiers
        let should_highlight = self.contains_point(self.mouse.x, self.mouse.y)
            && self.hints_config.iter().any(|hint_config| {
                hint_config.mouse.enabled && self.modifiers_match(&hint_config.mouse.mods)
            });

        let had_highlight = self
            .context_manager
            .current()
            .renderable_content
            .highlighted_hint
            .is_some();

        if !should_highlight {
            return self.clear_highlighted_hint();
        }

        let mods = self.modifiers.state();

        // Mouse events arrive per pixel; when the last probe of this
        // viewport cell with these modifiers found nothing, there is
        // nothing new to learn until one of them changes. The cell is
        // pure geometry, so an unchanged probe skips without even
        // taking the terminal lock. While a highlight is shown the
        // probe always reruns, so text changing under the underline
        // still refreshes it. Wheel scrolling resets the probe in
        // `Self::scroll`; content sliding under a stationary cursor
        // without one is stale until the mouse crosses a cell.
        let viewport_point = self.mouse_position(0);
        if !had_highlight && self.last_hint_probe == Some((viewport_point, mods)) {
            return false;
        }

        let terminal = self.context_manager.current().terminal.lock();
        let display_offset = terminal.display_offset();
        let mouse_point =
            Pos::new(viewport_point.row - display_offset, viewport_point.col);

        // Find hint at mouse position
        let highlighted_hint = self.find_hint_at_point(&*terminal, mouse_point);
        drop(terminal);
        self.last_hint_probe = Some((viewport_point, mods));

        let current = self.context_manager.current_mut();

        if let Some(hint_match) = highlighted_hint {
            // Reprobes run on every mouse event while a highlight is
            // shown (so text changing under it refreshes); when the
            // match is the same one already displayed there is nothing
            // to redraw, and re-marking full damage per pixel would
            // rebuild the grid for the whole hover.
            let unchanged = current
                .renderable_content
                .highlighted_hint
                .as_ref()
                .is_some_and(|shown| {
                    shown.start == hint_match.start
                        && shown.end == hint_match.end
                        && shown.text == hint_match.text
                });
            if unchanged {
                return false;
            }

            current
                .renderable_content
                .pending_update
                .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
            current.renderable_content.highlighted_hint = Some(hint_match);
            true
        } else {
            // Force a render so the previously-highlighted line clears.
            if had_highlight {
                current
                    .renderable_content
                    .pending_update
                    .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
            }
            current.renderable_content.highlighted_hint = None;
            had_highlight
        }
    }

    /// Drop any hint highlight, clearing its damage so the line
    /// repaints. Returns whether a highlight existed. Also forgets the
    /// last probed cell: clears run on context switches, where a stale
    /// probe could suppress the first probe of the new panel.
    pub fn clear_highlighted_hint(&mut self) -> bool {
        self.last_hint_probe = None;
        let current = self.context_manager.current_mut();
        let had_highlight = current.renderable_content.highlighted_hint.is_some();

        current.renderable_content.highlighted_hint = None;
        had_highlight
    }

    /// Check if current modifiers match the required modifiers
    fn modifiers_match(&self, required_mods: &[String]) -> bool {
        if required_mods.is_empty() {
            return true;
        }

        let current_mods = self.modifiers.state();

        for required_mod in required_mods {
            let matches = match required_mod.as_str() {
                "Shift" => current_mods.shift_key(),
                "Control" | "Ctrl" => current_mods.control_key(),
                "Alt" => current_mods.alt_key(),
                "Super" | "Cmd" | "Command" => current_mods.super_key(),
                _ => false,
            };

            if !matches {
                return false;
            }
        }

        true
    }

    /// Find hint at the specified point
    fn find_hint_at_point<H: crate::hints::HintGrid>(
        &self,
        terminal: &H,
        point: rio_backend::crosswords::pos::Pos,
    ) -> Option<crate::hints::HintMatch> {
        // Prefer OSC targets even when a regex from an earlier configuration
        // also matches the cell.
        for hint_config in &self.hints_config {
            if !hint_config.mouse.enabled
                || !hint_config.hyperlinks
                || !self.modifiers_match(&hint_config.mouse.mods)
            {
                continue;
            }

            if let Some(hyperlink_match) =
                self.find_hyperlink_at_point(terminal, point, hint_config.clone())
            {
                return Some(hyperlink_match);
            }
        }

        let mut logical_line: Option<Option<crate::hints::LogicalLine>> = None;
        for hint_config in &self.hints_config {
            if !hint_config.mouse.enabled
                || !self.modifiers_match(&hint_config.mouse.mods)
            {
                continue;
            }
            if let Some(regex_pattern) = &hint_config.regex {
                if let Some(regex) = self.compiled_hint_regex(regex_pattern) {
                    let line = logical_line.get_or_insert_with(|| {
                        crate::hints::LogicalLine::extract(terminal, point)
                    });
                    if let Some(m) = line.as_ref().and_then(|line| {
                        line.match_at(
                            terminal,
                            point,
                            &regex,
                            hint_config.post_processing,
                        )
                    }) {
                        return Some(crate::hints::HintMatch {
                            text: m.text,
                            start: m.start,
                            end: m.end,
                            hint: hint_config.clone(),
                        });
                    }
                }
            }
        }

        None
    }

    /// Find hyperlink at the specified point
    fn find_hyperlink_at_point<H: crate::hints::HintGrid>(
        &self,
        terminal: &H,
        point: rio_backend::crosswords::pos::Pos,
        hint_config: std::rc::Rc<rio_backend::config::hints::Hint>,
    ) -> Option<crate::hints::HintMatch> {
        // Check if the point is within grid bounds
        if point.row.0 < -(terminal.history_size() as i32)
            || point.row > terminal.bottommost_line()
            || point.col.0 >= terminal.columns()
        {
            return None;
        }

        // Look up the cell's hyperlink via the per-grid extras table.
        // Cells in the same OSC 8 span share an `extras_id`, so we
        // walk left/right comparing the hyperlink itself (extras slots
        // are interned by content, so a cell with combining marks has
        // a different id while belonging to the same link) to find the
        // span boundaries.
        let hyperlink = terminal.cell_hyperlink(point)?;

        let mut start_col = point.col;
        let mut end_col = point.col;

        while start_col > rio_backend::crosswords::pos::Column(0) {
            let prev_col = start_col - 1;
            if terminal
                .cell_hyperlink(Pos::new(point.row, prev_col))
                .as_ref()
                == Some(&hyperlink)
            {
                start_col = prev_col;
            } else {
                break;
            }
        }
        while end_col < terminal.columns() - 1 {
            let next_col = end_col + 1;
            if terminal
                .cell_hyperlink(Pos::new(point.row, next_col))
                .as_ref()
                == Some(&hyperlink)
            {
                end_col = next_col;
            } else {
                break;
            }
        }

        let mut uri = hyperlink.uri().to_string();
        if hint_config.post_processing {
            uri = crate::hints::post_process_hyperlink_uri(&uri);
        }
        if uri.is_empty() {
            return None;
        }

        Some(crate::hints::HintMatch {
            text: uri,
            start: rio_backend::crosswords::pos::Pos::new(point.row, start_col),
            end: rio_backend::crosswords::pos::Pos::new(point.row, end_col),
            hint: hint_config,
        })
    }

    /// Compiled regex for a hint pattern, from the cache when possible.
    /// A pattern that fails to compile is cached as absent implicitly:
    /// the failed compile repeats, but invalid patterns are a config
    /// error and rare.
    fn compiled_hint_regex(&self, pattern: &str) -> Option<std::rc::Rc<onig::Regex>> {
        if let Some(regex) = self.hint_regex_cache.borrow().get(pattern) {
            return Some(regex.clone());
        }
        let regex = std::rc::Rc::new(onig::Regex::new(pattern).ok()?);
        self.hint_regex_cache
            .borrow_mut()
            .insert(pattern.to_string(), regex.clone());
        Some(regex)
    }

    /// Whether a hint (regex match or OSC 8 link) is currently highlighted
    /// under the mouse. Only ever true while the hint's mods are held, so
    /// it doubles as "the user is following a link right now".
    #[inline]
    pub fn has_highlighted_hint(&self) -> bool {
        self.highlighted_hint().is_some()
    }

    pub fn highlighted_hint(&self) -> Option<&crate::hints::HintMatch> {
        self.context_manager
            .current()
            .renderable_content
            .highlighted_hint
            .as_ref()
    }

    /// Cursor icon for the current mouse position: a pointer over a
    /// highlighted hint, otherwise the icon the terminal mode calls for.
    #[inline]
    pub fn mouse_cursor_icon(&self) -> CursorIcon {
        if self.has_highlighted_hint() {
            CursorIcon::Pointer
        } else if !self.modifiers.state().shift_key() && self.mouse_mode() {
            CursorIcon::Default
        } else {
            CursorIcon::Text
        }
    }

    /// Execute a hint latched at press time. The latched match is the
    /// payload, not the release-time highlight: the modifier can
    /// change mid-click and swap which hint config the same span
    /// resolves to, and the action that runs must be the one the
    /// press landed on.
    #[inline]
    pub fn open_latched_hint(
        &mut self,
        latched: crate::hints::HintMatch,
        clipboard: &mut Clipboard,
    ) {
        // Clear with damage recorded: an action that steals no focus
        // (Copy) would otherwise leave the underline painted until
        // unrelated output touches those rows.
        self.clear_highlighted_hint();
        self.execute_hint_action(&latched, clipboard, false);
    }

    /// Hand `target` to the platform's default handler.
    ///
    /// `target` comes from terminal output, so it is attacker-controlled and
    /// must never reach a shell: `cmd /c start` would treat `&` in a URL as a
    /// command separator, and on Unix a launcher gets it as a single argv
    /// entry rather than a command line.
    fn open_with_default_handler(&self, target: &str) {
        #[cfg(not(any(target_os = "macos", windows)))]
        self.exec("xdg-open", [target]);

        #[cfg(target_os = "macos")]
        self.exec("open", [target]);

        #[cfg(windows)]
        shell_execute_open(target);
    }

    pub fn exec<I, S>(&self, program: &str, args: I)
    where
        I: IntoIterator<Item = S> + Debug + Copy,
        S: AsRef<OsStr>,
    {
        let cwd = self.ctx().current().foreground_process_path();
        match teletypewriter::spawn_daemon(program, args, cwd.as_deref()) {
            Ok(_) => tracing::debug!("Launched {} with args {:?}", program, args),
            Err(_) => {
                tracing::warn!("Unable to launch {} with args {:?}", program, args)
            }
        }
    }

    #[inline]
    /// Compute the selection scroll delta for the given mouse Y position.
    /// Returns 0 if the mouse is within the viewport, ±1 at the edges.
    /// `mouse_y` is in physical pixels (from CursorMoved position.y).
    pub fn selection_scroll_delta(&self, mouse_y: f64) -> i32 {
        let current_grid = self.context_manager.current_grid();
        let (context, margin) = current_grid.current_context_with_computed_dimension();
        let layout = context.dimension;
        // Canonical integer cell stride. line_height is already
        // baked into `cell.cell_height`; the previous code
        // multiplied by line_height again, breaking the
        // edge-of-viewport detection at line_height ≠ 1.0.
        let cell_height = layout.cell.cell_height as f64;
        let text_area_top = margin.top as f64;
        let text_area_bottom = text_area_top + layout.lines as f64 * cell_height;
        let window_height = self.sugarloaf.window_size().height as f64;

        if mouse_y < text_area_top {
            1 // scroll up (into history)
        } else if mouse_y >= window_height - cell_height && mouse_y >= text_area_bottom {
            -1 // scroll down (toward present)
        } else {
            0
        }
    }

    /// Perform one tick of selection auto-scroll.
    /// Reads mouse.raw_y to compute scroll direction.
    /// Scrolls 1 line per tick.
    pub fn selection_scroll_tick(&mut self) {
        if self.mouse.left_button_state != rio_window::event::ElementState::Pressed {
            return;
        }

        let delta = self.selection_scroll_delta(self.mouse.raw_y);
        if delta == 0 {
            return;
        }

        // The worker owns the scroll and selection update as one operation.
        let point = self.mouse_position(0);
        let side = self.mouse.square_side;
        self.context_manager
            .current_mut()
            .terminal
            .lock()
            .selection_autoscroll(delta, point, side);
        self.refresh_hints_after_scroll();
    }

    #[inline]
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        let current_grid = self.context_manager.current_grid();
        let (context, margin) = current_grid.current_context_with_computed_dimension();
        let layout = context.dimension;
        // Canonical integer stride — same as the GPU paints with.
        // line_height is already baked into `cell.cell_height`; do
        // NOT multiply again here.
        let cell_w = layout.cell.cell_width as f64;
        let cell_h = layout.cell.cell_height as f64;
        let left = margin.left as f64;
        let top = margin.top as f64;
        x > left
            && x <= left + layout.columns as f64 * cell_w
            && y > top
            && y <= top + layout.lines as f64 * cell_h
    }

    #[inline]
    pub fn side_by_pos(&self, x: f64) -> Side {
        let current_grid = self.context_manager.current_grid();
        let (_, margin) = current_grid.current_context_with_computed_dimension();
        let current_context = self.context_manager.current();
        let layout = current_context.dimension;

        crate::mouse::calculate_side_by_pos(
            x,
            margin.left,
            layout.cell.cell_width,
            layout.width,
        )
    }

    #[inline]
    pub fn selection_is_empty(&self) -> bool {
        self.context_manager
            .current()
            .renderable_content
            .selection_range
            .is_none()
    }

    pub(crate) fn execute_palette_selection(&mut self, clipboard: &mut Clipboard) {
        use crate::renderer::command_palette::PaletteAction;

        if let Some(font) = self.renderer.command_palette.get_selected_font() {
            clipboard.set(ClipboardType::Clipboard, font);
            self.renderer.command_palette.set_enabled(false);
            return;
        }
        if let Some(target) = self.renderer.command_palette.get_selected_recovery_target()
        {
            self.renderer.command_palette.set_enabled(false);
            self.select_recovery_target(target);
            self.context_manager.merge_window();
            return;
        }

        match self.renderer.command_palette.get_selected_action() {
            Some(PaletteAction::ListFonts) => {
                let fonts = self.sugarloaf.font_family_names();
                self.renderer.command_palette.enter_fonts_mode(fonts);
            }
            Some(action) => {
                self.renderer.command_palette.set_enabled(false);
                self.execute_palette_action(action, clipboard);
            }
            None => self.renderer.command_palette.set_enabled(false),
        }
    }

    // return true if the click was handled by the island
    #[inline]
    pub fn handle_palette_click(&mut self, clipboard: &mut Clipboard) -> bool {
        if !self.renderer.command_palette.is_enabled() {
            return false;
        }
        let scale_factor = self.sugarloaf.scale_factor();
        let window_width = self.sugarloaf.window_size().width;
        let mouse_x = self.mouse.x as f32 / scale_factor;
        let mouse_y = self.mouse.y as f32 / scale_factor;

        match self.renderer.command_palette.hit_test(
            mouse_x,
            mouse_y,
            window_width,
            scale_factor,
        ) {
            Ok(Some(index)) => {
                // Clicked a result row — select and execute
                self.renderer.command_palette.selected_index = index;
                self.execute_palette_selection(clipboard);
                self.mark_dirty();
                true
            }
            Ok(None) => {
                // Clicked inside palette but not on a result (e.g. input area)
                true
            }
            Err(()) => {
                // Clicked outside — close palette
                self.renderer.command_palette.set_enabled(false);
                self.mark_dirty();
                true
            }
        }
    }

    #[inline]
    pub fn handle_search_click(&mut self, clipboard: &mut Clipboard) -> bool {
        if !self.renderer.search.is_active() {
            return false;
        }

        let scale_factor = self.sugarloaf.scale_factor();
        let window_width = self.sugarloaf.window_size().width;
        let mouse_x = self.mouse.x as f32 / scale_factor;
        let mouse_y = self.mouse.y as f32 / scale_factor;

        match self
            .renderer
            .search
            .hit_test(mouse_x, mouse_y, window_width, scale_factor)
        {
            Ok(Some(action)) => {
                use crate::renderer::search::SearchOverlayAction;
                match action {
                    SearchOverlayAction::Next => {
                        self.advance_search_origin(self.search_state.direction);
                    }
                    SearchOverlayAction::Previous => {
                        let direction = self.search_state.direction.opposite();
                        self.advance_search_origin(direction);
                    }
                    SearchOverlayAction::Close => {
                        self.cancel_search(clipboard);
                    }
                }
                self.mark_dirty();
                true
            }
            Ok(None) => {
                // Clicked inside overlay but not on a button (input area)
                true
            }
            Err(()) => {
                // Clicked outside — don't close search, just pass through
                false
            }
        }
    }

    #[inline]
    pub fn handle_assistant_click(&mut self) -> bool {
        if !self.renderer.assistant.is_active() {
            return false;
        }

        let scale_factor = self.sugarloaf.scale_factor();
        let window_width = self.sugarloaf.window_size().width;
        let mouse_x = self.mouse.x as f32 / scale_factor;
        let mouse_y = self.mouse.y as f32 / scale_factor;

        match self.renderer.assistant.hit_test(
            mouse_x,
            mouse_y,
            window_width,
            scale_factor,
        ) {
            Ok(Some(action)) => {
                use crate::renderer::assistant::AssistantOverlayAction;
                match action {
                    AssistantOverlayAction::Close => {
                        self.renderer.assistant.clear();
                    }
                    AssistantOverlayAction::OpenDocs => {
                        Self::open_docs_url();
                    }
                }
                self.mark_dirty();
                true
            }
            Ok(None) => {
                // Clicked inside overlay but not on a button
                true
            }
            Err(()) => {
                // Clicked outside — close the assistant overlay
                self.renderer.assistant.clear();
                self.mark_dirty();
                true
            }
        }
    }

    fn open_docs_url() {
        let url = "https://rioterm.com/docs/config";
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(url).spawn();
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
        }
        #[cfg(windows)]
        shell_execute_open(url);
    }

    pub fn handle_scrollbar_click(&mut self) -> bool {
        let scale_factor = self.sugarloaf.scale_factor();
        let mouse_x = self.mouse.x as f32 / scale_factor;
        let mouse_y = self.mouse.y as f32 / scale_factor;

        let grid = self.context_manager.current_grid_mut();
        let grid_margin = (grid.scaled_margin.left, grid.scaled_margin.top);

        let item = match grid.current_item() {
            Some(item) => item,
            None => return false,
        };

        let panel_rect = item.layout_rect;
        let rich_text_id = item.context().rich_text_id;

        let terminal = item.context().terminal.lock();
        let display_offset = terminal.display_offset();
        let history_size = terminal.history_size();
        let screen_lines = terminal.screen_lines();
        drop(terminal);

        if let Some((grab_offset, geom)) = self.renderer.scrollbar.hit_test(
            mouse_x,
            mouse_y,
            panel_rect,
            scale_factor,
            display_offset,
            history_size,
            screen_lines,
            grid_margin,
        ) {
            self.renderer.scrollbar.start_drag(
                rich_text_id,
                grab_offset,
                &geom,
                history_size,
            );

            // If clicked on track (not on thumb), jump-scroll to that position
            if grab_offset.is_none() {
                if let Some(new_offset) = self.renderer.scrollbar.drag_update(mouse_y) {
                    let mut terminal = self.context_manager.current_mut().terminal.lock();
                    let current = terminal.display_offset();
                    let delta = new_offset as i32 - current as i32;
                    if delta != 0 {
                        terminal.scroll_display(Scroll::Delta(delta));
                    }
                    drop(terminal);
                    if delta != 0 {
                        self.refresh_selection_range();
                        self.refresh_hints_after_scroll();
                    }
                }
            }
            self.mark_dirty();
            true
        } else {
            false
        }
    }

    pub fn handle_scrollbar_drag(&mut self, mouse_y: f32) -> bool {
        if !self.renderer.scrollbar.is_dragging() {
            return false;
        }

        if let Some(new_offset) = self.renderer.scrollbar.drag_update(mouse_y) {
            let mut terminal = self.context_manager.current_mut().terminal.lock();
            let current = terminal.display_offset();
            let delta = new_offset as i32 - current as i32;
            if delta != 0 {
                terminal.scroll_display(Scroll::Delta(delta));
            }
            drop(terminal);
            if delta != 0 {
                self.refresh_selection_range();
                self.refresh_hints_after_scroll();
            }
            self.mark_dirty();
        }
        true
    }

    pub fn handle_scrollbar_release(&mut self) {
        self.renderer.scrollbar.end_drag();
    }

    pub fn is_hovering_scrollbar(&self) -> bool {
        if !self.renderer.scrollbar.is_enabled() {
            return false;
        }
        let scale_factor = self.sugarloaf.scale_factor();
        let mouse_x = self.mouse.x as f32 / scale_factor;
        let mouse_y = self.mouse.y as f32 / scale_factor;

        let grid = self.context_manager.current_grid();
        let grid_margin = (grid.scaled_margin.left, grid.scaled_margin.top);

        let item = match grid.current_item() {
            Some(item) => item,
            None => return false,
        };

        let panel_rect = item.layout_rect;

        let terminal = item.context().terminal.lock();
        let display_offset = terminal.display_offset();
        let history_size = terminal.history_size();
        let screen_lines = terminal.screen_lines();
        drop(terminal);

        self.renderer
            .scrollbar
            .hit_test(
                mouse_x,
                mouse_y,
                panel_rect,
                scale_factor,
                display_offset,
                history_size,
                screen_lines,
                grid_margin,
            )
            .is_some()
    }

    #[inline]
    fn island_tab_layout(&self, num_tabs: usize) -> TabStripLayout {
        island::tab_strip_layout(
            self.sugarloaf.window_size().width,
            self.sugarloaf.scale_factor(),
            num_tabs,
            self.renderer.navigation.tab_max_width,
        )
    }

    #[cfg(target_os = "macos")]
    pub fn start_window_drag(&mut self, window: &rio_window::window::Window) {
        self.mouse.left_button_state = ElementState::Released;
        let _ = window.drag_window();
    }

    fn on_chrome_press(
        &mut self,
        window: &rio_window::window::Window,
        prev: Option<ChromePress>,
    ) -> bool {
        let window_origin = window.outer_position().ok();
        let double = matches!(self.mouse.click_state, ClickState::DoubleClick)
            && prev.is_some_and(|p| p.validates_double_click(window_origin));
        if double {
            let is_maximized = window.is_maximized();
            window.set_maximized(!is_maximized);
            return true;
        }

        self.last_chrome_press = Some(ChromePress {
            window_origin,
            at: std::time::Instant::now(),
        });
        #[cfg(target_os = "macos")]
        if self.allow_manual_dragging {
            self.start_window_drag(window);
        }
        false
    }

    #[cfg_attr(
        not(all(feature = "wayland", target_os = "linux")),
        allow(unused_variables)
    )]
    fn handle_chrome_press(
        &mut self,
        window: &rio_window::window::Window,
        prev: Option<ChromePress>,
        x_unscaled: f32,
        layout: &TabStripLayout,
    ) {
        #[cfg(all(feature = "wayland", target_os = "linux"))]
        if self.on_chrome_press(window, prev) {
            return;
        }

        #[cfg(not(all(feature = "wayland", target_os = "linux")))]
        self.on_chrome_press(window, prev);

        #[cfg(all(feature = "wayland", target_os = "linux"))]
        self.start_wayland_window_drag(window, x_unscaled, layout);
    }

    #[inline]
    pub fn take_chrome_press(&mut self) -> Option<ChromePress> {
        self.last_chrome_press.take()
    }

    fn is_close_press_tail(&self, x_unscaled: f32) -> bool {
        const CLOSE_TAIL_SLOP: f32 = 16.0;
        self.last_close_press.is_some_and(|(at, press_x)| {
            at.elapsed() <= crate::constants::MULTI_CLICK_THRESHOLD
                && (x_unscaled - press_x).abs() <= CLOSE_TAIL_SLOP
        })
    }

    fn apply_close_hover(&mut self, hover: bool) -> bool {
        let changed = self
            .renderer
            .island
            .as_mut()
            .is_some_and(|island| island.set_close_hover(hover));
        if changed {
            self.mark_dirty();
        }
        changed
    }

    pub fn update_close_button_hover(&mut self, mouse_x: f64, mouse_y: f64) -> bool {
        let num_tabs = self.context_manager.len();
        let scale_factor = self.sugarloaf.scale_factor();

        let hovering = num_tabs > 1
            && self.renderer.navigation.island_visible(num_tabs)
            && mouse_y <= (self.renderer.navigation.tab_bar_height * scale_factor) as f64
            && island::close_button_hit(
                &self.island_tab_layout(num_tabs),
                self.context_manager.current_index(),
                mouse_x as f32 / scale_factor,
                &self.renderer.navigation,
            );

        self.apply_close_hover(hovering)
    }

    #[inline]
    pub fn clear_close_button_hover(&mut self) -> bool {
        self.apply_close_hover(false)
    }

    pub fn tab_bar_contains_y(&self, y: f64) -> bool {
        self.renderer
            .navigation
            .chrome_band_reserved(self.context_manager.len())
            && y <= (self.renderer.navigation.tab_bar_height
                * self.sugarloaf.scale_factor()) as f64
    }

    /// Return an external tab insertion slot for a physical client point.
    pub fn tab_drop_index(&self, x: f64, y: f64) -> Option<usize> {
        if !self.renderer.navigation.is_enabled() {
            return None;
        }
        let num_tabs = self.context_manager.len();
        island::tab_drop_index(
            &self.island_tab_layout(num_tabs),
            num_tabs,
            x,
            y,
            self.sugarloaf.scale_factor(),
            self.renderer.navigation.tab_bar_height,
        )
    }

    /// Show an external tab insertion marker for this target window.
    pub fn set_tab_drop_marker(&mut self, index: usize) -> bool {
        assert!(
            index <= self.context_manager.len(),
            "tab drop index exceeds tab count"
        );
        let changed = self
            .renderer
            .island
            .as_mut()
            .is_some_and(|island| island.set_drop_marker(index));
        if changed {
            self.mark_dirty();
        }
        changed
    }

    /// Mark the tab whose contents are being transferred.
    pub fn set_transfer_source_marker(&mut self, index: usize) -> bool {
        assert!(
            index < self.context_manager.len(),
            "transfer source index exceeds tab count"
        );
        let changed = self
            .renderer
            .island
            .as_mut()
            .is_some_and(|island| island.set_transfer_source(index));
        if changed {
            self.mark_dirty();
        }
        changed
    }

    /// Clear this window's transfer source marker.
    pub fn clear_transfer_source_marker(&mut self) -> bool {
        let changed = self
            .renderer
            .island
            .as_mut()
            .is_some_and(island::Island::clear_transfer_source);
        if changed {
            self.mark_dirty();
        }
        changed
    }

    /// Clear this window's external tab insertion marker.
    pub fn clear_tab_drop_marker(&mut self) -> bool {
        let changed = self
            .renderer
            .island
            .as_mut()
            .is_some_and(island::Island::clear_drop_marker);
        if changed {
            self.mark_dirty();
        }
        changed
    }

    pub fn set_window_overlay(
        &mut self,
        overlay: Option<crate::renderer::WindowOverlay>,
    ) -> bool {
        let changed = self.renderer.set_window_overlay(overlay);
        if changed {
            self.mark_dirty();
        }
        changed
    }

    pub fn handle_island_click(
        &mut self,
        window: &rio_window::window::Window,
        clipboard: &mut Clipboard,
        is_right_click: bool,
        chrome_press: Option<ChromePress>,
    ) -> bool {
        #[cfg(all(feature = "wayland", target_os = "linux"))]
        use rio_window::platform::wayland::WindowExtWayland;

        // Only handle if navigation is enabled
        if !self.renderer.navigation.is_enabled() {
            return false;
        }

        let mouse_x = self.mouse.x;
        let mouse_y = self.mouse.y;

        let scale_factor = self.sugarloaf.scale_factor();
        let island_height_px =
            (self.renderer.navigation.tab_bar_height * scale_factor) as f64;

        let window_width = self.sugarloaf.window_size().width;
        let num_tabs = self.context_manager.len();
        let island_visible = self.renderer.navigation.island_visible(num_tabs);
        let layout = self.island_tab_layout(num_tabs);

        if let Some(ref mut island) = self.renderer.island {
            if island.is_color_picker_open() {
                let consumed = island.handle_color_picker_click(
                    crate::renderer::island::ColorPickerClick {
                        mouse_x: mouse_x as f32,
                        mouse_y: mouse_y as f32,
                        scale_factor,
                        window_width,
                        num_tabs,
                        navigation: &self.renderer.navigation,
                        context_manager: &mut self.context_manager,
                    },
                );
                if consumed {
                    self.mark_dirty();
                    return true;
                }
            }
        }

        // Check if click is within island height
        if mouse_y > island_height_px {
            // Close picker if clicking outside
            if let Some(ref mut island) = self.renderer.island {
                if island.is_color_picker_open() {
                    island.close_color_picker(&mut self.context_manager);
                    self.mark_dirty();
                }
            }
            return false;
        }

        #[cfg(all(feature = "wayland", target_os = "linux"))]
        {
            let top_chrome_height =
                self.renderer.navigation.tab_inset_y as f64 * scale_factor as f64;
            if !is_right_click && mouse_y < top_chrome_height {
                let double = num_tabs == 1
                    && matches!(self.mouse.click_state, ClickState::DoubleClick)
                    && chrome_press.as_ref().is_some_and(|press| {
                        press.validates_double_click(window.outer_position().ok())
                    });
                if num_tabs == 1 && !double {
                    self.on_chrome_press(window, chrome_press);
                    let tab_id = self
                        .context_manager
                        .tab_id_at(0)
                        .expect("singleton tab must exist");
                    if let Some(island) = self.renderer.island.as_mut() {
                        island.start_drag(
                            tab_id,
                            0,
                            mouse_x as f32 / scale_factor,
                            mouse_x as f32 / scale_factor,
                        );
                    }
                    return true;
                }
                self.handle_chrome_press(
                    window,
                    chrome_press,
                    mouse_x as f32 / scale_factor,
                    &layout,
                );
                return true;
            }
        }

        let mouse_x_unscaled = mouse_x as f32 / scale_factor;

        // Island isn't painted (hide_if_single + single tab). On macOS the
        // terminal still starts below this band, so it remains custom window
        // chrome and must keep the same drag/double-click behavior as a
        // visible island. Other platforms render the terminal from the top
        // when the island is hidden, so their clicks keep falling through.
        if !island_visible {
            if self.is_close_press_tail(mouse_x_unscaled) {
                return true;
            }

            if self.renderer.navigation.chrome_band_reserved(num_tabs) {
                // Same contract as the visible island's chrome regions:
                // left starts a drag / validates a double-click, right
                // is consumed without an action. Letting a right-click
                // fall through would act on the first terminal row
                // while the pointer is over window chrome.
                if !is_right_click {
                    self.on_chrome_press(window, chrome_press);
                }
                return true;
            }

            return false;
        }

        let x_in_tabs = mouse_x_unscaled - layout.left_margin;

        if !is_right_click
            && self.is_close_press_tail(mouse_x_unscaled)
            && !island::close_button_hit(
                &layout,
                self.context_manager.current_index(),
                mouse_x_unscaled,
                &self.renderer.navigation,
            )
        {
            return true;
        }

        // A lone tab is drawn as a title centred across the strip rather than
        // an island in the first slot, so the whole strip belongs to it and a
        // right-click anywhere on it should reach that tab. The left margin
        // (traffic lights on macOS) stays outside either way.
        let past_last_tab = num_tabs > 1 && x_in_tabs >= layout.tabs_width;
        if x_in_tabs < 0.0 || past_last_tab {
            if !is_right_click {
                self.handle_chrome_press(window, chrome_press, mouse_x_unscaled, &layout);
            }
            return true;
        }

        // `.min` guards the float edge where x_in_tabs / tab_width
        // lands exactly on num_tabs despite x_in_tabs < tabs_width.
        let clicked_tab = ((x_in_tabs / layout.tab_width) as usize).min(num_tabs - 1);

        #[cfg(target_os = "macos")]
        if !is_right_click && self.modifiers.state().super_key() {
            if self.allow_manual_dragging {
                self.start_window_drag(window);
            }
            return true;
        }

        // Right-click or Control + left-click → toggle color picker for that tab
        if is_right_click || self.modifiers.state().control_key() {
            // Get current displayed title for the rename input
            let current_title = self
                .context_manager
                .title(clicked_tab)
                .and_then(|t| {
                    if !t.content.is_empty() {
                        Some(t.content.clone())
                    } else {
                        t.extra.as_ref().and_then(|e| {
                            if !e.program.is_empty() {
                                Some(e.program.clone())
                            } else {
                                None
                            }
                        })
                    }
                })
                .unwrap_or_else(|| String::from("~"));
            if let Some(ref mut island) = self.renderer.island {
                island.toggle_color_picker(
                    clicked_tab,
                    &current_title,
                    &mut self.context_manager,
                );
                self.mark_dirty();
            }
            return true;
        }

        if num_tabs == 1 {
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            {
                let double = matches!(self.mouse.click_state, ClickState::DoubleClick)
                    && chrome_press.as_ref().is_some_and(|press| {
                        press.validates_double_click(window.outer_position().ok())
                    });

                if !double && window.supports_toplevel_drag() {
                    if let Some(island) = self.renderer.island.as_mut() {
                        island.start_drag(
                            self.context_manager
                                .tab_id_at(0)
                                .expect("singleton tab must exist"),
                            0,
                            mouse_x_unscaled - layout.left_margin,
                            mouse_x_unscaled,
                        );
                    }
                }
            }
            self.on_chrome_press(window, chrome_press);
            return true;
        }

        if clicked_tab == self.context_manager.current_index()
            && island::close_button_hit(
                &layout,
                clicked_tab,
                mouse_x_unscaled,
                &self.renderer.navigation,
            )
        {
            self.stop_hint_mode_if_active();
            self.last_close_press = Some((std::time::Instant::now(), mouse_x_unscaled));
            self.close_tab(clipboard);
            return true;
        }

        if clicked_tab != self.context_manager.current_index() {
            self.stop_hint_mode_if_active();
            self.cancel_search(clipboard);
            self.clear_selection();
            let old_index = self.context_manager.current_index();
            self.context_manager.set_current(clicked_tab);
            self.context_manager
                .clear_context_overlays(&mut self.sugarloaf, old_index);

            self.mark_dirty();
        }

        if let Some(ref mut island) = self.renderer.island {
            if island.is_color_picker_open() {
                island.close_color_picker(&mut self.context_manager);
                self.mark_dirty();
            }
        }

        #[cfg(target_os = "macos")]
        let can_reorder = self.allow_manual_dragging;
        #[cfg(not(target_os = "macos"))]
        let can_reorder = true;
        if can_reorder {
            if let Some(ref mut island) = self.renderer.island {
                let tab_left = layout.left_margin + clicked_tab as f32 * layout.tab_width;
                island.start_drag(
                    self.context_manager
                        .tab_id_at(clicked_tab)
                        .expect("clicked tab must exist"),
                    clicked_tab,
                    mouse_x_unscaled - tab_left,
                    mouse_x_unscaled,
                );
            }
        }

        true
    }

    pub fn handle_tab_drag_move(&mut self, x_unscaled: f32, external: bool) {
        let num_tabs = self.context_manager.len();

        // A tab closed mid-drag invalidates the armed indices.
        if num_tabs == 0 {
            if let Some(ref mut island) = self.renderer.island {
                island.cancel_drag();
            }
            return;
        }

        let layout = self.island_tab_layout(num_tabs);

        let (drag_idx, center) = match self.renderer.island.as_mut() {
            Some(island) => {
                if !island.update_drag(x_unscaled) {
                    // armed but still below the drag threshold
                    return;
                }
                (island.drag_index(), island.drag_center(&layout))
            }
            None => return,
        };
        let old_index = self.context_manager.current_index();
        if drag_idx.is_some_and(|index| index != old_index) {
            if let Some(ref mut island) = self.renderer.island {
                island.cancel_drag();
            }
            self.mark_dirty();
            return;
        }
        if external {
            self.mark_dirty();
            return;
        }
        let Some(center) = center else { return };
        if drag_idx.is_none() {
            return;
        }

        let target = (((center - layout.left_margin) / layout.tab_width) as usize)
            .min(num_tabs - 1);
        if target != old_index {
            self.context_manager.move_current_tab_to(target);
            let new_index = self.context_manager.current_index();
            self.context_manager
                .clear_context_overlays(&mut self.sugarloaf, old_index);
            if let Some(ref mut island) = self.renderer.island {
                island.remap_tab_move(old_index, new_index, layout.tab_width);
            }
        }
        self.mark_dirty();
    }

    pub fn handle_tab_drag_release(&mut self) -> bool {
        let num_tabs = self.context_manager.len();
        let layout = self.island_tab_layout(num_tabs);

        if let Some(ref mut island) = self.renderer.island {
            let started = island.drag_index().is_some();
            island.end_drag(&layout);
            if started {
                self.mark_dirty();
            }
            return started;
        }
        false
    }

    #[inline]
    pub fn on_left_click(&mut self, point: Pos, clipboard: &mut Clipboard) {
        let side = self.mouse.square_side;

        match self.mouse.click_state {
            ClickState::Click => {
                // If Shift is pressed and there's an existing selection, expand it
                if self.modifiers.state().shift_key() && !self.selection_is_empty() {
                    self.update_selection(point, side);
                } else {
                    self.clear_selection();

                    // Start new empty selection.
                    if self.modifiers.state().control_key() {
                        self.start_selection(
                            SelectionType::Block,
                            point,
                            side,
                            clipboard,
                        );
                    } else {
                        self.start_selection(
                            SelectionType::Simple,
                            point,
                            side,
                            clipboard,
                        );
                    }
                }
            }
            ClickState::DoubleClick => {
                self.start_selection(SelectionType::Semantic, point, side, clipboard);
            }
            ClickState::TripleClick => {
                self.start_selection(SelectionType::Lines, point, side, clipboard);
            }
            ClickState::None => (),
        };

        // Move vi mode cursor to mouse click position.
        {
            let mut terminal = self.context_manager.current_mut().terminal.lock();
            if terminal.mode().contains(Mode::VI) {
                terminal.vi_goto_pos(point);
            }
        }
    }

    #[inline]
    fn start_search(&mut self, direction: Direction) {
        // Only create new history entry if the previous regex wasn't empty.
        if self
            .search_state
            .history
            .front()
            .is_none_or(|regex| !regex.is_empty())
        {
            self.search_state.history.push_front(String::new());
            self.search_state.history.truncate(MAX_SEARCH_HISTORY_SIZE);
        }

        self.search_state.history_index = Some(0);
        self.search_state.direction = direction;
        self.search_state.focused_match = None;

        // Store original search position as origin and reset location.
        if self.get_mode().contains(Mode::VI) {
            let terminal = self.context_manager.current().terminal.lock();
            self.search_state.origin = terminal.vi_cursor_position();
            self.search_state.display_offset_delta = 0;

            // Adjust origin for content moving upward on search start.
            if terminal.cursor().pos.row + 1 == terminal.screen_lines() {
                self.search_state.origin.row -= 1;
            }
        } else {
            let terminal = self.context_manager.current().terminal.lock();
            let viewport_top = Line(-(terminal.display_offset() as i32)) - 1;
            let viewport_bottom = viewport_top + terminal.bottommost_line();
            let last_column = terminal.last_column();
            self.search_state.origin = match direction {
                Direction::Right => Pos::new(viewport_top, Column(0)),
                Direction::Left => Pos::new(viewport_bottom, last_column),
            };
            drop(terminal);
        }

        // Enable IME so we can input into the search bar with it if we were in Vi mode.
        // self.window().set_ime_allowed(true);

        self.mark_dirty();
    }

    #[inline]
    fn confirm_search(&mut self, clipboard: &mut Clipboard) {
        // Just cancel search when not in vi mode.
        if !self.get_mode().contains(Mode::VI) {
            self.cancel_search(clipboard);
            return;
        }

        // Force unlimited search if the previous one was interrupted.
        // let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
        // if self.scheduler.scheduled(timer_id) {
        // self.goto_match(None);
        // }

        self.exit_search();
    }

    #[inline]
    fn cancel_search(&mut self, clipboard: &mut Clipboard) {
        let vi_mode = self.get_mode().contains(Mode::VI);
        let had_match = self.search_state.focused_match.is_some();
        self.context_manager
            .current_mut()
            .terminal
            .lock()
            .cancel_search();
        if !vi_mode && had_match {
            // The worker selects the focused match while cancelling. Request the
            // resulting text after that command in the same bounded queue.
            self.copy_selection(ClipboardType::Selection, clipboard);
        }

        self.search_state.dfas = None;
        self.exit_search();
        self.update_hint_state();
    }

    /// Cleanup the search state.
    fn exit_search(&mut self) {
        // let vi_mode = self.get_mode().contains(Mode::VI);
        // self.window().set_ime_allowed(!vi_mode);

        self.search_state.history_index = None;

        // Clear focused match.
        self.search_state.focused_match = None;

        self.mark_dirty();
    }

    #[inline]
    fn search_input(&mut self, c: char) {
        match self.search_state.history_index {
            Some(0) => (),
            // When currently in history, replace active regex with history on change.
            Some(index) => {
                self.search_state.history[0] = self.search_state.history[index].clone();
                self.search_state.history_index = Some(0);
            }
            None => return,
        }
        let regex = &mut self.search_state.history[0];

        match c {
            // Handle backspace/ctrl+h.
            '\x08' | '\x7f' => {
                let _ = regex.pop();
            }
            // Add ascii and unicode text.
            ' '..='~' | '\u{a0}'..='\u{10ffff}' => regex.push(c),
            // Ignore non-printable characters.
            _ => return,
        }

        let mode = self.get_mode();
        if !mode.contains(Mode::VI) {
            // Clear selection so we do not obstruct any matches.
            self.context_manager
                .current_mut()
                .terminal
                .lock()
                .clear_selection();
        }

        self.update_search();
        self.mark_dirty();
    }

    fn update_search(&mut self) {
        let regex = match self.search_state.regex() {
            Some(regex) => regex,
            None => return,
        };

        if regex.is_empty() {
            // Stop search if there's nothing to search for.
            self.search_reset_state();
        } else {
            let is_remote = self.ctx().current().terminal.lock().session().is_some();
            if is_remote {
                self.context_manager
                    .current_mut()
                    .terminal
                    .lock()
                    .search_matches(regex, 1024);
                self.context_manager
                    .current_mut()
                    .renderable_content
                    .hint_matches = None;
            }

            // Update search highlighting.
            self.goto_match(MAX_SEARCH_WHILE_TYPING);
        }
    }

    /// Reset terminal to the state before search was started.
    fn search_reset_state(&mut self) {
        // Unschedule pending timers.
        // let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
        // self.scheduler.unschedule(timer_id);

        // Clear focused match.
        self.search_state.focused_match = None;

        self.context_manager
            .current_mut()
            .terminal
            .lock()
            .cancel_search();
        self.search_state.display_offset_delta = 0;
        self.refresh_selection_range();
        self.refresh_hints_after_scroll();
    }

    /// Jump to the first regex match from the search origin.
    fn goto_match(&mut self, mut limit: Option<usize>) {
        let pattern = match self.search_state.regex() {
            Some(pattern) if !pattern.is_empty() => pattern.to_owned(),
            _ => return,
        };

        let mut terminal = self.context_manager.current_mut().terminal.lock();
        limit = limit.filter(|&limit| limit <= terminal.total_lines());
        if terminal.is_search_active() {
            terminal.next_search();
        } else {
            terminal.begin_search(
                pattern,
                self.search_state.origin,
                self.search_state.direction,
                Side::Left,
                limit,
            );
        }
    }

    #[inline]
    pub fn has_mouse_motion_and_drag(&mut self) -> bool {
        self.get_mode()
            .intersects(Mode::MOUSE_MOTION | Mode::MOUSE_DRAG)
    }

    #[inline]
    pub fn has_mouse_motion(&mut self) -> bool {
        self.get_mode().intersects(Mode::MOUSE_MOTION)
    }

    #[inline]
    pub fn mouse_report(&mut self, button: u8, state: ElementState) {
        let pos = self.mouse_position(0);
        let modifiers = wire_mouse_modifiers(self.modifiers.state());
        let mut terminal = self.ctx_mut().current_mut().terminal.lock();
        if button >= 32 {
            terminal.mouse_motion(pos, button.saturating_sub(32).min(3), modifiers);
        } else {
            terminal.mouse_button(pos, button, state == ElementState::Pressed, modifiers);
        }
    }

    /// Apply a search-navigation reply published by the session worker.
    /// Replies are asynchronous because the GUI must never wait on the worker
    /// socket while dispatching a native event.
    pub fn sync_session_search(&mut self, route_id: usize) {
        if self.context_manager.current_route() != route_id {
            return;
        }
        let mut terminal = self.context_manager.current_mut().terminal.lock();
        let Some(navigation) = terminal.take_search_navigation() else {
            return;
        };
        let history = terminal.history_size() as i32;
        self.search_state.focused_match = navigation.matched.map(|matched| {
            Pos::new(
                Line(matched.start_line as i32 - history),
                Column(matched.start_column as usize),
            )
                ..=Pos::new(
                    Line(matched.end_line as i32 - history),
                    Column(matched.end_column as usize),
                )
        });
        self.search_state.display_offset_delta = 0;
        drop(terminal);
        self.refresh_selection_range();
        self.refresh_hints_after_scroll();
        self.mark_dirty();
    }

    #[inline]
    pub fn on_focus_change(&mut self, is_focused: bool) {
        self.renderer.is_window_focused = is_focused;
        if is_focused {
            self.mark_dirty();
        }
        if !is_focused {
            let rc = &mut self.context_manager.current_mut().renderable_content;
            if !rc.is_blinking_cursor_visible {
                rc.is_blinking_cursor_visible = true;
            }
            rc.last_blink_toggle = None;
            rc.pending_update
                .set_terminal_damage(rio_backend::event::TerminalDamage::CursorOnly);

            if let Some(ref mut island) = self.renderer.island {
                if island.is_dragging() {
                    island.cancel_drag();
                    self.mark_dirty();
                }
            }
            self.mouse.left_button_state = ElementState::Released;
        }

        self.ctx_mut()
            .current_mut()
            .terminal
            .lock()
            .focus(is_focused);
    }

    #[inline]
    pub fn scroll(&mut self, new_scroll_x_px: f64, new_scroll_y_px: f64) {
        // Scrolling slides different text under the pointer while the
        // viewport cell stays the same, so the hover-probe dedup key
        // must not suppress the next probe.
        self.last_hint_probe = None;

        let dim = self.context_manager.current().dimension.dimension;
        let width = dim.width as f64;
        let height = dim.height as f64;
        let old_display_offset = self.display_offset();
        self.mouse.accumulated_scroll.x +=
            (new_scroll_x_px * self.mouse.multiplier) / self.mouse.divider;
        self.mouse.accumulated_scroll.y +=
            (new_scroll_y_px * self.mouse.multiplier) / self.mouse.divider;
        let lines = (self.mouse.accumulated_scroll.y / height) as i32;
        if lines != 0 {
            let point = self.mouse_position(0);
            self.context_manager
                .current_mut()
                .terminal
                .lock()
                .mouse_wheel(lines, point, wire_mouse_modifiers(self.modifiers.state()));
            self.refresh_selection_range();
            self.renderer
                .scrollbar
                .notify_scroll(self.ctx().current().rich_text_id);
        }
        if old_display_offset != self.display_offset() {
            self.refresh_hints_after_scroll();
        }

        self.mouse.accumulated_scroll.x %= width;
        self.mouse.accumulated_scroll.y %= height;
    }

    #[inline]
    pub fn paste(&mut self, text: &str, bracketed: bool) {
        if self.search_active() {
            for c in text.chars() {
                self.search_input(c);
            }
            return;
        }

        {
            self.scroll_bottom_when_cursor_not_visible();
            self.clear_selection();
            self.ctx_mut()
                .current_mut()
                .terminal
                .lock()
                .paste(text.to_owned(), bracketed);
        }
    }

    pub(crate) fn render_welcome(&mut self) {
        crate::router::routes::welcome::screen(
            &mut self.sugarloaf,
            &self.context_manager.current().dimension,
        );
        self.sugarloaf.render();
    }

    fn execute_palette_action(
        &mut self,
        action: crate::renderer::command_palette::PaletteAction,
        clipboard: &mut Clipboard,
    ) {
        use crate::renderer::command_palette::PaletteAction;
        match action {
            PaletteAction::TabCreate => self.create_tab(clipboard),
            PaletteAction::TabClose => self.close_tab(clipboard),
            PaletteAction::TabCloseUnfocused => {
                if self.ctx().len() > 1 {
                    let removed = self
                        .context_manager
                        .close_unfocused_tabs(&mut self.sugarloaf);
                    self.discard_routes(removed);
                    if let Some(ref mut island) = self.renderer.island {
                        island.dismiss_color_picker();
                    }
                    self.refresh_current_layout();
                }
            }
            PaletteAction::SelectNextTab => {
                self.clear_selection();
                let old = self.context_manager.current_index();
                self.context_manager.switch_to_next();
                self.context_manager
                    .clear_context_overlays(&mut self.sugarloaf, old);
            }
            PaletteAction::SelectPrevTab => {
                self.clear_selection();
                let old = self.context_manager.current_index();
                self.context_manager.switch_to_prev();
                self.context_manager
                    .clear_context_overlays(&mut self.sugarloaf, old);
            }
            PaletteAction::SplitRight => self.split_right(),
            PaletteAction::SplitDown => self.split_down(),
            PaletteAction::SelectNextSplit => {
                self.context_manager.select_next_split();
            }
            PaletteAction::SelectPrevSplit => {
                self.context_manager.select_prev_split();
            }
            PaletteAction::CloseCurrentSplitOrTab => self.close_split_or_tab(clipboard),
            PaletteAction::ConfigEditor => {
                self.context_manager.switch_to_settings();
            }
            PaletteAction::WindowCreateNew => {
                self.context_manager.create_new_window();
            }
            PaletteAction::MoveCurrentTabToNewWindow => {
                self.context_manager.move_current_tab_to_new_window();
            }
            PaletteAction::MergeWindow => {
                self.context_manager.merge_window();
            }
            PaletteAction::RecoverSession => {
                self.recovery_action_requested = true;
                self.context_manager.merge_window();
            }
            PaletteAction::IncreaseFontSize => {
                self.change_font_size(FontSizeAction::Increase);
            }
            PaletteAction::DecreaseFontSize => {
                self.change_font_size(FontSizeAction::Decrease);
            }
            PaletteAction::ResetFontSize => {
                self.change_font_size(FontSizeAction::Reset);
            }
            PaletteAction::ToggleViMode => {
                self.toggle_vi_mode();
            }
            PaletteAction::ToggleFullscreen => {
                self.context_manager.toggle_full_screen();
            }
            PaletteAction::ToggleAppearanceTheme => {
                self.context_manager.toggle_appearance_theme();
            }
            PaletteAction::Copy => {
                self.yank_selection(clipboard);
            }
            PaletteAction::Paste => {
                let content = clipboard.get(ClipboardType::Clipboard);
                self.paste(&content, true);
            }
            PaletteAction::SearchForward => {
                self.start_search(Direction::Right);
            }
            PaletteAction::SearchBackward => {
                self.start_search(Direction::Left);
            }
            PaletteAction::ClearHistory => {
                let mut terminal = self.context_manager.current_mut().terminal.lock();
                terminal.clear_saved_history();
            }
            PaletteAction::ListFonts => {
                // Handled in the router: switches the palette into fonts
                // mode and keeps it open. If we land here it's either a
                // bug (router should have intercepted) or an external
                // caller firing the action directly — do nothing so the
                // palette just closes without side effects.
            }
            PaletteAction::Quit => {
                self.context_manager.quit();
            }
        }
    }

    pub fn begin_recovery_targets(&mut self, targets: Vec<String>) {
        self.renderer
            .command_palette
            .enter_recovery_targets(targets);
        self.mark_dirty();
    }

    pub fn select_recovery_target(&mut self, target: usize) {
        self.recovery_target = Some(target);
    }

    pub fn take_recovery_target(&mut self) -> Option<usize> {
        self.recovery_target.take()
    }

    pub fn take_recovery_action_request(&mut self) -> bool {
        std::mem::take(&mut self.recovery_action_requested)
    }

    pub fn clear_merge_ui(&mut self) {
        self.renderer.command_palette.set_enabled(false);
        self.recovery_target = None;
        self.recovery_action_requested = false;
    }

    #[inline]
    fn ensure_grid(&mut self, route_id: usize, cols: u32, rows: u32) {
        use std::collections::hash_map::Entry;

        match self.grids.entry(route_id) {
            Entry::Occupied(mut entry) => entry.get_mut().resize(cols, rows),
            Entry::Vacant(entry) => {
                entry.insert(rio_backend::sugarloaf::grid::GridRenderer::new(
                    &self.sugarloaf.ctx,
                    cols,
                    rows,
                ));
            }
        }
    }

    pub(crate) fn prepare_session_imports(&mut self) -> Vec<usize> {
        let current = self.context_manager.current_index();
        let grid_count = self.context_manager.contexts_mut().len();
        for index in 0..grid_count {
            self.context_manager.set_current(index);
            self.render_direct_grids(false, true);
        }
        self.context_manager.set_current(current);
        self.take_ready_session_imports()
    }

    fn render_direct_grids(&mut self, should_present: bool, prepare_only: bool) {
        struct PanelFrame {
            route_id: usize,
            layout_rect: [f32; 4],
            cols: u32,
            rows: u32,
            cell_w: f32,
            cell_h: f32,
            font_px: f32,
            render_buffers: crate::context::session::RenderBuffers,
            term_colors: rio_backend::config::colors::term::TermColors,
            cursor_col: u16,
            cursor_row: u16,
            cursor_visible: bool,
            cursor_shape: rio_backend::ansi::CursorShape,
            cursor_blinking: bool,
            cursor_blink_visible: bool,
            cursor_preedit: bool,
            cursor_color: rio_backend::config::colors::ColorArray,
            is_active: bool,
            selection: Option<rio_backend::selection::SelectionRange>,
            history_size: usize,
            display_offset: i32,
            damage: rio_backend::event::TerminalDamage,
            graphics: rio_session::protocol::GraphicsFrame,
            graphics_dirty: bool,
            hint_matches: Option<Vec<rio_backend::crosswords::search::Match>>,
            focused_match: Option<rio_backend::crosswords::search::Match>,
            hovered_hyperlink: Option<(
                rio_backend::crosswords::pos::Pos,
                rio_backend::crosswords::pos::Pos,
            )>,
            hint_labels: Option<Vec<crate::context::renderable::HintLabel>>,
            pending_session: bool,
        }

        let active_route = self.context_manager.current().route_id;
        let focused_match = self.search_state.focused_match.as_ref();
        let mut panels = Vec::new();
        for item in self
            .context_manager
            .current_grid_mut()
            .contexts_mut()
            .values_mut()
        {
            let context = &mut item.val;
            let content = &mut context.renderable_content;
            let mut terminal = context.terminal.lock();
            terminal.refresh_renderable(content);
            let render_buffers = terminal.grid.take_render_buffers();
            let graphics = terminal.take_render_graphics();
            let graphics_dirty = terminal.graphics_dirty();
            panels.push(PanelFrame {
                route_id: context.route_id,
                layout_rect: item.layout_rect,
                cols: content.columns.max(1) as u32,
                rows: content.screen_lines.max(1) as u32,
                cell_w: context.dimension.cell.cell_width as f32,
                cell_h: context.dimension.cell.cell_height as f32,
                font_px: context.dimension.scaled_font_size.max(1.0),
                render_buffers,
                term_colors: content.term_colors,
                cursor_col: content.cursor.state.pos.col.0 as u16,
                cursor_row: content.cursor.state.pos.row.0.max(0) as u16,
                cursor_visible: content.cursor.state.is_visible(),
                cursor_shape: content.cursor.state.content,
                cursor_blinking: content.has_blinking_enabled,
                cursor_blink_visible: !content.has_blinking_enabled
                    || content.is_blinking_cursor_visible,
                cursor_preedit: context.ime.preedit().is_some(),
                cursor_color: content.term_colors
                    [rio_backend::config::colors::NamedColor::Cursor as usize]
                    .unwrap_or(self.renderer.named_colors.cursor),
                is_active: context.route_id == active_route,
                selection: content.selection_range,
                history_size: content.history_size,
                display_offset: content.display_offset as i32,
                damage: std::mem::replace(
                    &mut content.frame_damage,
                    rio_backend::event::TerminalDamage::Noop,
                ),
                graphics,
                graphics_dirty,
                hint_matches: content.hint_matches.clone(),
                focused_match: if context.route_id == active_route {
                    focused_match.cloned()
                } else {
                    None
                },
                hovered_hyperlink: if context.route_id == active_route {
                    content
                        .highlighted_hint
                        .as_ref()
                        .map(|hint| (hint.start, hint.end))
                } else {
                    None
                },
                hint_labels: if context.route_id == active_route {
                    std::mem::take(&mut content.hint_labels)
                } else {
                    None
                },
                pending_session: context.pending_session.is_some(),
            });
        }

        for panel in &panels {
            self.ensure_grid(panel.route_id, panel.cols, panel.rows);
        }

        let scaled_margin = self.context_manager.current_grid().scaled_margin;
        for panel in &mut panels {
            install_frame_graphics(
                &mut self.sugarloaf,
                FrameGraphicsInput {
                    route_id: panel.route_id,
                    frame: &panel.graphics,
                    visible_rows: &panel.render_buffers.rows,
                    styles: &panel.render_buffers.row_styles,
                    extras: &panel.render_buffers.extras,
                    history_size: panel.history_size,
                    display_offset: panel.display_offset,
                    cols: panel.cols,
                    screen_rows: panel.rows,
                    cell_w: panel.cell_w,
                    cell_h: panel.cell_h,
                    origin_x: (scaled_margin.left + panel.layout_rect[0]).round(),
                    origin_y: (scaled_margin.top + panel.layout_rect[1]).round(),
                    update_images: panel.graphics_dirty,
                },
            );
            panel.graphics_dirty = false;
        }

        let window_size = self.sugarloaf.window_size();
        let font_library = self.sugarloaf.font_library().clone();
        let renderer = &self.renderer;
        let background_color = renderer.named_colors.background.0;
        let mut frame_grids = Vec::with_capacity(panels.len());
        let panel_indices: rustc_hash::FxHashMap<usize, usize> = panels
            .iter()
            .enumerate()
            .map(|(index, panel)| (panel.route_id, index))
            .collect();
        for (route_id, grid) in &mut self.grids {
            let Some(&panel_index) = panel_indices.get(route_id) else {
                continue;
            };
            let panel = &mut panels[panel_index];

            let cols = panel.cols as usize;
            let force_full = grid.needs_full_rebuild()
                || matches!(panel.damage, rio_backend::event::TerminalDamage::Full);
            let rebuild_all = force_full;
            let mut bg = Vec::with_capacity(cols);
            let mut fg = Vec::with_capacity(cols);
            let mut hints = Vec::new();
            for row_index in 0..panel.rows as usize {
                let rebuild_row = rebuild_all
                    || panel
                        .render_buffers
                        .rows
                        .get(row_index)
                        .is_some_and(|row| row.dirty);
                if !rebuild_row {
                    continue;
                }
                let Some(row) = panel.render_buffers.rows.get_mut(row_index) else {
                    break;
                };
                hints.clear();
                rio_grid::row_hints_for(
                    panel.hint_matches.as_deref(),
                    panel.focused_match.as_ref(),
                    panel.hovered_hyperlink,
                    row_index,
                    cols,
                    panel.display_offset,
                    &mut hints,
                );
                let styles = panel
                    .render_buffers
                    .row_styles
                    .get(row_index)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let label_styles = rio_grid::hint_label_styles(
                    self.renderer.named_colors.hint_foreground,
                    self.renderer.named_colors.hint_background,
                );
                let hint_labels = panel.hint_labels.as_deref().map(|labels| {
                    labels
                        .iter()
                        .map(|label| rio_grid::HintLabel {
                            position: label.position,
                            label: label.label,
                            is_first: label.is_first,
                        })
                        .collect::<Vec<_>>()
                });
                let mut label_row = None;
                let mut label_styles_owned = None;
                if let Some(labels) = hint_labels.as_deref() {
                    if let Some((overlay_row, overlay_styles)) =
                        rio_grid::overlay_hint_labels(
                            row,
                            styles,
                            labels,
                            row_index,
                            panel.display_offset,
                            label_styles,
                            &mut hints,
                        )
                    {
                        label_row = Some(overlay_row);
                        label_styles_owned = Some(overlay_styles);
                    }
                }
                let row = label_row.as_ref().map_or(&*row, |row| row);
                let styles = label_styles_owned.as_deref().unwrap_or(styles);
                let selection = rio_grid::row_selection_for(
                    panel.selection,
                    row_index,
                    cols,
                    panel.display_offset,
                );
                rio_grid::build_row_bg(
                    row,
                    cols,
                    styles,
                    renderer,
                    &panel.term_colors,
                    selection,
                    &hints,
                    &mut bg,
                );
                rio_grid::build_row_fg(
                    row,
                    cols,
                    row_index as u16,
                    styles,
                    &panel.render_buffers.extras,
                    renderer,
                    &panel.term_colors,
                    &mut self.grid_rasterizer,
                    grid,
                    panel.font_px,
                    panel.cell_w,
                    panel.cell_h,
                    selection,
                    &hints,
                    &font_library,
                    panel.route_id,
                    if panel.cursor_visible && panel.cursor_row == row_index as u16 {
                        Some(panel.cursor_col)
                    } else {
                        None
                    },
                    &mut fg,
                );
                grid.write_row(row_index as u32, &bg, &fg);
                if let Some(row) = panel.render_buffers.rows.get_mut(row_index) {
                    row.dirty = false;
                }
            }
            if force_full {
                grid.mark_full_rebuild_done();
            }

            let render_style =
                rio_grid::cursor_render_style(rio_grid::CursorRenderInputs {
                    visible: panel.cursor_visible,
                    focused: panel.is_active && self.renderer.is_window_focused,
                    blink_visible: panel.cursor_blink_visible,
                    blinking: panel.cursor_blinking,
                    preedit: panel.cursor_preedit,
                    shape: panel.cursor_shape,
                });
            let mut block_cursor = None;
            let mut tail_cursor = None;
            if let Some(style) = render_style {
                let cell_w = panel.cell_w.round().max(1.0) as u32;
                let cell_h = panel.cell_h.round().max(1.0) as u32;
                let color = [
                    (panel.cursor_color[0].clamp(0.0, 1.0) * 255.0) as u8,
                    (panel.cursor_color[1].clamp(0.0, 1.0) * 255.0) as u8,
                    (panel.cursor_color[2].clamp(0.0, 1.0) * 255.0) as u8,
                    255,
                ];
                if let Some((is_block, cell)) = rio_grid::cursor_sprite_cell(
                    grid,
                    style,
                    panel.cursor_col,
                    panel.cursor_row,
                    color,
                    cell_w,
                    cell_h,
                ) {
                    if is_block {
                        block_cursor = Some(cell);
                    } else {
                        tail_cursor = Some(cell);
                    }
                }
            }
            grid.set_cursor(block_cursor.as_slice(), tail_cursor.as_slice());

            let (cursor_pos, cursor_color, cursor_bg_color) =
                if matches!(render_style, Some(rio_grid::CursorRenderStyle::Block)) {
                    (
                        [panel.cursor_col as u32, panel.cursor_row as u32],
                        background_color,
                        panel.cursor_color,
                    )
                } else {
                    ([u32::MAX; 2], [0.0; 4], [0.0; 4])
                };

            let left = (scaled_margin.left + panel.layout_rect[0]).round();
            let top = (scaled_margin.top + panel.layout_rect[1]).round();
            frame_grids.push((
                grid,
                rio_backend::sugarloaf::grid::GridUniforms {
                    projection:
                        rio_backend::sugarloaf::components::core::orthographic_projection(
                            window_size.width,
                            window_size.height,
                        ),
                    grid_padding: [top, 0.0, 0.0, left],
                    cursor_color,
                    cursor_bg_color,
                    cell_size: [panel.cell_w, panel.cell_h],
                    grid_size: [panel.cols, panel.rows],
                    cursor_pos,
                    _pad_cursor: [0; 2],
                    min_contrast: 0.0,
                    flags: 0,
                    padding_extend: 0,
                    input_colorspace: self.sugarloaf.input_colorspace(),
                },
            ));
        }

        let mut frame_dropped = false;
        if should_present || prepare_only {
            if frame_grids.is_empty() {
                self.sugarloaf.render();
            } else {
                self.sugarloaf.render_with_grids(&mut frame_grids);
            }
            frame_dropped = self.sugarloaf.take_frame_dropped();
            if frame_dropped {
                self.context_manager.request_render();
            }
        } else {
            self.sugarloaf.discard_frame();
        }

        if prepare_only && !frame_dropped {
            for panel in &panels {
                if panel.pending_session
                    && !self.ready_session_imports.contains(&panel.route_id)
                {
                    self.ready_session_imports.push(panel.route_id);
                }
            }
        }

        for item in self
            .context_manager
            .current_grid_mut()
            .contexts_mut()
            .values_mut()
        {
            if let Some(index) = panels
                .iter()
                .position(|panel| panel.route_id == item.val.route_id)
            {
                let panel = panels.swap_remove(index);
                let mut terminal = item.val.terminal.lock();
                terminal.grid.restore_render_buffers(panel.render_buffers);
                terminal.restore_render_graphics(panel.graphics);
                terminal.mark_graphics_clean();
            }
        }

        if !prepare_only {
            self.context_manager
                .current_mut()
                .renderable_content
                .pending_update
                .reset();
        }
    }

    pub fn take_ready_session_imports(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.ready_session_imports)
    }

    pub(crate) fn render(&mut self) -> Option<crate::context::renderable::WindowUpdate> {
        self.update_close_button_hover(self.mouse.x, self.mouse.y);

        let is_search_active = self.search_active();
        if is_search_active {
            if let Some(history_index) = self.search_state.history_index {
                self.renderer.set_active_search(
                    self.search_state.history.get(history_index).cloned(),
                );
            }
        } else {
            self.renderer.set_active_search(None);
        }

        if is_search_active {
            let remote_matches = {
                let current = self.context_manager.current_mut();
                current.terminal.lock().take_search_matches()
            };
            if let Some(matches) = remote_matches {
                self.context_manager
                    .current_mut()
                    .renderable_content
                    .hint_matches = Some(matches);
            }

            // Force invalidation for search with full damage
            {
                let current = self.context_manager.current_mut();
                current
                    .renderable_content
                    .pending_update
                    .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
            }
        }

        let (window_update, any_panel_dirty) = self
            .renderer
            .run(&mut self.sugarloaf, &mut self.context_manager);

        if self.renderer.custom_mouse_cursor {
            let scale = self.sugarloaf.scale_factor();
            crate::renderer::custom_cursor::draw(
                &mut self.sugarloaf,
                self.mouse.x as f32,
                self.mouse.y as f32,
                scale,
            );
        }

        if self.renderer.trail_cursor_enabled {
            let current_grid = self.context_manager.current_grid();
            let scaled_margin = current_grid.get_scaled_margin();

            if let Some(current_item) = current_grid.current_item() {
                let layout = current_item.val.dimension;
                // Canonical integer stride — same value the GPU
                // shader uses; line_height is already baked in.
                let cell_width = layout.cell.cell_width as f32;
                let cell_height = layout.cell.cell_height as f32;
                let scale_factor = self.sugarloaf.scale_factor();

                let panel_rect = current_item.layout_rect;
                let origin_x = panel_rect[0] + scaled_margin.left;
                let origin_y = panel_rect[1] + scaled_margin.top;

                let current = self.context_manager.current();
                let cursor = &current.renderable_content.cursor;
                // Vi mode reports the cursor in scroll-adjusted viewport
                // rows; today's clamps keep it non-negative, but a
                // negative Line wrapping through `as usize` would fling
                // the trail target off by ~10^18 px, so clamp first.
                let cursor_row = cursor.state.pos.row.0.max(0) as usize;
                let cursor_col = cursor.state.pos.col.0;

                // Cursor position in physical pixels.
                let cursor_px_x = origin_x + cursor_col as f32 * cell_width;
                let cursor_px_y = origin_y + cursor_row as f32 * cell_height;

                self.renderer.trail_cursor.update(
                    cursor_px_x,
                    cursor_px_y,
                    cell_width,
                    cell_height,
                    cursor.state.content,
                    cursor.state.is_visible(),
                    current.route_id,
                );

                let cursor_color = self.renderer.named_colors.cursor;
                self.renderer.trail_cursor.draw(
                    &mut self.sugarloaf,
                    scale_factor,
                    cursor_color,
                );
            }
        }

        // Animation state is read after the trail advanced: a cursor
        // movement can start animating in this same frame, and reading
        // it earlier would fail to schedule the continuation frame,
        // freezing the trail mid-flight until unrelated damage arrives.
        let has_animation = self.renderer.needs_redraw();
        let should_present = any_panel_dirty || has_animation;

        // Terminal cells are emitted into the window's resident Sugarloaf
        // grids. Session workers remain responsible for parsing and publish
        // immutable frame data only.
        self.render_direct_grids(should_present, false);

        // Mark as dirty if we need continuous rendering (e.g.,
        // indeterminate progress bar, trail cursor animation). UI-only
        // — terminal cells didn't change, but we want the next vsync
        // to fire a render so overlays/animations tick forward.
        if has_animation {
            self.context_manager
                .current_mut()
                .renderable_content
                .pending_update
                .set_dirty();
        }

        if let Some(wake_in) = self.renderer.scrollbar.next_wake_in() {
            self.context_manager
                .schedule_render_on_route(wake_in.as_millis() as u64);
        }

        // In case the configuration of blinking cursor is enabled
        // TODO: enable blinking for selection after adding debounce (https://github.com/raphamorim/rio/issues/437)
        if self.renderer.is_window_focused
            && self.renderer.config_has_blinking_enabled
            && self.selection_is_empty()
            && self
                .context_manager
                .current()
                .renderable_content
                .has_blinking_enabled
        {
            self.context_manager
                .blink_cursor(self.renderer.config_blinking_interval);
        }

        window_update
    }

    /// Update IME cursor position based on terminal cursor position
    /// This should be called after rendering to ensure cursor position is current
    pub fn update_ime_cursor_position_if_needed(
        &mut self,
        window: &rio_window::window::Window,
    ) {
        // Check if IME cursor positioning is enabled in config
        if !self.context_manager.config.keyboard.ime_cursor_positioning {
            return;
        }

        let current_grid = self.context_manager.current_grid();
        let scaled_margin = current_grid.get_scaled_margin();

        let Some(current_item) = current_grid.current_item() else {
            return;
        };

        let layout = current_item.val.dimension;
        let cursor_pos = current_item.val.renderable_content.cursor.state.pos;

        // Calculate pixel position of cursor — canonical integer
        // stride (line_height already baked into cell_height).
        let cell_width = layout.cell.cell_width as f32;
        let cell_height = layout.cell.cell_height as f32;

        // Validate dimensions before calculation
        if cell_width <= 0.0 || cell_height <= 0.0 {
            tracing::warn!(
                "Invalid cell dimensions for IME cursor positioning: {}x{}",
                cell_width,
                cell_height
            );
            return;
        }

        // Panel origin: layout_rect is relative to root container,
        // add scaled_margin to get absolute screen position
        let panel_rect = current_item.layout_rect;
        let origin_x = panel_rect[0] + scaled_margin.left;
        let origin_y = panel_rect[1] + scaled_margin.top;

        // Convert grid position to pixel position
        let pixel_x =
            origin_x + (cursor_pos.col.0 as f32 * cell_width) + (cell_width * 0.5);
        let pixel_y = origin_y + (cursor_pos.row.0 as f32 * cell_height);

        // Validate final coordinates
        if pixel_x.is_nan() || pixel_y.is_nan() || pixel_x < 0.0 || pixel_y < 0.0 {
            tracing::warn!("Invalid IME cursor coordinates: ({}, {})", pixel_x, pixel_y);
            return;
        }

        // Check if position has changed significantly to avoid unnecessary updates
        if let Some((last_x, last_y)) = self.last_ime_cursor_pos {
            if (pixel_x - last_x).abs() < 1.0 && (pixel_y - last_y).abs() < 1.0 {
                return; // Position hasn't changed significantly
            }
        }

        // Update last position
        self.last_ime_cursor_pos = Some((pixel_x, pixel_y));

        // Set IME cursor area
        window.set_ime_cursor_area(
            rio_window::dpi::PhysicalPosition::new(pixel_x as f64, pixel_y as f64),
            rio_window::dpi::PhysicalSize::new(cell_width as f64, cell_height as f64),
        );
    }

    fn stop_hint_mode_if_active(&mut self) {
        if self.hint_state.is_active() {
            self.hint_state.stop();
            self.update_hint_state();
        }
    }

    /// Start hint mode with the given hint configuration
    pub fn start_hint_mode(
        &mut self,
        hint: std::rc::Rc<rio_backend::config::hints::Hint>,
    ) {
        self.hint_state.start(hint);
        let terminal = self.context_manager.current().terminal.lock();
        // Keep hint mode active when this viewport has no matches; the user
        // can scroll to a matching line without the activation key reaching
        // the terminal and forcing the view to the bottom.
        self.hint_state.refresh_matches(&*terminal);
        drop(terminal);

        // Update hint state and trigger damage tracking
        self.update_hint_state();

        self.mark_dirty();
    }

    /// What a hint should hand to a launcher: the match text, or the path it
    /// resolves to against the terminal's OSC 7 CWD when it names one that
    /// exists. URLs and non-existent paths come back unchanged.
    fn hint_open_target(&self, hint_match: &crate::hints::HintMatch) -> String {
        // Cloned so the terminal lock is released before resolving, which
        // goes to the filesystem.
        let cwd = self
            .context_manager
            .current()
            .terminal
            .lock()
            .current_directory
            .clone();
        match crate::hints::resolve_path_for_opening(&hint_match.text, cwd.as_deref()) {
            Some(resolved) => resolved.to_string_lossy().into_owned(),
            None => hint_match.text.clone(),
        }
    }

    /// Execute the action for a selected hint
    fn execute_hint_action(
        &mut self,
        hint_match: &crate::hints::HintMatch,
        clipboard: &mut Clipboard,
        paste: bool,
    ) {
        use rio_backend::config::hints::{HintAction, HintCommand, HintInternalAction};

        match &hint_match.hint.action {
            HintAction::Action { action } => match action {
                HintInternalAction::Copy => {
                    clipboard.set(ClipboardType::Clipboard, hint_match.text.clone());
                    if paste {
                        self.paste(&hint_match.text, true);
                    }
                }
                HintInternalAction::Paste => {
                    self.paste(&hint_match.text, true);
                }
                HintInternalAction::Select => {
                    self.start_selection(
                        SelectionType::Simple,
                        hint_match.start,
                        Side::Left,
                        clipboard,
                    );
                    self.update_selection(hint_match.end, Side::Right);
                    self.mark_dirty();
                }
                HintInternalAction::MoveViModeCursor => {
                    // Move vi mode cursor to hint position.
                    let mut terminal = self.context_manager.current().terminal.lock();
                    terminal.vi_goto_pos(hint_match.start);
                    drop(terminal);
                    self.refresh_selection_range();
                    self.mark_dirty();
                }
                HintInternalAction::Open => {
                    let target = self.hint_open_target(hint_match);
                    self.open_with_default_handler(&target);
                }
            },
            HintAction::Command { command } => {
                let arg_text = self.hint_open_target(hint_match);

                match command {
                    HintCommand::Simple(program) => {
                        self.exec(program, [&arg_text]);
                    }
                    HintCommand::WithArgs { program, args } => {
                        let mut all_args = args.clone();
                        all_args.push(arg_text);
                        self.exec(program, &all_args);
                    }
                }
            }
        }
    }

    /// Update hint state and trigger appropriate damage tracking
    pub fn update_hint_state(&mut self) {
        use rio_backend::event::TerminalDamage;

        if self.hint_state.is_active() {
            // Update hint labels
            self.update_hint_labels();

            // Update hint matches in renderable content
            let matches: Vec<rio_backend::crosswords::search::Match> = self
                .hint_state
                .matches()
                .iter()
                .map(|hint_match| hint_match.start..=hint_match.end)
                .collect();
            self.context_manager
                .current_mut()
                .renderable_content
                .hint_matches = Some(matches);

            // Passive frames are immutable snapshots, so overlay changes use
            // coarse damage rather than mutating terminal row dirty bits.
            self.context_manager
                .current_mut()
                .renderable_content
                .pending_update
                .set_terminal_damage(TerminalDamage::Full);
        } else if !self.search_active() {
            // Clear hint state only if search is not active,
            // since search also uses hint_matches for highlighting
            self.context_manager
                .current_mut()
                .renderable_content
                .hint_matches = None;
            self.context_manager
                .current_mut()
                .renderable_content
                .hint_labels = None;
            // Force full damage to clear all hint highlights
            let current = self.context_manager.current_mut();
            current
                .renderable_content
                .pending_update
                .set_terminal_damage(TerminalDamage::Full);
        }
    }

    fn update_hint_labels(&mut self) {
        use crate::context::renderable::HintLabel;

        let hint_labels = if self.hint_state.is_active() {
            let matches = self.hint_state.matches();
            let visible_labels = self.hint_state.visible_labels();

            let mut labels = Vec::new();
            for (match_index, remaining_label) in visible_labels {
                if let Some(hint_match) = matches.get(match_index) {
                    // Create labels for each character in the hint label
                    for (char_index, &label_char) in remaining_label.iter().enumerate() {
                        let position = rio_backend::crosswords::pos::Pos::new(
                            hint_match.start.row,
                            hint_match.start.col + char_index,
                        );

                        labels.push(HintLabel {
                            position,
                            label: label_char,
                            is_first: char_index == 0, // First character gets different styling
                        });
                    }
                }
            }
            Some(labels)
        } else {
            None
        };

        self.context_manager
            .current_mut()
            .renderable_content
            .hint_labels = hint_labels;
    }
}

struct FrameGraphicsInput<'a> {
    route_id: usize,
    frame: &'a rio_session::protocol::GraphicsFrame,
    visible_rows: &'a [rio_backend::crosswords::grid::row::Row<
        rio_backend::crosswords::square::Square,
    >],
    styles: &'a [Vec<rio_backend::crosswords::style::Style>],
    extras: &'a rustc_hash::FxHashMap<u16, rio_backend::crosswords::square::Extras>,
    history_size: usize,
    display_offset: i32,
    cols: u32,
    screen_rows: u32,
    cell_w: f32,
    cell_h: f32,
    origin_x: f32,
    origin_y: f32,
    update_images: bool,
}

fn install_frame_graphics(sugarloaf: &mut Sugarloaf, input: FrameGraphicsInput<'_>) {
    let FrameGraphicsInput {
        route_id,
        frame,
        visible_rows,
        styles,
        extras,
        history_size,
        display_offset,
        cols,
        screen_rows,
        cell_w,
        cell_h,
        origin_x,
        origin_y,
        update_images,
    } = input;
    use rio_backend::ansi::graphics::{
        atlas_overlay_geometry, clip_overlay_to_rect, kitty_overlay_geometry,
        AtlasPlacement, KittyPlacement, OverlayViewport,
    };
    use rio_backend::sugarloaf::{
        kitty_image_key, ColorType, GraphicData, GraphicDataEntry, GraphicId, GraphicKey,
        GraphicOverlay,
    };
    use std::collections::HashMap;

    sugarloaf.clear_image_overlays_for(route_id);
    if update_images {
        sugarloaf.remove_route_images(route_id);
    }

    let mut dimensions = HashMap::with_capacity(frame.images.len());
    for image in &frame.images {
        let key = GraphicKey::new(route_id, image.key);
        dimensions.insert(key, (image.width, image.height));
        if !update_images && sugarloaf.image_data.contains_key(&key) {
            continue;
        }
        let (pixels, color_type) = match image.color_type {
            0 => {
                let expected = (image.width as usize)
                    .checked_mul(image.height as usize)
                    .and_then(|pixels| pixels.checked_mul(3));
                if expected != Some(image.pixels.len()) {
                    continue;
                }
                let mut pixels = Vec::with_capacity(image.pixels.len() / 3 * 4);
                for pixel in image.pixels.as_chunks::<3>().0 {
                    pixels.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
                }
                (pixels, ColorType::Rgba)
            }
            1 => (image.pixels.clone(), ColorType::Rgba),
            _ => continue,
        };
        sugarloaf.image_data.insert(
            key,
            GraphicDataEntry::from_graphic_data(GraphicData {
                id: GraphicId::new(image.key),
                width: image.width as usize,
                height: image.height as usize,
                color_type,
                pixels,
                is_opaque: image.opacity,
                resize: None,
                display_width: image.display_width.map(|value| value as usize),
                display_height: image.display_height.map(|value| value as usize),
                transmit_time: std::time::Instant::now(),
            }),
        );
    }

    if update_images {
        for key in &frame.removed_keys {
            sugarloaf.remove_image(GraphicKey::new(route_id, *key));
        }
    }

    let viewport = OverlayViewport {
        cell_width: cell_w,
        cell_height: cell_h,
        origin_x,
        origin_y,
        history_size: history_size as i64,
        display_offset: i64::from(display_offset),
        screen_lines: i64::from(screen_rows),
    };
    let clip_x1 = origin_x + cols as f32 * cell_w;
    let clip_y1 = origin_y + screen_rows as f32 * cell_h;
    let mut overlays = Vec::new();

    for placement in &frame.kitty_placements {
        let key = GraphicKey::new(route_id, kitty_image_key(placement.image_id));
        let Some(&(image_width, image_height)) = dimensions.get(&key) else {
            continue;
        };
        let placement = KittyPlacement {
            image_id: placement.image_id,
            placement_id: placement.placement_id,
            source_x: placement.source[0],
            source_y: placement.source[1],
            source_width: placement.source[2],
            source_height: placement.source[3],
            dest_col: placement.dest_col as usize,
            dest_row: placement.dest_row,
            columns: placement.columns,
            rows: placement.rows,
            requested_columns: placement.requested_columns,
            requested_rows: placement.requested_rows,
            pixel_width: 0,
            pixel_height: 0,
            cell_x_offset: placement.cell_offset[0],
            cell_y_offset: placement.cell_offset[1],
            z_index: placement.z_index,
            transmit_time: std::time::Instant::now(),
        };
        let Some(geometry) = kitty_overlay_geometry(
            &placement,
            image_width as usize,
            image_height as usize,
            &viewport,
        ) else {
            continue;
        };
        let mut overlay = GraphicOverlay {
            image_id: key,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            z_index: placement.z_index,
            source_rect: geometry.source_rect,
        };
        if clip_overlay_to_rect(&mut overlay, origin_x, origin_y, clip_x1, clip_y1) {
            overlays.push(overlay);
        }
    }

    for placement in &frame.atlas_placements {
        let key = GraphicKey::new(route_id, placement.key);
        if !dimensions.contains_key(&key) {
            continue;
        }
        let placement = AtlasPlacement {
            image_key: placement.key,
            abs_row: placement.row,
            col: placement.column as usize,
            columns: placement.columns as usize,
            rows: placement.rows as usize,
            src_x: placement.source[0],
            src_y: placement.source[1],
            src_width: placement.source[2],
            src_height: placement.source[3],
            total_width: placement.image_width,
            total_height: placement.image_height,
            insert_cell_w: placement.cell_width as u16,
            insert_cell_h: placement.cell_height as u16,
        };
        let Some(geometry) = atlas_overlay_geometry(&placement, &viewport) else {
            continue;
        };
        let mut overlay = GraphicOverlay {
            image_id: key,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            z_index: -1,
            source_rect: geometry.source_rect,
        };
        if clip_overlay_to_rect(&mut overlay, origin_x, origin_y, clip_x1, clip_y1) {
            overlays.push(overlay);
        }
    }

    append_virtual_graphics(
        visible_rows,
        styles,
        extras,
        &frame.virtual_placements,
        &dimensions,
        &mut overlays,
        route_id,
        origin_x,
        origin_y,
        cell_w,
        cell_h,
        (origin_x, origin_y, clip_x1, clip_y1),
    );

    for overlay in overlays {
        sugarloaf.push_image_overlay(route_id, overlay);
    }
}

#[allow(clippy::too_many_arguments)]
fn append_virtual_graphics(
    rows: &[rio_backend::crosswords::grid::row::Row<
        rio_backend::crosswords::square::Square,
    >],
    styles: &[Vec<rio_backend::crosswords::style::Style>],
    extras: &rustc_hash::FxHashMap<u16, rio_backend::crosswords::square::Extras>,
    placement_frames: &[rio_session::protocol::VirtualPlacementFrame],
    dimensions: &std::collections::HashMap<
        rio_backend::sugarloaf::GraphicKey,
        (u32, u32),
    >,
    overlays: &mut Vec<rio_backend::sugarloaf::GraphicOverlay>,
    route_id: usize,
    origin_x: f32,
    origin_y: f32,
    cell_width: f32,
    cell_height: f32,
    clip: (f32, f32, f32, f32),
) {
    use rio_backend::ansi::graphics::VirtualPlacement;
    use rio_backend::ansi::kitty_virtual::{IncompletePlacement, PLACEHOLDER};
    use std::collections::HashMap;

    let placements: HashMap<(u32, u32), VirtualPlacement> = placement_frames
        .iter()
        .map(|placement| {
            (
                (placement.image_id, placement.placement_id),
                VirtualPlacement {
                    image_id: placement.image_id,
                    placement_id: placement.placement_id,
                    columns: placement.columns,
                    rows: placement.rows,
                    x: placement.source[0],
                    y: placement.source[1],
                    width: placement.source[2],
                    height: placement.source[3],
                    cell_x_offset: placement.cell_offset[0],
                    cell_y_offset: placement.cell_offset[1],
                    z_index: placement.z_index,
                },
            )
        })
        .collect();

    for (line_idx, row) in rows.iter().enumerate() {
        if !row.kitty_virtual_placeholder {
            continue;
        }
        let Some(row_styles) = styles.get(line_idx) else {
            continue;
        };
        let mut run: Option<(IncompletePlacement, usize)> = None;
        for (column, square) in row.inner.iter().enumerate() {
            if square.c() != PLACEHOLDER {
                if let Some((partial, start_column)) = run.take() {
                    flush_virtual_run(
                        overlays,
                        &placements,
                        dimensions,
                        route_id,
                        partial.complete(),
                        line_idx,
                        start_column,
                        origin_x,
                        origin_y,
                        cell_width,
                        cell_height,
                        clip,
                    );
                }
                continue;
            }

            let style = rio_grid::resolve_style(row_styles, column);
            let combining = square
                .extras_id()
                .and_then(|id| extras.get(&id))
                .map(|value| value.zerowidth.as_slice())
                .unwrap_or(&[]);
            let cell = IncompletePlacement::from_cell(
                style.fg,
                style.underline_color,
                combining,
            );
            match &mut run {
                Some((current, _)) if current.can_append(&cell) => current.append(),
                _ => {
                    if let Some((partial, start_column)) = run.take() {
                        flush_virtual_run(
                            overlays,
                            &placements,
                            dimensions,
                            route_id,
                            partial.complete(),
                            line_idx,
                            start_column,
                            origin_x,
                            origin_y,
                            cell_width,
                            cell_height,
                            clip,
                        );
                    }
                    let mut cell = cell;
                    if cell.row.is_none() {
                        cell.row = Some(0);
                    }
                    if cell.col.is_none() {
                        cell.col = Some(0);
                    }
                    run = Some((cell, column));
                }
            }
        }
        if let Some((partial, start_column)) = run {
            flush_virtual_run(
                overlays,
                &placements,
                dimensions,
                route_id,
                partial.complete(),
                line_idx,
                start_column,
                origin_x,
                origin_y,
                cell_width,
                cell_height,
                clip,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_virtual_run(
    overlays: &mut Vec<rio_backend::sugarloaf::GraphicOverlay>,
    placements: &std::collections::HashMap<
        (u32, u32),
        rio_backend::ansi::graphics::VirtualPlacement,
    >,
    dimensions: &std::collections::HashMap<
        rio_backend::sugarloaf::GraphicKey,
        (u32, u32),
    >,
    route_id: usize,
    run: rio_backend::ansi::kitty_virtual::PlaceholderRun,
    screen_line: usize,
    start_screen_column: usize,
    origin_x: f32,
    origin_y: f32,
    cell_width: f32,
    cell_height: f32,
    clip: (f32, f32, f32, f32),
) {
    use rio_backend::ansi::graphics::clip_overlay_to_rect;
    use rio_backend::sugarloaf::{kitty_image_key, GraphicKey, GraphicOverlay};

    let placement = if run.placement_id != 0 {
        placements.get(&(run.image_id, run.placement_id))
    } else {
        placements.get(&(run.image_id, 0)).or_else(|| {
            placements
                .iter()
                .filter(|((image_id, _), _)| *image_id == run.image_id)
                .min_by_key(|((_, placement_id), _)| *placement_id)
                .map(|(_, placement)| placement)
        })
    };
    let Some(placement) = placement else {
        return;
    };
    let image_key = GraphicKey::new(route_id, kitty_image_key(run.image_id));
    let Some(&(image_width, image_height)) = dimensions.get(&image_key) else {
        return;
    };
    let Some(geometry) = rio_backend::ansi::kitty_virtual::compute_run_geometry(
        &run,
        placement.columns,
        placement.rows,
        image_width,
        image_height,
        (placement.x, placement.y, placement.width, placement.height),
        cell_width,
        cell_height,
        origin_x,
        origin_y,
        screen_line,
        start_screen_column,
    ) else {
        return;
    };
    let mut overlay = GraphicOverlay {
        image_id: image_key,
        x: geometry.x + placement.cell_x_offset as f32,
        y: geometry.y + placement.cell_y_offset as f32,
        width: geometry.width,
        height: geometry.height,
        z_index: placement.z_index,
        source_rect: geometry.source_rect,
    };
    if clip_overlay_to_rect(&mut overlay, clip.0, clip.1, clip.2, clip.3) {
        overlays.push(overlay);
    }
}

/// Open `target` with whatever Windows has registered for it, without a
/// shell in the middle. `ShellExecuteW` takes the target as one string
/// rather than a command line, so metacharacters in it stay data.
#[cfg(windows)]
fn shell_execute_open(target: &str) {
    use std::os::windows::ffi::OsStrExt;
    let wide_target: Vec<u16> = std::ffi::OsStr::new(target)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let result = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            wide_target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };

    // A return at or below 32 is an error code rather than an instance
    // handle. Worth logging, because the symptom of failing here is a click
    // that appears to do nothing at all.
    let code = result as isize;
    if code <= 32 {
        tracing::warn!("ShellExecuteW could not open {target}: code {code}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_graphics_reject_unrelated_routes_transactionally() {
        assert!(route_ids_belong_to([4, 7, 4], &[4, 7]));
        assert!(!route_ids_belong_to([4, 8], &[4, 7]));
    }

    #[test]
    fn transfer_tab_count_transitions_reflow_both_window_edges() {
        let navigation = rio_backend::config::navigation::Navigation {
            hide_if_single: true,
            ..Default::default()
        };
        let margin = Margin::new(3.0, 4.0, 5.0, 6.0);
        let source_visible = scaled_margin_for_tabs(&navigation, margin, false, 2, 1.25);
        let source_hidden = scaled_margin_for_tabs(&navigation, margin, false, 1, 1.25);
        let target_hidden = scaled_margin_for_tabs(&navigation, margin, false, 1, 1.25);
        let target_visible = scaled_margin_for_tabs(&navigation, margin, false, 2, 1.25);

        assert_ne!(source_visible.top, source_hidden.top);
        assert_eq!(target_visible.top, source_visible.top);
        assert_eq!(target_hidden.top, source_hidden.top);
        assert_eq!(source_hidden.right, 5.0);
        assert_eq!(source_hidden.bottom, 6.25);
        assert_eq!(source_hidden.left, 7.5);
        assert_eq!(
            scaled_margin_for_tabs(&navigation, margin, false, 1, 1.25),
            source_hidden,
            "a source losing its selected tab must hide the strip again"
        );
    }

    #[test]
    fn chrome_press_validates_double_click() {
        use rio_window::dpi::PhysicalPosition;
        let origin = Some(PhysicalPosition::new(10, 20));

        // Same origin, fresh → a chrome double-click.
        let fresh = ChromePress {
            window_origin: origin,
            at: std::time::Instant::now(),
        };
        assert!(fresh.validates_double_click(origin));

        // Window moved between the presses (a re-grab after a window
        // drag) → keep dragging, don't maximize.
        assert!(!fresh.validates_double_click(Some(PhysicalPosition::new(110, 20))));

        // Unreported origin on both presses (Wayland) → time guard
        // alone decides; a reported-vs-unreported mix never validates.
        let unknown = ChromePress {
            window_origin: None,
            at: std::time::Instant::now(),
        };
        assert!(unknown.validates_double_click(None));
        assert!(!unknown.validates_double_click(origin));

        // Stale press → expired even at the same origin.
        let stale = ChromePress {
            window_origin: origin,
            at: std::time::Instant::now() - crate::constants::MULTI_CLICK_THRESHOLD * 2,
        };
        assert!(!stale.validates_double_click(origin));
    }

    #[test]
    fn test_post_process_hyperlink_uri() {
        assert_eq!(crate::hints::post_process_hyperlink_uri(")"), "");

        // Test removing trailing parenthesis
        assert_eq!(
            crate::hints::post_process_hyperlink_uri("https://example.com)"),
            "https://example.com"
        );

        // Test removing trailing comma
        assert_eq!(
            crate::hints::post_process_hyperlink_uri("https://example.com,"),
            "https://example.com"
        );

        // Test removing trailing period
        assert_eq!(
            crate::hints::post_process_hyperlink_uri("https://example.com."),
            "https://example.com"
        );

        // Test handling balanced parentheses (should keep them)
        assert_eq!(
            crate::hints::post_process_hyperlink_uri(
                "https://example.com/path(with)parens"
            ),
            "https://example.com/path(with)parens"
        );

        // Test handling unbalanced parentheses
        assert_eq!(
            crate::hints::post_process_hyperlink_uri("https://example.com/path)"),
            "https://example.com/path"
        );

        // Test handling multiple trailing delimiters
        assert_eq!(
            crate::hints::post_process_hyperlink_uri("https://example.com.'),"),
            "https://example.com"
        );

        // Test markdown-style URLs
        assert_eq!(
            crate::hints::post_process_hyperlink_uri("https://example.com)"),
            "https://example.com"
        );

        // Test handling unbalanced brackets
        assert_eq!(
            crate::hints::post_process_hyperlink_uri("https://example.com/path]"),
            "https://example.com/path"
        );

        // Test balanced brackets (should keep them)
        assert_eq!(
            crate::hints::post_process_hyperlink_uri(
                "https://example.com/path[with]brackets"
            ),
            "https://example.com/path[with]brackets"
        );
    }
}
