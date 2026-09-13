pub mod renderable;
pub mod session;
pub mod title;

use crate::context::title::{
    create_title_extra_from_context, update_title, ContextTitle,
};
use crate::event::sync::FairMutex;
use crate::event::RioEvent;
use crate::ime::Ime;
pub use crate::layout::{ContextDimension, ContextGrid, TabId};
use renderable::Cursor;
use renderable::RenderableContent;
use rio_backend::config::layout::Margin;
use rio_backend::config::Shell;
use session::{PreparedSession, RemoteView, SessionHandle};
use smallvec::{smallvec, SmallVec};

use rio_backend::error::{RioError, RioErrorLevel, RioErrorType};
use rio_backend::event::EventListener;
use rio_backend::event::{WindowId, WindowTarget};
use rio_backend::selection::SelectionRange;
use rio_backend::sugarloaf::{font::SugarloafFont, Rect, Sugarloaf, SugarloafErrors};
use rio_session::protocol::FullFrame;
use std::error::Error;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Global atomic counter for generating unique route IDs
static ROUTE_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);

// Global atomic counter for generating unique rich text IDs
static RICH_TEXT_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Generate a unique rich text ID for terminal contexts
pub fn next_rich_text_id() -> usize {
    RICH_TEXT_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

pub struct Context<T: EventListener> {
    pub route_id: usize,
    pub window_target: WindowTarget,
    /// Passive frame cache. The worker, not this cache, owns terminal state.
    pub terminal: Arc<FairMutex<RemoteView>>,
    /// A prepared cross-process attachment remains here until the destination
    /// window has prepared and drawn its initial frame. Dropping it only closes the
    /// pending socket; it never closes or replaces the source session.
    pub pending_session: Option<PreparedSession>,
    pub renderable_content: RenderableContent,
    pub rich_text_id: usize,
    pub dimension: ContextDimension,
    pub title: ContextTitle,
    pub ime: Ime,
    _listener: PhantomData<fn() -> T>,
}

impl<T: rio_backend::event::EventListener> Drop for Context<T> {
    fn drop(&mut self) {
        // Closing a context is explicit. The session worker owns/reaps the
        // PTY; dropping the GUI cache must not create a replacement shell.
        if let Some(session) = self.terminal.lock().session() {
            session.close();
        }
    }
}

impl<T: EventListener> Context<T> {
    /// Reassign this context and all of its queued producer events to a window.
    pub fn rebind_window(&mut self, window_id: WindowId) {
        self.window_target.rebind(window_id);
        let mut terminal = self.terminal.lock();
        terminal.window_id = window_id;
        if let Some(session) = terminal.session() {
            session.rebind_window(window_id);
        }
    }

    fn foreground_process_name(&self) -> Option<String> {
        None
    }

    pub(crate) fn foreground_process_path(&self) -> Option<std::path::PathBuf> {
        self.terminal.lock().current_directory.as_ref().cloned()
    }

    #[inline]
    pub fn set_selection(&mut self, selection_range: Option<SelectionRange>) {
        let old_selection = self.renderable_content.selection_range;
        let has_updated = old_selection != selection_range;

        if has_updated {
            // Selection affects terminal line rendering, so use terminal damage
            self.renderable_content
                .pending_update
                .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
        }

        self.renderable_content.selection_range = selection_range;
    }

    #[inline]
    pub fn cursor_from_ref(&self) -> Cursor {
        Cursor {
            state: self.renderable_content.cursor.state.new_from_self(),
            content: self.renderable_content.cursor.content_ref,
            content_ref: self.renderable_content.cursor.content_ref,
            is_ime_enabled: false,
        }
    }

    pub fn commit_pending_session(
        &mut self,
        event_proxy: T,
    ) -> Result<(), rio_session::SessionError>
    where
        T: Clone + Send + 'static,
    {
        let Some(prepared) = self.pending_session.take() else {
            return Ok(());
        };
        let initial_sequence = self.terminal.lock().frame_sequence();
        let client = prepared.commit()?;
        let session = SessionHandle::from_client(
            client,
            initial_sequence,
            event_proxy,
            self.route_id,
            self.window_target.window_id(),
        );
        // Prepared layout changes could not reach the worker before takeover.
        let size = crate::renderer::utils::terminal_dimensions(&self.dimension);
        let mut terminal = self.terminal.lock();
        terminal.install_session(session);
        terminal.resize_to(size);
        Ok(())
    }

    pub fn disarm_for_transfer(&mut self) {
        self.terminal.lock().detach_session();
    }
}

#[derive(Clone, Default)]
pub struct ContextManagerConfig {
    /// Build contexts without spawning a PTY (see
    /// `create_dead_context`). Unit tests fork one real `$SHELL` per
    /// context otherwise, which is slow and flaky under the parallel
    /// test runner (fork failures surface as random test panics).
    #[cfg(test)]
    pub dead_pty: bool,
    pub shell: Shell,
    pub working_dir: Option<String>,
    pub cwd: bool,
    pub is_native: bool,
    pub should_update_title_extra: bool,
    pub split_color: [f32; 4],
    pub panel: rio_backend::config::layout::Panel,
    pub title: rio_backend::config::title::Title,
    pub keyboard: rio_backend::config::keyboard::Keyboard,
    pub scrollback_history_limit: usize,
    pub grapheme_clustering: bool,
}

const DEFAULT_CONTEXT_CAPACITY: usize = 28;

pub struct ContextManager<T: EventListener> {
    contexts: SmallVec<[ContextGrid<T>; DEFAULT_CONTEXT_CAPACITY]>,
    current_index: usize,
    capacity: usize,
    event_proxy: T,
    window_id: WindowId,
    pub config: ContextManagerConfig,
    last_title_update: Option<Instant>,
    pub bootstrap_transfer: Option<([u8; 16], usize)>,
}

/// Ownership bundle for moving a complete grid and all of its split routes.
pub struct GridTransfer<T: EventListener> {
    grid: Box<ContextGrid<T>>,
}

impl<T: EventListener> GridTransfer<T> {
    fn new(grid: ContextGrid<T>) -> Self {
        Self {
            grid: Box::new(grid),
        }
    }

    pub fn id(&self) -> TabId {
        self.grid.id()
    }

    pub fn route_ids(&self) -> Vec<usize> {
        self.grid.route_ids()
    }
}

pub fn create_dead_context<T>(
    _event_proxy: T,
    window_id: WindowId,
    route_id: usize,
    rich_text_id: usize,
    dimension: ContextDimension,
) -> Context<T>
where
    T: rio_backend::event::EventListener + Clone,
{
    let window_target = WindowTarget::dynamic(window_id);
    let terminal = Arc::new(FairMutex::new(RemoteView::new(
        None,
        window_id,
        dimension.columns.max(1),
        dimension.lines.max(1),
    )));
    Context {
        route_id,
        window_target,
        pending_session: None,
        renderable_content: RenderableContent::new(Cursor::default()),
        terminal,
        rich_text_id,
        dimension,
        title: ContextTitle::default(),
        ime: Ime::new(),
        _listener: PhantomData,
    }
}

fn create_error_context<T>(
    event_proxy: T,
    window_id: WindowId,
    route_id: usize,
    rich_text_id: usize,
    dimension: ContextDimension,
    error: Box<dyn Error>,
) -> Context<T>
where
    T: rio_backend::event::EventListener + Clone,
{
    let window_target = WindowTarget::dynamic(window_id);
    let _ = event_proxy;
    let session = SessionHandle::disconnected_with_error(
        rio_session::SessionError::Invalid(error.to_string()),
    );
    let terminal = Arc::new(FairMutex::new(RemoteView::new(
        Some(session),
        window_id,
        dimension.columns.max(1),
        dimension.lines.max(1),
    )));

    Context {
        route_id,
        window_target,
        pending_session: None,
        terminal,
        rich_text_id,
        renderable_content: RenderableContent::new(Cursor::default()),
        dimension,
        title: ContextTitle::default(),
        ime: Ime::new(),
        _listener: PhantomData,
    }
}

fn create_prepared_context<T>(
    prepared: PreparedSession,
    window_id: WindowId,
    route_id: usize,
    rich_text_id: usize,
    dimension: ContextDimension,
) -> Context<T>
where
    T: rio_backend::event::EventListener,
{
    let mut prepared = prepared;
    let frame = prepared.take_initial_frame();
    create_prepared_context_with_frame(
        prepared,
        frame,
        window_id,
        route_id,
        rich_text_id,
        dimension,
    )
}

fn create_prepared_context_with_frame<T>(
    prepared: PreparedSession,
    frame: FullFrame,
    window_id: WindowId,
    route_id: usize,
    rich_text_id: usize,
    dimension: ContextDimension,
) -> Context<T>
where
    T: rio_backend::event::EventListener,
{
    let terminal = Arc::new(FairMutex::new(RemoteView::from_frame(frame, window_id)));
    Context {
        route_id,
        window_target: WindowTarget::dynamic(window_id),
        pending_session: Some(prepared),
        renderable_content: RenderableContent::new(Cursor::default()),
        terminal,
        rich_text_id,
        dimension,
        title: ContextTitle::default(),
        ime: Ime::new(),
        _listener: PhantomData,
    }
}

fn create_prepared_context_for_transfer<T>(
    prepared: PreparedSession,
    window_id: WindowId,
    route_id: usize,
    rich_text_id: usize,
    dimension: ContextDimension,
) -> Context<T>
where
    T: rio_backend::event::EventListener,
{
    let frame = prepared.initial_frame().clone();
    create_prepared_context_with_frame(
        prepared,
        frame,
        window_id,
        route_id,
        rich_text_id,
        dimension,
    )
}

fn restore_prepared_transfer<T: EventListener>(
    mut panes: rustc_hash::FxHashMap<
        u64,
        (crate::router::window_control::PaneOffer, PreparedSession),
    >,
    contexts: Vec<Context<T>>,
    target_panes: &mut rustc_hash::FxHashMap<
        usize,
        crate::router::window_control::PaneOffer,
    >,
    source_order: &[u64],
) -> Vec<(crate::router::window_control::PaneOffer, PreparedSession)> {
    for mut context in contexts {
        let Some(pane) = target_panes.remove(&context.route_id) else {
            tracing::error!(
                route_id = context.route_id,
                "transfer rollback lost route mapping"
            );
            continue;
        };
        let Some(prepared) = context.pending_session.take() else {
            tracing::error!(
                route_id = context.route_id,
                "transfer rollback lost prepared session"
            );
            continue;
        };
        panes.insert(pane.route_id, (pane, prepared));
    }

    let restored = source_order
        .iter()
        .filter_map(|source_route| panes.remove(source_route))
        .collect::<Vec<_>>();
    if restored.len() != source_order.len() {
        tracing::error!(
            restored = restored.len(),
            expected = source_order.len(),
            "transfer rollback did not recover every prepared session"
        );
    }
    restored
}

#[cfg(test)]
pub fn create_mock_context<
    T: rio_backend::event::EventListener + Clone + std::marker::Send + 'static,
>(
    event_proxy: T,
    window_id: WindowId,
    rich_text_id: usize,
    dimension: ContextDimension,
) -> Context<T> {
    let config = ContextManagerConfig {
        dead_pty: true,
        ..ContextManagerConfig::default()
    };
    ContextManager::create_context(
        (&Cursor::default(), false),
        event_proxy.clone(),
        window_id,
        rich_text_id,
        dimension,
        &config,
    )
    .unwrap()
}

/// Where `current_index` lands after removing the tab at `removed`:
/// the focused tab falls back to its left neighbor, and a background
/// removal shifts the focused index left only when the removed tab
/// sat before it. Pure, so the arithmetic the tmux SIGHUP crash
/// hinged on stays testable without a GPU.
#[cfg(test)]
#[inline]
fn current_index_after_tab_removal(current: usize, removed: usize) -> usize {
    if removed <= current {
        current.saturating_sub(1)
    } else {
        current
    }
}

impl<T: EventListener + Clone + std::marker::Send + 'static> ContextManager<T> {
    fn next_route_id() -> usize {
        ROUTE_ID_COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    #[inline]
    fn create_context(
        cursor_state: (&Cursor, bool),
        event_proxy: T,
        window_id: WindowId,
        rich_text_id: usize,
        dimension: ContextDimension,
        config: &ContextManagerConfig,
    ) -> Result<Context<T>, Box<dyn Error>> {
        Self::create_context_with_route_id(
            cursor_state,
            event_proxy,
            window_id,
            rich_text_id,
            dimension,
            config,
            Self::next_route_id(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_context_with_route_id(
        cursor_state: (&Cursor, bool),
        event_proxy: T,
        window_id: WindowId,
        rich_text_id: usize,
        dimension: ContextDimension,
        config: &ContextManagerConfig,
        route_id: usize,
    ) -> Result<Context<T>, Box<dyn Error>> {
        #[cfg(test)]
        if config.dead_pty {
            return Ok(create_dead_context(
                event_proxy,
                window_id,
                route_id,
                rich_text_id,
                dimension,
            ));
        }

        let window_target = WindowTarget::dynamic(window_id);
        let winsize = crate::renderer::utils::terminal_dimensions(&dimension);
        let mut spec = rio_session::SessionSpec::from_current_environment()?;
        spec.shell = config.shell.program.clone();
        spec.args = config.shell.args.clone();
        spec.working_dir = config.working_dir.clone();
        spec.columns = dimension.columns.try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "terminal columns exceed session limit",
            )
        })?;
        spec.lines = dimension.lines.try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "terminal lines exceed session limit",
            )
        })?;
        spec.pixel_width = winsize.width;
        spec.pixel_height = winsize.height;
        spec.scrollback = config.scrollback_history_limit;
        spec.grapheme_clustering = config.grapheme_clustering;

        let session =
            SessionHandle::spawn(spec, event_proxy.clone(), route_id, window_id)?;
        let terminal = Arc::new(FairMutex::new(RemoteView::new(
            Some(session),
            window_id,
            dimension.columns,
            dimension.lines,
        )));
        terminal
            .lock()
            .set_cursor_style(cursor_state.0.state.content, cursor_state.1);
        Ok(Context {
            route_id,
            window_target,
            pending_session: None,
            terminal,
            rich_text_id,
            renderable_content: RenderableContent::new(cursor_state.0.clone()),
            dimension,
            title: ContextTitle::default(),
            ime: Ime::new(),
            _listener: PhantomData,
        })
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        cursor_state: (&Cursor, bool),
        event_proxy: T,
        window_id: WindowId,
        rich_text_id: usize,
        ctx_config: ContextManagerConfig,
        size: ContextDimension,
        scaled_margin: Margin,
        sugarloaf_errors: Option<SugarloafErrors>,
    ) -> Result<Self, Box<dyn Error>> {
        let route_id = Self::next_route_id();
        let bootstrap_transfer = crate::WINDOW_BOOTSTRAP
            .lock()
            .unwrap()
            .take()
            .map(|id| (id, route_id));
        let initial_context = if bootstrap_transfer.is_some() {
            create_dead_context(
                event_proxy.clone(),
                window_id,
                route_id,
                rich_text_id,
                size,
            )
        } else {
            match ContextManager::create_context_with_route_id(
                cursor_state,
                event_proxy.clone(),
                window_id,
                rich_text_id,
                size,
                &ctx_config,
                route_id,
            ) {
                Ok(context) => context,
                Err(err_message) => {
                    tracing::error!("{:?}", err_message);

                    event_proxy.send_event(
                        RioEvent::ReportToAssistant(RioError {
                            report: RioErrorType::InitializationError(
                                err_message.to_string(),
                            ),
                            level: RioErrorLevel::Error,
                        }),
                        window_id,
                    );

                    create_error_context(
                        event_proxy.clone(),
                        window_id,
                        route_id,
                        rich_text_id,
                        ContextDimension::default(),
                        err_message,
                    )
                }
            }
        };

        // Sugarloaf has found errors and context need to notify it for the user
        if let Some(errors) = sugarloaf_errors {
            if !errors.fonts_not_found.is_empty() {
                event_proxy.send_event(
                    RioEvent::ReportToAssistant({
                        RioError {
                            report: RioErrorType::FontsNotFound(errors.fonts_not_found),
                            level: RioErrorLevel::Warning,
                        }
                    }),
                    window_id,
                );
            }
        }

        Ok(ContextManager {
            current_index: 0,
            contexts: smallvec![ContextGrid::new(
                initial_context,
                scaled_margin,
                ctx_config.split_color,
                ctx_config.panel,
            )],
            capacity: DEFAULT_CONTEXT_CAPACITY,
            event_proxy,
            window_id,
            config: ctx_config,
            last_title_update: None,
            bootstrap_transfer,
        })
    }

    #[cfg(test)]
    pub fn start_with_capacity(
        capacity: usize,
        event_proxy: T,
        window_id: WindowId,
    ) -> Result<Self, Box<dyn Error>> {
        let config = ContextManagerConfig {
            #[cfg(test)]
            dead_pty: true,
            ..ContextManagerConfig::default()
        };
        let initial_context = ContextManager::create_context(
            (&Cursor::default(), false),
            event_proxy.clone(),
            window_id,
            0,
            ContextDimension::default(),
            &config,
        )?;

        Ok(ContextManager {
            current_index: 0,
            contexts: smallvec![ContextGrid::new(
                initial_context,
                Margin::default(),
                config.split_color,
                config.panel,
            )],
            capacity,
            event_proxy,
            window_id,
            config,
            last_title_update: None,
            bootstrap_transfer: None,
        })
    }

    /// Construct a manager from a transfer and bind every split to its new owner.
    #[allow(dead_code)]
    pub fn from_transfer(
        transfer: GridTransfer<T>,
        event_proxy: T,
        window_id: WindowId,
        config: ContextManagerConfig,
    ) -> Self {
        let mut grid = *transfer.grid;
        grid.rebind_window(window_id);
        Self {
            contexts: smallvec![grid],
            current_index: 0,
            capacity: DEFAULT_CONTEXT_CAPACITY,
            event_proxy,
            window_id,
            config,
            last_title_update: None,
            bootstrap_transfer: None,
        }
    }

    #[inline]
    pub fn should_close_context_manager(
        &mut self,
        route_id: usize,
        sugarloaf: &mut Sugarloaf,
    ) -> bool {
        // should_close_context_manager is only called when terminal.exit()
        // is triggered. The terminal.exit() happens for any drop on context
        // by tab removal or if the Pty is exited (e.g: exit/control+d)
        //
        // In the tab case we already have removed the context with the
        // specified route_id so isn't gonna find anything. Then will be false.
        //
        // However if the tab is killed by Pty and not a tab action then
        // it means we need to clean the context with the specified route_id.
        // If there's no context then should return true and kill the window.
        let Some(index_to_remove) = self.grid_index_for_route(route_id) else {
            return self.contexts.is_empty();
        };

        if self.contexts[index_to_remove].len() > 1 {
            self.contexts[index_to_remove].remove_route(route_id, sugarloaf);
            return false;
        }

        self.contexts[index_to_remove].remove_from_sugarloaf(sugarloaf);
        self.contexts.remove(index_to_remove);
        self.update_selection_after_grid_removal(index_to_remove);
        if !self.contexts.is_empty() {
            self.keep_only_active_context_visible(sugarloaf);
        }

        self.contexts.is_empty()
    }

    #[inline]
    pub fn request_render(&mut self) {
        self.event_proxy
            .send_event(RioEvent::RenderRoute(self.current_route()), self.window_id);
    }

    #[inline]
    pub fn blink_cursor(&mut self, scheduled_time: u64) {
        // PrepareRender will force a render for any route that is focused on window
        // PrepareRenderOnRoute only call render function for specific route ids.
        self.event_proxy.send_event(
            RioEvent::BlinkCursor(scheduled_time, self.current_route()),
            self.window_id,
        );
    }

    #[inline]
    pub fn schedule_render_on_route(&mut self, millis: u64) {
        self.event_proxy.send_event(
            RioEvent::PrepareRenderOnRoute(millis, self.current_route()),
            self.window_id,
        );
    }

    #[inline]
    pub fn report_error_fonts_not_found(&mut self, fonts_not_found: Vec<SugarloafFont>) {
        if !fonts_not_found.is_empty() {
            self.event_proxy.send_event(
                RioEvent::ReportToAssistant({
                    RioError {
                        report: RioErrorType::FontsNotFound(fonts_not_found),
                        level: RioErrorLevel::Warning,
                    }
                }),
                self.window_id,
            );
        }
    }

    #[inline]
    pub fn create_new_window(&self) {
        self.event_proxy
            .send_event(RioEvent::CreateWindow, self.window_id);
    }

    #[inline]
    pub fn move_current_tab_to_new_window(&self) {
        self.event_proxy
            .send_event(RioEvent::MoveCurrentTabToNewWindow, self.window_id);
    }

    #[inline]
    pub fn merge_window(&self) {
        self.event_proxy
            .send_event(RioEvent::MergeWindow, self.window_id);
    }

    #[inline]
    pub fn toggle_quake(&self) {
        self.event_proxy
            .send_event(RioEvent::ToggleQuake, self.window_id);
    }

    #[inline]
    pub fn close_unfocused_tabs(&mut self, sugarloaf: &mut Sugarloaf) -> Vec<usize> {
        let current_route_id = self.current().route_id;
        let removed = self
            .contexts
            .iter()
            .filter(|grid| grid.current().route_id != current_route_id)
            .flat_map(ContextGrid::route_ids)
            .collect();
        self.contexts.retain(|ctx| {
            let keep = ctx.current().route_id == current_route_id;
            if !keep {
                ctx.remove_from_sugarloaf(sugarloaf);
            }
            keep
        });
        self.set_current(0);
        removed
    }

    #[inline]
    pub fn set_last_typing(&mut self) {
        self.current_mut().renderable_content.last_typing = Some(Instant::now());
    }

    #[inline]
    pub fn select_next_split(&mut self) {
        self.contexts[self.current_index].select_next_split();
    }

    #[inline]
    pub fn select_prev_split(&mut self) {
        self.contexts[self.current_index].select_prev_split();
    }

    #[inline]
    pub fn switch_to_next_split_or_tab(&mut self) {
        if self.contexts[self.current_index].select_next_split_no_loop() {
            return;
        }
        self.switch_to_next();
        // Make sure first split is selected - get the root key
        let current_tab = &mut self.contexts[self.current_index];
        if let Some(root) = current_tab.root {
            current_tab.current = root;
        }
    }

    #[inline]
    pub fn switch_to_prev_split_or_tab(&mut self) {
        if self.contexts[self.current_index].select_prev_split_no_loop() {
            return;
        }
        self.switch_to_prev();
        // Make sure last split is selected - get the last key in order
        let current_tab = &mut self.contexts[self.current_index];
        let ordered_keys = current_tab.get_ordered_keys();
        if let Some(&last_key) = ordered_keys.last() {
            current_tab.current = last_key;
        }
    }

    #[inline]
    pub fn move_divider_up(&mut self, amount: f32) -> bool {
        self.contexts[self.current_index].move_divider_up(amount)
    }

    #[inline]
    pub fn move_divider_down(&mut self, amount: f32) -> bool {
        self.contexts[self.current_index].move_divider_down(amount)
    }

    #[inline]
    pub fn move_divider_left(&mut self, amount: f32) -> bool {
        self.contexts[self.current_index].move_divider_left(amount)
    }

    #[inline]
    pub fn move_divider_right(&mut self, amount: f32) -> bool {
        self.contexts[self.current_index].move_divider_right(amount)
    }

    #[inline]
    pub fn select_tab(&mut self, tab_index: usize) {
        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::SelectNativeTabByIndex(tab_index), self.window_id);
            return;
        }

        self.set_current(tab_index);
    }

    #[inline]
    pub fn toggle_full_screen(&mut self) {
        self.event_proxy
            .send_event(RioEvent::ToggleFullScreen, self.window_id);
    }

    #[inline]
    pub fn toggle_appearance_theme(&mut self) {
        self.event_proxy
            .send_event(RioEvent::ToggleAppearanceTheme, self.window_id);
    }

    #[inline]
    pub fn minimize(&mut self) {
        self.event_proxy
            .send_event(RioEvent::Minimize(true), self.window_id);
    }

    #[inline]
    pub fn hide(&mut self) {
        self.event_proxy.send_event(RioEvent::Hide, self.window_id);
    }

    #[inline]
    pub fn quit(&mut self) {
        self.event_proxy.send_event(RioEvent::Quit, self.window_id);
    }

    #[cfg(target_os = "macos")]
    #[inline]
    pub fn hide_other_apps(&mut self) {
        self.event_proxy
            .send_event(RioEvent::HideOtherApplications, self.window_id);
    }

    #[inline]
    pub fn select_last_tab(&mut self) {
        if self.is_empty() {
            return;
        }

        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::SelectNativeTabLast, self.window_id);
            return;
        }

        if let Some(last) = self.contexts.len().checked_sub(1) {
            self.set_current(last);
        }
    }

    #[inline]
    pub fn switch_to_settings(&mut self) {
        self.event_proxy
            .send_event(RioEvent::CreateConfigEditor, self.window_id);
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    pub fn tab_index(&self, tab_id: TabId) -> Option<usize> {
        self.contexts.iter().position(|grid| grid.id() == tab_id)
    }

    pub fn tab_id_at(&self, index: usize) -> Option<TabId> {
        self.contexts.get(index).map(|grid| grid.id())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    #[inline]
    pub fn title(&self, index: usize) -> Option<&ContextTitle> {
        self.contexts.get(index).map(|grid| &grid.current().title)
    }

    #[inline]
    pub fn custom_title(&self, index: usize) -> Option<&str> {
        self.contexts
            .get(index)
            .and_then(|grid| grid.custom_title.as_deref())
    }

    #[inline]
    pub fn set_custom_title(&mut self, index: usize, title: Option<String>) {
        if let Some(grid) = self.contexts.get_mut(index) {
            grid.custom_title = title;
        }
    }

    #[inline]
    pub fn custom_color(&self, index: usize) -> Option<[f32; 4]> {
        self.contexts.get(index).and_then(|grid| grid.custom_color)
    }

    #[inline]
    pub fn set_custom_color(&mut self, index: usize, color: Option<[f32; 4]>) {
        if let Some(grid) = self.contexts.get_mut(index) {
            grid.custom_color = color;
        }
    }

    pub fn update_titles(&mut self) {
        if self.is_empty() {
            return;
        }

        let interval_time = Duration::from_secs(2);
        if self
            .last_title_update
            .map(|i| i.elapsed() > interval_time)
            .unwrap_or(true)
        {
            self.last_title_update = Some(Instant::now());
            for grid in self.contexts.iter_mut() {
                let content = update_title(&self.config.title.content, grid.current());

                let extra = if self.config.should_update_title_extra {
                    create_title_extra_from_context(grid.current())
                } else {
                    None
                };

                grid.current_mut().title = ContextTitle { content, extra };
            }
            self.event_proxy.send_event(
                RioEvent::Title(
                    self.current().route_id,
                    self.current().title.content.clone(),
                ),
                self.window_id,
            );
        }
    }

    #[inline]
    pub fn get_by_route_id(&mut self, route_id: usize) -> Option<&mut Context<T>> {
        self.contexts
            .iter_mut()
            .find_map(|grid| grid.get_by_route_id(route_id).map(|item| &mut item.val))
    }

    pub fn session_handles(&self) -> Vec<SessionHandle> {
        self.contexts
            .iter()
            .flat_map(ContextGrid::session_handles)
            .collect()
    }

    pub fn owns_session_id(&self, session_id: rio_session::protocol::SessionId) -> bool {
        self.contexts
            .iter()
            .any(|grid| grid.owns_session_id(session_id))
    }

    fn grid_index_for_route(&self, route_id: usize) -> Option<usize> {
        self.contexts
            .iter()
            .position(|grid| grid.contains_route_id(route_id))
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub fn contains_route_id(&self, route_id: usize) -> bool {
        self.grid_index_for_route(route_id).is_some()
    }

    #[inline]
    pub fn contexts_mut(
        &mut self,
    ) -> &mut SmallVec<[ContextGrid<T>; DEFAULT_CONTEXT_CAPACITY]> {
        &mut self.contexts
    }

    fn mark_all_full_damage(&mut self) {
        for grid in &mut self.contexts {
            for context in grid.contexts_mut().values_mut() {
                context
                    .context_mut()
                    .renderable_content
                    .pending_update
                    .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
            }
        }
    }

    fn update_selection_after_grid_removal(&mut self, removed_index: usize) {
        if self.contexts.is_empty() {
            self.current_index = 0;
            return;
        }

        if removed_index < self.current_index {
            self.current_index -= 1;
        } else if removed_index == self.current_index {
            self.current_index = removed_index.min(self.contexts.len() - 1);
        }
    }

    /// Remove a complete grid into a rollback-capable ownership bundle.
    pub fn extract_grid(&mut self, index: usize) -> Option<GridTransfer<T>> {
        if index >= self.contexts.len() {
            return None;
        }

        let grid = self.contexts.remove(index);
        self.update_selection_after_grid_removal(index);
        self.mark_all_full_damage();

        Some(GridTransfer::new(grid))
    }

    /// Insert and activate a transferred grid at `index`.
    ///
    /// Capacity and index failures return the exact, unchanged transfer bundle.
    pub fn insert_grid(
        &mut self,
        index: usize,
        mut transfer: GridTransfer<T>,
    ) -> Result<(), GridTransfer<T>> {
        if self.contexts.len() >= self.capacity || index > self.contexts.len() {
            return Err(transfer);
        }

        transfer.grid.rebind_window(self.window_id);
        self.contexts.insert(index, *transfer.grid);
        self.current_index = index;
        self.mark_all_full_damage();
        Ok(())
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub fn can_insert_grid(&self, index: usize) -> bool {
        self.can_insert_grids(index, 1)
    }

    pub fn can_insert_grids(&self, index: usize, count: usize) -> bool {
        index <= self.contexts.len()
            && count <= self.capacity.saturating_sub(self.contexts.len())
    }

    pub fn route_ids(&self) -> Vec<usize> {
        self.contexts
            .iter()
            .flat_map(ContextGrid::route_ids)
            .collect()
    }

    pub fn transfer_offer(
        &self,
        index: usize,
        transfer_id: [u8; 16],
    ) -> Result<crate::router::window_control::TransferOffer, String> {
        let grid = self
            .contexts
            .get(index)
            .ok_or_else(|| "transfer tab no longer exists".to_string())?;
        let (panes, tab) = grid.transfer_parts()?;
        let active_pane = grid
            .get_ordered_keys()
            .iter()
            .position(|node| *node == grid.current)
            .unwrap_or(0);
        Ok(crate::router::window_control::TransferOffer {
            transfer_id,
            panes,
            tabs: vec![tab],
            active_pane: active_pane as u32,
        })
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    pub fn transfer_window_offer(
        &self,
        transfer_id: [u8; 16],
    ) -> Result<crate::router::window_control::TransferOffer, String> {
        let mut panes = Vec::new();
        let mut tabs = Vec::new();
        let mut active_pane = 0;
        let active_route = self.current_route();
        for grid in &self.contexts {
            let (grid_panes, tab) = grid.transfer_parts()?;
            if grid.current().route_id == active_route {
                active_pane = panes.len()
                    + grid_panes
                        .iter()
                        .position(|pane| pane.route_id as usize == active_route)
                        .unwrap_or(0);
            }
            tabs.push(tab);
            panes.extend(grid_panes);
        }
        if panes.is_empty() {
            return Err("transfer window has no panes".into());
        }
        Ok(crate::router::window_control::TransferOffer {
            transfer_id,
            panes,
            tabs,
            active_pane: active_pane as u32,
        })
    }

    pub fn insert_prepared_pane(
        &mut self,
        prepared: PreparedSession,
        rich_text_id: usize,
        dimension: ContextDimension,
    ) -> Result<usize, Box<PreparedSession>> {
        if self.contexts.len() >= self.capacity {
            return Err(Box::new(prepared));
        }
        let route_id = Self::next_route_id();
        let context = create_prepared_context::<T>(
            prepared,
            self.window_id,
            route_id,
            rich_text_id,
            dimension,
        );
        let grid = ContextGrid::new(
            context,
            self.get_current_grid_scaled_margin(),
            self.config.split_color,
            self.config.panel,
        );
        self.contexts.push(grid);
        self.current_index = self.contexts.len() - 1;
        Ok(route_id)
    }

    pub fn insert_prepared_tabs(
        &mut self,
        prepared: Vec<(crate::router::window_control::PaneOffer, PreparedSession)>,
        tabs: Vec<crate::router::window_control::TabOffer>,
        active_pane: usize,
        insertion_index: Option<usize>,
        dimension: ContextDimension,
    ) -> Result<
        Vec<usize>,
        Vec<(crate::router::window_control::PaneOffer, PreparedSession)>,
    > {
        if prepared.is_empty()
            || active_pane >= prepared.len()
            || tabs.is_empty()
            || tabs.len() > self.capacity.saturating_sub(self.contexts.len())
            || insertion_index.is_some_and(|index| index > self.contexts.len())
        {
            return Err(prepared);
        }

        let active_source = prepared[active_pane].0.route_id;
        let Ok(active_pane) = u32::try_from(active_pane) else {
            return Err(prepared);
        };
        let source_tab_routes =
            match crate::router::window_control::TransferOffer::validate_parts(
                prepared.iter().map(|(pane, _)| pane),
                &tabs,
                active_pane,
            ) {
                Ok(routes) => routes,
                Err(error) => {
                    tracing::warn!(%error, "rejecting invalid prepared transfer");
                    return Err(prepared);
                }
            };

        let source_order: Vec<_> =
            prepared.iter().map(|(pane, _)| pane.route_id).collect();
        let mut panes = prepared
            .into_iter()
            .map(|(pane, prepared)| (pane.route_id, (pane, prepared)))
            .collect::<rustc_hash::FxHashMap<
                u64,
                (crate::router::window_control::PaneOffer, PreparedSession),
            >>();

        let mut tab_routes = rustc_hash::FxHashMap::default();
        let mut target_panes = rustc_hash::FxHashMap::default();
        let mut new_grids = Vec::with_capacity(tabs.len());
        let mut active_grid = None;
        for (index, (tab, routes)) in tabs.iter().zip(source_tab_routes).enumerate() {
            let pane_rects = routes
                .iter()
                .filter_map(|route| {
                    panes.get(route).map(|(pane, _)| (*route, pane.layout_rect))
                })
                .collect();
            let contexts = routes
                .iter()
                .map(|route| {
                    let (pane, prepared) = panes
                        .remove(route)
                        .expect("validated transfer route disappeared");
                    let target_route = Self::next_route_id();
                    tab_routes.insert(*route, target_route);
                    target_panes.insert(target_route, pane);
                    create_prepared_context_for_transfer::<T>(
                        prepared,
                        self.window_id,
                        target_route,
                        next_rich_text_id(),
                        dimension,
                    )
                })
                .collect();
            let grid = match ContextGrid::from_layout_offer(
                crate::layout::LayoutOfferInput {
                    contexts,
                    layout: &tab.layout,
                    active_route: tab.active_route,
                    route_map: &tab_routes,
                    pane_rects: &pane_rects,
                    scaled_margin: self.get_current_grid_scaled_margin(),
                    border_color: self.config.split_color,
                    panel_config: self.config.panel,
                },
            ) {
                Ok(grid) => grid,
                Err((contexts, error)) => {
                    tracing::warn!(%error, "rejecting transfer with an unrebuildable layout");
                    let contexts = new_grids
                        .into_iter()
                        .flat_map(ContextGrid::take_contexts)
                        .chain(contexts)
                        .collect();
                    return Err(restore_prepared_transfer(
                        panes,
                        contexts,
                        &mut target_panes,
                        &source_order,
                    ));
                }
            };
            if tab_routes
                .get(&active_source)
                .is_some_and(|_| active_grid.is_none())
            {
                active_grid = Some(index);
            }
            new_grids.push(grid);
        }
        if !panes.is_empty() {
            let contexts = new_grids
                .into_iter()
                .flat_map(ContextGrid::take_contexts)
                .collect();
            return Err(restore_prepared_transfer(
                panes,
                contexts,
                &mut target_panes,
                &source_order,
            ));
        }
        let insert_at = insertion_index.unwrap_or(self.contexts.len());
        for (offset, grid) in new_grids.into_iter().enumerate() {
            self.contexts.insert(insert_at + offset, grid);
        }
        self.current_index = insert_at + active_grid.unwrap_or(0);
        Ok(source_order
            .into_iter()
            .map(|source| {
                *tab_routes
                    .get(&source)
                    .expect("validated transfer source route disappeared")
            })
            .collect())
    }

    pub fn disarm_routes_for_transfer(&mut self, route_ids: &[usize]) {
        for route_id in route_ids {
            if let Some(context) = self.get_by_route_id(*route_id) {
                context.disarm_for_transfer();
            }
        }
    }

    pub fn remove_transferred_routes(
        &mut self,
        route_ids: &[usize],
        sugarloaf: &mut Sugarloaf,
    ) {
        self.disarm_routes_for_transfer(route_ids);
        for route_id in route_ids {
            let Some(index) = self.grid_index_for_route(*route_id) else {
                continue;
            };
            if self.contexts[index].len() == 1 {
                self.contexts.remove(index);
                self.update_selection_after_grid_removal(index);
            } else {
                self.contexts[index].remove_by_route(*route_id, sugarloaf);
            }
        }
        self.mark_all_full_damage();
    }

    #[inline]
    pub fn remove_current_grid(&mut self, sugarloaf: &mut Sugarloaf) {
        if let Some(grid) = self.contexts.get_mut(self.current_index) {
            grid.remove_current(sugarloaf);
        }
    }

    #[inline]
    pub fn current_grid_mut(&mut self) -> &mut ContextGrid<T> {
        self.contexts
            .get_mut(self.current_index)
            .expect("context manager has no current grid")
    }

    #[inline]
    pub fn current_grid(&self) -> &ContextGrid<T> {
        self.current_grid_opt()
            .expect("context manager has no current grid")
    }

    #[inline]
    pub fn current_grid_opt(&self) -> Option<&ContextGrid<T>> {
        self.contexts.get(self.current_index)
    }

    #[inline]
    pub fn get_panel_borders(&self) -> Vec<Rect> {
        self.current_grid_opt()
            .map_or_else(Vec::new, ContextGrid::get_panel_borders)
    }

    #[inline]
    pub fn get_current_grid_scaled_margin(&self) -> rio_backend::config::layout::Margin {
        self.current_grid_opt()
            .map_or_else(Margin::default, ContextGrid::get_scaled_margin)
    }

    #[cfg(test)]
    pub fn increase_capacity(&mut self, inc_val: usize) {
        self.capacity += inc_val;
    }

    #[inline]
    pub fn set_current(&mut self, context_id: usize) {
        if context_id < self.contexts.len() {
            self.current_index = context_id;
            let current = self.current();
            self.event_proxy.send_event(
                RioEvent::Title(current.route_id, current.title.content.clone()),
                self.window_id,
            );
        }
    }

    #[inline]
    pub fn close_current_context(&mut self, sugarloaf: &mut Sugarloaf) {
        if self.contexts.len() <= 1 {
            // MacOS: Close last tab will work, leading to hide and
            // keep Rio running in background.
            #[cfg(target_os = "macos")]
            {
                self.event_proxy
                    .send_event(RioEvent::CloseWindow, self.window_id);
            }
            return;
        }

        let index_to_remove = self.current_index;
        let mut should_set_current = false;
        if index_to_remove > 1 {
            self.set_current(self.current_index - 1);
        } else {
            should_set_current = true;
        }

        // Remove all rich text from the grid before removing the context
        self.contexts[index_to_remove].remove_from_sugarloaf(sugarloaf);
        self.contexts.remove(index_to_remove);

        if should_set_current {
            self.set_current(0);
        }

        self.keep_only_active_context_visible(sugarloaf);
    }

    #[inline]
    pub fn current_index(&self) -> usize {
        self.current_index
    }

    #[inline]
    pub fn current_route(&self) -> usize {
        self.current_grid_opt()
            .map_or(0, |grid| grid.current().route_id)
    }

    #[inline]
    pub fn current(&self) -> &Context<T> {
        self.current_grid().current()
    }

    #[inline]
    pub fn current_mut(&mut self) -> &mut Context<T> {
        self.current_grid_mut().current_mut()
    }

    #[inline]
    pub fn switch_to_next(&mut self) {
        if self.is_empty() {
            return;
        }

        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::SelectNativeTabNext, self.window_id);
            return;
        }

        let next = if self.contexts.len() - 1 == self.current_index {
            0
        } else {
            self.current_index + 1
        };
        self.set_current(next);
    }

    #[inline]
    pub fn switch_to_prev(&mut self) {
        if self.is_empty() {
            return;
        }

        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::SelectNativeTabPrev, self.window_id);
            return;
        }

        let previous = if self.current_index == 0 {
            self.contexts.len() - 1
        } else {
            self.current_index - 1
        };
        self.set_current(previous);
    }

    #[inline]
    pub fn move_current_to_prev(&mut self) {
        let len = self.contexts.len();
        if len <= 1 {
            return;
        }

        let current = self.current_index;
        let target_index = if current == 0 { len - 1 } else { current - 1 };
        self.contexts.swap(current, target_index);
        self.select_tab(target_index);
    }

    #[inline]
    pub fn move_current_to_next(&mut self) {
        let len = self.contexts.len();
        if len <= 1 {
            return;
        }

        let current = self.current_index;
        let target_index = if current == len - 1 { 0 } else { current + 1 };
        self.contexts.swap(current, target_index);
        self.select_tab(target_index);
    }

    #[inline]
    pub fn move_current_tab_to(&mut self, target: usize) {
        if self.config.is_native {
            return;
        }

        let current = self.current_index;
        if target == current || target >= self.contexts.len() {
            return;
        }

        let grid = self.contexts.remove(current);
        self.contexts.insert(target, grid);
        self.set_current(target);
    }

    pub fn split(&mut self, rich_text_id: usize, split_down: bool) {
        let mut cloned_config = self.config.clone();
        cloned_config.working_dir = self.working_dir_for_new_context();

        let current = self.current();
        let cursor = current.cursor_from_ref();

        match ContextManager::create_context(
            (&cursor, current.renderable_content.has_blinking_enabled),
            self.event_proxy.clone(),
            self.window_id,
            rich_text_id,
            self.current().dimension,
            &cloned_config,
        ) {
            Ok(new_context) => {
                if split_down {
                    self.contexts[self.current_index].split_down(new_context);
                } else {
                    self.contexts[self.current_index].split_right(new_context);
                }
            }
            Err(..) => {
                tracing::error!("not able to create a new context");
            }
        }
    }

    pub fn split_from_config(
        &mut self,
        rich_text_id: usize,
        split_down: bool,
        config: rio_backend::config::Config,
    ) {
        let (shell, working_dir) = process_open_url(
            config.shell.to_owned(),
            config.working_dir.to_owned(),
            config.editor.to_owned(),
            None,
        );

        let context_manager_config = ContextManagerConfig {
            #[cfg(test)]
            dead_pty: false,
            cwd: config.navigation.current_working_directory,
            shell,
            working_dir,
            is_native: config.navigation.is_native(),
            // When navigation is collapsed and does not contain any color rule
            // does not make sense fetch for foreground process names
            should_update_title_extra: !config.navigation.color_automation.is_empty(),
            split_color: config.colors.split,
            panel: config.panel,
            title: config.title,
            keyboard: config.keyboard,
            scrollback_history_limit: config.scrollback_history_limit,
            grapheme_clustering: config.grapheme_clustering,
        };

        let current = self.current();
        let cursor = current.cursor_from_ref();

        match ContextManager::create_context(
            (&cursor, current.renderable_content.has_blinking_enabled),
            self.event_proxy.clone(),
            self.window_id,
            rich_text_id,
            self.current().dimension,
            &context_manager_config,
        ) {
            Ok(new_context) => {
                if split_down {
                    self.contexts[self.current_index].split_down(new_context);
                } else {
                    self.contexts[self.current_index].split_right(new_context);
                }
            }
            Err(..) => {
                tracing::error!("not able to create a new context");
            }
        }
    }

    #[inline]
    pub fn add_context(&mut self, redirect: bool, rich_text_id: usize) {
        let working_dir = self.working_dir_for_new_context();

        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::CreateNativeTab(working_dir), self.window_id);
            return;
        }

        let size = self.contexts.len();
        if size < self.capacity {
            let last_index = self.contexts.len();

            let mut cloned_config = self.config.clone();
            cloned_config.working_dir = working_dir;

            let current = self.current();
            let cursor = current.cursor_from_ref();
            let mut dimension = current.dimension;

            // If current has splits then shouldn't use that dimension
            if self.current_grid().len() > 1 {
                dimension = self.current_grid().grid_dimension();
            }

            match ContextManager::create_context(
                (&cursor, current.renderable_content.has_blinking_enabled),
                self.event_proxy.clone(),
                self.window_id,
                rich_text_id,
                dimension,
                &cloned_config,
            ) {
                Ok(new_context) => {
                    let previous_scaled_margin =
                        self.contexts[self.current_index].scaled_margin;
                    self.contexts.push(ContextGrid::new(
                        new_context,
                        previous_scaled_margin,
                        self.config.split_color,
                        self.config.panel,
                    ));
                    if redirect {
                        self.current_index = last_index;
                    }
                }
                Err(..) => {
                    tracing::error!("not able to create a new context");
                }
            }
        }
    }

    fn working_dir_for_new_context(&self) -> Option<String> {
        if self.config.cwd {
            self.current()
                .foreground_process_path()
                .map(|path| path.to_string_lossy().into_owned())
                .or_else(|| self.config.working_dir.clone())
        } else {
            self.config.working_dir.clone()
        }
    }

    /// Hide all rich text components except for the current tab
    #[inline]
    pub fn keep_only_active_context_visible(&self, sugarloaf: &mut Sugarloaf) {
        for (idx, context) in self.contexts.iter().enumerate() {
            if idx != self.current_index {
                context.clear_image_overlays(sugarloaf);
            }
        }
    }

    /// Switch visibility between two contexts (hide old, show new)
    #[inline]
    pub fn clear_context_overlays(&self, sugarloaf: &mut Sugarloaf, old_index: usize) {
        if let Some(old_context) = self.contexts.get(old_index) {
            old_context.clear_image_overlays(sugarloaf);
        }
    }
}

pub fn process_open_url(
    mut shell: Shell,
    mut working_dir: Option<String>,
    editor: Shell,
    open_url: Option<&str>,
) -> (Shell, Option<String>) {
    if open_url.is_none() {
        return (shell, working_dir);
    }

    if let Ok(url) = url::Url::parse(open_url.unwrap_or_default()) {
        if let Ok(path_buf) = url.to_file_path() {
            if path_buf.exists() {
                if path_buf.is_file() {
                    let mut args = editor.args;
                    args.push(path_buf.display().to_string());
                    shell = Shell {
                        program: editor.program,
                        args,
                    }
                } else if path_buf.is_dir() {
                    working_dir = Some(path_buf.display().to_string());
                }
            }
        }
    }

    (shell, working_dir)
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::event::{EventPayload, RioEventType, VoidListener};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct TitleListener(Arc<Mutex<Vec<(usize, String)>>>);

    impl EventListener for TitleListener {
        fn send_event(&self, event: RioEvent, _id: WindowId) {
            if let RioEvent::Title(route_id, title) = event {
                self.0.lock().unwrap().push((route_id, title));
            }
        }
    }

    #[test]
    fn test_capacity() {
        let window_id: WindowId = WindowId::from(0);

        let context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        assert_eq!(context_manager.capacity, 5);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        context_manager.increase_capacity(3);
        assert_eq!(context_manager.capacity, 8);
    }

    #[test]
    fn palette_selection_marks_full_terminal_damage() {
        let mut context_manager =
            ContextManager::start_with_capacity(1, VoidListener {}, WindowId::from(0))
                .unwrap();
        let selection = SelectionRange::new(
            rio_backend::crosswords::pos::Pos::new(
                rio_backend::crosswords::pos::Line(0),
                rio_backend::crosswords::pos::Column(0),
            ),
            rio_backend::crosswords::pos::Pos::new(
                rio_backend::crosswords::pos::Line(0),
                rio_backend::crosswords::pos::Column(1),
            ),
            false,
        );

        context_manager
            .current_mut()
            .renderable_content
            .pending_update
            .take_terminal_damage();
        context_manager.current_mut().set_selection(Some(selection));

        assert_eq!(
            context_manager
                .current_mut()
                .renderable_content
                .pending_update
                .take_terminal_damage(),
            Some(rio_backend::event::TerminalDamage::Full)
        );
    }

    /// Regression: backend events (PTY-reply targets, color requests,
    /// damage notifications) carry a `route_id` that identifies the
    /// originating panel, not the visible one. `get_by_route_id` used to
    /// scan only the active tab, so a reply destined for a hidden tab was
    /// silently dropped — most visibly, a shell on the hidden tab that had
    /// issued a cursor-position query would wait out its full timeout
    /// before continuing, freezing visible input echo on that tab.
    #[test]
    fn test_get_by_route_id_finds_hidden_tab() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let hidden_route_id = context_manager.contexts[0].current().route_id;

        context_manager.add_context(true, 0);
        assert_eq!(
            context_manager.current_index, 1,
            "second tab should be active after add_context(redirect=true, …)"
        );

        let found = context_manager
            .get_by_route_id(hidden_route_id)
            .expect("hidden tab's route_id must still resolve via get_by_route_id");
        assert_eq!(found.route_id, hidden_route_id);
    }

    #[test]
    fn update_titles_emits_only_the_current_tab() {
        let listener = TitleListener::default();
        let events = Arc::clone(&listener.0);
        let mut manager =
            ContextManager::start_with_capacity(3, listener, WindowId::from(0)).unwrap();
        manager.add_context(true, 0);
        manager.config.title.content = "{{columns}}".into();
        manager.contexts[0].current_mut().dimension.columns = 80;
        manager.contexts[1].current_mut().dimension.columns = 120;
        manager.update_titles();

        assert_eq!(manager.title(0).unwrap().content, "80");
        assert_eq!(manager.title(1).unwrap().content, "120");
        assert_eq!(
            *events.lock().unwrap(),
            [(manager.current().route_id, "120".into())]
        );

        events.lock().unwrap().clear();
        manager.set_current(0);
        assert_eq!(
            *events.lock().unwrap(),
            [(manager.current().route_id, "80".into())]
        );
    }

    #[test]
    fn test_add_context() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current_index, 0);

        let should_redirect = false;
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current_index, 0);

        let should_redirect = true;
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current_index, 2);
    }

    #[test]
    fn test_add_context_start_with_capacity_limit() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(3, VoidListener {}, window_id).unwrap();
        assert_eq!(context_manager.capacity, 3);
        assert_eq!(context_manager.current_index, 0);
        let should_redirect = false;
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.len(), 2);
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.len(), 3);

        for _ in 0..20 {
            context_manager.add_context(should_redirect, 0);
        }

        assert_eq!(context_manager.len(), 3);
        assert_eq!(context_manager.capacity, 3);
    }

    /// The tmux `new -ADXs` crash (#1848): SIGHUP kills a BACKGROUND
    /// tab's shell, the tab at a lower index is removed, and the
    /// focused index must shift left with it or the very next
    /// `contexts[current_index]` is out of bounds (len 1, index 1).
    #[test]
    fn current_index_adjusts_after_tab_removal() {
        // Background tab before the focused one dies: shift left.
        assert_eq!(current_index_after_tab_removal(1, 0), 0);
        assert_eq!(current_index_after_tab_removal(3, 1), 2);
        // Background tab after the focused one: nothing shifts.
        assert_eq!(current_index_after_tab_removal(0, 1), 0);
        assert_eq!(current_index_after_tab_removal(2, 3), 2);
        // The focused tab itself: fall back to the left neighbor.
        assert_eq!(current_index_after_tab_removal(0, 0), 0);
        assert_eq!(current_index_after_tab_removal(2, 2), 1);
    }

    #[test]
    fn test_set_current() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(8, VoidListener {}, window_id).unwrap();
        let should_redirect = true;

        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.current_index, 1);
        context_manager.set_current(0);
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.len(), 2);
        assert_eq!(context_manager.capacity, 8);

        let should_redirect = false;
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.set_current(3);
        assert_eq!(context_manager.current_index, 3);

        context_manager.set_current(8);
        assert_eq!(context_manager.current_index, 3);
    }

    fn set_tab_title(cm: &mut ContextManager<VoidListener>, index: usize, content: &str) {
        cm.contexts[index].current_mut().title.content = content.to_string();
    }

    fn tab_titles(cm: &ContextManager<VoidListener>) -> Vec<String> {
        (0..cm.len())
            .map(|i| cm.title(i).unwrap().content.clone())
            .collect()
    }

    fn tab_ids(cm: &ContextManager<VoidListener>) -> Vec<TabId> {
        cm.contexts.iter().map(ContextGrid::id).collect()
    }

    #[test]
    fn tab_identity_is_unique_and_stable_across_reordering() {
        let mut manager =
            ContextManager::start_with_capacity(4, VoidListener {}, WindowId::from(0))
                .unwrap();
        manager.add_context(false, 0);
        manager.add_context(false, 0);

        let original = tab_ids(&manager);
        assert_eq!(original.len(), 3);
        assert!(original.iter().all(|id| original
            .iter()
            .filter(|other| *other == id)
            .count()
            == 1));

        manager.set_current(0);
        manager.move_current_tab_to(2);
        assert_eq!(tab_ids(&manager), [original[1], original[2], original[0]]);
        assert_eq!(manager.current_grid().id(), original[0]);
    }

    #[test]
    fn extraction_captures_the_whole_grid_and_all_split_routes() {
        let mut manager =
            ContextManager::start_with_capacity(4, VoidListener {}, WindowId::from(0))
                .unwrap();
        manager.add_context(false, 0);
        manager.add_context(false, 0);
        for (index, title) in ["first", "second", "third"].iter().enumerate() {
            set_tab_title(&mut manager, index, title);
        }
        manager.set_custom_title(1, Some("custom".into()));
        manager.set_custom_color(1, Some([0.1, 0.2, 0.3, 1.0]));
        manager.set_current(1);
        let split_route = 1_000_000;
        manager
            .current_grid_mut()
            .add_split_for_test(create_dead_context(
                VoidListener {},
                WindowId::from(0),
                split_route,
                99,
                ContextDimension::default(),
            ));
        manager.current_mut().title.content = "active split".into();
        let extracted_id = manager.current_grid().id();
        let extracted_route = manager.current_route();
        let expected_routes = manager.current_grid().route_ids();

        let transfer = manager.extract_grid(1).unwrap();

        assert_eq!(transfer.id(), extracted_id);
        assert_eq!(transfer.route_ids(), expected_routes);
        assert!(transfer.route_ids().contains(&split_route));
        assert_eq!(transfer.grid.len(), 2);
        assert_eq!(transfer.grid.current().route_id, extracted_route);
        assert_eq!(transfer.grid.current().title.content, "active split");
        assert_eq!(transfer.grid.custom_title.as_deref(), Some("custom"));
        assert_eq!(transfer.grid.custom_color, Some([0.1, 0.2, 0.3, 1.0]));
        assert_eq!(tab_titles(&manager), ["first", "third"]);
        assert_eq!(manager.current_index(), 1);
        assert_eq!(manager.current_route(), manager.current().route_id);
    }

    #[test]
    fn hidden_split_route_resolves_its_grid_and_removal_keeps_current_tab() {
        let window_id = WindowId::from(0);
        let mut manager =
            ContextManager::start_with_capacity(4, VoidListener {}, window_id).unwrap();
        manager.add_context(false, 0);
        manager.add_context(true, 0);
        let selected_tab = manager.current_grid().id();
        let hidden_split_route = 1_000_002;
        manager.contexts[0].add_split_for_test(create_dead_context(
            VoidListener {},
            window_id,
            hidden_split_route,
            2,
            ContextDimension::default(),
        ));

        let hidden_index = manager
            .grid_index_for_route(hidden_split_route)
            .expect("a hidden split route must resolve to its owning tab");
        let removed = manager.extract_grid(hidden_index).unwrap();

        assert!(removed.route_ids().contains(&hidden_split_route));
        assert_eq!(manager.current_grid().id(), selected_tab);
        assert_eq!(manager.current_index(), 1);
        assert_eq!(manager.current_route(), manager.current().route_id);
    }

    #[test]
    fn transfer_rebinds_queued_events_for_every_split() {
        let old_window = WindowId::from(11);
        let new_window = WindowId::from(22);
        let mut source =
            ContextManager::start_with_capacity(2, VoidListener {}, old_window).unwrap();
        source
            .current_grid_mut()
            .add_split_for_test(create_dead_context(
                VoidListener {},
                old_window,
                1_000_001,
                1,
                ContextDimension::default(),
            ));
        let routes = source.current_grid().route_ids();
        let queued = EventPayload::new(
            RioEventType::Rio(RioEvent::Render),
            source.current().window_target.clone(),
        );
        let transfer = source.extract_grid(0).unwrap();

        let mut destination =
            ContextManager::start_with_capacity(2, VoidListener {}, new_window).unwrap();
        destination
            .insert_grid(1, transfer)
            .unwrap_or_else(|_| panic!("destination should accept transfer"));

        assert_eq!(queued.window_id(), new_window);
        for route_id in routes {
            let context = destination.get_by_route_id(route_id).unwrap();
            assert_eq!(context.window_target.window_id(), new_window);
            assert_eq!(context.terminal.lock().window_id, new_window);
        }
    }

    #[test]
    fn indexed_insert_preserves_order_and_activates_grid() {
        let window_id = WindowId::from(0);
        let mut source =
            ContextManager::start_with_capacity(3, VoidListener {}, window_id).unwrap();
        source.set_custom_title(0, Some("moved".into()));
        let route = source.current_route();
        let transfer = source.extract_grid(0).unwrap();
        assert!(source.is_empty());
        assert_eq!(source.current_route(), 0);

        let mut destination =
            ContextManager::start_with_capacity(3, VoidListener {}, window_id).unwrap();
        destination.add_context(false, 0);
        let previous_ids = tab_ids(&destination);
        let moved_id = transfer.id();
        destination
            .insert_grid(1, transfer)
            .unwrap_or_else(|_| panic!("transfer should fit"));

        assert_eq!(
            tab_ids(&destination),
            [previous_ids[0], moved_id, previous_ids[1]]
        );
        assert_eq!(destination.current_index(), 1);
        assert_eq!(destination.current_route(), route);
        assert_eq!(destination.custom_title(1), Some("moved"));
    }

    #[test]
    fn insert_failures_return_the_unchanged_transfer() {
        let window_id = WindowId::from(0);
        let mut source =
            ContextManager::start_with_capacity(2, VoidListener {}, window_id).unwrap();
        source.set_custom_title(0, Some("owned".into()));
        source.set_custom_color(0, Some([1.0, 0.5, 0.0, 1.0]));
        let transfer = source.extract_grid(0).unwrap();
        let id = transfer.id();
        let routes = transfer.route_ids();

        let mut full =
            ContextManager::start_with_capacity(1, VoidListener {}, window_id).unwrap();
        let returned = full
            .insert_grid(1, transfer)
            .expect_err("capacity must reject the insert");

        assert_eq!(returned.id(), id);
        assert_eq!(returned.route_ids(), routes);
        assert_eq!(returned.grid.custom_title.as_deref(), Some("owned"));
        assert_eq!(returned.grid.custom_color, Some([1.0, 0.5, 0.0, 1.0]));
        assert_eq!(full.len(), 1);
        assert_eq!(full.current_index(), 0);
        assert_eq!(full.current_route(), full.current().route_id);

        let mut room =
            ContextManager::start_with_capacity(2, VoidListener {}, window_id).unwrap();
        let returned = room
            .insert_grid(2, returned)
            .expect_err("out-of-bounds index must reject the insert");
        assert_eq!(returned.id(), id);
        assert_eq!(returned.route_ids(), routes);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn failed_initial_pty_preserves_allocated_route_id() {
        let mut config = ContextManagerConfig {
            dead_pty: false,
            ..ContextManagerConfig::default()
        };
        config.shell.program = Some("/rio/does/not/exist".into());

        let manager = ContextManager::start(
            (&Cursor::default(), false),
            VoidListener {},
            WindowId::from(1),
            7,
            config,
            ContextDimension::default(),
            Margin::default(),
            None,
        )
        .expect("failed PTY startup should create a dead fallback context");

        assert_ne!(manager.current_route(), 0);
        assert_eq!(manager.current().route_id, manager.current_route());
        assert_eq!(manager.current().rich_text_id, 7);
    }

    #[test]
    fn manager_from_transfer_preserves_identity_and_active_route() {
        let window_id = WindowId::from(0);
        let mut source =
            ContextManager::start_with_capacity(2, VoidListener {}, window_id).unwrap();
        source.set_custom_title(0, Some("detached".into()));
        let transfer = source.extract_grid(0).unwrap();
        let id = transfer.id();
        let route = transfer.grid.current().route_id;

        let manager = ContextManager::from_transfer(
            transfer,
            VoidListener {},
            window_id,
            ContextManagerConfig {
                dead_pty: true,
                ..ContextManagerConfig::default()
            },
        );

        assert_eq!(manager.current_grid().id(), id);
        assert_eq!(manager.current_route(), route);
        assert_eq!(manager.custom_title(0), Some("detached"));
    }

    #[test]
    fn transferred_grid_accepts_destination_window_geometry() {
        let mut manager =
            ContextManager::start_with_capacity(2, VoidListener {}, WindowId::from(0))
                .unwrap();
        let margin = Margin::new(11.25, 2.5, 6.25, 3.75);
        let grid = manager.current_grid_mut();

        grid.width = 901.0;
        grid.height = 607.0;
        grid.update_scaled_margin(margin);
        grid.update_scale(1.25);
        for context in grid.contexts_mut().values_mut() {
            context.context_mut().dimension.update_scale(1.25);
        }

        assert_eq!((grid.width, grid.height), (901.0, 607.0));
        assert_eq!(grid.scaled_margin, margin);
        assert_eq!(grid.current().dimension.dimension.scale, 1.25);
    }

    #[test]
    fn test_title_follows_tab_move() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        for _ in 0..3 {
            cm.add_context(false, 0);
        }
        assert_eq!(cm.len(), 4);
        for (i, label) in ["a", "b", "c", "d"].iter().enumerate() {
            set_tab_title(&mut cm, i, label);
        }

        // Drag tab 1 to slot 3 (rotate). The title must track the moved
        // tab immediately, without waiting on the next update_titles tick.
        cm.set_current(1);
        cm.move_current_tab_to(3);

        assert_eq!(tab_titles(&cm), ["a", "c", "d", "b"]);
        assert_eq!(cm.current().title.content, "b");
    }

    #[test]
    fn test_title_follows_tab_swap() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        for _ in 0..3 {
            cm.add_context(false, 0);
        }
        for (i, label) in ["a", "b", "c", "d"].iter().enumerate() {
            set_tab_title(&mut cm, i, label);
        }

        // Swap current (0) with its neighbor (1).
        cm.set_current(0);
        cm.move_current_to_next();

        assert_eq!(tab_titles(&cm), ["b", "a", "c", "d"]);
    }

    #[test]
    fn test_custom_title_follows_tab_move() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        for _ in 0..3 {
            cm.add_context(false, 0);
        }
        cm.set_custom_title(2, Some("work".to_string()));

        // Move tab 1 → 3 (rotate): the override on tab 2 shifts to slot 1,
        // with no remap bookkeeping.
        cm.set_current(1);
        cm.move_current_tab_to(3);

        assert_eq!(cm.custom_title(1), Some("work"));
        assert_eq!(cm.custom_title(2), None);

        // Clearing with None removes the override.
        cm.set_custom_title(1, None);
        assert_eq!(cm.custom_title(1), None);
    }

    #[test]
    fn test_custom_color_follows_tab_move() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        for _ in 0..3 {
            cm.add_context(false, 0);
        }
        let red = [1.0, 0.0, 0.0, 1.0];
        cm.set_custom_color(2, Some(red));

        // Move tab 1 → 3 (rotate): the color on tab 2 shifts to slot 1.
        cm.set_current(1);
        cm.move_current_tab_to(3);

        assert_eq!(cm.custom_color(1), Some(red));
        assert_eq!(cm.custom_color(2), None);

        cm.set_custom_color(1, None);
        assert_eq!(cm.custom_color(1), None);
    }

    #[test]
    fn test_switch_to_next() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let should_redirect = false;

        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.len(), 5);
        assert_eq!(context_manager.current_index, 0);

        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 1);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 2);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 3);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 4);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 0);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 1);
    }

    #[test]
    fn test_move_current_to_next() {
        let window_id = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let should_redirect = false;

        context_manager.current_mut().rich_text_id = 1;
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);

        assert_eq!(context_manager.len(), 5);
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 1);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 2);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 3);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 4);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 1);
        assert_eq!(context_manager.current().rich_text_id, 1);
    }

    #[test]
    fn test_move_current_to_prev() {
        let window_id = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let should_redirect = false;

        context_manager.current_mut().rich_text_id = 1;
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);

        assert_eq!(context_manager.len(), 5);
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 4);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 3);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 2);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 1);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 4);
        assert_eq!(context_manager.current().rich_text_id, 1);
    }

    #[test]
    fn test_move_current_tab_to() {
        let window_id = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let should_redirect = false;

        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);

        // Tag every tab with its starting position.
        for i in 0..5 {
            context_manager.set_current(i);
            context_manager.current_mut().rich_text_id = i;
        }

        let order = |cm: &mut ContextManager<VoidListener>| -> Vec<usize> {
            (0..5)
                .map(|i| {
                    cm.set_current(i);
                    cm.current().rich_text_id
                })
                .collect()
        };

        // Multi-slot jump forward: tabs in between shift left by one.
        context_manager.set_current(1);
        context_manager.move_current_tab_to(3);
        assert_eq!(context_manager.current_index, 3);
        assert_eq!(context_manager.current().rich_text_id, 1);
        assert_eq!(order(&mut context_manager), vec![0, 2, 3, 1, 4]);

        // Multi-slot jump backward: tabs in between shift right by one.
        context_manager.set_current(3);
        context_manager.move_current_tab_to(0);
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);
        assert_eq!(order(&mut context_manager), vec![1, 0, 2, 3, 4]);

        // No-op cases: same index and out-of-bounds target.
        context_manager.set_current(2);
        context_manager.move_current_tab_to(2);
        assert_eq!(context_manager.current_index, 2);
        context_manager.move_current_tab_to(5);
        assert_eq!(context_manager.current_index, 2);
        assert_eq!(order(&mut context_manager), vec![1, 0, 2, 3, 4]);
    }
}
