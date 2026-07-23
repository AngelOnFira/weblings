# Weblings

Weblings is a Rust compiler toolchain compiled to WASM. This means that you can
compile and execute Rust code in your browser!

Tech stack tl;dr `rust code -> rustc -> cranelift IR -> waffle -> wasm object
files -> linker -> wasm executable -> browser execution`

## The web UI

The primary way to interface with this WASM toolchain is through the web UI,
which you can access at
[https://weblings.forest-anderson.ca](https://weblings.forest-anderson.ca). It
has two sections:

- Playground: Similar to the [Rust Playground](https://play.rust-lang.org), but
  the code compiles and runs in your browser
- Rustlings: The beloved [Rustlings excercise
  suite](https://rustlings.rust-lang.org), brought to you in your browser!

## The stack

A few things had to be set up to get this toolchain working:

- rustc compiled to WASM: [@bjorn3](https://github.com/bjorn3) has [set up
  branches](https://github.com/bjorn3/rust/tree/compile_rustc_for_wasm20) that
  patch rustc so that it can run inside WASM correctly. Very awesome! I build
  the rustc binaries for WASM [in this
  repo](https://github.com/AngelOnFira/wasm-rustc).
- ⭐ [Cranelift IR -> waffle
  translation](https://github.com/AngelOnFira/rust/tree/riw-wasm20/compiler/rustc_codegen_cranelift/clif2wasm):
  a crate to go from Cranelift's intermediate representation (CLIF) to
  [waffle's](https://github.com/bytecodealliance/waffle) own intermediate
  representation, which waffle can then translate to WASM. This solution was
  inspired by disussion [in this issue on
  bytecodealliance/wasmtime](https://github.com/bytecodealliance/wasmtime/issues/2566).
- ⭐ [A wasm
  linker](https://github.com/AngelOnFira/rust/tree/riw-wasm20/compiler/rustc_codegen_cranelift/riwl):
  a crate to link the compiled WASM object to the bundled std sysroot rlibs.
  There are some alternatives, like `wasm-ld` from LLVM. I was hoping that the
  [Wild linker](https://github.com/wild-linker/wild) might have WASM linking
  support, but that's [still on the
  way](https://github.com/wild-linker/wild/issues/1431).

⭐ - these steps were written with AI tooling to achieve the goals of this
project, but I'd love to see them included in Cranelift/Wild, and hope to find a
way to upstream them!

## Why?

I've set up this toolchain so that I can build out some tooling that I'll be
using in upcoming projects to teach computer memory principles. I've been
exploring [visualizing memory segments during C code
execution](https://c-ray.forest-anderson.ca/), and I'll be hopefully expanding
that to Rust soon. Also, I want more ways to get quick feedback when learning
Rust concepts!

There is also the question of why to use Cranelift instead of LLVM as the
compiler backend. Here are a few things on my mind:

- LLVM would be a larger toolchain to include (I think)
- Cranelift is faster to get (I did not test numbers)
- The whole toolchain in Rust is pretty neat!

## Prior art

- [rubrc](https://github.com/oligamiq/rubrc):
  ([website](https://rubrc.pages.dev/)) Uses rustc compiled to WASM, with LLVM
  as the backend, also compiled to WASM.
- [rubri](https://github.com/lyonsyonii/rubri):
  ([website](https://garriga.dev/rubri/)) Uses Miri to create a Rust interpreter
  that can run in WASM.

(If you know of others, please make a PR!)

## AI Assistance

AI tooling was used to create the Leptos web UI, the Cranelift -> [Waffle
IR](https://github.com/bytecodealliance/waffle) translation, the WASM linker,
and various other scripts. Written content in this repo is human made.
