mod builtin;
mod runtime;

use crate::context::webgpu::WgpuContext;
use librashader::{
    presets::ShaderFeatures,
    runtime::{Size, Viewport},
};
use std::sync::Arc;

pub type Filter = String;

/// A brush for applying RetroArch filters.
#[derive(Default)]
pub struct FiltersBrush {
    filter_chains: Vec<crate::components::filters::runtime::FilterChain>,
    filter_intermediates: Vec<Arc<wgpu::Texture>>,
    intermediate_size: Option<(u32, u32)>,
    framecount: usize,
}

impl FiltersBrush {
    /// Best-effort wrapper retained for native callers.
    #[inline]
    pub fn update_filters(&mut self, ctx: &WgpuContext, filters: &[Filter]) {
        if let Err(error) = self.try_update_filters(ctx, filters) {
            tracing::error!("Failed to install filter chains: {error}");
        }
    }

    fn try_update_filters(
        &mut self,
        ctx: &WgpuContext,
        filters: &[Filter],
    ) -> Result<(), String> {
        if filters.is_empty() {
            self.filter_chains.clear();
            self.filter_intermediates.clear();
            self.intermediate_size = None;
            self.framecount = 0;
            return Ok(());
        }

        if !ctx.supports_filter_texture_usage() {
            return Err(
                "the selected WGPU surface does not support filter texture copies".into(),
            );
        }

        let mut filter_chains = Vec::with_capacity(filters.len());
        for filter in filters {
            let configured_filter = filter.to_lowercase();
            let chain = match configured_filter.as_str() {
                "newpixiecrt" | "fubax_vr" => {
                    tracing::debug!("Loading builtin filter {}", configured_filter);
                    let builtin_filter = match configured_filter.as_str() {
                        "newpixiecrt" => builtin::newpixiecrt,
                        "fubax_vr" => builtin::fubaxvr,
                        _ => unreachable!("builtin filter name was matched above"),
                    };
                    let shader_preset = builtin_filter().map_err(|error| {
                        format!(
                            "failed to build builtin filter {configured_filter}: {error}"
                        )
                    })?;
                    crate::components::filters::runtime::FilterChain::load_from_preset(
                        shader_preset,
                        &ctx.device,
                        &ctx.queue,
                        None,
                    )
                    .map_err(|error| {
                        format!(
                            "failed to load builtin filter {configured_filter}: {error}"
                        )
                    })?
                }
                _ => {
                    tracing::debug!("Loading filter {}", filter);
                    crate::components::filters::runtime::FilterChain::load_from_path(
                        filter,
                        ShaderFeatures::NONE,
                        &ctx.device,
                        &ctx.queue,
                        None,
                    )
                    .map_err(|error| format!("failed to load filter {filter}: {error}"))?
                }
            };
            filter_chains.push(chain);
        }

        let filter_intermediates = create_intermediates(ctx, filter_chains.len());
        self.filter_chains = filter_chains;
        self.filter_intermediates = filter_intermediates;
        self.intermediate_size = Some(context_size(ctx));
        self.framecount = 0;
        Ok(())
    }

    fn ensure_intermediates(&mut self, ctx: &WgpuContext) {
        let size = context_size(ctx);
        let intermediate_count = if self.filter_chains.len() % 2 == 1 {
            self.filter_chains.len().saturating_sub(1)
        } else {
            self.filter_chains.len()
        };
        if self.intermediate_size == Some(size)
            && self.filter_intermediates.len() == intermediate_count
        {
            return;
        }
        self.filter_intermediates = create_intermediates(ctx, self.filter_chains.len());
        self.intermediate_size = Some(size);
    }

    /// Render the filters on top of `src_texture` to `dst_texture`.
    ///
    /// Checked filter rendering used by the native WGPU render path.
    #[inline]
    fn render_checked(
        &mut self,
        ctx: &WgpuContext,
        encoder: &mut wgpu::CommandEncoder,
        src_texture: &wgpu::Texture,
        dst_texture: &wgpu::Texture,
    ) -> Result<(), String> {
        let filters_count = self.filter_chains.len();
        if filters_count == 0 {
            return Ok(());
        }

        if !ctx.supports_filter_texture_usage() {
            return Err(
                "the selected WGPU surface does not support filter texture copies".into(),
            );
        }
        self.ensure_intermediates(ctx);

        // Some shaders require different source and destination textures.
        // librashader also requires the source texture to be Arc-owned.
        let src_texture = {
            let new_src_texture =
                Arc::new(ctx.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("Filters Source Texture"),
                    size: src_texture.size(),
                    mip_level_count: src_texture.mip_level_count(),
                    sample_count: src_texture.sample_count(),
                    dimension: src_texture.dimension(),
                    format: src_texture.format(),
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[src_texture.format()],
                }));

            encoder.copy_texture_to_texture(
                src_texture.as_image_copy(),
                new_src_texture.as_image_copy(),
                new_src_texture.size(),
            );
            new_src_texture
        };

        let view_size = Size::new(ctx.size.width as u32, ctx.size.height as u32);
        for (idx, filter) in self.filter_chains.iter_mut().enumerate() {
            let filter_src_texture: Arc<wgpu::Texture>;
            let filter_dst_texture: &wgpu::Texture;

            if idx == 0 {
                filter_src_texture = src_texture.clone();
                if filters_count == 1 {
                    filter_dst_texture = dst_texture;
                } else {
                    filter_dst_texture = &self.filter_intermediates[0];
                }
            } else if idx == filters_count - 1 {
                filter_src_texture = self.filter_intermediates[idx - 1].clone();
                filter_dst_texture = dst_texture;
            } else {
                filter_src_texture = self.filter_intermediates[idx - 1].clone();
                filter_dst_texture = &self.filter_intermediates[idx];
            }

            let dst_texture_view =
                filter_dst_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let dst_output_view =
                crate::components::filters::runtime::WgpuOutputView::new_from_raw(
                    &dst_texture_view,
                    view_size,
                    ctx.format,
                );
            let dst_viewport =
                Viewport::new_render_target_sized_origin(dst_output_view, None).unwrap();

            self.framecount = self.framecount.wrapping_add(1);
            filter
                .frame(
                    filter_src_texture,
                    &dst_viewport,
                    encoder,
                    self.framecount,
                    None,
                    ctx,
                )
                .map_err(|error| format!("filter rendering failed: {error}"))?;
        }
        Ok(())
    }

    /// Best-effort wrapper retained for native callers.
    #[inline]
    pub fn render(
        &mut self,
        ctx: &WgpuContext,
        encoder: &mut wgpu::CommandEncoder,
        src_texture: &wgpu::Texture,
        dst_texture: &wgpu::Texture,
    ) {
        if let Err(error) = self.render_checked(ctx, encoder, src_texture, dst_texture) {
            tracing::error!("Filter rendering failed: {error}");
        }
    }
}

fn context_size(ctx: &WgpuContext) -> (u32, u32) {
    (ctx.size.width as u32, ctx.size.height as u32)
}

fn create_intermediates(
    ctx: &WgpuContext,
    filter_count: usize,
) -> Vec<Arc<wgpu::Texture>> {
    let skip = usize::from(filter_count % 2 == 1);
    let size = wgpu::Extent3d {
        depth_or_array_layers: 1,
        width: ctx.size.width as u32,
        height: ctx.size.height as u32,
    };
    (0..filter_count.saturating_sub(skip))
        .map(|_| {
            Arc::new(ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Filter Intermediate Texture"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: ctx.format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[ctx.format],
            }))
        })
        .collect()
}
