**Weblings, A Rust compiler in your browser**

Weblings has a full Rust compiler toolchain compiled to WASM. This includes
rustc for the compiler frontend, Cranelift for the compiler backend, and a
custom WASM linker to get a WASM binary that can be run by your browser!

Many things won't work, since there isn't a real operating system running the
binary. This means you can't do threads, networking, and normal OS calls.
