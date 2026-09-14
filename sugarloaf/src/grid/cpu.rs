// Copyright (c) 2023-present, Raphael Amorim.
//
// CPU backend for the grid renderer.
//
// Mirrors the GPU backends (`metal`, `vulkan`, `webgpu`) in scope and
// data layout, but rasterises directly into a caller-owned u32 pixel
// buffer instead of recording draw calls. Pixels use logical 0xAARRGGBB
// values (little-endian BGRA8 premultiplied bytes). Storage matches the Metal
// renderer cell-for-cell: one flat `Vec<CellBg>` indexed by
// `row * cols + col`, plus per-row `Vec<CellText>` slots (slot 0 =
// block-cursor cells, 1..=rows = content rows, last = non-block
// cursor) so the row-rebuild path in `frontends/rioterm/src/grid_emit`
// stays backend-agnostic.
//
// The atlases live in RAM. `CpuGridAtlas` packs glyph bitmaps into a
// shelf-allocated grayscale (R8) or color (RGBA8 premul) buffer with
// the same `AtlasAllocator` the GPU paths use, so the atlas slot
// coordinates round-trip across backends.
//
// Position math in `render` mirrors `grid.metal`'s vertex shaders so
// glyph placement matches the GPU paths pixel-for-pixel:
//   - cell origin = (col * cell_w + grid_padding.left,
//                    row * cell_h + grid_padding.top)
//   - glyph origin = cell_origin + (bearing_x, cell_h - bearing_y)

use rustc_hash::FxHashMap;

use super::atlas::{AtlasSlot, GlyphKey, RasterizedGlyph};
use super::cell::{CellBg, CellText, GridUniforms};
use crate::renderer::image_cache::atlas::AtlasAllocator;

/// Initial atlas side. 1024² bytes_per_pixel = 1 MiB grayscale,
/// 4 MiB color. Smaller than the Metal default (2048²) since CPU
/// builds are usually memory-constrained machines.
const ATLAS_SIZE: u16 = 1024;
const ATLAS_MAX_SIZE: u16 = 4096;

/// Slot 0 = block-cursor cells, slot `rows + 1` = non-block-cursor
/// cells. Matches the Metal layout in `metal::init_fg_rows`.
const CURSOR_ROW_SLOTS: usize = 2;

/// CPU-side glyph atlas. R8 for grayscale masks, RGBA8 premultiplied
/// for color emoji. Uses the same shelf allocator as the GPU atlases
/// so `AtlasSlot` coords round-trip.
pub struct CpuGridAtlas {
    pixels: Vec<u8>,
    side: u16,
    bytes_per_pixel: u8,
    allocator: AtlasAllocator,
    slots: FxHashMap<GlyphKey, AtlasSlot>,
}

impl CpuGridAtlas {
    fn new(bytes_per_pixel: u8) -> Self {
        let side = ATLAS_SIZE;
        let pixels =
            vec![0u8; (side as usize) * (side as usize) * (bytes_per_pixel as usize)];
        Self {
            pixels,
            side,
            bytes_per_pixel,
            allocator: AtlasAllocator::new(side, side),
            slots: FxHashMap::default(),
        }
    }

    pub fn new_grayscale() -> Self {
        Self::new(1)
    }

    pub fn new_color() -> Self {
        Self::new(4)
    }

    #[inline]
    pub fn lookup(&self, key: GlyphKey) -> Option<AtlasSlot> {
        self.slots.get(&key).copied()
    }

    pub fn insert(
        &mut self,
        key: GlyphKey,
        glyph: RasterizedGlyph<'_>,
    ) -> Option<AtlasSlot> {
        if !glyph.has_exact_len(self.bytes_per_pixel as usize) {
            return None;
        }
        if glyph.width == 0 || glyph.height == 0 {
            // Zero-sized glyphs (e.g. spaces) still need a cache entry
            // so the rasterizer doesn't keep producing them, but they
            // occupy no atlas space.
            let slot = AtlasSlot {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
                bearing_x: glyph.bearing_x,
                bearing_y: glyph.bearing_y,
                page: 0,
            };
            self.slots.insert(key, slot);
            return Some(slot);
        }

        let (x, y) = self.allocator.allocate(glyph.width, glyph.height)?;
        let slot = AtlasSlot {
            x,
            y,
            w: glyph.width,
            h: glyph.height,
            bearing_x: glyph.bearing_x,
            bearing_y: glyph.bearing_y,
            page: 0,
        };
        self.slots.insert(key, slot);
        self.write_pixels(
            x as usize,
            y as usize,
            glyph.width as usize,
            glyph.height as usize,
            glyph.bytes,
        );
        Some(slot)
    }

    fn write_pixels(&mut self, x: usize, y: usize, w: usize, h: usize, src: &[u8]) {
        let bpp = self.bytes_per_pixel as usize;
        let stride = self.side as usize * bpp;
        let row_bytes = w * bpp;
        for row in 0..h {
            let src_off = row * row_bytes;
            let dst_off = (y + row) * stride + x * bpp;
            self.pixels[dst_off..dst_off + row_bytes]
                .copy_from_slice(&src[src_off..src_off + row_bytes]);
        }
    }

    /// Double the atlas side, copying old pixels into the top-left of
    /// the new buffer. Existing `AtlasSlot`s stay valid because their
    /// `(x, y)` fall inside the unchanged old region. Returns `false`
    /// when already at `ATLAS_MAX_SIZE`.
    pub fn grow(&mut self) -> bool {
        let (old_w, old_h) = self.allocator.dimensions();
        if old_w >= ATLAS_MAX_SIZE {
            return false;
        }
        let new_side = old_w.saturating_mul(2).min(ATLAS_MAX_SIZE);
        if new_side <= old_w {
            return false;
        }
        let bpp = self.bytes_per_pixel as usize;
        let mut new_pixels = vec![0u8; (new_side as usize) * (new_side as usize) * bpp];

        let old_stride = old_w as usize * bpp;
        let new_stride = new_side as usize * bpp;
        for row in 0..old_h as usize {
            let src_off = row * old_stride;
            let dst_off = row * new_stride;
            new_pixels[dst_off..dst_off + old_stride]
                .copy_from_slice(&self.pixels[src_off..src_off + old_stride]);
        }
        self.pixels = new_pixels;
        self.side = new_side;
        self.allocator.grow_to(new_side, new_side);
        true
    }

    pub fn clear(&mut self) {
        self.allocator.clear();
        self.slots.clear();
    }

    #[inline]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    #[inline]
    pub fn side(&self) -> u16 {
        self.side
    }
}

pub struct CpuGridRenderer {
    cols: u32,
    rows: u32,
    /// `cols * rows` flat. Indexed `row * cols + col`.
    bg_cells: Vec<CellBg>,
    /// Per-row fg storage with the same indexing scheme as the GPU
    /// backends — slot 0 holds the block cursor, 1..=rows hold content
    /// rows, slot `rows + 1` holds the non-block cursor decoration.
    fg_rows: Vec<Vec<CellText>>,
    atlas_grayscale: CpuGridAtlas,
    atlas_color: CpuGridAtlas,
    needs_full_rebuild: bool,
}

impl CpuGridRenderer {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self {
            cols,
            rows,
            bg_cells: vec![CellBg::TRANSPARENT; bg_capacity(cols, rows)],
            fg_rows: init_fg_rows(rows),
            atlas_grayscale: CpuGridAtlas::new_grayscale(),
            atlas_color: CpuGridAtlas::new_color(),
            needs_full_rebuild: true,
        }
    }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.bg_cells = vec![CellBg::TRANSPARENT; bg_capacity(cols, rows)];
        self.fg_rows = init_fg_rows(rows);
        // Fresh buffers = zero contents; emission path must rewrite
        // every row on the next frame even if no damage came in.
        self.needs_full_rebuild = true;
    }

    pub fn write_row(&mut self, row: u32, bg: &[CellBg], fg: &[CellText]) {
        if !super::valid_row(row, self.rows, self.cols, bg.len()) {
            return;
        }
        let idx = (row as usize) + 1;
        if let Some(slot) = self.fg_rows.get_mut(idx) {
            slot.clear();
            slot.extend_from_slice(fg);
        }

        let cols = self.cols as usize;
        let row_start = (row as usize) * cols;
        let dst = &mut self.bg_cells[row_start..row_start + cols];
        dst.copy_from_slice(bg);
    }

    pub fn clear_row(&mut self, row: u32) {
        if row >= self.rows {
            return;
        }
        let idx = (row as usize) + 1;
        if let Some(slot) = self.fg_rows.get_mut(idx) {
            slot.clear();
        }
        let cols = self.cols as usize;
        let row_start = (row as usize) * cols;
        for slot in &mut self.bg_cells[row_start..row_start + cols] {
            *slot = CellBg::TRANSPARENT;
        }
    }

    pub fn set_cursor(&mut self, block: &[CellText], non_block: &[CellText]) {
        if let Some(slot) = self.fg_rows.first_mut() {
            slot.clear();
            slot.extend_from_slice(block);
        }
        let last = self.fg_rows.len().saturating_sub(1);
        if last > 0 {
            if let Some(slot) = self.fg_rows.get_mut(last) {
                slot.clear();
                slot.extend_from_slice(non_block);
            }
        }
    }

    #[inline]
    pub fn lookup_glyph(&self, key: GlyphKey) -> Option<AtlasSlot> {
        self.atlas_grayscale.lookup(key)
    }

    /// Drop every cached glyph and force a full rebuild. Called when
    /// the font library is swapped, since the new library reuses font ids.
    pub fn clear_atlas(&mut self) {
        self.atlas_grayscale.clear();
        self.atlas_color.clear();
        self.needs_full_rebuild = true;
    }

    pub fn insert_glyph(
        &mut self,
        key: GlyphKey,
        glyph: RasterizedGlyph<'_>,
    ) -> Option<AtlasSlot> {
        let mut cleared = false;
        loop {
            if let Some(slot) = self.atlas_grayscale.insert(key, glyph) {
                return Some(slot);
            }
            if self.atlas_grayscale.grow() {
                continue;
            }
            if cleared {
                return None;
            }
            self.atlas_grayscale.clear();
            self.needs_full_rebuild = true;
            cleared = true;
        }
    }

    #[inline]
    pub fn lookup_glyph_color(&self, key: GlyphKey) -> Option<AtlasSlot> {
        self.atlas_color.lookup(key)
    }

    pub fn insert_glyph_color(
        &mut self,
        key: GlyphKey,
        glyph: RasterizedGlyph<'_>,
    ) -> Option<AtlasSlot> {
        let mut cleared = false;
        loop {
            if let Some(slot) = self.atlas_color.insert(key, glyph) {
                return Some(slot);
            }
            if self.atlas_color.grow() {
                continue;
            }
            if cleared {
                return None;
            }
            self.atlas_color.clear();
            self.needs_full_rebuild = true;
            cleared = true;
        }
    }

    #[inline]
    pub fn needs_full_rebuild(&self) -> bool {
        self.needs_full_rebuild
    }

    #[inline]
    pub fn mark_full_rebuild_done(&mut self) {
        self.needs_full_rebuild = false;
    }

    /// Force the next frame to rebuild every row, even when nothing dirtied
    /// the cells. Used when state outside the grid (e.g. the color theme)
    /// changed and every cell must re-resolve.
    #[inline]
    pub fn request_full_rebuild(&mut self) {
        self.needs_full_rebuild = true;
    }

    /// Hash all state that affects what `render` will paint. The CPU
    /// rasterizer's frame-skip path uses this to short-circuit when
    /// the previous frame's output is still valid.
    pub fn hash_state<H: std::hash::Hasher>(&self, h: &mut H) {
        // bg_cells is a flat `Vec<CellBg>` — hash as raw bytes.
        h.write(bytemuck::cast_slice(self.bg_cells.as_slice()));
        // Per-row CellText bytes. Length-prefix so two adjacent rows
        // can't be confused with one wider row.
        for row in &self.fg_rows {
            h.write_usize(row.len());
            h.write(bytemuck::cast_slice(row.as_slice()));
        }
    }

    /// Paint the grid (bg cells + cursor + fg glyphs) into the
    /// caller's `0xAARRGGBB` u32 buffer. On little-endian systems the bytes
    /// are BGRA8 premultiplied. Mirrors the bg + text passes
    /// of `grid.metal`'s shaders, in the same draw order so glyphs
    /// composite correctly over their cell backgrounds.
    /// Paint the cell-bg pass into `buf`. Pair with `render_text`,
    /// with any `kitty_below_text` images composited in between.
    pub fn render_bg(
        &self,
        buf: &mut [u32],
        buf_w: u32,
        buf_h: u32,
        uniforms: &GridUniforms,
    ) {
        self.render_bg_strided(buf, buf_w, buf_h, buf_w, uniforms);
    }

    pub fn render_bg_strided(
        &self,
        buf: &mut [u32],
        buf_w: u32,
        buf_h: u32,
        stride_pixels: u32,
        uniforms: &GridUniforms,
    ) {
        if let Err(error) =
            crate::context::cpu::CpuRenderTarget::new(buf, buf_w, buf_h, stride_pixels)
        {
            tracing::warn!(%error, "skipping CPU grid background render for invalid target");
            return;
        }
        let cell_w = uniforms.cell_size[0];
        let cell_h = uniforms.cell_size[1];
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }
        let cols = uniforms.grid_size[0];
        let rows = uniforms.grid_size[1];
        if cols == 0 || rows == 0 {
            return;
        }
        let pad_top = uniforms.grid_padding[0];
        let pad_left = uniforms.grid_padding[3];

        let buf_w_i = buf_w as i32;
        let buf_h_i = buf_h as i32;
        let cursor_x = uniforms.cursor_pos[0];
        let cursor_y = uniforms.cursor_pos[1];
        let cursor_bg_active = uniforms.cursor_bg_color[3] > 0.0;
        let cursor_bg = normalize_color(uniforms.cursor_bg_color);

        let buf_cols = self.cols as usize;
        let row_count = (rows as usize).min(self.rows as usize);
        let col_count = (cols as usize).min(self.cols as usize);
        for row in 0..row_count {
            let row_off = row * buf_cols;
            for col in 0..col_count {
                let mut rgba = self.bg_cells[row_off + col].rgba;
                if cursor_bg_active && cursor_x == col as u32 && cursor_y == row as u32 {
                    rgba = cursor_bg;
                }
                if rgba[3] == 0 {
                    continue;
                }

                let x0 = (pad_left + (col as f32) * cell_w).round() as i32;
                let y0 = (pad_top + (row as f32) * cell_h).round() as i32;
                let x1 = (pad_left + ((col + 1) as f32) * cell_w).round() as i32;
                let y1 = (pad_top + ((row + 1) as f32) * cell_h).round() as i32;
                fill_rect(
                    buf,
                    buf_w_i,
                    buf_h_i,
                    stride_pixels as usize,
                    x0,
                    y0,
                    x1,
                    y1,
                    rgba,
                );
            }
        }
    }

    /// Paint the cell-text pass into `buf`. Walks `fg_rows` and blits
    /// each glyph's atlas slot at the cell origin computed from
    /// `grid_pos + bearings`.
    pub fn render_text(
        &self,
        buf: &mut [u32],
        buf_w: u32,
        buf_h: u32,
        uniforms: &GridUniforms,
    ) {
        self.render_text_strided(buf, buf_w, buf_h, buf_w, uniforms);
    }

    pub fn render_text_strided(
        &self,
        buf: &mut [u32],
        buf_w: u32,
        buf_h: u32,
        stride_pixels: u32,
        uniforms: &GridUniforms,
    ) {
        if let Err(error) =
            crate::context::cpu::CpuRenderTarget::new(buf, buf_w, buf_h, stride_pixels)
        {
            tracing::warn!(%error, "skipping CPU grid text render for invalid target");
            return;
        }
        let cell_w = uniforms.cell_size[0];
        let cell_h = uniforms.cell_size[1];
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }
        let cols = uniforms.grid_size[0];
        let rows = uniforms.grid_size[1];
        if cols == 0 || rows == 0 {
            return;
        }
        let pad_top = uniforms.grid_padding[0];
        let pad_left = uniforms.grid_padding[3];

        let buf_w_i = buf_w as i32;
        let buf_h_i = buf_h as i32;
        let cursor_x = uniforms.cursor_pos[0];
        let cursor_y = uniforms.cursor_pos[1];
        let cursor_fg_active = uniforms.cursor_color[3] > 0.0;
        let cursor_fg = normalize_color(uniforms.cursor_color);

        let mask = self.atlas_grayscale.pixels();
        let mask_side = self.atlas_grayscale.side as usize;
        let color_atlas = self.atlas_color.pixels();
        let color_side = self.atlas_color.side as usize;

        for fg in &self.fg_rows {
            for glyph in fg {
                let gw = glyph.glyph_size[0] as i32;
                let gh = glyph.glyph_size[1] as i32;
                if gw <= 0 || gh <= 0 {
                    continue;
                }

                let cell_pos_x = (glyph.grid_pos[0] as f32) * cell_w + pad_left;
                let cell_pos_y = (glyph.grid_pos[1] as f32) * cell_h + pad_top;
                let glyph_x = (cell_pos_x + glyph.bearings[0] as f32) as i32;
                let glyph_y = (cell_pos_y + cell_h - glyph.bearings[1] as f32) as i32;

                let mut color = glyph.color;
                if cursor_fg_active
                    && (glyph.bools & CellText::BOOL_IS_CURSOR_GLYPH) == 0
                    && cursor_x == glyph.grid_pos[0] as u32
                    && cursor_y == glyph.grid_pos[1] as u32
                {
                    color = cursor_fg;
                }

                let ax = glyph.glyph_pos[0] as usize;
                let ay = glyph.glyph_pos[1] as usize;

                if glyph.atlas == CellText::ATLAS_COLOR {
                    blit_color(
                        buf,
                        buf_w_i,
                        buf_h_i,
                        stride_pixels as usize,
                        glyph_x,
                        glyph_y,
                        gw,
                        gh,
                        color_atlas,
                        color_side,
                        ax,
                        ay,
                    );
                } else {
                    blit_mask(
                        buf,
                        buf_w_i,
                        buf_h_i,
                        stride_pixels as usize,
                        glyph_x,
                        glyph_y,
                        gw,
                        gh,
                        mask,
                        mask_side,
                        ax,
                        ay,
                        color,
                    );
                }
            }
        }
    }
}

#[inline]
fn bg_capacity(cols: u32, rows: u32) -> usize {
    (cols as usize)
        .checked_mul(rows as usize)
        .expect("CPU grid dimensions overflow background capacity")
}

#[inline]
fn init_fg_rows(rows: u32) -> Vec<Vec<CellText>> {
    let row_slots = (rows as usize)
        .checked_add(CURSOR_ROW_SLOTS)
        .expect("CPU grid dimensions overflow foreground row capacity");
    (0..row_slots).map(|_| Vec::new()).collect()
}

#[inline]
fn normalize_color(c: [f32; 4]) -> [u8; 4] {
    [
        (c[0].clamp(0.0, 1.0) * 255.0) as u8,
        (c[1].clamp(0.0, 1.0) * 255.0) as u8,
        (c[2].clamp(0.0, 1.0) * 255.0) as u8,
        (c[3].clamp(0.0, 1.0) * 255.0) as u8,
    ]
}

#[inline]
fn premul(c: [u8; 4]) -> [u8; 4] {
    let a = c[3] as u32;
    if a == 255 {
        return c;
    }
    if a == 0 {
        return [0, 0, 0, 0];
    }
    [
        ((c[0] as u32 * a + 127) / 255) as u8,
        ((c[1] as u32 * a + 127) / 255) as u8,
        ((c[2] as u32 * a + 127) / 255) as u8,
        c[3],
    ]
}

use crate::premul::{blend_premul_over as blend_over, pack_opaque};

#[allow(clippy::too_many_arguments)]
fn fill_rect(
    buf: &mut [u32],
    buf_w: i32,
    buf_h: i32,
    stride: usize,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    rgba: [u8; 4],
) {
    let x0 = x0.max(0);
    let y0 = y0.max(0);
    let x1 = x1.min(buf_w);
    let y1 = y1.min(buf_h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let pre = premul(rgba);
    if pre[3] == 255 {
        let opaque = pack_opaque(pre[0], pre[1], pre[2]);
        for y in y0..y1 {
            let row_start = (y as usize) * stride + (x0 as usize);
            let row_end = (y as usize) * stride + (x1 as usize);
            buf[row_start..row_end].fill(opaque);
        }
    } else {
        for y in y0..y1 {
            let row_off = (y as usize) * stride;
            for x in x0..x1 {
                let idx = row_off + (x as usize);
                buf[idx] = blend_over(pre, buf[idx]);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_mask(
    buf: &mut [u32],
    buf_w: i32,
    buf_h: i32,
    stride: usize,
    glyph_x: i32,
    glyph_y: i32,
    gw: i32,
    gh: i32,
    atlas: &[u8],
    atlas_side: usize,
    ax: usize,
    ay: usize,
    color: [u8; 4],
) {
    if color[3] == 0 {
        return;
    }
    let Some(atlas_len) = atlas_side.checked_mul(atlas_side) else {
        return;
    };
    if atlas.len() < atlas_len {
        return;
    }
    let Some(rect) = crate::context::cpu::clip_blit_rect(
        (glyph_x, glyph_y),
        (gw, gh),
        (buf_w, buf_h),
        atlas_side,
        (ax, ay),
    ) else {
        return;
    };
    let r = color[0] as u32;
    let g = color[1] as u32;
    let b = color[2] as u32;
    let ca = color[3] as u32;

    for row in 0..rect.height {
        let src_start = (rect.src_y + row) * atlas_side + rect.src_x;
        let src_row = &atlas[src_start..src_start + rect.width];
        let dst_start = (rect.dst_y + row) * stride + rect.dst_x;
        let dst_row = &mut buf[dst_start..dst_start + rect.width];
        for (&mask, dst) in src_row.iter().zip(dst_row.iter_mut()) {
            let m = mask as u32;
            if m == 0 {
                continue;
            }
            // mask alpha × text alpha → premultiplied src
            let a = (m * ca + 127) / 255;
            if a == 0 {
                continue;
            }
            let pr = (r * a + 127) / 255;
            let pg = (g * a + 127) / 255;
            let pb = (b * a + 127) / 255;
            let src = [pr as u8, pg as u8, pb as u8, a as u8];
            *dst = blend_over(src, *dst);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_color(
    buf: &mut [u32],
    buf_w: i32,
    buf_h: i32,
    stride: usize,
    glyph_x: i32,
    glyph_y: i32,
    gw: i32,
    gh: i32,
    atlas: &[u8],
    atlas_side: usize,
    ax: usize,
    ay: usize,
) {
    let Some(atlas_len) = atlas_side
        .checked_mul(atlas_side)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return;
    };
    if atlas.len() < atlas_len {
        return;
    }
    let Some(rect) = crate::context::cpu::clip_blit_rect(
        (glyph_x, glyph_y),
        (gw, gh),
        (buf_w, buf_h),
        atlas_side,
        (ax, ay),
    ) else {
        return;
    };
    let atlas_stride = atlas_side * 4;
    for row in 0..rect.height {
        let src_start = (rect.src_y + row) * atlas_stride + rect.src_x * 4;
        let src_row = &atlas[src_start..src_start + rect.width * 4];
        let dst_start = (rect.dst_y + row) * stride + rect.dst_x;
        let dst_row = &mut buf[dst_start..dst_start + rect.width];
        for (src, dst) in src_row.as_chunks::<4>().0.iter().zip(dst_row.iter_mut()) {
            let r = src[0];
            let g = src[1];
            let b = src[2];
            let a = src[3];
            if a == 0 {
                continue;
            }
            // Atlas already holds premultiplied RGBA (color emoji
            // rasterizer convention).
            let src = [r, g, b, a];
            *dst = blend_over(src, *dst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn uniforms() -> GridUniforms {
        GridUniforms {
            projection: [0.0; 16],
            grid_padding: [0.0; 4],
            cursor_color: [0.0; 4],
            cursor_bg_color: [0.0; 4],
            cell_size: [1.0; 2],
            grid_size: [1, 1],
            cursor_pos: [u32::MAX; 2],
            _pad_cursor: [0; 2],
            min_contrast: 0.0,
            flags: 0,
            padding_extend: 0,
            input_colorspace: 0,
        }
    }

    #[test]
    fn strided_render_rejects_invalid_targets() {
        let grid = CpuGridRenderer::new(1, 1);
        let uniforms = uniforms();
        let mut short = [0u32; 1];
        assert!(catch_unwind(AssertUnwindSafe(|| {
            grid.render_bg_strided(&mut short, 2, 2, 2, &uniforms);
        }))
        .is_ok());

        let mut zero_stride = [0u32; 1];
        assert!(catch_unwind(AssertUnwindSafe(|| {
            grid.render_text_strided(&mut zero_stride, 1, 1, 0, &uniforms);
        }))
        .is_ok());
    }
}
