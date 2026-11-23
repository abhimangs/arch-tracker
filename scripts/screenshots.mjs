#!/usr/bin/env node
// Regenerates docs/screenshots/ from made-up data (arch-tracker demo), never from your own.
// Runs a throwaway tracker on a private D-Bus session and drives Chromium's headless shell.
//
//   cargo build && node scripts/screenshots.mjs
//
// Options (environment): ARCH_TRACKER_BIN (default target/debug/arch-tracker), CHROME (a headless
// Chromium; default: Playwright's chrome-headless-shell), OUT (default docs/screenshots).
import { spawn, execFileSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), "..");
const bin = path.resolve(process.env.ARCH_TRACKER_BIN || path.join(root, "target/debug/arch-tracker"));
const out = path.resolve(process.env.OUT || path.join(root, "docs/screenshots"));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function findChrome() {
  if (process.env.CHROME) return process.env.CHROME;
  const base = path.join(os.homedir(), ".cache/ms-playwright");
  const dirs = fs.existsSync(base) ? fs.readdirSync(base).filter((d) => d.startsWith("chromium_headless_shell")).sort() : [];
  for (const d of dirs.reverse()) {
    const p = path.join(base, d, "chrome-headless-shell-linux64/chrome-headless-shell");
    if (fs.existsSync(p)) return p;
  }
  throw new Error("set CHROME to a headless Chromium (npx playwright install chromium-headless-shell)");
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "at-shots-"));
// Unix socket paths are short (108 bytes): keep the runtime folder near the root.
const userRun = `/run/user/${os.userInfo().uid}`;
const run = fs.mkdtempSync(fs.existsSync(userRun) ? path.join(userRun, "at-") : path.join("/tmp", "at-run-"));
const env = {
  ...process.env,
  HOME: path.join(tmp, "home"),
  XDG_CONFIG_HOME: path.join(tmp, "config"),
  XDG_DATA_HOME: path.join(tmp, "data"),
  XDG_RUNTIME_DIR: run,
  // The desktop's own Wayland socket, by absolute path, so idle detection works; else none.
  WAYLAND_DISPLAY: (() => {
    const w = process.env.WAYLAND_DISPLAY;
    const dir = process.env.XDG_RUNTIME_DIR;
    if (!w || !dir) return "none";
    const p = path.isAbsolute(w) ? w : path.join(dir, w);
    return fs.existsSync(p) ? p : "none";
  })(),
};
for (const d of [env.HOME, path.join(env.XDG_CONFIG_HOME, "arch-tracker"), path.join(env.XDG_DATA_HOME, "arch-tracker"), env.XDG_RUNTIME_DIR]) fs.mkdirSync(d, { recursive: true, mode: 0o700 });
const port = 7690;
fs.writeFileSync(
  path.join(env.XDG_CONFIG_HOME, "arch-tracker/config.toml"),
  `[dashboard]
port = ${port}
theme = "dark"
palette = "rose-pine"
dark_flavor = "main"
accent = "iris"
tray = false

[insights]
daily_goal_mins = 420

# Live agent scanning would find real processes on this machine: match nothing.
[[agents.tools]]
id = "claude"
name = "Claude Code"
matches = ["demo-agent-that-does-not-exist"]
`,
);
execFileSync(bin, ["demo", "--db", path.join(env.XDG_DATA_HOME, "arch-tracker/tracker.db"), "--days", "7"], { env, stdio: "inherit" });

const busFile = path.join(tmp, "bus");
const daemon = spawn("dbus-run-session", ["--", "sh", "-c", `echo "$DBUS_SESSION_BUS_ADDRESS" > '${busFile}'; exec '${bin}' daemon`], { env, stdio: "ignore", detached: true });
const control = path.join(env.XDG_RUNTIME_DIR, "arch-tracker/control.sock");
for (let i = 0; i < 100 && !fs.existsSync(control); i++) await sleep(100);

const ask = (msg) =>
  new Promise((resolve, reject) => {
    const s = net.createConnection(control, () => s.write(JSON.stringify(msg) + "\n"));
    let buf = "";
    s.on("data", (d) => {
      buf += d;
      if (buf.includes("\n")) {
        s.end();
        resolve(JSON.parse(buf));
      }
    });
    s.on("error", reject);
  });

// What KWin would report: Zed in front, the docs beside it.
const snapshot = JSON.stringify({
  active: "w1",
  desktop: "d1",
  screens: [{ x: 0, y: 0, w: 2560, h: 1440 }],
  windows: [
    { id: "w2", pid: 0, cls: "brave-browser", df: "brave-browser", cap: "tokio::sync::watch - Rust - Brave", x: 1500, y: 0, w: 1060, h: 1440, normal: true },
    { id: "w1", pid: 0, cls: "dev.zed.Zed", df: "dev.zed.Zed", cap: "atlas — src/report.rs", x: 0, y: 0, w: 1500, h: 1440, normal: true },
  ],
});
const bus = fs.readFileSync(busFile, "utf8").trim();
execFileSync("gdbus", ["call", "--session", "--dest", "dev.archtracker.Daemon", "--object-path", "/dev/archtracker/KWin", "--method", "dev.archtracker.KWin1.Snapshot", snapshot], {
  env: { ...env, DBUS_SESSION_BUS_ADDRESS: bus },
  stdio: "ignore",
});
const focusedAt = Date.now();

const cdpPort = 9400 + Math.floor(Math.random() * 400);
const chrome = spawn(findChrome(), ["--headless", "--disable-gpu", "--hide-scrollbars", `--remote-debugging-port=${cdpPort}`, `--user-data-dir=${path.join(tmp, "chrome")}`, "--window-size=1440,1000", "about:blank"], { stdio: "ignore" });
let ws;
for (let i = 0; i < 60 && !ws; i++) {
  try {
    const pages = await (await fetch(`http://127.0.0.1:${cdpPort}/json`)).json();
    const page = pages.find((p) => p.type === "page");
    if (page) ws = new WebSocket(page.webSocketDebuggerUrl);
  } catch {
    await sleep(200);
  }
}
await new Promise((r) => ws.addEventListener("open", r));
let id = 0;
const waiting = new Map();
ws.addEventListener("message", (m) => {
  const d = JSON.parse(m.data);
  if (waiting.has(d.id)) {
    waiting.get(d.id)(d.result);
    waiting.delete(d.id);
  }
});
const cdp = (method, params = {}) => new Promise((r) => {
  waiting.set(++id, r);
  ws.send(JSON.stringify({ id, method, params }));
});
const js = (expression) => cdp("Runtime.evaluate", { expression, awaitPromise: true });

await cdp("Page.enable");
await cdp("Emulation.setDeviceMetricsOverride", { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
const { url } = await ask({ cmd: "login_url", route: "today" });
await cdp("Page.navigate", { url });
await sleep(2500);
// The scratch tracker has no Wayland or browser extension: hide those notices.
await js(`document.head.insertAdjacentHTML("beforeend", "<style>#notices{display:none!important}</style>")`);

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
await shot("today-dawn", "#/today", { height: 1000, theme: "light" });
// Let the live timer run a little before showing Now.
await sleep(Math.max(0, 74_000 - (Date.now() - focusedAt)));
await shot("now", "#/now", { height: 1000 });

ws.close();
chrome.kill();
try {
  process.kill(-daemon.pid, "SIGTERM");
} catch {
  daemon.kill();
}
await sleep(500);
fs.rmSync(tmp, { recursive: true, force: true });
fs.rmSync(run, { recursive: true, force: true });
