//! Native site tooling shared by the bins:
//!   prep          — fetch/verify/extract the toolchain artifacts (trunk pre_build hook)
//!   gen_exercises — bundle a rustlings checkout into exercises.json
//!   strip_wasm    — drop debug-only custom sections from a wasm module

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------- artifacts.lock

pub struct Lock {
    pub repo: String,
    pub tag: String,
    /// (asset file name, hex sha256), in file order.
    pub assets: Vec<(String, String)>,
}

pub fn parse_lock(path: &Path) -> Result<Lock> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut repo = None;
    let mut tag = None;
    let mut assets = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("repo=") {
            repo = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("tag=") {
            tag = Some(v.to_string());
        } else {
            let mut it = line.split_whitespace();
            let (Some(name), Some(sha)) = (it.next(), it.next()) else {
                bail!("bad lock line: {line}");
            };
            let sha = sha.strip_prefix("sha256=").context("asset line missing sha256=")?;
            assets.push((name.to_string(), sha.to_string()));
        }
    }
    Ok(Lock {
        repo: repo.context("artifacts.lock: missing repo=")?,
        tag: tag.context("artifacts.lock: missing tag=")?,
        assets,
    })
}

// ------------------------------------------------------------ download + verify

pub fn cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("RIW_ARTIFACTS_CACHE") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/riw-artifacts")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Download (with cache + sha verification) one release asset; returns the
/// cached file path. Cache layout matches the old fetch-artifacts.sh
/// (`<sha>-<name>`), so existing caches keep working.
pub fn fetch_asset(lock: &Lock, name: &str, sha: &str) -> Result<PathBuf> {
    let cache = cache_dir();
    fs::create_dir_all(&cache)?;
    let cached = cache.join(format!("{sha}-{name}"));
    if cached.exists() {
        return Ok(cached);
    }
    let url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        lock.repo, lock.tag, name
    );
    eprintln!("  downloading {name} from {url}");
    let mut last_err = None;
    for attempt in 1..=3 {
        match try_download(&url) {
            Ok(bytes) => {
                let got = sha256_hex(&bytes);
                ensure!(
                    got == sha,
                    "sha256 mismatch for {name}: got {got}, lock says {sha} (update artifacts.lock?)"
                );
                let tmp = cached.with_extension("tmp");
                fs::write(&tmp, &bytes)?;
                fs::rename(&tmp, &cached)?;
                return Ok(cached);
            }
            Err(e) => {
                eprintln!("  attempt {attempt} failed: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap()).with_context(|| format!("downloading {url}"))
}

fn try_download(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url).call().with_context(|| format!("GET {url}"))?;
    let mut bytes = Vec::new();
    resp.into_reader().read_to_end(&mut bytes)?;
    Ok(bytes)
}

// --------------------------------------------------------- selective extraction

/// Extract entries from a .tar.zst whose paths pass `want`, mapping each archive
/// path to a destination via `dest_for` (None = skip). Returns extracted paths.
pub fn extract_tar_zst(
    archive: &Path,
    mut dest_for: impl FnMut(&Path) -> Option<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let f = fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let dec = zstd::stream::read::Decoder::new(f)?;
    let mut tar = tar::Archive::new(dec);
    let mut out = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let Some(dest) = dest_for(&path) else { continue };
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        fs::write(&dest, &buf)?;
        out.push(dest);
    }
    Ok(out)
}

// ------------------------------------------------------------------- wasm strip

/// Custom sections that are debug/metadata only — removing them cannot change
/// execution. Everything else is copied byte-for-byte (no re-encoding).
pub fn strip_wasm(input: &[u8]) -> Result<(Vec<u8>, usize)> {
    ensure!(input.len() >= 8 && &input[..4] == b"\0asm", "not a wasm module");
    let mut out = input[..8].to_vec();
    let mut i = 8usize;
    let mut dropped = 0usize;
    while i < input.len() {
        let sid = input[i];
        let (size, k) = uleb(input, i + 1)?;
        let seg_end = k + size as usize;
        ensure!(seg_end <= input.len(), "truncated section at {i}");
        let mut drop = false;
        if sid == 0 {
            let (nl, m) = uleb(input, k)?;
            let name = std::str::from_utf8(&input[m..m + nl as usize]).unwrap_or("");
            if name == "name" || name == "producers" || name.starts_with(".debug_") {
                drop = true;
            }
        }
        if drop {
            dropped += seg_end - i;
        } else {
            out.extend_from_slice(&input[i..seg_end]);
        }
        i = seg_end;
    }
    Ok((out, dropped))
}

fn uleb(b: &[u8], mut i: usize) -> Result<(u64, usize)> {
    let mut r = 0u64;
    let mut s = 0u32;
    loop {
        let x = *b.get(i).context("uleb out of bounds")?;
        i += 1;
        r |= u64::from(x & 0x7f) << s;
        if x & 0x80 == 0 {
            break;
        }
        s += 7;
    }
    Ok((r, i))
}

// -------------------------------------------------------------------- manifests

/// The wasip1 sysroot manifest shape runner.js consumes: {"files": [...]}.
pub fn write_files_manifest(root: &Path) -> Result<usize> {
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort();
    files.retain(|f| f != "manifest.json");
    let doc = serde_json::json!({ "files": files });
    fs::write(root.join("manifest.json"), serde_json::to_vec(&doc)?)?;
    Ok(files.len())
}

/// Bare-array rlib manifest (riscv / x86_64-std shape). `exts` like [".rlib"].
pub fn write_array_manifest(dir: &Path, out: &Path, exts: &[&str]) -> Result<usize> {
    let mut names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| exts.iter().any(|x| n.ends_with(x)))
        .collect();
    names.sort();
    fs::write(out, serde_json::to_vec(&names)?)?;
    Ok(names.len())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out)?;
        } else {
            out.push(
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}

/// assets-meta.json: byte sizes for the loading screen's progress bar.
pub fn write_assets_meta(rustc_dir: &Path) -> Result<()> {
    let rustc = rustc_dir.join("rustc.wasm");
    let bundle = rustc_dir.join("sysroot-wasip1.bundle");
    let doc = serde_json::json!({
        "rustcWasm": fs::metadata(&rustc)?.len(),
        "bundle": fs::metadata(&bundle)?.len(),
    });
    fs::write(rustc_dir.join("assets-meta.json"), serde_json::to_vec(&doc)?)?;
    Ok(())
}

// --------------------------------------------------------------------- fs utils

pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Parse simple `--flag value` / `--switch` CLI args into a map + positionals.
pub fn parse_args(spec_flags: &[&str]) -> (HashMap<String, String>, Vec<String>) {
    let mut flags = HashMap::new();
    let mut pos = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if let Some(name) = a.strip_prefix("--") {
            if spec_flags.contains(&name) {
                flags.insert(name.to_string(), it.next().unwrap_or_default());
            } else {
                flags.insert(name.to_string(), String::new());
            }
        } else {
            pos.push(a);
        }
    }
    (flags, pos)
}
