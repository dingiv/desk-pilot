//! egui renderer for the declarative [`ui::View`] tree — the render layer.
//!
//! [`render_view`] walks a `View` tree into egui widgets and returns the
//! interactions as [`ui::Msg`]s. This is the ONLY place that knows egui: the
//! business logic produces pure `View`s, the platform hands them here, and the
//! `Msg`s flow back to the app's `update`.
//!
//! # Flex layout (乞丐版)
//! `Column`/`Row` render children sequentially. Children wrapped in `View::Sized`
//! with `flex_grow > 0` get a share of the remaining main-axis space (allocated
//! via `ui.allocate_ui_with_layout`). Fixed children (flex_grow=0 or no Sized
//! wrapper) render normally — egui auto-sizes. Limitation: flexible children
//! should come after fixed ones.

use std::collections::HashMap;

use ui::{Color, FlexStyle, Id, ImageSource, Msg, View};

type ImgKey = (String, u32, u32);

/// Render `view` into `ctx`, appending interactive widget rects to `rects`.
/// Returns the interactions that occurred this frame.
pub fn render_view(
    ctx: &egui::Context,
    view: &View,
    scratch: &mut HashMap<Id, String>,
    rects: &mut Vec<egui::Rect>,
    img_cache: &mut HashMap<ImgKey, egui::TextureHandle>,
    focused_rect: &mut Option<egui::Rect>,
    preedit: &str,
    focused_id: &mut Option<Id>,
) -> Vec<Msg> {
    let mut msgs = Vec::new();
    let mut frame = egui::Frame::default();
    frame.fill = egui::Color32::TRANSPARENT;
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        render_node(ui, ctx, view, scratch, rects, &mut msgs, img_cache, focused_rect, preedit, focused_id);
    });
    msgs
}

/// Extract the flex style from a child (if wrapped in `View::Sized`).
/// Returns `(style, &View)` — the style + the (possibly unwrapped) inner view.
fn peel_flex(view: &View) -> (FlexStyle, &View) {
    match view {
        View::Sized { style, child } => (style.clone(), child.as_ref()),
        other => (FlexStyle::default(), other),
    }
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
    focused_rect: &mut Option<egui::Rect>,
    preedit: &str,
    focused_id: &mut Option<Id>,
) {
    match view {
        // ── display ──────────────────────────────────────────────────────
        View::Text { text, color, size } => {
            let mut rich = egui::RichText::new(text).size(*size);
            if let Some(c) = color {
                rich = rich.color(to_color32(*c));
            }
            ui.label(rich);
        }
        View::Circle { radius, color } => {
            let (rect, _) = ui.allocate_exact_size(
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

        // ── interactive ──────────────────────────────────────────────────
        View::Button { label, id } => {
            let r = ui.button(label);
            if r.clicked() {
                msgs.push(Msg::Clicked(*id));
            }
            rects.push(r.rect);
        }
        View::TextEdit { id, text, multiline, desired_rows, .. } => {
            let entry = scratch.entry(*id).or_insert_with(|| text.clone());
            let real_len = entry.len();
            let has_preedit = *focused_id == Some(*id) && !preedit.is_empty();
            if has_preedit {
                eprintln!("[ime] render: injecting preedit={preedit:?} at len={real_len} (focused_id={:?} this_id={id})", *focused_id);
                entry.push_str(preedit);
            }
            // Build a preedit-aware layouter (must outlive `te`).
            // Capture egui's default text style so the non-preedit portion
            // renders identically to when no layouter is used.
            let text_color = ui.visuals().widgets.active.text_color();
            let body_style = ui.style().text_styles.get(&egui::TextStyle::Body);
            let font_id = body_style
                .map(|s| egui::FontId::new(s.size, s.family.clone()))
                .unwrap_or_else(|| egui::FontId::proportional(14.0));
            eprintln!("[ime] layouter style: text_color={:?} font_id={:?} body_style={:?}",
                text_color, font_id, body_style.map(|s| (s.size, &s.family)));
            let split = real_len;
            let mut preedit_layouter = move |ui: &egui::Ui, text: &str, _: f32| -> std::sync::Arc<egui::Galley> {
                let mut job = egui::text::LayoutJob::default();
                let split = split.min(text.len());
                eprintln!("[ime] layouter called: text={text:?} split={split} text_color={text_color:?} font={font_id:?}");
                if split > 0 {
                    // Real text — match egui's default style exactly.
                    job.append(&text[..split], 0.0, egui::TextFormat {
                        font_id: font_id.clone(),
                        color: text_color,
                        ..Default::default()
                    });
                }
                if split < text.len() {
                    // Preedit text — same font/color + highlighted background.
                    job.append(&text[split..], 0.0, egui::TextFormat {
                        font_id: font_id.clone(),
                        color: text_color,
                        background: egui::Color32::from_rgb(0x3a, 0x5a, 0x8a),
                        ..Default::default()
                    });
                }
                ui.fonts(|f| f.layout_job(job))
            };

            let mut te = if *multiline {
                egui::TextEdit::multiline(entry)
                    .desired_rows(*desired_rows)
                    .desired_width(ui.available_width())
            } else {
                egui::TextEdit::singleline(entry)
            };
            if has_preedit {
                te = te.layouter(&mut preedit_layouter);
            }
            let output = te.show(ui);
            // Strip the preedit text we temporarily appended — the real text
            // stays intact for the model; only the rendered frame showed it.
            if has_preedit {
                entry.truncate(real_len);
            }
            let r = &output.response;
            if r.changed() && !has_preedit {
                msgs.push(Msg::TextChanged(*id, entry.clone()));
            } else if entry.as_str() != text.as_str() && !has_preedit {
                *entry = text.clone();
            }
            rects.push(r.rect);
            if r.has_focus() {
                if *focused_id != Some(*id) {
                    eprintln!("[ime] focus gained: id={id}, setting focused_id");
                }
                *focused_id = Some(*id);
                if let Some(cr) = &output.cursor_range {
                    let caret = output.galley.pos_from_cursor(&cr.primary);
                    let abs_min = output.galley_pos + caret.min.to_vec2();
                    *focused_rect = Some(egui::Rect::from_min_size(
                        abs_min,
                        egui::vec2(1.0, caret.height()),
                    ));
                } else {
                    *focused_rect = Some(r.rect);
                }
            }
        }
        View::Column { children, spacing } => {
            render_flex(ui, ctx, children, *spacing, true, scratch, rects, msgs, img_cache, focused_rect, preedit, focused_id);
        }
        View::Row { children, spacing } => {
            render_flex(ui, ctx, children, *spacing, false, scratch, rects, msgs, img_cache, focused_rect, preedit, focused_id);
        }
        View::Container { color, padding, child } => {
            let mut frame = egui::Frame::default();
            if let Some(c) = color {
                frame.fill = to_color32(*c);
            }
            frame.inner_margin = egui::Margin::symmetric(*padding, *padding);
            frame.show(ui, |ui| render_node(ui, ctx, child, scratch, rects, msgs, img_cache, focused_rect, preedit, focused_id));
        }
        View::Sized { style, child } => {
            // Apply size constraints (flex_grow is handled by the parent flex).
            apply_constraints(ui, style);
            render_node(ui, ctx, child, scratch, rects, msgs, img_cache, focused_rect, preedit, focused_id);
        }
        View::Decorated { decoration, child } => {
            let mut frame = egui::Frame::default();
            if let Some(bg) = decoration.background {
                frame.fill = to_color32(bg);
            }
            if let Some(bc) = decoration.border_color {
                frame.stroke = egui::Stroke::new(decoration.border_width, to_color32(bc));
            }
            if decoration.corner_radius > 0.0 {
                frame.rounding = egui::Rounding::same(decoration.corner_radius);
            }
            if decoration.padding > 0.0 {
                frame.inner_margin = egui::Margin::symmetric(decoration.padding, decoration.padding);
            }
            frame.show(ui, |ui| render_node(ui, ctx, child, scratch, rects, msgs, img_cache, focused_rect, preedit, focused_id));
        }
        View::ScrollView { child, stick_to_bottom } => {
            let mut area = egui::ScrollArea::vertical().auto_shrink([false; 2]);
            if *stick_to_bottom {
                area = area.stick_to_bottom(true);
            }
            area.show(ui, |ui| {
                render_node(ui, ctx, child, scratch, rects, msgs, img_cache, focused_rect, preedit, focused_id);
            });
        }
    }
}

/// Render children with flex layout. `vertical` = Column, `horizontal` = Row.
#[allow(clippy::too_many_arguments)]
fn render_flex(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    children: &[View],
    spacing: f32,
    vertical: bool,
    scratch: &mut HashMap<Id, String>,
    rects: &mut Vec<egui::Rect>,
    msgs: &mut Vec<Msg>,
    img_cache: &mut HashMap<ImgKey, egui::TextureHandle>,
    focused_rect: &mut Option<egui::Rect>,
    preedit: &str,
    focused_id: &mut Option<Id>,
) {
    // Peel flex styles from all children upfront.
    let peeled: Vec<(FlexStyle, &View)> = children.iter().map(peel_flex).collect();
    let total_flex: f32 = peeled.iter().map(|(s, _)| s.flex_grow).filter(|&f| f > 0.0).sum();

    let layout = if vertical {
        egui::Layout::top_down(egui::Align::LEFT)
    } else {
        egui::Layout::left_to_right(egui::Align::TOP)
    };

    // Render each child in sequence inside a sub-ui with the right layout.
    ui.with_layout(layout, |ui| {
        for (i, (style, child)) in peeled.iter().enumerate() {
            if i > 0 && spacing > 0.0 {
                ui.add_space(spacing);
            }

            if style.flex_grow > 0.0 && total_flex > 0.0 {
                // Flexible child — allocate a share of remaining space.
                let avail = ui.available_size();
                let main = if vertical { avail.y } else { avail.x };
                let mut allocated = main * (style.flex_grow / total_flex);

                // Clamp by min/max.
                if let Some(min) = if vertical { style.min_height } else { style.min_width } {
                    allocated = allocated.max(min);
                }
                if let Some(max) = if vertical { style.max_height } else { style.max_width } {
                    allocated = allocated.min(max);
                }

                let size = if vertical {
                    egui::vec2(avail.x, allocated)
                } else {
                    egui::vec2(allocated, avail.y)
                };
                ui.allocate_ui_with_layout(size, layout, |ui| {
                    apply_constraints(ui, style);
                    render_node(ui, ctx, child, scratch, rects, msgs, img_cache, focused_rect, preedit, focused_id);
                });
            } else {
                // Fixed child — apply width/height constraints + render normally.
                apply_constraints(ui, style);
                render_node(ui, ctx, child, scratch, rects, msgs, img_cache, focused_rect, preedit, focused_id);
            }
        }
    });
}

/// Apply FlexStyle size constraints to the current `ui`.
fn apply_constraints(ui: &mut egui::Ui, style: &FlexStyle) {
    if let Some(w) = style.width {
        ui.set_width(w);
    }
    if let Some(h) = style.height {
        ui.set_height(h);
    }
    if let Some(mw) = style.max_width {
        ui.set_max_width(mw);
    }
    if let Some(mh) = style.max_height {
        ui.set_max_height(mh);
    }
    if let Some(mw) = style.min_width {
        ui.set_min_width(mw);
    }
    if let Some(mh) = style.min_height {
        ui.set_min_height(mh);
    }
}

// ── image helpers ────────────────────────────────────────────────────────────

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
