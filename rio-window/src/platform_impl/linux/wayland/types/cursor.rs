use cursor_icon::CursorIcon;

use sctk::reexports::client::protocol::wl_shm::Format;
use sctk::shm::slot::{Buffer, SlotPool};

use crate::cursor::CursorImage;

#[derive(Debug)]
pub enum SelectedCursor {
    Named(CursorIcon),
    Custom(CustomCursor),
}

impl Default for SelectedCursor {
    fn default() -> Self {
        Self::Named(Default::default())
    }
}

#[derive(Debug)]
pub struct CustomCursor {
    pub buffer: Buffer,
    pub w: i32,
    pub h: i32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

impl CustomCursor {
    pub(crate) fn new(pool: &mut SlotPool, image: &CursorImage) -> Self {
        let (buffer, canvas) = pool
            .create_buffer(
                image.width as i32,
                image.height as i32,
                4 * (image.width as i32),
                Format::Argb8888,
            )
            .unwrap();

        let (canvas_chunks, _) = canvas.as_chunks_mut::<4>();
        let (rgba_chunks, _) = image.rgba.as_chunks::<4>();
        for (canvas_chunk, rgba) in canvas_chunks.iter_mut().zip(rgba_chunks) {
            // Alpha in buffer is premultiplied.
            let alpha = rgba[3] as f32 / 255.;
            let r = (rgba[0] as f32 * alpha) as u32;
            let g = (rgba[1] as f32 * alpha) as u32;
            let b = (rgba[2] as f32 * alpha) as u32;
            let color = ((rgba[3] as u32) << 24) + (r << 16) + (g << 8) + b;
            *canvas_chunk = color.to_le_bytes();
        }

        CustomCursor {
            buffer,
            w: image.width as i32,
            h: image.height as i32,
            hotspot_x: image.hotspot_x as i32,
            hotspot_y: image.hotspot_y as i32,
        }
    }
}
