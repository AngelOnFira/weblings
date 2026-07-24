// Verify browser-cache persistence for both front-ends:
//  - Playground: an edit is saved to localStorage (playground_src) and survives a reload.
//    Also checks the programmatic Examples path persists.
//  - Trainer: a per-exercise draft (rustlings_src:<name>) is saved, survives a reload, and
//    "Clear all saved progress" removes it. Also checks "Solution" persists.
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PW_PATH || 'playwright');
const base = process.env.URL || 'http://127.0.0.1:8090/';

const browser = await chromium.launch({ headless: true });
let failed = 0;
const ok = (c, m) => { console.log((c ? 'PASS  ' : 'FAIL  ') + m); if (!c) failed++; };

async function typeIntoCanvas(page, sel, text) {
  const c = await page.$(sel);
  const box = await c.boundingBox();
  await page.mouse.click(box.x + 60, box.y + 20); // focus + place caret near the top-left
  await page.keyboard.type(text, { delay: 10 });
}

// ---------------- Playground ----------------
{
  const page = await browser.newPage({ viewport: { width: 1200, height: 720 } });
  await page.goto(base, { waitUntil: 'load' });
  await page.evaluate(() => localStorage.clear());
  await page.reload({ waitUntil: 'load' });
  await page.waitForSelector('canvas.pg-editor');
  await page.waitForFunction(() => { const c = document.querySelector('canvas.pg-editor'); return c && c.clientWidth > 100 && c.clientHeight > 100; });

  await typeIntoCanvas(page, 'canvas.pg-editor', '//MARKER_PG\n');
  const saved = await page.waitForFunction(
    () => { const v = localStorage.getItem('playground_src'); return v && v.includes('MARKER_PG') ? v : false; },
    { timeout: 6000 }).then(() => true).catch(() => false);
  ok(saved, 'playground: typed edit autosaved to localStorage');

  await page.reload({ waitUntil: 'load' });
  const restored = await page.evaluate(() => localStorage.getItem('playground_src') || '');
  ok(restored.includes('MARKER_PG'), 'playground: draft restored after reload');

  // Examples path persists programmatically.
  await page.selectOption('select.btn', 'fizzbuzz');
  const ex = await page.waitForFunction(
    () => { const v = localStorage.getItem('playground_src'); return v && v.includes('FizzBuzz') ? v : false; },
    { timeout: 3000 }).then(() => true).catch(() => false);
  ok(ex, 'playground: Examples selection persisted');

  await page.close();
}

// ---------------- Trainer ----------------
{
  const page = await browser.newPage({ viewport: { width: 1200, height: 720 } });
  await page.goto(base + '#rustlings', { waitUntil: 'load' });
  await page.evaluate(() => localStorage.clear());
  await page.reload({ waitUntil: 'load' });
  await page.waitForSelector('canvas.tr-editor');
  await page.waitForFunction(() => document.querySelectorAll('.tr-item').length > 10, { timeout: 20000 });
  await page.waitForFunction(() => { const c = document.querySelector('canvas.tr-editor'); return c && c.clientWidth > 100 && c.clientHeight > 100; });

  // Reveal the current exercise's solution — a programmatic buffer change we persist explicitly.
  await page.getByText('Solution', { exact: true }).click();
  const draftKey = await page.waitForFunction(
    () => { const k = Object.keys(localStorage).find(k => k.startsWith('rustlings_src:')); return k || false; },
    { timeout: 8000 }).then(h => h.jsonValue()).catch(() => null);
  ok(!!draftKey, 'trainer: Solution saved a per-exercise draft (' + draftKey + ')');

  await page.reload({ waitUntil: 'load' });
  const persisted = await page.waitForFunction(
    () => Object.keys(localStorage).some(k => k.startsWith('rustlings_src:')), { timeout: 20000 })
    .then(() => true).catch(() => false);
  ok(persisted, 'trainer: draft persists across reload');

  // Type an extra marker into the editor → autosave path.
  await typeIntoCanvas(page, 'canvas.tr-editor', '//MARKER_TR\n');
  const typed = await page.waitForFunction(
    () => Object.keys(localStorage).some(k => k.startsWith('rustlings_src:') && localStorage.getItem(k).includes('MARKER_TR')),
    { timeout: 6000 }).then(() => true).catch(() => false);
  ok(typed, 'trainer: typed edit autosaved to the exercise draft');

  await page.getByText('Clear all saved progress', { exact: true }).click();
  // Drafts + schema are wiped; the done-set resets. (Re-selecting exercise 0 re-checks the trivial
  // first exercise, which legitimately compiles and may re-appear as done — same as a fresh load.)
  const wiped = await page.waitForFunction(
    () => {
      const drafts = Object.keys(localStorage).filter(k => k.startsWith('rustlings_src:'));
      const done = localStorage.getItem('rustlings_done');
      const doneReset = done === null || JSON.parse(done).every(n => n === 'intro1');
      return drafts.length === 0 && localStorage.getItem('rustlings_schema') === null && doneReset;
    },
    { timeout: 4000 }).then(() => true).catch(() => false);
  ok(wiped, 'trainer: Clear all removed drafts + schema + reset progress');
  await page.close();
}

await browser.close();
console.log(failed ? `PERSIST VERIFY: FAIL (${failed})` : 'PERSIST VERIFY: PASS ✅');
process.exit(failed ? 1 : 0);
