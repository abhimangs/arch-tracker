#!/usr/bin/env node
// Regenerates docs/screenshots/ from made-up data (arch-tracker demo), never from your own.
// Runs a throwaway tracker on a private D-Bus session and drives Chromium's headless shell.
//
//   cargo build && node scripts/screenshots.mjs
//
// Options (environment): ARCH_TRACKER_BIN (default target/debug/arch-tracker), CHROME (a headless
// Chromium; default: Playwright's chrome-headless-shell), OUT (default docs/screenshots).
import fs from "node:fs";
import path from "node:path";
import { openDemo, root, sleep } from "./demo-dashboard.mjs";

const out = path.resolve(process.env.OUT || path.join(root, "docs/screenshots"));
const { cdp, js, login, close, focusedAt } = await openDemo({ port: 7690 });
await login("today");

fs.mkdirSync(out, { recursive: true });
async function shot(name, hash, { height, theme = "dark", wait = 1800 } = {}) {
  await js(`document.documentElement.dataset.theme = "${theme}"; location.hash = "${hash}"`);
  await sleep(wait);
  await js("window.scrollTo(0, 0)");
  const h = Math.min(height || (await js("document.documentElement.scrollHeight")).result.value, 2000);
  // The window is as tall as the picture, so the sticky sidebar reaches the bottom.
  await cdp("Emulation.setDeviceMetricsOverride", { width: 1440, height: h, deviceScaleFactor: 1, mobile: false });
  await sleep(300);
  const clip = { x: 0, y: 0, width: 1440, height: h, scale: 1 };
  const r = await cdp("Page.captureScreenshot", { format: "png", clip, captureBeyondViewport: true });
  fs.writeFileSync(path.join(out, `${name}.png`), Buffer.from(r.data, "base64"));
  console.log("wrote", path.join(out, `${name}.png`));
}

await shot("today", "#/today", { height: 1500 });
await shot("timeline", "#/timeline", { height: 1000 });
await shot("projects", "#/projects/7", { height: 1000 });
await shot("trends", "#/trends/7", { height: 1100 });
await shot("browsers", "#/browser/brave-browser/7", { height: 1250 });
await shot("agent", "#/agent/claude/7", { height: 1200 });
await shot("codex", "#/agent/codex/7", { height: 1300 });
await shot("today-light", "#/today", { height: 1000, theme: "light" });
// Let the live timer run a little before showing Now.
await sleep(Math.max(0, 74_000 - (Date.now() - focusedAt)));
await shot("now", "#/now", { height: 1000 });

await close();
