#!/usr/bin/env node
// Opens every dashboard page on made-up data (arch-tracker demo) in headless Chromium, at desktop
// and phone widths and in dark and light, and fails on any script error, failed request, page that
// could not load, or "NaN" / "undefined" shown to the reader. CI runs it after the build:
//
//   cargo build && node scripts/smoke-dashboard.mjs
import { openDemo, sleep } from "./demo-dashboard.mjs";

const demo = await openDemo({ port: 7691 });
const problems = [];
let where = "sign-in";
demo.onEvent((m) => {
  const p = m.params || {};
  if (m.method === "Runtime.exceptionThrown") {
    const d = p.exceptionDetails || {};
    problems.push(`${where}: ${(d.exception && d.exception.description) || d.text}`);
  } else if (m.method === "Runtime.consoleAPICalled" && (p.type === "error" || p.type === "assert")) {
    problems.push(`${where}: console.${p.type} ${(p.args || []).map((a) => a.value ?? a.description).join(" ")}`);
  } else if (m.method === "Log.entryAdded" && p.entry && p.entry.level === "error") {
    // A browser whose icon is not installed (as on a test machine) gets a letter instead.
    if (/\/api\/icon\//.test(p.entry.url || "") && /404/.test(p.entry.text)) return;
    problems.push(`${where}: ${p.entry.text} ${p.entry.url || ""}`.trim());
  }
});
await demo.cdp("Runtime.enable");
await demo.cdp("Log.enable");
await demo.login("today");

const ids = async (api) => {
  const r = await demo.js(`fetch("${api}").then((r) => r.json()).then((l) => JSON.stringify(l.map((x) => x.id)))`);
  return JSON.parse(r.result.value || "[]");
};
const browsers = await ids("/api/browsers");
const agents = await ids("/api/agents");
if (!browsers.length || !agents.length) problems.push(`demo data lists no ${browsers.length ? "agents" : "browsers"}`);
const yesterday = new Date(Date.now() - 86_400_000).toLocaleDateString("sv-SE");
const routes = [
  "#/now",
  "#/today",
  `#/day/${yesterday}`,
  "#/timeline",
  "#/apps/1",
  "#/apps/7",
  "#/apps/30",
  "#/projects/7",
  "#/projects/30",
  "#/trends/7",
  "#/trends/30",
  "#/trends/365",
  "#/settings",
  "#/browser/all/1",
  "#/browser/all/7",
  ...browsers.flatMap((b) => [`#/browser/${encodeURIComponent(b)}/1`, `#/browser/${encodeURIComponent(b)}/7`]),
  "#/agent/all/7",
  ...agents.flatMap((a) => [`#/agent/${a}/1`, `#/agent/${a}/30`, `#/agent/${a}/all`]),
];
const looks = [
  { name: "desktop, dark", width: 1440, height: 1000, mobile: false, theme: "dark" },
  { name: "desktop, light", width: 1440, height: 1000, mobile: false, theme: "light" },
  { name: "phone, dark", width: 390, height: 844, mobile: true, theme: "dark" },
];
let views = 0;
for (const look of looks) {
  await demo.cdp("Emulation.setDeviceMetricsOverride", { width: look.width, height: look.height, deviceScaleFactor: 1, mobile: look.mobile });
  for (const r of routes) {
    where = `${r} (${look.name})`;
    await demo.js(`document.documentElement.dataset.theme = "${look.theme}"; location.hash = "${r}"`);
    await sleep(1000);
    const res = await demo.js(`(() => {
      const v = document.getElementById("view");
      const t = v.innerText;
      return JSON.stringify({
        empty: !v.children.length || /Loading…$/.test(t.trim()),
        failed: /Couldn't load this page/.test(t),
        bad: (t.match(/\\bNaN\\b|\\bundefined\\b|\\[object Object\\]|Infinity/) || [""])[0],
        wide: document.documentElement.scrollWidth > window.innerWidth + 1,
      });
    })()`);
    const s = JSON.parse(res.result.value);
    if (s.empty) problems.push(`${where}: nothing shown`);
    if (s.failed) problems.push(`${where}: the page could not load its data`);
    if (s.bad) problems.push(`${where}: shows "${s.bad}"`);
    if (s.wide) problems.push(`${where}: wider than the window`);
    views++;
  }
}
await demo.close();
if (problems.length) {
  console.error(problems.join("\n"));
  console.error(`\n${problems.length} problems in ${views} page views`);
  process.exit(1);
}
console.log(`ok: ${views} page views (${routes.length} pages, ${looks.length} looks), no errors`);
