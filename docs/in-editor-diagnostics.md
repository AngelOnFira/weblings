# In-editor diagnostics — SHIPPED (D1+D2); this doc now records the design

*Originally (2026-07-22) an assessment of what "language server support" would
take. The answer turned out to be: no language server, no editor fork — and
D1 (line markers) + D2 (column-accurate squiggles) shipped together. Kept for
the design rationale and the D3 follow-ups.*

## What shipped

Errors and warnings appear IN both egui editors as you type:

- **Squiggles** under the exact `col..endCol` span (Gruvbox red for errors,
  yellow for warnings), **gutter bars** at the pane's left edge on offending
  lines, and a **hover tooltip** showing rustc's full rendered message for the
  hovered line.
- **Data**: rustc itself is the diagnostics engine — `--error-format=json`
  parsed in `worker.js::parseDiagnostics` into `{level, line, col, endLine,
  endCol, rendered}`. Two producers, one shape:
  - auto-run OFF → a debounced (300 ms) `checkRust` (`--emit metadata`,
    ~250–400 ms) per pause in typing, skipped while a manual Run is in flight
    (the single-slot newest-wins pool would cancel it);
  - auto-run ON → the per-keystroke `runRust` results carry `diagnostics` too,
    so markers ride along with the runs instead of fighting them.
  The trainer feeds its existing per-keystroke `checkRust` results into the
  same renderer (markers are additive to its diagnostics panel).
- **Rendering** (`app/src/diag.rs`): painter-only overlay, no widget fork.
  The key fact the original assessment missed: `egui_code_editor 0.3.7`'s
  `CodeEditor::show()` *returns* egui's `TextEditOutput`, which carries
  `galley`, `galley_pos` and `text_clip_rect`. `Galley.rows[line-1].rect()`
  gives the row rectangle (1:1 with source lines — wrap is off in numlines
  mode) and `Row::x_offset(CharIndex)` gives exact glyph x-positions, so
  squiggles are column-accurate with zero monospace math. Stale spans while
  typing are clamped, never panic.
- **Testability**: marker state is published to `window.__weblings_diags`
  (`{errors, warnings}`) — canvas paint isn't DOM-assertable;
  `verify/verify-diags.mjs` drives both editors through broken→fixed code.

## Why not rust-analyzer (unchanged verdict)

- What it adds over rustc-as-checker is IDE features (completions, hover
  types, goto-def) — not better error reporting. rustc's diagnostics are
  always right; RA's are a reimplementation.
- What it costs: a ~30–40 MB wasm build with no official artifact, the std
  *sources* shipped for analysis, an LSP transport, and — the blocker under
  the egui/no-JS-deps constraint — an egui LSP-client widget (completion
  popups, hover infrastructure) that doesn't exist. That's its own multi-week
  project, worth doing only if completions become a goal.

## D3 follow-ups (not yet built)

- **Click-to-line**: clicking a diagnostic in the trainer panel moves the
  editor cursor there (set `TextEdit` cursor state via `TextEditState`).
- **Quick-fix**: rustc's JSON already carries `suggested_replacement` spans —
  apply = string splice into the shared buffer + repaint. A "fix it" affordance
  in the tooltip would be a day-ish increment.
- **Toolbar count chip** ("2 errors") if the markers alone prove too subtle.
