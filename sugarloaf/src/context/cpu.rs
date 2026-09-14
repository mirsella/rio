// Copyright (c) 2023-present, Raphael Amorim.
//
// CPU rendering backend context.
//
// The rasterizer writes logical `0xAARRGGBB` values. On the little-endian
// targets supported by Rio, those values are BGRA8 premultiplied bytes in
// memory. A native context owns the softbuffer surface presented by Rio.

use crate::sugarloaf::{SugarloafWindow, SugarloafWindowSize};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use std::num::NonZeroU32;
use std::rc::Rc;
use thiserror::Error;

/// Errors returned while constructing or using a CPU render target.
#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum CpuRenderError {
    #[error("CPU render target dimensions must be non-zero")]
    InvalidDimensions,
    #[error("CPU render target dimensions exceed the supported i32 range")]
    DimensionsTooLarge,
    #[error("CPU render target stride is too large")]
    StrideTooLarge,
    #[error("CPU render target stride ({stride_pixels} pixels) is smaller than its width ({width} pixels)")]
    StrideTooSmall { stride_pixels: u32, width: u32 },
    #[error("CPU render target size arithmetic overflowed")]
    SizeOverflow,
    #[error("CPU render target needs {required_pixels} pixels, but the buffer contains {actual_pixels}")]
    BufferTooShort {
        required_pixels: usize,
        actual_pixels: usize,
    },
    #[error("CPU image {width}x{height} needs {required_bytes} RGBA bytes, but only {actual_bytes} were provided")]
    InvalidImageData {
        width: u32,
        height: u32,
        required_bytes: usize,
        actual_bytes: usize,
    },
    #[error("CPU image {route_id}:{image_id} is missing from the image store")]
    MissingImageData { route_id: usize, image_id: u64 },
    #[error("CPU image {route_id}:{image_id} does not contain RGBA8 pixels")]
    UnsupportedImageData { route_id: usize, image_id: u64 },
    #[error("CPU color glyph atlas layer {layer} is unavailable")]
    MissingColorAtlas { layer: i32 },
    #[error("CPU color glyph atlas layer {layer} has invalid pixel data")]
    InvalidColorAtlas { layer: i32 },
    #[error("CPU mask glyph atlas has invalid pixel data")]
    InvalidMaskAtlas,
}

/// A validated caller-owned CPU framebuffer.
///
/// `stride_pixels` is the number of `u32` elements between the starts of
/// adjacent rows, so it may be larger than `width` for a padded buffer. Only
/// the first `width` pixels of each row are written. Pixels use the native
/// softbuffer's premultiplied `u32` format.
pub(crate) struct CpuRenderTarget<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
    stride_pixels: u32,
}

impl<'a> CpuRenderTarget<'a> {
    pub(crate) fn new(
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
        stride_pixels: u32,
    ) -> Result<Self, CpuRenderError> {
        validate_target(width, height, stride_pixels, pixels.len())?;
        Ok(Self {
            pixels,
            width,
            height,
            stride_pixels,
        })
    }

    #[inline]
    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub(crate) fn stride_pixels(&self) -> u32 {
        self.stride_pixels
    }

    #[inline]
    pub(crate) fn pixels_mut(&mut self) -> &mut [u32] {
        self.pixels
    }
}

/// A destination rectangle and its corresponding source rectangle after
/// clipping a glyph to both the framebuffer and a square atlas.
#[derive(Clone, Copy)]
pub(crate) struct CpuBlitRect {
    pub(crate) dst_x: usize,
    pub(crate) dst_y: usize,
    pub(crate) src_x: usize,
    pub(crate) src_y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

/// Clip a glyph once before entering its per-pixel loop. The caller must
/// separately validate that the atlas backing buffer contains the full
/// `atlas_side x atlas_side` image.
#[inline]
pub(crate) fn clip_blit_rect(
    (glyph_x, glyph_y): (i32, i32),
    (glyph_width, glyph_height): (i32, i32),
    (buf_width, buf_height): (i32, i32),
    atlas_side: usize,
    (atlas_x, atlas_y): (usize, usize),
) -> Option<CpuBlitRect> {
    if glyph_width <= 0 || glyph_height <= 0 || buf_width <= 0 || buf_height <= 0 {
        return None;
    }

    let side = i64::try_from(atlas_side).ok()?;
    let source_x = i64::try_from(atlas_x).ok()?;
    let source_y = i64::try_from(atlas_y).ok()?;
    if side <= 0 || source_x >= side || source_y >= side {
        return None;
    }

    let glyph_x = i64::from(glyph_x);
    let glyph_y = i64::from(glyph_y);
    let dst_x0 = glyph_x.max(0);
    let dst_y0 = glyph_y.max(0);
    let mut dst_x1 = (glyph_x + i64::from(glyph_width)).min(i64::from(buf_width));
    let mut dst_y1 = (glyph_y + i64::from(glyph_height)).min(i64::from(buf_height));
    if dst_x1 <= dst_x0 || dst_y1 <= dst_y0 {
        return None;
    }

    let source_x = source_x.checked_add(dst_x0.checked_sub(glyph_x)?)?;
    let source_y = source_y.checked_add(dst_y0.checked_sub(glyph_y)?)?;
    if source_x >= side || source_y >= side {
        return None;
    }

    dst_x1 = dst_x1.min(dst_x0.checked_add(side - source_x)?);
    dst_y1 = dst_y1.min(dst_y0.checked_add(side - source_y)?);
    if dst_x1 <= dst_x0 || dst_y1 <= dst_y0 {
        return None;
    }

    Some(CpuBlitRect {
        dst_x: usize::try_from(dst_x0).ok()?,
        dst_y: usize::try_from(dst_y0).ok()?,
        src_x: usize::try_from(source_x).ok()?,
        src_y: usize::try_from(source_y).ok()?,
        width: usize::try_from(dst_x1 - dst_x0).ok()?,
        height: usize::try_from(dst_y1 - dst_y0).ok()?,
    })
}

fn validate_target(
    width: u32,
    height: u32,
    stride_pixels: u32,
    buffer_len: usize,
) -> Result<(), CpuRenderError> {
    if width == 0 || height == 0 {
        return Err(CpuRenderError::InvalidDimensions);
    }
    if width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(CpuRenderError::DimensionsTooLarge);
    }
    if stride_pixels < width {
        return Err(CpuRenderError::StrideTooSmall {
            stride_pixels,
            width,
        });
    }
    let stride = stride_pixels as usize;
    if stride.checked_mul(std::mem::size_of::<u32>()).is_none() {
        return Err(CpuRenderError::StrideTooLarge);
    }
    let required_pixels = (height as usize)
        .checked_sub(1)
        .and_then(|rows| rows.checked_mul(stride))
        .and_then(|offset| offset.checked_add(width as usize))
        .ok_or(CpuRenderError::SizeOverflow)?;
    if required_pixels > buffer_len {
        return Err(CpuRenderError::BufferTooShort {
            required_pixels,
            actual_pixels: buffer_len,
        });
    }
    Ok(())
}

pub struct SoftbufferHandle {
    window: RawWindowHandle,
    display: RawDisplayHandle,
}

impl raw_window_handle::HasWindowHandle for SoftbufferHandle {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(self.window) })
    }
}

impl raw_window_handle::HasDisplayHandle for SoftbufferHandle {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(self.display) })
    }
}

unsafe impl Send for SoftbufferHandle {}
unsafe impl Sync for SoftbufferHandle {}

pub type CpuSurface = softbuffer::Surface<Rc<SoftbufferHandle>, Rc<SoftbufferHandle>>;

pub struct CpuContext {
    pub size: SugarloafWindowSize,
    pub scale: f32,
    /// Buffer width in u32 elements for the native surface.
    pub width_px: u32,
    pub height_px: u32,
    pub surface: CpuSurface,
    _handle: Rc<SoftbufferHandle>,
}

impl CpuContext {
    pub fn new(window: SugarloafWindow) -> Self {
        let size = window.size;
        let scale = window.scale;

        let handle = Rc::new(SoftbufferHandle {
            window: window.handle,
            display: window.display,
        });

        let context = softbuffer::Context::new(handle.clone())
            .expect("CPU backend: failed to create softbuffer context");
        let mut surface = softbuffer::Surface::new(&context, handle.clone())
            .expect("CPU backend: failed to create softbuffer surface");

        let width = (size.width as u32).max(1);
        let height = (size.height as u32).max(1);

        if let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            surface
                .resize(w, h)
                .expect("CPU backend: failed to size softbuffer surface");
        }

        Self {
            size,
            scale,
            width_px: width,
            height_px: height,
            surface,
            _handle: handle,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.size.width = width as f32;
        self.size.height = height as f32;
        self.width_px = width;
        self.height_px = height;
        if let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            let _ = self.surface.resize(w, h);
        }
    }

    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    pub fn supports_f16(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_rect_clips_destination_and_source_together() {
        let rect = clip_blit_rect((-2, 1), (6, 5), (4, 5), 8, (2, 3)).unwrap();
        assert_eq!(rect.dst_x, 0);
        assert_eq!(rect.dst_y, 1);
        assert_eq!(rect.src_x, 4);
        assert_eq!(rect.src_y, 3);
        assert_eq!(rect.width, 4);
        assert_eq!(rect.height, 4);
    }

    #[test]
    fn blit_rect_clips_atlas_overflow() {
        let rect = clip_blit_rect((0, 0), (2, 2), (2, 2), 4, (3, 3)).unwrap();
        assert_eq!(rect.dst_x, 0);
        assert_eq!(rect.src_x, 3);
        assert_eq!(rect.width, 1);
        assert_eq!(rect.height, 1);
    }

    #[test]
    fn target_rejects_invalid_dimensions_stride_and_length() {
        let mut pixels = [0u32; 4];
        assert!(matches!(
            CpuRenderTarget::new(&mut pixels, 0, 1, 1),
            Err(CpuRenderError::InvalidDimensions)
        ));
        assert!(matches!(
            CpuRenderTarget::new(&mut pixels, 2, 2, 1),
            Err(CpuRenderError::StrideTooSmall {
                stride_pixels: 1,
                width: 2,
            })
        ));
        assert!(matches!(
            CpuRenderTarget::new(&mut pixels, 3, 2, 3),
            Err(CpuRenderError::BufferTooShort {
                required_pixels: 6,
                actual_pixels: 4,
            })
        ));
    }

    #[test]
    fn target_accepts_padded_rows() {
        let mut pixels = [0u32; 8];
        let target = CpuRenderTarget::new(&mut pixels, 3, 2, 4).unwrap();
        assert_eq!(target.stride_pixels(), 4);
    }
}
