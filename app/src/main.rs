//! Rust-in-WASM Playground — a Leptos rewrite of the Rust Playground frontend.
//! The DOM shell (toolbar/output/CSS) mirrors play.rust-lang.org; the *editor* is a pure-Rust
//! egui canvas (egui_code_editor) embedded in the Leptos crate — no JS editor dependency.
//! "Run" hands the source to window.runRust (public/runner.js), which compiles it with the
//! cranelift `rustc.wasm`, links it with our linker (riwl, inside rustc.wasm), and runs the
//! result under a WASI shim — all client-side, on a background worker. Because runs execute
//! off-thread they are cancellable: a newer submission terminates the in-flight one (the
//! superseded promise resolves `{ cancelled: true }`), which is what makes live auto-run
//! (compile on every keystroke) affordable.
//!
//! The editor buffer is autosaved to localStorage (`playground_src`) so a reload restores your
//! work; picking an example replaces the buffer (and the save) with that snippet.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

mod ansi;
mod diag;
mod editor;
mod rustlings;
use diag::SharedDiags;
use editor::{EditorHandle, EditorPrefs};
use rustlings::RustlingsView;

/// Which half of a view is on screen. Only meaningful in the narrow layout,
/// where the two panes are tabbed instead of side by side.
#[derive(Clone, Copy, PartialEq)]
pub enum Pane {
    Code,
    Out,
}

#[wasm_bindgen]
extern "C" {
    // Defined by public/runner.js. Returns { ok, stdout, stderr, exit,
    // runtimeError, compileFailed?, diagnostics, compileMs, execMs, ... }.
    // `status` is a JS callback invoked with progress strings (download phase mostly).
    #[wasm_bindgen(js_namespace = window, catch)]
    async fn runRust(source: String, status: &JsValue) -> Result<JsValue, JsValue>;
    // Type-check only (rustc --emit metadata) — feeds the in-editor diagnostics
    // while auto-run is off. Same signature as the Rustlings view's import.
    #[wasm_bindgen(js_namespace = window, catch)]
    async fn checkRust(source: String, isTest: bool, constCheck: String, status: &JsValue)
        -> Result<JsValue, JsValue>;
    // Saves `text` as a file download named `filename` (blob URL + anchor click).
    #[wasm_bindgen(js_namespace = window)]
    fn downloadText(filename: String, text: String);
    // Toolchain preload (idempotent; same promise runner.js awaits internally).
    #[wasm_bindgen(js_namespace = window, catch)]
    async fn preloadRust(on_progress: &JsValue) -> Result<JsValue, JsValue>;
}

// Rendering the full output of a chatty program (hundreds of thousands of
// lines) into the <pre> stalls layout for seconds; past this many lines the
// pane shows a prefix and offers the rest as a download.
const MAX_OUTPUT_LINES: usize = 1000;

/// One severity-colored run of text in the output pane: `Plain` for program
/// stdout, `Warn`/`Err` for rustc diagnostics and failure text.
#[derive(Clone, Copy, PartialEq)]
enum SegKind {
    Plain,
    Warn,
    Err,
}
type Segs = Vec<(SegKind, String)>;

/// Segments are stored separator-inclusive: every non-final segment ends in
/// '\n' (matching the old `join("\n")`), so render/download/truncation are
/// plain concatenation.
fn seal_segs(mut segs: Segs) -> Segs {
    let n = segs.len();
    for (i, (_, s)) in segs.iter_mut().enumerate() {
        if i + 1 < n && !s.ends_with('\n') {
            s.push('\n');
        }
    }
    segs
}

/// One run's output, split rust-playground-style into collapsible sections.
#[derive(Clone, PartialEq)]
struct RunOut {
    /// false only for the boot placeholder (shown headerless).
    ran: bool,
    /// Failure summary: "Exited with status N" / "Runtime error: …" /
    /// "Compilation failed".
    errors: Option<String>,
    /// Compiler diagnostics (ANSI-colored segments) + program stderr.
    stderr: Segs,
    stdout: String,
}

/// First `max` lines of the segment list (the cut can land mid-segment) +
/// hidden line count — the old whole-pane truncation, per section now.
fn truncate_segs(segs: &Segs, max: usize) -> (Segs, usize) {
    let mut budget = max;
    let mut vis: Segs = Vec::new();
    let mut hidden = 0usize;
    for (k, s) in segs {
        if budget == 0 {
            hidden += s.lines().count();
            continue;
        }
        let nls = s.match_indices('\n').count();
        if nls < budget {
            budget -= nls;
            vis.push((*k, s.clone()));
        } else {
            match s.match_indices('\n').nth(budget - 1) {
                Some((idx, _)) if idx + 1 < s.len() => {
                    vis.push((*k, s[..idx].to_string()));
                    hidden += s[idx + 1..].lines().count();
                }
                // The budget-th newline is the segment's final byte: keep it
                // whole; later segments are fully hidden.
                _ => vis.push((*k, s.clone())),
            }
            budget = 0;
        }
    }
    (vis, hidden)
}

fn truncate_str(s: &str, max: usize) -> (String, usize) {
    match s.match_indices('\n').nth(max - 1) {
        Some((idx, _)) if idx + 1 < s.len() => {
            (s[..idx].to_string(), s[idx + 1..].lines().count())
        }
        _ => (s.to_string(), 0),
    }
}

/// One output segment: severity class, with rustc's ANSI colors converted to
/// HTML when present (the `ansi` class neutralizes the severity tint so only
/// the tokens rustc colored are colored).
fn seg_view(k: SegKind, s: String) -> AnyView {
    let class = match k {
        SegKind::Plain => "",
        SegKind::Warn => "warn",
        SegKind::Err => "err",
    };
    if ansi::has_ansi(&s) {
        view! {
            <span class=format!("{class} ansi") inner_html=ansi::ansi_to_html(&s)></span>
        }
        .into_any()
    } else {
        view! { <span class=class>{s}</span> }.into_any()
    }
}

// Long-form page text is authored in content/*.md and rendered to HTML by
// build.rs (pulldown-cmark on the host — nothing added to the wasm binary).
const ABOUT_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/about.html"));
const HELP_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/help.html"));

// Phase B: the editor holds PLAIN RUST — full std (Vec/String/HashMap/format!),
// real println! formatting, compiled and linked entirely in the browser.
const DEFAULT_SRC: &str = r#"fn main() {
    println!("Hello from Rust, compiled by cranelift in your browser!");
    let total: u64 = (1..=100u64).sum();
    println!("sum 1..=100 = {}", total);

    let mut langs = vec!["Rust", "in", "your", "browser"];
    langs.push("with std!");
    println!("{}", langs.join(" "));
}
"#;

const EX_FIZZBUZZ: &str = r#"fn main() {
    let mut i: u32 = 1;
    while i <= 20 {
        if i % 15 == 0 {
            println!("FizzBuzz");
        } else if i % 3 == 0 {
            println!("Fizz");
        } else if i % 5 == 0 {
            println!("Buzz");
        } else {
            println!("{}", i);
        }
        i += 1;
    }
}
"#;

const EX_FIB: &str = r#"fn main() {
    let mut a: u64 = 0;
    let mut b: u64 = 1;
    let mut n: u32 = 0;
    while n < 20 {
        println!("fib = {}", a);
        let c = a + b;
        a = b;
        b = c;
        n += 1;
    }
}
"#;

// --- localStorage persistence: the editor buffer survives a reload. ---
const KEY_SRC: &str = "playground_src";

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}
fn load_src() -> String {
    storage()
        .and_then(|s| s.get_item(KEY_SRC).ok().flatten())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SRC.to_string())
}
fn save_src(src: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(KEY_SRC, src);
    }
}
// Auto-run preference (off by default) persists like the buffer does.
const KEY_AUTORUN: &str = "playground_autorun";
fn load_autorun() -> bool {
    storage()
        .and_then(|s| s.get_item(KEY_AUTORUN).ok().flatten())
        .as_deref()
        == Some("1")
}
fn save_autorun(on: bool) {
    if let Some(s) = storage() {
        let _ = s.set_item(KEY_AUTORUN, if on { "1" } else { "0" });
    }
}

fn get_str(v: &JsValue, k: &str) -> Option<String> {
    js_sys::Reflect::get(v, &JsValue::from_str(k)).ok().and_then(|x| x.as_string())
}
fn get_num(v: &JsValue, k: &str) -> Option<f64> {
    js_sys::Reflect::get(v, &JsValue::from_str(k)).ok().and_then(|x| x.as_f64())
}
fn get_bool(v: &JsValue, k: &str) -> Option<bool> {
    js_sys::Reflect::get(v, &JsValue::from_str(k)).ok().and_then(|x| x.as_bool())
}

/// One-line summary of the current diagnostics for the touch-mode strip under
/// the textarea: the counts, plus the first error's own headline. `.0` is
/// "there is at least one error" (the strip tints red).
fn diag_headline(ds: &[diag::Diag]) -> Option<(bool, String)> {
    let first = ds.iter().find(|d| d.is_error).or_else(|| ds.first())?;
    let errors = ds.iter().filter(|d| d.is_error).count();
    let warnings = ds.len() - errors;
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut counts = Vec::new();
    if errors > 0 {
        counts.push(format!("{errors} error{}", plural(errors)));
    }
    if warnings > 0 {
        counts.push(format!("{warnings} warning{}", plural(warnings)));
    }
    let rendered = ansi::strip_ansi(&first.rendered);
    let head = rendered.lines().next().unwrap_or_default().trim();
    Some((errors > 0, format!("{}  ·  line {}: {head}", counts.join(", "), first.line)))
}

#[component]
fn PlaygroundView(active: Signal<bool>, touch: Signal<bool>, prefs: EditorPrefs) -> impl IntoView {
    // Restore the last-edited buffer (or the default snippet on first visit).
    let ed = EditorHandle::new(load_src());
    let code = ed.code.clone();
    let generation = Rc::new(Cell::new(0u64));

    // Replaced by the first auto-run's result; until the toolchain is fetched
    // (progress overlay bottom-right) this is what the output pane shows.
    let (output, set_output) = signal(RunOut {
        ran: false,
        errors: None,
        stderr: Vec::new(),
        stdout: "Waiting for compiler download...".into(),
    });
    let (status, set_status) = signal(String::new());
    // What the pane actually renders: each section truncated to its own
    // MAX_OUTPUT_LINES budget (byte scans, no per-line allocation), plus the
    // combined hidden line count for the "... N more lines" row.
    let shown = Memo::new(move |_| {
        output.with(|o| {
            let (stderr, hid_err) = truncate_segs(&o.stderr, MAX_OUTPUT_LINES);
            let (stdout, hid_out) = truncate_str(&o.stdout, MAX_OUTPUT_LINES);
            (RunOut { ran: o.ran, errors: o.errors.clone(), stderr, stdout }, hid_err + hid_out)
        })
    });
    // Per-section collapse toggles — sticky across runs, so collapsing the
    // compiler noise stays collapsed during auto-run.
    let (sec_errors, set_sec_errors) = signal(true);
    let (sec_stderr, set_sec_stderr) = signal(true);
    let (sec_stdout, set_sec_stdout) = signal(true);
    let (help, set_help) = signal(false);

    // Narrow layout only: which pane the Code/Output tabs are showing, and
    // whether the Output tab should wear an "there are errors over here" dot.
    let (pane, set_pane) = signal(Pane::Code);
    let (out_badge, set_out_badge) = signal(false);
    // Touch only: the latest check's headline. Without the canvas there are no
    // squiggles and no hover tooltips, and with auto-run off (the default) the
    // debounced check's text never reaches the output pane — so a type error
    // would otherwise produce no visible feedback at all until you press Run.
    let (editnote, set_editnote) = signal::<Option<(bool, String)>>(None);
    // (errors, warnings) from the same check — the focus-mode bar needs a form
    // short enough to sit between two buttons. Not derived from `editnote`:
    // that's a formatted string, and `.pg-editnote` renders nothing at all when
    // the code is clean, so "✓ no errors" is genuinely new information.
    let (editcounts, set_editcounts) = signal::<Option<(usize, usize)>>(None);

    let (autorun, set_autorun) = signal(load_autorun());

    // In-editor diagnostics (squiggles/tooltips) + their two producers' state:
    // a run-in-flight counter (debounced checks must not cancel a manual Run
    // in the single-slot newest-wins pool) and the check debounce generation.
    let diags: SharedDiags = Rc::new(RefCell::new(Vec::new()));
    let runs_in_flight = Rc::new(Cell::new(0u32));
    let check_gen = Rc::new(Cell::new(0u64));

    let apply_diags = {
        let diags = diags.clone();
        let egui_ctx = ed.egui_ctx.clone();
        Rc::new(move |v: &JsValue| {
            let ds = diag::parse_diags(v);
            diag::publish_counts(&ds);
            set_editnote.set(diag_headline(&ds));
            let errors = ds.iter().filter(|d| d.is_error).count();
            set_editcounts.set(Some((errors, ds.len() - errors)));
            if pane.get_untracked() == Pane::Code && ds.iter().any(|d| d.is_error) {
                set_out_badge.set(true);
            }
            *diags.borrow_mut() = ds;
            if let Some(ctx) = egui_ctx.borrow().as_ref() {
                ctx.request_repaint();
            }
        })
    };

    // Submitting while a run is in flight is fine: runner.js terminates the
    // superseded worker mid-compile and that call resolves { cancelled: true }.
    // The previous output stays on screen until a surviving run replaces it
    // (no flashing during live auto-run).
    let run_now: Rc<dyn Fn()> = {
        let code = code.clone();
        let runs_in_flight = runs_in_flight.clone();
        let apply_diags = apply_diags.clone();
        Rc::new(move || {
            set_status.set("Working...".into());
            let src = code.borrow().clone();
            let runs_in_flight = runs_in_flight.clone();
            let apply_diags = apply_diags.clone();
            runs_in_flight.set(runs_in_flight.get() + 1);
            spawn_local(async move {
                let status_cb = Closure::wrap(Box::new(move |s: String| {
                    set_status.set(s);
                }) as Box<dyn Fn(String)>);
                let result = runRust(src, status_cb.as_ref()).await;
                // Every submission resolves exactly once (result/cancelled/err).
                runs_in_flight.set(runs_in_flight.get().saturating_sub(1));
                match result {
                    Ok(v) => {
                        if get_bool(&v, "cancelled") == Some(true) {
                            // A newer keystroke superseded this run; its own
                            // submission owns the status/running signals now.
                            return;
                        }
                        // Runs carry rustc's JSON diagnostics too — the editor
                        // markers stay fresh in auto-run mode without separate
                        // checks (which would fight the runs in the pool).
                        apply_diags(&v);
                        let compile_failed = get_bool(&v, "compileFailed") == Some(true);
                        // Standard Error: compiler diagnostics (rustc's own
                        // order — warnings stay visible even when the run
                        // succeeds), then the program's stderr / ICE residue.
                        let mut stderr: Segs = diag::parse_output_diags(&v)
                            .into_iter()
                            .map(|(is_err, rendered)| {
                                (if is_err { SegKind::Err } else { SegKind::Warn }, rendered)
                            })
                            .collect();
                        match get_str(&v, "stderr") {
                            Some(se) if !se.is_empty() => stderr.push((
                                if compile_failed { SegKind::Err } else { SegKind::Plain },
                                se,
                            )),
                            _ => {}
                        }
                        // Engine errors (worker catch-all) arrive in the old
                        // `output` field with no stdout/stderr/diagnostics.
                        match get_str(&v, "output") {
                            Some(o) if !o.is_empty() => stderr.push((SegKind::Err, o)),
                            _ => {}
                        }
                        // Errors section: the failure summary, playground-style.
                        let errors = if compile_failed {
                            Some("Compilation failed".to_string())
                        } else if let Some(re) = get_str(&v, "runtimeError") {
                            Some(format!("Runtime error: {re}"))
                        } else {
                            match get_num(&v, "exit") {
                                Some(code) if code != 0.0 => {
                                    Some(format!("Exited with status {}", code as i64))
                                }
                                _ => None,
                            }
                        };
                        let c = get_num(&v, "compileMs").unwrap_or(0.0);
                        let l = get_num(&v, "linkMs");
                        let e = get_num(&v, "execMs").unwrap_or(0.0);
                        // Narrow layout: if the user is looking at the code,
                        // dot the Output tab rather than yanking them over.
                        if pane.get_untracked() == Pane::Code && errors.is_some() {
                            set_out_badge.set(true);
                        }
                        set_output.set(RunOut {
                            ran: true,
                            errors,
                            stderr: seal_segs(stderr),
                            stdout: get_str(&v, "stdout").unwrap_or_default(),
                        });
                        // std mode reports the in-rustc riwl link time separately
                        // (per-stage breakdown is logged to the console by runner.js).
                        set_status.set(match l {
                            Some(l) => format!(
                                "compiled in {} ms, linked in {} ms, executed in {} ms",
                                c.round() as i64,
                                l.round() as i64,
                                e.round() as i64,
                            ),
                            None => format!(
                                "compiled in {} ms, executed in {} ms",
                                c.round() as i64,
                                e.round() as i64,
                            ),
                        });
                    }
                    Err(e) => {
                        set_output.set(RunOut {
                            ran: true,
                            errors: Some(format!("error: {e:?}")),
                            stderr: Vec::new(),
                            stdout: String::new(),
                        });
                        set_status.set(String::new());
                    }
                }
            });
        })
    };
    // Pressing Run reveals the output pane (narrow layout only — the class is
    // inert on desktop). This lives here and NOT in `run_now`, which auto-run
    // calls on every keystroke: doing it there would yank the pane away as you
    // type.
    let go_run: Rc<dyn Fn()> = {
        let run_now = run_now.clone();
        Rc::new(move || {
            set_pane.set(Pane::Out);
            set_out_badge.set(false);
            run_now()
        })
    };
    let on_run = {
        let go_run = go_run.clone();
        move |_| go_run()
    };

    // Focus mode's bar: Run there means "same as Done, but show me the output",
    // which is exactly what the toolbar's Run already does once the editor blurs.
    let chrome = editor::EditorChrome {
        verdict: Signal::derive(move || match editcounts.get() {
            None => String::new(),
            Some((0, 0)) => "✓ no errors".into(),
            Some((0, w)) => format!("{w} warning{}", if w == 1 { "" } else { "s" }),
            Some((e, _)) => format!("{e} error{}", if e == 1 { "" } else { "s" }),
        }),
        verdict_class: Signal::derive(move || match editcounts.get() {
            None => "",
            Some((0, 0)) => "ok",
            Some((0, _)) => "warn",
            Some(_) => "err",
        }),
        action_label: "Run",
        on_action: go_run,
    };

    // Per keystroke: with auto-run on, submit a compile IMMEDIATELY — the
    // in-flight one is cancelled (worker terminated), so the toolchain is
    // always working on the newest source and never queues up behind stale
    // runs (the run result carries the diagnostics). With auto-run off, a
    // debounced type-check (300 ms) feeds the in-editor markers instead —
    // skipped while a manual Run is in flight so it can't cancel it in the
    // newest-wins pool. The localStorage save keeps its own 400 ms debounce.
    let on_edit: Rc<dyn Fn()> = {
        let code = code.clone();
        let generation = generation.clone();
        let run_now = run_now.clone();
        let check_gen = check_gen.clone();
        let runs_in_flight = runs_in_flight.clone();
        let apply_diags = apply_diags.clone();
        Rc::new(move || {
            if autorun.get_untracked() {
                run_now();
            } else {
                let g = check_gen.get().wrapping_add(1);
                check_gen.set(g);
                let code = code.clone();
                let check_gen = check_gen.clone();
                let runs_in_flight = runs_in_flight.clone();
                let apply_diags = apply_diags.clone();
                set_timeout(
                    move || {
                        if check_gen.get() != g || runs_in_flight.get() > 0 {
                            return;
                        }
                        let src = code.borrow().clone();
                        let apply_diags = apply_diags.clone();
                        spawn_local(async move {
                            let res =
                                checkRust(src, false, String::new(), &JsValue::NULL).await;
                            if let Ok(v) = res {
                                if get_bool(&v, "cancelled") != Some(true) {
                                    apply_diags(&v);
                                }
                            }
                        });
                    },
                    Duration::from_millis(300),
                );
            }
            let g = generation.get().wrapping_add(1);
            generation.set(g);
            let code = code.clone();
            let generation = generation.clone();
            set_timeout(
                move || {
                    if generation.get() == g {
                        save_src(&code.borrow());
                    }
                },
                Duration::from_millis(400),
            );
        })
    };

    // First compile happens on load with whatever is in the buffer — submitted
    // only once the toolchain is ready. Submitting earlier would park it behind
    // the download, where the Rustlings view's boot-time check (submitted later)
    // supersedes it in the pool's newest-wins ordering and the run resolves
    // { cancelled }.
    //
    // This is deliberately NOT part of the editor's boot: the editor may be a
    // textarea (no eframe at all), and the toolchain still has to be fetched.
    {
        let run_now = run_now.clone();
        Effect::new(move |done: Option<bool>| {
            if done == Some(true) {
                return true;
            }
            if !active.get() {
                return false;
            }
            let run_now = run_now.clone();
            spawn_local(async move {
                let _ = preloadRust(&JsValue::NULL).await;
                run_now();
            });
            true
        });
    }

    let on_example = {
        let ed = ed.clone();
        let run_now = run_now.clone();
        move |ev: leptos::ev::Event| {
            let src = match event_target_value(&ev).as_str() {
                "fizzbuzz" => EX_FIZZBUZZ,
                "fib" => EX_FIB,
                _ => DEFAULT_SRC,
            };
            // Programmatic buffer changes don't fire the editor's on_edit, so persist explicitly.
            ed.set(src.to_string());
            save_src(src);
            run_now();
        }
    };

    view! {
        // show-code / show-out only bite under html.is-narrow, where the two
        // panes are tabbed instead of side by side.
        <div
            class="pg"
            class:show-code=move || pane.get() == Pane::Code
            class:show-out=move || pane.get() == Pane::Out
        >
            <div class="pg-toolbar">
                <div class="btnset">
                    // Static label (no layout shift during live auto-run);
                    // clicking mid-run cancels the in-flight compile and
                    // starts over with the current buffer.
                    <button class="btn btn-primary" on:click=on_run>"Run"</button>
                    <select class="btn" on:change=on_example>
                        <option value="default">"Example: Hello + sum"</option>
                        <option value="fizzbuzz">"Example: FizzBuzz"</option>
                        <option value="fib">"Example: Fibonacci"</option>
                    </select>
                    <button class="btn" on:click=move |_| set_help.update(|h| *h = !*h)>"?"</button>
                </div>
                {editor::editor_settings(prefs, "btn")}
                <label class="pg-autorun" title="Compile & run on every keystroke; a newer keystroke cancels the compile in flight.">
                    <input
                        type="checkbox"
                        prop:checked=move || autorun.get()
                        on:change=move |ev| {
                            let on = event_target_checked(&ev);
                            set_autorun.set(on);
                            save_autorun(on);
                        }
                    />
                    "auto-run"
                </label>
                <div class="pg-spacer"></div>
            </div>

            <div class="pg-tabs" role="tablist">
                <button
                    class="pg-tab"
                    class:cur=move || pane.get() == Pane::Code
                    on:click=move |_| set_pane.set(Pane::Code)
                >"Code"</button>
                <button
                    class="pg-tab"
                    class:cur=move || pane.get() == Pane::Out
                    class:badge=move || out_badge.get()
                    on:click=move |_| { set_pane.set(Pane::Out); set_out_badge.set(false); }
                >"Output"</button>
            </div>

            {move || help.get().then(|| view! {
                <div class="pg-help" inner_html=HELP_HTML></div>
            })}

            <div class="pg-body">
                // canvas + textarea are ONE grid item; CSS picks which shows.
                <div class="pg-editorpane">
                    {editor::code_editor(
                        ed.clone(),
                        on_edit.clone(),
                        diags.clone(),
                        active,
                        touch,
                        "editor",
                        "pg-editor",
                        "pg-editor-text",
                        chrome,
                    )}
                    {move || editnote.get().map(|(is_err, text)| view! {
                        <div class="pg-editnote" class:err=is_err id="editnote">{text}</div>
                    })}
                </div>
                <div class="pg-outpane">
                    <div class="pg-output" id="output">
                        {move || {
                            let (o, _) = shown.get();
                            if !o.ran {
                                // Boot placeholder: bare text, no section chrome.
                                return view! { <pre class="pg-sec-body">{o.stdout}</pre> }
                                    .into_any();
                            }
                            view! {
                                {o.errors.map(|e| view! {
                                    <div class="pg-sec">
                                        <button
                                            class="pg-sec-head"
                                            on:click=move |_| set_sec_errors.update(|v| *v = !*v)
                                        >
                                            {move || if sec_errors.get() { "▾ Errors" } else { "▸ Errors" }}
                                        </button>
                                        <pre
                                            class="pg-sec-body err"
                                            class:collapsed=move || !sec_errors.get()
                                        >{e}</pre>
                                    </div>
                                })}
                                {(!o.stderr.is_empty()).then(|| view! {
                                    <div class="pg-sec">
                                        <button
                                            class="pg-sec-head"
                                            on:click=move |_| set_sec_stderr.update(|v| *v = !*v)
                                        >
                                            {move || if sec_stderr.get() { "▾ Standard Error" } else { "▸ Standard Error" }}
                                        </button>
                                        <pre
                                            class="pg-sec-body"
                                            class:collapsed=move || !sec_stderr.get()
                                        >
                                            {o.stderr.into_iter().map(|(k, s)| seg_view(k, s)).collect_view()}
                                        </pre>
                                    </div>
                                })}
                                <div class="pg-sec">
                                    <button
                                        class="pg-sec-head"
                                        on:click=move |_| set_sec_stdout.update(|v| *v = !*v)
                                    >
                                        {move || if sec_stdout.get() { "▾ Standard Output" } else { "▸ Standard Output" }}
                                    </button>
                                    <pre
                                        class="pg-sec-body"
                                        id="stdout"
                                        class:collapsed=move || !sec_stdout.get()
                                    >{o.stdout}</pre>
                                </div>
                            }.into_any()
                        }}
                    </div>
                    {move || {
                        let hidden = shown.get().1;
                        (hidden > 0).then(|| view! {
                            <div class="pg-trunc">
                                <span>{format!("... {hidden} more lines")}</span>
                                <button on:click=move |_| {
                                    let o = output.get_untracked();
                                    let mut full = String::new();
                                    if let Some(e) = &o.errors {
                                        full.push_str("--- Errors ---\n");
                                        full.push_str(e);
                                        full.push_str("\n\n");
                                    }
                                    if !o.stderr.is_empty() {
                                        full.push_str("--- Standard Error ---\n");
                                        for (_, s) in &o.stderr {
                                            full.push_str(&ansi::strip_ansi(s));
                                        }
                                        full.push_str("\n\n");
                                    }
                                    full.push_str("--- Standard Output ---\n");
                                    full.push_str(&o.stdout);
                                    downloadText("output.txt".into(), full)
                                }>"Download full output"</button>
                            </div>
                        })
                    }}
                    <div class="pg-status" id="status">{move || status.get()}</div>
                </div>
            </div>
        </div>
    }
}

/// The ground-up story of how Weblings works — reachable from the brand button.
/// Content lives in content/about.md; build.rs renders it to HTML.
#[component]
fn AboutView() -> impl IntoView {
    view! {
        <div class="about">
            <div class="about-inner" inner_html=ABOUT_HTML></div>
        </div>
    }
}

/// Which tool is showing. One page, one preload — switching is instant and each
/// view keeps its full state (both stay mounted; the inactive one is hidden).
#[derive(Clone, Copy, PartialEq)]
enum Site {
    Playground,
    Rustlings,
    About,
}

fn site_from_hash() -> Site {
    let hash = web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .unwrap_or_default();
    match hash.as_str() {
        "#rustlings" => Site::Rustlings,
        "#about" => Site::About,
        _ => Site::Playground,
    }
}

/// Coarse pointer (or a viewport too narrow to hit a caret on): use the
/// textarea editor. Deliberately not the same query as `NARROW` — a desktop
/// window narrowed past the layout breakpoint keeps the egui editor.
const Q_TOUCH: &str = "(hover: none) and (pointer: coarse), (max-width: 720px)";
/// Phone-width layout: one pane at a time, sidebar as a drawer.
const Q_NARROW: &str = "(max-width: 720px)";

#[component]
fn App() -> impl IntoView {
    let (site, set_site) = signal(site_from_hash());

    // The two mode signals, mirrored onto <html> as `.is-touch` / `.is-narrow`.
    // Every mobile CSS rule keys off those classes rather than a raw @media, so
    // the stylesheet and the "did we boot eframe?" decision can never disagree
    // (a 700px desktop window matches max-width:720px but is not coarse-pointer,
    // and showing the textarea there while eframe ran would give two live
    // editors on one buffer).
    let touch = editor::media_signal(Q_TOUCH);
    let narrow = editor::media_signal(Q_NARROW);

    // Text size + wrap mode for the touch editor, restored from localStorage and
    // pushed back to <html> whenever either changes.
    let prefs = EditorPrefs::load();
    Effect::new(move |_| prefs.apply());
    Effect::new(move |_| {
        let touch = touch.get();
        let narrow = narrow.get();
        editor::publish_mode(if touch { "textarea" } else { "canvas" });
        let Some(html) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
        else {
            return;
        };
        let cl = html.class_list();
        let _ = cl.toggle_with_force("is-touch", touch);
        let _ = cl.toggle_with_force("is-narrow", narrow);
    });

    // Back/forward + manual hash edits switch views too.
    window_event_listener(leptos::ev::hashchange, move |_| set_site.set(site_from_hash()));
    let goto = move |s: Site| {
        if let Some(w) = web_sys::window() {
            let _ = w
                .location()
                .set_hash(match s {
                    Site::Playground => "",
                    Site::Rustlings => "rustlings",
                    Site::About => "about",
                });
        }
        set_site.set(s);
    };

    view! {
        <nav class="site-nav">
            <button
                class="site-brand"
                class:cur=move || site.get() == Site::About
                title="How does this work?"
                on:click=move |_| goto(Site::About)
            >"Weblings"</button>
            <button
                class="site-tab"
                class:cur=move || site.get() == Site::Playground
                on:click=move |_| goto(Site::Playground)
            >"Playground"</button>
            <button
                class="site-tab"
                class:cur=move || site.get() == Site::Rustlings
                on:click=move |_| goto(Site::Rustlings)
            >"Rustlings"</button>
        </nav>
        <div class="site-view" class:hidden=move || site.get() != Site::Playground>
            <PlaygroundView
                active=Signal::derive(move || site.get() == Site::Playground)
                touch=touch
                prefs=prefs
            />
        </div>
        <div class="site-view" class:hidden=move || site.get() != Site::Rustlings>
            <RustlingsView
                active=Signal::derive(move || site.get() == Site::Rustlings)
                narrow=narrow
                touch=touch
                prefs=prefs
            />
        </div>
        <div class="site-view" class:hidden=move || site.get() != Site::About>
            <AboutView />
        </div>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
