//! DesignCraft renderer: spreads → premultiplied RGBA pixels with `vello_cpu`.
//!
//! The renderer draws *document content only* (paper, page items, parent items, composed text,
//! placed images). Non-printing chrome — guides, frame edges, ports, selection — is drawn by the
//! UI as vector overlays so it stays crisp at any zoom.
//!
//! Callers pass a list of spreads with their placement on a shared canvas and a canvas → pixel
//! view transform, so the UI can render exactly its viewport with every visible spread.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use designcraft_color::BlendMode as DcBlend;
use designcraft_compose::{Cache, ComposedStory, ShownOn};
use designcraft_doc::{Content, Document, Item, PageSide, SpreadRef, StoryId, StrokeAlign, StrokeType};
use designcraft_geom::corners;
use designcraft_geom::{Affine, BezPath, Rect, Shape, Vec2};
use vello_cpu::kurbo;
use vello_cpu::peniko::{self, BlendMode, Compose, Mix};
use vello_cpu::{Pixmap, RenderContext, Resources};

pub use vello_cpu;

mod blendspace;
mod colorize;
pub mod damage;
mod fx;
pub mod glyphs;
mod grid;
mod pdf_bounds;
mod pdf_layers;
pub use pdf_layers::{pdf_hide_layers, pdf_layers};

mod color;
mod decode;
mod draw;
mod driver;
mod pdf_raster;
mod stroke;
mod types;

use color::{PLATE, rect_overlaps, set_fill_paint};
pub use color::{blend_mode, color_of, proof_view};
pub(crate) use color::{overprints, screen_color};
use decode::halve;
pub use decode::{decode_pixmap, decode_pixmap_page, image_mime, image_size, natural_size};
#[cfg(test)]
use pdf_raster::PDF_RENDERS;
pub use pdf_raster::{
    PdfRasterError, check_pdf_input_size, crop_pixmap, is_pdf, pdf_crop_box, pdf_page_box, pdf_page_count, pdf_page_png, pdf_page_png_checked,
    pdf_page_png_options_checked, pdf_page_size, render_pdf_page, shown_box,
};
pub(crate) use types::Frame;
pub use types::{DisplayQuality, FrameStats, MAX_PAGE_PIXELS, MAX_SIDE, Placed, RenderOptions, Rendered, default_threads, effect_outset};
pub mod images;
mod text;

impl Rendered {
    pub fn to_straight(&self) -> Vec<u8> {
        let mut out = self.pixels.clone();
        for px in out.as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            if a != 0 && a != 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        out
    }
    /// PNG bytes (empty, with the error logged, if the image can't be encoded).
    pub fn to_png(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut px = self.to_straight();
        // `pixels` is public: a mismatched buffer is padded or cut to the stated size.
        px.resize(self.width as usize * self.height as usize * 4, 0);
        let Some(img) = image::RgbaImage::from_raw(self.width, self.height, px) else {
            log::error!("PNG encode: {}×{} image is too large", self.width, self.height);
            return Vec::new();
        };
        if let Err(e) = img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png) {
            log::error!("PNG encode: {e}");
            return Vec::new();
        }
        buf
    }
    pub fn to_jpeg(&self, quality: u8) -> Vec<u8> {
        let rgba = self.to_straight();
        let rgb: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                let a = p[3] as u32;
                let mix = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
                [mix(p[0]), mix(p[1]), mix(p[2])]
            })
            .collect();
        let mut buf = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality.clamp(1, 100));
        let _ = image::ImageEncoder::write_image(enc, &rgb, self.width, self.height, image::ExtendedColorType::Rgb8);
        buf
    }
    /// Straight-alpha RGBA at (x, y).
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        let p = &self.pixels[i..i + 4];
        let a = p[3] as u32;
        if a == 0 {
            return [0, 0, 0, 0];
        }
        let un = |c: u8| ((c as u32 * 255 + a / 2) / a).min(255) as u8;
        [un(p[0]), un(p[1]), un(p[2]), p[3]]
    }
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            background: None,
            paper: true,
            printing_only: false,
            hidden: vec![],
            greek_below_px: 0.0,
            page_shadow: false,
            highlight_missing_fonts: false,
            highlight_hj: false,
            highlight_keeps: false,
            highlight_custom_tracking: false,
            condition_indicators: false,
            note_indicators: false,
            change_markup: false,
            tag_markers: false,
            plate: None,
            quality: DisplayQuality::High,
            rich_black: false,
            overprint_preview: false,
            blend_space_view: false,
            document_grid: vec![],
        }
    }
}

/// Reusable renderer: keeps the render context, the frame-space glyph paths of composed text
/// and (process-wide) decoded images between renders.
pub struct Renderer {
    ctx: Option<RenderContext>,
    resources: Resources,
    glyphs: text::GlyphCache,
    /// Composed stories looked up during the current render.
    stories: HashMap<(StoryId, ShownOn), Arc<ComposedStory>>,
    drawing_items: Vec<designcraft_doc::ItemId>,
    cmyk_view: Option<blendspace::CachedView>,
    /// Whether the last render ran the CMYK ink passes (only then is its view worth keeping).
    cmyk_ran: bool,
    #[cfg(test)]
    cmyk_passes: usize,
    /// Pixels the last ink pass covered (tests).
    #[cfg(test)]
    cmyk_pass_pixels: usize,
    /// Items the last ink pass drew (tests).
    #[cfg(test)]
    cmyk_pass_items: usize,
    pub threads: u16,
    pub stats: FrameStats,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Frame<'a> {
    doc: &'a Document,
    cache: &'a Cache,
    view: Affine,
    /// Visible area in spread space.
    visible: Rect,
    /// Size of a device pixel in points.
    px: f64,
    opts: &'a RenderOptions,
    /// Drawing into a multithreaded context (no filter layers).
    mt: bool,
}

pub fn default_threads() -> u16 {
    #[cfg(target_arch = "wasm32")]
    {
        0
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(n) = std::env::var("DESIGNCRAFT_RENDER_THREADS").ok().and_then(|v| v.parse().ok()) {
            return n;
        }
        std::thread::available_parallelism().map(|n| (n.get().saturating_sub(1)).clamp(0, 8) as u16).unwrap_or(0)
    }
}

/// An item's fill under its content; an overprinting fill multiplies into what is beneath.
fn draw_body_fill(ctx: &mut RenderContext, f: &Frame, it: &Item, bp: &BezPath, xf: Affine) {
    if it.fill.is_none() {
        return;
    }
    let doc = f.doc;
    ctx.set_transform(f.view * xf);
    let op = overprints(f, &it.fill.swatch, it.fill.tint, it.fill.overprint);
    if op {
        ctx.push_layer(None, Some(BlendMode::new(Mix::Multiply, Compose::SrcOver)), None, None, None);
    }
    if set_fill_paint(ctx, doc, &it.fill, bp.bounding_box()) {
        ctx.fill_path(bp);
    }
    ctx.reset_paint_transform();
    if op {
        ctx.pop_layer();
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            ctx: None,
            resources: Resources::new(),
            glyphs: text::GlyphCache::default(),
            stories: HashMap::new(),
            drawing_items: Vec::new(),
            cmyk_view: None,
            cmyk_ran: false,
            #[cfg(test)]
            cmyk_passes: 0,
            #[cfg(test)]
            cmyk_pass_pixels: 0,
            #[cfg(test)]
            cmyk_pass_items: 0,
            threads: default_threads(),
            stats: FrameStats::default(),
        }
    }

    /// Drop cached glyph paths and the CMYK viewport (rebuilt on demand).
    pub fn clear_caches(&mut self) {
        self.cmyk_view = None;
        self.glyphs.clear();
        self.stories.clear();
    }

    /// Number of text frames whose glyph paths are cached.
    pub fn cached_frames(&self) -> usize {
        self.glyphs.len()
    }

    /// Render the given spreads into a `width`×`height` image with `view` (canvas → pixels).
    pub fn render(
        &mut self,
        doc: &Document,
        cache: &Cache,
        spreads: &[Placed],
        width: u32,
        height: u32,
        view: Affine,
        opts: &RenderOptions,
    ) -> Rendered {
        let start = now();
        RICH_BLACK.with(|r| r.set(opts.rich_black));
        PLATE.with(|p| p.set(opts.plate));
        if let Some(hit) = self.cmyk_view.as_ref().filter(|entry| entry.matches(doc, spreads, width, height, view, opts, self.threads)) {
            let image = hit.image();
            self.stats = hit.stats();
            self.stats.micros = now().saturating_sub(start);
            return image;
        }
        // Release the old snapshot before rendering an edited document or resized viewport.
        self.cmyk_view = None;
        self.cmyk_ran = false;
        let image = self.render_uncached(doc, cache, spreads, width, height, view, opts);
        // Kept only when the ink passes ran: other views render at their usual cost anyway.
        if self.cmyk_ran && image.pixels.len() <= blendspace::CachedView::MAX_BYTES {
            self.cmyk_view = Some(blendspace::CachedView::new(doc, spreads, view, opts, &image, self.stats, self.threads));
        }
        self.stats.micros = now().saturating_sub(start);
        image
    }
    #[allow(clippy::too_many_arguments)]
    fn render_uncached(
        &mut self,
        doc: &Document,
        cache: &Cache,
        spreads: &[Placed],
        width: u32,
        height: u32,
        view: Affine,
        opts: &RenderOptions,
    ) -> Rendered {
        let start = now();
        let w = width.clamp(1, MAX_SIDE) as u16;
        let h = height.clamp(1, MAX_SIDE) as u16;
        let mut ctx = match self.ctx.take() {
            Some(mut c) if c.width() == w && c.height() == h && c.render_settings().num_threads == self.threads => {
                c.reset();
                c
            }
            _ => RenderContext::new_with(w, h, vello_cpu::RenderSettings { num_threads: self.threads, ..Default::default() }),
        };
        let mt = ctx.render_settings().num_threads > 0;
        RICH_BLACK.with(|r| r.set(opts.rich_black));
        PLATE.with(|p| p.set(opts.plate));
        self.stats = FrameStats::default();
        self.stories.clear();
        self.glyphs.tick();
        if let Some(bg) = opts.background {
            ctx.set_transform(Affine::IDENTITY);
            ctx.set_paint(peniko::Color::from_rgba8(bg[0], bg[1], bg[2], bg[3]));
            ctx.fill_rect(&kurbo::Rect::new(0.0, 0.0, w as f64, h as f64));
        }
        let px = 1.0 / view.determinant().abs().sqrt().max(1e-12);
        let canvas_visible = view.inverse().transform_rect_bbox(Rect::new(0.0, 0.0, w as f64, h as f64));
        for pl in spreads {
            let Some(sp) = doc.spread(pl.spread) else { continue };
            let sview = view * pl.xf;
            let visible = pl.xf.inverse().transform_rect_bbox(canvas_visible);
            let bounds = sp.bounds();
            // Cull spreads far outside the viewport (pasteboard items may extend; allow a margin).
            if !rect_overlaps(bounds.inflate(2000.0, 2000.0), visible) {
                continue;
            }
            let f = Frame { doc, cache, view: sview, visible, px, opts, mt };
            self.draw_spread(&mut ctx, &f, pl.spread);
        }
        ctx.flush();
        // Render straight into the returned buffer (no extra copy of the frame).
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        if let Some(pm) = vello_cpu::PixmapMut::new(w, h, &mut pixels) {
            ctx.render(pm, &mut self.resources);
        }
        if opts.blend_space_view && doc.settings.blend_space == designcraft_doc::BlendSpace::Cmyk && opts.plate.is_none() {
            let mut img = Rendered { width: w as u32, height: h as u32, pixels };
            for pl in spreads {
                let Some(sp) = doc.spread(pl.spread) else { continue };
                if blendspace::uses_rgb_proof(doc, pl.spread, opts) {
                    let r = (view * pl.xf).transform_rect_bbox(sp.bounds());
                    blend_space_view(&mut img, r);
                }
            }
            pixels = img.pixels;
        }
        self.ctx = Some(ctx);
        self.stories.clear();
        self.stats.micros = now().saturating_sub(start);
        Rendered { width: w as u32, height: h as u32, pixels }
    }

    /// Render one document page (absolute index) at `scale` px/pt, optionally including bleed.
    pub fn render_page(&mut self, doc: &Document, cache: &Cache, abs: usize, scale: f64, bleed: bool, opts: &RenderOptions) -> Option<Rendered> {
        let (si, pi) = doc.page_loc(abs)?;
        let page = &doc.spreads[si].pages[pi];
        let mut r = page.bounds();
        if bleed {
            let b = doc.settings.bleed;
            let (l, rr) = if page.side == PageSide::Left { (b[3], b[2]) } else { (b[2], b[3]) };
            r = Rect::new(r.x0 - l, r.y0 - b[0], r.x1 + rr, r.y1 + b[1]);
        }
        let w = (r.width() * scale).round().max(1.0) as u32;
        let h = (r.height() * scale).round().max(1.0) as u32;
        if w > MAX_SIDE || h > MAX_SIDE || w as u64 * h as u64 > MAX_PAGE_PIXELS {
            log::warn!("page {abs} at {scale} px/pt would be {w}×{h} pixels: too large to render");
            return None;
        }
        let view = Affine::scale(scale) * Affine::translate(-r.origin().to_vec2());
        let mut o = opts.clone();
        o.background = Some([255, 255, 255, 255]);
        Some(self.render(doc, cache, &[Placed { spread: SpreadRef::Doc(si), xf: Affine::translate(Vec2::ZERO) }], w, h, view, &o))
    }

    fn draw_spread(&mut self, ctx: &mut RenderContext, f: &Frame, sr: SpreadRef) {
        let doc = f.doc;
        let Some(sp) = doc.spread(sr) else { return };
        // Paper.
        if f.opts.paper {
            let paper = doc.resolve_color(designcraft_color::swatch::PAPER, 1.0).unwrap_or(designcraft_color::Color::WHITE);
            if f.opts.page_shadow {
                ctx.set_transform(f.view);
                ctx.set_paint(peniko::Color::from_rgba8(0, 0, 0, 90));
                for p in &sp.pages {
                    let s = 3.0 * f.px;
                    ctx.fill_rect(&(p.bounds() + Vec2::new(s, s)));
                }
            }
            ctx.set_transform(f.view);
            ctx.set_paint(color_of(&paper, 1.0));
            // One rect for equal-height spreads avoids an anti-aliasing seam at the spine.
            if sp.pages.windows(2).all(|w| w[0].height == w[1].height) {
                ctx.fill_rect(&sp.bounds());
            } else {
                for p in &sp.pages {
                    ctx.fill_rect(&p.bounds());
                }
            }
        }
        let grid_area = f.opts.document_grid.iter().find(|(r, _)| *r == sr).map(|(_, a)| *a);
        let grid = &doc.settings.grid;
        if let Some(area) = grid_area.filter(|_| grid.in_back) {
            grid::draw(ctx, f, sp, grid, area);
        }
        if blendspace::uses_cmyk(doc, sr, f.opts) {
            self.draw_cmyk_spread(ctx, f, sr);
            if let Some(area) = grid_area.filter(|_| !grid.in_back) {
                grid::draw(ctx, f, sp, grid, area);
            }
            return;
        }
        // Blend modes blend with the objects below, not with the paper (as in InDesign and in PDF,
        // where the page's group starts transparent): a Color Dodge or Screen object over blank paper
        // shows as it is. So the items are drawn in their own group, laid over the paper once.
        let isolate = f.opts.paper && spread_blends(doc, sp);
        if isolate {
            ctx.push_layer(None, None, None, None, None);
        }
        // Layers back to front; within each layer parent items first.
        for layer in doc.layers.iter().rev() {
            if !layer.visible || f.opts.printing_only && !layer.printable {
                continue;
            }
            // Document pages show their parent and its bases; a parent spread shows its bases.
            let first = match sr {
                SpreadRef::Doc(si) => Some(doc.first_page_of_spread(si)),
                SpreadRef::Parent(_) => None,
            };
            // Base items compose for where they show: the document page, or this parent spread.
            let shown_on = |pi: usize| match (sr, first) {
                (SpreadRef::Parent(p), _) => ShownOn::Parent(p),
                (_, Some(a)) => ShownOn::Page(a + pi),
                (_, None) => ShownOn::Own,
            };
            for (pi, page) in sp.pages.iter().enumerate() {
                if !page.show_parent_items {
                    continue;
                }
                for (ppi, ppage) in doc.shown_parent_pages(sr, pi) {
                    let Some(parent) = doc.parents.get(ppi) else { continue };
                    let Some(ppg) = parent.pages.get(ppage) else { continue };
                    let dx = page.x - ppg.x;
                    for it in &parent.items {
                        if it.layer != layer.id || page.overridden.contains(&it.id) {
                            continue;
                        }
                        // Parent items belong to the parent page they sit on.
                        if parent.page_at_x(it.bounds().center().x) != Some(ppage) && parent.pages.len() > 1 {
                            continue;
                        }
                        self.draw_item(ctx, f, it, Affine::translate((dx, 0.0)), shown_on(pi));
                    }
                }
            }
            for it in &sp.items {
                if it.layer == layer.id {
                    self.draw_item(ctx, f, it, Affine::IDENTITY, ShownOn::Own);
                }
            }
        }
        if isolate {
            ctx.pop_layer();
        }
        if let Some(area) = grid_area.filter(|_| !grid.in_back) {
            grid::draw(ctx, f, sp, grid, area);
        }
    }

    fn draw_item(&mut self, ctx: &mut RenderContext, f: &Frame, it: &Item, parent: Affine, page: ShownOn) {
        if self.drawing_items.len() >= 64 || self.drawing_items.contains(&it.id) {
            return;
        }
        self.drawing_items.push(it.id);
        self.draw_item_inner(ctx, f, it, parent, page);
        self.drawing_items.pop();
    }

    fn draw_item_inner(&mut self, ctx: &mut RenderContext, f: &Frame, it: &Item, parent: Affine, page: ShownOn) {
        if it.hidden || f.opts.hidden.contains(&it.id) || f.opts.printing_only && it.nonprinting {
            return;
        }
        let xf = parent * it.xf;
        let vb = xf.transform_rect_bbox(it.inner_bounds()).inflate(it.stroke.extent() + 2.0, it.stroke.extent() + 2.0);
        // A frame with items pasted into it draws like any other object (draw_body: fill, the items
        // clipped to the frame, stroke), so its effects apply to all of it.
        if !it.children().is_empty() && !it.has_nested_items() {
            // Groups: children carry their own transforms relative to the group.
            // Isolate Blending / Knockout: the group is its own layer (blend modes stop at it).
            let layered = it.opacity < 0.999 || it.blend != DcBlend::Normal || it.isolate || it.knockout;
            if layered {
                ctx.set_transform(Affine::IDENTITY);
                ctx.push_layer(None, Some(blend_mode(it.blend)), Some(it.opacity), None, None);
            }
            // Gradient feather: the group's objects drawn together, then faded as one.
            let feather = it.effects.gradient_feather.on && f.opts.quality != DisplayQuality::Fast;
            if feather {
                ctx.set_transform(Affine::IDENTITY);
                ctx.push_layer(None, None, None, None, None);
            }
            if it.knockout {
                // Knockout: each object first clears its area of those behind it, so it composites
                // with the group's backdrop rather than with its siblings.
                for c in it.shown_children() {
                    if !c.hidden {
                        ctx.set_transform(Affine::IDENTITY);
                        ctx.push_layer(None, Some(BlendMode::new(Mix::Normal, Compose::DestOut)), None, None, None);
                        ctx.set_transform(f.view * xf * c.xf);
                        ctx.set_paint(peniko::Color::BLACK);
                        ctx.fill_path(&c.path.to_bezpath());
                        ctx.pop_layer();
                    }
                    self.draw_item(ctx, f, c, xf, page);
                }
            } else {
                for c in it.shown_children() {
                    self.draw_item(ctx, f, c, xf, page);
                }
            }
            if feather {
                fx::gradient_feather_mask(ctx, f, it, xf);
                ctx.pop_layer();
            }
            if layered {
                ctx.pop_layer();
            }
            return;
        }
        // Composed text stays inside its frame apart from glyph overhang (italics, swashes, big
        // initials), so text frames get a margin rather than being exempt from culling.
        let margin = match it.content {
            Content::Text(_) => 36.0 + 0.25 * vb.width().max(vb.height()),
            _ => 0.0,
        };
        let effects = it.effects.any() && f.opts.quality != DisplayQuality::Fast;
        let fx_outset = if effects { fx::outset(it) } else { 0.0 };
        if !rect_overlaps(vb.inflate(margin + fx_outset, margin + fx_outset), f.visible) {
            return;
        }
        self.stats.items += 1;
        let bp = if it.corners.is_none() { it.path.to_bezpath() } else { corners::apply(&it.path, &it.corners) };
        let layered = it.opacity < 0.999 || it.blend != DcBlend::Normal;
        if layered {
            ctx.set_transform(Affine::IDENTITY);
            ctx.push_layer(None, Some(blend_mode(it.blend)), Some(it.opacity), None, None);
        }
        if effects {
            self.draw_item_fx(ctx, f, it, &bp, xf, page);
        } else {
            self.draw_body(ctx, f, it, &bp, xf, page);
        }
        if layered {
            ctx.pop_layer();
        }
    }

    /// The composed story for a frame (memoised for the current render: the cache lookup
    /// validates the whole thread each time).
    fn story(&mut self, f: &Frame, sid: StoryId, page: ShownOn) -> Arc<ComposedStory> {
        let key = (sid, page);
        if let Some(cs) = self.stories.get(&key) {
            return cs.clone();
        }
        let cs = f.cache.get_on(f.doc, sid, page);
        self.stories.insert(key, cs.clone());
        cs
    }

    /// Fill, content and stroke of a leaf item (no opacity, blend or effects).
    pub(crate) fn draw_body(&mut self, ctx: &mut RenderContext, f: &Frame, it: &Item, bp: &BezPath, xf: Affine, page: ShownOn) {
        draw_body_fill(ctx, f, it, bp, xf);
        // Content.
        match &it.content {
            Content::Graphic(g) => {
                let g = &if it.media.is_some() { it.drawn_graphic(&f.doc.assets).unwrap_or_else(|| g.clone()) } else { g.clone() };
                ctx.set_transform(f.view * xf);
                ctx.push_clip_layer(bp);
                if !it.pdf_hidden_layers.is_empty() {
                    HIDDEN_LAYERS.with(|h| *h.borrow_mut() = it.pdf_hidden_layers.clone());
                }
                self.draw_graphic(ctx, f, g, xf);
                if !it.pdf_hidden_layers.is_empty() {
                    HIDDEN_LAYERS.with(|h| h.borrow_mut().clear());
                }
                ctx.pop_layer();
            }
            // Items pasted into the frame, clipped by it.
            Content::Group { items } if it.has_nested_items() => {
                ctx.set_transform(f.view * xf);
                ctx.push_clip_layer(bp);
                for c in items {
                    self.draw_item(ctx, f, c, xf, page);
                }
                ctx.pop_layer();
            }
            Content::Text(tf) => {
                let cs = self.story(f, tf.story, page);
                if let Some(ft) = cs.frame(it.id) {
                    match &tf.options.path {
                        Some(pt) => self.draw_path_text(ctx, f, &cs, ft, xf, bp, pt),
                        None => self.draw_text(ctx, f, &cs, ft, xf * f.doc.text_local(it)),
                    }
                }
            }
            _ => {}
        }
        // Form fields (screen only; PDF viewers draw their own): a tint, a check mark.
        if let Some(ff) = &it.form_field
            && !f.opts.printing_only
        {
            let r = it.inner_bounds();
            ctx.set_transform(f.view * xf);
            ctx.set_paint(screen_color([80, 140, 255], 40));
            ctx.fill_rect(&r);
            ctx.set_paint(screen_color([60, 110, 220], 200));
            ctx.set_stroke(kurbo::Stroke::new(0.75));
            ctx.stroke_path(&r.to_path(0.1));
            if ff.kind == designcraft_doc::FieldKind::CheckBox && !ff.value.is_empty() && ff.value != "Off" {
                let (w, h) = (r.width(), r.height());
                let mut c = BezPath::new();
                c.move_to((r.x0 + w * 0.22, r.y0 + h * 0.48));
                c.line_to((r.x0 + w * 0.42, r.y0 + h * 0.72));
                c.line_to((r.x0 + w * 0.78, r.y0 + h * 0.24));
                ctx.set_paint(screen_color([0, 0, 0], 255));
                ctx.set_stroke(kurbo::Stroke::new((w.min(h) * 0.12).max(0.8)).with_caps(kurbo::Cap::Round).with_join(kurbo::Join::Round));
                ctx.stroke_path(&c);
            }
        }
        // Stroke.
        if !it.stroke.is_none() {
            let op = overprints(f, &it.stroke.swatch, it.stroke.tint, it.stroke.overprint);
            if op {
                ctx.set_transform(f.view * xf);
                ctx.push_layer(None, Some(BlendMode::new(Mix::Multiply, Compose::SrcOver)), None, None, None);
            }
            self.draw_stroke(ctx, f, it, bp, xf);
            if op {
                ctx.pop_layer();
            }
        }
    }

    fn draw_stroke(&mut self, ctx: &mut RenderContext, f: &Frame, it: &Item, bp: &BezPath, xf: Affine) {
        self.draw_stroke_painted(ctx, f, it, bp, xf);
        ctx.reset_paint_transform();
    }

    /// The stroke, in its colour or gradient (a gradient runs across the path's box, as a fill's).
    fn draw_stroke_painted(&mut self, ctx: &mut RenderContext, f: &Frame, it: &Item, bp: &BezPath, xf: Affine) {
        let st = &it.stroke;
        let (paint, bounds) = (st.as_fill(), bp.bounding_box());
        if f.doc.resolve_color(&st.swatch, st.tint).is_none() && designcraft_color::swatch::resolve_gradient(&f.doc.swatches, &st.swatch).is_none() {
            return;
        }
        let mut stroke = kurbo::Stroke::new(st.weight)
            .with_caps(match st.cap {
                designcraft_doc::Cap::Butt => kurbo::Cap::Butt,
                designcraft_doc::Cap::Round => kurbo::Cap::Round,
                designcraft_doc::Cap::Projecting => kurbo::Cap::Square,
            })
            .with_join(match st.join {
                designcraft_doc::Join::Miter => kurbo::Join::Miter,
                designcraft_doc::Join::Round => kurbo::Join::Round,
                designcraft_doc::Join::Bevel => kurbo::Join::Bevel,
            })
            .with_miter_limit(st.miter_limit);
        let kind = f.doc.stroke_kind(&st.kind);
        let length = designcraft_doc::path_length_bound(bp);
        match &kind {
            StrokeType::Dashed { pattern } if StrokeType::expandable_dashes(pattern, length) => {
                stroke = stroke.with_dashes(0.0, pattern.iter().copied())
            }
            StrokeType::Dotted if StrokeType::expandable_dashes(&[0.0, st.weight * 2.0], length) => {
                stroke = stroke.with_dashes(0.0, [0.0, st.weight * 2.0]).with_caps(kurbo::Cap::Round)
            }
            _ => {}
        }
        ctx.set_transform(f.view * xf);
        if !set_fill_paint(ctx, f.doc, &paint, bounds) {
            return;
        }
        let closed = it.path.is_closed();
        // Arrowheads: the path is shortened under them; they're drawn after it.
        let arrows = designcraft_doc::arrow::apply(bp, st, closed);
        let bp = arrows.as_ref().map_or(bp, |a| &a.0);
        // Gap colour under dashes and dots: the whole path, undashed.
        let gap = if matches!(kind, StrokeType::Solid) { None } else { f.doc.resolve_color(&st.gap_swatch, st.gap_tint) };
        if let Some(g) = gap
            && !matches!(st.align, StrokeAlign::Inside | StrokeAlign::Outside if closed)
        {
            let op = overprints(f, &st.gap_swatch, st.gap_tint, st.gap_overprint);
            if op {
                ctx.set_transform(Affine::IDENTITY);
                ctx.push_layer(None, Some(BlendMode::new(Mix::Multiply, Compose::SrcOver)), None, None, None);
                ctx.set_transform(f.view * xf);
            }
            ctx.set_paint(color_of(&g, 1.0));
            ctx.set_stroke(kurbo::Stroke {
                dash_pattern: Default::default(),
                start_cap: kurbo::Cap::Butt,
                end_cap: kurbo::Cap::Butt,
                ..stroke.clone()
            });
            ctx.stroke_path(bp);
            if op {
                ctx.pop_layer();
            }
            set_fill_paint(ctx, f.doc, &paint, bounds);
        }
        // Stripes, wavy and hash strokes are fills (along the path's centre line).
        if let Some(o) = kind.outline(bp, st.weight, 0.05 * f.px) {
            ctx.fill_path(&o);
            for h in arrows.iter().flat_map(|a| &a.1) {
                match h.outline {
                    Some(w) => {
                        ctx.set_stroke(kurbo::Stroke::new(w).with_join(kurbo::Join::Miter));
                        ctx.stroke_path(&h.path);
                    }
                    None => ctx.fill_path(&h.path),
                }
            }
            return;
        }
        match st.align {
            StrokeAlign::Inside if closed => {
                // Clip a double-width stroke to the path.
                ctx.push_clip_layer(bp);
                ctx.set_stroke(kurbo::Stroke { width: st.weight * 2.0, ..stroke });
                ctx.stroke_path(bp);
                ctx.pop_layer();
            }
            StrokeAlign::Outside if closed => {
                // Subtract the filled path from a double-width stroke. Combining a stroked
                // outline (already hollow) with the path using even-odd would paint its centre.
                // Isolate the stroke so the subtraction leaves the item's fill/content and
                // backdrop intact; the same nonzero fill rule also preserves compound holes.
                ctx.push_layer(None, None, None, None, None);
                ctx.set_stroke(kurbo::Stroke { width: st.weight * 2.0, ..stroke });
                ctx.stroke_path(bp);
                ctx.push_layer(None, Some(BlendMode::new(Mix::Normal, Compose::DestOut)), None, None, None);
                ctx.fill_path(bp);
                ctx.pop_layer();
                ctx.pop_layer();
            }
            _ => {
                ctx.set_stroke(stroke);
                ctx.stroke_path(bp);
            }
        }
        for h in arrows.iter().flat_map(|a| &a.1) {
            match h.outline {
                Some(w) => {
                    ctx.set_stroke(kurbo::Stroke::new(w).with_join(kurbo::Join::Miter));
                    ctx.stroke_path(&h.path);
                }
                None => ctx.fill_path(&h.path),
            }
        }
    }

    /// Draw a placed graphic, inside its clipping path when it has one.
    pub(crate) fn draw_graphic(&mut self, ctx: &mut RenderContext, f: &Frame, g: &designcraft_doc::Graphic, xf: Affine) {
        let Some((region, even_odd)) = g.clip_region() else {
            self.draw_graphic_unclipped(ctx, f, g, xf);
            return;
        };
        ctx.set_transform(f.view * xf);
        if even_odd {
            ctx.set_fill_rule(peniko::Fill::EvenOdd);
        }
        ctx.push_clip_layer(&region);
        ctx.set_fill_rule(peniko::Fill::NonZero);
        self.draw_graphic_unclipped(ctx, f, g, xf);
        ctx.pop_layer();
    }

    fn draw_graphic_unclipped(&mut self, ctx: &mut RenderContext, f: &Frame, g: &designcraft_doc::Graphic, xf: Affine) {
        let Some(asset) = f.doc.assets.get(&g.asset) else { return };
        if designcraft_doc::media_kind(&asset.mime).is_some() {
            // Video / sound: a dark frame with a play mark (the poster, when set, is drawn by the item).
            ctx.set_transform(f.view * xf * g.xf);
            ctx.set_paint(screen_color([48, 48, 52], 255));
            ctx.fill_rect(&Rect::new(0.0, 0.0, g.size.0, g.size.1));
            let (cx, cy, r) = (g.size.0 / 2.0, g.size.1 / 2.0, g.size.0.min(g.size.1) * 0.18);
            let mut tri = BezPath::new();
            tri.move_to((cx - r * 0.6, cy - r));
            tri.line_to((cx + r, cy));
            tri.line_to((cx - r * 0.6, cy + r));
            tri.close_path();
            ctx.set_paint(screen_color([235, 235, 235], 255));
            ctx.fill_path(&tri);
            return;
        }
        if f.opts.quality == DisplayQuality::Fast {
            ctx.set_transform(f.view * xf * g.xf);
            ctx.set_paint(screen_color([178, 178, 178], 255));
            ctx.fill_rect(&Rect::new(0.0, 0.0, g.size.0, g.size.1));
            return;
        }
        // Pick a mip level close to the on-screen size (Typical: a 72 ppi proxy).
        let on_screen = (f.view * xf * g.xf).determinant().abs().sqrt();
        let on_screen = if f.opts.quality == DisplayQuality::Typical { on_screen.min(1.0) } else { on_screen };
        let hidden = HIDDEN_LAYERS.with(|h| h.borrow().clone());
        let data = if !hidden.is_empty() && is_pdf(&asset.data) {
            // Stale persisted/relinked choices must not reveal hidden source content.
            let Some(variant) = pdf_layers::layered(&asset.data, &hidden) else { return };
            variant
        } else {
            asset.data.clone()
        };
        // A placed PDF cropped to a box: the whole page, placed so that the box spans graphic space.
        let [l, t, r, b] = shown_box(asset).unwrap_or([0.0, 0.0, 1.0, 1.0]);
        let (page_w, page_h) = (g.size.0 / (r - l), g.size.1 / (b - t));
        let page = Rect::new(-l * page_w, -t * page_h, (1.0 - l) * page_w, (1.0 - t) * page_h);
        let Some(pm) = images::mip(&data, asset.page, page_w, on_screen) else { return };
        // A coloured image is already in the plate's inks (its colours come from `color_of`).
        let pm = match (colorize::colorized(f.doc, &g.fill, &asset.data, &pm), PLATE.with(std::cell::Cell::get)) {
            (Some(coloured), _) => coloured,
            (None, Some(plate)) => plate_pixmap(&pm, plate),
            (None, None) => pm,
        };
        // Only where the page is: a box reaching past it (a media box larger than the crop box)
        // shows nothing there.
        let rect = Rect::new(0.0, 0.0, g.size.0, g.size.1).intersect(page);
        ctx.set_transform(f.view * xf * g.xf);
        let sx = page_w / pm.width().max(1) as f64;
        let sy = page_h / pm.height().max(1) as f64;
        ctx.set_paint(vello_cpu::Image {
            image: vello_cpu::ImageSource::Pixmap(pm),
            sampler: peniko::ImageSampler::default().with_quality(peniko::ImageQuality::Medium),
        });
        ctx.set_paint_transform(Affine::translate((page.x0, page.y0)) * Affine::scale_non_uniform(sx, sy));
        ctx.fill_rect(&rect);
        ctx.reset_paint_transform();
    }
}

const CMYK_N: usize = 17;

/// The sRGB → working CMYK table for the current colour settings (17³, built once per settings,
/// trilinear);
/// sample it with [`cmyk_in`]. Fetch it once for many pixels.
pub(crate) fn cmyk_table() -> std::sync::Arc<Vec<[f32; 4]>> {
    const N: usize = CMYK_N;
    type Lut = (designcraft_color::cms::ColorSettings, std::sync::Arc<Vec<[f32; 4]>>);
    static LUT: std::sync::Mutex<Option<Lut>> = std::sync::Mutex::new(None);
    let settings = designcraft_color::cms::active_settings();
    let mut g = LUT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match g.as_ref() {
        Some((s, t)) if *s == settings => t.clone(),
        _ => {
            let cms = designcraft_color::cms::active();
            let step = |i: usize| i as f32 / (N - 1) as f32;
            let t: Vec<[f32; 4]> = (0..N * N * N)
                .map(|i| cms.srgb_to_cmyk([step(i / (N * N)), step(i / N % N), step(i % N)], designcraft_color::cms::Intent::RelativeColorimetric))
                .collect();
            let t = std::sync::Arc::new(t);
            *g = Some((settings, t.clone()));
            t
        }
    }
}

/// Trilinear lookup of `rgb` in a [`cmyk_table`].
pub(crate) fn cmyk_in(table: &[[f32; 4]], rgb: [f32; 3]) -> [f32; 4] {
    const N: usize = CMYK_N;
    let f = rgb.map(|v| v.clamp(0.0, 1.0) * (N - 1) as f32);
    let i = f.map(|v| (v as usize).min(N - 2));
    let d = [f[0] - i[0] as f32, f[1] - i[1] as f32, f[2] - i[2] as f32];
    let mut out = [0.0f32; 4];
    for (corner, w) in (0..8).map(|c| {
        let (a, b, cc) = (c >> 2 & 1, c >> 1 & 1, c & 1);
        let w = (if a == 1 { d[0] } else { 1.0 - d[0] }) * (if b == 1 { d[1] } else { 1.0 - d[1] }) * (if cc == 1 { d[2] } else { 1.0 - d[2] });
        ((i[0] + a) * N * N + (i[1] + b) * N + i[2] + cc, w)
    }) {
        if let Some(t) = table.get(corner) {
            for k in 0..4 {
                out[k] += t[k] * w;
            }
        }
    }
    out
}

/// An image's ink coverage on one process plate, as grey (cached per image and plate).
fn plate_pixmap(pm: &std::sync::Arc<vello_cpu::Pixmap>, plate: u8) -> std::sync::Arc<vello_cpu::Pixmap> {
    // Keep source allocations alive (their addresses are keys), invalidate on colour settings,
    // and cap derived raster memory rather than merely counting differently sized images.
    type Entry = (std::sync::Arc<vello_cpu::Pixmap>, std::sync::Arc<vello_cpu::Pixmap>);
    type Cache = (designcraft_color::cms::ColorSettings, std::collections::HashMap<(usize, u8), Entry>, usize);
    const BUDGET: usize = 64 << 20;
    static CACHE: std::sync::Mutex<Option<Cache>> = std::sync::Mutex::new(None);
    let key = (std::sync::Arc::as_ptr(pm) as usize, plate);
    let settings = designcraft_color::cms::active_settings();
    {
        let mut guard = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((previous, map, _)) = guard.as_ref() {
            if *previous == settings {
                if let Some((_, derived)) = map.get(&key) {
                    return derived.clone();
                }
            } else {
                *guard = None;
            }
        }
    }
    let table = cmyk_table();
    let data: Vec<vello_cpu::color::PremulRgba8> = pm
        .data()
        .iter()
        .map(|p| {
            if p.a == 0 {
                return *p;
            }
            let k = 255.0 / p.a as f32;
            let rgb = [p.r, p.g, p.b].map(|v| (v as f32 * k / 255.0).min(1.0));
            let cmyk = cmyk_in(&table, rgb);
            let ink = |i: usize| ((1.0 - cmyk[i].clamp(0.0, 1.0)) * p.a as f32).round() as u8;
            if plate == 4 {
                return vello_cpu::color::PremulRgba8 { r: ink(0), g: ink(1), b: ink(2), a: p.a };
            }
            let v = ink(plate.min(3) as usize);
            vello_cpu::color::PremulRgba8 { r: v, g: v, b: v, a: p.a }
        })
        .collect();
    let out = std::sync::Arc::new(vello_cpu::Pixmap::from_parts(data, pm.width(), pm.height()));
    let bytes = (pm.width() as usize).saturating_mul(pm.height() as usize).saturating_mul(8);
    if bytes <= BUDGET {
        let mut guard = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (previous, map, used) = guard.get_or_insert_with(|| (settings.clone(), Default::default(), 0));
        if *previous != settings || used.saturating_add(bytes) > BUDGET {
            map.clear();
            *used = 0;
            *previous = settings;
        }
        if map.insert(key, (pm.clone(), out.clone())).is_none() {
            *used += bytes;
        }
    }
    out
}

fn rect_overlaps(a: Rect, b: Rect) -> bool {
    a.x0 < b.x1 && a.x1 > b.x0 && a.y0 < b.y1 && a.y1 > b.y0
}

thread_local! {
    /// Object Layer Options of the placed PDF being drawn.
    static HIDDEN_LAYERS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Appearance of Black for the render on this thread: 100% K as pure black.
    static RICH_BLACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Separations Preview plate for the render on this thread.
    static PLATE: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

/// Screen indicators and placeholders are display sRGB, independent of the document's RGB
/// working profile. Encode their inks only while drawing a process plate or CMYK blend pass.
pub(crate) fn screen_color(rgb: [u8; 3], alpha: u8) -> peniko::Color {
    if PLATE.with(std::cell::Cell::get).is_none() {
        return peniko::Color::from_rgba8(rgb[0], rgb[1], rgb[2], alpha);
    }
    let [c, m, y, k] = cmyk_in(&cmyk_table(), rgb.map(|v| v as f32 / 255.0));
    color_of(&designcraft_color::Color::Cmyk { c, m, y, k }, alpha as f32 / 255.0)
}

pub fn color_of(c: &designcraft_color::Color, alpha: f32) -> peniko::Color {
    if let Some(p) = PLATE.with(std::cell::Cell::get) {
        let cmyk = match *c {
            designcraft_color::Color::Cmyk { c, m, y, k } => [c, m, y, k],
            _ => c.to_cmyk_managed(designcraft_color::cms::Intent::RelativeColorimetric),
        };
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        let ink = |i: usize| ((1.0 - cmyk[i].clamp(0.0, 1.0)) * 255.0).round() as u8;
        if p == 4 {
            return peniko::Color::from_rgba8(ink(0), ink(1), ink(2), a);
        }
        let v = ink(p.min(3) as usize);
        return peniko::Color::from_rgba8(v, v, v, a);
    }
    if RICH_BLACK.with(std::cell::Cell::get)
        && let designcraft_color::Color::Cmyk { c: cc, m, y, k } = *c
        && cc <= 1e-3
        && m <= 1e-3
        && y <= 1e-3
        && k >= 0.999
    {
        return peniko::Color::from_rgba8(0, 0, 0, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8);
    }
    let [r, g, b, a] = c.to_rgba8(alpha);
    peniko::Color::from_rgba8(r, g, b, a)
}

/// Set a swatch fill (solid or gradient) as the paint. Returns false for [None]/unknown.
fn set_fill_paint(ctx: &mut RenderContext, doc: &Document, fill: &designcraft_doc::Fill, bounds: Rect) -> bool {
    let (swatch, tint) = (fill.swatch.as_str(), fill.tint);
    if let Some(g) = designcraft_color::swatch::resolve_gradient(&doc.swatches, swatch) {
        // Sampled so process colours blend as inks, as InDesign shows them (and as the PDF's CMYK
        // shading prints).
        let stops: Vec<peniko::ColorStop> = g.display_stops().iter().map(|(o, c, a)| peniko::ColorStop::from((*o, color_of(c, *a)))).collect();
        if stops.is_empty() {
            return false;
        }
        let grad = match g.kind {
            // Radial: a circle stretched into the object's ellipse (the paint transform).
            designcraft_color::GradientKind::Radial => {
                let (c, rx, ry) = fill.radial_ellipse(bounds);
                let rx = rx.max(1e-3);
                ctx.set_paint_transform(Affine::translate(c.to_vec2()) * Affine::scale_non_uniform(1.0, ry / rx) * Affine::translate(-c.to_vec2()));
                peniko::Gradient::new_radial(c, rx as f32).with_stops(stops.as_slice())
            }
            _ => {
                let (a, b) = fill.linear_axis(bounds);
                peniko::Gradient::new_linear(a, b).with_stops(stops.as_slice())
            }
        };
        ctx.set_paint(grad);
        return true;
    }
    match doc.resolve_color(swatch, tint) {
        Some(c) => {
            ctx.set_paint(color_of(&c, 1.0));
            true
        }
        None => false,
    }
}

/// Overprint Preview: does this paint overprint (its own flag, or 100% [Black], which
/// overprints by default)? It's then multiplied over what's beneath.
pub(crate) fn overprints(f: &Frame, swatch: &str, tint: f32, flag: bool) -> bool {
    f.opts.overprint_preview && (flag || (f.doc.settings.overprint_black && swatch == designcraft_color::swatch::BLACK && tint >= 0.999))
}

/// Does any item on the spread, or on the parent pages it shows, use a blend mode other than
/// Normal (groups and pasted-in items included)?
fn spread_blends(doc: &Document, sp: &designcraft_doc::Spread) -> bool {
    let blends = |it: &Item| {
        let mut any = false;
        it.walk(&mut |i| any |= i.blend != Default::default());
        any
    };
    sp.items.iter().any(|i| blends(i)) || doc.parents.iter().any(|p| p.items.iter().any(|i| blends(i)))
}

pub fn blend_mode(b: DcBlend) -> BlendMode {
    use DcBlend as B;
    let mix = match b {
        B::Normal => Mix::Normal,
        B::Darken => Mix::Darken,
        B::Multiply => Mix::Multiply,
        B::ColorBurn => Mix::ColorBurn,
        B::Lighten => Mix::Lighten,
        B::Screen => Mix::Screen,
        B::ColorDodge => Mix::ColorDodge,
        B::Overlay => Mix::Overlay,
        B::SoftLight => Mix::SoftLight,
        B::HardLight => Mix::HardLight,
        B::Difference => Mix::Difference,
        B::Exclusion => Mix::Exclusion,
        B::Hue => Mix::Hue,
        B::Saturation => Mix::Saturation,
        B::Color => Mix::Color,
        B::Luminosity => Mix::Luminosity,
    };
    BlendMode::new(mix, Compose::SrcOver)
}

/// Decode encoded image bytes into a premultiplied pixmap.
pub fn decode_pixmap(bytes: &[u8]) -> Option<Pixmap> {
    decode_pixmap_page(bytes, 0)
}

/// [`decode_pixmap`] for page `page` of a PDF (other formats have one page).
pub fn decode_pixmap_page(bytes: &[u8], page: u32) -> Option<Pixmap> {
    // Documents retain the EPS source; only display its TIFF preview or placeholder.
    let proxy = if designcraft_images::is_eps(bytes) { Some(designcraft_images::eps_proxy(bytes)?.0) } else { None };
    let bytes = proxy.as_deref().unwrap_or(bytes);
    if is_pdf(bytes) {
        return render_pdf_page(bytes, page as usize, 3000);
    }
    // A WMF is replayed as SVG; both draw as vectors with a transparent background.
    let wmf_svg;
    let svg = if designcraft_images::is_wmf(bytes) {
        wmf_svg = designcraft_images::wmf_svg(bytes)?;
        Some(wmf_svg.as_bytes())
    } else if designcraft_images::is_svg(bytes) {
        Some(bytes)
    } else {
        None
    };
    if let Some(svg) = svg {
        let (px, w, h) = designcraft_images::render_svg(svg, 3000)?;
        let data = px.as_chunks::<4>().0.iter().map(|p| vello_cpu::color::PremulRgba8 { r: p[0], g: p[1], b: p[2], a: p[3] }).collect();
        return Some(Pixmap::from_parts(data, w.min(u16::MAX as u32) as u16, h.min(u16::MAX as u32) as u16));
    }
    let img = designcraft_images::decode_rgba(bytes)?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
        return None;
    }
    let data: Vec<vello_cpu::color::PremulRgba8> = img
        .pixels()
        .map(|p| {
            let a = p[3] as u16;
            let m = |c: u8| ((c as u16 * a + 127) / 255) as u8;
            vello_cpu::color::PremulRgba8 { r: m(p[0]), g: m(p[1]), b: m(p[2]), a: p[3] }
        })
        .collect();
    Some(Pixmap::from_parts(data, w as u16, h as u16))
}

/// Box-filter downsample by 2.
fn halve(pm: &Pixmap) -> Pixmap {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let src = pm.data();
    let mut out = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        for x in 0..nw {
            let mut acc = [0u32; 4];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let sx = (2 * x + dx).min(w - 1);
                let sy = (2 * y + dy).min(h - 1);
                let p = src[sy * w + sx];
                acc[0] += p.r as u32;
                acc[1] += p.g as u32;
                acc[2] += p.b as u32;
                acc[3] += p.a as u32;
            }
            out.push(vello_cpu::color::PremulRgba8 { r: (acc[0] / 4) as u8, g: (acc[1] / 4) as u8, b: (acc[2] / 4) as u8, a: (acc[3] / 4) as u8 });
        }
    }
    Pixmap::from_parts(out, nw as u16, nh as u16)
}

#[cfg(not(target_arch = "wasm32"))]
fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}
#[cfg(target_arch = "wasm32")]
fn now() -> u64 {
    0
}

/// Pixel size of an encoded image without decoding it fully.
pub fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if designcraft_images::is_eps(bytes) {
        return designcraft_images::eps_pixel_size(bytes);
    }
    if is_pdf(bytes) {
        // Placed PDFs are sized by their page (crop box) in points.
        return pdf_page_size(bytes, 0).map(|(w, h)| (w.round().max(1.0) as u32, h.round().max(1.0) as u32));
    }
    if designcraft_images::is_svg(bytes) {
        // SVGs by their width and height (CSS pixels → points).
        return designcraft_images::natural_size(bytes).map(|(w, h)| (w.round().max(1.0) as u32, h.round().max(1.0) as u32));
    }
    designcraft_images::pixel_size(bytes)
}

/// Natural size of a placed graphic, unrounded: a PDF page (crop box) and an SVG in points, a raster
/// in pixels, an EPS its bounding box independently of preview pixels. Placed graphics are drawn at
/// this size; `image_size` rounds it to whole pixels for display.
pub fn natural_size(bytes: &[u8], page: usize) -> Option<(f64, f64)> {
    if is_pdf(bytes) {
        return pdf_page_size(bytes, page).map(|(w, h)| (w.max(1e-3), h.max(1e-3)));
    }
    designcraft_images::natural_size(bytes)
}

/// MIME type guess for encoded image bytes.
/// Is this a PDF file (placed PDFs are graphics)?
pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF")
}

/// Page count of a PDF.
pub fn pdf_page_count(bytes: &[u8]) -> Option<usize> {
    let pdf = craft_pdf::open_slice(bytes, None).ok()?;
    Some(pdf.pages().len())
}

/// Size of PDF page `page` (0-based), in points, as it shows (crop box, rotation applied).
pub fn pdf_page_size(bytes: &[u8], page: usize) -> Option<(f64, f64)> {
    let pdf = craft_pdf::open_slice(bytes, None).ok()?;
    let p = pdf.pages().get(page)?;
    let (w, h) = p.render_dimensions();
    Some((w as f64, h as f64))
}

/// A placed page could not be rasterized completely for export.
#[derive(Debug, thiserror::Error)]
pub enum PdfRasterError {
    #[error("{0}")]
    Input(#[source] craft_pdf::OpenError),
    #[error("{0}")]
    Render(#[from] craft_pdf::raster::Error),
    #[error("the PDF page could not be read or rendered")]
    InvalidPage,
    #[error("PDF page {page} is unavailable; choose an existing page in Image Import Options or place the replacement PDF again")]
    MissingPage { page: usize },
    #[error("the selected PDF layers could not be hidden")]
    LayerRewrite,
    #[error("an image could not be decoded or exceeded the PDF renderer's resource limits; repair or replace this placed PDF before exporting again")]
    ImageDecode,
    #[error("the PDF renderer encountered an unsupported font")]
    UnsupportedFont,
    #[error(
        "the PDF renderer could not read all page content or reached its resource limits; repair or replace this placed PDF before exporting again"
    )]
    ContentTruncated,
}

/// Reject oversized PDFs before placement, layer rewriting or output allocation.
pub fn check_pdf_input_size(size: usize) -> Result<(), PdfRasterError> {
    craft_pdf::check_input_size(size).map_err(PdfRasterError::Input)
}

/// PDF page `page` as PNG, its longer side about `max_side` pixels, with the `hidden` layers off.
/// Only the `shown` part ([`designcraft_doc::Asset::shown_box`]) is kept when given.
/// Returns `None` on the same failures as [`pdf_page_png_checked`].
pub fn pdf_page_png(bytes: &[u8], page: usize, max_side: u32, hidden: &[String], shown: Option<[f64; 4]>) -> Option<Vec<u8>> {
    pdf_page_png_checked(bytes, page, max_side, hidden, shown).ok()
}

/// Rasterize a placed page for export, preserving available interpreter diagnostics.
/// This cannot detect backend approximations that do not emit an interpreter warning.
pub fn pdf_page_png_checked(bytes: &[u8], page: usize, max_side: u32, hidden: &[String], shown: Option<[f64; 4]>) -> Result<Vec<u8>, PdfRasterError> {
    check_pdf_input_size(bytes.len())?;
    let data = if hidden.is_empty() {
        std::borrow::Cow::Borrowed(bytes)
    } else {
        std::borrow::Cow::Owned(pdf_hide_layers(bytes, hidden).ok_or(PdfRasterError::LayerRewrite)?)
    };
    let raster = render_pdf_raster(&data, page, max_side)?;
    if raster.diagnostics.image_decode_failed {
        return Err(PdfRasterError::ImageDecode);
    }
    if raster.diagnostics.unsupported_font {
        return Err(PdfRasterError::UnsupportedFont);
    }
    if raster.diagnostics.content_truncated {
        return Err(PdfRasterError::ContentTruncated);
    }
    let pm = raster_pixmap(&raster);
    let pm = match shown {
        Some(frac) => crop_pixmap(&pm, frac).ok_or(PdfRasterError::InvalidPage)?,
        None => pm,
    };
    let pixels = pm.data().iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
    Ok(Rendered { width: u32::from(pm.width()), height: u32::from(pm.height()), pixels }.to_png())
}

/// Rasterize PDF page `page` so its longer side is about `max_side` pixels (screen display).
/// Export callers that need failure diagnostics use [`pdf_page_png_checked`].
pub fn render_pdf_page(bytes: &[u8], page: usize, max_side: u32) -> Option<Pixmap> {
    #[cfg(test)]
    PDF_RENDERS.with(|n| n.set(n.get() + 1));
    let raster = render_pdf_raster(bytes, page, max_side).ok()?;
    Some(raster_pixmap(&raster))
}

fn raster_pixmap(raster: &craft_pdf::raster::Raster) -> Pixmap {
    let data = raster.bytes().as_chunks::<4>().0.iter().map(|c| vello_cpu::color::PremulRgba8 { r: c[0], g: c[1], b: c[2], a: c[3] }).collect();
    Pixmap::from_parts(data, raster.width as u16, raster.height as u16)
}

#[cfg(test)]
mod tests;
