//! Build-time Markdown → HTML for the long-form page text (About view, help
//! popover). pulldown-cmark runs on the HOST here — zero bytes of it reach the
//! wasm binary; main.rs include_str!s the rendered HTML from OUT_DIR.

use std::path::Path;

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    for (src, dst) in [("content/about.md", "about.html"), ("content/help.md", "help.html")] {
        println!("cargo:rerun-if-changed={src}");
        let md = std::fs::read_to_string(src)
            .unwrap_or_else(|e| panic!("reading {src}: {e}"));
        let mut html = String::with_capacity(md.len() * 2);
        pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(&md));
        std::fs::write(Path::new(&out_dir).join(dst), html)
            .unwrap_or_else(|e| panic!("writing {dst}: {e}"));
    }
}
