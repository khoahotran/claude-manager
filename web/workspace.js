'use strict';
/* claude-manager UI - Workspace tab: several Claude terminals in parallel (xterm.js panes <-> WebSocket PTYs).
   Terminals live on the server and outlive this page; closing the browser tab never kills them.
   Persisted state (PUT /api/workspace, opaque to the server):
     { v:1, layout, sizes:{layoutId:{cols:[..],rows:[..]}}, focus, panes:[{key, termId, source, cwd, resume, title}] } */
(() => {
  const CM = window.CM;
  const { $, $$, toast, api, TOKEN, store, T, esc } = CM;

  const WS_CHUNK = 16 * 1024;        // max bytes per outgoing websocket frame (server limit is 64 KiB)
  const MAX_PANES = 32;            // the server allows at most 32 live terminals (HTTP 429 beyond that)
  const DEF_DIMS = { cols: 120, rows: 32 };
  const GAP = 6;                 // px between panes (also the splitter thickness)
  const MIN_W = 220, MIN_H = 120;
  const NARROW = window.matchMedia('(max-width: 760px)');
  const enc = new TextEncoder();
  const LRM = String.fromCharCode(0x200e);   // left-to-right mark
  const uid = () => 'p' + Math.random().toString(36).slice(2, 9) + Date.now().toString(36).slice(-3);
  const A = (r, c, rs = 1, cs = 1) => ({ r, c, rs, cs });

  // Layout presets. Every area is {row, col, rowSpan, colSpan}; panes fill the areas in order.
  const LAYOUTS = {
    '1':   { cols: 1, rows: 1, areas: [A(0, 0)] },
    '2c':  { cols: 2, rows: 1, areas: [A(0, 0), A(0, 1)] },
    '2r':  { cols: 1, rows: 2, areas: [A(0, 0), A(1, 0)] },
    '2x2': { cols: 2, rows: 2, areas: [A(0, 0), A(0, 1), A(1, 0), A(1, 1)] },
    '3c':  { cols: 3, rows: 1, areas: [A(0, 0), A(0, 1), A(0, 2)] },
    '1+2': { cols: 2, rows: 2, areas: [A(0, 0, 2, 1), A(0, 1), A(1, 1)] },
    '3x2': { cols: 3, rows: 2, areas: [A(0, 0), A(0, 1), A(0, 2), A(1, 0), A(1, 1), A(1, 2)] },
  };
  const layLabel = (id) => T('workspace.layout_' + id.replace('+', 'p'));
  const LAYOUT_ORDER = ['1', '2c', '2r', '2x2', '3c', '1+2', '3x2'];
  // When a new pane does not fit, grow to the next preset in this chain.
  const GROW = { '1': '2c', '2c': '3c', '2r': '1+2', '3c': '2x2', '1+2': '2x2', '2x2': '3x2', '3x2': null };
  const SINGLE = { cols: 1, rows: 1, areas: [A(0, 0)] };

  const STATES = ['starting', 'connecting', 'live', 'reconnecting', 'exited', 'error'];
  const stateLabel = (s) => (STATES.includes(s) ? T('workspace.state_' + s) : s);
  const ALIVE = new Set(['starting', 'connecting', 'live', 'reconnecting']);

  const SVG = (d) => `<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${d}</svg>`;
  const ICON = {
    restart: SVG('<path d="M13.2 8A5.2 5.2 0 1 1 11.6 4.3"/><path d="M13.4 2.2v3.3h-3.3"/>'),
    max: SVG('<path d="M9.5 2.5h4v4M6.5 13.5h-4v-4M13.5 2.5L9 7M2.5 13.5L7 9"/>'),
    restore: SVG('<path d="M13 6.5H9.5V3M3 9.5h3.5V13M9.5 6.5L14 2M6.5 9.5L2 14"/>'),
    code: SVG('<path d="M5.5 4L2 8l3.5 4M10.5 4L14 8l-3.5 4"/>'),
    close: SVG('<path d="M3.5 3.5l9 9M12.5 3.5l-9 9"/>'),
  };

  const S = {
    layout: '1',
    sizes: {},          // layoutId -> {cols:[weights], rows:[weights]}
    panes: [],          // slot order: the first N panes are visible in a layout with N slots
    focus: null,        // pane key
    maxKey: null,       // maximized pane key (not persisted)
    sources: [],
    splitters: [],      // [{el, sp}]
    ready: false,
    readyP: null,       // resolves when the saved layout has been restored
  };
  const els = {};
  const isNarrow = () => NARROW.matches;
  const isAlive = (p) => ALIVE.has(p.state);
  const byKey = (k) => S.panes.find((p) => p.key === k) || null;
  const mk = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e; };
  const basename = (p) => {
    const s = String(p || '').replace(/[\\/]+$/, '');
    const i = Math.max(s.lastIndexOf('/'), s.lastIndexOf('\\'));
    return (i >= 0 ? s.slice(i + 1) : s) || String(p || '') || T('common.unknown_paren');
  };
  const normPath = (p) => String(p || '').trim().replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();

  function termTheme() {
    const cs = getComputedStyle(document.documentElement);
    const v = (n) => cs.getPropertyValue(n).trim();
    const t = { background: v('--term-bg'), foreground: v('--term-fg'), cursor: v('--term-cursor'), cursorAccent: v('--term-bg'), selectionBackground: v('--term-sel') };
    ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white', 'brightBlack', 'brightRed', 'brightGreen', 'brightYellow', 'brightBlue', 'brightMagenta', 'brightCyan', 'brightWhite']
      .forEach((n, i) => { t[n] = v('--ansi-' + i); });
    return t;
  }
  function applyTermTheme() {
    const th = termTheme();
    for (const p of S.panes) { try { p.term.options.theme = th; } catch { /* ignore */ } }
  }

  function newPane(spec) {
    const pane = {
      key: spec.key || uid(), termId: spec.termId || null,
      source: spec.source, cwd: spec.cwd, resume: spec.resume || null, model: spec.model || '', effort: spec.effort || '', title: spec.title || '',
      state: spec.state || 'connecting', exitInfo: '', error: '', oscTitle: '',
      ws: null, retry: 0, retryTimer: 0, resizeTimer: 0, lastSent: null, hadData: false,
      opened: false, ro: null, fitRaf: 0, titleRaf: 0, mxShown: null, closing: false, restarting: false, wantFocus: false,
      freshUntil: spec.freshUntil || 0, answered: !!spec.answered, attachAt: 0, lastMsgAt: 0, pendingWrites: 0, mutedReplies: 0,
      term: null, fit: null, el: null,
    };
    buildPaneEl(pane);
    createTerm(pane);
    renderHeader(pane);
    renderOverlay(pane);
    return pane;
  }

  function buildPaneEl(pane) {
    const root = mk('div', 'pane');
    root.dataset.key = pane.key;
    root.setAttribute('role', 'group');
    root.innerHTML = `<div class="pane-hd">
        <span class="dot" role="img"></span><span class="badge"></span>
        <span class="pane-folder"></span><span class="pane-title"></span>
        <button type="button" class="ibtn" data-act="editor" title="${esc(T('workspace.pane_editor_tip'))}" aria-label="${esc(T('workspace.pane_editor'))}">${ICON.code}</button>
        <button type="button" class="ibtn" data-act="restart" title="${esc(T('workspace.pane_restart'))}" aria-label="${esc(T('workspace.pane_restart'))}">${ICON.restart}</button>
        <button type="button" class="ibtn" data-act="max">${ICON.max}</button>
        <button type="button" class="ibtn close" data-act="close" title="${esc(T('workspace.pane_close'))}" aria-label="${esc(T('workspace.pane_close'))}">${ICON.close}</button>
      </div>
      <div class="pane-body"><div class="xterm-host"></div><div class="pane-overlay" hidden></div></div>`;
    pane.el = {
      root, hd: $('.pane-hd', root), dot: $('.dot', root), badge: $('.badge', root), folder: $('.pane-folder', root),
      title: $('.pane-title', root), host: $('.xterm-host', root), overlay: $('.pane-overlay', root), btnMax: $('[data-act=max]', root),
    };
    root.addEventListener('focusin', () => {
      if (S.focus !== pane.key) { S.focus = pane.key; markFocus(); renderStrip(); }
    });
  }

  function createTerm(pane) {
    const term = new Terminal({
      scrollback: 5000,
      fontFamily: 'ui-monospace, "Cascadia Mono", Consolas, "Liberation Mono", Menlo, monospace',
      fontSize: 13,
      cursorBlink: true,
      theme: termTheme(),
    });
    const fit = new FitAddon.FitAddon();
    term.loadAddon(fit);
    term.onData((d) => {
      if (d.charCodeAt(0) === 27 && RESPONSE_RE.test(d) && replaying(pane)) { pane.mutedReplies++; return; }
      sendInput(pane, enc.encode(d));
    });
    term.onBinary((d) => {
      const u = new Uint8Array(d.length);
      for (let i = 0; i < d.length; i++) u[i] = d.charCodeAt(i) & 255;
      sendInput(pane, u);
    });
    term.onResize(() => { scheduleResizeSend(pane); requestAnimationFrame(() => dropGhostSelection(pane)); });
    term.onTitleChange((t) => {                       // Claude animates the title (spinner): coalesce to one update per frame
      pane.oscTitle = String(t || '').slice(0, 120);
      if (pane.titleRaf) return;
      pane.titleRaf = requestAnimationFrame(() => { pane.titleRaf = 0; renderHeader(pane); });
    });
    term.attachCustomKeyEventHandler((ev) => keyHandler(pane, ev));
    pane.term = term;
    pane.fit = fit;
  }

  // When a page (re)attaches to a running terminal, the server replays the scrollback and xterm re-parses it. Any query in
  // that history (cursor-position request, device attributes ...) makes xterm answer again, and those answers would reach
  // the live process as keystrokes (cmd.exe turns a replayed cursor-position reply into F3 = "repeat last command", Claude's
  // prompt would get "[24;80R" typed into it). So terminal-generated replies are dropped while a replay is being processed.
  // A brand-new terminal is exempt: its very first attach has to answer ConPTY's startup query.
  const RESPONSE_RE = /^\x1b(?:\[[?>=]?[0-9;:]*[Rcn]|\[\??[0-9;]*\$y|P[\s\S]*|\][\s\S]*)$/;
  const FRESH_MS = 6000;
  function replaying(pane) {
    if (!pane.answered && pane.freshUntil > Date.now()) return false;
    const now = performance.now();
    if (now - pane.attachAt > 4000) return false;                       // hard cap: never mute for long
    return pane.pendingWrites > 0 || now - pane.lastMsgAt < 250 || now - pane.attachAt < 300;
  }

  /** xterm 5.5's DOM renderer re-draws the last (already cleared) selection after a resize: remove that ghost highlight. */
  function dropGhostSelection(pane) {
    if (!pane.opened || pane.term.hasSelection()) return;
    const layer = pane.el.host.querySelector('.xterm-selection');
    if (layer && layer.firstChild) layer.replaceChildren();
  }

  const isNewPaneKey = (e) => e.ctrlKey && !e.metaKey && e.code === 'KeyT' && (e.shiftKey !== e.altKey);   // Ctrl+Shift+T / Ctrl+Alt+T
  const ARROWS = { ArrowLeft: 'left', ArrowRight: 'right', ArrowUp: 'up', ArrowDown: 'down' };
  const isFocusKey = (e) => e.ctrlKey && e.altKey && !e.shiftKey && !e.metaKey && Object.prototype.hasOwnProperty.call(ARROWS, e.key);

  function keyHandler(pane, ev) {
    if (ev.type !== 'keydown') return true;
    if (isNewPaneKey(ev) || isFocusKey(ev)) return false;          // app shortcuts: never forwarded to the PTY
    if (ev.ctrlKey && !ev.altKey && !ev.metaKey && ev.code === 'KeyC' && (ev.shiftKey || pane.term.hasSelection())) {
      // Windows Terminal behaviour: Ctrl+C copies only when text is selected; otherwise it stays a normal ^C.
      ev.preventDefault();
      if (pane.term.hasSelection()) { copyText(pane.term.getSelection()); pane.term.clearSelection(); }
      return false;
    }
    return true;
  }

  async function copyText(text) {
    try { await navigator.clipboard.writeText(text); toast(T('toast.copied')); return; } catch { /* fall back below */ }
    const ta = mk('textarea', 'sr-only');
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try { ok = document.execCommand('copy'); } catch { /* ignore */ }
    ta.remove();
    toast(ok ? T('toast.copied') : T('toast.copy_failed'), ok ? undefined : 'err');
  }

  // Terminal titles set by the program (OSC 0/2) are shown when they carry information; bare product / shell / exe names do not.
  const GENERIC_TITLE = /^[\s\W_]*(claude(\s*code)?|cmd(\.exe)?|powershell|pwsh|wsl|bash|zsh|sh)[\s\W_]*$|\.exe\s*$/i;
  function paneSubtitle(pane) {
    if (pane.oscTitle && !GENERIC_TITLE.test(pane.oscTitle)) return pane.oscTitle;
    if (pane.resume) {
      const s = CM.getData().sessions.find((x) => x.sessionId === pane.resume && x.source === pane.source);
      if (s) return s.title;
    }
    return pane.title || '';
  }

  function renderHeader(pane) {
    const e = pane.el, name = basename(pane.cwd), label = stateLabel(pane.state), sub = paneSubtitle(pane);
    e.dot.className = 'dot st-' + pane.state;
    e.dot.setAttribute('aria-label', label);
    e.dot.title = label;
    e.badge.className = 'badge ' + CM.srcClass(pane.source);
    e.badge.textContent = pane.source;
    e.folder.textContent = name;
    const showSub = sub && sub.trim().toLowerCase() !== name.toLowerCase();   // the server title is often just the folder name again
    e.title.textContent = showSub ? sub : '';
    e.hd.title = [pane.cwd, pane.source + (pane.termId ? ' · ' + T('workspace.pane_hd_tip_terminal', { id: pane.termId }) : ''), sub && T('workspace.pane_hd_tip_session', { title: sub }), label, T('workspace.pane_hd_tip_hint')].filter(Boolean).join('\n');
    e.root.setAttribute('aria-label', T('workspace.pane_aria', { name, state: label }));
    const mx = S.maxKey === pane.key;
    if (pane.mxShown !== mx) {
      pane.mxShown = mx;
      const mxLabel = mx ? T('workspace.pane_restore') : T('workspace.pane_maximize');
      e.btnMax.innerHTML = mx ? ICON.restore : ICON.max;
      e.btnMax.title = mxLabel;
      e.btnMax.setAttribute('aria-label', mxLabel);
      e.btnMax.setAttribute('aria-pressed', String(mx));
    }
  }

  function latestSession(pane) {
    const cwd = normPath(pane.cwd);
    return CM.getData().sessions
      .filter((s) => s.status === 'active' && s.source === pane.source && s.cwd && normPath(s.cwd) === cwd)
      .sort((a, b) => (a.last < b.last ? 1 : -1))[0] || null;
  }

  function renderOverlay(pane) {
    const ov = pane.el.overlay, st = pane.state;
    ov.textContent = '';
    ov.className = 'pane-overlay';
    if (st === 'exited' || st === 'error') {
      ov.hidden = false;
      const card = mk('div', 'ov-card');
      card.append(mk('div', 'ov-title', st === 'error' ? T('workspace.ov_error_title') : T('workspace.ov_exited_title')),
        mk('div', 'ov-sub', st === 'error' ? pane.error : (pane.exitInfo || T('workspace.ov_process_stopped'))));
      const act = mk('div', 'ov-actions');
      const b1 = mk('button', 'primary', st === 'error' ? T('workspace.ov_retry') : T('workspace.ov_restart')); b1.type = 'button'; b1.dataset.act = 'restart';
      act.append(b1);
      if (st === 'exited' && !pane.resume && latestSession(pane)) {
        const b2 = mk('button', '', T('workspace.ov_continue_latest')); b2.type = 'button'; b2.dataset.act = 'continue';
        act.append(b2);
      }
      const b3 = mk('button', '', T('workspace.ov_close')); b3.type = 'button'; b3.dataset.act = 'close';
      act.append(b3);
      card.append(act);
      ov.append(card);
    } else if (st === 'starting' || st === 'reconnecting') {
      ov.hidden = false;
      ov.classList.add('soft');
      const card = mk('div', 'ov-card');
      card.append(mk('span', 'dot st-' + st), mk('span', '', stateLabel(st)));
      ov.append(card);
    } else {
      ov.hidden = true;
    }
  }

  function setState(pane, st, extra) {
    if (pane.state === st && !extra) return;
    pane.state = st;
    if (extra) Object.assign(pane, extra);
    renderHeader(pane);
    renderOverlay(pane);
    renderStrip();
    updateSummary();
  }

  function updateSummary() {
    const live = S.panes.filter(isAlive).length, ended = S.panes.length - live;
    els.sum.textContent = '';
    if (!S.panes.length) {
      els.sum.append(mk('span', 'muted', T('workspace.sum_none')));
    } else {
      els.sum.append(mk('span', 'dot ' + (live ? 'st-live' : 'st-exited')), mk('span', '', T('workspace.sum_live', { n: live }) + (ended ? T('workspace.sum_ended', { n: ended }) : '')));
    }
    const b = els.badge;
    b.hidden = live === 0;
    b.textContent = String(live);
    const sr = mk('span', 'sr-only', T('workspace.sr_running'));
    b.append(sr);
  }

  function renderStrip() {
    const strip = els.strip;
    const everything = isNarrow() || !!S.maxKey;
    const vis = new Set(visibleList());
    const list = everything ? S.panes : S.panes.filter((p) => !vis.has(p));
    strip.hidden = list.length === 0 || (everything && S.panes.length < 2);
    if (strip.hidden) return;
    strip.textContent = '';
    strip.append(mk('span', 'muted', everything ? T('workspace.strip_switch') : T('workspace.strip_hidden')));
    const names = new Map();
    for (const p of S.panes) names.set(basename(p.cwd), (names.get(basename(p.cwd)) || 0) + 1);
    for (const p of list) {
      const b = mk('button', 'chip');
      b.type = 'button';
      b.dataset.key = p.key;
      b.setAttribute('aria-current', String(p.key === S.focus));
      b.title = `${p.cwd}\n${p.source} - ${stateLabel(p.state)}`;
      const dup = names.get(basename(p.cwd)) > 1;   // same folder name in several panes: add the source to tell them apart
      b.append(mk('span', 'dot st-' + p.state), mk('span', 't', basename(p.cwd) + (dup ? ' · ' + p.source : '')));
      strip.append(b);
    }
  }

  function markFocus() {
    for (const p of S.panes) p.el.root.classList.toggle('focused', p.key === S.focus);
  }

  function visibleList() {
    const panes = S.panes;
    if (!panes.length) return [];
    if (S.maxKey) {
      const p = byKey(S.maxKey);
      if (p) return [p];
      S.maxKey = null;
    }
    if (isNarrow()) return [byKey(S.focus) || panes[0]];
    return panes.slice(0, LAYOUTS[S.layout].areas.length);
  }
  function effLayout() {
    if (S.maxKey || isNarrow()) return Object.assign({ id: '1' }, SINGLE);
    return Object.assign({ id: S.layout }, LAYOUTS[S.layout]);
  }
  function sizesFor(id, lay) {
    let z = S.sizes[id];
    if (!z || !Array.isArray(z.cols) || !Array.isArray(z.rows) || z.cols.length !== lay.cols || z.rows.length !== lay.rows) {
      z = S.sizes[id] = { cols: Array(lay.cols).fill(1), rows: Array(lay.rows).fill(1) };
    }
    return z;
  }
  function sanitizeSizes(o) {
    const out = {};
    for (const id of Object.keys(o || {})) {
      const lay = LAYOUTS[id], z = o[id];
      const ok = (a, n) => Array.isArray(a) && a.length === n && a.every((v) => typeof v === 'number' && isFinite(v) && v > 0.05 && v < 50);
      if (lay && z && ok(z.cols, lay.cols) && ok(z.rows, lay.rows)) out[id] = { cols: z.cols.slice(), rows: z.rows.slice() };
    }
    return out;
  }
  const trackTemplate = (arr) => arr.map((w) => `minmax(0, ${+w.toFixed(4)}fr)`).join(' ');
  function applyTemplate(z) {
    els.grid.style.gridTemplateColumns = trackTemplate(z.cols);
    els.grid.style.gridTemplateRows = trackTemplate(z.rows);
  }
  function place(el, a) {
    el.style.gridRow = `${a.r + 1} / span ${a.rs}`;
    el.style.gridColumn = `${a.c + 1} / span ${a.cs}`;
  }

  /** The focused pane must always be visible in the wide layout: swap it into a slot if it was hidden. */
  function ensureInSlots(pane, preferKey) {
    if (isNarrow() || S.maxKey || !pane) return false;
    const slots = LAYOUTS[S.layout].areas.length;
    const i = S.panes.indexOf(pane);
    if (i < 0 || i < slots) return false;
    let j = preferKey ? S.panes.findIndex((p) => p.key === preferKey) : -1;
    if (j < 0 || j >= slots) j = Math.min(slots, S.panes.length) - 1;
    [S.panes[i], S.panes[j]] = [S.panes[j], S.panes[i]];
    return true;
  }

  function renderLayout() {
    if (!els.grid) return;
    if (S.focus && !byKey(S.focus)) S.focus = null;
    if (!S.focus && S.panes.length) S.focus = S.panes[0].key;
    if (ensureInSlots(byKey(S.focus))) persist();
    const vis = visibleList();
    const lay = effLayout();
    const z = lay.id === S.layout && !S.maxKey && !isNarrow() ? sizesFor(lay.id, lay) : { cols: [1], rows: [1] };
    applyTemplate(z);
    for (const p of S.panes) {
      if (p.el.root.parentNode !== els.grid) els.grid.appendChild(p.el.root);
      const idx = vis.indexOf(p);
      p.el.root.hidden = idx < 0;
      if (idx >= 0) place(p.el.root, lay.areas[Math.min(idx, lay.areas.length - 1)]);
      renderHeader(p);   // refreshes the maximize button state
    }
    $$('.slot-empty', els.grid).forEach((n) => n.remove());
    if (!S.maxKey) {
      for (let i = vis.length; i < lay.areas.length; i++) {
        const b = mk('button', 'slot-empty');
        b.type = 'button';
        b.dataset.act = 'new';
        b.setAttribute('aria-label', T('workspace.slot_empty_label'));
        b.append(mk('span', 'plus', '＋'), mk('span', '', T('workspace.slot_empty_text')), mk('small', '', 'Ctrl+Shift+T'));
        if (!S.panes.length && i === 0) {
          b.append(mk('small', 'slot-hint', T('workspace.slot_hint')));
        }
        place(b, lay.areas[i]);
        els.grid.appendChild(b);
      }
    }
    markFocus();
    buildSplitters();
    renderStrip();
    updateLayoutButtons();
    updateSummary();
    requestAnimationFrame(() => {
      for (const p of vis) { if (ensureOpened(p)) scheduleFit(p); }
      positionSplitters();
    });
  }

  function setLayout(id) {
    if (!LAYOUTS[id]) return;
    S.layout = id;
    S.maxKey = null;
    renderLayout();
    persist();
  }

  /** Make room for n panes by moving up the preset chain; false if even the largest preset is too small. */
  function makeRoom(n) {
    let id = S.layout;
    while (LAYOUTS[id].areas.length < n) {
      const nx = GROW[id];
      if (!nx) break;
      id = nx;
    }
    if (id !== S.layout) { S.layout = id; toast(T('toast.workspace_layout_switched', { label: layLabel(id) })); }
    return LAYOUTS[id].areas.length >= n;
  }

  function layoutIcon(lay) {
    const W = 22, H = 16, g = 1.6;
    const cw = (W - g * (lay.cols - 1)) / lay.cols, rh = (H - g * (lay.rows - 1)) / lay.rows;
    return `<svg viewBox="0 0 ${W} ${H}" width="22" height="16" aria-hidden="true">` + lay.areas.map((a) =>
      `<rect x="${(a.c * (cw + g)).toFixed(2)}" y="${(a.r * (rh + g)).toFixed(2)}" width="${(a.cs * cw + (a.cs - 1) * g).toFixed(2)}" height="${(a.rs * rh + (a.rs - 1) * g).toFixed(2)}" rx="1.6"/>`).join('') + '</svg>';
  }
  function buildLayoutButtons() {
    els.layouts.textContent = '';
    for (const id of LAYOUT_ORDER) {
      const lay = LAYOUTS[id];
      const b = mk('button');
      b.type = 'button';
      b.dataset.layout = id;
      b.title = layLabel(id);
      b.setAttribute('aria-label', T('workspace.layout_aria', { label: layLabel(id) }));
      b.innerHTML = layoutIcon(lay);
      els.layouts.append(b);
    }
  }
  function updateLayoutButtons() {
    $$('button', els.layouts).forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.layout === S.layout)));
  }

  function splitterSpecs(lay) {
    const out = [];
    for (let k = 1; k < lay.cols; k++) {          // vertical bar between column k-1 and k
      let run = null;
      for (let r = 0; r < lay.rows; r++) {
        const crosses = lay.areas.some((a) => a.r <= r && r < a.r + a.rs && a.c < k && a.c + a.cs > k);
        if (!crosses) { if (run) run.r1 = r + 1; else run = { dir: 'v', k, r0: r, r1: r + 1 }; }
        else if (run) { out.push(run); run = null; }
      }
      if (run) out.push(run);
    }
    for (let k = 1; k < lay.rows; k++) {          // horizontal bar between row k-1 and k
      let run = null;
      for (let c = 0; c < lay.cols; c++) {
        const crosses = lay.areas.some((a) => a.c <= c && c < a.c + a.cs && a.r < k && a.r + a.rs > k);
        if (!crosses) { if (run) run.c1 = c + 1; else run = { dir: 'h', k, c0: c, c1: c + 1 }; }
        else if (run) { out.push(run); run = null; }
      }
      if (run) out.push(run);
    }
    return out;
  }
  const trackPx = (s) => String(s || '').trim().split(/\s+/).map(parseFloat);

  function buildSplitters() {
    for (const { el } of S.splitters) el.remove();
    S.splitters = [];
    if (S.maxKey || isNarrow()) return;
    const lay = effLayout();
    for (const sp of splitterSpecs(lay)) {
      const el = mk('div', 'splitter ' + sp.dir);
      el.setAttribute('role', 'separator');
      el.setAttribute('aria-orientation', sp.dir === 'v' ? 'vertical' : 'horizontal');
      el.setAttribute('aria-label', sp.dir === 'v' ? T('workspace.splitter_v') : T('workspace.splitter_h'));
      el.tabIndex = 0;
      el.addEventListener('pointerdown', (ev) => startDrag(ev, el, sp));
      el.addEventListener('keydown', (ev) => splitterKey(ev, sp));
      el.addEventListener('dblclick', () => resetSplit(sp));
      els.grid.prepend(el);          // first in DOM order => reachable with Tab before the terminals (which swallow Tab)
      S.splitters.push({ el, sp });
    }
    positionSplitters();
  }

  function geometry() {
    const cs = getComputedStyle(els.grid);
    const cols = trackPx(cs.gridTemplateColumns), rows = trackPx(cs.gridTemplateRows);
    if (!cols.length || !rows.length || [...cols, ...rows].some((n) => !isFinite(n))) return null;
    const xs = [], ys = [];
    let x = parseFloat(cs.paddingLeft) || 0, y = parseFloat(cs.paddingTop) || 0;
    for (const w of cols) { xs.push(x); x += w + GAP; }
    for (const h of rows) { ys.push(y); y += h + GAP; }
    return { cols, rows, xs, ys };
  }

  function positionSplitters() {
    if (!S.splitters.length) return;
    const g = geometry();
    for (const { el, sp } of S.splitters) {
      if (!g || sp.k >= (sp.dir === 'v' ? g.cols.length : g.rows.length)) { el.hidden = true; continue; }
      el.hidden = false;
      if (sp.dir === 'v') {
        el.style.left = (g.xs[sp.k] - GAP) + 'px'; el.style.width = GAP + 'px';
        el.style.top = g.ys[sp.r0] + 'px'; el.style.height = (g.ys[sp.r1 - 1] + g.rows[sp.r1 - 1] - g.ys[sp.r0]) + 'px';
      } else {
        el.style.top = (g.ys[sp.k] - GAP) + 'px'; el.style.height = GAP + 'px';
        el.style.left = g.xs[sp.c0] + 'px'; el.style.width = (g.xs[sp.c1 - 1] + g.cols[sp.c1 - 1] - g.xs[sp.c0]) + 'px';
      }
    }
  }

  /** Set the two tracks next to splitter `sp` to (a, total-a) px and store them as normalised fr weights. */
  function setSplit(sp, tracks, a) {
    const lay = effLayout(), z = sizesFor(lay.id, lay), vertical = sp.dir === 'v';
    const i = sp.k - 1, total = tracks[i] + tracks[i + 1];
    const min = Math.min(vertical ? MIN_W : MIN_H, total / 2);
    const t = tracks.slice();
    t[i] = Math.min(total - min, Math.max(min, a));
    t[i + 1] = total - t[i];
    const sum = t.reduce((s, v) => s + v, 0);
    const norm = t.map((v) => v / sum * t.length);
    if (vertical) z.cols = norm; else z.rows = norm;
    applyTemplate(z);
    positionSplitters();
  }

  function startDrag(ev, el, sp) {
    if (ev.button !== 0) return;
    ev.preventDefault();
    const vertical = sp.dir === 'v';
    const cs = getComputedStyle(els.grid);
    const tracks = trackPx(vertical ? cs.gridTemplateColumns : cs.gridTemplateRows);
    const i = sp.k - 1, a0 = tracks[i], start = vertical ? ev.clientX : ev.clientY;
    try { el.setPointerCapture(ev.pointerId); } catch { /* ignore */ }
    el.classList.add('drag');
    document.body.classList.add('dragging');
    const move = (e) => setSplit(sp, tracks, a0 + (vertical ? e.clientX : e.clientY) - start);
    const up = () => {
      el.removeEventListener('pointermove', move);
      el.removeEventListener('pointerup', up);
      el.removeEventListener('pointercancel', up);
      el.classList.remove('drag');
      document.body.classList.remove('dragging');
      persist();
    };
    el.addEventListener('pointermove', move);
    el.addEventListener('pointerup', up);
    el.addEventListener('pointercancel', up);
  }
  function splitterKey(ev, sp) {
    const vertical = sp.dir === 'v';
    const dec = vertical ? 'ArrowLeft' : 'ArrowUp', inc = vertical ? 'ArrowRight' : 'ArrowDown';
    if (ev.key !== dec && ev.key !== inc) return;
    ev.preventDefault();
    const cs = getComputedStyle(els.grid);
    const tracks = trackPx(vertical ? cs.gridTemplateColumns : cs.gridTemplateRows);
    const step = (ev.shiftKey ? 96 : 24) * (ev.key === inc ? 1 : -1);
    setSplit(sp, tracks, tracks[sp.k - 1] + step);
    persist();
  }
  function resetSplit(sp) {
    const lay = effLayout(), z = sizesFor(lay.id, lay);
    if (sp.dir === 'v') z.cols = Array(lay.cols).fill(1); else z.rows = Array(lay.rows).fill(1);
    applyTemplate(z);
    positionSplitters();
    persist();
  }

  function ensureOpened(pane) {
    if (pane.opened) return true;
    const host = pane.el.host;
    if (!host.isConnected || host.clientWidth < 40 || host.clientHeight < 40) return false;
    pane.term.open(host);
    pane.opened = true;
    pane.ro = new ResizeObserver(() => scheduleFit(pane));
    pane.ro.observe(host);
    fitPane(pane);
    if (pane.ws && pane.ws.readyState === WebSocket.OPEN) sendResize(pane, true);
    if (pane.wantFocus) { pane.wantFocus = false; pane.term.focus(); }
    return true;
  }
  function fitPane(pane) {
    if (!pane.opened) return;
    const h = pane.el.host;
    if (h.clientWidth < 40 || h.clientHeight < 40) return;
    try { pane.fit.fit(); } catch { /* not measurable yet */ }
  }
  function scheduleFit(pane) {
    if (pane.fitRaf) return;
    pane.fitRaf = requestAnimationFrame(() => { pane.fitRaf = 0; fitPane(pane); });
  }
  function scheduleResizeSend(pane) {
    clearTimeout(pane.resizeTimer);
    pane.resizeTimer = setTimeout(() => sendResize(pane), 120);
  }
  function sendResize(pane, force) {
    const ws = pane.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN || !pane.opened) return;
    const { cols, rows } = pane.term, l = pane.lastSent;
    if (!force && l && l.cols === cols && l.rows === rows) return;
    pane.lastSent = { cols, rows };
    ws.send(JSON.stringify({ type: 'resize', cols, rows }));
  }
  /** Keystrokes / pastes go out as binary frames. The server closes a socket that sends a frame > 64 KiB, so a big
   *  paste is split into smaller frames (order is preserved; the PTY just receives the concatenated bytes). */
  function sendInput(pane, bytes) {
    const ws = pane.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    for (let i = 0; i < bytes.length; i += WS_CHUNK) ws.send(bytes.subarray(i, i + WS_CHUNK));
  }
  function paneDims(pane) {
    return pane.opened && pane.term.cols > 1 ? { cols: pane.term.cols, rows: pane.term.rows } : null;
  }
  function estimateDims() {
    for (const p of visibleList()) { const d = paneDims(p); if (d) return d; }
    for (const p of S.panes) { const d = paneDims(p); if (d) return d; }
    return DEF_DIMS;
  }
  function focusTerm(pane) {
    if (!pane) return;
    if (pane.opened) pane.term.focus(); else pane.wantFocus = true;
  }

  function wsUrl(pane) {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${proto}//${location.host}/ws/term/${encodeURIComponent(pane.termId)}?t=${encodeURIComponent(TOKEN)}`;
  }
  function closeSocket(pane) {
    const ws = pane.ws;
    pane.ws = null;
    if (ws) { ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null; try { ws.close(); } catch { /* ignore */ } }
  }

  function connect(pane) {
    if (!pane.termId || pane.closing) return;
    closeSocket(pane);
    clearTimeout(pane.retryTimer);
    if (pane.state !== 'exited' && pane.state !== 'live') setState(pane, pane.retry ? 'reconnecting' : 'connecting');
    let ws;
    try { ws = new WebSocket(wsUrl(pane)); } catch { onSocketGone(pane); return; }
    ws.binaryType = 'arraybuffer';
    pane.ws = ws;
    ws.onopen = () => {
      if (pane.ws !== ws) return;
      pane.retry = 0;
      pane.lastSent = null;
      pane.attachAt = pane.lastMsgAt = performance.now();
      pane.pendingWrites = 0;
      if (pane.hadData) { pane.term.reset(); pane.hadData = false; }   // the server replays scrollback: start clean to avoid duplicates
      if (pane.state !== 'exited') setState(pane, 'live');
      if (pane.opened) { fitPane(pane); sendResize(pane, true); }
    };
    ws.onmessage = (ev) => {
      if (pane.ws !== ws) return;
      if (typeof ev.data === 'string') { onControl(pane, ev.data); return; }
      pane.hadData = true;
      pane.lastMsgAt = performance.now();
      pane.pendingWrites++;
      pane.term.write(new Uint8Array(ev.data), () => {
        pane.pendingWrites = Math.max(0, pane.pendingWrites - 1);
        if (!pane.answered && pane.pendingWrites === 0) { pane.answered = true; persist(); }   // startup queries have been answered once
      });
    };
    ws.onclose = (ev) => {
      if (pane.ws !== ws) return;
      pane.ws = null;
      onSocketGone(pane, ev && ev.code);
    };
    ws.onerror = () => { /* onclose follows */ };
  }

  function onControl(pane, text) {
    let m = null;
    try { m = JSON.parse(text); } catch { return; }
    if (!m || m.type !== 'exit') return;
    const info = m.code != null ? T('workspace.exit_with_code', { code: m.code }) : T('workspace.ov_process_stopped');
    if (pane.state === 'exited') { pane.exitInfo = info; renderOverlay(pane); }   // replay of a dead terminal: now we know the exit code
    else markExited(pane, info);
  }

  function markExited(pane, info) {
    if (pane.state === 'exited') return;
    clearTimeout(pane.retryTimer);
    if (pane.hadData) pane.term.write('\r\n\x1b[2m[' + (info || T('workspace.ov_process_stopped')) + ']\x1b[0m\r\n');
    setState(pane, 'exited', { exitInfo: info || T('workspace.ov_process_stopped') });
    persist();
  }

  function onSocketGone(pane, code) {
    if (pane.closing || pane.state === 'exited') return;
    if (code === 4001) {
      // The server dropped this client because it lagged behind: re-attach right away to resync.
      // The scrollback is replayed on the new socket and onopen calls term.reset() first.
      const now = Date.now(), quick = !pane.lagAt || now - pane.lagAt > 3000;
      pane.lagAt = now;
      clearTimeout(pane.retryTimer);
      pane.retryTimer = setTimeout(() => { if (!pane.closing && pane.state !== 'exited') connect(pane); }, quick ? 0 : 1000);
      return;
    }
    if (pane.state !== 'reconnecting') setState(pane, 'reconnecting');
    const delay = Math.min(15000, 500 * Math.pow(2, Math.min(pane.retry, 5)));
    pane.retry++;
    clearTimeout(pane.retryTimer);
    pane.retryTimer = setTimeout(() => recheck(pane), delay);
  }
  /** After an unexpected socket drop: reconnect only if the terminal is still alive on the server. */
  async function recheck(pane) {
    if (pane.closing || pane.state === 'exited') return;
    let list;
    try { list = await listTerms(); } catch (e) {
      if (e.status === 403) { markExited(pane, T('workspace.session_invalid')); return; }
      onSocketGone(pane);        // server unreachable: keep retrying with backoff
      return;
    }
    if (pane.closing || pane.state === 'exited') return;
    const t = list.find((x) => x.id === pane.termId);
    if (!t) markExited(pane, T('workspace.term_gone'));
    else if (!t.alive) markExited(pane, T('workspace.ov_process_stopped'));
    else connect(pane);
  }

  async function listTerms() {
    const r = await api('/api/term', { quiet: true });
    const a = Array.isArray(r) ? r : (r && (r.terminals || r.items)) || [];
    return a.filter((t) => t && t.id != null).map((t) => ({
      id: String(t.id), source: t.source || '', cwd: t.cwd || '', title: t.title || '', alive: t.alive !== false, createdMs: t.createdMs || 0,
    }));
  }

  async function spawnTerm(spec, dims) {
    const body = { source: spec.source, cwd: spec.cwd, cols: dims.cols, rows: dims.rows };
    if (spec.resume) body.resume = spec.resume;
    if (spec.model) body.model = spec.model;
    if (spec.effort) body.effort = spec.effort;
    let r;
    try { r = await api('/api/term', { method: 'POST', body, quiet: true }); }
    catch (e) {
      // REST errors: 400 bad request, 404, 429 (more than 32 live terminals), 500 - the server text is shown in the dialog / pane overlay
      if (e.status === 429) throw new Error(T('workspace.err_limit_32') + (e.detail ? ' (' + e.detail + ')' : ''));
      if (e.status === 400 || e.status === 404) throw new Error(T('workspace.err_open_failed', { detail: e.detail || e.message }));
      throw e;
    }
    if (!r || r.id == null) throw new Error(T('workspace.err_no_term_id'));
    return { id: String(r.id), title: r.title || '' };
  }

  /** Create a terminal on the server and show it in a new pane. Throws (message in Vietnamese or server text) on failure. */
  async function createPane(spec) {
    if (S.panes.length >= MAX_PANES) throw new Error(T('workspace.err_max_panes', { n: MAX_PANES }));
    if (spec.resume) {
      const ex = S.panes.find((p) => p.resume === spec.resume && p.source === spec.source && isAlive(p));
      if (ex) { focusPane(ex); toast(T('toast.workspace_already_open')); return ex; }
    }
    const info = await spawnTerm(spec, estimateDims());
    const pane = newPane({
      source: spec.source, cwd: spec.cwd, resume: spec.resume, model: spec.model, effort: spec.effort, title: spec.title || info.title, termId: info.id,
      state: 'connecting', freshUntil: Date.now() + FRESH_MS,
    });
    const old = S.focus;
    S.panes.push(pane);
    if (!makeRoom(S.panes.length) && !isNarrow()) toast(T('toast.workspace_layout_full'));
    if (S.maxKey) S.maxKey = pane.key;
    S.focus = pane.key;
    ensureInSlots(pane, old);
    pane.wantFocus = true;
    renderLayout();
    connect(pane);
    persist();
    return pane;
  }

  function focusPane(pane, opts = {}) {
    if (!pane) return;
    const old = S.focus;
    if (S.maxKey) S.maxKey = pane.key;
    S.focus = pane.key;
    if (ensureInSlots(pane, old)) persist();
    renderLayout();
    if (opts.term !== false) focusTerm(pane);
  }

  function toggleMax(pane) {
    if (!pane) return;
    if (S.maxKey === pane.key) S.maxKey = null;
    else { S.maxKey = pane.key; S.focus = pane.key; }
    renderLayout();
    focusTerm(pane);
  }

  async function closePane(pane) {
    if (isAlive(pane) && !(await CM.confirm(T('workspace.confirm_close', { name: basename(pane.cwd) }), { okText: T('workspace.confirm_close_ok'), danger: true }))) return;
    const termId = pane.termId;
    removePane(pane);
    if (!termId) return;
    try { await api('/api/term/' + encodeURIComponent(termId), { method: 'DELETE', quiet: true }); }
    catch (e) { if (e.status !== 404) toast(T('toast.workspace_stop_failed', { id: termId, message: e.message }), 'err'); }
  }

  function removePane(pane) {
    pane.closing = true;
    clearTimeout(pane.retryTimer);
    clearTimeout(pane.resizeTimer);
    closeSocket(pane);
    if (pane.ro) pane.ro.disconnect();
    if (pane.fitRaf) cancelAnimationFrame(pane.fitRaf);
    if (pane.titleRaf) cancelAnimationFrame(pane.titleRaf);
    try { pane.term.dispose(); } catch { /* ignore */ }
    pane.el.root.remove();
    const idx = S.panes.indexOf(pane);
    S.panes = S.panes.filter((p) => p !== pane);
    if (S.maxKey === pane.key) S.maxKey = null;
    if (S.focus === pane.key) S.focus = (S.panes[Math.min(idx, S.panes.length - 1)] || {}).key || null;
    renderLayout();
    const f = byKey(S.focus);
    if (f) focusTerm(f);
    persist();
  }

  async function restartPane(pane, opts = {}) {
    if (pane.restarting) return;
    if (isAlive(pane) && !(await CM.confirm(T('workspace.confirm_restart', { name: basename(pane.cwd) }), { okText: T('workspace.confirm_restart_ok'), danger: true }))) return;
    pane.restarting = true;
    const oldId = pane.termId;
    closeSocket(pane);
    clearTimeout(pane.retryTimer);
    pane.termId = null;
    if (opts.resume !== undefined) pane.resume = opts.resume;
    setState(pane, 'starting', { error: '', exitInfo: '' });
    if (oldId) { try { await api('/api/term/' + encodeURIComponent(oldId), { method: 'DELETE', quiet: true }); } catch { /* already gone */ } }
    if (pane.closing) return;
    pane.term.reset();
    pane.hadData = false;
    pane.oscTitle = '';
    pane.retry = 0;
    try {
      const info = await spawnTerm({ source: pane.source, cwd: pane.cwd, resume: pane.resume, model: pane.model, effort: pane.effort }, paneDims(pane) || estimateDims());
      if (pane.closing) { api('/api/term/' + encodeURIComponent(info.id), { method: 'DELETE', quiet: true }).catch(() => {}); return; }
      pane.termId = info.id;
      pane.freshUntil = Date.now() + FRESH_MS;
      pane.answered = false;
      if (info.title) pane.title = info.title;
      pane.wantFocus = true;
      connect(pane);
      persist();
      focusTerm(pane);
    } catch (e) {
      if (!pane.closing) { setState(pane, 'error', { error: e.message || String(e) }); persist(); }
    } finally {
      pane.restarting = false;
    }
  }

  function moveFocus(dir) {
    if (!S.panes.some((p) => p.el.root.contains(document.activeElement))) {   // focus is on the toolbar etc.: go back into the terminal
      const p = byKey(S.focus);
      if (p) focusPane(p);
      return;
    }
    const vis = visibleList().filter((p) => !p.el.root.hidden);
    if (vis.length < 2) { if (dir === 'up') $('#wsNew').focus(); return; }
    const cur = vis.find((p) => p.key === S.focus) || vis[0];
    const r = cur.el.root.getBoundingClientRect();
    let best = null, bestScore = Infinity;
    for (const p of vis) {
      if (p === cur) continue;
      const q = p.el.root.getBoundingClientRect();
      let gap, overlap;
      if (dir === 'left')       { gap = r.left - q.right;  overlap = Math.min(r.bottom, q.bottom) - Math.max(r.top, q.top); }
      else if (dir === 'right') { gap = q.left - r.right;  overlap = Math.min(r.bottom, q.bottom) - Math.max(r.top, q.top); }
      else if (dir === 'up')    { gap = r.top - q.bottom;  overlap = Math.min(r.right, q.right) - Math.max(r.left, q.left); }
      else                      { gap = q.top - r.bottom;  overlap = Math.min(r.right, q.right) - Math.max(r.left, q.left); }
      if (gap < -2 || overlap <= 0) continue;
      const score = Math.max(0, gap) * 1000 - overlap;
      if (score < bestScore) { bestScore = score; best = p; }
    }
    if (best) focusPane(best);
    else if (dir === 'up') $('#wsNew').focus();     // top edge: leave the terminals (they swallow Tab) for the toolbar
  }

  let persistTimer = 0, lastPersistErr = 0;
  function snapshot() {
    return {
      v: 1, layout: S.layout, sizes: S.sizes, focus: S.focus,
      panes: S.panes.map((p) => ({ key: p.key, termId: p.termId, source: p.source, cwd: p.cwd, resume: p.resume || null, title: p.title || '', ans: p.answered ? 1 : 0 })),
    };
  }
  function persist() {
    if (!S.ready) return;
    clearTimeout(persistTimer);
    persistTimer = setTimeout(flushPersist, 400);
  }
  async function flushPersist(keepalive) {
    clearTimeout(persistTimer);
    persistTimer = 0;
    if (!S.ready) return;
    try { await api('/api/workspace', { method: 'PUT', body: snapshot(), quiet: true, keepalive: keepalive === true }); }
    catch (e) {
      if (Date.now() - lastPersistErr > 30000) { lastPersistErr = Date.now(); toast(T('toast.workspace_persist_failed', { message: e.message }), 'err'); }
    }
  }
  window.addEventListener('pagehide', () => { if (persistTimer) flushPersist(true); });
  document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'hidden' && persistTimer) flushPersist(true); });

  const dlg = {};
  const combo = { items: [], active: -1, timer: 0, seq: 0, escAt: 0 };

  async function loadSources() {
    if (S.sources.length) return;
    try {
      const r = await api('/api/sources', { quiet: true });
      const a = Array.isArray(r) ? r : (r && r.sources) || [];
      S.sources = a.map((s) => (typeof s === 'string' ? { name: s } : s)).filter((s) => s && s.name);
    } catch { /* fall back below */ }
    if (!S.sources.length) S.sources = (CM.getData().sources || []).map((n) => ({ name: n }));
  }
  function fillSources(preferred) {
    const sel = dlg.source;
    sel.textContent = '';
    for (const s of S.sources) {
      const o = new Option(s.name + (s.distro && !s.name.includes(s.distro) ? ` (WSL: ${s.distro})` : ''), s.name);
      if (s.dir) o.title = String(s.dir);
      sel.append(o);
    }
    const want = [preferred, store.get('cm-last-source')].find((n) => n && S.sources.some((s) => s.name === n));
    sel.value = want || (S.sources[0] && S.sources[0].name) || '';
  }

  function showErr(msg) {
    dlg.err.textContent = msg || '';
    dlg.err.hidden = !msg;
  }
  function setBusy(b) {
    dlg.busy = b;
    for (const e of [dlg.source, dlg.cwd, dlg.ok, dlg.model, dlg.effort]) e.disabled = b;
    dlg.resume.disabled = b || dlg.resume.options.length < 2;
    dlg.ok.textContent = b ? T('workspace.dlg_busy') : T('workspace.dlg_ok');
  }

  function refreshResume(preselect) {
    const source = dlg.source.value, cwd = normPath(dlg.cwd.value);
    const keep = preselect !== undefined ? preselect : dlg.resume.value;
    const list = cwd ? CM.getData().sessions
      .filter((s) => s.status === 'active' && s.source === source && s.cwd && normPath(s.cwd) === cwd)
      .sort((a, b) => (a.last < b.last ? 1 : -1)).slice(0, 60) : [];
    dlg.resume.textContent = '';
    dlg.resume.append(new Option(T('workspace.dlg_new_session'), ''));
    for (const s of list) dlg.resume.append(new Option(`${s.title} - ${CM.ago(s.last)} - ${s.sessionId.slice(0, 8)}`, s.sessionId));
    if (keep && list.some((s) => s.sessionId === keep)) dlg.resume.value = keep;
    dlg.resume.disabled = list.length === 0;
    dlg.resumeHint.textContent = !cwd ? T('workspace.dlg_resume_hint_no_cwd')
      : list.length ? T('workspace.dlg_resume_hint_some', { n: list.length }) : T('workspace.dlg_resume_hint_none');
  }

  function localFolders(source, q) {
    const m = new Map();
    for (const s of CM.getData().sessions) {
      if (!s.cwd || s.source !== source) continue;
      const f = m.get(s.cwd) || { source, cwd: s.cwd, sessions: 0, lastMs: 0 };
      f.sessions++;
      f.lastMs = Math.max(f.lastMs, Date.parse(s.last) || 0);
      m.set(s.cwd, f);
    }
    const needle = q.toLowerCase();
    return [...m.values()].filter((f) => !needle || f.cwd.toLowerCase().includes(needle)).sort((a, b) => b.lastMs - a.lastMs);
  }

  async function queryFolders(q) {
    const seq = ++combo.seq;
    const source = dlg.source.value;
    let items;
    try {
      const r = await api(`/api/folders?source=${encodeURIComponent(source)}&q=${encodeURIComponent(q)}`, { quiet: true });
      const a = Array.isArray(r) ? r : (r && (r.folders || r.items)) || [];
      items = a.map((f) => (typeof f === 'string' ? { cwd: f } : f)).filter((f) => f && f.cwd && (!f.source || f.source === source));
    } catch { items = localFolders(source, q); }
    if (seq !== combo.seq || !dlg.el.open) return;
    combo.items = items.slice(0, 15);
    combo.active = -1;
    renderList();
  }
  function scheduleFolderQuery() {
    clearTimeout(combo.timer);
    combo.timer = setTimeout(() => queryFolders(dlg.cwd.value.trim()), 130);
  }
  function renderList() {
    const ul = dlg.list;
    ul.textContent = '';
    const typed = dlg.cwd.value.trim();
    if (!combo.items.length) {
      if (!typed) { closeList(); return; }
      ul.append(mk('li', 'none', T('workspace.dlg_no_suggestions')));
    }
    combo.items.forEach((f, i) => {
      const li = mk('li');
      li.id = 'npo-' + i;
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(i === combo.active));
      li.dataset.i = String(i);
      li.append(mk('span', 'fo-path', LRM + f.cwd + LRM));   // LRM both sides: keeps a leading '/' in place inside the rtl ellipsis box
      const meta = [f.sessions ? T('workspace.dlg_folder_sessions', { n: f.sessions }) : '', f.lastMs ? CM.ago(f.lastMs) : ''].filter(Boolean).join(' · ');
      if (meta) li.append(mk('span', 'fo-meta', meta));
      ul.append(li);
    });
    ul.hidden = false;
    dlg.cwd.setAttribute('aria-expanded', 'true');
    if (combo.active >= 0) dlg.cwd.setAttribute('aria-activedescendant', 'npo-' + combo.active); else dlg.cwd.removeAttribute('aria-activedescendant');
  }
  function closeList() {
    clearTimeout(combo.timer);
    combo.seq++;
    dlg.list.hidden = true;
    dlg.cwd.setAttribute('aria-expanded', 'false');
    dlg.cwd.removeAttribute('aria-activedescendant');
    combo.active = -1;
  }
  function pickFolder(i) {
    const f = combo.items[i];
    if (!f) return;
    dlg.cwd.value = f.cwd;
    closeList();
    refreshResume();
    dlg.cwd.focus();
  }
  function moveActive(d) {
    if (dlg.list.hidden || !combo.items.length) return;
    combo.active = (combo.active + d + combo.items.length) % combo.items.length;
    $$('li[role=option]', dlg.list).forEach((li, i) => li.setAttribute('aria-selected', String(i === combo.active)));
    dlg.cwd.setAttribute('aria-activedescendant', 'npo-' + combo.active);
    const li = $('#npo-' + combo.active, dlg.list);
    if (li) li.scrollIntoView({ block: 'nearest' });
  }

  function fillOpts(sel, first, list, want) {
    sel.textContent = '';
    sel.append(new Option(first, ''));
    for (const m of list) sel.append(new Option(m, m));
    sel.value = list.includes(want) ? want : '';
    sel.closest('.field').hidden = !list.length;
  }

  async function openNewPane(prefill = {}) {
    if (CM.getTab() !== 'workspace') CM.showTab('workspace');
    if (dlg.el.open || document.querySelector('dialog[open]')) return;
    await S.readyP;
    await loadSources();
    fillSources(prefill.source);
    dlg.cwd.value = prefill.cwd || store.get('cm-last-cwd') || '';
    fillOpts(dlg.model, T('workspace.dlg_default'), CM.settings.models(), prefill.model || store.get('cm-last-model'));
    fillOpts(dlg.effort, T('workspace.dlg_default'), CM.settings.efforts(), prefill.effort || store.get('cm-last-effort'));
    showErr(prefill.error || '');
    closeList();
    refreshResume(prefill.resume || '');
    setBusy(false);
    dlg.el.showModal();
    dlg.cwd.focus();
    dlg.cwd.select();
    if (!prefill.cwd) scheduleFolderQuery();
  }

  async function submitDialog(e) {
    e.preventDefault();
    if (dlg.busy) return;
    const spec = { source: dlg.source.value, cwd: dlg.cwd.value.trim(), resume: dlg.resume.value || null, model: dlg.model.value, effort: dlg.effort.value };
    if (!spec.source) { showErr(T('workspace.dlg_err_no_source')); return; }
    if (!spec.cwd) { showErr(T('workspace.dlg_err_no_cwd')); dlg.cwd.focus(); return; }
    closeList();
    showErr('');
    setBusy(true);
    try {
      if (spec.resume) {
        const s = CM.getData().sessions.find((x) => x.sessionId === spec.resume && x.source === spec.source);
        if (s) spec.title = s.title;
      }
      await createPane(spec);
      store.set('cm-last-source', spec.source);
      store.set('cm-last-cwd', spec.cwd);
      store.set('cm-last-model', spec.model || '');
      store.set('cm-last-effort', spec.effort || '');
      dlg.el.close();
    } catch (err) {
      showErr(err.message || String(err));
    } finally {
      setBusy(false);
    }
  }

  /** "Open in Workspace" from the Sessions drawer: resume that session in a new pane (same cwd + source). */
  async function openSession(s) {
    if (!s || !s.cwd) { toast(T('toast.workspace_session_no_cwd'), 'err'); return; }
    CM.showTab('workspace');
    toast(T('toast.workspace_opening_session'));
    await S.readyP;
    try { await createPane({ source: s.source, cwd: s.cwd, resume: s.sessionId, title: s.title }); }
    catch (e) { openNewPane({ source: s.source, cwd: s.cwd, resume: s.sessionId, error: e.message || String(e) }); }
  }

  function restore(saved, terms) {
    const sv = saved && typeof saved === 'object' ? saved : {};
    if (LAYOUTS[sv.layout]) S.layout = sv.layout;
    S.sizes = sanitizeSizes(sv.sizes);
    const byId = new Map((terms || []).map((t) => [t.id, t]));
    const keys = new Set(), used = new Set();
    for (const sp of Array.isArray(sv.panes) ? sv.panes : []) {
      if (!sp || typeof sp !== 'object' || !sp.source || !sp.cwd) continue;
      const termId = sp.termId != null && sp.termId !== '' ? String(sp.termId) : null;
      if (termId && used.has(termId)) continue;
      if (termId) used.add(termId);
      let key = typeof sp.key === 'string' && sp.key && !keys.has(sp.key) ? sp.key : uid();
      keys.add(key);
      const t = termId ? byId.get(termId) : null;
      const pane = newPane({
        key, termId, source: String(sp.source), cwd: String(sp.cwd), resume: sp.resume || null, title: sp.title ? String(sp.title) : '',
        state: 'connecting', freshUntil: t && t.createdMs ? t.createdMs + FRESH_MS : 0, answered: !!sp.ans,
      });
      S.panes.push(pane);
      if (!termId) setState(pane, 'exited', { exitInfo: T('workspace.no_terminal_yet') });
      else if (!terms) connect(pane);                                   // list unavailable: let the reconnect loop decide
      else if (!t) setState(pane, 'exited', { exitInfo: T('workspace.term_gone_restart') });
      else if (!t.alive) { setState(pane, 'exited', { exitInfo: T('workspace.ov_process_stopped') }); connect(pane); }   // replay the final output
      else connect(pane);
    }
    // terminals that are alive on the server but unknown to the saved layout (e.g. layout file lost): adopt them
    for (const t of terms || []) {
      if (used.has(t.id)) continue;
      // dead terminals stay listed until DELETE (the server keeps at most 64 entries): forget the ones no pane refers to
      if (!t.alive) { api('/api/term/' + encodeURIComponent(t.id), { method: 'DELETE', quiet: true }).catch(() => {}); continue; }
      if (S.panes.length >= MAX_PANES) continue;
      const pane = newPane({
        termId: t.id, source: t.source || (S.sources[0] && S.sources[0].name) || '', cwd: t.cwd || '', title: t.title,
        state: 'connecting', freshUntil: t.createdMs ? t.createdMs + FRESH_MS : 0,
      });
      S.panes.push(pane);
      connect(pane);
    }
    makeRoomQuiet(S.panes.length);
    if (typeof sv.focus === 'string' && byKey(sv.focus)) S.focus = sv.focus;
  }
  function makeRoomQuiet(n) {
    let id = S.layout;
    while (LAYOUTS[id].areas.length < n && GROW[id]) id = GROW[id];
    S.layout = id;
  }

  function init() {
    els.grid = $('#wsGrid'); els.strip = $('#wsStrip'); els.layouts = $('#wsLayouts'); els.sum = $('#wsSum'); els.badge = $('#wsBadge');
    Object.assign(dlg, {
      el: $('#newPaneDlg'), form: $('#npForm'), source: $('#npSource'), cwd: $('#npCwd'), list: $('#npList'), resume: $('#npResume'), model: $('#npModel'), effort: $('#npEffort'),
      resumeHint: $('#npResumeHint'), err: $('#npErr'), ok: $('#npOk'), cancel: $('#npCancel'), busy: false,
    });
    S.readyP = Promise.resolve();
    CM.workspace = { openSession, newPane: openNewPane, liveCount: () => S.panes.filter(isAlive).length, panes: () => S.panes.slice() };

    if (typeof Terminal === 'undefined' || !window.FitAddon) {
      els.grid.append(mk('div', 'empty', T('workspace.xterm_missing')));
      toast(T('toast.workspace_xterm_missing_toast'), 'err');
      return;
    }
    buildLayoutButtons();
    wire();
    updateSummary();
    S.readyP = Promise.all([
      api('/api/workspace', { quiet: true }).catch(() => null),
      listTerms().catch(() => null),
      loadSources(),
    ]).then(([saved, terms]) => {
      restore(saved, terms);
      S.ready = true;
      renderLayout();
      if (CM.getTab() === 'workspace') onShow();
    });
  }

  function onShow() {
    renderLayout();
    requestAnimationFrame(() => { const p = byKey(S.focus); if (p) focusTerm(p); });
  }

  function wire() {
    els.layouts.addEventListener('click', (e) => { const b = e.target.closest('button[data-layout]'); if (b) setLayout(b.dataset.layout); });
    $('#wsNew').addEventListener('click', () => openNewPane());
    els.strip.addEventListener('click', (e) => { const b = e.target.closest('button[data-key]'); if (b) focusPane(byKey(b.dataset.key)); });
    els.grid.addEventListener('click', (e) => {
      const b = e.target.closest('[data-act]');
      if (!b) return;
      if (b.dataset.act === 'new') { openNewPane(); return; }
      const root = b.closest('.pane');
      const pane = root && byKey(root.dataset.key);
      if (!pane) return;
      const act = b.dataset.act;
      if (act === 'close') closePane(pane);
      else if (act === 'max') toggleMax(pane);
      else if (act === 'restart') restartPane(pane);
      else if (act === 'editor') CM.openEditor({ source: pane.source, cwd: pane.cwd });
      else if (act === 'continue') { const s = latestSession(pane); if (s) restartPane(pane, { resume: s.sessionId }); }
    });
    els.grid.addEventListener('dblclick', (e) => {
      const hd = e.target.closest('.pane-hd');
      if (!hd || e.target.closest('button')) return;
      toggleMax(byKey(hd.parentNode.dataset.key));
    });
    els.grid.addEventListener('pointerdown', (e) => {
      const root = e.target.closest('.pane');
      const p = root && byKey(root.dataset.key);
      if (p && S.focus !== p.key) { S.focus = p.key; markFocus(); renderStrip(); }
    }, true);
    els.grid.addEventListener('click', (e) => {          // clicking a header focuses its terminal
      const hd = e.target.closest('.pane-hd');
      if (hd && !e.target.closest('button')) focusTerm(byKey(hd.parentNode.dataset.key));
    });

    // dialog
    dlg.form.addEventListener('submit', submitDialog);
    dlg.cancel.addEventListener('click', () => dlg.el.close());
    dlg.source.addEventListener('change', () => { refreshResume(''); scheduleFolderQuery(); });
    dlg.cwd.addEventListener('input', () => { refreshResume(); scheduleFolderQuery(); });
    dlg.cwd.addEventListener('focus', () => { if (dlg.list.hidden && dlg.el.open) scheduleFolderQuery(); });
    dlg.cwd.addEventListener('blur', closeList);
    dlg.cwd.addEventListener('keydown', (e) => {
      const open = !dlg.list.hidden;
      if (e.key === 'ArrowDown') { e.preventDefault(); if (!open) queryFolders(dlg.cwd.value.trim()); else moveActive(1); }
      else if (e.key === 'ArrowUp') { e.preventDefault(); moveActive(-1); }
      else if (e.key === 'Enter' && open && combo.active >= 0) { e.preventDefault(); pickFolder(combo.active); }
      else if (e.key === 'Escape' && open) { e.preventDefault(); e.stopPropagation(); combo.escAt = performance.now(); closeList(); }
      else if (e.key === 'Tab') closeList();
    });
    dlg.el.addEventListener('cancel', (e) => { if (!dlg.list.hidden || performance.now() - (combo.escAt || -1e9) < 150) e.preventDefault(); });
    dlg.list.addEventListener('mousedown', (e) => {
      const li = e.target.closest('li[data-i]');
      e.preventDefault();                                // keep focus in the input
      if (li) pickFolder(+li.dataset.i);
    });
    dlg.el.addEventListener('close', () => { closeList(); setBusy(false); });

    // global shortcuts (capture phase so they win over the focused terminal; they never reach the PTY)
    document.addEventListener('keydown', (e) => {
      if (e.isComposing || document.querySelector('dialog[open]')) return;
      if (isNewPaneKey(e)) { e.preventDefault(); e.stopPropagation(); openNewPane(); return; }
      if (CM.getTab() === 'workspace' && isFocusKey(e)) { e.preventDefault(); e.stopPropagation(); moveFocus(ARROWS[e.key]); }
    }, true);

    document.addEventListener('cm:tab', (e) => { if (e.detail === 'workspace') onShow(); });
    document.addEventListener('cm:sessions', () => { for (const p of S.panes) { renderHeader(p); if (p.state === 'exited') renderOverlay(p); } });
    NARROW.addEventListener('change', () => { renderLayout(); const p = byKey(S.focus); if (p) focusTerm(p); });
    new ResizeObserver(() => positionSplitters()).observe(els.grid);
    new MutationObserver(() => requestAnimationFrame(applyTermTheme)).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
    window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => requestAnimationFrame(applyTermTheme));
  }

  init();
})();
