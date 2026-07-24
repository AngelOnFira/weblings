// Verify severity-colored output panel:
//  - a successful run STILL shows its warnings (amber span above plain stdout)
//  - a failed compile shows errors and warnings in DIFFERENT colors
//  - no console/page errors
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PW_PATH || 'playwright');

const url = process.env.URL || 'http://127.0.0.1:8090/';
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
const errors = [];
page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
page.on('console', (m) => { if (m.type() === 'error') errors.push('console: ' + m.text()); });

let pass = true;
const check = (cond, name) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}`); if (!cond) pass = false; };

const retype = async (src) => {
  await page.locator('.pg-editor').click();
  await page.keyboard.press('Control+A');
  await page.keyboard.type(src, { delay: 15 });
  await page.click('.btn-primary');
};

try {
  await page.goto(url);
  await page.waitForFunction(() => !document.getElementById('pg-loading'), null, { timeout: 300000 });

  // 1) Warning-only program: compiles + runs, warning must STILL be visible.
  await retype('fn main() { let unused = 1; println!("done"); }');
  await page.waitForFunction(
    () => (document.getElementById('output')?.textContent || '').includes('done'),
    null, { timeout: 120000 }
  );
  const warnOnly = await page.evaluate(() => {
    const out = document.getElementById('output');
    const warn = out.querySelector('span.warn');
    const tok = warn && warn.querySelector('.fg-yellow');
    return {
      warnText: warn ? warn.textContent : '',
      warnTok: tok ? tok.textContent : '',
      stdoutText: document.getElementById('stdout')?.textContent || '',
      hasErrSpan: !!out.querySelector('span.err'),
      headers: [...out.querySelectorAll('.pg-sec-head')].map((h) => h.textContent.trim()),
    };
  });
  check(/warning: unused variable/.test(warnOnly.warnText), 'success run shows its warning in a .warn span');
  check(/warning/.test(warnOnly.warnTok), 'the "warning" token itself is yellow (cargo-style)');
  check(warnOnly.stdoutText.trim() === 'done', `stdout lives in the Standard Output section ("${warnOnly.stdoutText.trim()}")`);
  check(!warnOnly.hasErrSpan, 'no error span on a successful run');
  check(warnOnly.headers.some((h) => /Standard Error/.test(h)) && warnOnly.headers.some((h) => /Standard Output/.test(h))
    && !warnOnly.headers.some((h) => /Errors/.test(h)),
    `sections on success: Standard Error + Standard Output, no Errors (${warnOnly.headers.join(' | ')})`);

  // Collapsing Standard Error hides the warning but keeps stdout in view.
  await page.locator('.pg-sec-head', { hasText: 'Standard Error' }).click();
  const collapsed = await page.evaluate(() => ({
    warnVisible: !!document.querySelector('#output span.warn')?.offsetParent,
    stdoutVisible: !!document.getElementById('stdout')?.offsetParent,
  }));
  check(!collapsed.warnVisible && collapsed.stdoutVisible, 'collapsing Standard Error hides warnings, stdout stays');
  await page.locator('.pg-sec-head', { hasText: 'Standard Error' }).click();
  const reopened = await page.evaluate(() => !!document.querySelector('#output span.warn')?.offsetParent);
  check(reopened, 'expanding Standard Error brings the warning back');

  // 2) Warning + error together (unused_parens is an early lint, so it survives
  //    the type error): the two severities must render in different colors.
  await retype('fn main() { let x: u32 = "nope"; let _ = (x); }');
  await page.waitForFunction(
    () => !!document.querySelector('#output span.err'),
    null, { timeout: 120000 }
  );
  const mixed = await page.evaluate(() => {
    const out = document.getElementById('output');
    const err = out.querySelector('span.err');
    const warn = out.querySelector('span.warn');
    const errTok = err && err.querySelector('.fg-red');
    const warnTok = warn && warn.querySelector('.fg-yellow');
    return {
      errText: err ? err.textContent : '',
      warnText: warn ? warn.textContent : '',
      errColor: errTok ? getComputedStyle(errTok).color : '',
      warnColor: warnTok ? getComputedStyle(warnTok).color : '',
      // cargo-likeness: the gutter/arrows are blue, and the source-code lines
      // inside an error block stay uncolored (the .ansi class neutralizes the
      // old block-level red).
      hasBlue: !!(err && err.querySelector('.fg-blue')),
      errBase: err ? getComputedStyle(err).color : '',
    };
  });
  check(/error\[E0308\]/.test(mixed.errText), 'compile error rendered in an .err span');
  check(/warning: unnecessary parentheses/.test(mixed.warnText), 'warning rendered in a .warn span alongside the error');
  check(mixed.errColor && mixed.warnColor && mixed.errColor !== mixed.warnColor,
    `error and warning tokens have different colors (${mixed.errColor} vs ${mixed.warnColor})`);
  check(mixed.hasBlue, 'gutter/arrow spans are blue like cargo');
  check(mixed.errBase === 'rgb(51, 51, 51)', `source lines in diagnostics stay plain (${mixed.errBase})`);
  const failHeaders = await page.evaluate(() =>
    [...document.querySelectorAll('#output .pg-sec-head')].map((h) => h.textContent.trim()));
  const failErrBody = await page.evaluate(() =>
    document.querySelector('#output .pg-sec-body.err')?.textContent || '');
  check(failHeaders.some((h) => /Errors/.test(h)) && /Compilation failed/.test(failErrBody),
    `compile failure shows an Errors section ("${failErrBody}")`);

  // 3) Real exit codes: std::process::exit(42) → "Exited with status 42".
  await retype('fn main() { println!("bye"); std::process::exit(42); }');
  await page.waitForFunction(
    () => /Exited with status 42/.test(document.getElementById('output')?.textContent || ''),
    null, { timeout: 120000 }
  );
  const exitStdout = await page.textContent('#stdout');
  check(exitStdout.trim() === 'bye', `stdout preserved alongside the exit status ("${exitStdout.trim()}")`);

  check(errors.length === 0, `no console/page errors (${errors.length})`);
  if (errors.length) console.log(errors.join('\n'));
} catch (e) {
  console.log('EXCEPTION:', e.message);
  pass = false;
}

await page.screenshot({ path: 'verify/screenshots/output-colors.png' });
await browser.close();
console.log(pass ? '\nOUTPUT-COLORS VERIFY: PASS ✅' : '\nOUTPUT-COLORS VERIFY: FAIL ❌');
process.exit(pass ? 0 : 1);
