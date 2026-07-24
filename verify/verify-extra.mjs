// Supplementary checks the basic verify doesn't cover:
//  - switching the Examples <select> updates the shared source and runs (FizzBuzz)
//  - an unsupported feature (big zero-init array → memset) → link error → friendly Hint
//  - format! (not in the prelude) → compile error, with the line number remapped to the user's line
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PW_PATH);

const url = process.env.URL || 'http://127.0.0.1:8090/';
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1200, height: 720 } });
const errors = [];
page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
page.on('console', (m) => { if (m.type() === 'error') errors.push('console: ' + m.text()); });

await page.goto(url, { waitUntil: 'load' });
await page.waitForSelector('canvas.pg-editor', { timeout: 20000 });

// Warm up so rustc.wasm is loaded/cached in the page before the timed cases.
await page.click('.btn-primary');
await page.waitForFunction(() => /5050/.test(document.querySelector('#output')?.textContent || ''), { timeout: 300000 });

// 1) Switch to the FizzBuzz example (real DOM <select>) and run it.
await page.selectOption('select.btn', 'fizzbuzz');
await page.click('.btn-primary');
await page.waitForFunction(() => /FizzBuzz/.test(document.querySelector('#output')?.textContent || ''), { timeout: 120000 });
const fb = await page.textContent('#output');
const fizzOk = /FizzBuzz/.test(fb) && /(^|\n)Fizz(\n|$)/.test(fb) && /(^|\n)Buzz(\n|$)/.test(fb);

// 2) Std-mode win: a 256-byte zero-init array (memset) + indexing (bounds checks)
//    used to be a link error with a Hint — with real linking it just runs.
const linkErr = await page.evaluate(async () =>
  window.runRust('fn main() {\n    let buf = [0u8; 256];\n    let mut s = 0u64;\n    let mut i = 0usize;\n    while i < 256 { s += buf[i] as u64; i += 1; }\n    println!("{}", s);\n}', null));
const linkErrOk = linkErr && linkErr.ok === true && /(^|\n)0(\n|$)/.test(linkErr.stdout.trim() + "\n");

// 3) Compile error surfaces with the user's line number (std mode has no hidden
//    prelude, so rustc's own /work/prog.rs:2 maps 1:1; runner renames the path).
const compErr = await page.evaluate(async () =>
  window.runRust('fn main() {\n    let x: u32 = "nope";\n    let _ = x;\n}', null));
const compErrOk = compErr && compErr.ok === false &&
  (compErr.diagnostics || []).some((d) => /(program:2|line 2)/.test(d.rendered));

// 4) The Weblings brand opens the About (how-it-works) view and back.
await page.click('button.site-brand');
await page.waitForFunction(() => location.hash === '#about' &&
  !!document.querySelector('.about') && document.querySelector('.about').offsetParent !== null, null, { timeout: 10000 });
const aboutOk = /compile and execute Rust/.test(await page.textContent('.about-inner'));
await page.click('button.site-tab:has-text("Playground")');


console.log('--- fizzbuzz output ---\n' + fb);
console.log('--- link-error stdout ---\n' + (linkErr && linkErr.stdout));
console.log('--- compile-error diagnostics ---\n' +
  (compErr && (compErr.diagnostics || []).map((d) => d.rendered).join('\n')));
console.log('--- errors ---\n' + (errors.join('\n') || '(none)'));
await page.screenshot({ path: 'verify/screenshots/live-playground-fizzbuzz.png' });
await browser.close();

const pass = fizzOk && linkErrOk && compErrOk && aboutOk && errors.length === 0;
console.log(pass ? 'EXTRA VERIFY: PASS ✅ — examples + both error paths'
                 : `EXTRA VERIFY: FAIL ❌ (fizz=${fizzOk} link=${linkErrOk} comp=${compErrOk} about=${aboutOk} errs=${errors.length})`);
process.exit(pass ? 0 : 1);
