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
//! work; "Reset" clears the save and returns to the default snippet.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

mod diag;
mod rustlings;
use diag::SharedDiags;
use rustlings::RustlingsView;

#[wasm_bindgen]
extern "C" {
    // Defined by public/runner.js. Returns { ok, output, compileMs, execMs }.
    // `status` is a JS callback invoked with progress strings (download phase mostly).
    #[wasm_bindgen(js_namespace = window, catch)]
    async fn runRust(source: String, status: &JsValue) -> Result<JsValue, JsValue>;
    // Type-check only (rustc --emit metadata) — feeds the in-editor diagnostics
    // while auto-run is off. Same signature as the Rustlings view's import.
    #[wasm_bindgen(js_namespace = window, catch)]
    async fn checkRust(source: String, isTest: bool, constCheck: String, status: &JsValue)
        -> Result<JsValue, JsValue>;
}

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
fn clear_src() {
    if let Some(s) = storage() {
        let _ = s.remove_item(KEY_SRC);
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

/// The egui editor app. Shares its text buffer with the Leptos shell via `Rc<RefCell<String>>`,
/// so "Run"/"Examples"/"Reset" can read/replace it. `on_edit` fires on each keystroke so the
/// shell can debounce-save. egui only repaints on input (idle cost ~0).
struct EditorApp {
    code: Rc<RefCell<String>>,
    on_edit: Rc<dyn Fn()>,
    diags: SharedDiags,
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
                .id_source("editor")
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

fn configure_style(ctx: &egui::Context) {
    // The code pane is ALWAYS dark (Rust-Playground style); page chrome stays light.
    ctx.set_visuals(egui::Visuals::dark());
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

#[component]
fn PlaygroundView(active: Signal<bool>) -> impl IntoView {
    // Restore the last-edited buffer (or the default snippet on first visit).
    let code = Rc::new(RefCell::new(load_src()));
    let egui_ctx: Rc<RefCell<Option<egui::Context>>> = Rc::new(RefCell::new(None));
    let generation = Rc::new(Cell::new(0u64));

    let (output, set_output) = signal(String::from(
        "Press Run to compile & execute. The first run downloads rustc.wasm (~92 MB, cached after).",
    ));
    let (status, set_status) = signal(String::new());
    let (is_err, set_is_err) = signal(false);
    let (help, set_help) = signal(false);

    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();

    let (autorun, set_autorun) = signal(load_autorun());

    // In-editor diagnostics (squiggles/tooltips) + their two producers' state:
    // a run-in-flight counter (debounced checks must not cancel a manual Run
    // in the single-slot newest-wins pool) and the check debounce generation.
    let diags: SharedDiags = Rc::new(RefCell::new(Vec::new()));
    let runs_in_flight = Rc::new(Cell::new(0u32));
    let check_gen = Rc::new(Cell::new(0u64));

    let apply_diags = {
        let diags = diags.clone();
        let egui_ctx = egui_ctx.clone();
        Rc::new(move |v: &JsValue| {
            let ds = diag::parse_diags(v);
            diag::publish_counts(&ds);
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
            let t_click = js_sys::Date::now();
            set_status.set("Working…".into());
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
                        let ok = get_bool(&v, "ok").unwrap_or(true);
                        let out = get_str(&v, "output").unwrap_or_else(|| "(no output)".into());
                        let c = get_num(&v, "compileMs").unwrap_or(0.0);
                        let l = get_num(&v, "linkMs");
                        let e = get_num(&v, "execMs").unwrap_or(0.0);
                        set_is_err.set(!ok);
                        set_output.set(out);
                        // std mode reports the in-rustc riwl link time separately;
                        // the wall time from the click catches everything else
                        // (per-stage breakdown is logged to the console by runner.js).
                        let wall = (js_sys::Date::now() - t_click).round() as i64;
                        set_status.set(match l {
                            Some(l) => format!(
                                "compiled in {} ms · linked in {} ms · executed in {} ms · {} ms from click",
                                c.round() as i64,
                                l.round() as i64,
                                e.round() as i64,
                                wall
                            ),
                            None => format!(
                                "compiled in {} ms · executed in {} ms · {} ms from click",
                                c.round() as i64,
                                e.round() as i64,
                                wall
                            ),
                        });
                    }
                    Err(e) => {
                        set_is_err.set(true);
                        set_output.set(format!("error: {e:?}"));
                        set_status.set(String::new());
                    }
                }
            });
        })
    };
    let on_run = {
        let run_now = run_now.clone();
        move |_| run_now()
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

    // Boot the egui editor onto the canvas exactly once, after it mounts.
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
                            configure_style(&cc.egui_ctx);
                            *egui_ctx.borrow_mut() = Some(cc.egui_ctx.clone());
                            Ok(Box::new(EditorApp { code, on_edit, diags }))
                        }),
                    )
                    .await;
            });
            true
        });
    }


    let on_example = {
        let code = code.clone();
        let egui_ctx = egui_ctx.clone();
        move |ev: leptos::ev::Event| {
            let src = match event_target_value(&ev).as_str() {
                "fizzbuzz" => EX_FIZZBUZZ,
                "fib" => EX_FIB,
                _ => DEFAULT_SRC,
            };
            *code.borrow_mut() = src.to_string();
            // Programmatic buffer changes don't fire the editor's on_edit, so persist explicitly.
            save_src(src);
            if let Some(ctx) = egui_ctx.borrow().as_ref() {
                ctx.request_repaint();
            }
        }
    };

    let on_reset = {
        let code = code.clone();
        let egui_ctx = egui_ctx.clone();
        move |_| {
            clear_src();
            *code.borrow_mut() = DEFAULT_SRC.to_string();
            if let Some(ctx) = egui_ctx.borrow().as_ref() {
                ctx.request_repaint();
            }
        }
    };

    view! {
        <div class="pg">
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
                    <button class="btn" on:click=on_reset>"Reset"</button>
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
                    <button class="btn" on:click=move |_| set_help.update(|h| *h = !*h)>"?"</button>
                </div>
                <div class="pg-spacer"></div>
                <span class="pg-tag">"100% client-side · rustc + riwl in wasm"</span>
            </div>

            {move || help.get().then(|| view! {
                <div class="pg-help">
                    <b>"Full-std Rust, entirely in your browser"</b>
                    <p>
                        "Plain "<code>"fn main"</code>" programs with the real standard library: "
                        <code>"println!(\"{}\", x)"</code>", "<code>"Vec"</code>", "<code>"HashMap"</code>", "
                        <code>"format!"</code>", files (in a sandbox), time — no server. "
                        <code>"rustc.wasm"</code>" compiles AND links your code in one pass (a forked "
                        "cranelift backend emits wasm; the built-in riwl linker links it against the "
                        "std sysroot), then it runs right here under a WASI shim."
                    </p>
                    <p>
                        "Not available: threads, networking, processes. Panics abort with a real message. "
                        "Click the "<b>"Weblings"</b>" logo for the full story."
                    </p>
                    <p>"Your edits are saved in this browser — reload and they'll still be here. \"Reset\" restores the default."</p>
                </div>
            })}

            <div class="pg-body">
                <canvas class="pg-editor" tabindex="0" node_ref=canvas_ref></canvas>
                <div class="pg-outpane">
                    <pre class="pg-output" id="output" class:err=move || is_err.get()>
                        {move || output.get()}
                    </pre>
                    <div class="pg-status" id="status">{move || status.get()}</div>
                </div>
            </div>
        </div>
    }
}

/// The ground-up story of how Weblings works — reachable from the brand button.
#[component]
fn AboutView() -> impl IntoView {
    view! {
        <div class="about">
            <div class="about-inner">
                <h1>"Weblings — real Rust, entirely in your browser"</h1>
                <p class="about-lead">
                    "Nothing here talks to a server. The Rust compiler itself runs in this page, "
                    "compiles your code to WebAssembly, links it, and runs it — all in the time it "
                    "takes to blink twice. Here is the whole stack, from the ground up."
                </p>

                <h2>"1. The compiler is a WebAssembly program"</h2>
                <p>
                    "A real "<code>"rustc"</code>" (1.96, nightly line) is itself compiled to "
                    "WebAssembly (wasm32-wasip1, ~84 MB after stripping) and executed in the page "
                    "under a tiny WASI shim that fakes a filesystem, clocks and stdio in JS. "
                    "Your code is written into that in-memory filesystem and rustc runs on it "
                    "exactly like it would on a laptop."
                </p>

                <h2>"2. A Cranelift backend that emits wasm — no LLVM anywhere"</h2>
                <p>
                    "Stock rustc uses LLVM, which is not practical inside a browser. Weblings' rustc "
                    "carries a forked "<code>"rustc_codegen_cranelift"</code>" backend: Cranelift IR "
                    "is translated to WebAssembly (via the waffle library's structured-control-flow "
                    "algorithm) and emitted as standard relocatable wasm object files — the same "
                    "linking format LLVM uses, so the two toolchains' objects are interchangeable."
                </p>

                <h2>"3. A pure-Rust linker built into the compiler"</h2>
                <p>
                    "A browser can't spawn a linker process, so one is linked INTO rustc.wasm: "
                    <code>"riwl"</code>", a small pure-Rust wasm linker. One rustc invocation "
                    "compiles your crate AND links it against the real "<code>"std"</code>" — "
                    "archive resolution, relocation patching, table/memory layout — in ~30 ms."
                </p>

                <h2>"4. The standard library is the real one"</h2>
                <p>
                    <code>"Vec"</code>", "<code>"HashMap"</code>", "<code>"format!"</code>", files, "
                    "time: your program links against genuine wasm32-wasip1 std rlibs (LLVM-built, "
                    "byte-compatible ABI), shipped as one preloaded bundle. Threads, networking and "
                    "processes don't exist under WASI in a page — everything else is ordinary Rust."
                </p>

                <h2>"5. Running your program (and your tests)"</h2>
                <p>
                    "The linked binary is a normal wasip1 command: it's instantiated with a "
                    "fresh WASI shim, "<code>"_start"</code>" is called, and stdout streams into the "
                    "output pane. The whole toolchain runs on a background worker, so the page never "
                    "freezes — and a compile can be CANCELLED mid-flight by killing its worker, which "
                    "is how live auto-run recompiles on every keystroke without queueing up stale "
                    "work. The Rustlings trainer goes further: after the fast type-check, it "
                    "builds your exercise with "<code>"--test"</code>" and runs the REAL libtest "
                    "harness — \"done\" means the tests passed, right here."
                </p>

                <h2>"6. Delivery: pinned artifacts, preloaded once"</h2>
                <p>
                    "The compiler and sysroot are built by CI from pinned forks and published as "
                    "release artifacts; this site downloads them once at page load (the progress "
                    "card), sha-verified, then the browser cache keeps them. The site itself is "
                    "100% Rust too: a Leptos + egui UI, with trunk hooks and Rust tool bins doing "
                    "the fetching, stripping and bundling."
                </p>

                <h2>"Credits"</h2>
                <p>
                    "Built on bjorn3's rustc-on-wasm branches, "<code>"rustc_codegen_cranelift"</code>
                    " and "<code>"browser_wasi_shim"</code>"; Cranelift by the Bytecode Alliance; "
                    "structured-control-flow via cfallin's waffle; exercises from rust-lang/rustlings "
                    "(MIT). The wasm backend, riwl linker, and this site are the Weblings project."
                </p>
            </div>
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

#[component]
fn App() -> impl IntoView {
    let (site, set_site) = signal(site_from_hash());
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
            <PlaygroundView active=Signal::derive(move || site.get() == Site::Playground) />
        </div>
        <div class="site-view" class:hidden=move || site.get() != Site::Rustlings>
            <RustlingsView active=Signal::derive(move || site.get() == Site::Rustlings) />
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
