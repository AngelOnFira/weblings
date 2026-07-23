// worker.js — the toolchain execution engine, running in a dedicated Worker.
//
// Everything that executes wasm (rustc.wasm compile+link, the produced program,
// libtest harnesses) lives HERE, off the main thread, for two reasons:
//   1. snappiness: a compile never blocks typing/rendering in the page;
//   2. cancellation: synchronous wasm cannot be interrupted, but a Worker can be
//      killed mid-compile with terminate() — runner.js does exactly that when a
//      newer keystroke supersedes an in-flight run.
//
// Protocol (runner.js is the only client):
//   → { type:"init", module, bundleUrl, bundle? }   rustc module (structured
//     clone) + the sysroot bundle: transferred bytes if given, else read from
//     the Cache API / network by URL
//   ← { type:"ready" } | { type:"init-error", error }
//   → { type:"job", id, kind:"run"|"check"|"tests", source, ... }
//   ← { type:"status", id, text }*  then  { type:"result", id, result }
import { Fd, File, Directory, PreopenDirectory, WASI } from "./vendor/browser_wasi_shim/index.js";

let rustcModule = null;
let stdSysroot = null; // Map(relPath -> File) per sysroot-wasip1 bundle index

// Parse the RIWB1 sysroot bundle: "RIWB1\n" + u32le index length + JSON index
// {files:[{p,o,l}], total} + concatenated bytes.
function parseBundle(bytes) {
  const magic = new TextDecoder().decode(bytes.subarray(0, 6));
  if (magic !== "RIWB1\n") throw new Error("bad sysroot bundle magic");
  const ilen = new DataView(bytes.buffer, bytes.byteOffset + 6, 4).getUint32(0, true);
  const index = JSON.parse(new TextDecoder().decode(bytes.subarray(10, 10 + ilen)));
  const base = 10 + ilen;
  const files = new Map();
  for (const f of index.files) {
    if (f.p === "manifest.json") continue;
    files.set(f.p, new File(bytes.slice(base + f.o, base + f.o + f.l)));
  }
  return files;
}

function stdSysrootPreopen() {
  // Build the real sysroot tree: lib/rustlib/wasm32-wasip1/lib/... from flat paths.
  const root = new Map();
  const dirFor = (segs) => {
    let m = root;
    for (const seg of segs) {
      if (!m.has(seg)) m.set(seg, new Map());
      m = m.get(seg);
    }
    return m;
  };
  for (const [path, file] of stdSysroot) {
    const segs = path.split("/");
    const name = segs.pop();
    dirFor(segs).set(name, file);
  }
  const toDir = (m) =>
    new Directory([...m.entries()].map(([n, v]) => [n, v instanceof Map ? toDir(v) : v]));
  return new PreopenDirectory("/sysroot", [...root.entries()].map(
    ([n, v]) => [n, v instanceof Map ? toDir(v) : v]));
}

async function initEngine(msg) {
  rustcModule = msg.module;
  let bytes = msg.bundle ? new Uint8Array(msg.bundle) : null;
  if (!bytes && typeof caches !== "undefined") {
    // Preload staged the bundle here so worker (re)spawns read it off-thread.
    try {
      const hit = await caches.match(msg.bundleUrl);
      if (hit) bytes = new Uint8Array(await hit.arrayBuffer());
    } catch { /* fall through to network (browser HTTP cache) */ }
  }
  if (!bytes) {
    const resp = await fetch(msg.bundleUrl);
    if (!resp.ok) throw new Error(`fetch sysroot bundle: ${resp.status}`);
    bytes = new Uint8Array(await resp.arrayBuffer());
  }
  stdSysroot = parseBundle(bytes);
}

// ---- run: compile+link (one rustc.wasm invocation) then execute -------------
async function runJob(msg, status) {
  status("Compiling + linking with rustc.wasm...");
  const tEnter = performance.now();
  let log = "";
  const dec0 = new TextDecoder();
  const CapErr = class extends Fd {
    fd_write(data) { log += dec0.decode(data, { stream: true }); return { ret: 0, nwritten: data.byteLength }; }
  };
  const work = new PreopenDirectory("/work", [["prog.rs", new File(new TextEncoder().encode(msg.source))]]);
  const args = [
    "rustc", "/work/prog.rs", "--sysroot", "/sysroot",
    "-Zunstable-options", "--target", "wasm32-wasip1",
    "--edition", "2024", "-O", "-Cpanic=abort",
    "--error-format=json",
    "-o", "/work/prog.wasm",
  ];
  const fds = [new CapErr(), new CapErr(), new CapErr(), new PreopenDirectory("/tmp", []), stdSysrootPreopen(), work];
  const w = new WASI(args, ["CLIF2WASM_OBJECT=1", "RIWL_TIMINGS=1"], fds, { debug: false });
  const setupMs = performance.now() - tEnter; // sysroot tree + WASI fd assembly
  // Instantiating the 84 MB rustc module is a real per-run cost (memory alloc +
  // start) that used to be invisible — it ran before the compile timer started.
  const tInst = performance.now();
  const inst = await WebAssembly.instantiate(rustcModule, {
    wasi_snapshot_preview1: w.wasiImport,
  });
  const rustcInstantiateMs = performance.now() - tInst;
  const t0 = performance.now();
  let exit = 0;
  try {
    exit = w.start(inst);
  } catch (e) {
    const m = e && e.message ? e.message : String(e);
    if (!log.trim()) log += m;
    exit = 1;
  }
  const totalMs = performance.now() - t0;
  // riwl prints one machine-readable timing line to stderr (RIWL_TIMINGS=1):
  // "riwl-timing: total_ms=N load_ms=N resolve_ms=N layout_ms=N apply_ms=N emit_ms=N".
  // Split compile vs link from it, log the phase breakdown, strip it from
  // user-visible diagnostics.
  let linkMs = null;
  let linkPhases = null;
  const tm = log.match(/riwl-timing: ([^\n]+)/);
  if (tm) {
    linkPhases = Object.fromEntries(
      tm[1].trim().split(/\s+/).map((kv) => kv.split("=")).map(([k, v]) => [k, Number(v)])
    );
    linkMs = linkPhases.total_ms;
    console.log("[riwl] link phases:", tm[1].trim());
    log = log.replace(/^.*riwl-timing:[^\n]*\n?/m, "");
  }
  const compileMs = linkMs != null ? Math.max(0, totalMs - linkMs) : totalMs;
  // rustc emits JSON diagnostics (one per stderr line); the structured list
  // feeds the in-editor squiggles, and its `rendered` texts reconstruct the
  // human-readable error output (fallback to raw stderr for ICEs etc.).
  const diagnostics = parseDiagnostics(log).map((d) => ({
    ...d,
    rendered: d.rendered.replaceAll("/work/prog.rs", "program"),
  }));
  const bin = work.dir.contents.get("prog.wasm");
  if (!bin || !bin.data || bin.data.length === 0) {
    const msgOut = diagnostics.length
      ? diagnostics.map((d) => d.rendered).join("\n")
      : log.replaceAll("/work/prog.rs", "program").trim();
    return { ok: false, diagnostics, output: msgOut || `rustc exited ${exit} without emitting a program`, compileMs, execMs: 0 };
  }

  status("Running...");
  let progOut = "";
  const dec1 = new TextDecoder();
  const CapOut = class extends Fd {
    fd_write(data) { progOut += dec1.decode(data, { stream: true }); return { ret: 0, nwritten: data.byteLength }; }
  };
  const t1 = performance.now();
  const pfds = [new CapOut(), new CapOut(), new CapOut(), new PreopenDirectory("/sandbox", [])];
  const pw = new WASI(["prog"], [], pfds, { debug: false });
  const finish = (ok, output, progInstantiateMs, execMs) => {
    const stages = {
      setupMs: +setupMs.toFixed(1),
      rustcInstantiateMs: +rustcInstantiateMs.toFixed(1),
      compileMs: +compileMs.toFixed(1),
      linkMs,
      progInstantiateMs: +progInstantiateMs.toFixed(1),
      execMs: +execMs.toFixed(1),
      totalMs: +(performance.now() - tEnter).toFixed(1),
    };
    console.log("[playground] stages:", JSON.stringify(stages), linkPhases ? "link phases: " + JSON.stringify(linkPhases) : "");
    // diagnostics: warnings survive successful compiles — the editor shows them.
    return { ok, output, diagnostics, compileMs, linkMs, linkPhases, execMs, stages, totalMs: stages.totalMs };
  };
  try {
    const tPI = performance.now();
    const { instance } = await WebAssembly.instantiate(bin.data.slice().buffer, {
      wasi_snapshot_preview1: pw.wasiImport,
    });
    const progInstantiateMs = performance.now() - tPI;
    const tRun = performance.now();
    const rc = pw.start(instance);
    const execMs = performance.now() - tRun;
    return finish(rc === 0, progOut.trimEnd() + (rc !== 0 ? `\n[exit code ${rc}]` : ""), progInstantiateMs, execMs);
  } catch (e) {
    const msgOut = progOut.trimEnd() + "\n[runtime error: " + (e && e.message ? e.message : e) + "]";
    return finish(false, msgOut.trim(), 0, performance.now() - t1);
  }
}

// ---- check: type-check only (--emit metadata), structured diagnostics -------
function parseDiagnostics(log) {
  const out = [];
  for (const line of log.split("\n")) {
    if (line[0] !== "{") continue;
    let d;
    try { d = JSON.parse(line); } catch { continue; }
    if (d.$message_type !== "diagnostic") continue;
    if (d.level !== "error" && d.level !== "warning") continue;
    const spans = d.spans || [];
    const sp = spans.find((s) => s.is_primary) || spans[0] || null;
    out.push({
      level: d.level,
      message: d.message,
      code: d.code && d.code.code ? d.code.code : null,
      line: sp ? sp.line_start : null,
      col: sp ? sp.column_start : null,
      endLine: sp ? sp.line_end : null,
      endCol: sp ? sp.column_end : null,
      rendered: (d.rendered || d.message).trimEnd(),
    });
  }
  return out;
}

async function checkJob(msg, status) {
  status("Type-checking...");

  // Optional const-eval verifier: appended anonymous const whose assertions rustc
  // EVALUATES at compile time — a wrong answer becomes an E0080 compile error.
  let src = msg.source;
  if (msg.constCheck && msg.constCheck.length) src = msg.source + "\n" + msg.constCheck + "\n";

  const dec = new TextDecoder();
  let log = "";
  class Cap extends Fd {
    fd_write(data) { log += dec.decode(data, { stream: true }); return { ret: 0, nwritten: data.byteLength }; }
  }

  const work = new PreopenDirectory("/work", [["prog.rs", new File(new TextEncoder().encode(src))]]);
  const args = [
    "rustc", "/work/prog.rs", "--sysroot", "/sysroot",
    "--target", "wasm32-wasip1", "--edition", "2024",
    "--emit", "metadata", "-o", "/work/libprog.rmeta",
    "--error-format=json",
  ];
  // Test-mode exercises: type-check the #[cfg(test)] module too (does NOT run tests).
  if (msg.isTest) args.push("--test");

  const fds = [new Cap(), new Cap(), new Cap(), new PreopenDirectory("/tmp", []), stdSysrootPreopen(), work];
  const w = new WASI(args, [], fds, { debug: false });
  const inst = await WebAssembly.instantiate(rustcModule, {
    wasi_snapshot_preview1: w.wasiImport,
  });

  const t0 = performance.now();
  let exit = 0;
  try {
    exit = w.start(inst);
  } catch (e) {
    // rustc aborts via a wasm trap after printing diagnostics.
    exit = 1;
  }
  const compileMs = performance.now() - t0;

  const diagnostics = parseDiagnostics(log).map((d) => ({
    ...d,
    rendered: d.rendered.replaceAll("/work/prog.rs", "program"),
  }));
  const errorCount = diagnostics.filter((d) => d.level === "error").length;
  const warningCount = diagnostics.filter((d) => d.level === "warning").length;
  const ok = exit === 0 && errorCount === 0;
  const text = diagnostics
    .slice()
    .sort((a, b) => (a.level === b.level ? 0 : a.level === "error" ? -1 : 1))
    .map((d) => d.rendered)
    .join("\n");
  return { ok, errorCount, warningCount, compileMs, text, diagnostics };
}

// ---- tests: compile with --test and RUN the libtest harness -----------------
// panic=abort semantics: a failing assert aborts the harness — we still get a
// clear verdict + the panic message via --nocapture, but tests after the first
// failure don't run. WASI has no threads: thread-spawn failures are detected
// and reported as { unsupported: "threads" }.
async function testsJob(msg, status) {
  status("Building tests...");

  const dec = new TextDecoder();
  let log = "";
  class CapC extends Fd {
    fd_write(data) { log += dec.decode(data, { stream: true }); return { ret: 0, nwritten: data.byteLength }; }
  }
  const work = new PreopenDirectory("/work", [["prog.rs", new File(new TextEncoder().encode(msg.source))]]);
  const args = [
    "rustc", "/work/prog.rs", "--sysroot", "/sysroot",
    "--target", "wasm32-wasip1", "--edition", "2024",
    "--test", "-O", "-Cpanic=abort",
    "-o", "/work/tests.wasm",
  ];
  const fds = [new CapC(), new CapC(), new CapC(), new PreopenDirectory("/tmp", []), stdSysrootPreopen(), work];
  const w = new WASI(args, ["CLIF2WASM_OBJECT=1"], fds, { debug: false });
  const inst = await WebAssembly.instantiate(rustcModule, { wasi_snapshot_preview1: w.wasiImport });
  const t0 = performance.now();
  let exit = 0;
  try { exit = w.start(inst); } catch (e) { if (!log.trim()) log += String(e && e.message || e); exit = 1; }
  const compileMs = performance.now() - t0;
  const bin = work.dir.contents.get("tests.wasm");
  if (exit !== 0 || !bin || !bin.data || bin.data.length === 0) {
    return { ok: false, phase: "compile", output: log.replaceAll("/work/prog.rs", "exercise").trim(), compileMs, execMs: 0 };
  }

  status("Running tests...");
  let out = "";
  class CapT extends Fd {
    fd_write(data) { out += dec.decode(data, { stream: true }); return { ret: 0, nwritten: data.byteLength }; }
  }
  const t1 = performance.now();
  const pfds = [new CapT(), new CapT(), new CapT(), new PreopenDirectory("/sandbox", [])];
  // --nocapture: with panic=abort a failing assert kills the harness before it
  // could print captured output — streaming makes the panic message visible.
  const pw = new WASI(["tests", "--test-threads=1", "--nocapture"], [], pfds, { debug: false });
  let rc = 0;
  let trapped = false;
  try {
    const { instance } = await WebAssembly.instantiate(bin.data.slice().buffer, {
      wasi_snapshot_preview1: pw.wasiImport,
    });
    rc = pw.start(instance);
  } catch (e) {
    trapped = true;
    rc = 101;
  }
  const execMs = performance.now() - t1;
  const cleaned = out.replaceAll("/work/prog.rs", "exercise").trim();

  if (/failed to spawn thread|operation not supported/i.test(cleaned)) {
    return { ok: false, unsupported: "threads", phase: "run", output: cleaned, compileMs, execMs };
  }
  const m = cleaned.match(/test result: (ok|FAILED)\. (\d+) passed; (\d+) failed/);
  if (m) {
    return {
      ok: rc === 0 && m[1] === "ok",
      passed: Number(m[2]),
      failed: Number(m[3]),
      phase: "run",
      output: cleaned,
      compileMs, execMs,
    };
  }
  // Aborted before the summary (panic=abort fail path): the panic message is the verdict.
  return { ok: false, phase: "run", trapped, output: cleaned || "test harness aborted", compileMs, execMs };
}

const jobs = { run: runJob, check: checkJob, tests: testsJob };

self.onmessage = async (e) => {
  const msg = e.data;
  if (msg.type === "init") {
    try {
      await initEngine(msg);
      self.postMessage({ type: "ready" });
    } catch (err) {
      self.postMessage({ type: "init-error", error: String((err && err.message) || err) });
    }
    return;
  }
  if (msg.type === "job") {
    const status = (text) => self.postMessage({ type: "status", id: msg.id, text });
    let result;
    try {
      result = await jobs[msg.kind](msg, status);
    } catch (err) {
      result = { ok: false, output: "engine error: " + String((err && err.message) || err) };
    }
    self.postMessage({ type: "result", id: msg.id, result });
  }
};
