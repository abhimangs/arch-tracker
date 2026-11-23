// Runs extension/background.js against a fake browser API with a fake clock:
//   node scripts/test-extension.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const SOURCE = readFileSync(new URL("../extension/background.js", import.meta.url), "utf8");

function event() {
  const listeners = [];
  return {
    addListener: (f) => listeners.push(f),
    fire: (...args) => listeners.forEach((f) => f(...args)),
  };
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

// Loads background.js in a fresh context, as Chromium (chrome.* with promises) or as Firefox
// (browser.* with promises, callback-only chrome.*, no tabs.onReplaced).
function load({ windows = [], firefox = false } = {}) {
  let now = 0;
  let seq = 0;
  let timers = [];
  const setTimeout = (fn, ms = 0) => {
    const id = ++seq;
    timers.push({ id, at: now + ms, fn });
    return id;
  };
  const clearTimeout = (id) => {
    timers = timers.filter((t) => t.id !== id);
  };
  const setInterval = (fn, ms) => {
    const id = ++seq;
    const tick = () => {
      timers.push({ id, at: now + ms, fn: tick });
      fn();
    };
    timers.push({ id, at: now + ms, fn: tick });
    return id;
  };

  const b = { hostUp: true, crashAfter: 0, ports: [], windows };
  b.connectNative = (name) => {
    const port = {
      name,
      sent: [],
      dead: false,
      onDisconnect: event(),
      onMessage: event(),
      postMessage(m) {
        if (port.dead) throw new Error("Attempting to use a disconnected port object");
        port.sent.push(JSON.parse(JSON.stringify(m)));
      },
      disconnect() {},
    };
    b.ports.push(port);
    if (!b.hostUp) setTimeout(() => b.kill(port), 10);
    else if (b.crashAfter) setTimeout(() => b.kill(port), b.crashAfter);
    return port;
  };
  b.kill = (port = b.ports.at(-1)) => {
    if (port.dead) return;
    port.dead = true;
    port.onDisconnect.fire(port);
  };
  b.live = () => b.ports.filter((p) => !p.dead);
  b.snapshots = () => b.ports.flatMap((p) => p.sent).filter((m) => m.type === "snapshot");
  b.advance = async (ms) => {
    const end = now + ms;
    for (;;) {
      await flush();
      timers.sort((x, y) => x.at - y.at || x.id - y.id);
      const t = timers[0];
      if (!t || t.at > end) break;
      timers.shift();
      now = t.at;
      t.fn();
    }
    now = end;
    await flush();
  };

  const api = {
    runtime: { onStartup: event(), lastError: undefined, connectNative: b.connectNative },
    tabs: {},
    windows: {
      getAll: async () => JSON.parse(JSON.stringify(b.windows)),
    },
  };
  for (const name of ["onActivated", "onUpdated", "onRemoved", "onCreated", "onReplaced", "onAttached", "onDetached", "onMoved"]) {
    if (!(firefox && name === "onReplaced")) api.tabs[name] = event();
  }
  for (const name of ["onFocusChanged", "onCreated", "onRemoved"]) {
    api.windows[name] = event();
  }
  b.api = api;
  const ctx = { setTimeout, clearTimeout, setInterval, URL, console, chrome: api };
  if (firefox) {
    const callbackOnly = { ...api, windows: { ...api.windows, getAll: (_q, cb) => void cb?.([]) } };
    Object.assign(ctx, { browser: api, chrome: callbackOnly });
  }
  vm.createContext(ctx);
  vm.runInContext(SOURCE, ctx);
  return b;
}

const tests = [];
const test = (name, fn) => tests.push({ name, fn });

test("connects at start and sends a snapshot", async () => {
  const b = load({ windows: [{ id: 1, focused: true, type: "normal", tabs: [] }] });
  await b.advance(500);
  assert.equal(b.ports.length, 1);
  assert.equal(b.ports[0].name, "dev.archtracker.native");
  assert.equal(b.snapshots().length, 1);
});

test("backoff grows while the host is missing", async () => {
  const b = load();
  b.hostUp = false;
  b.kill();
  await b.advance(60_000);
  // 1 + 2 + 4 + 8 + 16 s: about five attempts a minute, not sixty.
  assert.ok(b.ports.length <= 7, `${b.ports.length} connection attempts in a minute`);
  await b.advance(10 * 60_000);
  const before = b.ports.length;
  await b.advance(5 * 60_000);
  assert.ok(b.ports.length - before <= 6, "backoff is capped at one attempt a minute");
});

test("backoff resets once a port stays up", async () => {
  const b = load();
  b.hostUp = false;
  b.kill();
  await b.advance(120_000);
  b.hostUp = true;
  await b.advance(61_000);
  assert.equal(b.live().length, 1, "reconnected once the host is back");
  await b.advance(6_000);
  const n = b.ports.length;
  b.kill();
  await b.advance(1_100);
  assert.equal(b.ports.length, n + 1, "reconnects after 1 s once the port was stable");
});

test("a host that keeps crashing early does not reset the backoff", async () => {
  const b = load();
  b.crashAfter = 3_000; // starts, then dies before the port counts as stable
  b.kill();
  await b.advance(5 * 60_000);
  // Resetting on connect would retry every 4 s (75 times); backing off gives about ten.
  assert.ok(b.ports.length <= 15, `${b.ports.length} attempts in five minutes`);
});

function tab(url, title, extra = {}) {
  return { id: 7, active: true, audible: true, mutedInfo: { muted: true }, url, title, ...extra };
}

// Sends one window holding the given tab and returns what the extension reported for it.
async function report(t, win = {}, firefox = false) {
  const b = load({ windows: [{ id: 1, focused: true, type: "normal", tabs: [t], ...win }], firefox });
  await b.advance(500);
  return b.snapshots().at(-1).windows[0].tabs[0];
}

test("normal tabs send host and title", async () => {
  const r = await report(tab("https://www.example.com/watch?v=abc", "Some video - Example"));
  assert.equal(r.host, "www.example.com");
  assert.equal(r.title, "Some video - Example");
  assert.equal(r.audible, true);
  assert.equal(r.muted, true);
});

test("URL-like titles are dropped", async () => {
  const cases = [
    ["https://www.example.com/search?q=private+words", "www.example.com/search?q=private+words"],
    ["https://www.example.com/search?q=private+words", "example.com/search?q=private words"],
    ["https://www.example.com/search?q=private+words", "https://www.example.com/search?q=private+words"],
    ["http://127.0.0.1:8080/api/items?token=abc", "127.0.0.1:8080/api/items?token=abc"],
    ["https://example.org/a/b/c", "example.org/a/b"],
    ["https://example.org/docs/page", "example.org/docs/page – loading"],
    ["file:///srv/shared/notes/plan.txt", "/srv/shared/notes/plan.txt"],
    ["about:blank", "https://example.net/secret/path"],
  ];
  for (const [url, title] of cases) {
    assert.equal((await report(tab(url, title))).title, "", `${title} from ${url}`);
  }
});

test("real titles that mention the site are kept", async () => {
  const cases = [
    ["https://example.com/", "Example Domain"],
    ["https://example.com/", "example.com"],
    ["https://news.example.com/item?id=1", "Example News"],
    ["https://docs.example.com/guide", "Guide - docs.example.com"],
  ];
  for (const [url, title] of cases) {
    assert.equal((await report(tab(url, title))).title, title, `${title} from ${url}`);
  }
});

test("private windows report time only", async () => {
  const r = await report(tab("https://example.com/x", "Example"), { incognito: true });
  assert.deepEqual(r, { id: 7, active: true, audible: false, muted: false, host: "", title: "", split: null });
});

test("Firefox: uses browser.* and reports tabs", async () => {
  const r = await report(tab("https://www.example.com/a?b=c", "Example page"), {}, true);
  assert.equal(r.host, "www.example.com");
  assert.equal(r.title, "Example page");
});

test("internal pages get a host without paths or queries", async () => {
  const cases = [
    ["about:newtab", "about:newtab"],
    ["about:preferences#privacy", "about:preferences"],
    ["about:reader?url=https%3A%2F%2Fnews.example.com%2Fstory%3Fid%3D3", "news.example.com"],
    ["chrome://newtab/", "chrome://newtab"],
    ["brave://settings/privacy", "brave://settings"],
    ["moz-extension://0d2f4e5a-1111-2222-3333-444455556666/page.html", "moz-extension://0d2f4e5a-1111-2222-3333-444455556666"],
    ["file:///srv/shared/notes/plan.txt", "file://"],
  ];
  for (const [url, host] of cases) {
    assert.equal((await report(tab(url, "Page"), {}, true)).host, host, url);
  }
});

let failed = 0;
for (const { name, fn } of tests) {
  try {
    await fn();
    console.log(`ok    ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL  ${name}\n${e.stack}`);
  }
}
if (failed) {
  console.log(`${failed} of ${tests.length} failed`);
  process.exit(1);
}
console.log(`all ${tests.length} passed`);
