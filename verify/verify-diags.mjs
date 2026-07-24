// Verify in-editor diagnostics (rustc JSON → egui painter):
//  - playground, auto-run OFF: typing broken code produces error markers
//    (asserted via the window.__weblings_diags hook — the squiggles themselves
//    are canvas paint), fixing the code clears them
//  - trainer: selecting a broken exercise produces error markers
//  - screenshots capture the visual (squiggle + gutter dot) for review
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PW_PATH || 'playwright');

const url = process.env.URL || 'http://127.0.0.1:8090/';
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
const errors = [];
page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
page.on('console', (m) => { if (m.type() === 'error') errors.push('console: ' + m.text()); });

let pass = true;
const check = (cond, name) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}`); if (!cond) pass = false; };
const diagCounts = () => page.evaluate(() => window.__weblings_diags || null);
const waitDiags = (pred, timeout = 60000) =>
  page.waitForFunction(
    (src) => new Function('d', `return ${src}`)(window.__weblings_diags),
    pred, { timeout }
  );

try {
  await page.goto(url);
  await page.waitForFunction(() => !document.getElementById('pg-loading'), null, { timeout: 300000 });

  // 1) playground: broken code -> error markers (auto-run off by default)
  await page.locator('.pg-editor').click();
  await page.keyboard.press('Control+A');
  await page.keyboard.type('fn main() { let x: i32 = "s"; }', { delay: 10 });
  await waitDiags('d && d.errors > 0');
  const broken = await diagCounts();
  check(broken.errors > 0, `broken code -> ${broken.errors} error(s) in the editor`);
  await page.mouse.move(300, 130); // hover the bad line for the tooltip shot
  await page.waitForTimeout(600);
  await page.screenshot({ path: 'verify/screenshots/diags-playground.png' });

  // 2) fixing the code clears the error markers
  await page.locator('.pg-editor').click();
  await page.keyboard.press('Control+A');
  await page.keyboard.type('fn main() { println!("ok"); }', { delay: 10 });
  await waitDiags('d && d.errors === 0');
  check(true, 'fixed code -> error markers cleared');

  // 3) trainer: broken exercise -> error markers
  await page.goto(url + '#rustlings');
  await page.waitForFunction(() => document.querySelectorAll('.tr-item').length >= 90, null, { timeout: 30000 });
  await page.locator('.tr-item', { hasText: '01_variables/variables1' }).first().click();
  await waitDiags('d && d.errors > 0', 120000);
  const tr = await diagCounts();
  check(tr.errors > 0, `variables1 -> ${tr.errors} error(s) in the trainer editor`);
  await page.waitForTimeout(300);
  await page.screenshot({ path: 'verify/screenshots/diags-trainer.png' });

  check(errors.length === 0, `no console/page errors (${errors.length})`);
  if (errors.length) console.log(errors.join('\n'));
} catch (e) {
  console.log('EXCEPTION:', e.message);
  pass = false;
}

await browser.close();
console.log(pass ? '\nDIAGS VERIFY: PASS ✅' : '\nDIAGS VERIFY: FAIL ❌');
process.exit(pass ? 0 : 1);
