//! The code-editing surface, shared by the Playground and Rustlings views.
//!
//! There are two implementations of the same buffer, and exactly one is live:
//!
//! * **Desktop** — an egui canvas (`egui_code_editor`) with syntax highlighting
//!   and the inline diagnostics painted by [`crate::diag`].
//! * **Touch** — a plain `<textarea>`. egui's web backend does open the soft
//!   keyboard (via eframe's hidden text agent), but a canvas cannot offer the
//!   native caret, selection handles, magnifier or copy/paste menu, and its
//!   diagnostics are hover-only — none of which exist on a phone. So on a
//!   coarse-pointer device we don't boot eframe at all and edit DOM text
//!   instead, trading highlighting for an editor that actually works.
//!
//! Both write the same `Rc<RefCell<String>>`, so everything upstream (autosave,
//! Run, examples, exercise select) is unchanged. `EditorHandle::rev` is what
//! lets a *programmatic* buffer replacement reach the textarea: egui is polled
//! every frame and just needs a repaint, but the DOM needs to be told.
use std::cell::RefCell;
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::diag::{self, SharedDiags};

/// The buffer plus the two channels for pushing a programmatic change into
/// whichever editor is live. Cheap to clone (all shared handles).
#[derive(Clone)]
pub struct EditorHandle {
    pub code: Rc<RefCell<String>>,
    pub egui_ctx: Rc<RefCell<Option<egui::Context>>>,
    /// Bumped on every programmatic write; the textarea's sync effect watches it.
    /// `u64` so this stays `Send + Sync` and usable from reactive closures.
    rev: RwSignal<u64>,
}

impl EditorHandle {
    pub fn new(initial: String) -> Self {
        Self {
            code: Rc::new(RefCell::new(initial)),
            egui_ctx: Rc::new(RefCell::new(None)),
            rev: RwSignal::new(0),
        }
    }

    /// Call after ANY write to `code` that did not come from the user typing.
    /// (Typing already notifies both editors by construction.)
    pub fn bump(&self) {
        if let Some(ctx) = self.egui_ctx.borrow().as_ref() {
            ctx.request_repaint();
        }
        self.rev.update(|n| *n = n.wrapping_add(1));
    }

    /// Replace the buffer and notify the live editor.
    pub fn set(&self, src: String) {
        *self.code.borrow_mut() = src;
        self.bump();
    }
}

/// The egui editor app. Shares its text buffer with the Leptos shell via
/// `Rc<RefCell<String>>`, so "Run"/"Examples" can read/replace it. `on_edit`
/// fires on each keystroke so the shell can debounce-save. egui only repaints
/// on input (idle cost ~0).
struct EditorApp {
    code: Rc<RefCell<String>>,
    on_edit: Rc<dyn Fn()>,
    diags: SharedDiags,
    id: &'static str,
}

impl eframe::App for EditorApp {
    // egui/eframe 0.35: App exposes `ui` (a Ui, not a Context); CodeEditor takes the syntax as a
    // `show` argument (no `with_syntax` builder in egui_code_editor 0.3.7).
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Gruvbox: warm dark (#282828) — softer than GitHub Dark's near-black.
        let theme = egui_code_editor::ColorTheme::GRUVBOX;
        // Panel painted in the editor's own background: the code field reaches
        // the bottom of the pane even when the text is short. `with_rows(1)`
        // keeps line numbers tied to actual content (+1 after the trailing
        // newline) instead of padding numbers down the whole pane.
        let frame = egui::Frame::central_panel(ui.style()).fill(theme.bg());
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            let pane = ui.max_rect();
            // The TextEdit's own hover/focus box only wraps the text rows;
            // suppress it and draw our own around the WHOLE pane (below).
            let v = &mut ui.style_mut().visuals;
            v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
            v.widgets.active.bg_stroke = egui::Stroke::NONE;
            v.selection.stroke = egui::Stroke::NONE; // focus ring (selection bg is set by the theme)
            let mut text = self.code.borrow_mut();
            let out = egui_code_editor::CodeEditor::default()
                .id_source(self.id)
                .with_rows(1)
                .with_fontsize(15.0)
                .with_theme(theme)
                .with_numlines(true)
                .show(ui, &mut *text, &egui_code_editor::Syntax::rust());
            let changed = out.response.changed();
            // Release the buffer BEFORE on_edit: it reads `code` to save/check.
            drop(text);
            if changed {
                (self.on_edit)();
            }
            diag::paint_diags(ui, pane, &out, &self.diags.borrow());
            // Clicking the empty area below the text focuses the editor.
            let rest = ui.available_size();
            if rest.y > 0.0 {
                let (_, resp) = ui.allocate_exact_size(rest, egui::Sense::click());
                if resp.clicked() {
                    ui.memory_mut(|m| m.request_focus(out.response.id));
                }
            }
            // Full-pane hover/focus ring: "you are in the code area".
            let hovered = ui.rect_contains_pointer(pane);
            let focused = out.response.has_focus();
            if hovered || focused {
                let color = if focused {
                    egui::Color32::from_gray(170)
                } else {
                    egui::Color32::from_gray(110)
                };
                ui.painter().rect_stroke(
                    pane.shrink(0.5),
                    egui::CornerRadius::ZERO,
                    egui::Stroke::new(1.0, color),
                    egui::StrokeKind::Inside,
                );
            }
        });
    }
}

// --- Touch-editor preferences: text size and soft wrap. ---
// Both are mirrored onto <html> (a custom property and a class) rather than
// applied per element: the textarea and its highlight underlay must agree on
// every metric that affects wrapping, so they read from ONE source.
const KEY_FONT: &str = "editor_font_px";
const KEY_WRAP: &str = "editor_wrap";
const FONT_MIN: u32 = 12;
const FONT_MAX: u32 = 24;
const FONT_DEFAULT: u32 = 16;

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

#[derive(Clone, Copy)]
pub struct EditorPrefs {
    font: RwSignal<u32>,
    wrap: RwSignal<bool>,
}

impl EditorPrefs {
    pub fn load() -> Self {
        let s = storage();
        let font = s
            .as_ref()
            .and_then(|s| s.get_item(KEY_FONT).ok().flatten())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(FONT_DEFAULT)
            .clamp(FONT_MIN, FONT_MAX);
        let wrap = s
            .as_ref()
            .and_then(|s| s.get_item(KEY_WRAP).ok().flatten())
            .map(|v| v != "0")
            .unwrap_or(true);
        Self { font: RwSignal::new(font), wrap: RwSignal::new(wrap) }
    }

    /// Push the current prefs to `<html>` and persist them. Reactive: call it
    /// from an `Effect` and it re-runs whenever either signal changes.
    pub fn apply(&self) {
        let font = self.font.get();
        let wrap = self.wrap.get();
        if let Some(s) = storage() {
            let _ = s.set_item(KEY_FONT, &font.to_string());
            let _ = s.set_item(KEY_WRAP, if wrap { "1" } else { "0" });
        }
        let Some(html) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
        else {
            return;
        };
        if let Some(el) = html.dyn_ref::<web_sys::HtmlElement>() {
            let _ = el.style().set_property("--ed-font", &format!("{font}px"));
        }
        let _ = html.class_list().toggle_with_force("ed-nowrap", !wrap);
    }
}

/// The "Aa" popover: text size steppers + a soft-wrap toggle. Rendered in both
/// toolbars but shown only under `html.is-narrow` — on desktop the egui editor
/// is the one on screen and neither control would reach it.
///
/// `btn_class` lets each toolbar supply its own button chrome (`btn` in the
/// Playground's pill row, `tr-btn` in the Rustlings row) so this doesn't need a
/// third button style.
pub fn editor_settings(prefs: EditorPrefs, btn_class: &'static str) -> impl IntoView {
    let EditorPrefs { font, wrap } = prefs;
    let open = RwSignal::new(false);
    view! {
        <div class="ed-tools">
            <button
                class=format!("{btn_class} ed-btn")
                title="Text size and wrapping"
                on:click=move |_| open.update(|o| *o = !*o)
            >"Aa"</button>
            {move || open.get().then(|| view! {
                <div class="ed-scrim" on:click=move |_| open.set(false)></div>
                <div class="ed-menu">
                    <div class="ed-menu-row">
                        <span class="ed-menu-label">"Text size"</span>
                        <button
                            class="ed-step"
                            disabled=move || { font.get() <= FONT_MIN }
                            on:click=move |_| font.update(|f| *f = f.saturating_sub(1).max(FONT_MIN))
                        >"−"</button>
                        <span class="ed-menu-val">{move || format!("{} px", font.get())}</span>
                        <button
                            class="ed-step"
                            disabled=move || { font.get() >= FONT_MAX }
                            on:click=move |_| font.update(|f| *f = (*f + 1).min(FONT_MAX))
                        >"+"</button>
                    </div>
                    <label class="ed-menu-row">
                        <input
                            type="checkbox"
                            prop:checked=move || wrap.get()
                            on:change=move |ev| wrap.set(event_target_checked(&ev))
                        />
                        <span class="ed-menu-label">"Wrap long lines"</span>
                    </label>
                </div>
            })}
        </div>
    }
}

fn escape_html(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

/// Syntax-highlight `src` into HTML for the textarea's underlay.
///
/// Uses `egui_code_editor`'s own lexer and its gruvbox `format_token` colors —
/// the same two things the canvas editor renders with — so the touch editor is
/// colored identically to the desktop one by construction, not by a second
/// palette someone has to keep in sync.
fn highlight_html(src: &str) -> String {
    let theme = egui_code_editor::ColorTheme::GRUVBOX;
    let syntax = egui_code_editor::Syntax::rust();
    let mut lexer = egui_code_editor::Token::default();
    let mut out = String::with_capacity(src.len() * 2);
    for tok in lexer.tokens(&syntax, src) {
        // Whitespace has nothing to color; skipping the span keeps the DOM
        // roughly half the size on indented code.
        if matches!(tok.ty(), egui_code_editor::TokenType::Whitespace(_)) {
            escape_html(tok.buffer(), &mut out);
            continue;
        }
        // `format_token` also picks a font, which we discard — only .color matters.
        let c = egui_code_editor::format_token(&theme, 16.0, tok.ty()).color;
        out.push_str(&format!("<span style=\"color:#{:02x}{:02x}{:02x}\">", c.r(), c.g(), c.b()));
        escape_html(tok.buffer(), &mut out);
        out.push_str("</span>");
    }
    // A <pre> gives its final line no height unless something follows the last
    // newline; without this the underlay comes up a row short of the textarea
    // (whose own trailing line the caret can sit on) and the two desync.
    out.push('\n');
    out
}

/// A `bool` signal tracking a CSS media query, updated when it changes (device
/// rotation, window resize). Falls back to `false` where `matchMedia` is absent.
pub fn media_signal(query: &str) -> Signal<bool> {
    let mql = web_sys::window().and_then(|w| w.match_media(query).ok().flatten());
    let sig = RwSignal::new(mql.as_ref().map(|m| m.matches()).unwrap_or(false));
    if let Some(m) = mql {
        let m2 = m.clone();
        let cb = Closure::<dyn FnMut()>::new(move || sig.set(m2.matches()));
        m.set_onchange(Some(cb.as_ref().unchecked_ref()));
        // One deliberate leak per query, for the lifetime of the page (the App
        // creates two and never drops them).
        cb.forget();
    }
    Signal::derive(move || sig.get())
}

/// Publish the live editor mode to `window.__weblings_editor` so the Playwright
/// verifies can assert it — "did eframe boot?" is otherwise invisible from the
/// DOM. Mirrors `diag::publish_counts`.
pub fn publish_mode(mode: &str) {
    if let Some(w) = web_sys::window() {
        let o = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&o, &"mode".into(), &JsValue::from_str(mode));
        let _ = js_sys::Reflect::set(&w, &"__weblings_editor".into(), &o);
    }
}

/// Mount both editor surfaces (only one is ever visible, and eframe is booted
/// only in canvas mode). CSS — `html.is-touch` — does the showing/hiding; see
/// `index.html`.
///
/// A plain `fn`, not a `#[component]`: the props builder a component generates
/// would impose bounds that `Rc<RefCell<String>>` cannot meet.
#[allow(clippy::too_many_arguments)]
pub fn code_editor(
    h: EditorHandle,
    on_edit: Rc<dyn Fn()>,
    diags: SharedDiags,
    active: Signal<bool>,
    touch: Signal<bool>,
    id: &'static str,
    canvas_class: &'static str,
    text_class: &'static str,
) -> impl IntoView {
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let ta_ref = NodeRef::<leptos::html::Textarea>::new();
    let hl_ref = NodeRef::<leptos::html::Pre>::new();
    // The highlighted HTML painted *behind* the (transparent-text) textarea.
    // A String signal, not the buffer itself, so the reactive `inner_html`
    // closure stays `Send`.
    let hl = RwSignal::new(String::new());

    // Boot the egui editor onto the canvas exactly once, and only in canvas
    // mode. Returning `true` short-circuits before any signal is read, so the
    // effect drops its subscriptions and never runs again; returning `false`
    // keeps it subscribed to `active`/`touch`, so a desktop window widened past
    // the breakpoint still boots later.
    //
    // NOTE: eframe has no "restart on a different canvas" path, so the canvas
    // stays mounted forever and is hidden with CSS rather than unmounted. That
    // is safe: eframe puts keydown/keyup on the canvas (not the document), so a
    // hidden instance cannot swallow keystrokes meant for the textarea.
    {
        let code = h.code.clone();
        let egui_ctx = h.egui_ctx.clone();
        let on_edit = on_edit.clone();
        let diags = diags.clone();
        Effect::new(move |started: Option<bool>| {
            if started == Some(true) {
                return true;
            }
            // Booting eframe on a display:none canvas gives it a 0x0 surface.
            if !active.get() || touch.get() {
                return false;
            }
            let Some(canvas) = canvas_ref.get() else {
                return false;
            };
            let code = code.clone();
            let egui_ctx = egui_ctx.clone();
            let on_edit = on_edit.clone();
            let diags = diags.clone();
            spawn_local(async move {
                let _ = eframe::WebRunner::new()
                    .start(
                        canvas,
                        eframe::WebOptions::default(),
                        Box::new(move |cc| {
                            // The code pane is ALWAYS dark (Rust-Playground
                            // style); page chrome stays light.
                            cc.egui_ctx.set_visuals(egui::Visuals::dark());
                            *egui_ctx.borrow_mut() = Some(cc.egui_ctx.clone());
                            Ok(Box::new(EditorApp { code, on_edit, diags, id }))
                        }),
                    )
                    .await;
            });
            true
        });
    }

    // Push programmatic buffer changes into the textarea. This has to be an
    // Effect rather than a reactive `prop:value`: attribute/property closures
    // must be `Send` and `code` is an `Rc`. It is also more correct — for a
    // <textarea> the value *attribute* is `defaultValue`, not `value`.
    {
        let code = h.code.clone();
        let rev = h.rev;
        Effect::new(move |first: Option<bool>| {
            rev.get(); // programmatic replacements
            let touch_now = touch.get(); // and re-sync when switching away from the canvas
            let Some(ta) = ta_ref.get() else {
                return false; // not mounted yet; re-runs when the node ref fills
            };
            if first != Some(true) {
                // iOS-only and not in tachys' typed attribute set.
                let _ = ta.set_attribute("autocorrect", "off");
            }
            let want = code.borrow();
            // Guard: an unconditional set_value() would reset the caret to the
            // end on every unrelated bump.
            if ta.value() != *want {
                ta.set_value(&want);
            }
            // Only the touch editor renders the underlay; on desktop this would
            // be pure waste on every example/exercise switch.
            if touch_now {
                hl.set(highlight_html(&want));
            }
            true
        });
    }

    let on_input = {
        let code = h.code.clone();
        let on_edit = on_edit.clone();
        move |ev: leptos::ev::Event| {
            let text = event_target_value(&ev);
            hl.set(highlight_html(&text));
            // The borrow ends at the semicolon — on_edit reads `code` too.
            *code.borrow_mut() = text;
            on_edit();
        }
    };

    // The underlay does not scroll itself (overflow:hidden); it follows the
    // textarea, which is the element the user actually drags. Both axes: with
    // wrapping turned off the textarea scrolls sideways too.
    let on_scroll = move |_: leptos::ev::Event| {
        if let (Some(ta), Some(pre)) = (ta_ref.get_untracked(), hl_ref.get_untracked()) {
            pre.set_scroll_top(ta.scroll_top());
            pre.set_scroll_left(ta.scroll_left());
        }
    };

    view! {
        <canvas class=canvas_class tabindex="0" node_ref=canvas_ref></canvas>
        // Underlay + transparent textarea, exactly overlapping: the browser
        // gives us the caret, selection handles and copy/paste, the <pre>
        // underneath supplies the colors a plain textarea cannot.
        <div class="ed-wrap">
            <pre class="ed-hl" aria-hidden="true" node_ref=hl_ref inner_html=move || hl.get()></pre>
            <textarea
                class=text_class
                node_ref=ta_ref
                on:input=on_input
                on:scroll=on_scroll
                spellcheck="false"
                autocapitalize="off"
                autocomplete="off"
            ></textarea>
        </div>
    }
}
