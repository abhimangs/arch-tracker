"use strict";
// arch-tracker dashboard. Plain JS, no build step. Every string from the tracker (titles, hosts,
// app names) is escaped before it touches the DOM: page titles are untrusted input.

(() => {
  const TOKEN = document.querySelector('meta[name="csrf"]').content;
  const view = document.getElementById("view");
  const nowEl = document.getElementById("now");
  const noticesEl = document.getElementById("notices");
  const NB = " ";
  let status = null;
  let firstRibbon = true;
  let refreshTimer = null; // the one Today refresh; only route() and a full Today render touch it
  let currentDay = null;
  // Bumped on every navigation. A page render that awaited something checks it afterwards and
  // drops its result if the user has moved on, so a slow reply can't overwrite a newer page.
  let renderGen = 0;
  const stale = (gen) => gen !== renderGen;
  const openRows = new Set();

  // ---------- helpers ----------

  const esc = (v) =>
    String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  const pad = (n) => String(n).padStart(2, "0");
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
  const hm = (ms) => {
    const d = new Date(ms);
    return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  };
  const dayLabel = (s, opts) =>
    parseYmd(s).toLocaleDateString(undefined, opts || { weekday: "long", day: "numeric", month: "long" });
  const rawColor = (c) => (/^#[0-9a-fA-F]{3,8}$/.test(c || "") ? c : "#6b7392");

  // ---------- theme ----------
  // <html> carries the look (data-theme/palette/dark/accent/theme-cats, filled in by the server so
  // there is no flash). themes.css maps it to colours; this code only switches attributes.
  const root = document.documentElement;
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
  /** A category colour: the palette's slot when "follow theme" is on, else the colour from Settings. */
  const safeColor = (c, name) => {
    const hex = rawColor(c);
    if (!themedCats()) return hex;
    const n = name || catByColor.get(hex.toLowerCase());
    return n ? `var(--cat-${slotFor(n)})` : hex;
  };
  const FLAVOR = { mocha: "Mocha", macchiato: "Macchiato", frappe: "Frappé", main: "Main", moon: "Moon", night: "Night", latte: "Latte", dawn: "Dawn", fog: "Fog" };
  const cap = (s) => s.charAt(0).toUpperCase() + s.slice(1);
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

  function dur(secs) {
    secs = Math.max(0, Math.round(secs || 0));
    if (secs < 60) return `${secs}${NB}s`;
    const h = Math.floor(secs / 3600);
    const m = Math.floor(secs / 60) % 60;
    if (h === 0) return `${m}${NB}m`;
    return `${h}${NB}h ${pad(m)}${NB}m`;
  }

  function durBig(secs) {
    secs = Math.max(0, Math.round(secs || 0));
    const h = Math.floor(secs / 3600);
    const m = Math.floor(secs / 60) % 60;
    if (h === 0) return `${m}<small>m</small>`;
    return `${h}<small>h</small>${pad(m)}<small>m</small>`;
  }

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
    if (!r.ok) throw new Error(body.error || `The tracker answered ${r.status}.`);
    return body;
  }

  const errorHtml = (e) =>
    `<p class="empty">Couldn't load this page: ${esc(e.message)}. Check that the tracker is running (systemctl --user status arch-tracker), then reload.</p>`;

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
      detail = parts[1] || "";
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
      rest > 0 && opts.moreLink ? `<p class="more">${rest} more in <a href="${opts.moreLink}">Apps and sites</a>.</p>` : ""
    }`;
  }

  function catsHtml(list) {
    list = (list || []).filter((c) => c.total > 0);
    if (!list.length) return `<p class="empty">No screen time yet.</p>`;
    const total = list.reduce((a, c) => a + c.total, 0) || 1;
    const seg = list.map((c) => `<i style="--w:${((c.total / total) * 100).toFixed(2)}%;--c:${safeColor(c.color)}" title="${esc(c.name)}"></i>`).join("");
    const items = list
      .map((c) => `<li><span class="rn"><i class="sw" style="--c:${safeColor(c.color)}"></i><span>${esc(c.name)}</span></span><span class="d">${Math.round((c.total / total) * 100)}%</span><span>${dur(c.total)}</span></li>`)
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
        <div class="who"><span>${esc(a.name || a.tool)}</span><em>${esc(a.project || "no project")}</em></div>
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

  // ---------- day view ----------

  const STATE_CLASS = { active: "a", passive: "p", call: "k", idle: "i", excluded: "x" };
  const STATE_WORD = { active: "active", passive: "watching", call: "in a call", idle: "idle, not counted", excluded: "locked, asleep or paused", "": "nothing recorded" };

  function ribbonHtml(d, isToday) {
    const span = d.end_ms - d.start_ms;
    const bars = d.timeline
      .map((b, i) => {
        const cls = STATE_CLASS[b.state] || "e";
        const counted = cls === "a" || cls === "p" || cls === "k";
        const h = counted ? Math.max(7, Math.min(100, (b.counted / 300) * 100)) : 0;
        return `<i class="${cls}" data-i="${i}" style="--h:${h.toFixed(1)}%;--c:${safeColor(b.color)};--n:${i}"></i>`;
      })
      .join("");
    let needle = "";
    if (isToday) {
      const pct = ((Date.now() - d.start_ms) / span) * 100;
      if (pct >= 0 && pct <= 100) needle = `<span class="needle" style="left:${pct.toFixed(2)}%" title="Now"></span>`;
    }
    // Labels come from the server's local-clock hours: a day when the clocks change has 23 or 25.
    const marks = d.hours && d.hours.length ? d.hours : Array.from({ length: 24 }, (_, h) => ({ start_ms: d.start_ms + h * 3600000, label: `${pad(h)}:00` }));
    const seen = new Set();
    const hours = marks
      .filter((h) => /^\d\d:00$/.test(h.label) && Number(h.label.slice(0, 2)) % 3 === 0 && !seen.has(h.label) && seen.add(h.label))
      .map((h) => `<span style="left:${(((h.start_ms - d.start_ms) / span) * 100).toFixed(3)}%">${esc(h.label)}</span>`);
    hours.push('<span style="left:100%">24:00</span>');
    const label = `Day ribbon: ${dur(d.totals.screen)} on screen, in five-minute bars.`;
    const rise = firstRibbon ? " rise" : "";
    firstRibbon = false;
    return `<div class="ribbon${rise}" id="ribbon" role="img" aria-label="${esc(label)}">${bars}${needle}</div>
      <div class="hours" aria-hidden="true">${hours.join("")}</div>
      <p class="readout" id="readout" aria-live="polite">Point at a bar to see what you were doing.</p>`;
  }

  function bindRibbon(d) {
    const rib = document.getElementById("ribbon");
    const out = document.getElementById("readout");
    if (!rib || !out) return;
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
      out.innerHTML = `<b>${hm(b.start_ms)}–${hm(b.start_ms + 300000)}</b>${what}<span>${esc(STATE_WORD[b.state] ?? "")}</span>`;
    };
    rib.addEventListener("pointermove", (e) => {
      const t = e.target.closest("i[data-i]");
      if (t) show(Number(t.dataset.i));
    });
    rib.addEventListener("pointerleave", () => {
      if (last) last.classList.remove("hover");
      last = null;
      out.textContent = "Point at a bar to see what you were doing.";
    });
  }

  function bindRows(root) {
    root.addEventListener("click", (e) => {
      const btn = e.target.closest(".rh[aria-expanded]");
      if (!btn) return;
      const row = btn.closest(".row");
      const open = row.dataset.open !== "true";
      row.dataset.open = String(open);
      btn.setAttribute("aria-expanded", String(open));
      if (open) openRows.add(row.dataset.id);
      else openRows.delete(row.dataset.id);
    });
  }

  async function pageDay(date, quiet, gen) {
    currentDay = date;
    const isToday = date === today();
    let d;
    try {
      d = await get(`/api/day?date=${encodeURIComponent(date)}`);
    } catch (e) {
      if (!stale(gen) && !quiet) view.innerHTML = errorHtml(e);
      return;
    }
    if (stale(gen) || currentDay !== date) return;
    learnCats(d.categories);
    const usedNames = new Set(d.apps.filter((a) => a.total >= 60).map((a) => a.name));
    const unused = d.running.filter((r) => !usedNames.has(r.name));
    const t = d.totals;
    const nothing = t.screen === 0;
    const y = window.scrollY;
    view.innerHTML = `
      <div class="dayhead">
        <h1>${isToday ? "Today, " : ""}${esc(dayLabel(date))}</h1>
        <div class="daynav">
          <button class="btn" type="button" data-go="${addDays(date, -1)}">Previous day</button>
          ${isToday ? "" : `<button class="btn" type="button" data-go="${today()}">Today</button>`}
          <button class="btn" type="button" data-go="${addDays(date, 1)}" ${isToday ? "disabled" : ""}>Next day</button>
        </div>
      </div>
      <p class="total">${durBig(t.screen)}<span class="of">on screen</span></p>
      <div class="legend">
        <span><i class="key"></i>Active <b>${dur(t.active)}</b></span>
        <span><i class="key p"></i>Watching <b>${dur(t.passive)}</b></span>
        <span><i class="key c"></i>Calls <b>${dur(t.call)}</b></span>
        <span><i class="key i"></i>Idle, not counted <b>${dur(t.idle_removed)}</b></span>
      </div>
      ${ribbonHtml(d, isToday)}
      ${nothing ? `<p class="empty">${isToday ? "Nothing counted yet today. This page refreshes every minute." : "Nothing was recorded on this day."}</p>` : ""}
      <div class="cols">
        <div>
          <section><h2>Apps</h2>${rowsHtml(d.apps, "app", { moreLink: "#/apps/1", empty: "No apps used yet." })}</section>
          <section><h2>Sites</h2><p class="sub">Each subdomain is its own row. Click one to see page titles.</p>${rowsHtml(d.sites, "site", { moreLink: "#/apps/1", empty: "No browsing yet." })}</section>
          <section><h2>In the terminal</h2><p class="sub">Programs you had in front of you, by project.</p>${rowsHtml(d.programs, "prog", { program: true, noTitles: true, empty: "No terminal programs yet." })}</section>
        </div>
        <div>
          <section><h2>Categories</h2>${catsHtml(d.categories)}</section>
          <section><h2>AI agents</h2><p class="sub">You: time their terminal had your focus. Agent: time it was working, wherever you were.</p>${agentsHtml(d.agents, "No AI agents ran on this day.")}</section>
          <section><h2>Background listening</h2><p class="sub">Audio while you looked at something else. Not screen time.</p>${plainHtml(d.background_audio, "Nothing played in the background.")}</section>
          <section><h2>Running, not used</h2><p class="sub">Open in the background or tray, never counted as screen time.</p>${plainHtml(unused, "Every running app was used.", { max: 12 })}</section>
        </div>
      </div>`;
    if (quiet) window.scrollTo(0, y);
    bindRibbon(d);
    view.querySelectorAll("[data-go]").forEach((b) =>
      b.addEventListener("click", () => {
        const go = b.dataset.go;
        location.hash = go === today() ? "#/today" : `#/day/${go}`;
      }),
    );
    if (isToday && !quiet) {
      clearInterval(refreshTimer);
      refreshTimer = setInterval(refreshToday, 60000);
    }
  }

  function refreshToday() {
    if (document.hidden || !currentDay) return;
    // Past midnight the page for "today" is yesterday's: route again to show the new day.
    if (currentDay !== today()) route();
    else pageDay(currentDay, true, renderGen);
  }

  // ---------- ranges ----------

  const RANGES = [
    ["1", "Today"],
    ["7", "7 days"],
    ["30", "30 days"],
    ["365", "12 months"],
  ];

  function rangeBar(page, current, ranges = RANGES) {
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
      return { ...d, days: [{ date: to, totals: d.totals, categories: d.categories }] };
    }
    const from = addDays(to, -(Number(n) - 1));
    return get(`/api/range?from=${from}&to=${to}`);
  }

  async function pageApps(n, gen) {
    view.innerHTML = `<div class="toolbar"><h1>Apps and sites</h1>${rangeBar("apps", n)}</div><p class="empty">Loading…</p>`;
    bindRange("apps");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      if (!stale(gen)) view.innerHTML = errorHtml(e);
      return;
    }
    if (stale(gen)) return;
    view.innerHTML = `<div class="toolbar"><h1>Apps and sites</h1>${rangeBar("apps", n)}</div>
      <p class="sub">Screen time is focus time. A row's second number, when shown, is time it sat beside other windows.</p>
      <div class="cols">
        <div>
          <section><h2>Apps</h2>${rowsHtml(r.apps, "app", { max: 60, withVisible: true, empty: "No apps in this period." })}</section>
          <section><h2>In the terminal</h2>${rowsHtml(r.programs, "prog", { max: 40, program: true, noTitles: true, empty: "No terminal programs in this period." })}</section>
        </div>
        <div>
          <section><h2>Sites</h2>${rowsHtml(r.sites, "site", { max: 60, withVisible: true, empty: "No browsing in this period." })}</section>
        </div>
      </div>`;
    bindRange("apps");
  }

  async function pageAgents(n, gen) {
    view.innerHTML = `<div class="toolbar"><h1>AI agents</h1>${rangeBar("agents", n)}</div><p class="empty">Loading…</p>`;
    bindRange("agents");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      if (!stale(gen)) view.innerHTML = errorHtml(e);
      return;
    }
    if (stale(gen)) return;
    const busy = (r.agents || []).reduce((a, x) => a + x.busy, 0);
    const focus = (r.agents || []).reduce((a, x) => a + x.focus, 0);
    view.innerHTML = `<div class="toolbar"><h1>AI agents</h1>${rangeBar("agents", n)}</div>
      <div class="figures">
        <div><span>Agents worked</span><b>${dur(busy)}</b></div>
        <div><span>You watched them</span><b>${dur(focus)}</b></div>
      </div>
      <p class="sub">An agent counts as working while its process uses CPU or writes to its terminal, focused or not. Thresholds are in Settings.</p>
      <section>${agentsHtml(r.agents, "No AI agents ran in this period. Agents are recognised by name; add more in Settings.")}</section>`;
    bindRange("agents");
  }

  // ---------- trends ----------

  function stackedChart(days, cats) {
    const W = 960;
    const H = 240;
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
        svg += `<rect x="${x.toFixed(1)}" y="${y1.toFixed(1)}" width="${w.toFixed(1)}" height="${Math.max(0, y0 - y1).toFixed(1)}" fill="${color[name]}" rx="1.5"><title>${esc(dayLabel(d.date, { weekday: "short", day: "numeric", month: "short" }))}: ${esc(name)} ${dur(v)}</title></rect>`;
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
    view.innerHTML = `<div class="toolbar"><h1>Trends</h1>${rangeBar("trends", n, ranges)}</div><p class="empty">Loading…</p>`;
    bindRange("trends");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      if (!stale(gen)) view.innerHTML = errorHtml(e);
      return;
    }
    if (stale(gen)) return;
    const tracked = r.days.filter((d) => d.totals.screen > 0);
    const avg = tracked.length ? r.totals.screen / tracked.length : 0;
    const busiest = tracked.reduce((a, d) => (!a || d.totals.screen > a.totals.screen ? d : a), null);
    const watchShare = r.totals.screen ? Math.round((r.totals.passive / r.totals.screen) * 100) : 0;
    view.innerHTML = `<div class="toolbar"><h1>Trends</h1>${rangeBar("trends", n, ranges)}</div>
      <div class="figures">
        <div><span>Average day</span><b>${dur(avg)}</b></div>
        <div><span>Busiest day</span><b>${busiest ? dur(busiest.totals.screen) : "–"}</b><span>${busiest ? esc(dayLabel(busiest.date, { weekday: "short", day: "numeric", month: "short" })) : ""}</span></div>
        <div><span>Spent watching</span><b>${watchShare}%</b></div>
        <div><span>Agents worked</span><b>${dur(r.totals.agent_busy)}</b></div>
      </div>
      ${n === "365" ? heatmap(r.days, r.week_start) : stackedChart(r.days, r.categories)}
      <div class="cols">
        <div><section><h2>Top apps</h2>${rowsHtml(r.apps, "tapp", { max: 10, empty: "No apps in this period." })}</section></div>
        <div>
          <section><h2>Categories</h2>${catsHtml(r.categories)}</section>
          <section><h2>Top sites</h2>${rowsHtml(r.sites, "tsite", { max: 10, empty: "No browsing in this period." })}</section>
        </div>
      </div>`;
    bindRange("trends");
  }

  // ---------- settings ----------

  const lines = (a) => (a || []).join("\n");
  const unlines = (s) =>
    s
      .split(/[\n,]/)
      .map((x) => x.trim())
      .filter(Boolean);

  function field(label, hint, control) {
    return `<div class="field"><label>${esc(label)}${hint ? `<span class="hint">${esc(hint)}</span>` : ""}</label><div>${control}</div></div>`;
  }
  const num = (id, v, unit, min, max, step = 1) =>
    `<div class="inline"><input type="number" id="${id}" value="${esc(v)}" min="${min}" max="${max}" step="${step}"><span class="unit">${esc(unit)}</span></div>`;
  const check = (id, v, text) => `<label class="check"><input type="checkbox" id="${id}" ${v ? "checked" : ""}>${esc(text)}</label>`;

  function catRow(c, i) {
    const fixed = c.name === "Browsing" || c.name === "Other";
    return `<input type="color" data-cat="${i}" data-k="color" value="${esc(rawColor(c.color))}" aria-label="Colour">
      <input type="text" data-cat="${i}" data-k="name" value="${esc(c.name)}" aria-label="Name" ${fixed ? "readonly" : ""}>
      <textarea data-cat="${i}" data-k="hosts" aria-label="Sites">${esc(lines(c.hosts))}</textarea>
      <textarea data-cat="${i}" data-k="apps" aria-label="Apps">${esc(lines(c.apps))}</textarea>
      <textarea data-cat="${i}" data-k="programs" aria-label="Programs">${esc(lines(c.programs))}</textarea>
      <button type="button" class="btn" data-delcat="${i}" ${fixed ? "disabled title='Fallback category'" : ""}>Remove</button>`;
  }

  async function pageSettings(gen) {
    let s;
    try {
      s = await get("/api/settings");
    } catch (e) {
      if (!stale(gen)) view.innerHTML = errorHtml(e);
      return;
    }
    if (stale(gen)) return;
    const c = s.config;
    let cats = c.categories.map((x) => ({ ...x }));
    const st = status || {};
    const tools = c.agents.tools.map((t) => `${t.id} | ${t.name} | ${t.matches.join(", ")}`).join("\n");
    const t0 = today();
    view.innerHTML = `<form class="settings" id="sform" novalidate>
      <div class="toolbar"><h1>Settings</h1><span class="msg">Saved to ${esc(s.config_path)}</span></div>

      <fieldset id="pausebox">
        <legend>Pause</legend>
        <div class="field"><span class="label" id="pause-label">Tracking<span class="hint" id="pausestate" role="status">${esc(pauseText(st))}</span></span>
          <div class="inline" role="group" aria-labelledby="pause-label">
            <button type="button" class="btn" data-pause="30">Pause 30 minutes</button>
            <button type="button" class="btn" data-pause="60">Pause 1 hour</button>
            <button type="button" class="btn" data-pause="">Pause until I resume</button>
            <button type="button" class="btn" data-resume ${st.paused ? "" : "disabled"}>Resume</button>
          </div></div>
      </fieldset>

      <fieldset>
        <legend>When time counts</legend>
        <p class="sub">These rules are applied when you look at the data, so changing them updates every past day too.</p>
        ${field("Idle after", "No keyboard or mouse for this long stops the count, back to your last input.", num("idle", c.idle.threshold_secs / 60, "minutes", 0.5, 240, 0.5))}
        ${field("Calls allow", "With the microphone open, you can be silent this long before time stops.", num("call", c.idle.call_threshold_secs / 60, "minutes", 0.5, 480, 0.5))}
        ${field("Watching", "Video or audio playing in a window you can see.", check("media", c.idle.media_keeps_active, "Keeps counting as watching, with no limit"))}
        ${field("Awake requests", "Apps that keep the screen on without detected media.", check("inhib", c.idle.count_inhibited_as_passive, "Count as watching too"))}
      </fieldset>

      <fieldset>
        <legend>Tracking</legend>
        ${field("Check every", "Running apps, terminals, agents and the microphone.", num("sample", c.general.sample_interval_secs, "seconds", 2, 300))}
        ${field("Save every", "At most this much is lost if the computer crashes.", num("flush", c.general.flush_interval_secs, "seconds", 5, 600))}
        ${field("Running apps", "", check("running", c.general.track_running, "Record apps that are open, even unused"))}
        ${field("Visible windows", "A window beside the focused one counts as visible if this share of it shows.", num("visfrac", Math.round(c.visibility.min_fraction * 100), "% of the window", 1, 100))}
      </fieldset>

      <fieldset>
        <legend>AI agents</legend>
        ${field("Working when CPU is", "", num("acpu", c.agents.busy_cpu_percent, "% of one core or more", 0.1, 400, 0.1))}
        ${field("…or its builds use", "Processes the agent started, like tests and compilers.", num("akid", c.agents.busy_child_cpu_percent, "% of one core or more", 0.1, 1600, 0.1))}
        ${field("…or it writes", "Output to its terminal per 10 seconds.", num("awrite", c.agents.busy_write_bytes, "bytes or more", 0, 10000000))}
        ${field("Known agents", "One per line: id | name | names to match", `<textarea id="tools" class="code">${esc(tools)}</textarea>`)}
      </fieldset>

      <fieldset>
        <legend>Privacy</legend>
        ${field("Never record these apps", "One per line, by name or app id.", `<textarea id="xapps">${esc(lines(c.privacy.exclude_apps))}</textarea>`)}
        ${field("Never record these sites", "One per line. Subdomains are included.", `<textarea id="xhosts">${esc(lines(c.privacy.exclude_hosts))}</textarea>`)}
        ${field("For those", "", `<select id="xmode"><option value="hide" ${c.privacy.exclude_mode === "hide" ? "selected" : ""}>Count the time as Hidden, keep no details</option><option value="drop" ${c.privacy.exclude_mode === "drop" ? "selected" : ""}>Record nothing at all</option></select>`)}
      </fieldset>

      <fieldset>
        <legend>Categories</legend>
        <p class="sub">Sites match their subdomains too; the most specific site wins. Programs beat sites, sites beat apps.</p>
        <div class="cat-edit" id="cats">
          <span class="h"></span><span class="h">Name</span><span class="h">Sites</span><span class="h">Apps</span><span class="h">Programs</span><span class="h"></span>
          ${cats.map(catRow).join("")}
        </div>
        <div><button type="button" class="btn" id="addcat">Add category</button></div>
      </fieldset>

      <fieldset>
        <legend>Dashboard</legend>
        ${field("Port", "The page moves to the new port after saving.", num("port", c.dashboard.port, "", 1024, 65535))}
        ${field("Weeks start on", "Used by the year view in Trends.", `<select id="weekstart">${["monday", "sunday"].map((d) => `<option value="${d}" ${c.general.week_start === d ? "selected" : ""}>${d[0].toUpperCase() + d.slice(1)}</option>`).join("")}</select>`)}
        ${field("Look", "Changes apply right away. The palette button at the top right has the same choices.", '<div class="look-inline" id="lookset"></div>')}
        ${field("Tray icon", "Shows today's screen time; click it to open this dashboard.", check("tray", c.dashboard.tray, "Show the arch-tracker icon in the system tray"))}
        ${field("Custom CSS", "Loaded after the built-in styles. Variables like --bg, --text and --shu are yours to change.", `<textarea id="css" class="code" spellcheck="false">${esc(s.custom_css)}</textarea>`)}
      </fieldset>

      <fieldset>
        <legend>Export</legend>
        ${field("Raw data", "Every recorded interval, for your own analysis.", `<div class="inline"><input type="date" id="exfrom" value="${addDays(t0, -6)}"><input type="date" id="exto" value="${t0}"><select id="exfmt"><option value="csv">CSV</option><option value="json">JSON</option></select><a class="btn" id="exlink" href="#">Download</a></div>`)}
      </fieldset>

      <div class="savebar"><button type="submit" class="btn primary">Save changes</button><span class="msg" id="smsg"></span></div>
    </form>`;

    const $ = (id) => document.getElementById(id);
    const msg = (text, kind) => {
      $("smsg").textContent = text;
      $("smsg").dataset.kind = kind || "";
    };
    const exlink = () => {
      $("exlink").href = `/api/export?from=${$("exfrom").value}&to=${$("exto").value}&format=${$("exfmt").value}`;
    };
    ["exfrom", "exto", "exfmt"].forEach((id) => $(id).addEventListener("change", exlink));
    exlink();

    learnCats(c.categories);
    lookControls($("lookset"));
    const readCats = () => {
      view.querySelectorAll("[data-cat]").forEach((el) => {
        const i = Number(el.dataset.cat);
        const k = el.dataset.k;
        cats[i][k] = k === "color" || k === "name" ? el.value.trim() : unlines(el.value);
      });
    };
    const drawCats = () => {
      $("cats").innerHTML =
        '<span class="h"></span><span class="h">Name</span><span class="h">Sites</span><span class="h">Apps</span><span class="h">Programs</span><span class="h"></span>' +
        cats.map(catRow).join("");
    };
    $("addcat").addEventListener("click", () => {
      readCats();
      cats.splice(cats.length - 2, 0, { name: "New category", color: "#8f98b8", hosts: [], apps: [], programs: [] });
      drawCats();
    });
    $("cats").addEventListener("click", (e) => {
      const b = e.target.closest("[data-delcat]");
      if (!b || b.disabled) return;
      readCats();
      cats.splice(Number(b.dataset.delcat), 1);
      drawCats();
    });
    // Pause and resume redraw only this fieldset (through pollStatus and syncPause), so edits
    // elsewhere in the form stay as they are.
    $("pausebox").addEventListener("click", async (e) => {
      const b = e.target.closest("[data-pause], [data-resume]");
      if (!b || b.disabled) return;
      try {
        if (b.matches("[data-resume]")) await send("POST", "/api/resume");
        else await send("POST", "/api/pause", { minutes: b.dataset.pause ? Number(b.dataset.pause) : null });
        await pollStatus();
      } catch (err) {
        msg(err.message, "error");
      }
    });

    $("sform").addEventListener("submit", async (e) => {
      e.preventDefault();
      readCats();
      const n = (id) => Number($(id).value);
      const next = structuredClone(c);
      next.idle.threshold_secs = Math.round(n("idle") * 60);
      next.idle.call_threshold_secs = Math.round(n("call") * 60);
      next.idle.media_keeps_active = $("media").checked;
      next.idle.count_inhibited_as_passive = $("inhib").checked;
      next.general.sample_interval_secs = n("sample");
      next.general.flush_interval_secs = n("flush");
      next.general.track_running = $("running").checked;
      next.general.week_start = $("weekstart").value;
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
      next.categories = cats.filter((x) => x.name);
      next.dashboard.port = n("port");
      Object.assign(next.dashboard, look());
      next.dashboard.tray = $("tray").checked;
      msg("Saving…");
      try {
        const res = await send("PUT", "/api/settings", { config: next, custom_css: $("css").value });
        const link = document.getElementById("custom-css");
        link.href = "/custom.css?" + Date.now();
        if (res.redirect) {
          msg(`Saved. Moving to port ${next.dashboard.port}…`);
          setTimeout(() => (location.href = res.redirect), 700);
          return;
        }
        msg("Saved.");
      } catch (err) {
        msg(err.message, "error");
      }
    });
  }

  // ---------- now strip + notices ----------

  const pauseText = (st) =>
    st.paused ? (st.paused_until_ms ? `Paused until ${hm(st.paused_until_ms)}.` : "Paused until you resume.") : "Tracking is on.";

  // Keeps the Settings page's Pause fieldset in step with the tracker without touching the form.
  function syncPause() {
    const box = document.getElementById("pausebox");
    if (!box || !status) return;
    const text = document.getElementById("pausestate");
    const t = pauseText(status);
    if (text.textContent !== t) text.textContent = t;
    const resume = box.querySelector("[data-resume]");
    const hadFocus = document.activeElement === resume;
    resume.disabled = !status.paused;
    if (hadFocus && resume.disabled) box.querySelector("[data-pause]").focus();
  }

  function stateOf(s) {
    if (s.viewer) return ["paused", "Read-only view"];
    if (s.paused) return ["paused", s.paused_until_ms ? `Paused until ${hm(s.paused_until_ms)}` : "Paused"];
    if (s.locked) return ["locked", "Locked"];
    if (s.idle && s.idle.input_idle && (s.media_visible || []).length) return ["watching", "Watching"];
    if (s.mic) return ["call", "In a call"];
    if (s.idle && s.idle.input_idle) return ["idle", "No input"];
    return ["active", "Active"];
  }

  function describe(f) {
    if (!f) return "Nothing focused";
    const bits = [f.app_name];
    if (f.private) bits.push("private window");
    else if (f.host) bits.push(f.host);
    if (f.program) bits.push(f.project ? `${f.program} in ${f.project}` : f.program);
    else if (!f.host && f.title && !f.private) bits.push(f.title);
    return bits.join(", ");
  }

  async function pollStatus() {
    try {
      status = await get("/api/status");
    } catch {
      nowEl.innerHTML = '<span class="state" data-s="locked">The tracker is not answering</span>';
      return;
    }
    const [s, label] = stateOf(status);
    const what = status.viewer ? "Saved data, nothing is being tracked" : describe(status.focus);
    nowEl.innerHTML = `<span class="state" data-s="${s}">${esc(label)}</span><span class="what" title="${esc(what)}">${esc(what)}</span>${
      status.paused ? '<button type="button" class="btn" id="nowresume">Resume</button>' : ""
    }`;
    const r = document.getElementById("nowresume");
    if (r) r.addEventListener("click", async () => {
      await send("POST", "/api/resume").catch(() => {});
      pollStatus();
    });
    renderNotices();
    syncPause();
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
    const h = document.getElementById("hideext");
    if (h) h.addEventListener("click", () => {
      localStorage.setItem("hide-ext-notice", String(Date.now() + 7 * 86400000));
      renderNotices();
    });
  }

  // ---------- theme picker ----------

  let palettesCache = null;
  async function palettes() {
    if (!palettesCache) {
      const s = await get("/api/settings");
      palettesCache = s.palettes;
      learnCats(s.config.categories);
    }
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

  /** Category colours are inline styles, so pages redraw when they switch (not Settings: unsaved edits). */
  function redrawForLook() {
    if (!location.hash.startsWith("#/settings")) route();
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
        `<div class="look-row"><span class="look-k" id="lk-${key}">${label}</span><div class="seg" role="radiogroup" aria-labelledby="lk-${key}">${values
          .map((v, i) => `<button type="button" role="radio" data-look="${key}" data-v="${esc(v)}" aria-checked="${l[key] === v}">${esc(names[i])}</button>`)
          .join("")}</div></div>`;
      el.innerHTML = `
        ${seg("palette", "Theme", pals.map((x) => x.id), pals.map((x) => x.name))}
        ${seg("theme", "Mode", ["system", "dark", "light"], ["System", "Dark", "Light"])}
        ${p.dark.length > 1 ? seg("dark_flavor", "Dark flavour", p.dark, p.dark.map((f) => FLAVOR[f] || cap(f))) : ""}
        <div class="look-row"><span class="look-k" id="lk-accent">Accent</span><div class="swatches" role="radiogroup" aria-labelledby="lk-accent">${p.accents
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

  const lookBtn = document.getElementById("lookbtn");
  const lookPop = document.getElementById("lookpop");
  let lookReady = false;
  async function toggleLook(open) {
    const show = open ?? lookPop.hidden;
    if (show && !lookReady) {
      lookReady = true;
      await lookControls(lookPop);
    }
    lookPop.hidden = !show;
    lookBtn.setAttribute("aria-expanded", String(show));
    if (show) lookPop.querySelector('[aria-checked="true"]')?.focus();
  }
  lookBtn.addEventListener("click", () => toggleLook());
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !lookPop.hidden) {
      toggleLook(false);
      lookBtn.focus();
    }
  });
  document.addEventListener("click", (e) => {
    // A redraw inside the popover detaches the clicked button: that click was inside.
    if (lookPop.hidden || !e.target.isConnected || lookPop.contains(e.target) || lookBtn.contains(e.target)) return;
    toggleLook(false);
  });

  // The skip link moves focus without changing the hash (which would re-route).
  document.getElementById("skip")?.addEventListener("click", (e) => {
    e.preventDefault();
    view.focus();
  });

  // ---------- router ----------

  function route() {
    clearInterval(refreshTimer);
    refreshTimer = null;
    currentDay = null;
    const gen = ++renderGen;
    const h = location.hash.replace(/^#\/?/, "") || "today";
    const [page, arg] = h.split("/");
    const key = page === "day" ? "today" : page;
    document.querySelectorAll(".nav a").forEach((a) => {
      if (a.dataset.route === key) a.setAttribute("aria-current", "page");
      else a.removeAttribute("aria-current");
    });
    const valid = (r, list) => (list.some(([v]) => v === r) ? r : null);
    switch (page) {
      case "day":
        return pageDay(/^\d{4}-\d{2}-\d{2}$/.test(arg || "") ? arg : today(), false, gen);
      case "apps":
        return pageApps(valid(arg, RANGES) || "1", gen);
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

  document.addEventListener("keydown", (e) => {
    if (e.target.closest("input, textarea, select, [contenteditable]") || e.altKey || e.ctrlKey || e.metaKey) return;
    const h = location.hash;
    const onDay = h === "" || h === "#/today" || h.startsWith("#/day/");
    if (!onDay || !currentDay) return;
    if (e.key === "ArrowLeft") location.hash = `#/day/${addDays(currentDay, -1)}`;
    else if (e.key === "ArrowRight" && currentDay < today()) {
      const next = addDays(currentDay, 1);
      location.hash = next === today() ? "#/today" : `#/day/${next}`;
    } else if (e.key === "t") location.hash = "#/today";
  });

  bindRows(view);
  window.addEventListener("hashchange", route);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) return;
    pollStatus();
    if (refreshTimer) refreshToday();
  });
  route();
  pollStatus();
  setInterval(() => {
    if (!document.hidden) pollStatus();
  }, 5000);
})();
