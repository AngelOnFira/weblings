# Weblings — real Rust, entirely in your browser

Nothing here talks to a server. The Rust compiler itself runs in this page,
compiles your code to WebAssembly, links it, and runs it — all in the time it
takes to blink twice. Here is the whole stack, from the ground up.

## 1. The compiler is a WebAssembly program

A real `rustc` (1.96, nightly line) is itself compiled to WebAssembly
(wasm32-wasip1, ~84 MB after stripping) and executed in the page under a tiny
WASI shim that fakes a filesystem, clocks and stdio in JS. Your code is written
into that in-memory filesystem and rustc runs on it exactly like it would on a
laptop.

## 2. A Cranelift backend that emits wasm — no LLVM anywhere

Stock rustc uses LLVM, which is not practical inside a browser. Weblings' rustc
carries a forked `rustc_codegen_cranelift` backend: Cranelift IR is translated
to WebAssembly (via the waffle library's structured-control-flow algorithm) and
emitted as standard relocatable wasm object files — the same linking format
LLVM uses, so the two toolchains' objects are interchangeable.

## 3. A pure-Rust linker built into the compiler

A browser can't spawn a linker process, so one is linked INTO rustc.wasm:
`riwl`, a small pure-Rust wasm linker. One rustc invocation compiles your crate
AND links it against the real `std` — archive resolution, relocation patching,
table/memory layout — in ~30 ms.

## 4. The standard library is the real one

`Vec`, `HashMap`, `format!`, files, time: your program links against genuine
wasm32-wasip1 std rlibs (LLVM-built, byte-compatible ABI), shipped as one
preloaded bundle. Threads, networking and processes don't exist under WASI in a
page — everything else is ordinary Rust.

## 5. Running your program (and your tests)

The linked binary is a normal wasip1 command: it's instantiated with a fresh
WASI shim, `_start` is called, and stdout streams into the output pane. The
whole toolchain runs on a background worker, so the page never freezes — and a
compile can be CANCELLED mid-flight by killing its worker, which is how live
auto-run recompiles on every keystroke without queueing up stale work. The
Rustlings tab goes further: after the fast type-check, it builds your exercise
with `--test` and runs the REAL libtest harness — "done" means the tests
passed, right here.

## 6. Delivery: pinned artifacts, preloaded once

The compiler and sysroot are built by CI from pinned forks and published as
release artifacts; this site downloads them once at page load (the progress
card), sha-verified, then the browser cache keeps them. The site itself is
100% Rust too: a Leptos + egui UI, with trunk hooks and Rust tool bins doing
the fetching, stripping and bundling.

## Credits

Built on bjorn3's rustc-on-wasm branches, `rustc_codegen_cranelift` and
`browser_wasi_shim`; Cranelift by the Bytecode Alliance;
structured-control-flow via cfallin's waffle; exercises from
rust-lang/rustlings (MIT). The wasm backend, riwl linker, and this site are the
Weblings project.
