//! gen_exercises <rustlings_checkout> <out.json> — bundle a Rustlings checkout
//! into the single exercises.json the browser Rustlings view consumes.
//! (Rust port of tools/gen-rustlings-exercises.py.)
//!
//! Reads <rustlings>/rustlings-macros/info.toml + exercises/<dir>/<name>.rs
//! (+ solutions) and emits { meta, exercises: [{ order, name, dir, test,
//! strict_clippy, hint, exercise, solution }] }.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next().context("usage: gen_exercises <rustlings_checkout> <out.json>")?,
    );
    let out_path = args.next().context("usage: gen_exercises <rustlings_checkout> <out.json>")?;

    let info_path = root.join("rustlings-macros/info.toml");
    let info: toml::Value = toml::from_str(
        &std::fs::read_to_string(&info_path)
            .with_context(|| format!("reading {}", info_path.display()))?,
    )?;

    let list = info
        .get("exercises")
        .and_then(|v| v.as_array())
        .context("info.toml: no [[exercises]]")?;

    let mut exercises = Vec::with_capacity(list.len());
    for (i, ex) in list.iter().enumerate() {
        let name = ex.get("name").and_then(|v| v.as_str()).context("exercise without name")?;
        let dir = ex.get("dir").and_then(|v| v.as_str()).unwrap_or_default();
        let ex_file = root.join("exercises").join(dir).join(format!("{name}.rs"));
        let sol_file = root.join("solutions").join(dir).join(format!("{name}.rs"));
        exercises.push(json!({
            "order": i,
            "name": name,
            "dir": dir,
            "test": ex.get("test").and_then(|v| v.as_bool()).unwrap_or(true),
            "strict_clippy": ex.get("strict_clippy").and_then(|v| v.as_bool()).unwrap_or(false),
            "hint": ex.get("hint").and_then(|v| v.as_str()).unwrap_or("").trim(),
            "exercise": std::fs::read_to_string(&ex_file)
                .with_context(|| format!("reading {}", ex_file.display()))?,
            "solution": std::fs::read_to_string(&sol_file).ok(),
        }));
    }

    let compile_only = exercises
        .iter()
        .filter(|e| e["test"] == serde_json::Value::Bool(false))
        .count();
    let doc = json!({
        "meta": {
            "source": "rust-lang/rustlings",
            "edition": "2024",
            "count": exercises.len(),
            "welcome_message": info.get("welcome_message").and_then(|v| v.as_str()).unwrap_or("").trim(),
        },
        "exercises": exercises,
    });
    std::fs::write(&out_path, serde_json::to_vec(&doc)?)?;
    println!(
        "wrote {} exercises ({} compile-only, {} test) to {}",
        doc["exercises"].as_array().unwrap().len(),
        compile_only,
        doc["exercises"].as_array().unwrap().len() - compile_only,
        out_path
    );
    Ok(())
}
