//! prep — stage the toolchain artifacts for the site. Runs as trunk's pre_build
//! hook (fast no-op when everything is already in place) and as the manual /
//! main-repo fetch entry.
//!
//! Modes:
//!   (default, lean)      stage ONLY what the site ships into app/public/rustc:
//!                        stripped rustc.wasm + sysroot-wasip1.bundle +
//!                        assets-meta.json. The loose sysroot / std-sysroot /
//!                        riscv artifacts are never extracted — nothing to prune.
//!   --full --public DIR  extract EVERYTHING into DIR (loose wasip1 sysroot,
//!                        x86_64 std-sysroot, riscv sysroot) — used by the main
//!                        rust-in-wasm repo, whose node gates and legacy modes
//!                        want the loose files.
//!   --post               trunk post_build hook: write CNAME into
//!                        $TRUNK_STAGING_DIR when CNAME_DOMAIN is set.
//!
//! Local source (both modes): RIW_ARTIFACTS_LOCAL=<path to another checkout's
//! public/> copies instead of downloading.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use xtask::*;

fn main() {
    if let Err(e) = run() {
        eprintln!("prep: error: {e:#}");
        std::process::exit(1);
    }
}

fn repo_root() -> PathBuf {
    // xtask/ lives at the repo root; CARGO_MANIFEST_DIR is stable regardless of cwd.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn run() -> Result<()> {
    let (flags, _pos) = parse_args(&["public"]);

    if flags.contains_key("post") {
        return post_build();
    }

    let root = repo_root();
    let full = flags.contains_key("full");
    let public = flags
        .get("public")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("app/public"));
    let rustc_dir = public.join("rustc");

    // Fast path: everything staged already (keeps `trunk serve` rebuilds snappy).
    if !full
        && rustc_dir.join("rustc.wasm").exists()
        && rustc_dir.join("sysroot-wasip1.bundle").exists()
        && rustc_dir.join("assets-meta.json").exists()
    {
        return Ok(());
    }

    if let Ok(local) = std::env::var("RIW_ARTIFACTS_LOCAL") {
        return stage_local(Path::new(&local), &public, full);
    }

    let lock = parse_lock(&root.join("artifacts.lock"))?;
    eprintln!("prep: staging artifacts from {}@{}", lock.repo, lock.tag);
    fs::create_dir_all(&rustc_dir)?;

    for (name, sha) in &lock.assets {
        match name.as_str() {
            "rustc-wasm.tar.zst" => {
                let a = fetch_asset(&lock, name, sha)?;
                let got = extract_tar_zst(&a, |p| {
                    (p.file_name()?.to_str()? == "rustc.wasm").then(|| rustc_dir.join("rustc.wasm"))
                })?;
                ensure!(!got.is_empty(), "{name}: no rustc.wasm inside");
                // Debug/metadata custom sections are dead weight in the browser;
                // CI blobs arrive pre-stripped, so this is usually a no-op.
                let bytes = fs::read(rustc_dir.join("rustc.wasm"))?;
                let (stripped, dropped) = strip_wasm(&bytes)?;
                if dropped > 0 {
                    fs::write(rustc_dir.join("rustc.wasm"), &stripped)?;
                    eprintln!("  stripped rustc.wasm: dropped {:.1} MB", dropped as f64 / 1e6);
                }
            }
            "wasip1-sysroot.tar.zst" => {
                let a = fetch_asset(&lock, name, sha)?;
                if full {
                    // Everything, preserving the rustc/... layout.
                    extract_tar_zst(&a, |p| Some(public.join(p)))?;
                    let n = write_files_manifest(&public.join("rustc/sysroot-wasip1"))?;
                    eprintln!("  wasip1 sysroot: {n} loose files + bundle");
                } else {
                    // Lean: the site only uses the single-file bundle.
                    let got = extract_tar_zst(&a, |p| {
                        (p.file_name()?.to_str()? == "sysroot-wasip1.bundle")
                            .then(|| rustc_dir.join("sysroot-wasip1.bundle"))
                    })?;
                    ensure!(!got.is_empty(), "{name}: no sysroot-wasip1.bundle inside");
                }
            }
            "std-sysroot.tar.zst" if full => {
                let a = fetch_asset(&lock, name, sha)?;
                extract_tar_zst(&a, |p| Some(public.join(p)))?;
                let lib = public.join("std-sysroot/lib/rustlib/x86_64-unknown-linux-gnu/lib");
                let n = write_array_manifest(
                    &lib,
                    &public.join("std-sysroot/manifest.json"),
                    &[".rlib", ".rmeta"],
                )?;
                eprintln!("  x86_64 std-sysroot: {n} entries");
            }
            "riscv64-sysroot.tar.zst" if full => {
                let a = fetch_asset(&lock, name, sha)?;
                extract_tar_zst(&a, |p| Some(public.join(p)))?;
                let lib = public.join("rustc/sysroot/lib/rustlib/riscv64gc-unknown-none-elf/lib");
                if lib.exists() {
                    write_array_manifest(&lib, &public.join("rustc/sysroot/manifest.json"), &[".rlib"])?;
                }
            }
            // Lean mode: assets the site doesn't ship are not even downloaded.
            _ => {}
        }
    }

    write_assets_meta(&rustc_dir)?;
    eprintln!("prep: done ({})", if full { "full" } else { "lean" });
    Ok(())
}

fn stage_local(src: &Path, public: &Path, full: bool) -> Result<()> {
    eprintln!("prep: local mode, copying from {}", src.display());
    let rustc_dir = public.join("rustc");
    fs::create_dir_all(&rustc_dir)?;
    for f in ["rustc.wasm", "sysroot-wasip1.bundle"] {
        let from = src.join("rustc").join(f);
        ensure!(from.exists(), "{} not found", from.display());
        fs::copy(&from, rustc_dir.join(f))?;
    }
    if full {
        copy_dir(&src.join("rustc"), &rustc_dir)?;
        if src.join("std-sysroot").exists() {
            copy_dir(&src.join("std-sysroot"), &public.join("std-sysroot"))?;
        }
    }
    write_assets_meta(&rustc_dir)?;
    eprintln!("prep: done (local{})", if full { ", full" } else { "" });
    Ok(())
}

fn post_build() -> Result<()> {
    if let Ok(domain) = std::env::var("CNAME_DOMAIN") {
        if !domain.is_empty() {
            let staging = std::env::var("TRUNK_STAGING_DIR")
                .context("TRUNK_STAGING_DIR not set (run as a trunk post_build hook)")?;
            fs::write(Path::new(&staging).join("CNAME"), format!("{domain}\n"))?;
            eprintln!("prep: wrote CNAME ({domain})");
        }
    }
    Ok(())
}
