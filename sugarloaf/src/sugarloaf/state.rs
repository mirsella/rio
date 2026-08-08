// Copyright (c) 2023-present, Raphael Amorim.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Sugarloaf-side global state.

use crate::font::FontLibrary;
use crate::layout::RootStyle;

pub struct SugarState {
    pub style: RootStyle,
    /// Live font handle. Cloned (Arc-shallow) into per-frame contexts.
    /// Replaces the previous indirection through `Content`.
    pub fonts: FontLibrary,
}

impl SugarState {
    pub fn new(style: RootStyle, font_library: &FontLibrary) -> SugarState {
        SugarState {
            fonts: font_library.clone(),
            style,
        }
    }

    /// Refresh `RootStyle.scale_factor`. Per-panel `dimension` /
    /// `scaled_font_size` updates happen on rio's `ContextDimension`
    /// — this only touches sugarloaf's global default that new panels
    /// inherit from.
    #[inline]
    pub fn compute_layout_rescale(&mut self, scale: f32) {
        self.style.scale_factor = scale;
    }
}
