//! strip_wasm <in.wasm> <out.wasm> — drop debug-only custom sections (name,
//! producers, .debug_*). Byte-for-byte copy of everything else; removing these
//! cannot change execution. (Rust port of tools/strip-wasm-custom.py.)

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (inp, out) = (
        args.next().context("usage: strip_wasm <in.wasm> <out.wasm>")?,
        args.next().context("usage: strip_wasm <in.wasm> <out.wasm>")?,
    );
    let data = std::fs::read(&inp).with_context(|| format!("reading {inp}"))?;
    let (stripped, dropped) = xtask::strip_wasm(&data)?;
    std::fs::write(&out, &stripped).with_context(|| format!("writing {out}"))?;
    println!(
        "in {:.2} MB  out {:.2} MB  dropped {:.2} MB",
        data.len() as f64 / 1048576.0,
        stripped.len() as f64 / 1048576.0,
        dropped as f64 / 1048576.0
    );
    Ok(())
}
