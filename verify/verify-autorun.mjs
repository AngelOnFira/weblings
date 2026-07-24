// Verify live auto-run + cancellation:
//  - with auto-run enabled, typing alone (zero Run clicks) produces output
//  - every keystroke submits a compile; superseded ones are CANCELLED mid-flight
//    (runner.js terminates the busy worker — observed via its console log)
//  - the final output reflects exactly the final buffer (newest-wins coalescing)
//  - no console/page errors
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PW_PATH || 'playwright');

const url = process.env.URL || 'http://127.0.0.1:8090/';
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
const errors = [];
let cancels = 0;
page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
page.on('console', (m) => {
  if (m.type() === 'error') errors.push('console: ' + m.text());
  if (m.text().includes('cancelled in-flight')) cancels++;
});

let pass = true;
const check = (cond, name) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}`); if (!cond) pass = false; };

try {
  await page.goto(url);
  await page.waitForFunction(() => !document.getElementById('pg-loading'), null, { timeout: 300000 });

  await page.locator('.pg-autorun input').check();

  // Type a fresh program over the default one, keystroke by keystroke. Each
  // keypress submits a compile; while one is physically running, the next
  // keypress terminates it (a "cancelled in-flight" log); while the worker
  // pool re-warms, newer keypresses coalesce (newest wins, no log).
  await page.locator('.pg-editor').click();
  await page.keyboard.press('Control+A');
  await page.keyboard.type('fn main() { println!("live {}", 6*7); }', { delay: 90 });

  await page.waitForFunction(
    () => (document.getElementById('output')?.textContent || '').includes('live 42'),
    null, { timeout: 60000 }
  );
  const status = await page.textContent('#status');
  const out = await page.textContent('#stdout');
  check(true, 'auto-run produced output with zero Run clicks');
  check(cancels >= 1, `superseded compiles were cancelled mid-flight (${cancels})`);
  check(/compiled in \d+ ms(, linked in \d+ ms)?, executed in \d+ ms/.test(status), `timing status intact: "${status}"`);
  check(out.trim() === 'live 42', `stdout section is exactly the final program's ("${out.trim()}")`);
  check(errors.length === 0, `no console/page errors (${errors.length})`);
  if (errors.length) console.log(errors.join('\n'));
} catch (e) {
  console.log('EXCEPTION:', e.message);
  pass = false;
}

await browser.close();
console.log(pass ? '\nAUTORUN VERIFY: PASS ✅' : '\nAUTORUN VERIFY: FAIL ❌');
process.exit(pass ? 0 : 1);
