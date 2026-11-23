// arch-tracker companion: reports which tab is active in each window, as host + title only,
// to the local arch-tracker daemon through native messaging. Nothing leaves this machine.
// Paths, queries and anything from incognito windows are never sent.

const HOST = "dev.archtracker.native";
// A missing or crashing host is only reported later, through onDisconnect. So the backoff is
// reset only once a port has stayed up this long; otherwise the browser would respawn a broken
// host every second, forever.
const STABLE_MS = 5000;
let port = null;
let retryMs = 1000;
let retryTimer = null;
let timer = null;

function hostOf(url) {
  if (!url) return "";
  try {
    const u = new URL(url);
    if (u.protocol === "http:" || u.protocol === "https:") return u.host.toLowerCase();
    if (u.protocol === "file:") return "file://";
    return u.protocol + "//" + u.hostname; // brave://newtab, chrome-extension://<id>
  } catch {
    return "";
  }
}

async function sendSnapshot() {
  timer = null;
  if (!port) return;
  let wins;
  try {
    wins = await chrome.windows.getAll({ populate: true, windowTypes: ["normal", "popup", "app"] });
  } catch {
    return;
  }
  const windows = wins.map((w) => ({
    id: w.id,
    focused: !!w.focused,
    incognito: !!w.incognito,
    type: w.type || "normal",
    tabs: (w.tabs || []).map((t) => ({
      id: t.id,
      active: !!t.active,
      audible: !!t.audible,
      muted: !!(t.mutedInfo && t.mutedInfo.muted),
      host: w.incognito ? "" : hostOf(t.url || t.pendingUrl),
      title: w.incognito ? "" : t.title || "",
      split: typeof t.splitViewId === "number" && t.splitViewId >= 0 ? t.splitViewId : null,
    })),
  }));
  try {
    port.postMessage({ type: "snapshot", windows });
  } catch {
    // Port died between checks; onDisconnect reconnects.
  }
}

function schedule() {
  if (!timer) timer = setTimeout(sendSnapshot, 120);
}

function connect() {
  retryTimer = null;
  let p;
  try {
    p = chrome.runtime.connectNative(HOST);
  } catch {
    port = null;
    return reconnectLater();
  }
  port = p;
  const stable = setTimeout(() => {
    if (port === p) retryMs = 1000;
  }, STABLE_MS);
  p.onDisconnect.addListener(() => {
    clearTimeout(stable);
    void chrome.runtime.lastError; // read it, or Chromium logs "Unchecked runtime.lastError"
    if (port === p) port = null;
    reconnectLater();
  });
  schedule();
}

function reconnectLater() {
  if (retryTimer) return;
  retryTimer = setTimeout(connect, retryMs);
  retryMs = Math.min(retryMs * 2, 60000);
}

for (const ev of [
  chrome.tabs.onActivated,
  chrome.tabs.onUpdated,
  chrome.tabs.onRemoved,
  chrome.tabs.onCreated,
  chrome.tabs.onReplaced,
  chrome.tabs.onAttached,
  chrome.tabs.onDetached,
  chrome.tabs.onMoved,
  chrome.windows.onFocusChanged,
  chrome.windows.onCreated,
  chrome.windows.onRemoved,
]) {
  ev.addListener(schedule);
}

// Heartbeat: lets the daemon know the extension is alive (and keeps the worker awake).
setInterval(() => {
  if (port) schedule();
}, 20000);

chrome.runtime.onStartup.addListener(() => {
  if (!port && !retryTimer) connect();
});
connect();
