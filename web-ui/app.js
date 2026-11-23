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
  let refreshTimer = null;
  let currentDay = null;
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
  const safeColor = (c) => (/^#[0-9a-fA-F]{3,8}$/.test(c || "") ? c : "#6b7392");

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

  async function get(path) {
    const r = await fetch(path, { headers: { accept: "application/json" } });
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
          <span>Agent</span><span class="rb"><i style="--w:${w(a.busy)};--c:#b49be8"></i></span><span class="v">${dur(a.busy)}</span>
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

  async function pageDay(date, quiet) {
    currentDay = date;
    const isToday = date === today();
    let d;
    try {
      d = await get(`/api/day?date=${encodeURIComponent(date)}`);
    } catch (e) {
      view.innerHTML = errorHtml(e);
      return;
    }
    if (currentDay !== date) return;
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
    if (isToday) {
      refreshTimer = setInterval(() => {
        if (!document.hidden && currentDay === date && date === today()) pageDay(date, true);
      }, 60000);
    }
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

  async function pageApps(n) {
    view.innerHTML = `<div class="toolbar"><h1>Apps and sites</h1>${rangeBar("apps", n)}</div><p class="empty">Loading…</p>`;
    bindRange("apps");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      view.innerHTML = errorHtml(e);
      return;
    }
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

  async function pageAgents(n) {
    view.innerHTML = `<div class="toolbar"><h1>AI agents</h1>${rangeBar("agents", n)}</div><p class="empty">Loading…</p>`;
    bindRange("agents");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      view.innerHTML = errorHtml(e);
      return;
    }
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
    const color = Object.fromEntries(cats.map((c) => [c.name, safeColor(c.color)]));
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

  function heatmap(days) {
    const max = Math.max(1, ...days.map((d) => d.totals.screen));
    const first = parseYmd(days[0].date);
    const lead = (first.getDay() + 6) % 7; // weeks start Monday
    const cells = [];
    for (let i = 0; i < lead; i++) cells.push('<i style="visibility:hidden"></i>');
    for (const d of days) {
      const pct = Math.round((d.totals.screen / max) * 100);
      const bg = d.totals.screen ? `color-mix(in srgb, #5fb4e0 ${Math.max(12, pct)}%, transparent)` : "";
      cells.push(`<i style="${bg ? `background:${bg}` : ""}" title="${esc(dayLabel(d.date, { weekday: "short", day: "numeric", month: "short" }))}: ${dur(d.totals.screen)}"></i>`);
    }
    return `<div class="heat-wrap"><div class="heat" role="img" aria-label="Screen time per day for the last year">${cells.join("")}</div></div>`;
  }

  async function pageTrends(n) {
    const ranges = RANGES.filter(([v]) => v !== "1");
    view.innerHTML = `<div class="toolbar"><h1>Trends</h1>${rangeBar("trends", n, ranges)}</div><p class="empty">Loading…</p>`;
    bindRange("trends");
    let r;
    try {
      r = await loadRange(n);
    } catch (e) {
      view.innerHTML = errorHtml(e);
      return;
    }
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
      ${n === "365" ? heatmap(r.days) : stackedChart(r.days, r.categories)}
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
    return `<input type="color" data-cat="${i}" data-k="color" value="${esc(safeColor(c.color))}" aria-label="Colour">
      <input type="text" data-cat="${i}" data-k="name" value="${esc(c.name)}" aria-label="Name" ${fixed ? "readonly" : ""}>
      <textarea data-cat="${i}" data-k="hosts" aria-label="Sites">${esc(lines(c.hosts))}</textarea>
      <textarea data-cat="${i}" data-k="apps" aria-label="Apps">${esc(lines(c.apps))}</textarea>
      <textarea data-cat="${i}" data-k="programs" aria-label="Programs">${esc(lines(c.programs))}</textarea>
      <button type="button" class="btn" data-delcat="${i}" ${fixed ? "disabled title='Fallback category'" : ""}>Remove</button>`;
  }

  async function pageSettings() {
    let s;
    try {
      s = await get("/api/settings");
    } catch (e) {
      view.innerHTML = errorHtml(e);
      return;
    }
    const c = s.config;
    let cats = c.categories.map((x) => ({ ...x }));
    const st = status || {};
    const pausedText = st.paused ? (st.paused_until_ms ? `Paused until ${hm(st.paused_until_ms)}.` : "Paused until you resume.") : "Tracking is on.";
    const tools = c.agents.tools.map((t) => `${t.id} | ${t.name} | ${t.matches.join(", ")}`).join("\n");
    const t0 = today();
    view.innerHTML = `<form class="settings" id="sform" novalidate>
      <div class="toolbar"><h1>Settings</h1><span class="msg">Saved to ${esc(s.config_path)}</span></div>

      <fieldset>
        <legend>Pause</legend>
        ${field("Tracking", pausedText, `<div class="inline">
          <button type="button" class="btn" data-pause="30">Pause 30 minutes</button>
          <button type="button" class="btn" data-pause="60">Pause 1 hour</button>
          <button type="button" class="btn" data-pause="">Pause until I resume</button>
          <button type="button" class="btn" data-resume ${st.paused ? "" : "disabled"}>Resume</button></div>`)}
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
        ${field("…or it writes", "Output to its terminal per check.", num("awrite", c.agents.busy_write_bytes, "bytes or more", 0, 10000000))}
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
        ${field("Theme", "", `<div class="seg" role="group" aria-label="Theme">${["system", "dark", "light"].map((t) => `<button type="button" data-theme-pick="${t}" aria-pressed="${c.dashboard.theme === t}">${t[0].toUpperCase() + t.slice(1)}</button>`).join("")}</div>`)}
        ${field("Custom CSS", "Loaded after the built-in styles. Variables like --bg, --text and --shu are yours to change.", `<textarea id="css" class="code" spellcheck="false">${esc(s.custom_css)}</textarea>`)}
      </fieldset>

      <fieldset>
        <legend>Export</legend>
        ${field("Raw data", "Every recorded interval, for your own analysis.", `<div class="inline"><input type="date" id="exfrom" value="${addDays(t0, -6)}"><input type="date" id="exto" value="${t0}"><select id="exfmt"><option value="csv">CSV</option><option value="json">JSON</option></select><a class="btn" id="exlink" href="#">Download</a></div>`)}
      </fieldset>

      <div class="savebar"><button type="submit" class="btn primary">Save changes</button><span class="msg" id="smsg"></span></div>
    </form>`;

    const $ = (id) => document.getElementById(id);
    let theme = c.dashboard.theme;
    const msg = (text, kind) => {
      $("smsg").textContent = text;
      $("smsg").dataset.kind = kind || "";
    };
    const exlink = () => {
      $("exlink").href = `/api/export?from=${$("exfrom").value}&to=${$("exto").value}&format=${$("exfmt").value}`;
    };
    ["exfrom", "exto", "exfmt"].forEach((id) => $(id).addEventListener("change", exlink));
    exlink();

    view.querySelectorAll("[data-theme-pick]").forEach((b) =>
      b.addEventListener("click", () => {
        theme = b.dataset.themePick;
        document.documentElement.dataset.theme = theme;
        view.querySelectorAll("[data-theme-pick]").forEach((x) => x.setAttribute("aria-pressed", String(x === b)));
      }),
    );
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
    view.querySelectorAll("[data-pause]").forEach((b) =>
      b.addEventListener("click", async () => {
        const m = b.dataset.pause;
        try {
          await send("POST", "/api/pause", { minutes: m ? Number(m) : null });
          await pollStatus();
          pageSettings();
        } catch (e) {
          msg(e.message, "error");
        }
      }),
    );
    view.querySelector("[data-resume]").addEventListener("click", async () => {
      try {
        await send("POST", "/api/resume");
        await pollStatus();
        pageSettings();
      } catch (e) {
        msg(e.message, "error");
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
      next.dashboard.theme = theme;
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
  }

  function renderNotices() {
    const s = status;
    if (!s || !s.sources) return;
    const out = [];
    const ext = (s.sources.browser_extension || []).some((e) => e.alive);
    const browserSeen = !!s.sources.plasma_browser_integration || /brave|chrom/i.test((s.focus && s.focus.app) || "");
    const hidden = Number(localStorage.getItem("hide-ext-notice") || 0) > Date.now();
    if (!ext && browserSeen && !hidden) {
      out.push(`<div class="notice"><p>Brave tabs are tracked with a fallback, so two tabs with the same title can be mixed up. Connect the arch-tracker extension for exact tabs:</p>
        <ol><li>Open <code>brave://extensions</code> and switch on Developer mode.</li>
        <li>Choose Load unpacked and pick <code>${esc(s.extension && s.extension.dir)}</code>.</li>
        <li>Check that its ID is <code>${esc(s.extension && s.extension.id)}</code>.</li></ol>
        <div class="actions"><button type="button" class="btn" id="hideext">Hide for a week</button></div></div>`);
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

  // ---------- router ----------

  function route() {
    clearInterval(refreshTimer);
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
        return pageDay(/^\d{4}-\d{2}-\d{2}$/.test(arg || "") ? arg : today());
      case "apps":
        return pageApps(valid(arg, RANGES) || "1");
      case "agents":
        return pageAgents(valid(arg, RANGES) || "7");
      case "trends":
        return pageTrends(valid(arg, RANGES.slice(1)) || "30");
      case "settings":
        return pageSettings();
      default:
        return pageDay(today());
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
    if (!document.hidden) pollStatus();
  });
  route();
  pollStatus();
  setInterval(() => {
    if (!document.hidden) pollStatus();
  }, 5000);
})();
