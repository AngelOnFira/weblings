// Verify the Rustlings view end-to-end (client-side check + real test runs):
//  - the exercise list loads (~94 items) from bundled rustlings v6.4.0
//  - selecting a broken compile-mode exercise (variables1) shows a real compile error
//  - clicking "Solution" makes it type-check clean → "done" (proves the std sysroot works: println!)
//  - a std-heavy exercise (vecs1) type-checks its solution (proves Vec/std metadata resolves)
//  - a test-mode exercise (tests1) solution compiles but is labeled "tests not run" (honest)
//  - no console/page errors
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PW_PATH);

const url = (process.env.URL || 'http://127.0.0.1:8090/') + '#rustlings';
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
const errors = [];
page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
page.on('console', (m) => { if (m.type() === 'error') errors.push('console: ' + m.text()); });

const selectExercise = (label) =>
  page.locator('.tr-item', { hasText: label }).first().click();
const waitStat = (reSrc, timeout) =>
  page.waitForFunction(
    (rs) => { const el = document.querySelector('.tr-stat'); return el && new RegExp(rs).test(el.textContent || ''); },
    reSrc, { timeout });
const stat = () => page.textContent('.tr-stat');
const diag = () => page.textContent('.tr-diag');

let pass = true;
const check = (cond, name) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}`); if (!cond) pass = false; };

try {
  await page.goto(url, { waitUntil: 'load' });
  // 1) exercise list loads
  await page.waitForFunction(() => document.querySelectorAll('.tr-item').length >= 90, null, { timeout: 30000 });
  const n = await page.evaluate(() => document.querySelectorAll('.tr-item').length);
  check(n >= 90, `exercise list loaded (${n} items)`);

  // 2) broken compile-mode exercise -> error (first check also downloads rustc.wasm + 85MB sysroot)
  await selectExercise('01_variables/variables1');
  await waitStat('does not compile|error', 120000);
  check(/does not compile/i.test(await stat()), 'variables1 shows a compile error');
  check(/error\[|cannot find/i.test(await diag()), 'diagnostics show the rustc error');

  // 3) Solution -> type-checks clean -> done (println! => proves std sysroot works)
  await page.getByRole('button', { name: 'Solution' }).click();
  await waitStat('done|verified', 60000);
  check(/done|verified|✓/.test(await stat()), 'variables1 solution -> done (std println! resolves)');

  // 4) std-heavy exercise: solution must type-check (Vec/std metadata)
  await selectExercise('05_vecs/vecs1');
  await page.getByRole('button', { name: 'Solution' }).click();
  await waitStat('done|verified|compiles|tests? passed|tests failed', 120000);
  check(!/does not compile/i.test(await stat()), 'vecs1 solution type-checks (Vec/std resolves)');

  // 5) test-mode exercise: the solution's tests RUN and pass (Phase B: real
  //    --test binaries, linked in-browser and executed under the WASI shim).
  await selectExercise('17_tests/tests1');
  await page.getByRole('button', { name: 'Solution' }).click();
  await waitStat('tests? passed|tests failed|does not compile', 120000);
  check(/\d+ tests? passed/.test(await stat()), 'tests1 solution -> tests RUN and pass');

  // 6) failing assert: panic=abort fail path returns ok:false with the assertion
  const failRes = await page.evaluate(async () =>
    window.runTests('fn main(){}\n#[cfg(test)]\nmod t { #[test] fn f() { assert_eq!(1, 2); } }', null));
  check(failRes && failRes.ok === false && /assertion|panicked/.test(failRes.output),
    'failing assert -> ok:false with the panic message');

  // 7) threads are honestly unsupported on WASI
  const thrRes = await page.evaluate(async () =>
    window.runTests('fn main(){}\n#[cfg(test)]\nmod t { #[test] fn f() { std::thread::spawn(|| {}).join().unwrap(); } }', null));
  check(thrRes && thrRes.unsupported === 'threads',
    'thread-spawn test -> unsupported:"threads"');

  check(errors.length === 0, `no console/page errors (${errors.length})`);
} catch (e) {
  console.log('EXCEPTION:', e.message);
  pass = false;
}

console.log('--- final status ---', (await stat().catch(() => '')) || '(none)');
if (errors.length) console.log('--- errors ---\n' + errors.join('\n'));
await page.screenshot({ path: 'verify/screenshots/rustlings.png' });
await browser.close();
console.log(pass ? '\nRUSTLINGS VERIFY: PASS ✅' : '\nRUSTLINGS VERIFY: FAIL ❌');
process.exit(pass ? 0 : 1);
