// Photographs the /previews render sheet into public/plugins/<name>.png —
// one PNG per plugin that describes its screen as `preview` data. Run after
// `astro build`, commit what it writes. Needs playwright; if it is not a
// dependency here, point PLAYWRIGHT_DIR at any node_modules that has it.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

let chromium;
try {
  ({ chromium } = await import("playwright"));
} catch {
  const dir = process.env.PLAYWRIGHT_DIR;
  if (!dir) {
    console.error("playwright is not installed — `pnpm add -D playwright`, or set PLAYWRIGHT_DIR to a node_modules that has it");
    process.exit(1);
  }
  const { createRequire } = await import("node:module");
  ({ chromium } = createRequire(path.join(dir, "_.js"))("playwright"));
}

const dist = fileURLToPath(new URL("../dist/", import.meta.url));
if (!existsSync(path.join(dist, "previews", "index.html"))) {
  console.error("dist/previews is missing — run `astro build` first");
  process.exit(1);
}

const MIME = {
  ".html": "text/html",
  ".css": "text/css",
  ".js": "text/javascript",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".webp": "image/webp",
  ".jpg": "image/jpeg",
  ".ico": "image/x-icon",
};
const srv = createServer(async (req, res) => {
  const p = decodeURIComponent(new URL(req.url, "http://x").pathname);
  const file = p.endsWith("/") || !path.extname(p) ? path.join(dist, p, "index.html") : path.join(dist, p);
  try {
    const data = await readFile(file);
    res.setHeader("content-type", MIME[path.extname(file)] ?? "application/octet-stream");
    res.end(data);
  } catch {
    res.statusCode = 404;
    res.end();
  }
});
await new Promise((r) => srv.listen(0, r));

const out = fileURLToPath(new URL("../public/plugins/", import.meta.url));
mkdirSync(out, { recursive: true });

const browser = await chromium.launch();
const page = await browser.newPage({ deviceScaleFactor: 2 });
await page.goto(`http://localhost:${srv.address().port}/previews`, { waitUntil: "networkidle" });
await page.evaluate(() => document.fonts.ready);

for (const el of await page.$$("[data-shot]")) {
  const name = await el.getAttribute("data-shot");
  await el.screenshot({ path: path.join(out, `${name}.png`) });
  console.log(`plugins/${name}.png`);
}

await browser.close();
srv.close();
