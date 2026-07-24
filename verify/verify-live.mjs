// Verify the full live playground end-to-end:
//  - the pure-Rust egui editor canvas mounts with a non-zero size
//  - click Run → wrap in prelude → compile (cranelift rustc.wasm) → link → run → output
//  - the default program prints "sum 1..=100 = 5050"
//  - a "compiled in ... · executed in ..." timing status line appears
//  - no console/page errors
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PW_PATH || 'playwright');

const url = process.env.URL || 'http://127.0.0.1:8090/';
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1200, height: 720 } });
const errors = [];
page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
page.on('console', (m) => { if (m.type() === 'error') errors.push('console: ' + m.text()); });

await page.goto(url, { waitUntil: 'load' });

// 1) egui editor canvas mounts with a real size
await page.waitForSelector('canvas.pg-editor', { timeout: 20000 });
await page.waitForFunction(() => {
  const c = document.querySelector('canvas.pg-editor');
  return c && c.clientWidth > 100 && c.clientHeight > 100;
}, { timeout: 20000 });
const canvas = await page.evaluate(() => {
  const c = document.querySelector('canvas.pg-editor');
  return { w: c.clientWidth, h: c.clientHeight, bw: c.width, bh: c.height };
});
console.log('editor canvas:', JSON.stringify(canvas));
await page.waitForTimeout(1200); // let egui paint the code + gutter before we screenshot

// 2) Run the default program
await page.waitForSelector('.btn-primary', { timeout: 20000 });
await page.click('.btn-primary');
let ok = false;
try {
  await page.waitForFunction(
    () => /sum 1\.\.=100 = 5050/.test(document.querySelector('#output')?.textContent || ''),
    { timeout: 300000 },
  );
  ok = true;
} catch {}

const out = await page.textContent('#output');
// The click's run and the on-mount auto-run overlap; wait for whichever run
// survives to post its timing line instead of sampling "Working..." mid-flight.
let timingOk = false;
try {
  await page.waitForFunction(
    () => /compiled in \d+ ms(, linked in \d+ ms)?, executed in \d+ ms/
      .test(document.querySelector('#status')?.textContent || ''),
    { timeout: 120000 },
  );
  timingOk = true;
} catch {}
const status = await page.textContent('#status');
console.log('--- #output ---\n' + out);
console.log('--- #status ---\n' + status);
console.log('--- errors ---\n' + (errors.join('\n') || '(none)'));
await page.screenshot({ path: 'verify/screenshots/live-playground.png' });
await browser.close();

const pass = ok && timingOk && canvas.w > 100 && errors.length === 0;
console.log(pass ? 'VERIFY: PASS ✅ — egui editor + compiled & executed Rust + timing'
                 : `VERIFY: FAIL ❌ (ran=${ok} timing=${timingOk} canvas=${canvas.w > 100} errors=${errors.length})`);
process.exit(pass ? 0 : 1);
