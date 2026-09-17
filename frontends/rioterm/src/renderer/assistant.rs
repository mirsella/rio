// Copyright (c) 2023-present, Raphael Amorim.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use rio_backend::error::{RioError, RioErrorLevel, RioErrorType};
use rio_backend::sugarloaf::text::DrawOpts;
use rio_backend::sugarloaf::Sugarloaf;

/// Convert `[f32; 4]` colour to `[u8; 4]` for the `Text` API (the
/// vertex shader premultiplies, so pass non-premul RGBA).
#[inline]
fn color_u8(c: [f32; 4]) -> [u8; 4] {
    [
        (c[0].clamp(0.0, 1.0) * 255.0) as u8,
        (c[1].clamp(0.0, 1.0) * 255.0) as u8,
        (c[2].clamp(0.0, 1.0) * 255.0) as u8,
        (c[3].clamp(0.0, 1.0) * 255.0) as u8,
    ]
}

// Layout
const MAX_OVERLAY_WIDTH: f32 = 560.0;
const MIN_OVERLAY_WIDTH: f32 = 240.0;
const OVERLAY_CORNER_RADIUS: f32 = 10.0;
const OVERLAY_MARGIN_TOP: f32 = 10.0;
const OVERLAY_MARGIN_RIGHT: f32 = 10.0;
const OVERLAY_PADDING: f32 = 16.0;

const HEADING_FONT_SIZE: f32 = 15.0;
const BODY_FONT_SIZE: f32 = 12.0;

const LINE_HEIGHT: f32 = 18.0;
const HEADING_HEIGHT: f32 = 26.0;
const MAX_VISIBLE_LINES: usize = 16;

// Colors
const BG_COLOR: [f32; 4] = [0.12, 0.12, 0.12, 0.98];
const BORDER_COLOR: [f32; 4] = [0.28, 0.28, 0.30, 0.8];
const HEADING_COLOR_ERROR: [f32; 4] = [1.0, 0.07, 0.38, 1.0];
const HEADING_COLOR_WARNING: [f32; 4] = [0.99, 0.73, 0.16, 1.0];
const TEXT_COLOR: [f32; 4] = [0.85, 0.85, 0.85, 1.0];

// Depth / order
const DEPTH_BG: f32 = 0.1;
const ORDER: u8 = 20;

pub(crate) fn wrap_text(
    text: &str,
    max_width: f32,
    mut measure: impl FnMut(&str) -> f32,
) -> Vec<String> {
    let mut lines = Vec::new();
    let space_width = measure(" ");
    let mut buf = [0u8; 4];

    for paragraph in text.lines() {
        if paragraph.trim().is_empty() {
            lines.push(String::new());
            continue;
        }

        let mut line = String::new();
        let mut width = 0.0;

        for word in paragraph.split_whitespace() {
            let word_width = measure(word);
            if word_width > max_width {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    width = 0.0;
                }
                for ch in word.chars() {
                    let char_width = measure(ch.encode_utf8(&mut buf));
                    if width + char_width > max_width && !line.is_empty() {
                        lines.push(std::mem::take(&mut line));
                        width = 0.0;
                    }
                    line.push(ch);
                    width += char_width;
                }
            } else if line.is_empty() {
                line.push_str(word);
                width = word_width;
            } else if width + space_width + word_width <= max_width {
                line.push(' ');
                line.push_str(word);
                width += space_width + word_width;
            } else {
                lines.push(std::mem::take(&mut line));
                line.push_str(word);
                width = word_width;
            }
        }

        if !line.is_empty() {
            lines.push(line);
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

/// Widest the toast may grow for this window. Never exceeds the window
/// itself, so narrow windows get a full-bleed card instead of overflow.
fn max_overlay_width(logical_width: f32) -> f32 {
    let available = (logical_width - OVERLAY_MARGIN_RIGHT * 2.0).max(0.0);
    available.clamp(available.min(MIN_OVERLAY_WIDTH), MAX_OVERLAY_WIDTH)
}

#[derive(Default)]
pub struct AssistantOverlay {
    error: Option<RioError>,
}

impl AssistantOverlay {
    #[inline]
    pub fn is_active(&self) -> bool {
        self.error.is_some()
    }

    /// Whether the active toast is a hard error. Errors are modal
    /// (keys and IME blocked, Enter dismisses); warnings render but
    /// must never block input over a working terminal.
    #[inline]
    pub fn is_error(&self) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.level == RioErrorLevel::Error)
    }

    #[inline]
    pub fn set_error(&mut self, error: RioError) {
        self.error = Some(error);
    }

    #[inline]
    pub fn set_warning(&mut self, report: RioErrorType) {
        self.error = Some(RioError {
            level: RioErrorLevel::Warning,
            report,
        });
    }

    #[inline]
    pub fn clear(&mut self) {
        self.error = None;
    }

    pub fn render(&mut self, sugarloaf: &mut Sugarloaf, dimensions: (f32, f32, f32)) {
        let Some(error) = &self.error else {
            return;
        };

        let (window_width, _window_height, scale_factor) = dimensions;
        let logical_width = window_width / scale_factor;

        let max_overlay_w = max_overlay_width(logical_width);
        let max_content_w = (max_overlay_w - OVERLAY_PADDING * 2.0).max(10.0);

        let is_error = error.level == RioErrorLevel::Error;
        let heading_text = if is_error { "Error" } else { "Warning" };
        let heading_opts = DrawOpts {
            font_size: HEADING_FONT_SIZE,
            color: color_u8(if is_error {
                HEADING_COLOR_ERROR
            } else {
                HEADING_COLOR_WARNING
            }),
            ..DrawOpts::default()
        };
        let body_opts = DrawOpts {
            font_size: BODY_FONT_SIZE,
            color: color_u8(TEXT_COLOR),
            ..DrawOpts::default()
        };

        let ui = sugarloaf.text_mut();
        let heading_w = ui.measure(heading_text, &heading_opts);
        let wrapped = wrap_text(&error.report.to_string(), max_content_w, |s| {
            ui.measure(s, &body_opts)
        });

        let visible_count = wrapped.len().min(MAX_VISIBLE_LINES);
        let mut max_line_w = heading_w;
        for line in wrapped.iter().take(visible_count) {
            max_line_w = max_line_w.max(ui.measure(line, &body_opts));
        }

        let ow = (max_line_w + OVERLAY_PADDING * 2.0)
            .clamp(max_overlay_w.min(MIN_OVERLAY_WIDTH), max_overlay_w);
        let ox = (logical_width - ow - OVERLAY_MARGIN_RIGHT).max(0.0);
        let oy = OVERLAY_MARGIN_TOP;
        let oh = OVERLAY_PADDING
            + HEADING_HEIGHT
            + (visible_count as f32 * LINE_HEIGHT)
            + OVERLAY_PADDING;

        // Border & Background
        sugarloaf.rounded_rect(
            None,
            ox,
            oy,
            ow,
            oh,
            BORDER_COLOR,
            DEPTH_BG,
            OVERLAY_CORNER_RADIUS,
            ORDER,
        );
        sugarloaf.rounded_rect(
            None,
            ox + 1.0,
            oy + 1.0,
            ow - 2.0,
            oh - 2.0,
            BG_COLOR,
            DEPTH_BG + 0.01,
            (OVERLAY_CORNER_RADIUS - 1.0).max(0.0),
            ORDER,
        );

        // Heading
        let text_x = ox + OVERLAY_PADDING;
        let heading_y = oy + OVERLAY_PADDING;
        let ui = sugarloaf.text_mut();
        ui.draw(text_x, heading_y, heading_text, &heading_opts);

        // Body lines
        let body_y_start = heading_y + HEADING_HEIGHT;
        for (i, line_text) in wrapped.iter().take(visible_count).enumerate() {
            if !line_text.is_empty() {
                ui.draw(
                    text_x,
                    body_y_start + (i as f32 * LINE_HEIGHT),
                    line_text,
                    &body_opts,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_text_basic() {
        let text = "hello world from rio terminal emulator";
        // 1 char = 10.0 width
        let lines = wrap_text(text, 100.0, |s| s.len() as f32 * 10.0);
        // "hello" (50), "hello world" (110 > 100) -> "hello", "world from" (100), "rio" (30), etc.
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world from");
        assert_eq!(lines[2], "rio");
        assert_eq!(lines[3], "terminal");
        assert_eq!(lines[4], "emulator");
    }

    #[test]
    fn test_wrap_text_long_word() {
        let text =
            "short /home/user/very_long_path_without_spaces_here_that_exceeds_width done";
        let lines = wrap_text(text, 100.0, |s| s.len() as f32 * 10.0);
        assert_eq!(lines[0], "short");
        for line in &lines {
            assert!(
                (line.len() as f32 * 10.0) <= 100.0,
                "line overflows: {line:?}"
            );
        }
        assert!(lines.last().unwrap().ends_with("done"));
    }

    #[test]
    fn test_wrap_text_preserves_empty_lines() {
        let text = "paragraph 1\n\nparagraph 2";
        let lines = wrap_text(text, 200.0, |s| s.len() as f32 * 10.0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "paragraph 1");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "paragraph 2");
    }

    #[test]
    fn test_wrap_text_stale_working_directory() {
        let text = "Working directory \"/home/mirsella/dev/mightyminions/androidsdk\" is not available; started in \"/home/mirsella\" instead.";
        // Say 400.0 max width, each char ~8.0 -> ~50 chars per line
        let lines = wrap_text(text, 400.0, |s| s.len() as f32 * 8.0);
        for line in &lines {
            assert!((line.len() as f32 * 8.0) <= 400.0);
        }
        assert!(lines.len() > 1);
    }
}
