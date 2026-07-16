//! egui renderer for the declarative [`ui::View`] tree — the render layer.
//!
//! [`render_view`] walks a `View` tree into egui widgets and returns the
//! interactions as [`ui::Msg`]s. This is the ONLY place that knows egui: the
//! business logic produces pure `View`s, the platform hands them here, and the
//! `Msg`s flow back to the app's `update`. Swapping renderers (or
//! data-driving `View` for theming) never touches this file's callers.
//!
//! `scratch` holds retained text-edit buffers (per `Id`) so egui's cursor/
//! selection persists across frames; it's reconciled with the model `text`
//! each frame (external model change → reset; user edit → emit `TextChanged`).

use std::collections::HashMap;

use ui::{Color, Id, ImageSource, Msg, View};

/// A loaded pet image, keyed by (asset path, display width, display height):
/// re-resizing only when the asset or the display size changes.
type ImgKey = (String, u32, u32);

/// Render `view` into `ctx`, appending interactive widget rects (for the press
/// hit-test) to `rects`. Returns the interactions that occurred this frame.
/// `img_cache` holds resized pet images (see [`render_image`]).
pub fn render_view(
    ctx: &egui::Context,
    view: &View,
    scratch: &mut HashMap<Id, String>,
    rects: &mut Vec<egui::Rect>,
    img_cache: &mut HashMap<ImgKey, egui::TextureHandle>,
) -> Vec<Msg> {
    let mut msgs = Vec::new();
    // Transparent panel — the wgpu clear (TRANSPARENT) shows through where egui
    // draws nothing, so the window is transparent except the pet body + widgets.
    let mut frame = egui::Frame::default();
    frame.fill = egui::Color32::TRANSPARENT;
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        render_node(ui, ctx, view, scratch, rects, &mut msgs, img_cache);
    });
    msgs
}

#[allow(clippy::too_many_arguments)]
fn render_node(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    view: &View,
    scratch: &mut HashMap<Id, String>,
    rects: &mut Vec<egui::Rect>,
    msgs: &mut Vec<Msg>,
    img_cache: &mut HashMap<ImgKey, egui::TextureHandle>,
) {
    match view {
        View::Text { text, color, size } => {
            let mut rich = egui::RichText::new(text).size(*size);
            if let Some(c) = color {
                rich = rich.color(to_color32(*c));
            }
            ui.label(rich);
        }
        View::Button { label, id } => {
            let r = ui.button(label);
            if r.clicked() {
                msgs.push(Msg::Clicked(*id));
            }
            rects.push(r.rect);
        }
        View::TextEdit { id, text, .. } => {
            // egui edits the retained buffer in place; reconcile with the model.
            let entry = scratch.entry(*id).or_insert_with(|| text.clone());
            let r = ui.text_edit_singleline(entry);
            if r.changed() {
                msgs.push(Msg::TextChanged(*id, entry.clone()));
            } else if entry.as_str() != text.as_str() {
                // no edit this frame but they differ → the model changed externally
                *entry = text.clone();
            }
            rects.push(r.rect);
        }
        View::Circle { radius, color } => {
            let (rect, _resp) = ui.allocate_exact_size(
                egui::vec2(*radius * 2.0, *radius * 2.0),
                egui::Sense::hover(),
            );
            ui.painter().circle_filled(rect.center(), *radius, to_color32(*color));
        }
        View::Image { src, width, height } => {
            let dw = width.round().max(1.0) as u32;
            let dh = height.round().max(1.0) as u32;
            let key = (src.cache_key(), dw, dh);
            let handle = img_cache.entry(key).or_insert_with(|| {
                load_crisp_texture(ctx, src, dw, dh).unwrap_or_else(|e| {
                    eprintln!("[geek-familiar] image load failed: {e}");
                    placeholder_texture(ctx, dw, dh)
                })
            });
            ui.add(
                egui::Image::from_texture(&*handle)
                    .fit_to_exact_size(egui::vec2(dw as f32, dh as f32)),
            );
        }
        View::Column { children } => {
            ui.vertical(|ui| {
                for child in children {
                    render_node(ui, ctx, child, scratch, rects, msgs, img_cache);
                }
            });
        }
        View::Row { children } => {
            ui.horizontal(|ui| {
                for child in children {
                    render_node(ui, ctx, child, scratch, rects, msgs, img_cache);
                }
            });
        }
        View::Container { color, padding, child } => {
            let mut frame = egui::Frame::default();
            if let Some(c) = color {
                frame.fill = to_color32(*c);
            }
            frame.inner_margin = egui::Margin::symmetric(*padding, *padding);
            frame.show(ui, |ui| render_node(ui, ctx, child, scratch, rects, msgs, img_cache));
        }
        View::SizedBox { width, height, child } => {
            if let Some(w) = width {
                ui.set_width(*w);
            }
            if let Some(h) = height {
                ui.set_height(*h);
            }
            render_node(ui, ctx, child, scratch, rects, msgs, img_cache);
        }
    }
}

/// Decode the image (from path or embedded bytes), downscale to `(dw, dh)` with
/// a high-quality Lanczos3 filter (preserving the transparent surround via
/// straight alpha), and upload as a NEAREST-filtered texture for crisp 1:1
/// display.
fn load_crisp_texture(
    ctx: &egui::Context,
    src: &ImageSource,
    dw: u32,
    dh: u32,
) -> Result<egui::TextureHandle, String> {
    let img = match src {
        ImageSource::Path(p) => image::open(p),
        ImageSource::Bytes(b) => image::load_from_memory(b),
    }
    .map_err(|e| e.to_string())?;
    let resized = img.resize_exact(dw, dh, image::imageops::FilterType::Lanczos3);
    let rgba = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Ok(ctx.load_texture(
        &src.cache_key(),
        color_image,
        egui::TextureOptions::NEAREST,
    ))
}

/// A magenta checker so a failed load is visible (not a silent blank).
fn placeholder_texture(ctx: &egui::Context, w: u32, h: u32) -> egui::TextureHandle {
    let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let magenta = ((x / 8 + y / 8) % 2) == 0;
            let c = if magenta { [220, 0, 220, 255] } else { [40, 0, 40, 255] };
            pixels.extend_from_slice(&c);
        }
    }
    ctx.load_texture(
        "pet-placeholder",
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels),
        egui::TextureOptions::NEAREST,
    )
}

fn to_color32(c: Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}
