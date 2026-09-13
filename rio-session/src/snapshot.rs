use crate::protocol::{
    AtlasPlacementFrame, CellContentFrame, CellFrame, ColorFrame, CursorFrame,
    ExtrasFrame, FrameDelta, FrameUpdate, FullFrame, GraphicFrame, GraphicsFrame,
    KittyPlacementFrame, RowFrame, RowUpdate, SelectionFrame, StyleFrame,
    VirtualPlacementFrame, MAX_FRAME_GRAPHICS_BYTES, MAX_FRAME_SIZE, MAX_GRAPHICS_ITEMS,
    MAX_IMAGE_BYTES,
};
use crate::SessionError;
use librio::{GraphicsSnapshot, RenderState, Surface};
use rio_graphics::{atlas_image_key, kitty_image_key, ColorType, GraphicData};
use rio_vt::ansi::graphics::UpdateQueues;
use rio_vt::ansi::CursorShape;
use rio_vt::config::colors::AnsiColor;
use std::collections::HashMap;

fn check_image_size(size: usize) -> Result<(), SessionError> {
    if size > MAX_IMAGE_BYTES {
        return Err(SessionError::unsupported(
            "graphic image exceeds the supported snapshot image budget",
        ));
    }
    Ok(())
}

/// Converts the in-process render state into the stable wire representation.
/// The worker owns this object; the GUI never receives `Crosswords` or a
/// parser replica.
pub(crate) struct Snapshotter {
    render_state: RenderState,
    atlas_images: HashMap<u64, GraphicData>,
    published_atlas_keys: Vec<u64>,
    sequence: u64,
}

impl Snapshotter {
    pub(crate) fn new(surface: &Surface) -> Self {
        Self {
            render_state: RenderState::new_for_snapshot(surface),
            atlas_images: HashMap::new(),
            published_atlas_keys: Vec::new(),
            sequence: 0,
        }
    }

    pub(crate) fn full_frame(
        &mut self,
        surface: &Surface,
    ) -> Result<FullFrame, SessionError> {
        let result = self
            .capture(surface, true)
            .and_then(|snapshot| self.full_frame_from_snapshot(snapshot));
        self.restore_graphics_dirty_on_failure(result)
    }

    pub(crate) fn snapshot_since(
        &mut self,
        surface: &Surface,
        base_sequence: u64,
    ) -> Result<FrameUpdate, SessionError> {
        if base_sequence == 0 {
            return Err(SessionError::invalid(
                "snapshot base sequence must be non-zero",
            ));
        }
        if base_sequence != self.sequence {
            return self.full_frame(surface).map(FrameUpdate::Full);
        }

        let result = self.capture(surface, false).and_then(|snapshot| {
            if snapshot.graphics.is_some() {
                self.full_frame_from_snapshot(snapshot)
                    .map(FrameUpdate::Full)
            } else {
                self.delta_from_snapshot(snapshot, base_sequence)
                    .map(FrameUpdate::Delta)
            }
        });
        self.restore_graphics_dirty_on_failure(result)
    }

    fn restore_graphics_dirty_on_failure<T>(
        &self,
        result: Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        if result.is_err() {
            // The next capture must include full graphics after a failed
            // publication; dirty rows are retained until publication succeeds.
            self.render_state.restore_graphics_dirty();
        }
        result
    }

    fn dimensions(&self) -> Result<(u16, u16), SessionError> {
        let columns = self.render_state.columns().try_into().map_err(|_| {
            SessionError::invalid("snapshot columns exceed protocol range")
        })?;
        let lines =
            self.render_state.lines().try_into().map_err(|_| {
                SessionError::invalid("snapshot lines exceed protocol range")
            })?;
        Ok((columns, lines))
    }

    fn cursor_frame(&self, blinking: bool) -> Result<CursorFrame, SessionError> {
        let (line, column) = self.render_state.cursor();
        Ok(CursorFrame {
            line: line.try_into().map_err(|_| {
                SessionError::invalid("cursor line exceeds protocol range")
            })?,
            column: column.try_into().map_err(|_| {
                SessionError::invalid("cursor column exceeds protocol range")
            })?,
            visible: self.render_state.cursor_visible(),
            blinking,
            shape: cursor_shape(self.render_state.cursor_shape()),
        })
    }

    fn selection_frame(&self) -> Option<SelectionFrame> {
        self.render_state
            .selection()
            .map(|selection| SelectionFrame {
                start_line: selection.start_line,
                start_column: selection.start_col,
                end_line: selection.end_line,
                end_column: selection.end_col,
                block: selection.is_block,
            })
    }

    fn colors(&self) -> Vec<Option<[f32; 4]>> {
        (0..269)
            .map(|index| self.render_state.term_colors()[index])
            .collect()
    }

    fn capture(
        &mut self,
        surface: &Surface,
        include_graphics: bool,
    ) -> Result<librio::SurfaceSnapshot, SessionError> {
        let mut snapshot = if include_graphics {
            self.render_state.update_with_surface_state(
                surface,
                MAX_FRAME_SIZE,
                crate::protocol::MAX_IMAGE_BYTES,
                MAX_GRAPHICS_ITEMS,
            )
        } else {
            self.render_state.update_with_surface_state_for_delta(
                surface,
                MAX_FRAME_SIZE,
                crate::protocol::MAX_IMAGE_BYTES,
                MAX_GRAPHICS_ITEMS,
            )
        }
        .map_err(|error| {
            SessionError::unsupported(format!(
                "terminal graphics snapshot rejected: {error}"
            ))
        })?;
        let retained_bytes = self.remember_graphics_updates(
            snapshot.graphics_updates.take(),
            &snapshot.graphics_invalidated_keys,
            snapshot.graphics_invalidate_all,
            &snapshot.atlas_keys,
        )?;
        if retained_bytes.saturating_add(snapshot.graphics_bytes)
            > MAX_FRAME_GRAPHICS_BYTES
        {
            return Err(SessionError::unsupported(
                "terminal graphics exceed the session frame budget",
            ));
        }
        Ok(snapshot)
    }

    fn full_frame_from_snapshot(
        &mut self,
        snapshot: librio::SurfaceSnapshot,
    ) -> Result<FullFrame, SessionError> {
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| SessionError::unsupported("snapshot sequence exhausted"))?;

        let rows = self.rows();

        let cursor = self.cursor_frame(snapshot.cursor_blinking)?;
        let selection = self.selection_frame();
        let (columns, lines) = self.dimensions()?;

        let mut frame = FullFrame {
            sequence,
            columns,
            lines,
            rows,
            display_offset: self.render_state.display_offset().try_into().map_err(
                |_| SessionError::invalid("display offset exceeds protocol range"),
            )?,
            history_size: self.render_state.history_size().try_into().map_err(|_| {
                SessionError::invalid("history size exceeds protocol range")
            })?,
            lines_evicted: self.render_state.lines_evicted(),
            alternate_screen: self.render_state.alt_screen(),
            modes: snapshot.modes,
            cursor,
            selection,
            colors: self.colors(),
            graphics: GraphicsFrame::default(),
            title: snapshot.title,
            working_dir: snapshot.working_dir,
        };

        let retained_keys = snapshot.atlas_keys;
        let graphics = snapshot.graphics.ok_or_else(|| {
            SessionError::protocol("full snapshot is missing graphics state")
        })?;
        frame.graphics = self.graphics_frame(graphics, &retained_keys)?;
        frame.validate()?;
        validate_encoded_size(
            &frame,
            "structured terminal snapshot exceeds frame budget",
        )?;
        self.published_atlas_keys = retained_keys;
        self.sequence = sequence;
        self.render_state.reset_dirty();
        Ok(frame)
    }

    fn delta_from_snapshot(
        &mut self,
        snapshot: librio::SurfaceSnapshot,
        base_sequence: u64,
    ) -> Result<FrameDelta, SessionError> {
        let sequence = base_sequence
            .checked_add(1)
            .ok_or_else(|| SessionError::unsupported("snapshot sequence exhausted"))?;
        let (columns, lines) = self.dimensions()?;
        let rows = self
            .render_state
            .rows()
            .iter()
            .enumerate()
            .filter(|(line, _)| self.render_state.row_dirty(*line))
            .map(|(line, _)| {
                Ok(RowUpdate {
                    line: line.try_into().map_err(|_| {
                        SessionError::invalid("row index exceeds protocol range")
                    })?,
                    row: self.row(line),
                })
            })
            .collect::<Result<Vec<_>, SessionError>>()?;
        let cursor = self.cursor_frame(snapshot.cursor_blinking)?;
        let selection = self.selection_frame();
        let delta = FrameDelta {
            base_sequence,
            sequence,
            columns,
            lines,
            rows,
            display_offset: self.render_state.display_offset().try_into().map_err(
                |_| SessionError::invalid("display offset exceeds protocol range"),
            )?,
            history_size: self.render_state.history_size().try_into().map_err(|_| {
                SessionError::invalid("history size exceeds protocol range")
            })?,
            lines_evicted: self.render_state.lines_evicted(),
            alternate_screen: self.render_state.alt_screen(),
            modes: snapshot.modes,
            cursor,
            selection,
            colors: self.colors(),
            title: snapshot.title,
            working_dir: snapshot.working_dir,
        };
        delta.validate()?;
        validate_encoded_size(&delta, "structured terminal delta exceeds frame budget")?;
        self.sequence = sequence;
        self.render_state.reset_dirty();
        Ok(delta)
    }

    fn rows(&self) -> Vec<RowFrame> {
        (0..self.render_state.rows().len())
            .map(|line| self.row(line))
            .collect()
    }

    fn row(&self, line: usize) -> RowFrame {
        let row = &self.render_state.rows()[line];
        let cells = row.inner.iter().map(cell_frame).collect();
        let styles = self
            .render_state
            .row_styles(line)
            .iter()
            .map(style_frame)
            .collect();
        let extras = row
            .inner
            .iter()
            .map(|square| {
                square
                    .extras_id_checked()
                    .and_then(|id| self.render_state.extras().get(&id))
                    .map(|extras| ExtrasFrame {
                        zero_width: extras.zerowidth.iter().map(|c| *c as u32).collect(),
                        hyperlink: extras
                            .hyperlink
                            .as_ref()
                            .map(|hyperlink| hyperlink.uri().to_string()),
                    })
            })
            .collect();
        RowFrame {
            cells,
            styles,
            extras,
            kitty_virtual_placeholder: row.kitty_virtual_placeholder,
            text: self.render_state.text_row(line),
        }
    }

    fn remember_graphics_updates(
        &mut self,
        updates: Option<UpdateQueues>,
        invalidated_keys: &[u64],
        invalidate_all: bool,
        retained_keys: &[u64],
    ) -> Result<usize, SessionError> {
        if retained_keys.len() > MAX_GRAPHICS_ITEMS {
            return Err(SessionError::unsupported(
                "retained Atlas image count exceeds the session limit",
            ));
        }
        if invalidate_all {
            self.atlas_images.clear();
        } else {
            for key in invalidated_keys {
                self.atlas_images.remove(key);
            }
        }
        self.atlas_images
            .retain(|key, _| retained_keys.binary_search(key).is_ok());
        if let Some(updates) = updates {
            for graphic in updates.pending {
                let key = atlas_image_key(graphic.id.get());
                if retained_keys.binary_search(&key).is_err() {
                    continue;
                }
                if check_image_size(graphic.pixels.len()).is_ok() {
                    self.atlas_images.insert(key, graphic);
                } else {
                    // Never let a rejected replacement reuse stale cached pixels.
                    self.atlas_images.remove(&key);
                }
            }
        }
        // The map is a subset of `retained_keys` after the retain/update steps,
        // so equal lengths prove that every retained key has pixels.
        if self.atlas_images.len() != retained_keys.len() {
            return Err(SessionError::unsupported("retained Atlas image pixels are missing or exceed the session budget; clear the affected image to resume snapshots"));
        }
        self.atlas_images
            .values()
            .try_fold(0usize, |bytes, graphic| {
                bytes.checked_add(graphic.pixels.len()).ok_or_else(|| {
                    SessionError::unsupported("retained graphics size overflow")
                })
            })
    }

    fn graphics_frame(
        &self,
        snapshot: GraphicsSnapshot,
        retained_keys: &[u64],
    ) -> Result<GraphicsFrame, SessionError> {
        let image_capacity = snapshot
            .kitty_images
            .len()
            .checked_add(self.atlas_images.len())
            .ok_or_else(|| {
                SessionError::unsupported("snapshot graphics count overflow")
            })?;
        if image_capacity > MAX_GRAPHICS_ITEMS
            || snapshot.kitty_placements.len() > MAX_GRAPHICS_ITEMS
            || snapshot.kitty_virtual_placements.len() > MAX_GRAPHICS_ITEMS
            || snapshot.atlas_placements.len() > MAX_GRAPHICS_ITEMS
        {
            return Err(SessionError::unsupported(
                "snapshot graphics item count exceeds the session limit",
            ));
        }
        let mut images = Vec::with_capacity(image_capacity);
        for (image_id, graphic) in snapshot.kitty_images {
            images.push(graphic_frame(0, kitty_image_key(image_id), graphic)?);
        }
        for (key, graphic) in &self.atlas_images {
            images.push(graphic_frame(1, *key, graphic.clone())?);
        }
        images.sort_by_key(|image| (image.kind, image.key));

        let mut kitty_placements = snapshot
            .kitty_placements
            .iter()
            .map(|(_, placement)| {
                Ok(KittyPlacementFrame {
                    image_id: placement.image_id,
                    placement_id: placement.placement_id,
                    source: [
                        placement.source_x,
                        placement.source_y,
                        placement.source_width,
                        placement.source_height,
                    ],
                    dest_col: placement.dest_col.try_into().map_err(|_| {
                        SessionError::invalid("kitty column exceeds protocol range")
                    })?,
                    dest_row: placement.dest_row,
                    columns: placement.columns,
                    rows: placement.rows,
                    requested_columns: placement.requested_columns,
                    requested_rows: placement.requested_rows,
                    cell_offset: [placement.cell_x_offset, placement.cell_y_offset],
                    z_index: placement.z_index,
                })
            })
            .collect::<Result<Vec<_>, SessionError>>()?;
        kitty_placements
            .sort_by_key(|placement| (placement.image_id, placement.placement_id));
        let mut virtual_placements = snapshot
            .kitty_virtual_placements
            .iter()
            .map(|(_, placement)| VirtualPlacementFrame {
                image_id: placement.image_id,
                placement_id: placement.placement_id,
                columns: placement.columns,
                rows: placement.rows,
                source: [placement.x, placement.y, placement.width, placement.height],
                cell_offset: [placement.cell_x_offset, placement.cell_y_offset],
                z_index: placement.z_index,
            })
            .collect::<Vec<_>>();
        virtual_placements
            .sort_by_key(|placement| (placement.image_id, placement.placement_id));
        let mut atlas_placements = snapshot
            .atlas_placements
            .iter()
            .map(|placement| {
                Ok(AtlasPlacementFrame {
                    key: placement.image_key,
                    row: placement.abs_row,
                    column: placement.col.try_into().map_err(|_| {
                        SessionError::invalid("atlas column exceeds protocol range")
                    })?,
                    columns: placement.columns.try_into().map_err(|_| {
                        SessionError::invalid("atlas columns exceed protocol range")
                    })?,
                    rows: placement.rows.try_into().map_err(|_| {
                        SessionError::invalid("atlas rows exceed protocol range")
                    })?,
                    source: [
                        placement.src_x,
                        placement.src_y,
                        placement.src_width,
                        placement.src_height,
                    ],
                    image_width: placement.total_width,
                    image_height: placement.total_height,
                    cell_width: placement.insert_cell_w.into(),
                    cell_height: placement.insert_cell_h.into(),
                })
            })
            .collect::<Result<Vec<_>, SessionError>>()?;
        atlas_placements
            .sort_by_key(|placement| (placement.key, placement.row, placement.column));

        let removed_keys = self
            .published_atlas_keys
            .iter()
            .filter(|key| retained_keys.binary_search(key).is_err())
            .copied()
            .collect::<Vec<_>>();

        Ok(GraphicsFrame {
            images,
            kitty_placements,
            virtual_placements,
            atlas_placements,
            // Hints cover only published images; transient removals need no state.
            removed_keys,
        })
    }
}

fn cell_frame(square: &rio_vt::crosswords::square::Square) -> CellFrame {
    use rio_vt::crosswords::square::{CellFlags, ContentTag};

    let content = match square.content_tag() {
        ContentTag::Codepoint => CellContentFrame::Codepoint(square.c() as u32),
        ContentTag::BgPalette => CellContentFrame::Palette(square.bg_palette_index()),
        ContentTag::BgRgb => {
            let (r, g, b) = square.bg_rgb();
            CellContentFrame::Rgb { r, g, b }
        }
    };
    CellFrame {
        content,
        wide: square.wide() as u8,
        flags: square.cell_flags().intersection(CellFlags::all()).bits(),
    }
}

fn graphic_frame(
    kind: u8,
    key: u64,
    graphic: GraphicData,
) -> Result<GraphicFrame, SessionError> {
    Ok(GraphicFrame {
        kind,
        key,
        width: graphic
            .width
            .try_into()
            .map_err(|_| SessionError::invalid("graphic width is too large"))?,
        height: graphic
            .height
            .try_into()
            .map_err(|_| SessionError::invalid("graphic height is too large"))?,
        color_type: match graphic.color_type {
            ColorType::Rgb => 0,
            ColorType::Rgba => 1,
        },
        pixels: graphic.pixels,
        opacity: graphic.is_opaque,
        display_width: graphic
            .display_width
            .map(u32::try_from)
            .transpose()
            .map_err(|_| SessionError::invalid("graphic display width is too large"))?,
        display_height: graphic
            .display_height
            .map(u32::try_from)
            .transpose()
            .map_err(|_| SessionError::invalid("graphic display height is too large"))?,
    })
}

fn validate_encoded_size<T: bincode::Encode>(
    value: &T,
    message: &'static str,
) -> Result<(), SessionError> {
    crate::codec::encoded_size(value)
        .map(|_| ())
        .map_err(|error| match error {
            SessionError::Codec(_) | SessionError::Protocol(_) => {
                SessionError::unsupported(message)
            }
            error => error,
        })
}

fn style_frame(style: &rio_vt::crosswords::style::Style) -> StyleFrame {
    StyleFrame {
        foreground: color_frame(style.fg),
        background: color_frame(style.bg),
        underline: style.underline_color.map(color_frame),
        flags: style.flags.bits(),
    }
}

fn color_frame(color: AnsiColor) -> ColorFrame {
    match color {
        AnsiColor::Named(name) => ColorFrame::Named(name as u16),
        AnsiColor::Indexed(index) => ColorFrame::Indexed(index),
        AnsiColor::Spec(rgb) => ColorFrame::Rgb {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        },
    }
}

fn cursor_shape(shape: CursorShape) -> u8 {
    match shape {
        CursorShape::Block => 0,
        CursorShape::Underline => 1,
        CursorShape::Beam => 2,
        CursorShape::Hidden => 3,
    }
}
