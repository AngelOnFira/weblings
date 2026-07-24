//! ANSI SGR → HTML for the output pane: rustc's `--json=diagnostic-rendered-ansi`
//! carries the exact colors cargo prints in a terminal (bold-red "error", bold
//! "message", bright-blue gutter/arrows, yellow warnings). This converts those
//! escape codes to `<span class="b fg-red">…</span>` runs; the classes are
//! styled in index.html with a palette tuned for the light pane background.
//!
//! Only what rustc emits is interpreted: reset (0), bold (1/22), the 16-color
//! foregrounds (30-37 / 90-97 / 38;5;N<16), and default fg (39). Everything
//! else (backgrounds, underline, truecolor) is consumed and ignored.

/// Class names for the 8 base hues; bright variants (9x) map to the same hue —
/// on a light background there is no readable "bright" distinction.
const COLOR_CLASSES: [&str; 8] = [
    "fg-black", "fg-red", "fg-green", "fg-yellow",
    "fg-blue", "fg-magenta", "fg-cyan", "fg-white",
];

pub fn has_ansi(s: &str) -> bool {
    s.contains('\u{1b}')
}

/// Plain text with every escape sequence removed (downloads, fallbacks).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' && it.peek() == Some(&'[') {
            it.next();
            while let Some(&n) = it.peek() {
                it.next();
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// One maximal stretch of identically-styled text.
pub struct Run {
    pub bold: bool,
    /// Index into the 8-hue palette (`COLOR_CLASSES` order); None = default fg.
    pub color: Option<usize>,
    pub text: String,
}

/// Decode SGR escapes into styled runs — the shared state machine behind the
/// HTML output pane and the egui tooltip rendering.
pub fn parse_runs(s: &str) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut buf = String::new();
    let mut bold = false;
    let mut color: Option<usize> = None;
    let flush = |buf: &mut String, bold: bool, color: Option<usize>, runs: &mut Vec<Run>| {
        if !buf.is_empty() {
            runs.push(Run { bold, color, text: std::mem::take(buf) });
        }
    };
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' || it.peek() != Some(&'[') {
            buf.push(c);
            continue;
        }
        it.next();
        let mut params = String::new();
        let mut term = ' ';
        while let Some(&n) = it.peek() {
            it.next();
            if n.is_ascii_alphabetic() {
                term = n;
                break;
            }
            params.push(n);
        }
        if term != 'm' {
            continue; // non-SGR sequence: dropped
        }
        flush(&mut buf, bold, color, &mut runs);
        let nums: Vec<u32> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        let mut j = 0;
        while j < nums.len() {
            match nums[j] {
                0 => {
                    bold = false;
                    color = None;
                }
                1 => bold = true,
                22 => bold = false,
                30..=37 => color = Some(nums[j] as usize - 30),
                90..=97 => color = Some(nums[j] as usize - 90),
                39 => color = None,
                // Extended fg: 38;5;N (use N's base hue when in the 16-color
                // range) or 38;2;r;g;b (ignored) — consume the arguments so
                // they aren't misread as standalone codes.
                38 => match nums.get(j + 1) {
                    Some(5) => {
                        if let Some(&n) = nums.get(j + 2) {
                            if n < 16 {
                                color = Some(n as usize % 8);
                            }
                        }
                        j += 2;
                    }
                    Some(2) => j += 4,
                    _ => {}
                },
                48 => match nums.get(j + 1) {
                    Some(5) => j += 2,
                    Some(2) => j += 4,
                    _ => {}
                },
                _ => {}
            }
            j += 1;
        }
    }
    flush(&mut buf, bold, color, &mut runs);
    runs
}

/// HTML with `&<>` escaped and SGR runs wrapped in class-carrying spans —
/// safe to assign via `inner_html`.
pub fn ansi_to_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 64);
    for run in parse_runs(s) {
        if run.bold || run.color.is_some() {
            out.push_str("<span class=\"");
            if run.bold {
                out.push('b');
            }
            if let Some(c) = run.color {
                if run.bold {
                    out.push(' ');
                }
                out.push_str(COLOR_CLASSES[c]);
            }
            out.push_str("\">");
            escape_into(&mut out, &run.text);
            out.push_str("</span>");
        } else {
            escape_into(&mut out, &run.text);
        }
    }
    out
}
