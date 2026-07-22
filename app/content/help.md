**Full-std Rust, entirely in your browser**

Plain `fn main` programs with the real standard library: `println!("{}", x)`,
`Vec`, `HashMap`, `format!`, files (in a sandbox), time — no server.
`rustc.wasm` compiles AND links your code in one pass (a forked cranelift
backend emits wasm; the built-in riwl linker links it against the std sysroot),
then it runs right here under a WASI shim.

Not available: threads, networking, processes. Panics abort with a real
message. Click the **Weblings** logo for the full story.

Your edits are saved in this browser — reload and they'll still be here.
"Reset" restores the default.
