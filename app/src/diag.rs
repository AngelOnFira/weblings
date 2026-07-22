//! Inline diagnostics: rustc's JSON diagnostics painted INTO the egui editors —
//! column-accurate squiggles, gutter dots and hover tooltips.
//!
//! Data comes from checkRust/runRust results (worker.js parses rustc's
//! `--error-format=json` stderr into `{level, line, col, endLine, endCol,
//! rendered}`). Rendering is painter-only on top of the `TextEditOutput`
//! galley that `egui_code_editor::CodeEditor::show` already returns — no
//! widget fork, same technique as the editors' full-pane focus ring.
use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsValue;

#[derive(Clone)]
pub struct Diag {
    pub is_error: bool,
    /// 1-based, as rustc reports them (char columns).
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub rendered: String,
}

/// Shared between the Leptos shell (writes on check/run results) and the egui
/// editor app (reads every paint).
pub type SharedDiags = Rc<RefCell<Vec<Diag>>>;

fn get_num(v: &JsValue, k: &str) -> Option<f64> {
    js_sys::Reflect::get(v, &JsValue::from_str(k)).ok().and_then(|x| x.as_f64())
}
fn get_str(v: &JsValue, k: &str) -> Option<String> {
    js_sys::Reflect::get(v, &JsValue::from_str(k)).ok().and_then(|x| x.as_string())
}

/// Parse the `diagnostics` array carried by checkRust/runRust results.
/// Span-less diagnostics ("aborting due to N previous errors") drop out via
/// the `line` filter — they have nothing to point at in the editor.
pub fn parse_diags(result: &JsValue) -> Vec<Diag> {
    let arr = match js_sys::Reflect::get(result, &JsValue::from_str("diagnostics")) {
        Ok(a) if js_sys::Array::is_array(&a) => js_sys::Array::from(&a),
        _ => return Vec::new(),
    };
    arr.iter()
        .filter_map(|d| {
            let line = get_num(&d, "line")? as usize;
            Some(Diag {
                is_error: get_str(&d, "level").as_deref() == Some("error"),
                line,
                col: get_num(&d, "col").unwrap_or(1.0) as usize,
                end_line: get_num(&d, "endLine").unwrap_or(line as f64) as usize,
                end_col: get_num(&d, "endCol").unwrap_or(0.0) as usize,
                rendered: get_str(&d, "rendered").unwrap_or_default(),
            })
        })
        .collect()
}

/// Publish counts to `window.__weblings_diags` for the Playwright verifies —
/// canvas-painted markers aren't DOM-assertable.
pub fn publish_counts(diags: &[Diag]) {
    let errors = diags.iter().filter(|d| d.is_error).count();
    let warnings = diags.len() - errors;
    if let Some(w) = web_sys::window() {
        let o = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&o, &"errors".into(), &JsValue::from_f64(errors as f64));
        let _ = js_sys::Reflect::set(&o, &"warnings".into(), &JsValue::from_f64(warnings as f64));
        let _ = js_sys::Reflect::set(&w, &"__weblings_diags".into(), &o);
    }
}

const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(0xfb, 0x49, 0x34); // gruvbox red
const WARN_COLOR: egui::Color32 = egui::Color32::from_rgb(0xfa, 0xbd, 0x2f); // gruvbox yellow

/// Paint markers over the editor and show a tooltip for the hovered line.
/// `pane` is the full editor pane rect (for the gutter x and hover band).
pub fn paint_diags(
    ui: &egui::Ui,
    pane: egui::Rect,
    out: &egui::text_edit::TextEditOutput,
    diags: &[Diag],
) {
    if diags.is_empty() {
        return;
    }
    // Markers must scroll/clip with the text, but the gutter dot sits left of
    // the text clip rect — clip to the whole pane and let row geometry (which
    // already moves with the scrolled galley) place things correctly.
    let painter = ui.painter().with_clip_rect(pane);
    let origin = out.galley_pos.to_vec2();
    let rows = &out.galley.rows;
    let pointer = ui.ctx().pointer_hover_pos();
    let mut hovered: Vec<&Diag> = Vec::new();

    for d in diags {
        // Diagnostics can be stale while the user types — clamp, never panic.
        let Some(placed) = d.line.checked_sub(1).and_then(|i| rows.get(i)) else {
            continue;
        };
        let row_rect = placed.rect().translate(origin);
        let color = if d.is_error { ERROR_COLOR } else { WARN_COLOR };

        // Gutter marker: a thin bar at the pane's left edge (kept off the
        // line numbers so they stay readable).
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(pane.left(), row_rect.top()),
                egui::pos2(pane.left() + 3.0, row_rect.bottom()),
            ),
            egui::CornerRadius::ZERO,
            color,
        );

        // Squiggle under col..end_col (exact glyph offsets via the galley row;
        // multi-line spans underline to the end of the first row).
        let x0 = row_rect.left() + placed.x_offset(egui::epaint::text::CharIndex(d.col.saturating_sub(1)));
        let x1 = if d.end_line == d.line && d.end_col > d.col {
            row_rect.left() + placed.x_offset(egui::epaint::text::CharIndex(d.end_col - 1))
        } else {
            row_rect.right()
        };
        let x1 = x1.max(x0 + 6.0); // keep zero-width spans visible
        squiggle(&painter, x0, x1, row_rect.bottom() - 1.0, color);

        if let Some(p) = pointer {
            if pane.contains(p) && p.y >= row_rect.top() && p.y <= row_rect.bottom() {
                hovered.push(d);
            }
        }
    }

    if !hovered.is_empty() {
        egui::Tooltip::always_open(
            ui.ctx().clone(),
            ui.layer_id(),
            egui::Id::new("diag-tooltip"),
            egui::PopupAnchor::Pointer,
        )
        .show(|ui| {
            ui.set_max_width(560.0);
            for (i, d) in hovered.iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                ui.label(egui::RichText::new(&d.rendered).monospace().size(12.0));
            }
        });
    }
}

fn squiggle(painter: &egui::Painter, x0: f32, x1: f32, y: f32, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.2, color);
    let (step, amp) = (3.0, 1.6);
    let mut prev = egui::pos2(x0, y);
    let mut x = x0;
    let mut up = true;
    while x < x1 {
        x = (x + step).min(x1);
        let p = egui::pos2(x, if up { y - amp } else { y });
        painter.line_segment([prev, p], stroke);
        prev = p;
        up = !up;
    }
}
