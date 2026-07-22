//! Rustlings view — 100% client-side: type-check as you type, run the real tests.
//!
//! Type-checks the current exercise ~200 ms after the last keystroke via `window.checkRust`
//! (rustc `--emit metadata` on a background worker); once it compiles clean with no `todo!()`,
//! test-mode exercises are compiled with `--test` and the libtest harness RUNS in the page
//! (`window.runTests`). Exercises are real rust-lang/rustlings.
//! A newer keystroke cancels a check in flight — superseded calls resolve `{ cancelled: true }`
//! and are discarded here.
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use serde::Deserialize;

use crate::diag::{self, SharedDiags};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

#[wasm_bindgen]
extern "C" {
    // public/runner-check.js — type-check only; returns { ok, errorCount, warningCount, compileMs, text }.
    #[wasm_bindgen(js_namespace = window, catch)]
    async fn checkRust(source: String, isTest: bool, constCheck: String, status: &JsValue)
        -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_namespace = window, catch)]
    async fn loadExercisesJson() -> Result<JsValue, JsValue>;
    // public/runner.js — compile with --test and RUN the libtest harness.
    // Returns { ok, passed?, failed?, unsupported?, phase, output, compileMs, execMs }.
    #[wasm_bindgen(js_namespace = window, catch)]
    async fn runTests(source: String, status: &JsValue) -> Result<JsValue, JsValue>;
}

#[derive(Clone, Deserialize)]
struct Exercise {
    name: String,
    dir: String,
    #[serde(default = "yes")]
    test: bool,
    #[serde(default)]
    hint: String,
    exercise: String,
    #[serde(default)]
    solution: Option<String>,
    #[serde(default)]
    const_check: Option<String>,
}
fn yes() -> bool { true }

#[derive(Default, Deserialize)]
struct Meta {
    #[serde(default)]
    source: String,
}

#[derive(Deserialize)]
struct Doc {
    #[serde(default)]
    meta: Meta,
    exercises: Vec<Exercise>,
}

#[derive(Clone, Copy, PartialEq)]
enum Stat {
    Todo,
    Error,
    Compiles,
    Done,
}

fn get_bool(v: &JsValue, k: &str) -> Option<bool> {
    js_sys::Reflect::get(v, &JsValue::from_str(k)).ok().and_then(|x| x.as_bool())
}
fn get_str(v: &JsValue, k: &str) -> Option<String> {
    js_sys::Reflect::get(v, &JsValue::from_str(k)).ok().and_then(|x| x.as_string())
}
fn get_num(v: &JsValue, k: &str) -> Option<f64> {
    js_sys::Reflect::get(v, &JsValue::from_str(k)).ok().and_then(|x| x.as_f64())
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}
fn load_done() -> HashSet<String> {
    storage()
        .and_then(|s| s.get_item("rustlings_done").ok().flatten())
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}
fn save_done(d: &HashSet<String>) {
    if let Some(s) = storage() {
        let _ = s.set_item("rustlings_done", &serde_json::to_string(d).unwrap_or_default());
    }
}

// --- Per-exercise draft persistence: the in-progress buffer survives a reload. ---
const DRAFT_PREFIX: &str = "rustlings_src:";

fn draft_key(name: &str) -> String {
    format!("{DRAFT_PREFIX}{name}")
}
fn load_draft(name: &str) -> Option<String> {
    storage()?.get_item(&draft_key(name)).ok().flatten().filter(|s| !s.is_empty())
}
fn save_draft(name: &str, src: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(&draft_key(name), src);
    }
}
fn clear_draft(name: &str) {
    if let Some(s) = storage() {
        let _ = s.remove_item(&draft_key(name));
    }
}
fn clear_all_drafts() {
    if let Some(s) = storage() {
        // Collect first — removing shifts the indices returned by key(i).
        let mut keys = Vec::new();
        let n = s.length().unwrap_or(0);
        for i in 0..n {
            if let Ok(Some(k)) = s.key(i) {
                if k.starts_with(DRAFT_PREFIX) {
                    keys.push(k);
                }
            }
        }
        for k in keys {
            let _ = s.remove_item(&k);
        }
    }
}
/// Drop all drafts if the bundled exercise set changed (a saved draft would otherwise apply to a
/// different exercise body). Keyed on `doc.meta.source`, e.g. "rustlings v6.4.0".
fn ensure_schema(source: &str) {
    if let Some(s) = storage() {
        let prev = s.get_item("rustlings_schema").ok().flatten();
        if prev.as_deref() != Some(source) {
            clear_all_drafts();
            let _ = s.set_item("rustlings_schema", source);
        }
    }
}
fn clear_all_progress() {
    clear_all_drafts();
    if let Some(s) = storage() {
        let _ = s.remove_item("rustlings_done");
        let _ = s.remove_item("rustlings_schema");
    }
}

/// Number of passed tests rides along for the Done label on test exercises.
fn stat_label(s: Stat, test: bool, passed: Option<u32>, threads_unsupported: bool) -> String {
    match s {
        Stat::Todo => "—".into(),
        Stat::Error => {
            if test { "✗ tests failed".into() } else { "✗ does not compile".into() }
        }
        Stat::Compiles => {
            if threads_unsupported {
                "compiles ✓ — needs threads, can't run in the browser".into()
            } else {
                "compiles ✓ — remove the remaining todo!()".into()
            }
        }
        Stat::Done => match passed {
            Some(n) if test => format!("✓ {n} test{} passed", if n == 1 { "" } else { "s" }),
            _ => "✓ done".into(),
        },
    }
}

/// egui editor sharing its buffer with the Leptos shell; calls `on_edit` when the user types.
struct EditorApp {
    code: Rc<RefCell<String>>,
    on_edit: Rc<dyn Fn()>,
    diags: SharedDiags,
}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Same treatment as the playground editor: Gruvbox, panel filled with
        // the editor background so the field reaches the bottom, rows=1 so
        // line numbers follow the content, full-pane hover/focus ring.
        let theme = egui_code_editor::ColorTheme::GRUVBOX;
        let frame = egui::Frame::central_panel(ui.style()).fill(theme.bg());
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            let pane = ui.max_rect();
            let v = &mut ui.style_mut().visuals;
            v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
            v.widgets.active.bg_stroke = egui::Stroke::NONE;
            v.selection.stroke = egui::Stroke::NONE;
            let mut text = self.code.borrow_mut();
            let out = egui_code_editor::CodeEditor::default()
                .id_source("trainer-editor")
                .with_rows(1)
                .with_fontsize(15.0)
                .with_theme(theme)
                .with_numlines(true)
                .show(ui, &mut *text, &egui_code_editor::Syntax::rust());
            let changed = out.response.changed();
            drop(text);
            if changed {
                (self.on_edit)();
            }
            diag::paint_diags(ui, pane, &out, &self.diags.borrow());
            let rest = ui.available_size();
            if rest.y > 0.0 {
                let (_, resp) = ui.allocate_exact_size(rest, egui::Sense::click());
                if resp.clicked() {
                    ui.memory_mut(|m| m.request_focus(out.response.id));
                }
            }
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

#[component]
pub fn RustlingsView(active: Signal<bool>) -> impl IntoView {
    // Shared, non-reactive state.
    let code = Rc::new(RefCell::new(String::new()));
    let egui_ctx: Rc<RefCell<Option<egui::Context>>> = Rc::new(RefCell::new(None));
    let exercises: Rc<RefCell<Vec<Exercise>>> = Rc::new(RefCell::new(Vec::new()));
    let generation = Rc::new(Cell::new(0u64));
    // In-editor diagnostics (squiggles/tooltips), fed by the checkRust results.
    let diags: SharedDiags = Rc::new(RefCell::new(Vec::new()));

    // Reactive state.
    let (list_meta, set_list_meta) = signal(Vec::<(usize, String, bool)>::new()); // (idx, name, test)
    let (current, set_current) = signal(0usize);
    let (diag_text, set_diag_text) = signal(String::new());
    let (status, set_status) = signal(String::from("Loading exercises + compiler…"));
    let (stat, set_stat) = signal(Stat::Todo);
    let (cur_test, set_cur_test) = signal(false);
    let (cur_passed, set_cur_passed) = signal::<Option<u32>>(None);
    let (cur_threads_unsup, set_cur_threads_unsup) = signal(false);
    let (cur_hint, set_cur_hint) = signal(String::new());
    let (show_hint, set_show_hint) = signal(false);
    let done = RwSignal::new(load_done());
    // Sidebar clicks can only capture Send signal-setters inside the reactive list; route the chosen
    // index through this signal and run the (!Send) `select` from an Effect.
    let (select_req, set_select_req) = signal::<Option<usize>>(None);

    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();

    // The core type-check action (reads the live buffer + current exercise).
    let run_check: Rc<dyn Fn()> = {
        let code = code.clone();
        let exercises = exercises.clone();
        let diags = diags.clone();
        let egui_ctx = egui_ctx.clone();
        Rc::new(move || {
            let idx = current.get_untracked();
            let ex = {
                let exs = exercises.borrow();
                match exs.get(idx) {
                    Some(e) => e.clone(),
                    None => return,
                }
            };
            let src = code.borrow().clone();
            let has_todo = src.contains("todo!(");
            set_status.set("Type-checking…".into());
            let diags = diags.clone();
            let egui_ctx = egui_ctx.clone();
            spawn_local(async move {
                let status_cb =
                    Closure::wrap(Box::new(move |s: String| set_status.set(s)) as Box<dyn Fn(String)>);
                // Stage 1: the fast --emit metadata type/borrow check (every keystroke).
                // The const-eval verifier is retired — real tests run below.
                let res = checkRust(src.clone(), ex.test, String::new(), status_cb.as_ref()).await;
                // Discard a stale result if the user switched exercises while this check ran,
                // or if a newer submission cancelled this one mid-compile.
                if current.get_untracked() != idx {
                    return;
                }
                if let Ok(v) = &res {
                    if get_bool(v, "cancelled") == Some(true) {
                        return;
                    }
                    // Editor markers, additive to the diagnostics panel below.
                    let ds = diag::parse_diags(v);
                    diag::publish_counts(&ds);
                    *diags.borrow_mut() = ds;
                    if let Some(ctx) = egui_ctx.borrow().as_ref() {
                        ctx.request_repaint();
                    }
                }
                let (ok, text, check_ms) = match res {
                    Ok(v) => (
                        get_bool(&v, "ok").unwrap_or(false),
                        get_str(&v, "text").unwrap_or_default(),
                        get_num(&v, "compileMs").unwrap_or(0.0),
                    ),
                    Err(e) => {
                        set_diag_text.set(format!("checker error: {e:?}"));
                        set_stat.set(Stat::Error);
                        set_status.set(String::new());
                        return;
                    }
                };
                set_cur_passed.set(None);
                set_cur_threads_unsup.set(false);

                if !ok || has_todo || !ex.test {
                    // Compile-only verdict (same as before Phase B's test runner).
                    let s = if !ok {
                        Stat::Error
                    } else if has_todo {
                        Stat::Compiles
                    } else {
                        Stat::Done
                    };
                    set_diag_text.set(if text.is_empty() && ok { "No errors. 🎉".into() } else { text });
                    set_stat.set(s);
                    // A compile error on a TEST exercise is still "does not compile".
                    let label = stat_label(s, ex.test && s != Stat::Error, None, false);
                    set_status.set(format!("{label}  ·  checked in {} ms", check_ms.round() as i64));
                    if s == Stat::Done {
                        done.update(|d| { d.insert(ex.name.clone()); });
                        save_done(&done.get_untracked());
                    }
                    return;
                }

                // Stage 2: it type-checks and has no todo!() — RUN the tests for real.
                let status_cb2 =
                    Closure::wrap(Box::new(move |s: String| set_status.set(s)) as Box<dyn Fn(String)>);
                let tres = runTests(src, status_cb2.as_ref()).await;
                if current.get_untracked() != idx {
                    return;
                }
                match tres {
                    Ok(v) => {
                        if get_bool(&v, "cancelled") == Some(true) {
                            return;
                        }
                        let tok = get_bool(&v, "ok").unwrap_or(false);
                        let output = get_str(&v, "output").unwrap_or_default();
                        let unsupported = get_str(&v, "unsupported");
                        let passed = get_num(&v, "passed").map(|n| n as u32);
                        let cms = get_num(&v, "compileMs").unwrap_or(0.0);
                        let ems = get_num(&v, "execMs").unwrap_or(0.0);
                        if unsupported.as_deref() == Some("threads") {
                            set_cur_threads_unsup.set(true);
                            set_diag_text.set(output);
                            set_stat.set(Stat::Compiles);
                            set_status.set(format!(
                                "{}  ·  built in {} ms",
                                stat_label(Stat::Compiles, true, None, true),
                                cms.round() as i64
                            ));
                            return;
                        }
                        if tok {
                            set_cur_passed.set(passed);
                            set_diag_text.set(output);
                            set_stat.set(Stat::Done);
                            set_status.set(format!(
                                "{}  ·  built in {} ms · ran in {} ms",
                                stat_label(Stat::Done, true, passed, false),
                                cms.round() as i64,
                                ems.round() as i64
                            ));
                            done.update(|d| { d.insert(ex.name.clone()); });
                            save_done(&done.get_untracked());
                        } else {
                            set_diag_text.set(output);
                            set_stat.set(Stat::Error);
                            set_status.set(format!(
                                "{}  ·  built in {} ms · ran in {} ms",
                                stat_label(Stat::Error, true, None, false),
                                cms.round() as i64,
                                ems.round() as i64
                            ));
                        }
                    }
                    Err(e) => {
                        set_diag_text.set(format!("test runner error: {e:?}"));
                        set_stat.set(Stat::Error);
                        set_status.set(String::new());
                    }
                }
            });
        })
    };

    // Debounced editor-change handler: 200 ms after the last keystroke, persist the draft and run
    // the latest check. Saving happens here (not in run_check) so the immediate check fired by
    // select()/Reset()/Solution() doesn't re-persist a buffer the user didn't type.
    let on_edit: Rc<dyn Fn()> = {
        let generation = generation.clone();
        let run_check = run_check.clone();
        let code = code.clone();
        let exercises = exercises.clone();
        Rc::new(move || {
            let g = generation.get().wrapping_add(1);
            generation.set(g);
            let generation = generation.clone();
            let run_check = run_check.clone();
            let code = code.clone();
            let exercises = exercises.clone();
            set_timeout(
                move || {
                    if generation.get() == g {
                        let idx = current.get_untracked();
                        if let Some(name) = exercises.borrow().get(idx).map(|e| e.name.clone()) {
                            save_draft(&name, &code.borrow());
                        }
                        run_check();
                    }
                },
                Duration::from_millis(200),
            );
        })
    };

    // Select an exercise: load its source into the editor, repaint egui, run an immediate check.
    let select: Rc<dyn Fn(usize)> = {
        let code = code.clone();
        let exercises = exercises.clone();
        let egui_ctx = egui_ctx.clone();
        let run_check = run_check.clone();
        let diags = diags.clone();
        Rc::new(move |idx: usize| {
            let ex = {
                let exs = exercises.borrow();
                match exs.get(idx) {
                    Some(e) => e.clone(),
                    None => return,
                }
            };
            // Prefer a saved in-progress draft over the pristine exercise body.
            *code.borrow_mut() = load_draft(&ex.name).unwrap_or_else(|| ex.exercise.clone());
            set_current.set(idx);
            set_cur_test.set(ex.test);
            set_cur_passed.set(None);
            set_cur_threads_unsup.set(false);
            set_cur_hint.set(ex.hint.clone());
            set_show_hint.set(false);
            set_stat.set(Stat::Todo);
            set_diag_text.set(String::new());
            diags.borrow_mut().clear();
            diag::publish_counts(&diags.borrow());
            if let Some(ctx) = egui_ctx.borrow().as_ref() {
                ctx.request_repaint();
            }
            run_check();
        })
    };

    // Boot the egui editor onto the canvas once it mounts.
    {
        let code = code.clone();
        let egui_ctx = egui_ctx.clone();
        let on_edit = on_edit.clone();
        let diags = diags.clone();
        Effect::new(move |started: Option<bool>| {
            if started == Some(true) {
                return true;
            }
            // Boot egui only once this view is (or has been) shown — starting
            // eframe on a display:none canvas gives it a 0x0 surface.
            if !active.get() {
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
                            cc.egui_ctx.set_visuals(egui::Visuals::dark());
                            *egui_ctx.borrow_mut() = Some(cc.egui_ctx.clone());
                            Ok(Box::new(EditorApp { code, on_edit, diags }))
                        }),
                    )
                    .await;
            });
            true
        });
    }

    // Load exercises once, then select the first.
    {
        let exercises = exercises.clone();
        let select = select.clone();
        spawn_local(async move {
            match loadExercisesJson().await {
                Ok(v) => {
                    let txt = v.as_string().unwrap_or_default();
                    match serde_json::from_str::<Doc>(&txt) {
                        Ok(doc) => {
                            // Invalidate stale drafts if the bundled exercise set changed.
                            ensure_schema(&doc.meta.source);
                            let meta: Vec<(usize, String, bool)> = doc
                                .exercises
                                .iter()
                                .enumerate()
                                .map(|(i, e)| (i, format!("{}/{}", e.dir, e.name), e.test))
                                .collect();
                            *exercises.borrow_mut() = doc.exercises;
                            set_list_meta.set(meta);
                            set_status.set("Ready — edit and it re-checks 200 ms after you stop.".into());
                            select(0);
                        }
                        Err(e) => set_status.set(format!("bad exercises.json: {e}")),
                    }
                }
                Err(e) => set_status.set(format!("failed to load exercises: {e:?}")),
            }
        });
    }

    let on_reset = {
        let select = select.clone();
        let exercises = exercises.clone();
        move |_| {
            let idx = current.get_untracked();
            // Drop the saved draft so select() reloads the pristine exercise body.
            if let Some(name) = exercises.borrow().get(idx).map(|e| e.name.clone()) {
                clear_draft(&name);
            }
            select(idx);
        }
    };
    let on_check = {
        let run_check = run_check.clone();
        move |_| run_check()
    };
    let on_solution = {
        let code = code.clone();
        let exercises = exercises.clone();
        let egui_ctx = egui_ctx.clone();
        let run_check = run_check.clone();
        move |_| {
            let (name, sol) = {
                let exs = exercises.borrow();
                match exs.get(current.get_untracked()) {
                    Some(e) => (e.name.clone(), e.solution.clone()),
                    None => return,
                }
            };
            if let Some(sol) = sol {
                *code.borrow_mut() = sol.clone();
                // Programmatic change won't fire on_edit, so persist the revealed solution.
                save_draft(&name, &sol);
                if let Some(ctx) = egui_ctx.borrow().as_ref() {
                    ctx.request_repaint();
                }
                run_check();
            }
        }
    };
    let on_clear_all = {
        let select = select.clone();
        move |_| {
            clear_all_progress();
            done.set(HashSet::new());
            select(0);
        }
    };

    // Run `select` whenever the sidebar sets a new index.
    {
        let select = select.clone();
        Effect::new(move |_| {
            if let Some(idx) = select_req.get() {
                select(idx);
            }
        });
    }

    let stat_class = move || match stat.get() {
        Stat::Done => "ok",
        Stat::Compiles => "warn",
        Stat::Error => "err",
        Stat::Todo => "",
    };

    view! {
        <div class="tr">
            <aside class="tr-side">
                <div class="tr-brand">
                    "Rustlings" <span class="tr-sub">"client-side compile trainer"</span>
                </div>
                <div class="tr-progress">
                    {move || format!("{} / {} done", done.with(|d| d.len()), list_meta.get().len())}
                </div>
                <div class="tr-list">
                    {move || {
                        list_meta.get().into_iter().map(move |(idx, label, test)| {
                            let label_done = label.clone();
                            view! {
                                <button
                                    class="tr-item"
                                    class:cur=move || current.get() == idx
                                    // label is "dir/name"; the `done` set stores the bare name.
                                    class:done=move || done.with(|d| {
                                        label_done.rsplit('/').next().map(|n| d.contains(n)).unwrap_or(false)
                                    })
                                    on:click=move |_| set_select_req.set(Some(idx))
                                >
                                    <span class="tr-item-name">{label}</span>
                                    {test.then(|| view! { <span class="tr-badge">"test"</span> })}
                                </button>
                            }
                        }).collect::<Vec<_>>()
                    }}
                </div>
                <button class="tr-clear" on:click=on_clear_all>"Clear all saved progress"</button>
                <a class="tr-attr" href="./rustlings/LICENSE-rustlings" target="_blank">
                    "exercises: rust-lang/rustlings v6.4.0 (MIT)"
                </a>
            </aside>

            <main class="tr-main">
                <div class="tr-toolbar">
                    <span class=move || format!("tr-stat {}", stat_class())>
                        {move || stat_label(stat.get(), cur_test.get(), cur_passed.get(), cur_threads_unsup.get())}
                    </span>
                    <div class="tr-spacer"></div>
                    <button class="tr-btn" on:click=on_check>"Check"</button>
                    <button class="tr-btn" on:click=move |_| set_show_hint.update(|h| *h = !*h)>"Hint"</button>
                    <button class="tr-btn" on:click=on_solution>"Solution"</button>
                    <button class="tr-btn" on:click=on_reset>"Reset"</button>
                </div>

                {move || show_hint.get().then(|| view! {
                    <div class="tr-hint">{move || cur_hint.get()}</div>
                })}

                <div class="tr-editorwrap">
                    <canvas class="tr-editor" tabindex="0" node_ref=canvas_ref></canvas>
                </div>

                <div class="tr-diagwrap">
                    <pre class=move || format!("tr-diag {}", stat_class())>{move || diag_text.get()}</pre>
                </div>
                <div class="tr-statusbar">{move || status.get()}</div>
            </main>
        </div>
    }
}


