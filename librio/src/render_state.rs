use crate::{GraphicsSnapshot, GraphicsSnapshotError, Listener, Surface};
use rio_vt::ansi::graphics::{
    kitty_overlay_geometry, KittyOverlayGeometry, KittyPlacement, OverlayViewport,
    UpdateQueues, VirtualPlacement,
};
use rio_vt::ansi::kitty_virtual::{
    compute_run_geometry, resolve_virtual_placement, IncompletePlacement, PlaceholderRun,
    PLACEHOLDER,
};
use rio_vt::ansi::CursorShape;
use rio_vt::config::colors::term::TermColors;
use rio_vt::config::colors::{AnsiColor, ColorRgb};
use rio_vt::crosswords::grid::row::Row;
use rio_vt::crosswords::grid::Dimensions;
use rio_vt::crosswords::pos::Column;
use rio_vt::crosswords::square::{ContentTag, Extras, Square};
use rio_vt::crosswords::style::Style;
use rio_vt::crosswords::Crosswords;
use rio_vt::event::sync::FairMutex;
use rio_vt::event::TerminalDamage;
use rustc_hash::FxHashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportSelection {
    pub start_line: u16,
    pub start_col: u16,
    pub end_line: u16,
    pub end_col: u16,
    pub is_block: bool,
}

/// Terminal metadata captured while the same lock protects the rows and
/// placement state in [`RenderState`].
#[derive(Debug)]
pub struct SurfaceSnapshot {
    pub modes: u32,
    /// Whether the terminal requested a blinking cursor. The effective
    /// visibility remains available through `RenderState::cursor_visible`.
    pub cursor_blinking: bool,
    pub title: String,
    pub working_dir: Option<String>,
    /// Total bytes in active Kitty images. Pixel data is intentionally absent
    /// from the metadata snapshot and can be copied later after a wire-size
    /// preflight.
    pub graphics_bytes: usize,
    /// Complete active graphics state. Delta captures leave this absent when
    /// graphics and dimensions are unchanged, because deltas carry no graphics.
    pub graphics: Option<GraphicsSnapshot>,
    /// Current retained Atlas keys are needed to reconcile the session's
    /// bounded pixel cache even when a delta does not copy active images.
    pub atlas_keys: Vec<u64>,
    /// Atlas uploads/removals drained while the terminal lock was held. The
    /// session snapshotter incorporates these into its retained asset store.
    pub graphics_updates: Option<UpdateQueues>,
    /// Atlas keys whose newest pixels were rejected by the bounded update
    /// store. Consumers must discard any older cached pixels for these keys.
    pub graphics_invalidated_keys: Vec<u64>,
    /// The invalidation set exceeded its own bound, so all retained Atlas
    /// pixels must be discarded before applying any accepted updates.
    pub graphics_invalidate_all: bool,
}

/// One drawable kitty item: a direct overlay placement, or one row-run
/// of U+10EEEE placeholder cells from a virtual placement (`U=1`, what
/// `kitten icat --transfer-mode` emits under multiplexers).
enum KittyEntry {
    Direct {
        placement: KittyPlacement,
        image_width: usize,
        image_height: usize,
    },
    Virtual {
        run: PlaceholderRun,
        line: usize,
        start_col: usize,
        placement: VirtualPlacement,
        image_width: usize,
        image_height: usize,
    },
}

impl KittyEntry {
    fn z_index(&self) -> i32 {
        match self {
            KittyEntry::Direct { placement, .. } => placement.z_index,
            KittyEntry::Virtual { placement, .. } => placement.z_index,
        }
    }
}

pub struct RenderState {
    terminal: Arc<FairMutex<Crosswords<Listener>>>,
    rows: Vec<Row<Square>>,
    /// Per-row resolved cell styles, index-parallel to `rows`.
    row_styles: Vec<Vec<Style>>,
    extras: FxHashMap<u16, Extras>,
    columns: usize,
    cursor_line: usize,
    cursor_column: usize,
    cursor_visible: bool,
    cursor_shape: CursorShape,
    /// Terminal palette (OSC-set / dynamic colors), snapshotted under
    /// the same lock as the grid so the GPU emit path resolves indexed
    /// and named colors against the exact frame it draws.
    term_colors: TermColors,
    display_offset: usize,
    selection: Option<ViewportSelection>,
    history_size: i64,
    lines_evicted: u64,
    alt_screen: bool,
    /// Kitty graphics placements (direct overlays and virtual
    /// placeholder runs), captured under the same lock as the grid
    /// snapshot and sorted by z-index, lowest first.
    kitty: Vec<KittyEntry>,
    /// Baseline for image change stamps (Instants aren't representable
    /// over the C ABI; nanoseconds relative to this are).
    epoch: rio_vt::time::Instant,
    /// Session snapshots do not use the renderer's resolved Kitty geometry.
    /// Avoid rebuilding that cache and scanning placeholder rows there.
    collect_kitty_geometry: bool,
}

impl RenderState {
    pub fn new(surface: &Surface) -> Self {
        Self::new_with_kitty_geometry(surface, true)
    }

    /// Build render state for a consumer that serializes terminal state rather
    /// than drawing Kitty overlays. The terminal graphics snapshot still owns
    /// all protocol-visible images and placements.
    pub fn new_for_snapshot(surface: &Surface) -> Self {
        Self::new_with_kitty_geometry(surface, false)
    }

    fn new_with_kitty_geometry(surface: &Surface, collect_kitty_geometry: bool) -> Self {
        let terminal = surface.terminal();
        let (columns, term_colors) = {
            let term = terminal.lock();
            (term.grid.columns(), *term.colors())
        };
        Self {
            terminal,
            rows: Vec::new(),
            row_styles: Vec::new(),
            extras: FxHashMap::default(),
            columns,
            cursor_line: 0,
            cursor_column: 0,
            cursor_visible: true,
            cursor_shape: CursorShape::Block,
            term_colors,
            display_offset: 0,
            selection: None,
            history_size: 0,
            lines_evicted: 0,
            alt_screen: false,
            kitty: Vec::new(),
            epoch: rio_vt::time::Instant::now(),
            collect_kitty_geometry,
        }
    }

    pub fn update(&mut self) {
        let terminal = Arc::clone(&self.terminal);
        let mut term = terminal.lock();
        self.update_locked(&mut term);
    }

    /// Update the render cache and capture all terminal-facing metadata under
    /// one terminal lock. Active image pixels are validated while the graphics
    /// snapshot is cloned, avoiding a second traversal of Kitty images.
    pub fn update_with_surface_state(
        &mut self,
        surface: &Surface,
        graphics_budget: usize,
        per_image_budget: usize,
        graphics_item_limit: usize,
    ) -> Result<SurfaceSnapshot, GraphicsSnapshotError> {
        self.update_with_surface_state_inner(
            surface,
            graphics_budget,
            per_image_budget,
            graphics_item_limit,
            true,
        )
    }

    /// Update render state for a delta publication. Active image pixels are
    /// copied only when graphics or dimensions changed; the caller still gets
    /// the current retained Atlas keys for cache reconciliation.
    pub fn update_with_surface_state_for_delta(
        &mut self,
        surface: &Surface,
        graphics_budget: usize,
        per_image_budget: usize,
        graphics_item_limit: usize,
    ) -> Result<SurfaceSnapshot, GraphicsSnapshotError> {
        self.update_with_surface_state_inner(
            surface,
            graphics_budget,
            per_image_budget,
            graphics_item_limit,
            false,
        )
    }

    fn update_with_surface_state_inner(
        &mut self,
        surface: &Surface,
        graphics_budget: usize,
        per_image_budget: usize,
        graphics_item_limit: usize,
        include_graphics: bool,
    ) -> Result<SurfaceSnapshot, GraphicsSnapshotError> {
        let terminal = Arc::clone(&self.terminal);
        let mut term = terminal.lock();
        let previous_columns = self.columns;
        let previous_lines = self.rows.len();
        self.update_locked(&mut term);
        let kitty_graphics_changed = term.graphics.kitty_graphics_dirty;
        let graphics_count = term
            .graphics
            .kitty_images
            .len()
            .checked_add(term.graphics.kitty_placements.len())
            .and_then(|count| {
                count.checked_add(term.graphics.kitty_virtual_placements.len())
            })
            .and_then(|count| count.checked_add(term.graphics.atlas_placements.len()))
            .ok_or(GraphicsSnapshotError {
                required_bytes: usize::MAX,
                limit_bytes: graphics_item_limit,
            })?;
        if graphics_count > graphics_item_limit {
            return Err(GraphicsSnapshotError {
                required_bytes: usize::MAX,
                limit_bytes: graphics_item_limit,
            });
        }
        let atlas_keys = crate::atlas_keys_locked(&term);
        if atlas_keys.len() > graphics_item_limit {
            return Err(GraphicsSnapshotError {
                required_bytes: usize::MAX,
                limit_bytes: graphics_item_limit,
            });
        }
        let mut graphics_store = surface.graphics_updates.lock().unwrap();
        let graphics_changed = kitty_graphics_changed || graphics_store.has_updates();
        let dimensions_changed =
            previous_columns != self.columns || previous_lines != self.rows.len();
        let (graphics_bytes, graphics) =
            if include_graphics || dimensions_changed || graphics_changed {
                let (graphics, graphics_bytes) = crate::graphics_snapshot_locked(
                    &term,
                    graphics_budget,
                    per_image_budget,
                )?;
                (graphics_bytes, Some(graphics))
            } else {
                (0, None)
            };
        // A rejected capture must leave uploads available for the next snapshot.
        let (graphics_updates, _, graphics_invalidated_keys, graphics_invalidate_all) =
            graphics_store.take_with_over_budget();
        drop(graphics_store);
        let working_dir = term
            .current_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .or_else(|| {
                #[cfg(all(feature = "pty", not(target_os = "windows")))]
                {
                    teletypewriter::foreground_process_path(
                        surface.main_fd,
                        surface.shell_pid,
                    )
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned())
                }
                #[cfg(any(not(feature = "pty"), target_os = "windows"))]
                {
                    let _ = surface;
                    None
                }
            });
        // Consume the flag only after complete graphics state and update
        // queues have been captured under the terminal lock.
        term.graphics.kitty_graphics_dirty = false;
        Ok(SurfaceSnapshot {
            modes: term.mode().bits(),
            cursor_blinking: term.blinking_cursor,
            title: term.title.clone(),
            working_dir,
            graphics_bytes,
            graphics,
            atlas_keys,
            graphics_updates,
            graphics_invalidated_keys,
            graphics_invalidate_all,
        })
    }

    /// Preserve a graphics resync requirement when publication or encoding
    /// fails after `update_with_surface_state` consumed the flag.
    pub fn restore_graphics_dirty(&self) {
        let mut term = self.terminal.lock();
        term.graphics.kitty_graphics_dirty = true;
    }

    fn update_locked(&mut self, term: &mut Crosswords<Listener>) {
        let viewport_changed = term.display_offset() != self.display_offset;
        let damage = if self.rows.is_empty() || viewport_changed {
            TerminalDamage::Full
        } else {
            match term.peek_damage_event() {
                Some(damage) => damage,
                None => TerminalDamage::Noop,
            }
        };
        term.snapshot_visible(
            &damage,
            &mut self.rows,
            &mut self.row_styles,
            &mut self.extras,
        );
        term.reset_damage();
        term.damage_event_in_flight = false;
        self.columns = term.grid.columns();
        self.display_offset = term.display_offset();
        // Kitty dest_rows live in absolute row space: rows evicted from
        // the ring still count (rio-vt anchors placements the same way),
        // so a full scrollback must not shift placements off-screen.
        self.lines_evicted = term.lines_evicted();
        self.history_size = self.lines_evicted as i64 + term.history_size() as i64;
        self.alt_screen = term.mode().contains(rio_vt::crosswords::Mode::ALT_SCREEN);
        if self.collect_kitty_geometry
            && (!matches!(damage, TerminalDamage::Noop)
                || term.graphics.kitty_graphics_dirty)
        {
            let mut kitty = std::mem::take(&mut self.kitty);
            kitty.clear();
            for placement in term.graphics.kitty_placements.values() {
                if let Some(image) = term.graphics.get_kitty_image(placement.image_id) {
                    kitty.push(KittyEntry::Direct {
                        placement: placement.clone(),
                        image_width: image.data.width,
                        image_height: image.data.height,
                    });
                }
            }
            self.collect_virtual_runs(term, &mut kitty);
            // Under-background placements (z < i32::MIN / 2) first, then
            // under-text (z < 0), then over-text: drawing in order layers
            // correctly, and the host can split the list at those bounds.
            kitty.sort_by_key(KittyEntry::z_index);
            self.kitty = kitty;
        }
        self.term_colors = *term.colors();
        let cursor = term.cursor();
        self.cursor_line = cursor.pos.row.0.max(0) as usize;
        self.cursor_column = cursor.pos.col.0;
        self.cursor_shape = cursor.content;
        // Hidden covers both DECTCEM (CSI ?25l) and a scrolled viewport;
        // renderers must not paint a cursor in either case.
        self.cursor_visible = cursor.content != CursorShape::Hidden;
        self.selection = term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(term))
            .and_then(|range| {
                let offset = term.display_offset() as i32;
                let lines = self.rows.len() as i32;
                let start = range.start.row.0 + offset;
                let end = range.end.row.0 + offset;
                if end < 0 || start >= lines {
                    return None;
                }
                let clamped_start = start.max(0);
                let clamped_end = end.min(lines - 1);
                Some(ViewportSelection {
                    start_line: clamped_start as u16,
                    start_col: if start < 0 {
                        0
                    } else {
                        range.start.col.0 as u16
                    },
                    end_line: clamped_end as u16,
                    end_col: if end >= lines {
                        (self.columns.saturating_sub(1)) as u16
                    } else {
                        range.end.col.0 as u16
                    },
                    is_block: range.is_block,
                })
            });
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn lines(&self) -> usize {
        self.rows.len()
    }

    pub fn row_dirty(&self, line: usize) -> bool {
        self.rows.get(line).map(|row| row.dirty).unwrap_or(false)
    }

    pub fn reset_dirty(&mut self) {
        for row in &mut self.rows {
            row.dirty = false;
        }
    }

    /// The codepoints attached to a square's grapheme cluster
    /// (combining marks, or a full mode-2027 cluster tail) when it
    /// carries any. Bg-only squares reuse the extras-id bits for
    /// color and never report a cluster.
    pub fn cluster_of(&self, square: &Square) -> Option<&[char]> {
        if !square.has_grapheme()
            || !matches!(
                square.content_tag(),
                rio_vt::crosswords::square::ContentTag::Codepoint
            )
        {
            return None;
        }
        let extras = square.extras_id().and_then(|eid| self.extras.get(&eid))?;
        if extras.zerowidth.is_empty() {
            None
        } else {
            Some(&extras.zerowidth)
        }
    }

    /// The cell's full text (base codepoint plus attached cluster
    /// codepoints), or `None` for cells with no attachments.
    pub fn cell_cluster_text(&self, line: usize, column: usize) -> Option<String> {
        let square = self.square(line, column)?;
        let cluster = self.cluster_of(square)?;
        let mut text = String::with_capacity(4 * (1 + cluster.len()));
        text.push(square.c());
        text.extend(cluster.iter());
        Some(text)
    }

    pub fn square(&self, line: usize, column: usize) -> Option<&Square> {
        let row = self.rows.get(line)?;
        if column >= self.columns {
            return None;
        }
        Some(&row[Column(column)])
    }

    pub fn style_at(&self, line: usize, column: usize, square: &Square) -> Style {
        // Bg-only cells (erase fills, blank lines after `clear`) encode
        // their background inline instead of carrying a style id; the
        // resolved per-row styles hold the default for them.
        match square.content_tag() {
            ContentTag::Codepoint => self
                .row_styles(line)
                .get(column)
                .copied()
                .unwrap_or_default(),
            ContentTag::BgPalette => Style {
                bg: AnsiColor::Indexed(square.bg_palette_index()),
                ..Style::default()
            },
            ContentTag::BgRgb => {
                let (r, g, b) = square.bg_rgb();
                Style {
                    bg: AnsiColor::Spec(ColorRgb { r, g, b }),
                    ..Style::default()
                }
            }
        }
    }

    /// Resolved styles for one visible row, index-parallel to its
    /// cells. Empty for an out-of-range line.
    pub fn row_styles(&self, line: usize) -> &[Style] {
        self.row_styles.get(line).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The snapshot's visible rows. Index-parallel to the resolved
    /// per-row styles served by `style_at` / `row_styles`, and to
    /// `extras()`: the GPU emit path (rio-grid) walks these.
    pub fn rows(&self) -> &[Row<Square>] {
        &self.rows
    }

    /// Per-frame extras (grapheme clusters, hyperlinks), keyed by a
    /// cell's `extras_id`.
    pub fn extras(&self) -> &FxHashMap<u16, Extras> {
        &self.extras
    }

    /// Terminal palette captured with this frame's grid snapshot.
    pub fn term_colors(&self) -> &TermColors {
        &self.term_colors
    }

    /// The terminal-side configured cursor shape (block / underline /
    /// beam / hidden) for this frame.
    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_line, self.cursor_column)
    }

    /// False when the program hid the cursor (DECTCEM, `CSI ?25l`) or the
    /// view is scrolled into history; renderers skip painting it then.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    pub fn history_size(&self) -> usize {
        self.history_size
            .saturating_sub(self.lines_evicted as i64)
            .max(0) as usize
    }

    pub fn lines_evicted(&self) -> u64 {
        self.lines_evicted
    }

    /// Whether the alternate screen (full-screen TUIs) is active. Hosts
    /// use it to gate prompt-output affordances like table skins.
    pub fn alt_screen(&self) -> bool {
        self.alt_screen
    }

    pub fn selection(&self) -> Option<ViewportSelection> {
        self.selection
    }

    pub fn kitty_count(&self) -> usize {
        self.kitty.len()
    }

    /// Scan the snapshot rows for U+10EEEE placeholder cells and collapse
    /// them into row-runs (kitty virtual placements). One entry per run,
    /// resolved against the virtual placement registry and image store.
    /// The walk mirrors rioterm's renderer: a cell with missing
    /// diacritics inherits from its left neighbour, and consecutive cells
    /// showing sequential image columns collapse into one run.
    fn collect_virtual_runs(
        &self,
        term: &Crosswords<Listener>,
        entries: &mut Vec<KittyEntry>,
    ) {
        let graphics = &term.graphics;
        if graphics.kitty_virtual_placements.is_empty() {
            return;
        }

        let flush = |entries: &mut Vec<KittyEntry>,
                     run: PlaceholderRun,
                     line: usize,
                     start_col: usize| {
            let placement = resolve_virtual_placement(
                &graphics.kitty_virtual_placements,
                run.image_id,
                run.placement_id,
            );
            let Some(placement) = placement else { return };
            let Some(image) = graphics.get_kitty_image(run.image_id) else {
                return;
            };
            entries.push(KittyEntry::Virtual {
                run,
                line,
                start_col,
                placement: placement.clone(),
                image_width: image.data.width,
                image_height: image.data.height,
            });
        };

        for (line, row) in self.rows.iter().enumerate() {
            if !row.kitty_virtual_placeholder {
                continue;
            }
            let mut run: Option<(IncompletePlacement, usize)> = None;
            for (col, square) in row.inner.iter().enumerate() {
                if square.c() != PLACEHOLDER {
                    if let Some((p, start_col)) = run.take() {
                        flush(entries, p.complete(), line, start_col);
                    }
                    continue;
                }

                let style = self.style_at(line, col, square);
                let combining: &[char] = square
                    .extras_id()
                    .and_then(|eid| self.extras.get(&eid))
                    .map(|extras| extras.zerowidth.as_slice())
                    .unwrap_or(&[]);
                let mut cell = IncompletePlacement::from_cell(
                    style.fg,
                    style.underline_color,
                    combining,
                );

                match &mut run {
                    Some((current, _)) if current.can_append(&cell) => {
                        current.append();
                    }
                    _ => {
                        if let Some((p, start_col)) = run.take() {
                            flush(entries, p.complete(), line, start_col);
                        }
                        // Default missing row/col on the FIRST cell of a
                        // run, so a later cell with an explicit column can
                        // still extend it sequentially.
                        if cell.row.is_none() {
                            cell.row = Some(0);
                        }
                        if cell.col.is_none() {
                            cell.col = Some(0);
                        }
                        run = Some((cell, col));
                    }
                }
            }
            if let Some((p, start_col)) = run {
                flush(entries, p.complete(), line, start_col);
            }
        }
    }

    /// Viewport geometry for the placement at `index`, in pixels relative
    /// to the grid origin. `None` when it's scrolled out of view.
    pub fn kitty_geometry(
        &self,
        index: usize,
        cell_width: f32,
        cell_height: f32,
    ) -> Option<(u32, i32, KittyOverlayGeometry)> {
        match self.kitty.get(index)? {
            KittyEntry::Direct {
                placement,
                image_width,
                image_height,
            } => {
                let viewport = OverlayViewport {
                    cell_width,
                    cell_height,
                    origin_x: 0.0,
                    origin_y: 0.0,
                    history_size: self.history_size,
                    display_offset: self.display_offset as i64,
                    screen_lines: self.rows.len() as i64,
                };
                let geometry = kitty_overlay_geometry(
                    placement,
                    *image_width,
                    *image_height,
                    &viewport,
                )?;
                Some((placement.image_id, placement.z_index, geometry))
            }
            KittyEntry::Virtual {
                run,
                line,
                start_col,
                placement,
                image_width,
                image_height,
            } => {
                let geometry = compute_run_geometry(
                    run,
                    placement.columns,
                    placement.rows,
                    *image_width as u32,
                    *image_height as u32,
                    (placement.x, placement.y, placement.width, placement.height),
                    cell_width,
                    cell_height,
                    0.0,
                    0.0,
                    *line,
                    *start_col,
                )?;
                Some((
                    run.image_id,
                    placement.z_index,
                    KittyOverlayGeometry {
                        x: geometry.x + placement.cell_x_offset as f32,
                        y: geometry.y + placement.cell_y_offset as f32,
                        width: geometry.width,
                        height: geometry.height,
                        source_rect: geometry.source_rect,
                    },
                ))
            }
        }
    }

    /// Pixel dimensions and a change stamp for a kitty image. The stamp
    /// changes on retransmission, so renderers can cache decoded bitmaps.
    pub fn kitty_image_info(&self, image_id: u32) -> Option<(usize, usize, u64)> {
        let term = self.terminal.lock();
        let image = term.graphics.get_kitty_image(image_id)?;
        let stamp = match image.transmission_time.checked_duration_since(self.epoch) {
            Some(duration) => duration.as_nanos() as u64,
            None => {
                u64::MAX
                    - self
                        .epoch
                        .duration_since(image.transmission_time)
                        .as_nanos() as u64
            }
        };
        Some((image.data.width, image.data.height, stamp))
    }

    /// Copy a kitty image into `buf` as RGBA8 (RGB sources gain an opaque
    /// alpha). Returns bytes written; 0 when unknown or `buf` is too small.
    pub fn kitty_image_rgba(&self, image_id: u32, buf: &mut [u8]) -> usize {
        let term = self.terminal.lock();
        let Some(image) = term.graphics.get_kitty_image(image_id) else {
            return 0;
        };
        let width = image.data.width;
        let height = image.data.height;
        let expected = width * height * 4;
        if buf.len() < expected {
            return 0;
        }
        use rio_graphics::ColorType;
        match image.data.color_type {
            ColorType::Rgba => {
                if image.data.pixels.len() < expected {
                    return 0;
                }
                buf[..expected].copy_from_slice(&image.data.pixels[..expected]);
            }
            ColorType::Rgb => {
                let source = width * height * 3;
                if image.data.pixels.len() < source {
                    return 0;
                }
                for i in 0..width * height {
                    buf[i * 4] = image.data.pixels[i * 3];
                    buf[i * 4 + 1] = image.data.pixels[i * 3 + 1];
                    buf[i * 4 + 2] = image.data.pixels[i * 3 + 2];
                    buf[i * 4 + 3] = 0xFF;
                }
            }
        }
        expected
    }

    /// The OSC 8 hyperlink under a viewport cell, if any.
    pub fn link_at(&self, line: usize, column: usize) -> Option<&str> {
        let square = self.square(line, column)?;
        let eid = square.extras_id_checked()?;
        self.extras
            .get(&eid)?
            .hyperlink
            .as_ref()
            .map(|link| link.uri())
    }

    /// The contiguous run of cells on `line` carrying the hyperlink under
    /// `column`: what a renderer underlines on hover. Runs are per-row on
    /// purpose; a link that wraps is two runs sharing one URI.
    pub fn link_run(&self, line: usize, column: usize) -> Option<(usize, usize)> {
        let uri = self.link_at(line, column)?;
        let mut start = column;
        while start > 0 && self.link_at(line, start - 1) == Some(uri) {
            start -= 1;
        }
        let mut end = column;
        while end + 1 < self.columns() && self.link_at(line, end + 1) == Some(uri) {
            end += 1;
        }
        Some((start, end))
    }

    pub fn text_row(&self, line: usize) -> String {
        let Some(row) = self.rows.get(line) else {
            return String::new();
        };
        let mut text = String::with_capacity(self.columns);
        for column in 0..self.columns {
            let square = row[Column(column)];
            // Bg-only and never-written cells have no codepoint; save them
            // as spaces so scrollback text doesn't accumulate NULs.
            let c = if square.is_bg_only() { ' ' } else { square.c() };
            text.push(if c == '\0' { ' ' } else { c });
        }
        text.trim_end().to_string()
    }
}

#[cfg(all(test, feature = "pty", not(target_os = "windows")))]
mod tests {
    use super::*;
    use crate::{Engine, SurfaceDelegate, SurfaceDesc, SurfaceId};
    use std::sync::Arc;

    struct NoopDelegate;

    impl SurfaceDelegate for NoopDelegate {
        fn wakeup(&self, _surface: SurfaceId) {}
    }

    fn quiet_surface() -> Surface {
        // Tests inject terminal output directly; login scripts must not mutate it.
        Engine::new(Arc::new(NoopDelegate))
            .create_surface(&SurfaceDesc {
                shell: Some("/bin/sh".into()),
                args: vec!["-c".into(), "read -r _".into()],
                clear_environment: true,
                ..SurfaceDesc::default()
            })
            .expect("spawn quiet shell")
    }

    #[cfg(feature = "graphics")]
    #[test]
    fn rejected_kitty_snapshot_preserves_pending_atlas_pixels() {
        let surface = quiet_surface();
        let mut state = RenderState::new(&surface);
        surface.inject_output(b"\x1bPq\"1;1;1;1#0;2;100;0;0#0~\x1b\\");
        surface.inject_output(b"\x1b_Gf=32,s=1,v=1,i=9;/////w==\x1b\\");

        assert!(state
            .update_with_surface_state(&surface, 0, usize::MAX, usize::MAX)
            .is_err());
        let recovered = state
            .update_with_surface_state(&surface, usize::MAX, usize::MAX, usize::MAX)
            .unwrap();
        let updates = recovered.graphics_updates.expect("pending Sixel upload");
        assert_eq!(updates.pending.len(), 1);
        assert!(!updates.pending[0].pixels.is_empty());
        assert_eq!(recovered.graphics.unwrap().kitty_images.len(), 1);
    }

    #[test]
    fn noop_snapshot_refreshes_a_dirty_row_after_damage_is_consumed() {
        let surface = quiet_surface();
        let mut state = RenderState::new(&surface);

        surface.inject_output(b"before\r\nkeep\x1b]2;dirty-row\x07");
        let initial = state
            .update_with_surface_state(&surface, usize::MAX, usize::MAX, usize::MAX)
            .expect("capture initial state");
        assert_eq!(initial.title, "dirty-row");
        assert_eq!(state.text_row(0), "before");
        assert_eq!(state.text_row(1), "keep");
        let initial_cursor = state.cursor();
        state.reset_dirty();

        surface.inject_output(b"\x1b[1;1H\x1b[2Kafter");
        {
            let mut term = state.terminal.lock();
            assert!(term.peek_damage_event().is_some());
            term.reset_damage();
            assert!(term.peek_damage_event().is_none());
        }

        let snapshot = state
            .update_with_surface_state_for_delta(
                &surface,
                usize::MAX,
                usize::MAX,
                usize::MAX,
            )
            .expect("capture dirty row after damage was consumed");
        assert_eq!(snapshot.title, "dirty-row");
        assert_eq!(initial_cursor, (1, 4));
        assert_eq!(state.cursor(), (0, 5));
        assert_eq!(state.text_row(0), "after");
        assert_eq!(state.text_row(1), "keep");
        assert!(state.rows()[0].inner.iter().any(|square| square.c() == 'a'));
        assert!(state.row_dirty(0));
        assert!(!state.row_dirty(1));
    }

    #[cfg(feature = "graphics")]
    #[test]
    fn snapshot_state_skips_renderer_kitty_geometry() {
        let surface = quiet_surface();
        let mut state = RenderState::new_for_snapshot(&surface);
        surface.inject_output(
            b"\x1b[6;5H\x1b_Gf=32,s=2,v=2,i=7,a=T;/wAA/wD/AP8AAP///////w==\x1b\\",
        );

        let snapshot = state
            .update_with_surface_state(&surface, usize::MAX, usize::MAX, usize::MAX)
            .expect("capture snapshot graphics");
        assert!(state.kitty.is_empty());
        assert_eq!(snapshot.graphics.unwrap().kitty_placements.len(), 1);
    }
}
