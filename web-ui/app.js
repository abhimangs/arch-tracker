"use strict";
// arch-tracker dashboard. Plain JS, no build step. Every string from the tracker (titles, hosts,
// app names) is escaped before it touches the DOM: page titles are untrusted input.

(() => {
  const TOKEN = document.querySelector('meta[name="csrf"]').content;
  const root = document.documentElement;
  const view = document.getElementById("view");
  const railLive = document.getElementById("now");
  const noticesEl = document.getElementById("notices");
  // One listener for the whole notices area: the buttons are re-rendered on every status.
  noticesEl.addEventListener("click", (e) => {
    if (!e.target.closest("#hideext")) return;
    localStorage.setItem("hide-ext-notice", String(Date.now() + 7 * 86400000));
    renderNotices();
  });
  const shell = document.getElementById("shell");
  const NB = "\u00a0";
  let status = null;
  let clockOffset = 0; // server clock minus ours, from each status
  let firstRibbon = true;
  let refreshTimer = null; // the one periodic page refresh; route() clears it
  let currentDay = null;
  let currentPage = "";
  // Bumped on every navigation. A render that awaited something checks it afterwards and drops
  // its result if the user has moved on, so a slow reply can't overwrite a newer page.
  let renderGen = 0;
  const stale = (gen) => gen !== renderGen;
  const openRows = new Set();
  // Settings the pages need (time format, goals…), loaded once and refreshed after saving.
  let settings = null;
  const prefs = () => (settings && settings.config) || { dashboard: {}, insights: {}, categories: [] };

  // ---------- helpers ----------

  const esc = (v) =>
    String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  const pad = (n, w = 2) => String(n).padStart(w, "0");
  const ymd = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const parseYmd = (s) => {
    const [y, m, d] = s.split("-").map(Number);
    return new Date(y, m - 1, d);
  };
  const addDays = (s, n) => {
    const d = parseYmd(s);
    d.setDate(d.getDate() + n);
    return ymd(d);
  };
  const today = () => ymd(new Date());
  const twelve = () => prefs().dashboard.time_format === "12h";
  // Clock times: "14:03", "14:03:12", and with milliseconds in a dimmer span.
  function clock(ms, withSecs = false) {
    const d = new Date(ms);
    let h = d.getHours();
    const tail = withSecs ? `:${pad(d.getMinutes())}:${pad(d.getSeconds())}` : `:${pad(d.getMinutes())}`;
    if (!twelve()) return pad(h) + tail;
    const ap = h < 12 ? "am" : "pm";
    h = h % 12 || 12;
    return `${h}${tail}${NB}${ap}`;
  }
  const clockMs = (ms) => `${esc(clock(ms, true))}<span class="ms">.${pad(new Date(ms).getMilliseconds(), 3)}</span>`;
  const hm = (ms) => clock(ms);
  // "14:00" from the server's hour marks, shown in the chosen clock.
  const hourLabel = (label) => {
    if (!twelve()) return label;
    const h = Number(label.slice(0, 2));
    return `${h % 12 || 12}${NB}${h < 12 || h === 24 ? "am" : "pm"}`;
  };
  const dayLabel = (s, opts) =>
    parseYmd(s).toLocaleDateString(undefined, opts || { weekday: "long", day: "numeric", month: "long" });
  const rawColor = (c) => (/^#[0-9a-fA-F]{3,8}$/.test(c || "") ? c : "#6b7392");
  const cap = (s) => s.charAt(0).toUpperCase() + s.slice(1);
  const projName = (p) => String(p || "").replace(/\/+$/, "").split("/").pop() || p;

  function dur(secs) {
    secs = Math.max(0, Math.round(secs || 0));
    if (secs < 60) return `${secs}${NB}s`;
    const h = Math.floor(secs / 3600);
    const m = Math.floor(secs / 60) % 60;
    if (h === 0) return `${m}${NB}m`;
    return `${h}${NB}h ${pad(m)}${NB}m`;
  }
  // Exact length, for sessions: "1 h 02 m 07 s", "4 m 12.418 s".
  function durExact(ms) {
    ms = Math.max(0, Math.round(ms || 0));
    const h = Math.floor(ms / 3600000);
    const m = Math.floor(ms / 60000) % 60;
    const s = Math.floor(ms / 1000) % 60;
    if (h) return `${h}${NB}h ${pad(m)}${NB}m ${pad(s)}${NB}s`;
    if (m) return `${m}${NB}m ${pad(s)}.${pad(ms % 1000, 3)}${NB}s`;
    return `${s}.${pad(ms % 1000, 3)}${NB}s`;
  }
  function durBig(secs) {
    secs = Math.max(0, Math.round(secs || 0));
    const h = Math.floor(secs / 3600);
    const m = Math.floor(secs / 60) % 60;
    if (h === 0) return `${m}<small>m</small>`;
    return `${h}<small>h</small>${pad(m)}<small>m</small>`;
  }
  // A stopwatch: "1:02:07" or "02:07", with milliseconds when asked.
  function stopwatch(ms, withMs) {
    ms = Math.max(0, ms);
    const h = Math.floor(ms / 3600000);
    const m = Math.floor(ms / 60000) % 60;
    const s = Math.floor(ms / 1000) % 60;
    const main = h ? `${h}:${pad(m)}:${pad(s)}` : `${pad(m)}:${pad(s)}`;
    return withMs ? `${main}<span class="ms">.${pad(Math.floor(ms % 1000), 3)}</span>` : main;
  }
  const serverNow = () => Date.now() + clockOffset;

  // ---------- theme ----------
  // <html> carries the look (data-theme/palette/dark/accent/theme-cats/density/motion, filled in
  // by the server so there is no flash). themes.css maps it to colours; this code only switches
  // attributes.
  const CAT_SLOTS = ["ai", "development", "social", "video", "messaging", "music", "meetings", "productivity", "learning", "system", "browsing", "other"];
  const catByColor = new Map();
  const learnCats = (list) => {
    for (const c of list || []) if (c && c.name && c.color) catByColor.set(String(c.color).toLowerCase(), c.name);
  };
  const slotFor = (name) => {
    const s = String(name || "").toLowerCase();
    if (CAT_SLOTS.includes(s)) return s;
    let h = 0;
    for (const ch of s) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
    return `x${(h % 4) + 1}`;
  };
  const themedCats = () => root.dataset.themeCats === "on" && root.dataset.palette !== "aizome";
  // A category colour: the palette's slot when "follow theme" is on, else the colour from Settings.
  const safeColor = (c, name) => {
    const hex = rawColor(c);
    if (!themedCats()) return hex;
    const n = name || catByColor.get(hex.toLowerCase());
    return n ? `var(--cat-${slotFor(n)})` : hex;
  };
  const FLAVOR = { mocha: "Mocha", macchiato: "Macchiato", frappe: "Frappé", main: "Main", moon: "Moon", night: "Night", latte: "Latte", dawn: "Dawn", fog: "Fog" };
  const look = () => ({
    theme: root.dataset.theme,
    palette: root.dataset.palette,
    dark_flavor: root.dataset.dark,
    accent: root.dataset.accent,
    theme_categories: root.dataset.themeCats === "on",
  });
  const applyLook = (l) => {
    root.dataset.theme = l.theme;
    root.dataset.palette = l.palette;
    root.dataset.dark = l.dark_flavor;
    root.dataset.accent = l.accent;
    root.dataset.themeCats = l.theme_categories ? "on" : "off";
  };
  const motionOK = () =>
    root.dataset.motion === "full" || (root.dataset.motion !== "reduced" && !matchMedia("(prefers-reduced-motion: reduce)").matches);
  // Popovers grow from their trigger; pages settle in. Both short, both skipped for reduced motion.
  function popIn(el) {
    if (motionOK()) el.animate([{ opacity: 0, transform: "scale(0.97)" }, { opacity: 1, transform: "scale(1)" }], { duration: 160, easing: "cubic-bezier(0.23, 1, 0.32, 1)" });
  }
  let enterNext = false;
  function paint(html) {
    view.innerHTML = html;
    if (enterNext) {
      enterNext = false;
      if (motionOK()) view.animate([{ opacity: 0, transform: "translateY(6px)" }, { opacity: 1, transform: "none" }], { duration: 200, easing: "cubic-bezier(0.23, 1, 0.32, 1)" });
    }
  }

  // ---------- requests ----------

  // The session cookie is gone (deleted key, cleared cookies): reload to show the locked page.
  let reloading = false;
  function signedOut(r) {
    if (r.status === 401 && !reloading) {
      reloading = true;
      location.reload();
    }
  }

  async function get(path) {
    const r = await fetch(path, { headers: { accept: "application/json" } });
    signedOut(r);
    const body = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(body.error || `The tracker answered ${r.status}.`);
    return body;
  }

  async function send(method, path, data) {
    const r = await fetch(path, {
      method,
      headers: { "content-type": "application/json", "x-arch-tracker-token": TOKEN },
      body: JSON.stringify(data ?? {}),
    });
    signedOut(r);
    const body = await r.json().catch(() => ({}));
    if (!r.ok) {
      const err = new Error(body.error || `The tracker answered ${r.status}.`);
      err.field = body.field;
      throw err;
    }
    return body;
  }

  async function loadSettings() {
    settings = await get("/api/settings");
    learnCats(settings.config.categories);
    return settings;
  }

  // A selector that finds the focused element again after a redraw.
  function focusKey() {
    const a = document.activeElement;
    if (!a || !view.contains(a)) return null;
    if (a.id) return `#${CSS.escape(a.id)}`;
    const row = a.closest("[data-id]");
    return row ? `[data-id="${CSS.escape(row.dataset.id)}"] button` : null;
  }

  const errorHtml = (e) =>
    `<p class="empty">Couldn't load this page: ${esc(e.message)}. Check that the tracker is running (systemctl --user status arch-tracker), then reload.</p>`;

  function head(title, meta = "", actions = "") {
    return `<div class="page-head"><div class="titles"><h1>${title}</h1>${meta ? `<span class="meta">${meta}</span>` : ""}</div>${actions ? `<div class="actions">${actions}</div>` : ""}</div>`;
  }
  function panel(title, body, opts = {}) {
    const aside = opts.href ? `<a class="aside" href="${opts.href}">${esc(opts.aside || "More")}</a>` : opts.aside ? `<span class="aside">${esc(opts.aside)}</span>` : "";
    return `<section class="panel ${opts.w || ""}"><header><h2>${esc(title)}</h2>${aside}</header>${opts.sub ? `<p class="sub">${esc(opts.sub)}</p>` : ""}${body}</section>`;
  }
  function dayNav(date, base) {
    const isToday = date === today();
    return `<button class="btn" type="button" data-go="${addDays(date, -1)}" data-base="${base}" aria-label="Previous day">Previous</button>
      ${isToday ? "" : `<button class="btn" type="button" data-go="${today()}" data-base="${base}">Today</button>`}
      <button class="btn" type="button" data-go="${addDays(date, 1)}" data-base="${base}" ${isToday ? "disabled" : ""} aria-label="Next day">Next</button>`;
  }
  function bindDayNav() {
    view.querySelectorAll("[data-go]").forEach((b) =>
      b.addEventListener("click", () => {
        const go = b.dataset.go;
        const base = b.dataset.base;
        location.hash = go === today() ? `#/${base}` : `#/${base === "today" ? "day" : base}/${go}`;
      }),
    );
  }

  // ---------- shared pieces ----------

  function rowHtml(r, top, prefix, opts = {}) {
    const c = safeColor(r.color);
    const w = (x) => `${((x / top) * 100).toFixed(2)}%`;
    const titles = opts.noTitles ? [] : (r.titles || []).filter((t) => t.title);
    const id = `${prefix}:${r.key}`;
    const open = openRows.has(id);
    let name = r.name || r.key;
    let detail = "";
    if (opts.program) {
      const parts = r.key.split("\u001f");
      name = parts[0];
      detail = parts[1] ? `in ${projName(parts[1])}` : "";
    } else if (r.visible && r.total < r.visible) {
      detail = `${dur(r.visible)} beside other windows`;
    }
    return `<li class="row" data-id="${esc(id)}" data-open="${open}">
      <button class="rh" type="button" ${titles.length ? `aria-expanded="${open}"` : ""}>
        <span class="rn"><i class="sw" style="--c:${c}"></i><span>${esc(name)}</span>${detail ? `<em>${esc(detail)}</em>` : ""}</span>
        <span class="rc">${esc(r.category)}</span>
        <span class="rd">${dur(r.total)}</span>
        <span class="rb" aria-hidden="true"><i style="--w:${w(r.active)};--c:${c}"></i><i class="p" style="--w:${w(r.passive)};--c:${c}"></i><i class="k" style="--w:${w(r.call)};--c:${c}"></i></span>
      </button>
      ${titles.length ? `<div class="rx"><div><ul class="titles">${titles.map((t) => `<li><span>${esc(t.title)}</span><span>${dur(t.secs)}</span></li>`).join("")}</ul></div></div>` : ""}
    </li>`;
  }

  function rowsHtml(list, prefix, opts = {}) {
    const max = opts.max ?? 8;
    list = (list || []).filter((r) => r.total > 0 || (opts.withVisible && r.visible > 0));
    if (!list.length) return `<p class="empty">${esc(opts.empty || "Nothing yet.")}</p>`;
    const top = Math.max(...list.map((r) => r.total), 1);
    const rest = list.length - max;
    return `<ol class="rows">${list.slice(0, max).map((r) => rowHtml(r, top, prefix, opts)).join("")}</ol>${
      rest > 0 && opts.moreLink ? `<p class="more">${rest} more in <a href="${opts.moreLink}">${esc(opts.moreLabel || "Apps and sites")}</a>.</p>` : ""
    }`;
  }

  // Categories, with each one's daily limit (from Settings) when it has one.
  function catsHtml(list, opts = {}) {
    list = (list || []).filter((c) => c.total > 0);
    if (!list.length) return `<p class="empty">No screen time yet.</p>`;
    const total = list.reduce((a, c) => a + c.total, 0) || 1;
    const limits = Object.fromEntries((prefs().categories || []).map((c) => [c.name, (c.goal_mins || 0) * 60]));
    const seg = list.map((c) => `<i style="--w:${((c.total / total) * 100).toFixed(2)}%;--c:${safeColor(c.color, c.name)}" title="${esc(c.name)}"></i>`).join("");
    const items = list
      .map((c) => {
        const lim = opts.limits !== false ? limits[c.name] : 0;
        const bar = lim
          ? `<span class="lim" data-over="${c.total > lim}" title="Limit ${dur(lim)}"><i style="--w:${Math.min(100, (c.total / lim) * 100).toFixed(1)}%;--c:${safeColor(c.color, c.name)}"></i></span>`
          : "";
        const of = lim ? ` <span class="d">of ${dur(lim)}</span>` : "";
        return `<li><span class="cn"><i class="sw" style="--c:${safeColor(c.color, c.name)}"></i><span>${esc(c.name)}</span></span><span class="d">${Math.round((c.total / total) * 100)}%</span><span>${dur(c.total)}${of}</span>${bar}</li>`;
      })
      .join("");
    return `<div class="stack" aria-hidden="true">${seg}</div><ul class="plain cats">${items}</ul>`;
  }

  function agentsHtml(list, empty) {
    list = (list || []).filter((a) => a.focus > 0 || a.busy > 0);
    if (!list.length) return `<p class="empty">${esc(empty)}</p>`;
    const top = Math.max(...list.map((a) => Math.max(a.focus, a.busy)), 1);
    const w = (x) => `${((x / top) * 100).toFixed(2)}%`;
    return list
      .map(
        (a) => `<div class="agent">
        <div class="who"><span>${esc(a.name || a.tool)}</span><em>${esc(a.project ? projName(a.project) : "no project")}</em></div>
        <div class="pair">
          <span>You</span><span class="rb"><i style="--w:${w(a.focus)};--c:var(--text)"></i></span><span class="v">${dur(a.focus)}</span>
          <span>Agent</span><span class="rb"><i style="--w:${w(a.busy)};--c:var(--agent, #b49be8)"></i></span><span class="v">${dur(a.busy)}</span>
        </div>
      </div>`,
      )
      .join("");
  }

  function plainHtml(list, empty, opts = {}) {
    list = (list || []).filter((x) => x.secs > 0);
    if (!list.length) return `<p class="empty">${esc(empty)}</p>`;
    return `<ul class="plain">${list
      .slice(0, opts.max ?? 8)
      .map((x) => `<li><span>${esc(x.name)} <span class="d">${esc(x.detail || "")}</span></span><span>${dur(x.secs)}</span></li>`)
      .join("")}</ul>`;
  }

  // Coding per project: editor and terminal time you focused, and time an agent worked on it.
  function projectsHtml(list, opts = {}) {
    list = (list || []).filter((p) => p.total > 0 || p.agent_busy > 0);
    if (!list.length) return `<p class="empty">${esc(opts.empty || "No coding yet.")}</p>`;
    const top = Math.max(...list.map((p) => Math.max(p.total, p.agent_busy)), 1);
    const w = (x) => `${((x / top) * 100).toFixed(2)}%`;
    return list
      .slice(0, opts.max ?? 50)
      .map((p) => {
        const tools = [...(p.editors || []), ...(p.programs || [])].sort((a, b) => b.secs - a.secs).slice(0, 5);
        const spark = opts.days ? sparkHtml(opts.days, p.key) : "";
        return `<div class="proj" data-id="proj:${esc(p.key)}">
          <div class="top"><b title="${esc(p.key)}">${esc(p.name)}</b><span>${dur(p.total)}</span></div>
          <div class="split" aria-hidden="true"><i class="e" style="--w:${w(p.editor)}"></i><i class="t" style="--w:${w(p.terminal)}"></i></div>
          ${p.agent_busy ? `<div class="split" aria-hidden="true"><i class="a" style="--w:${w(p.agent_busy)}"></i></div>` : ""}
          <div class="meta"><span>Editor <b>${dur(p.editor)}</b></span><span>Terminal <b>${dur(p.terminal)}</b></span>${p.agent_busy ? `<span>Agents worked <b>${dur(p.agent_busy)}</b></span>` : ""}${
            tools.length ? `<span>${tools.map((t) => esc(t.title)).join(", ")}</span>` : ""
          }</div>
          ${spark}
        </div>`;
      })
      .join("");
  }
  function sparkHtml(days, key) {
    const vals = days.map((d) => ((d.projects || []).find((p) => p.key === key) || {}).total || 0);
    const max = Math.max(...vals, 1);
    return `<div class="spark" aria-hidden="true">${vals
      .map((v, i) => (v ? `<i style="--h:${Math.max(8, (v / max) * 100).toFixed(1)}%" title="${esc(dayLabel(days[i].date, { weekday: "short", day: "numeric", month: "short" }))}: ${dur(v)}"></i>` : '<i class="z"></i>'))
      .join("")}</div>`;
  }

  // ---------- the day chart ----------

  const STATE_CLASS = { active: "a", passive: "p", call: "k", idle: "i", excluded: "x" };
  const STATE_WORD = { active: "active", passive: "watching", call: "in a call", idle: "idle, not counted", excluded: "locked, asleep or paused", "": "nothing recorded" };

  function ribbonHtml(d, isToday) {
    const span = d.end_ms - d.start_ms;
    const bars = d.timeline
      .map((b, i) => {
        const cls = STATE_CLASS[b.state] || "e";
        const counted = cls === "a" || cls === "p" || cls === "k";
        const h = counted ? Math.max(7, Math.min(100, (b.counted / 300) * 100)) : 0;
        return `<i class="${cls}" data-i="${i}" style="--h:${h.toFixed(1)}%;--c:${safeColor(b.color, b.category)};--n:${i}"></i>`;
      })
      .join("");
    let needle = "";
    if (isToday) {
      const pct = ((serverNow() - d.start_ms) / span) * 100;
      if (pct >= 0 && pct <= 100) needle = `<span class="needle" style="left:${pct.toFixed(2)}%" title="Now"></span>`;
    }
    // Labels come from the server's local-clock hours: a day when the clocks change has 23 or 25.
    const marks = d.hours && d.hours.length ? d.hours : Array.from({ length: 24 }, (_, h) => ({ start_ms: d.start_ms + h * 3600000, label: `${pad(h)}:00` }));
    const seen = new Set();
    const hours = marks
      .filter((h) => /^\d\d:00$/.test(h.label) && Number(h.label.slice(0, 2)) % 3 === 0 && !seen.has(h.label) && seen.add(h.label))
      .map((h) => `<span style="left:${(((h.start_ms - d.start_ms) / span) * 100).toFixed(3)}%">${esc(hourLabel(h.label))}</span>`);
    hours.push(`<span style="left:100%">${twelve() ? "12" + NB + "am" : "24:00"}</span>`);
    const label = `Day chart: ${dur(d.totals.screen)} on screen, in five-minute bars.`;
    const rise = firstRibbon && motionOK() ? " rise" : "";
    firstRibbon = false;
    return `<div class="ribbon${rise}" id="ribbon" role="slider" tabindex="0" aria-label="${esc(label)} Use the arrow keys to read each bar." aria-valuemin="0" aria-valuemax="${d.timeline.length - 1}" aria-valuenow="0">${bars}${needle}</div>
      <div class="hours" aria-hidden="true">${hours.join("")}</div>
      <p class="readout" id="readout" aria-live="polite">Point at a bar, or focus the chart and use the arrow keys, to see what you were doing.</p>`;
  }

  function bindRibbon(d) {
    const rib = document.getElementById("ribbon");
    const out = document.getElementById("readout");
    if (!rib || !out) return;
    const idle = out.textContent;
    let last = null;
    const show = (i) => {
      const b = d.timeline[i];
      if (!b) return;
      if (last) last.classList.remove("hover");
      last = rib.children[i];
      last.classList.add("hover");
      const what = b.app
        ? `<span>${esc(b.app)}</span>${b.detail ? `<span>${esc(b.detail)}</span>` : ""}<span>${dur(b.counted)} counted</span>`
        : "";
      out.innerHTML = `<b>${hm(b.start_ms)} to ${hm(b.start_ms + 300000)}</b>${what}<span>${esc(STATE_WORD[b.state] ?? "")}</span>`;
    };
    rib.addEventListener("pointermove", (e) => {
      const t = e.target.closest("i[data-i]");
      if (t) show(Number(t.dataset.i));
    });
    // Keyboard: arrows step through five-minute bars, Home/End jump; the readout is announced.
    let cur = null;
    rib.addEventListener("keydown", (e) => {
      const n = d.timeline.length;
      if (!n) return;
      const nowIdx = Math.min(n - 1, Math.max(0, Math.floor((serverNow() - d.start_ms) / 300000)));
      const from = cur ?? nowIdx;
      const to = { ArrowLeft: from - 1, ArrowRight: from + 1, Home: 0, End: n - 1, PageUp: from - 12, PageDown: from + 12 }[e.key];
      if (to === undefined) return;
      e.preventDefault();
      cur = Math.max(0, Math.min(n - 1, to));
      rib.setAttribute("aria-valuenow", String(cur));
      show(cur);
      rib.setAttribute("aria-valuetext", out.textContent);
    });
    const reset = () => {
      if (last) last.classList.remove("hover");
      last = null;
      out.textContent = idle;
    };
    rib.addEventListener("blur", () => {
      cur = null;
      reset();
    });
    rib.addEventListener("pointerleave", () => {
      if (document.activeElement !== rib) reset();
    });
  }

  function bindRows(root) {
    root.addEventListener("click", (e) => {
      const btn = e.target.closest(".rh[aria-expanded]");
      if (btn) {
        const row = btn.closest(".row");
        const open = row.dataset.open !== "true";
        row.dataset.open = String(open);
        btn.setAttribute("aria-expanded", String(open));
        if (open) openRows.add(row.dataset.id);
        else openRows.delete(row.dataset.id);
        return;
      }
      const s = e.target.closest(".sess[aria-expanded]");
      if (s) {
        const open = s.getAttribute("aria-expanded") !== "true";
        s.setAttribute("aria-expanded", String(open));
        if (open) openRows.add(s.dataset.id);
        else openRows.delete(s.dataset.id);
      }
    });
  }

  // ---------- live state ----------

  function stateOf(s) {
    if (!s) return ["", "Connecting"];
    if (s.viewer) return ["paused", "Read-only view"];
    if (s.paused) return ["paused", s.paused_until_ms ? `Paused until ${hm(s.paused_until_ms)}` : "Paused"];
    if (s.suspended) return ["locked", "Asleep"];
    if (s.locked) return ["locked", "Locked"];
    if (s.idle && s.idle.input_idle && (s.media_visible || []).length) return ["watching", "Watching"];
    if (s.mic) return ["call", "In a call"];
    if (s.idle && s.idle.input_idle) return ["idle", "No input"];
    return ["active", "Active"];
  }

  // What the focused window shows, in words: [app, title, details[]].
  function focusParts(f) {
    if (!f) return ["Nothing in focus", "", []];
    const details = [];
    let title = f.title || "";
    if (f.private) {
      details.push("Private window");
      title = "";
    } else if (f.host) details.push(f.host);
    if (f.program) {
      details.push(f.project ? `${f.program} in ${projName(f.project)}` : f.program);
      title = "";
    }
    return [f.app_name || f.app, title, details];
  }

  // Elements with data-since show a running stopwatch from that time; data-ms adds milliseconds.
  let ticking = false;
  function startTicker() {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(tick);
  }
  function tick() {
    const els = document.querySelectorAll("[data-since]");
    if (!els.length || document.hidden) {
      ticking = false;
      return;
    }
    const now = serverNow();
    const showMs = prefs().dashboard.show_ms !== false;
    for (const el of els) {
      const v = stopwatch(now - Number(el.dataset.since), el.dataset.ms === "1" && showMs);
      if (el._v !== v) {
        el.innerHTML = v;
        el._v = v;
      }
    }
    requestAnimationFrame(tick);
  }

  // ---------- Now ----------

  function heroHtml(s) {
    const [st, label] = stateOf(s);
    const f = s.focus;
    const [app, title, details] = focusParts(f);
    const idle = s.idle && s.idle.input_idle && s.idle_since_ms;
    const clock = f
      ? `<div class="clock"><span>On this window</span><div class="timer" data-since="${s.focus_since_ms}" data-ms="1"></div>
         <span>In ${esc(app)} for <b class="timer small" data-since="${s.app_since_ms}"></b></span></div>`
      : idle
        ? `<div class="clock"><span>No input for</span><div class="timer" data-since="${s.idle_since_ms}" data-ms="1"></div></div>`
        : "";
    return `<section class="hero" aria-label="In focus now">
      <div class="who">
        <span class="state" data-s="${st}">${esc(label)}</span>
        <div class="app">${esc(app)}</div>
        ${title ? `<div class="title" title="${esc(title)}">${esc(title)}</div>` : ""}
        ${details.length ? `<div class="detail">${details.map((d) => `<span>${esc(d)}</span>`).join("")}</div>` : ""}
      </div>
      ${clock}
    </section>`;
  }

  function nowPanels(s) {
    const subj = (x) => {
      const [a, t, d] = focusParts(x);
      return `<span class="n">${esc(a)}<em>${esc(d.join(", ") || t)}</em></span>`;
    };
    const visible = (s.visible || []).length
      ? `<ul class="list">${s.visible.map((v) => `<li>${subj(v)}</li>`).join("")}</ul>`
      : '<p class="empty">Only the focused window is on screen.</p>';
    const terms = (s.terminals || []).length
      ? `<ul class="list">${s.terminals
          .map((t) => `<li><span class="n">${esc(t.program || "shell")}<em>${esc(t.project ? projName(t.project) : "")}</em></span><span class="v">${t.source === "kitty" ? "exact" : "best guess"}</span></li>`)
          .join("")}</ul>`
      : '<p class="empty">No terminal on screen.</p>';
    const agents = (s.agents || []).length
      ? `<ul class="list">${s.agents
          .map((a) => `<li><span class="n">${esc(a.subject.app_name)}<em>${esc(projName(a.subject.project) || "")}</em></span><span class="v"><span class="pill" data-on="${a.busy}">${a.busy ? "Working" : "Waiting"}</span> ${a.cpu_percent.toFixed(1)}% CPU</span></li>`)
          .join("")}</ul>`
      : '<p class="empty">No AI agent is running.</p>';
    const players = (s.players || []).filter((p) => p.playing || p.title);
    const media = players.length || (s.background_audio || []).length
      ? `<ul class="list">${players
          .map((p) => `<li><span class="n">${esc(p.name)}<em>${esc(p.title || p.host || "")}</em></span><span class="v"><span class="pill" data-on="${p.playing}">${p.playing ? "Playing" : "Paused"}</span></span></li>`)
          .join("")}${(s.background_audio || [])
          .map((b) => `<li>${subj(b)}<span class="v">in the background</span></li>`)
          .join("")}</ul>`
      : '<p class="empty">Nothing is playing.</p>';
    const idle = s.idle || {};
    const facts = `<dl class="facts">
      <dt>Input</dt><dd>${idle.input_idle ? (s.idle_since_ms ? `none for <span data-since="${s.idle_since_ms}"></span>` : "none") : "active"}</dd>
      <dt>Microphone</dt><dd>${s.mic ? "in use" : "off"}</dd>
      <dt>Screen</dt><dd>${s.locked ? "locked" : "unlocked"}</dd>
      <dt>Awake request</dt><dd>${idle.inhibited ? "an app keeps the screen on" : "none"}</dd>
      <dt>Tracking</dt><dd>${s.paused ? (s.paused_until_ms ? `paused until ${hm(s.paused_until_ms)}` : "paused") : "on"}</dd>
    </dl>`;
    const src = s.sources || {};
    const ext = src.browser_extension || [];
    const browsers = ext.length || src.plasma_browser_integration
      ? `<ul class="list">${ext
          .map((e) => `<li><span class="n">Browser ${e.browser_pid}<em>${e.windows} windows, ${e.tabs} tabs</em></span><span class="v"><span class="pill" data-on="${e.alive}">${e.alive ? "Connected" : "Silent"}</span></span></li>`)
          .join("")}${src.plasma_browser_integration ? `<li><span class="n">Plasma Browser Integration<em>${src.plasma_browser_integration.tabs} tabs</em></span><span class="v">fallback</span></li>` : ""}</ul>`
      : '<p class="empty">No browser extension is connected.</p>';
    const running = (s.running || []).length
      ? `<div class="tags">${s.running.map((r) => `<span>${esc(r)}</span>`).join("")}</div>`
      : '<p class="empty">Nothing else is running.</p>';
    const health = `<dl class="facts">
      <dt>Windows</dt><dd>${src.kwin ? `${src.kwin.windows} from KWin, ${src.kwin.ok ? "up to date" : "stale"}` : "unknown"}</dd>
      <dt>Idle detection</dt><dd>${src.idle && src.idle.ok ? `ext-idle-notify v${src.idle.protocol_version}` : esc((src.idle && src.idle.error) || "unavailable")}</dd>
      <dt>Lock and sleep</dt><dd>${src.logind && src.logind.ok ? "logind connected" : "unavailable"}</dd>
      <dt>Open intervals</dt><dd>${s.open_spans ?? 0}</dd>
      <dt>Running for</dt><dd><span data-since="${s.started_ms}"></span></dd>
      <dt>Version</dt><dd>${esc(s.version || "")}</dd>
    </dl>`;
    return [
      panel("Beside it", visible, { w: "w4", aside: `${(s.visible || []).length} visible` }),
      panel("Terminals", terms, { w: "w4" }),
      panel("AI agents", agents, { w: "w4" }),
      panel("Media", media, { w: "w4" }),
      panel("Input and session", facts, { w: "w4" }),
      panel("Browsers", browsers, { w: "w4" }),
      panel("Running apps", running, { w: "w8", aside: `${(s.running || []).length} open` }),
      panel("Tracker", health, { w: "w4" }),
    ].join("");
  }

  let nowKey = "";
  function renderNow(force) {
    if (currentPage !== "now" || !status) return;
    const hero = document.getElementById("nowhero");
    const grid = document.getElementById("nowgrid");
    if (!hero || !grid) return;
    // Redraw only when something shown changed (the status also carries clocks).
    const { now_ms, server_ms, ...rest } = status;
    const key = JSON.stringify(rest);
    if (!force && key === nowKey) return;
    nowKey = key;
    hero.innerHTML = heroHtml(status);
    grid.innerHTML = nowPanels(status);
    startTicker();
  }

  async function loadNowDay(gen) {
    const box = document.getElementById("nowday");
    if (!box) return;
    let d;
    try {
      d = await get(`/api/day?date=${today()}`);
    } catch {
      return;
    }
    if (stale(gen)) return;
    const t = d.totals;
    const st = d.stats || {};
    box.innerHTML = `<div class="figures">
      <div><span>On screen today</span><b>${dur(t.screen)}</b></div>
      <div><span>Active</span><b>${dur(t.active)}</b></div>
      <div><span>Sessions</span><b>${st.sessions ?? 0}</b></div>
      <div><span>Deep work</span><b>${dur(st.deep_work)}</b></div>
      <div><span>App switches</span><b>${st.switches ?? 0}</b></div>
      <div><span>Agents worked</span><b>${dur(t.agent_busy)}</b></div>
    </div>`;
  }

  function pageNow(gen) {
    nowKey = "";
    paint(head("Now", "Live: updates the moment your focus changes.", '<a class="btn" href="#/timeline">Timeline</a>') + '<div id="nowhero"></div><div class="grid" id="nowgrid"></div><div id="nowday"></div>');
    renderNow(true);
    loadNowDay(gen);
    refreshTimer = setInterval(() => !document.hidden && loadNowDay(renderGen), 60000);
  }

  // ---------- Today ----------

  function nowStripHtml(s) {
    if (!s || s.viewer) return "";
    const [st, label] = stateOf(s);
    const [app, title, details] = focusParts(s.focus);
    const line = [...details, title].filter(Boolean).join(", ");
    return `<a class="nowstrip" href="#/now" id="nowstrip"><span class="state" data-s="${st}">${esc(label)}</span>
      <span class="what"><b>${esc(app)}</b>${line ? `<span>${esc(line)}</span>` : ""}</span>
      ${s.focus ? `<span class="timer small" data-since="${s.focus_since_ms}" data-ms="1"></span>` : ""}</a>`;
  }
  function renderNowStrip() {
    const el = document.getElementById("nowstrip");
    if (!el || !status) return;
    const html = nowStripHtml(status);
    const tmp = document.createElement("div");
    tmp.innerHTML = html;
    const next = tmp.firstElementChild;
    if (next && el.outerHTML !== next.outerHTML) el.replaceWith(next);
    startTicker();
  }

  function kpisHtml(d) {
    const t = d.totals;
    const st = d.stats || {};
    const goal = st.goal_secs || 0;
    const goalBar = goal
      ? `<div class="goal" data-over="${t.screen > goal}" title="Daily goal ${dur(goal)}"><i style="--w:${Math.min(100, (t.screen / goal) * 100).toFixed(1)}%"></i></div><em>${t.screen > goal ? `${dur(t.screen - goal)} over` : `${dur(goal - t.screen)} left`} of your ${dur(goal)} goal</em>`
      : "";
    const first = st.first_ms ? hm(st.first_ms) : "–";
    const last = st.last_ms ? `until ${hm(st.last_ms)}` : "";
    return `<div class="kpis">
      <div class="kpi lead"><span>On screen</span><b>${durBig(t.screen)}</b>${goalBar}</div>
      <div class="kpi"><span>Active</span><b>${dur(t.active)}</b></div>
      <div class="kpi"><span>Watching</span><b>${dur(t.passive)}</b></div>
      <div class="kpi"><span>Calls</span><b>${dur(t.call)}</b></div>
      <div class="kpi"><span>Sessions</span><b>${st.sessions ?? 0}</b><em>${st.switches ?? 0} app switches</em></div>
      <div class="kpi"><span>Deep work</span><b>${dur(st.deep_work)}</b><em>${st.deep_sessions ?? 0} long sessions</em></div>
      <div class="kpi"><span>First activity</span><b>${first}</b><em>${last}</em></div>
    </div>`;
  }

  function focusFacts(d) {
    const st = d.stats || {};
    if (!st.sessions) return '<p class="empty">No sessions yet.</p>';
    return `<dl class="facts">
      <dt>Longest stretch</dt><dd>${dur(st.longest)} in ${esc(st.longest_app)}</dd>
      <dt>Average session</dt><dd>${dur(st.avg_session)}</dd>
      <dt>Busiest hour</dt><dd>${st.peak_hour ? esc(hourLabel(st.peak_hour)) : "–"}</dd>
      <dt>First activity</dt><dd>${st.first_ms ? clock(st.first_ms, true) : "–"}</dd>
      <dt>Last activity</dt><dd>${st.last_ms ? clock(st.last_ms, true) : "–"}</dd>
      <dt>Visible beside focus</dt><dd>${dur(d.totals.visible)}</dd>
      <dt>Background audio</dt><dd>${dur(d.totals.background_audio)}</dd>
    </dl>`;
  }

  async function pageDay(date, quiet, gen) {
    currentDay = date;
    const isToday = date === today();
    let d;
    try {
      d = await get(`/api/day?date=${encodeURIComponent(date)}`);
    } catch (e) {
      if (!stale(gen) && !quiet) paint(errorHtml(e));
      return;
    }
    if (stale(gen) || currentDay !== date) return;
    learnCats(d.categories);
    const usedNames = new Set(d.apps.filter((a) => a.total >= 60).map((a) => a.name));
    const unused = d.running.filter((r) => !usedNames.has(r.name));
    const t = d.totals;
    const y = window.scrollY;
    const keep = focusKey();
    const title = isToday ? "Today" : esc(dayLabel(date, { weekday: "long" }));
    paint(`
      ${head(title, esc(dayLabel(date, { day: "numeric", month: "long", year: "numeric" })), dayNav(date, "today"))}
      ${isToday ? nowStripHtml(status) : ""}
      ${kpisHtml(d)}
      <section class="panel w12" aria-label="The day in five-minute bars">
        <header><h2>Your day</h2><div class="legend">
          <span><i class="key"></i>Active<b>${dur(t.active)}</b></span>
          <span><i class="key p"></i>Watching<b>${dur(t.passive)}</b></span>
          <span><i class="key c"></i>Calls<b>${dur(t.call)}</b></span>
          <span><i class="key i"></i>Idle<b>${dur(t.idle_removed)}</b></span>
        </div></header>
        ${ribbonHtml(d, isToday)}
      </section>
      ${t.screen === 0 ? `<p class="empty">${isToday ? "Nothing counted yet today. This page fills in as you work." : "Nothing was recorded on this day."}</p>` : ""}
      <div class="grid">
        ${panel("Categories", catsHtml(d.categories), { w: "w5" })}
        ${panel("Apps", rowsHtml(d.apps, "app", { moreLink: "#/apps/1", empty: "No apps used yet." }), { w: "w7", href: "#/apps/1", aside: "All apps" })}
        ${panel("Sites", rowsHtml(d.sites, "site", { moreLink: "#/apps/1", empty: "No browsing yet." }), { w: "w7", sub: "Each subdomain is its own row. Open one to see page titles." })}
        ${panel("Projects", projectsHtml(d.projects, { max: 6, empty: "No coding yet today." }), { w: "w5", href: `#/projects/1`, aside: "All projects" })}
        ${panel("Focus", focusFacts(d), { w: "w4", href: isToday ? "#/timeline" : `#/timeline/${date}`, aside: "Sessions" })}
        ${panel("AI agents", agentsHtml(d.agents, "No AI agents ran on this day."), { w: "w4", sub: "You: time their terminal had your focus. Agent: time it worked, wherever you were." })}
        ${panel("In the terminal", rowsHtml(d.programs, "prog", { program: true, noTitles: true, max: 6, empty: "No terminal programs yet." }), { w: "w4" })}
        ${panel("Background listening", plainHtml(d.background_audio, "Nothing played in the background."), { w: "w6", sub: "Audio while you looked at something else. Not screen time." })}
        ${panel("Running, not used", plainHtml(unused, "Every running app was used.", { max: 12 }), { w: "w6", sub: "Open in the background or tray, never counted as screen time." })}
      </div>`);
    if (quiet) window.scrollTo(0, y);
    bindRibbon(d);
    bindDayNav();
    startTicker();
    if (keep) view.querySelector(keep)?.focus({ preventScroll: true });
    if (isToday && !quiet) {
      clearInterval(refreshTimer);
      refreshTimer = setInterval(refreshToday, 60000);
    }
  }

  function refreshToday() {
    if (document.hidden || !currentDay) return;
    // Past midnight the page for "today" is yesterday's: route again to show the new day.
    if (currentDay !== today()) route();
    else if (currentPage === "today") pageDay(currentDay, true, renderGen);
    else if (currentPage === "timeline") pageTimeline(currentDay, true, renderGen);
  }

  // ---------- Timeline ----------

  let tlFilter = "all";
  let tlQuery = "";
  let tlShort = false; // sessions under 10 s are hidden unless asked for
  const SHORT_SECS = 10;
  const TL_FILTERS = [
    ["all", "All"],
    ["app", "Apps"],
    ["site", "Sites"],
    ["code", "Coding"],
  ];
  function tlMatch(s) {
    if (!tlShort && s.counted < SHORT_SECS) return false;
    if (tlFilter === "app" && s.kind !== "app") return false;
    if (tlFilter === "site" && s.kind !== "site") return false;
    if (tlFilter === "code" && s.kind !== "terminal" && s.kind !== "editor") return false;
    if (!tlQuery) return true;
    const q = tlQuery.toLowerCase();
    return [s.app_name, s.detail, s.title, s.project, s.category].some((x) => String(x || "").toLowerCase().includes(q));
  }
  function sessHtml(s) {
    const id = `sess:${s.start_ms}`;
    const open = openRows.has(id);
    const c = safeColor(s.color, s.category);
    const tot = Math.max(1, s.counted);
    const w = (x) => `${((x / tot) * 100).toFixed(2)}%`;
    const line = [s.detail, s.project ? `project ${s.project}` : "", s.title].filter(Boolean).join(", ");
    return `<button type="button" class="sess" data-id="${esc(id)}" aria-expanded="${open}" style="--c:${c}">
        <span class="t">${clockMs(s.start_ms)} <span class="to">to</span> ${clockMs(s.end_ms)}</span>
        <span class="w"><b>${esc(s.app_name)}</b>${line ? `<span>${esc(line)}</span>` : ""}</span>
        <span class="d">${dur(s.counted)}</span>
        <span class="mix" aria-hidden="true"><i style="--w:${w(s.active)};--c:${c}"></i><i class="p" style="--w:${w(s.passive)};--c:${c}"></i><i class="k" style="--w:${w(s.call)};--c:${c}"></i></span>
      </button>
      <div class="sess-x"><div><dl class="facts">
        <dt>Started</dt><dd>${clockMs(s.start_ms)}</dd>
        <dt>Ended</dt><dd>${clockMs(s.end_ms)}</dd>
        <dt>From first to last</dt><dd>${durExact(s.end_ms - s.start_ms)}</dd>
        <dt>Counted</dt><dd>${dur(s.counted)}: active ${dur(s.active)}, watching ${dur(s.passive)}, calls ${dur(s.call)}</dd>
        <dt>Category</dt><dd>${esc(s.category || "Other")}</dd>
        ${s.project ? `<dt>Project</dt><dd>${esc(s.project)}</dd>` : ""}
        ${s.title ? `<dt>Window</dt><dd>${esc(s.title)}</dd>` : ""}
      </dl></div></div>`;
  }
  function tlListHtml(d) {
    const all = d.sessions || [];
    const list = all.filter(tlMatch);
    const short = all.filter((s) => s.counted < SHORT_SECS).length;
    const note = document.getElementById("tlnote");
    if (note) note.textContent = !tlShort && short ? `${short} sessions under ${SHORT_SECS} seconds are hidden.` : "";
    if (!list.length) return `<p class="empty">${all.length ? "No session matches." : "No sessions on this day."}</p>`;
    const groups = new Map();
    for (const s of list) {
      const h = new Date(s.start_ms);
      h.setMinutes(0, 0, 0);
      const k = h.getTime();
      if (!groups.has(k)) groups.set(k, []);
      groups.get(k).push(s);
    }
    return [...groups.entries()]
      .map(([k, ss]) => `<div class="tl-hour"><span>${esc(hm(k))}</span><span>${dur(ss.reduce((a, s) => a + s.counted, 0))}</span></div>${ss.map(sessHtml).join("")}`)
      .join("");
  }
  let tlDay = null;
  async function pageTimeline(date, quiet, gen) {
    currentDay = date;
    const isToday = date === today();
    let d;
    try {
      d = await get(`/api/day?date=${encodeURIComponent(date)}`);
    } catch (e) {
      if (!stale(gen) && !quiet) paint(errorHtml(e));
      return;
    }
    if (stale(gen) || currentDay !== date) return;
    tlDay = d;
    learnCats(d.categories);
    const st = d.stats || {};
    const y = window.scrollY;
    const keep = focusKey();
    paint(`
      ${head("Timeline", esc(dayLabel(date, { weekday: "long", day: "numeric", month: "long" })), dayNav(date, "timeline"))}
      <div class="figures">
        <div><span>Sessions</span><b>${st.sessions ?? 0}</b></div>
        <div><span>On screen</span><b>${dur(d.totals.screen)}</b></div>
        <div><span>Deep work</span><b>${dur(st.deep_work)}</b></div>
        <div><span>Longest stretch</span><b>${dur(st.longest)}</b></div>
        <div><span>App switches</span><b>${st.switches ?? 0}</b></div>
      </div>
      <div class="tl-tools">
        <div class="chips" role="group" aria-label="Show">${TL_FILTERS.map(([v, l]) => `<button type="button" class="chip" data-tl="${v}" aria-pressed="${tlFilter === v}">${l}</button>`).join("")}
          <button type="button" class="chip" id="tlshort" aria-pressed="${tlShort}">Short ones</button></div>
        <input type="search" id="tlq" placeholder="Search apps, sites, titles" value="${esc(tlQuery)}" aria-label="Search sessions">
      </div>
      <p class="sub">Each row is a stretch on one app, site or project, timed to the millisecond. Breaks under ${dur(prefs().insights.session_gap_secs || 60)} don't split a session. <span id="tlnote"></span></p>
      <div class="tl" id="tl"></div>`);
    document.getElementById("tl").innerHTML = tlListHtml(d);
    if (quiet) window.scrollTo(0, y);
    bindDayNav();
    view.querySelectorAll("[data-tl]").forEach((b) =>
      b.addEventListener("click", () => {
        tlFilter = b.dataset.tl;
        view.querySelectorAll("[data-tl]").forEach((x) => x.setAttribute("aria-pressed", String(x === b)));
        document.getElementById("tl").innerHTML = tlListHtml(tlDay);
      }),
    );
    document.getElementById("tlshort").addEventListener("click", (e) => {
      tlShort = !tlShort;
      e.currentTarget.setAttribute("aria-pressed", String(tlShort));
      document.getElementById("tl").innerHTML = tlListHtml(tlDay);
    });
    document.getElementById("tlq").addEventListener("input", (e) => {
      tlQuery = e.target.value.trim();
      document.getElementById("tl").innerHTML = tlListHtml(tlDay);
    });
    if (keep) view.querySelector(keep)?.focus({ preventScroll: true });
    if (isToday && !quiet) {
      clearInterval(refreshTimer);
      refreshTimer = setInterval(refreshToday, 60000);
    }
  }

  // ---------- ranges ----------

  const RANGES = [
    ["1", "Today"],
    ["7", "7 days"],
    ["30", "30 days"],
    ["365", "12 months"],
  ];
  function rangeBar(current, ranges = RANGES) {
    return `<div class="seg" role="group" aria-label="Period">${ranges
      .map(([v, label]) => `<button type="button" data-range="${v}" aria-pressed="${v === current}">${label}</button>`)
      .join("")}</div>`;
  }
  function bindRange(page) {
    view.querySelectorAll("[data-range]").forEach((b) =>
      b.addEventListener("click", () => (location.hash = `#/${page}/${b.dataset.range}`)),
    );
  }
  async function loadRange(n) {
    const to = today();
    if (n === "1") {
      const d = await get(`/api/day?date=${to}`);
      return { ...d, days: [{ date: to, totals: d.totals, categories: d.categories, projects: d.projects }] };
    }
    const from = addDays(to, -(Number(n) - 1));
    return get(`/api/range?from=${from}&to=${to}`);
  }
  const loading = (title, n, ranges) => head(title, "", rangeBar(n, ranges)) + '<p class="empty">Loading…</p>';

  async function pageApps(n, gen) {
    paint(loading("Apps and sites", n));
    bindRange("apps");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      if (!stale(gen)) paint(errorHtml(e));
      return;
    }
    if (stale(gen)) return;
    paint(`${head("Apps and sites", "Screen time is focus time. A second number, when shown, is time a window sat beside others.", rangeBar(n))}
      <div class="grid">
        ${panel("Apps", rowsHtml(r.apps, "app", { max: 60, withVisible: true, empty: "No apps in this period." }), { w: "w6" })}
        ${panel("Sites", rowsHtml(r.sites, "site", { max: 60, withVisible: true, empty: "No browsing in this period." }), { w: "w6" })}
        ${panel("In the terminal", rowsHtml(r.programs, "prog", { max: 40, program: true, noTitles: true, empty: "No terminal programs in this period." }), { w: "w6" })}
        ${panel("Categories", catsHtml(r.categories, { limits: n === "1" }), { w: "w6" })}
      </div>`);
    bindRange("apps");
  }

  async function pageProjects(n, gen) {
    paint(loading("Projects", n));
    bindRange("projects");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      if (!stale(gen)) paint(errorHtml(e));
      return;
    }
    if (stale(gen)) return;
    const list = r.projects || [];
    const sum = (k) => list.reduce((a, p) => a + (p[k] || 0), 0);
    paint(`${head("Projects", "Coding time from your terminals, code editors and AI agents.", rangeBar(n))}
      <div class="figures">
        <div><span>Coding</span><b>${dur(sum("total"))}</b></div>
        <div><span>In editors</span><b>${dur(sum("editor"))}</b></div>
        <div><span>In terminals</span><b>${dur(sum("terminal"))}</b></div>
        <div><span>Agents worked</span><b>${dur(sum("agent_busy"))}</b></div>
        <div><span>Projects</span><b>${list.length}</b></div>
      </div>
      <div class="key-row"><span><i style="background:var(--cat-development, #6f9ee8)"></i>Editor</span><span><i style="background:var(--cat-ai, #b49be8)"></i>Terminal</span><span><i style="background:repeating-linear-gradient(135deg, var(--agent, #b49be8) 0 2px, transparent 2px 4px)"></i>Agent working</span></div>
      <section class="panel w12">${projectsHtml(list, {
        days: n === "1" ? null : r.days,
        empty: "No coding in this period. Projects come from git folders your terminals are in and from code editor window titles (VS Code, Cursor, Zed, JetBrains…). The editor list is in Settings.",
      })}</section>`);
    bindRange("projects");
  }

  async function pageAgents(n, gen) {
    paint(loading("AI agents", n));
    bindRange("agents");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      if (!stale(gen)) paint(errorHtml(e));
      return;
    }
    if (stale(gen)) return;
    const busy = (r.agents || []).reduce((a, x) => a + x.busy, 0);
    const focus = (r.agents || []).reduce((a, x) => a + x.focus, 0);
    paint(`${head("AI agents", "An agent counts as working while it uses CPU or writes to its terminal, focused or not.", rangeBar(n))}
      <div class="figures">
        <div><span>Agents worked</span><b>${dur(busy)}</b></div>
        <div><span>You watched them</span><b>${dur(focus)}</b></div>
        <div><span>Agents</span><b>${new Set((r.agents || []).map((a) => a.tool)).size}</b></div>
      </div>
      <section class="panel w12">${agentsHtml(r.agents, "No AI agents ran in this period. Agents are recognised by name; add more in Settings.")}</section>`);
    bindRange("agents");
  }

  // ---------- trends ----------

  function stackedChart(days, cats) {
    const W = 960;
    const H = 260;
    const top = 16;
    const bottom = 26;
    const left = 40;
    const max = Math.max(3600, ...days.map((d) => d.totals.screen));
    const stepH = max > 10 * 3600 ? 4 : max > 4 * 3600 ? 2 : 1;
    const yMax = Math.ceil(max / 3600 / stepH) * stepH * 3600;
    const y = (s) => H - bottom - (s / yMax) * (H - top - bottom);
    const bw = (W - left) / days.length;
    const order = cats.map((c) => c.name);
    learnCats(cats);
    const color = Object.fromEntries(cats.map((c) => [c.name, safeColor(c.color, c.name)]));
    let svg = "";
    for (let h = 0; h <= yMax / 3600; h += stepH) {
      svg += `<line class="grid" x1="${left}" x2="${W}" y1="${y(h * 3600)}" y2="${y(h * 3600)}"/><text x="${left - 8}" y="${y(h * 3600) + 4}" text-anchor="end">${h}h</text>`;
    }
    days.forEach((d, i) => {
      const x = left + i * bw + bw * 0.18;
      const w = bw * 0.64;
      let acc = 0;
      const byName = Object.fromEntries((d.categories || []).map((c) => [c.name, c.total]));
      for (const name of order) {
        const v = byName[name] || 0;
        if (!v) continue;
        const y1 = y(acc + v);
        const y0 = y(acc);
        svg += `<rect x="${x.toFixed(1)}" y="${y1.toFixed(1)}" width="${w.toFixed(1)}" height="${Math.max(0, y0 - y1).toFixed(1)}" style="fill:${color[name]}" rx="1.5"><title>${esc(dayLabel(d.date, { weekday: "short", day: "numeric", month: "short" }))}: ${esc(name)} ${dur(v)}</title></rect>`;
        acc += v;
      }
      const every = days.length <= 10 ? 1 : 5;
      if (i % every === 0 || i === days.length - 1) {
        const label = days.length <= 10 ? dayLabel(d.date, { weekday: "short" }) : String(parseYmd(d.date).getDate());
        svg += `<text x="${(x + w / 2).toFixed(1)}" y="${H - 8}" text-anchor="middle">${esc(label)}</text>`;
      }
    });
    return `<svg class="chart" viewBox="0 0 ${W} ${H}" role="img" aria-label="Screen time per day by category">${svg}</svg>`;
  }

  function heatmap(days, weekStart) {
    const max = Math.max(1, ...days.map((d) => d.totals.screen));
    const first = parseYmd(days[0].date);
    const startDay = weekStart === "sunday" ? 0 : 1; // getDay(): 0 is Sunday
    const lead = (first.getDay() - startDay + 7) % 7;
    const cells = [];
    for (let i = 0; i < lead; i++) cells.push('<i style="visibility:hidden"></i>');
    for (const d of days) {
      const pct = Math.round((d.totals.screen / max) * 100);
      const bg = d.totals.screen ? `color-mix(in srgb, var(--heat, #5fb4e0) ${Math.max(12, pct)}%, transparent)` : "";
      cells.push(`<i style="${bg ? `background:${bg}` : ""}" title="${esc(dayLabel(d.date, { weekday: "short", day: "numeric", month: "short" }))}: ${dur(d.totals.screen)}"></i>`);
    }
    return `<div class="heat-wrap"><div class="heat" role="img" aria-label="Screen time per day for the last year, one column per week starting ${weekStart === "sunday" ? "Sunday" : "Monday"}">${cells.join("")}</div></div>`;
  }

  async function pageTrends(n, gen) {
    const ranges = RANGES.filter(([v]) => v !== "1");
    paint(loading("Trends", n, ranges));
    bindRange("trends");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      if (!stale(gen)) paint(errorHtml(e));
      return;
    }
    if (stale(gen)) return;
    const tracked = r.days.filter((d) => d.totals.screen > 0);
    const avg = tracked.length ? r.totals.screen / tracked.length : 0;
    const busiest = tracked.reduce((a, d) => (!a || d.totals.screen > a.totals.screen ? d : a), null);
    const watchShare = r.totals.screen ? Math.round((r.totals.passive / r.totals.screen) * 100) : 0;
    const coding = r.days.reduce((a, d) => a + (d.coding || 0), 0);
    paint(`${head("Trends", "", rangeBar(n, ranges))}
      <div class="figures">
        <div><span>Average day</span><b>${dur(avg)}</b></div>
        <div><span>Busiest day</span><b>${busiest ? dur(busiest.totals.screen) : "–"}</b><span>${busiest ? esc(dayLabel(busiest.date, { weekday: "short", day: "numeric", month: "short" })) : ""}</span></div>
        <div><span>Coding</span><b>${dur(coding)}</b></div>
        <div><span>Spent watching</span><b>${watchShare}%</b></div>
        <div><span>Agents worked</span><b>${dur(r.totals.agent_busy)}</b></div>
      </div>
      <section class="panel w12"><header><h2>${n === "365" ? "The year, day by day" : "Screen time per day"}</h2></header>${n === "365" ? heatmap(r.days, r.week_start) : stackedChart(r.days, r.categories)}</section>
      <div class="grid">
        ${panel("Top apps", rowsHtml(r.apps, "tapp", { max: 10, empty: "No apps in this period." }), { w: "w6" })}
        ${panel("Categories", catsHtml(r.categories, { limits: false }), { w: "w6" })}
        ${panel("Top sites", rowsHtml(r.sites, "tsite", { max: 10, empty: "No browsing in this period." }), { w: "w6" })}
        ${panel("Projects", projectsHtml(r.projects, { max: 8, empty: "No coding in this period." }), { w: "w6", href: `#/projects/${n}`, aside: "All projects" })}
      </div>`);
    bindRange("trends");
  }

  // ---------- settings ----------

  const lines = (a) => (a || []).join("\n");
  const unlines = (s) =>
    s
      .split(/[\n,]/)
      .map((x) => x.trim())
      .filter(Boolean);

  // A labelled row; the label points at the first form control and the hint describes it.
  function field(label, hint, control) {
    const m = control.match(/<(?:input|select|textarea)\b[^>]*\bid="([^"]+)"/);
    if (!m) {
      return `<div class="field"><span class="flabel">${esc(label)}${hint ? `<span class="hint">${esc(hint)}</span>` : ""}</span><div>${control}</div></div>`;
    }
    const id = m[1];
    const described = hint ? control.replace(`id="${id}"`, `id="${id}" aria-describedby="${id}-hint"`) : control;
    return `<div class="field"><label for="${id}">${esc(label)}${hint ? `<span class="hint" id="${id}-hint">${esc(hint)}</span>` : ""}</label><div>${described}</div></div>`;
  }
  const num = (id, v, unit, min, max, step = 1) =>
    `<div class="inline"><input type="number" id="${id}" value="${esc(v)}" min="${min}" max="${max}" step="${step}" required><span class="unit">${esc(unit)}</span></div>`;
  const check = (id, v, text) => `<label class="check"><input type="checkbox" id="${id}" ${v ? "checked" : ""}>${esc(text)}</label>`;
  const select = (id, value, options) =>
    `<select id="${id}">${options.map(([v, l]) => `<option value="${v}" ${v === value ? "selected" : ""}>${esc(l)}</option>`).join("")}</select>`;

  function catRow(c, i) {
    const fixed = c.name === "Browsing" || c.name === "Other";
    return `<input type="color" data-cat="${i}" data-k="color" value="${esc(rawColor(c.color))}" aria-label="Colour">
      <input type="text" data-cat="${i}" data-k="name" value="${esc(c.name)}" aria-label="Name" ${fixed ? "readonly" : ""}>
      <textarea data-cat="${i}" data-k="hosts" aria-label="Sites">${esc(lines(c.hosts))}</textarea>
      <textarea data-cat="${i}" data-k="apps" aria-label="Apps">${esc(lines(c.apps))}</textarea>
      <textarea data-cat="${i}" data-k="programs" aria-label="Programs">${esc(lines(c.programs))}</textarea>
      <input type="number" data-cat="${i}" data-k="goal_mins" value="${c.goal_mins || 0}" min="0" max="1440" step="5" aria-label="Daily limit in minutes, 0 for none">
      <button type="button" class="btn" data-delcat="${i}" ${fixed ? "disabled title='Fallback category'" : ""}>Remove</button>`;
  }
  const CAT_HEAD = '<span class="h"></span><span class="h">Name</span><span class="h">Sites</span><span class="h">Apps</span><span class="h">Programs</span><span class="h">Limit (min/day)</span><span class="h"></span>';

  const SECTIONS = [
    ["look", "Look"],
    ["dashboard", "Dashboard"],
    ["rules", "When time counts"],
    ["goals", "Goals and sessions"],
    ["projects", "Projects"],
    ["tracking", "Tracking"],
    ["agents", "AI agents"],
    ["privacy", "Privacy"],
    ["categories", "Categories"],
    ["data", "Data"],
  ];

  async function pageSettings(gen) {
    let s;
    try {
      s = await loadSettings();
    } catch (e) {
      if (!stale(gen)) paint(errorHtml(e));
      return;
    }
    if (stale(gen)) return;
    const c = s.config;
    const d = c.dashboard;
    let cats = c.categories.map((x) => ({ ...x }));
    const tools = c.agents.tools.map((t) => `${t.id} | ${t.name} | ${t.matches.join(", ")}`).join("\n");
    const t0 = today();
    const sec = (id, title, body, sub = "") => `<fieldset id="s-${id}"><legend>${esc(title)}</legend>${sub ? `<p class="sub">${esc(sub)}</p>` : ""}${body}</fieldset>`;
    paint(`${head("Settings", `Saved to ${esc(s.config_path)}`)}
    <div class="settings-wrap">
      <nav class="toc" aria-label="Settings sections">${SECTIONS.map(([id, t]) => `<a href="#/settings" data-sec="${id}">${esc(t)}</a>`).join("")}</nav>
      <form class="settings" id="sform" novalidate>
      ${sec("look", "Look", `
        ${field("Theme and accent", "Applies right away, like the palette button in the sidebar.", '<div class="look-inline" id="lookset"></div>')}
        ${field("Clock", "", select("tfmt", d.time_format, [["24h", "24-hour (14:05)"], ["12h", "12-hour (2:05 pm)"]]))}
        ${field("Milliseconds", "On the live timers in Now and Today.", check("showms", d.show_ms, "Show milliseconds"))}
        ${field("Density", "", select("density", d.density, [["comfortable", "Comfortable"], ["compact", "Compact"]]))}
        ${field("Motion", "Animations when pages open and menus appear.", select("motion", d.motion, [["system", "Follow the system setting"], ["reduced", "Reduced"], ["full", "Full"]]))}
      `, "Everything in this section applies and saves as you change it.")}
      ${sec("dashboard", "Dashboard", `
        ${field("Open on", "The page the dashboard shows first.", select("startpage", d.start_page, [["today", "Today"], ["now", "Now"], ["timeline", "Timeline"], ["apps", "Apps and sites"], ["projects", "Projects"], ["agents", "AI agents"], ["trends", "Trends"]]))}
        ${field("Weeks start on", "Used by the year view in Trends.", select("weekstart", c.general.week_start, [["monday", "Monday"], ["sunday", "Sunday"]]))}
        ${field("Port", "The page moves to the new port after saving.", num("port", d.port, "", 1024, 65535))}
        ${field("Tray icon", "Shows today's screen time; click it to open this dashboard.", check("tray", d.tray, "Show the arch-tracker icon in the system tray"))}
        ${field("Custom CSS", "Loaded after the built-in styles. Variables like --bg, --text and --shu are yours to change.", `<textarea id="css" class="code" spellcheck="false">${esc(s.custom_css)}</textarea>`)}
      `)}
      ${sec("rules", "When time counts", `
        ${field("Idle after", "No keyboard or mouse for this long stops the count, back to your last input.", num("idle", c.idle.threshold_secs / 60, "minutes", 0.5, 240, 0.5))}
        ${field("Calls allow", "With the microphone open, you can be silent this long before time stops.", num("call", c.idle.call_threshold_secs / 60, "minutes", 0.5, 480, 0.5))}
        ${field("Watching", "Video or audio playing in a window you can see.", check("media", c.idle.media_keeps_active, "Keeps counting as watching, with no limit"))}
        ${field("Awake requests", "Apps that keep the screen on without detected media.", check("inhib", c.idle.count_inhibited_as_passive, "Count as watching too"))}
      `, "These rules are applied when you look at the data, so changing them updates every past day too.")}
      ${sec("goals", "Goals and sessions", `
        ${field("Daily goal", "Screen time you aim to stay under; Today shows how close you are. 0 turns it off.", num("goal", (c.insights.daily_goal_mins || 0) / 60, "hours", 0, 24, 0.25))}
        ${field("Session break", "A pause shorter than this doesn't end a session in the Timeline.", num("sgap", c.insights.session_gap_secs, "seconds", 5, 3600))}
        ${field("Deep work", "A session at least this long counts as deep work.", num("deep", c.insights.deep_work_mins, "minutes", 5, 480))}
      `, "Per-category limits are in Categories below.")}
      ${sec("projects", "Projects", `
        ${field("Code editors", "Apps whose window titles name the open project. One per line, by app id or name.", `<textarea id="editors">${esc(lines(c.projects.editors))}</textarea>`)}
        ${field("Rename or merge", "One per line as old=new. Old can be a folder (~/code/site) or a name.", `<textarea id="aliases" spellcheck="false">${esc(lines(c.projects.aliases))}</textarea>`)}
        ${field("Project folders", "Folders that hold your projects, like ~/projects. Each folder directly inside counts as a project, git repository or not.", `<textarea id="pfolders" spellcheck="false">${esc(lines(c.projects.folders))}</textarea>`)}
        ${field("Not projects", "Folders to leave out. Outside your project folders, only git repositories count as projects.", `<textarea id="pignore">${esc(lines(c.projects.ignore))}</textarea>`)}
      `, "Terminal time counts for the git repository the shell is in; editor time for the project in the window title.")}
      ${sec("tracking", "Tracking", `
        ${field("Check every", "Running apps, terminals, agents and the microphone.", num("sample", c.general.sample_interval_secs, "seconds", 2, 300))}
        ${field("Save every", "At most this much is lost if the computer crashes.", num("flush", c.general.flush_interval_secs, "seconds", 5, 600))}
        ${field("Running apps", "", check("running", c.general.track_running, "Record apps that are open, even unused"))}
        ${field("Visible windows", "A window beside the focused one counts as visible if this share of it shows.", num("visfrac", Math.round(c.visibility.min_fraction * 100), "% of the window", 1, 100))}
      `)}
      ${sec("agents", "AI agents", `
        ${field("Working when CPU is", "", num("acpu", c.agents.busy_cpu_percent, "% of one core or more", 0.1, 400, 0.1))}
        ${field("…or its builds use", "Processes the agent started, like tests and compilers.", num("akid", c.agents.busy_child_cpu_percent, "% of one core or more", 0.1, 1600, 0.1))}
        ${field("…or it writes", "Output to its terminal per 10 seconds.", num("awrite", c.agents.busy_write_bytes, "bytes or more", 0, 10000000))}
        ${field("Known agents", "One per line: id | name | names to match", `<textarea id="tools" class="code">${esc(tools)}</textarea>`)}
      `)}
      ${sec("privacy", "Privacy", `
        ${field("Never record these apps", "One per line, by name or app id.", `<textarea id="xapps">${esc(lines(c.privacy.exclude_apps))}</textarea>`)}
        ${field("Never record these sites", "One per line. Subdomains are included.", `<textarea id="xhosts">${esc(lines(c.privacy.exclude_hosts))}</textarea>`)}
        ${field("For those", "", select("xmode", c.privacy.exclude_mode, [["hide", "Count the time as Hidden, keep no details"], ["drop", "Record nothing at all"]]))}
      `)}
      ${sec("categories", "Categories", `
        <div class="cat-edit" id="cats">${CAT_HEAD}${cats.map(catRow).join("")}</div>
        <div style="margin-top:12px"><button type="button" class="btn" id="addcat">Add category</button></div>
      `, "Sites match their subdomains too; the most specific site wins. Programs beat sites, sites beat apps. A limit shows each day's time against it.")}
      ${sec("data", "Data", `
        ${field("Keep history", "Older recorded time, with its titles, is deleted within the hour. 0 keeps everything.", num("keep", c.general.keep_days, "days", 0, 36500))}
        ${field("Recompute history", "Trends keeps a summary of each finished day. Recompute them after changes it can't notice, like a folder that became a git repository.", `<div class="inline"><button type="button" class="btn" id="recompute">Recompute</button><span class="msg" id="rmsg"></span></div>`)}
        ${field("Export", "Every recorded interval, for your own analysis.", `<div class="inline"><input type="date" id="exfrom" value="${addDays(t0, -6)}"><input type="date" id="exto" value="${t0}"><select id="exfmt"><option value="csv">CSV</option><option value="json">JSON</option></select><a class="btn" id="exlink" href="#">Download</a></div>`)}
      `)}
      <div class="savebar"><button type="submit" class="btn primary">Save changes</button><span class="msg" id="smsg"></span></div>
      </form>
    </div>`);

    const $ = (id) => document.getElementById(id);
    const msg = (text, kind) => {
      $("smsg").textContent = text;
      $("smsg").dataset.kind = kind || "";
    };
    view.querySelectorAll("[data-sec]").forEach((a) =>
      a.addEventListener("click", (e) => {
        e.preventDefault();
        const t = $(`s-${a.dataset.sec}`);
        t.scrollIntoView({ behavior: motionOK() ? "smooth" : "auto", block: "start" });
        t.querySelector("input, select, textarea, button")?.focus({ preventScroll: true });
      }),
    );
    const exlink = () => {
      $("exlink").href = `/api/export?from=${$("exfrom").value}&to=${$("exto").value}&format=${$("exfmt").value}`;
    };
    ["exfrom", "exto", "exfmt"].forEach((id) => $(id).addEventListener("change", exlink));
    exlink();
    $("recompute").addEventListener("click", async () => {
      const btn = $("recompute");
      btn.disabled = true;
      try {
        const res = await send("POST", "/api/recompute");
        $("rmsg").textContent = `Recomputed: Trends will rebuild ${res.cleared} ${res.cleared === 1 ? "day" : "days"}.`;
        $("rmsg").dataset.kind = "";
      } catch (err) {
        $("rmsg").textContent = err.message;
        $("rmsg").dataset.kind = "error";
      } finally {
        btn.disabled = false;
      }
    });

    lookControls($("lookset"));
    // Display options apply and save at once, like the theme.
    const display = async () => {
      const body = { time_format: $("tfmt").value, show_ms: $("showms").checked, density: $("density").value, motion: $("motion").value };
      root.dataset.density = body.density;
      root.dataset.motion = body.motion;
      Object.assign(prefs().dashboard, body);
      try {
        await send("PUT", "/api/appearance", body);
        msg("Saved.");
      } catch (err) {
        msg(err.message, "error");
      }
    };
    ["tfmt", "showms", "density", "motion"].forEach((id) => $(id).addEventListener("change", display));

    const readCats = () => {
      view.querySelectorAll("[data-cat]").forEach((el) => {
        const i = Number(el.dataset.cat);
        const k = el.dataset.k;
        cats[i][k] = k === "goal_mins" ? Math.max(0, Math.round(Number(el.value) || 0)) : k === "color" || k === "name" ? el.value.trim() : unlines(el.value);
      });
    };
    const drawCats = () => {
      $("cats").innerHTML = CAT_HEAD + cats.map(catRow).join("");
    };
    $("addcat").addEventListener("click", () => {
      readCats();
      cats.splice(cats.length - 2, 0, { name: "New category", color: "#8f98b8", hosts: [], apps: [], programs: [], goal_mins: 0 });
      drawCats();
    });
    $("cats").addEventListener("click", (e) => {
      const b = e.target.closest("[data-delcat]");
      if (!b || b.disabled) return;
      readCats();
      cats.splice(Number(b.dataset.delcat), 1);
      drawCats();
    });

    $("sform").addEventListener("submit", async (e) => {
      e.preventDefault();
      readCats();
      // Check every number field in the page first, and say which one is wrong.
      view.querySelectorAll("[aria-invalid]").forEach((x) => x.removeAttribute("aria-invalid"));
      view.querySelectorAll(".ferr").forEach((x) => x.remove());
      const bad = [...view.querySelectorAll('#sform input[type="number"]')].filter((x) => !x.checkValidity());
      for (const x of bad) markInvalid(x, x.validationMessage);
      if (bad.length) {
        msg(`Fix ${bad.length === 1 ? "the highlighted field" : `the ${bad.length} highlighted fields`} first.`, "error");
        bad[0].focus();
        return;
      }
      const n = (id) => Number($(id).value);
      // Start from the config as it is now, so hand edits to settings this form doesn't show
      // (title patterns, minimum pixels…) are kept.
      let fresh = c;
      try {
        fresh = (await get("/api/settings")).config;
      } catch {
        // Keep the copy loaded with the page.
      }
      const next = structuredClone(fresh);
      next.idle.threshold_secs = Math.round(n("idle") * 60);
      next.idle.call_threshold_secs = Math.round(n("call") * 60);
      next.idle.media_keeps_active = $("media").checked;
      next.idle.count_inhibited_as_passive = $("inhib").checked;
      next.general.sample_interval_secs = n("sample");
      next.general.flush_interval_secs = n("flush");
      next.general.track_running = $("running").checked;
      next.general.week_start = $("weekstart").value;
      next.general.keep_days = n("keep");
      next.visibility.min_fraction = n("visfrac") / 100;
      next.agents.busy_cpu_percent = n("acpu");
      next.agents.busy_child_cpu_percent = n("akid");
      next.agents.busy_write_bytes = n("awrite");
      next.agents.tools = $("tools")
        .value.split("\n")
        .map((l) => l.split("|").map((x) => x.trim()))
        .filter((p) => p[0])
        .map((p) => ({ id: p[0], name: p[1] || p[0], matches: unlines(p[2] || p[0]) }));
      next.privacy.exclude_apps = unlines($("xapps").value);
      next.privacy.exclude_hosts = unlines($("xhosts").value);
      next.privacy.exclude_mode = $("xmode").value;
      next.insights = {
        daily_goal_mins: Math.round(n("goal") * 60),
        session_gap_secs: n("sgap"),
        deep_work_mins: n("deep"),
      };
      next.projects = {
        editors: unlines($("editors").value),
        aliases: $("aliases").value.split("\n").map((x) => x.trim()).filter(Boolean),
        ignore: unlines($("pignore").value),
        folders: unlines($("pfolders").value),
      };
      next.categories = cats.filter((x) => x.name);
      Object.assign(next.dashboard, look(), {
        port: n("port"),
        tray: $("tray").checked,
        start_page: $("startpage").value,
        time_format: $("tfmt").value,
        show_ms: $("showms").checked,
        density: $("density").value,
        motion: $("motion").value,
      });
      msg("Saving…");
      try {
        const res = await send("PUT", "/api/settings", { config: next, custom_css: $("css").value });
        const link = document.getElementById("custom-css");
        link.href = "/custom.css?" + Date.now();
        await loadSettings().catch(() => {});
        if (res.redirect) {
          msg(`Saved. Moving to port ${next.dashboard.port}…`);
          setTimeout(() => (location.href = res.redirect), 700);
          return;
        }
        msg("Saved.");
      } catch (err) {
        msg(err.message, "error");
        const el = err.field && FIELD_INPUT[err.field] && $(FIELD_INPUT[err.field]);
        if (el) {
          markInvalid(el, err.message);
          el.focus();
        }
      }
    });
  }

  // Server field names (config paths) to the inputs that edit them.
  const FIELD_INPUT = {
    "dashboard.port": "port",
    "idle.threshold_secs": "idle",
    "idle.call_threshold_secs": "call",
    "general.sample_interval_secs": "sample",
    "general.flush_interval_secs": "flush",
    "general.keep_days": "keep",
    "visibility.min_fraction": "visfrac",
    "agents.busy_cpu_percent": "acpu",
    "agents.busy_child_cpu_percent": "akid",
    "agents.busy_write_bytes": "awrite",
    "agents.tools": "tools",
    "insights.session_gap_secs": "sgap",
    "insights.deep_work_mins": "deep",
    "insights.daily_goal_mins": "goal",
    "projects.aliases": "aliases",
    "projects.folders": "pfolders",
    custom_css: "css",
  };

  function markInvalid(el, text) {
    el.setAttribute("aria-invalid", "true");
    const id = `${el.id}-err`;
    const span = document.createElement("span");
    span.className = "ferr";
    span.id = id;
    span.textContent = text;
    (el.closest(".inline") || el).after(span);
    el.setAttribute("aria-describedby", [el.getAttribute("aria-describedby"), id].filter(Boolean).join(" "));
  }

  // ---------- live status ----------

  function onStatus(s) {
    status = s;
    if (s.server_ms) clockOffset = s.server_ms - Date.now();
    renderRail();
    renderNotices();
    if (currentPage === "now") renderNow();
    renderNowStrip();
    startTicker();
  }

  async function pollStatus() {
    try {
      onStatus(await get("/api/status"));
    } catch {
      railLive.innerHTML = '<span class="state" data-s="locked">The tracker is not answering</span>';
    }
  }

  // Server-sent events: the tracker pushes its status the moment focus changes. The browser
  // reconnects by itself; a check now and then notices a signed-out session.
  let live = null;
  let lastCheck = 0;
  function connectLive() {
    if (!("EventSource" in window)) {
      setInterval(() => !document.hidden && pollStatus(), 3000);
      return pollStatus();
    }
    live = new EventSource("/api/live");
    live.onmessage = (e) => {
      try {
        onStatus(JSON.parse(e.data));
      } catch {
        // A partial message; the next one replaces it.
      }
    };
    live.onerror = () => {
      railLive.innerHTML = '<span class="state" data-s="locked">Reconnecting</span>';
      if (Date.now() - lastCheck > 10000) {
        lastCheck = Date.now();
        get("/api/status").catch(() => {});
      }
    };
  }

  function renderRail() {
    const s = status;
    const [st, label] = stateOf(s);
    const [app] = focusParts(s && s.focus);
    const html = s && s.viewer
      ? `<span class="state" data-s="paused">Read-only view</span><span class="what">Saved data, nothing is being tracked</span>`
      : `<span class="state" data-s="${st}">${esc(label)}</span><span class="what">${esc(app)}</span>${s && s.focus ? `<span class="clock" data-since="${s.app_since_ms}"></span>` : ""}`;
    const key = `${st}|${label}|${app}|${s && s.app_since_ms}`;
    if (railLive.dataset.key !== key) {
      railLive.dataset.key = key;
      railLive.innerHTML = html;
      railLive.title = s && s.focus ? `${label}: ${focusParts(s.focus).flat().filter(Boolean).join(", ")}` : label;
    }
    const pb = document.getElementById("pausebtn");
    pb.dataset.on = String(!!(s && s.paused));
    pb.title = s && s.paused ? "Paused. Click to resume or change." : "Pause tracking";
  }

  function renderNotices() {
    const s = status;
    if (!s || !s.sources) return;
    const out = [];
    const ext = (s.sources.browser_extension || []).some((e) => e.alive);
    const browserSeen =
      !!s.sources.plasma_browser_integration ||
      /brave|chrom|helium|vivaldi|edge|opera|thorium|firefox|zen|librewolf|floorp|waterfox/i.test((s.focus && s.focus.app) || "");
    const hidden = Number(localStorage.getItem("hide-ext-notice") || 0) > Date.now();
    if (!ext && browserSeen && !hidden) {
      out.push(`<div class="notice"><p>Browser tabs are tracked with a fallback, so two tabs with the same title can be mixed up. Connect the arch-tracker extension for exact tabs (run <code>arch-tracker setup</code> for every step):</p>
        <ol><li>Chromium browsers (Brave, Chrome, Helium, Vivaldi…): open <code>chrome://extensions</code> or <code>brave://extensions</code> and switch on Developer mode.</li>
        <li>Choose Load unpacked and pick <code>${esc(s.extension && s.extension.dir)}</code>; its ID must be <code>${esc(s.extension && s.extension.id)}</code>.</li>
        <li>Firefox, Zen and other Firefox-based browsers: install the signed arch-tracker add-on (.xpi).</li></ol>
        <div class="actions"><button type="button" class="btn" id="hideext">Hide for a week</button></div></div>`);
    }
    if (s.config_error) {
      out.push(`<div class="notice"><p>config.toml has an error, so the last settings that loaded are in use: ${esc(s.config_error)}</p></div>`);
    }
    if (s.dashboard_error) {
      out.push(`<div class="notice"><p>${esc(s.dashboard_error)} The dashboard stays where it is; pick another port in Settings.</p></div>`);
    }
    if (s.sources.idle && !s.sources.idle.ok) {
      out.push(`<div class="notice"><p>Idle detection isn't working, so idle time is being counted. ${esc(s.sources.idle.error || "")}</p></div>`);
    }
    if (s.sources.kwin && !s.sources.kwin.ok) {
      out.push('<div class="notice"><p>KWin hasn\'t reported windows for a few minutes. The tracker reloads its KWin script automatically; if this stays, restart it with systemctl --user restart arch-tracker.</p></div>');
    }
    const html = out.join("");
    if (noticesEl.innerHTML !== html) noticesEl.innerHTML = html;
  }

  // ---------- popovers: pause menu and theme ----------

  // Places a popover next to its trigger: above it in the sidebar, below it in the top bar.
  function place(pop, btn) {
    const r = btn.getBoundingClientRect();
    pop.style.left = "0px";
    pop.style.top = "0px";
    const w = pop.offsetWidth;
    const h = pop.offsetHeight;
    const above = r.top - h - 10 > 8;
    const left = Math.max(8, Math.min(r.left, innerWidth - w - 8));
    pop.style.left = `${left}px`;
    pop.style.top = `${above ? r.top - h - 10 : r.bottom + 10}px`;
    pop.style.setProperty("--origin", `${r.left + r.width / 2 - left}px ${above ? "100%" : "0%"}`);
  }
  const pops = [];
  function popover(btn, pop, fill, onOpen) {
    const open = async (show) => {
      if (show) {
        await fill();
        pops.forEach((p) => p !== pop && closeOf(p)(false));
      }
      pop.hidden = !show;
      btn.setAttribute("aria-expanded", String(show));
      if (show) {
        place(pop, btn);
        popIn(pop);
        onOpen && onOpen();
      }
    };
    pop._open = open;
    pops.push(pop);
    btn.addEventListener("click", () => open(pop.hidden));
    return open;
  }
  const closeOf = (pop) => pop._open;
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    for (const p of pops) {
      if (!p.hidden) {
        p._open(false);
        document.querySelector(`[aria-controls="${p.id}"]`)?.focus();
      }
    }
  });
  document.addEventListener("click", (e) => {
    // A redraw inside a popover detaches the clicked element: that click was inside.
    if (!e.target.isConnected) return;
    for (const p of pops) {
      const btn = document.querySelector(`[aria-controls="${p.id}"]`);
      if (!p.hidden && !p.contains(e.target) && !btn.contains(e.target)) p._open(false);
    }
  });

  const pauseMenu = document.getElementById("pausemenu");
  const pauseOpen = popover(
    document.getElementById("pausebtn"),
    pauseMenu,
    async () => {
      const s = status || {};
      pauseMenu.innerHTML = s.paused
        ? `<button type="button" role="menuitem" data-resume>Resume tracking<span>${s.paused_until_ms ? `paused until ${esc(hm(s.paused_until_ms))}` : "paused"}</span></button>`
        : [
            [30, "Pause for 30 minutes"],
            [60, "Pause for 1 hour"],
            [120, "Pause for 2 hours"],
            ["", "Pause until I resume"],
          ]
            .map(([m, l]) => `<button type="button" role="menuitem" data-pause="${m}">${l}<span>${m ? `until ${esc(hm(Date.now() + m * 60000))}` : ""}</span></button>`)
            .join("");
    },
    () => pauseMenu.querySelector("button")?.focus(),
  );
  pauseMenu.addEventListener("click", async (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    try {
      if (b.matches("[data-resume]")) await send("POST", "/api/resume");
      else await send("POST", "/api/pause", { minutes: b.dataset.pause ? Number(b.dataset.pause) : null });
    } catch (err) {
      pauseMenu.innerHTML = `<p class="sub" style="margin:8px">${esc(err.message)}</p>`;
      return;
    }
    pauseOpen(false);
    document.getElementById("pausebtn").focus();
    pollStatus();
  });
  pauseMenu.addEventListener("keydown", (e) => {
    const items = [...pauseMenu.querySelectorAll("button")];
    const i = items.indexOf(document.activeElement);
    const to = { ArrowDown: i + 1, ArrowUp: i - 1, Home: 0, End: items.length - 1 }[e.key];
    if (to === undefined || !items.length) return;
    e.preventDefault();
    items[(to + items.length) % items.length].focus();
  });

  let palettesCache = null;
  async function palettes() {
    if (!palettesCache) palettesCache = (settings || (await loadSettings())).palettes;
    return palettesCache;
  }

  let lookSaveTimer = null;
  function saveLook() {
    clearTimeout(lookSaveTimer);
    lookSaveTimer = setTimeout(async () => {
      try {
        const r = await send("PUT", "/api/appearance", look());
        applyLook(r.dashboard);
      } catch {
        // Read-only view or a failed save: the look still applies to this page.
      }
    }, 250);
  }

  // Category colours are inline styles, so pages redraw when they switch (not Settings: unsaved edits).
  function redrawForLook() {
    if (currentPage !== "settings") route(true);
  }

  async function lookControls(el) {
    if (!el) return;
    let pals;
    try {
      pals = await palettes();
    } catch (e) {
      el.textContent = e.message;
      return;
    }
    const draw = () => {
      const l = look();
      const p = pals.find((x) => x.id === l.palette) || pals[0];
      const seg = (key, label, values, names) =>
        `<div class="look-row"><span class="look-k" id="lk-${key}-${el.id}">${label}</span><div class="seg" role="radiogroup" aria-labelledby="lk-${key}-${el.id}">${values
          .map((v, i) => `<button type="button" role="radio" data-look="${key}" data-v="${esc(v)}" aria-checked="${l[key] === v}">${esc(names[i])}</button>`)
          .join("")}</div></div>`;
      el.innerHTML = `
        ${seg("palette", "Theme", pals.map((x) => x.id), pals.map((x) => x.name))}
        ${seg("theme", "Mode", ["system", "dark", "light"], ["System", "Dark", "Light"])}
        ${p.dark.length > 1 ? seg("dark_flavor", "Dark flavour", p.dark, p.dark.map((f) => FLAVOR[f] || cap(f))) : ""}
        <div class="look-row"><span class="look-k" id="lk-accent-${el.id}">Accent</span><div class="swatches" role="radiogroup" aria-labelledby="lk-accent-${el.id}">${p.accents
          .map((a) => `<button type="button" role="radio" class="swatch" data-look="accent" data-v="${a}" aria-checked="${l.accent === a}" title="${cap(a)}" aria-label="${cap(a)}" style="--sw:var(--p-${a})"></button>`)
          .join("")}</div></div>
        <label class="check"><input type="checkbox" data-look="theme_categories" ${l.theme_categories ? "checked" : ""}>Category colours follow the theme</label>
        <p class="look-note">Light mode uses ${esc(FLAVOR[p.light] || cap(p.light))}.${p.id === "aizome" ? " Aizome keeps the category colours from Settings." : ""}</p>`;
    };
    draw();
    el.addEventListener("click", (e) => {
      const b = e.target.closest("button[data-look]");
      if (!b) return;
      const l = look();
      const k = b.dataset.look;
      const v = b.dataset.v;
      if (k === "palette" && v !== l.palette) {
        const p = pals.find((x) => x.id === v);
        Object.assign(l, { palette: v, dark_flavor: p.dark[0], accent: p.accents[0] });
      } else l[k] = v;
      applyLook(l);
      draw();
      el.querySelector(`[data-look="${k}"][data-v="${v}"]`)?.focus();
      saveLook();
      if (k === "palette") redrawForLook();
    });
    el.addEventListener("change", (e) => {
      if (!e.target.matches('input[data-look="theme_categories"]')) return;
      applyLook({ ...look(), theme_categories: e.target.checked });
      saveLook();
      redrawForLook();
    });
  }

  const lookPop = document.getElementById("lookpop");
  let lookReady = false;
  popover(
    document.getElementById("lookbtn"),
    lookPop,
    async () => {
      if (!lookReady) {
        lookReady = true;
        await lookControls(lookPop);
      }
    },
    () => lookPop.querySelector('[aria-checked="true"]')?.focus(),
  );

  // ---------- sidebar ----------

  const collapseBtn = document.getElementById("collapse");
  function setCollapsed(on) {
    shell.dataset.collapsed = String(on);
    collapseBtn.setAttribute("aria-pressed", String(on));
    collapseBtn.title = on ? "Expand the sidebar" : "Collapse the sidebar";
    localStorage.setItem("rail-collapsed", on ? "1" : "");
    requestAnimationFrame(moveGlide);
  }
  collapseBtn.addEventListener("click", () => setCollapsed(shell.dataset.collapsed !== "true"));

  // The highlight behind the current page slides there (it appears in place the first time).
  const glide = document.querySelector(".glide");
  function moveGlide() {
    const a = document.querySelector('.nav a[aria-current="page"]');
    if (!a) {
      glide.style.setProperty("--glide-o", "0");
      return;
    }
    glide.style.setProperty("--glide-y", `${a.offsetTop}px`);
    glide.style.setProperty("--glide-h", `${a.offsetHeight}px`);
    glide.style.setProperty("--glide-o", "1");
    if (!glide.classList.contains("ready")) requestAnimationFrame(() => requestAnimationFrame(() => glide.classList.add("ready")));
  }
  addEventListener("resize", moveGlide);

  // The skip link moves focus without changing the hash (which would re-route).
  document.getElementById("skip")?.addEventListener("click", (e) => {
    e.preventDefault();
    view.focus();
  });

  // ---------- router ----------

  const PAGES = ["now", "today", "timeline", "apps", "projects", "agents", "trends", "settings"];
  function route(sameView) {
    clearInterval(refreshTimer);
    refreshTimer = null;
    currentDay = null;
    const gen = ++renderGen;
    const start = prefs().dashboard.start_page || "today";
    const h = location.hash.replace(/^#\/?/, "") || start;
    const [page, arg] = h.split("/");
    const key = page === "day" ? "today" : PAGES.includes(page) ? page : "today";
    if (key !== currentPage && !sameView) enterNext = true;
    currentPage = key;
    document.querySelectorAll(".nav a").forEach((a) => {
      if (a.dataset.route === key) a.setAttribute("aria-current", "page");
      else a.removeAttribute("aria-current");
    });
    moveGlide();
    document.title = `${cap(key === "apps" ? "Apps and sites" : key)}: arch-tracker`;
    const date = /^\d{4}-\d{2}-\d{2}$/.test(arg || "") ? arg : today();
    const valid = (r, list) => (list.some(([v]) => v === r) ? r : null);
    switch (page) {
      case "now":
        return pageNow(gen);
      case "day":
        return pageDay(date, false, gen);
      case "timeline":
        return pageTimeline(date, false, gen);
      case "apps":
        return pageApps(valid(arg, RANGES) || "1", gen);
      case "projects":
        return pageProjects(valid(arg, RANGES) || "7", gen);
      case "agents":
        return pageAgents(valid(arg, RANGES) || "7", gen);
      case "trends":
        return pageTrends(valid(arg, RANGES.slice(1)) || "30", gen);
      case "settings":
        return pageSettings(gen);
      default:
        return pageDay(today(), false, gen);
    }
  }

  // Keys: 1-8 open the pages in sidebar order; on day pages, arrows move a day and t goes to today.
  document.addEventListener("keydown", (e) => {
    if (e.target.closest("input, textarea, select, [contenteditable], #ribbon, .pop") || e.altKey || e.ctrlKey || e.metaKey) return;
    const i = "12345678".indexOf(e.key);
    if (i >= 0) {
      location.hash = `#/${PAGES[i]}`;
      return;
    }
    const daily = currentPage === "today" || currentPage === "timeline";
    if (!daily || !currentDay) return;
    const base = currentPage === "today" ? "day" : "timeline";
    const home = currentPage === "today" ? "#/today" : "#/timeline";
    if (e.key === "ArrowLeft") location.hash = `#/${base}/${addDays(currentDay, -1)}`;
    else if (e.key === "ArrowRight" && currentDay < today()) {
      const next = addDays(currentDay, 1);
      location.hash = next === today() ? home : `#/${base}/${next}`;
    } else if (e.key === "t") location.hash = home;
  });

  bindRows(view);
  addEventListener("hashchange", () => route());
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) return;
    startTicker();
    if (refreshTimer) refreshToday();
  });
  setCollapsed(localStorage.getItem("rail-collapsed") === "1");
  loadSettings()
    .catch(() => null)
    .then(() => {
      route();
      connectLive();
    });
})();
