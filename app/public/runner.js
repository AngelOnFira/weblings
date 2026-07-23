// runner.js — preload + worker pool + the window.* API the Rust UI calls.
//
// Execution (rustc.wasm compile+link, running programs and test harnesses)
// lives in worker.js, on a dedicated Worker — the main thread never blocks, and
// an in-flight job can be CANCELLED by terminating its worker mid-compile.
// That powers live auto-run: every keystroke submits a fresh compile and kills
// the superseded one.
//
//   preload (page load): sysroot-wasip1.bundle + rustc.wasm, streamed with byte
//     progress into the #pg-loading card; the bundle is then staged in the
//     Cache API so (re)spawned workers read it off-thread, and the rustc module
//     is passed to workers by structured clone (compiled code is shared).
//   pool: a couple of pre-warmed workers (module + parsed sysroot ready).
//     Completed workers are reused; cancelled (terminated) ones are replaced
//     in the background. One job runs at a time; the newest submission wins.
//   API: window.runRust / window.checkRust / window.runTests — same result
//     shapes as ever, plus { cancelled: true } when a newer submission
//     superseded the call. window.loadExercisesJson serves the preloaded set.
//
// rustc.wasm is non-threaded (internal memory) → no SharedArrayBuffer needed.

let rustcModule = null;
let exercisesJson = null; // preloaded rustlings exercise set
let bundleBytes = null; // fallback hand-off when the Cache API is unavailable
const BUNDLE_URL = new URL("./rustc/sysroot-wasip1.bundle", import.meta.url).href;
const BUNDLE_CACHE = "weblings-toolchain-v1";

// --- preload machinery --------------------------------------------------------
// Progress-counting fetch: streams the body, reporting received bytes via onBytes.
async function fetchCounted(url, onBytes) {
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`fetch ${url}: ${resp.status}`);
  const reader = resp.body.getReader();
  const stream = new ReadableStream({
    async pull(controller) {
      const { done, value } = await reader.read();
      if (done) { controller.close(); return; }
      onBytes(value.byteLength);
      controller.enqueue(value);
    },
    cancel(reason) { return reader.cancel(reason); },
  });
  return stream;
}

async function streamToBytes(stream) {
  const chunks = [];
  const reader = stream.getReader();
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    total += value.byteLength;
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) { out.set(c, off); off += c.byteLength; }
  return out;
}

// Preload rustc.wasm + the sysroot bundle in parallel with byte progress.
// onProgress(receivedBytes, totalBytes) — total from assets-meta.json (ground
// truth; Content-Length may be a compressed size behind some servers).
let preloadPromise = null;
window.preloadRust = function (onProgress) {
  if (preloadPromise) return preloadPromise;
  preloadPromise = (async () => {
    let total = 0;
    try {
      const meta = await (await fetch("./rustc/assets-meta.json")).json();
      total = (meta.rustcWasm | 0) + (meta.bundle | 0);
    } catch { /* indeterminate progress */ }
    let received = 0;
    const onBytes = (n) => { received += n; onProgress && onProgress(received, total); };
    const [bundle, rustc, exercises] = await Promise.all([
      fetchCounted(BUNDLE_URL, onBytes).then(streamToBytes),
      fetchCounted("./rustc/rustc.wasm", onBytes).then((stream) =>
        WebAssembly.compileStreaming(
          new Response(stream, { headers: { "Content-Type": "application/wasm" } })
        )
      ),
      fetch("./rustlings/exercises.json").then((r) => (r.ok ? r.text() : null)).catch(() => null),
    ]);
    rustcModule = rustc;
    exercisesJson = exercises;
    // Stage the 72 MB bundle in the Cache API: worker (re)spawns read it there
    // on their own thread, so cancelling never re-clones it through this one.
    try {
      const cache = await caches.open(BUNDLE_CACHE);
      await cache.put(BUNDLE_URL, new Response(bundle));
      bundleBytes = null;
    } catch {
      bundleBytes = bundle; // no Cache API: clone to each worker at spawn
    }
    ensurePool(); // warm workers so the first Run doesn't pay spawn+parse
  })();
  return preloadPromise;
};

// Auto-start at page load, driving the #pg-loading overlay card directly.
{
  const overlay = () => document.getElementById("pg-loading");
  const fmtMB = (b) => (b / 1e6).toFixed(0);
  const done = (err) => {
    const el = overlay();
    if (!el) return;
    if (err) {
      const t = document.getElementById("pg-loading-text");
      if (t) t.textContent = "preload failed (will retry on Run): " + err;
      setTimeout(() => el.remove(), 4000);
    } else {
      el.style.opacity = "0";
      setTimeout(() => el.remove(), 350);
    }
  };
  window.preloadRust((received, total) => {
    const bar = document.getElementById("pg-loading-bar");
    const text = document.getElementById("pg-loading-text");
    if (bar && total) bar.style.width = Math.min(100, (received / total) * 100).toFixed(1) + "%";
    if (text) text.textContent = total
      ? `${fmtMB(received)} / ${fmtMB(total)} MB`
        : `${fmtMB(received)} MB...`;
  }).then(() => done(null), (e) => done(e && e.message ? e.message : String(e)));
}

async function ensureReady(status) {
  if (!rustcModule) {
    status && status("Downloading toolchain...");
    try {
      await window.preloadRust();
    } catch (e) {
      // Preload failed earlier (e.g. transient network) — reset and retry once.
      preloadPromise = null;
      await window.preloadRust();
    }
  }
}

// --- worker pool --------------------------------------------------------------
// Invariant: at most ONE job in flight (`current`); at most one request waiting
// (`queued`, always the newest — older waiters resolve { cancelled: true }).
const POOL_TARGET = 2; // 1 busy + 1 warm spare during continuous typing
const idle = []; // warm, unoccupied workers
let warming = 0;
let current = null; // { handle, id, statusCb, resolve }
let queued = null; // { kind, payload, statusCb, resolve }
let jobSeq = 0;

function spawnWorker() {
  warming++;
  const handle = { worker: new Worker(new URL("./worker.js", import.meta.url), { type: "module" }) };
  const init = { type: "init", module: rustcModule, bundleUrl: BUNDLE_URL };
  if (bundleBytes) init.bundle = bundleBytes.buffer; // structured clone: one copy, master kept
  handle.worker.postMessage(init);
  handle.worker.onmessage = (e) => onWorkerMessage(handle, e.data);
  handle.worker.onerror = (e) => console.warn("[weblings] worker error:", e.message || e);
}

function onWorkerMessage(handle, msg) {
  if (msg.type === "ready") {
    warming--;
    idle.push(handle);
    dispatch();
    return;
  }
  if (msg.type === "init-error") {
    warming--;
    console.warn("[weblings] worker init failed:", msg.error);
    handle.worker.terminate();
    return;
  }
  // Job traffic: only the current job's worker+id is live — anything else is a
  // late message from a superseded job.
  if (!current || current.handle !== handle || current.id !== msg.id) return;
  if (msg.type === "status") {
    current.statusCb && current.statusCb(msg.text);
    return;
  }
  if (msg.type === "result") {
    const { resolve } = current;
    current = null;
    idle.push(handle); // completed cleanly — fully reusable
    resolve(msg.result);
    dispatch();
  }
}

function ensurePool() {
  if (!rustcModule) return;
  while (idle.length + warming + (current ? 1 : 0) < POOL_TARGET) spawnWorker();
}

function dispatch() {
  if (!queued || current) return;
  const handle = idle.pop();
  if (!handle) {
    ensurePool(); // a 'ready' message will re-enter dispatch()
    return;
  }
  const req = queued;
  queued = null;
  current = { handle, id: ++jobSeq, statusCb: req.statusCb, resolve: req.resolve };
  handle.worker.postMessage({ type: "job", id: current.id, kind: req.kind, ...req.payload });
  ensurePool(); // keep a spare warming while this job runs
}

// Submit a job, CANCELLING whatever is in flight: the busy worker is terminated
// mid-compile and the superseded promise resolves { cancelled: true }.
// "Newest wins" is decided by SUBMISSION order (seq taken synchronously), not by
// who reaches the queue first: a submission parked in ensureReady (e.g. behind
// the preload's Cache API staging) must not wake up late and cancel a newer one.
let subSeq = 0;
async function submit(kind, payload, statusCb) {
  const seq = ++subSeq;
  await ensureReady(statusCb);
  return new Promise((resolve) => {
    if (seq < subSeq) {
      resolve({ cancelled: true }); // superseded while waiting for the toolchain
      return;
    }
    if (current) {
      console.log("[weblings] cancelled in-flight", current.id, "(superseded)");
      current.handle.worker.terminate();
      current.resolve({ cancelled: true });
      current = null;
    }
    if (queued) queued.resolve({ cancelled: true });
    queued = { kind, payload, statusCb, resolve };
    dispatch();
  });
}

const asStatusCb = (status) =>
  typeof status === "function" ? (t) => { try { status(t); } catch { /* dropped closure */ } } : null;

// --- public API (same shapes as the pre-worker implementation) ----------------
window.runRust = (source, status) => submit("run", { source }, asStatusCb(status));

window.checkRust = (source, isTest, constCheck, status) =>
  submit("check", { source, isTest, constCheck }, asStatusCb(status));

window.runTests = (source, status) => submit("tests", { source }, asStatusCb(status));

// Save `text` as a file download (the UI truncates huge program output and
// offers the full text through this).
window.downloadText = (filename, text) => {
  const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  URL.revokeObjectURL(url);
};

// Bundled Rustlings exercises (raw JSON text for serde_json on the Rust side) —
// preloaded with everything else; falls back to a direct fetch.
window.loadExercisesJson = async function () {
  if (exercisesJson != null) return exercisesJson;
  return await (await fetch("./rustlings/exercises.json")).text();
};
