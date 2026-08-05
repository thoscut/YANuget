// Drive the real gallery in a real browser and capture what it looks like.
//
// Two kinds of output, both into <out>:
//   *.png            still screenshots, one per page
//   frames/<name>/   numbered frames plus timing.json, assembled into a GIF
//                    by build-gif.py
//
// The frames are stop-motion rather than a video capture: one frame per
// discrete step (a keystroke, a click, a page settling), each with its own
// duration. That reads better than a 30fps recording at a tenth of the size,
// and it never catches the page mid-repaint.
//
// Usage: node capture.mjs <base-url> <empty-base-url> <out-dir>

import { mkdir, writeFile, rm } from "node:fs/promises";
import path from "node:path";
import { chromium } from "playwright";

const [baseUrl, emptyUrl, outDir] = process.argv.slice(2);
if (!baseUrl || !emptyUrl || !outDir) {
  console.error("usage: capture.mjs <base-url> <empty-base-url> <out-dir>");
  process.exit(2);
}

const VIEWPORT = { width: 1180, height: 740 };

/** Collects frames for one animation, each with a display duration. */
class Reel {
  constructor(name) {
    this.dir = path.join(outDir, "frames", name);
    this.timings = [];
  }
  async start() {
    await rm(this.dir, { recursive: true, force: true });
    await mkdir(this.dir, { recursive: true });
  }
  /** Capture one frame, shown for `ms` milliseconds. */
  async shoot(page, ms) {
    const n = String(this.timings.length).padStart(4, "0");
    await page.screenshot({ path: path.join(this.dir, `${n}.png`) });
    this.timings.push(ms);
  }
  async finish() {
    await writeFile(
      path.join(this.dir, "timing.json"),
      JSON.stringify({ durations: this.timings }, null, 2),
    );
    console.log(`  ${path.basename(this.dir)}: ${this.timings.length} frames`);
  }
}

/** Type into a field one character at a time, capturing each keystroke. */
async function typeInto(page, selector, text, reel, msPerKey) {
  await page.click(selector);
  for (const ch of text) {
    await page.type(selector, ch, { delay: 0 });
    await reel.shoot(page, msPerKey);
  }
}

async function still(context, url, file, colorScheme = "dark") {
  const page = await context.newPage();
  await page.emulateMedia({ colorScheme });
  await page.goto(url, { waitUntil: "networkidle" });
  await page.screenshot({ path: path.join(outDir, file), fullPage: false });
  await page.close();
  console.log(`  ${file}`);
}

const browser = await chromium.launch();
const context = await browser.newContext({
  viewport: VIEWPORT,
  deviceScaleFactor: 1,
  colorScheme: "dark",
  // Deterministic rendering: no scrollbar overlay differences between runs.
  reducedMotion: "reduce",
});

await mkdir(outDir, { recursive: true });

console.log("stills:");
await still(context, `${baseUrl}/`, "gallery.png");
await still(context, `${baseUrl}/packages/contoso.build.tools`, "package.png");
await still(context, `${baseUrl}/stats`, "stats.png");
await still(context, `${baseUrl}/docs/`, "docs.png");
await still(context, `${emptyUrl}/`, "first-run.png");

console.log("reels:");

// ---------------------------------------------------------------- search →
// detail → copy. The story is "find a package, get the command", which is the
// only thing most visitors ever do here.
{
  const reel = new Reel("gallery");
  await reel.start();
  const page = await context.newPage();
  await page.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await reel.shoot(page, 1400);

  await typeInto(page, "#q", "logging", reel, 110);
  await reel.shoot(page, 500);

  await page.click('form.search button[type=submit]');
  await page.waitForLoadState("networkidle");
  await reel.shoot(page, 1500);

  await page.click(".card h2 a");
  await page.waitForLoadState("networkidle");
  await reel.shoot(page, 1800);

  // The copy button swaps its label to "Copied" for 1.2s — worth showing,
  // because it is the one interactive affordance on the page.
  const copy = page.locator(".install .snip .copy").first();
  await copy.scrollIntoViewIfNeeded();
  await copy.hover();
  await reel.shoot(page, 600);
  await copy.click();
  await page.waitForTimeout(120);
  await reel.shoot(page, 1600);

  await page.close();
  await reel.finish();
}

// ------------------------------------------------------- light/dark switch
// One frame each, held long enough to read. Cheap to produce and it answers
// "will this look wrong on my machine" without a paragraph of prose.
{
  const reel = new Reel("theme");
  await reel.start();
  const page = await context.newPage();
  for (let i = 0; i < 2; i++) {
    for (const scheme of ["dark", "light"]) {
      await page.emulateMedia({ colorScheme: scheme });
      await page.goto(`${baseUrl}/packages/acme.logging`, {
        waitUntil: "networkidle",
      });
      await reel.shoot(page, 1800);
    }
  }
  await page.close();
  await reel.finish();
}

await browser.close();
console.log("done");
