'use strict';
/* claude-manager UI - shared helpers + Sessions and Usage tabs.
   The Workspace tab lives in workspace.js and talks to this file through window.CM. */
(() => {
  const T = window.t;   // i18n.js (loaded first); T() is not escaped: esc() it when it goes into innerHTML
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
  const fmt = (n) => n >= 1e9 ? (n / 1e9).toFixed(2) + 'B' : n >= 1e6 ? (n / 1e6).toFixed(2) + 'M' : n >= 1e3 ? (n / 1e3).toFixed(1) + 'k' : String(n);
  const fmtSize = (b) => b >= 1048576 ? (b / 1048576).toFixed(1) + ' MB' : b >= 1024 ? Math.round(b / 1024) + ' KB' : b + ' B';
  const ago = (v) => {
    if (!v) return '';
    const t = new Date(v).getTime();
    if (isNaN(t)) return '';
    const s = (Date.now() - t) / 1000;
    if (s < 60) return T('time.just_now');
    if (s < 3600) return T('time.minutes_ago', { n: Math.floor(s / 60) });
    if (s < 86400) return T('time.hours_ago', { n: Math.floor(s / 3600) });
    if (s < 86400 * 30) return T('time.days_ago', { n: Math.floor(s / 86400) });
    return new Date(t).toLocaleDateString(I18N.locale);
  };
  // Session titles, paths, prompts and terminal titles are untrusted: always escape before innerHTML.
  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const store = {
    get(k) { try { return localStorage.getItem(k); } catch { return null; } },
    set(k, v) { try { localStorage.setItem(k, v); } catch { /* ignore */ } },
  };

  const usd = (n) => {
    n = Number(n) || 0;
    if (n >= 1) return '$' + n.toLocaleString(I18N.locale, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    if (n === 0) return '$0.00';
    return n < 0.0001 ? '<$0.0001' : '$' + n.toFixed(4);
  };
  /** Label that shows text, or only an icon (with title / aria-label on the button) in icon mode. */
  const lb = (ic, text) => `<span class="ic" aria-hidden="true">${ic}</span><span class="tx">${esc(text)}</span>`;
  const hidden = (k) => !!(window.CM && CM.settings && CM.settings.isHidden(k));

  const TOKEN = (document.querySelector('meta[name="cm-token"]') || {}).content || '';

  let toastTimer = 0;
  let lastToast = { text: '', at: 0 };
  function toast(text, kind) {
    const el = $('#toast');
    const now = Date.now();
    if (text === lastToast.text && now - lastToast.at < 2500 && el.classList.contains('on')) { lastToast.at = now; return; }
    lastToast = { text, at: now };
    el.textContent = text;
    el.classList.toggle('err', kind === 'err');
    el.classList.add('on');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.remove('on'), kind === 'err' ? 5200 : 2600);
  }

  let authShown = false;
  function authLost(msg) {
    if (authShown) return;
    authShown = true;
    $('#authBannerText').textContent = msg || T('header.auth_lost');
    $('#authBanner').hidden = false;
  }

  class ApiError extends Error {
    constructor(message, status) { super(message); this.status = status; }
  }

  /** fetch wrapper: sends X-CM-Token, parses JSON, toasts on 403 / 5xx / network failure (unless opts.quiet). */
  async function api(path, opts = {}) {
    const { method = 'GET', body, quiet = false, keepalive = false } = opts;
    const m = method.toUpperCase();
    const headers = { 'X-CM-Token': TOKEN };
    let payload;
    // Contract: every non-GET request carries Content-Type: application/json (empty-body POST/PUT/PATCH send "{}")
    if (m !== 'GET' && m !== 'HEAD') headers['Content-Type'] = 'application/json';
    if (body !== undefined) payload = JSON.stringify(body);
    else if (m === 'POST' || m === 'PUT' || m === 'PATCH') payload = '{}';
    let res;
    try {
      res = await fetch(path, { method, headers, body: payload, keepalive, cache: 'no-store' });
    } catch {
      const err = new ApiError(T('api.network_error'), 0);
      if (!quiet) toast(err.message, 'err');
      throw err;
    }
    const text = await res.text();
    let json = null;
    if (text) { try { json = JSON.parse(text); } catch { json = null; } }
    if (!res.ok) {
      const detail = (json && (json.error || json.message)) || text.slice(0, 200) || res.statusText || ('HTTP ' + res.status);
      let msg = String(detail);
      if (res.status === 403) { msg = T('api.forbidden'); authLost(); }
      else if (res.status >= 500) msg = T('api.server_error', { status: res.status, detail });
      const err = new ApiError(msg, res.status);
      err.detail = String(detail);
      if (!quiet) toast(msg, 'err');
      throw err;
    }
    return json;
  }

  function confirmDialog(message, opts = {}) {
    const dlg = $('#confirmDlg');
    if (dlg.open) return Promise.resolve(false);
    const ok = $('#cfOk');
    $('#cfMsg').textContent = message;
    ok.textContent = opts.okText || T('common.agree');
    ok.classList.toggle('danger', !!opts.danger);
    ok.classList.toggle('primary', !opts.danger);
    dlg.returnValue = '';
    return new Promise((resolve) => {
      const done = () => { dlg.removeEventListener('close', done); resolve(dlg.returnValue === 'ok'); };
      dlg.addEventListener('close', done);
      dlg.showModal();
      (opts.danger ? $('#cfCancel') : ok).focus();
    });
  }

  let DATA = { sources: [], sessions: [], scan: {} };
  let sel = new Set();
  let sort = { key: 'last', dir: -1 };
  let cur = null;       // session shown in the drawer
  let drawerOpener = null;
  let loadTimer = 0;
  let EXP = { cleanupPeriodDays: 30, warnDays: 7, sessions: [] };   // GET /api/expiring (sessions Claude Code is about to delete)
  let expById = new Map();

  const totalTok = (s) => s.tok.i + s.tok.o + s.tok.cr + s.tok.cc;
  const projLabel = (s) => s.cwd || s.projDir || '';
  const baseName = (p) => String(p || '').replace(/[\\/]+$/, '').split(/[\\/]/).pop() || String(p || '');
  const projKey = (s) => s.projectKey || projLabel(s);
  const projName = (s) => s.projectName || baseName(projLabel(s)) || T('common.unknown_paren');
  let META_OK = false;      // GET /api/meta exists -> show star / tags / note / rename UI
  // last-used filters (remembered in localStorage; defaults come from Settings when nothing is remembered)
  const F = { source: '', status: 'active', project: '', model: '', tag: '', star: false, group: false, hide: false };
  let fStored = false;
  try {
    const o = JSON.parse(store.get('cm-filters') || 'null');
    if (o && typeof o === 'object') {
      for (const k of Object.keys(F)) if (k in o && typeof o[k] === typeof F[k]) F[k] = o[k];
      if (!['active', 'archived', 'trashed', 'all'].includes(F.status)) F.status = 'active';
      fStored = true;
    }
  } catch { /* ignore */ }
  if (!fStored && store.get('cm-group') === 'project') F.group = true;
  const saveF = () => { store.set('cm-filters', JSON.stringify(F)); fStored = true; };
  const collapsedGroups = new Set();
  const srcClass = (n) => n === 'Windows' ? 'win' : 'wsl';
  const mainModel = (s) => Object.entries(s.models).sort((a, b) => b[1] - a[1])[0]?.[0] || '';
  const ROLES = new Set(['user', 'assistant', 'tool', 'result', 'meta']);

  function normalize(d) {
    const out = d && typeof d === 'object' ? d : {};
    out.sources = Array.isArray(out.sources) ? out.sources : [];
    out.scan = out.scan || {};
    const c = out.counts && typeof out.counts === 'object' ? out.counts : null;
    out.counts = c ? { active: +c.active || 0, archived: +c.archived || 0, trashed: +c.trashed || 0 } : null;
    out.sessions = (Array.isArray(out.sessions) ? out.sessions : []).map((s) => {
      s.models = s.models || {};
      s.tok = Object.assign({ i: 0, o: 0, cr: 0, cc: 0 }, s.tok);
      s.daily = s.daily || {};
      s.title = s.title || T('common.empty_session');
      s.tags = Array.isArray(s.tags) ? s.tags.filter((t) => typeof t === 'string') : [];
      s.starred = !!s.starred;
      s.liveState = s.liveState === 'running' ? 'running' : 'idle';
      s.last = s.last || '';
      s.modelsTok = s.modelsTok && typeof s.modelsTok === 'object' ? s.modelsTok : {};
      s.efforts = Array.isArray(s.efforts) ? s.efforts.filter((e) => typeof e === 'string' && e) : [];
      s.costUsd = typeof s.costUsd === 'number' ? s.costUsd : null;
      s.cacheHitPct = typeof s.cacheHitPct === 'number' ? s.cacheHitPct : null;
      s.compacts = Number(s.compacts) || 0; s.hookFailures = Number(s.hookFailures) || 0;
      s.hookNames = Array.isArray(s.hookNames) ? s.hookNames.filter((x) => typeof x === 'string') : [];
      s.users = Number(s.users) || 0; s.assistants = Number(s.assistants) || 0; s.tools = Number(s.tools) || 0; s.size = Number(s.size) || 0;
      return s;
    });
    return out;
  }

  async function load() {
    clearTimeout(loadTimer);
    try {
      DATA = normalize(await api('/api/sessions'));
    } catch (e) {
      $('#scanStatus').textContent = T('common.error_prefix', { message: e.message });
      return;
    }
    const sc = DATA.scan;
    $('#scanStatus').textContent = sc.running
      ? T('header.scanning', { done: sc.done, total: sc.total })
      : T('header.scanned_count', { n: DATA.sessions.length });
    $('#scanStatus').title = !sc.running && sc.finishedAtMs ? T('header.scan_done_ago', { when: ago(sc.finishedAtMs) }) : '';
    if (cur) { const fresh = DATA.sessions.find((x) => x.id === cur.id); if (fresh) cur = fresh; }
    fillFilters();
    render();
    if (cur && $('#drawer').classList.contains('open')) drawerHead();
    document.dispatchEvent(new CustomEvent('cm:sessions'));
    if (sc.running) loadTimer = setTimeout(load, 1500);
    else loadExpiring();
  }

  let expSeq = 0;
  async function loadExpiring() {
    const my = ++expSeq;
    let d = null;
    try { d = await api('/api/expiring', { quiet: true }); } catch { d = null; }   // older backend / transient error: just no banner
    if (my !== expSeq) return;
    const list = d && Array.isArray(d.sessions) ? d.sessions.filter((x) => x && x.id) : [];
    EXP = { cleanupPeriodDays: Number(d && d.cleanupPeriodDays) || 30, warnDays: Number(d && d.warnDays) || 7, sessions: list };
    expById = new Map(list.map((x) => [x.id, x]));
    renderExpiring();
    if (!$('#tab-sessions').hidden) renderTable(filtered().sort(sorter()));
  }
  const daysText = (n) => (n <= 0 ? T('time.today') : n === 1 ? T('time.tomorrow') : T('time.in_days', { n }));
  let expOpen = false;
  const expDismissed = () => Date.now() - (Number(store.get('cm-exp-dismiss')) || 0) < 864e5;
  function renderExpiring() {
    const b = $('#expBanner');
    const list = EXP.sessions;
    b.hidden = list.length === 0 || expDismissed();
    if (b.hidden) { b.innerHTML = ''; return; }
    const days = list.map((x) => Number(x.daysLeft) || 0);
    const min = Math.max(0, Math.min(...days));
    const max = Math.max(0, Math.max(...days));
    const when = max <= 0 ? T('time.today') : (min === max ? T('time.in_days', { n: max }) : T('time.next_days', { n: max }));
    const urgent = min < max ? ' · ' + T('sessions.exp_urgent', { when: daysText(min) }) : '';
    const items = list.slice().sort((x, y) => (x.daysLeft - y.daysLeft) || (x.lastMs - y.lastMs)).slice(0, 50)
      .map((x) => `<li><span class="badge exp">${esc(daysText(Number(x.daysLeft) || 0))}</span> <span class="t" title="${esc(x.title)}">${esc(x.title || T('common.empty_session'))}</span> <span class="muted">${esc(x.source)}</span></li>`).join('');
    b.innerHTML = `<div class="exp-line"><span class="exp-ic" aria-hidden="true">!</span>
      <span class="exp-tx">${esc(T('sessions.exp_banner', { n: list.length, when, urgent }))}</span>
      <button type="button" class="small" data-exp="all" title="${esc(T('sessions.exp_archive_all_tip'))}">${esc(T('sessions.exp_archive_all'))}</button>
      <button type="button" class="ghost small" data-exp="toggle" aria-expanded="${expOpen}">${esc(T('sessions.exp_details'))}</button>
      <button type="button" class="ghost small" data-exp="dismiss" title="${esc(T('sessions.exp_dismiss_tip'))}" aria-label="${esc(T('sessions.exp_dismiss_label'))}">✕</button></div>
      <div class="exp-detail" ${expOpen ? '' : 'hidden'}><p class="muted small">${esc(T('sessions.exp_detail_text', { days: EXP.cleanupPeriodDays }))}</p><ul>${items}</ul></div>`;
  }
  $('#expBanner').addEventListener('click', (e) => {
    const btn = e.target.closest('button[data-exp]');
    if (!btn) return;
    if (btn.dataset.exp === 'toggle') { expOpen = !expOpen; renderExpiring(); return; }
    if (btn.dataset.exp === 'dismiss') { store.set('cm-exp-dismiss', String(Date.now())); renderExpiring(); return; }
    const ids = EXP.sessions.map((x) => x.id);
    if (ids.length) act('archive', ids);
  });

  // model order: strongest -> weakest (Settings `models` order when present), newer versions first
  const FAM = ['opus', 'sonnet', 'fable', 'haiku'];
  function modelCmp(a, b) {
    const order = (window.CM && CM.settings && CM.settings.models()) || [];
    const ia = order.indexOf(a), ib = order.indexOf(b);
    if (ia >= 0 || ib >= 0) return (ia < 0 ? 1e6 : ia) - (ib < 0 ? 1e6 : ib);
    const fam = (m) => { const i = FAM.findIndex((f) => m.toLowerCase().includes(f)); return i < 0 ? 99 : i; };
    const ver = (m) => (m.toLowerCase().match(/\d+/g) || []).filter((n) => n.length <= 2).map(Number);
    if (fam(a) !== fam(b)) return fam(a) - fam(b);
    const va = ver(a), vb = ver(b);
    for (let i = 0; i < Math.max(va.length, vb.length); i++) { const d = (vb[i] || 0) - (va[i] || 0); if (d) return d; }
    return a < b ? -1 : a > b ? 1 : 0;
  }
  const shortModel = (m) => String(m).replace(/^claude-/, '').replace(/-\d{8}$/, '');
  const modelsOf = (s) => Object.keys(Object.keys(s.modelsTok).length ? s.modelsTok : s.models).sort(modelCmp);

  function fillSelect(el, first, values, cur, labelOf) {
    el.innerHTML = `<option value="">${esc(first)}</option>` + values.map((x) => `<option value="${esc(x)}">${esc(labelOf ? labelOf(x) : x)}</option>`).join('');
    el.value = values.includes(cur) ? cur : '';
    return el.value;
  }
  function fillFilters() {
    F.source = fillSelect($('#fSource'), T('sessions.filter_source_all'), DATA.sources, F.source);
    {
      const c = DATA.counts || DATA.sessions.reduce((a, x) => (a[x.status] = (a[x.status] || 0) + 1, a), {});
      const el = $('#fStatus');
      el.innerHTML = [['active', 'sessions.status_active'], ['archived', 'sessions.status_archived'], ['trashed', 'sessions.status_trashed']].map(([v, l]) => `<option value="${v}">${esc(T(l))} (${c[v] || 0})</option>`).join('') +
        `<option value="all">${esc(T('sessions.status_all'))} (${(c.active || 0) + (c.archived || 0) + (c.trashed || 0)})</option>`;
      el.value = F.status;
    }
    {
      const m = new Map(), lastOf = new Map();
      for (const x of DATA.sessions) {
        const k = projKey(x);
        if (!m.has(k)) m.set(k, x.projectName ? `${projName(x)} - ${k}` : k);
        if (x.last > (lastOf.get(k) || '')) lastOf.set(k, x.last);
      }
      const keys = [...m.keys()].sort((a, b) => (lastOf.get(b) || '').localeCompare(lastOf.get(a) || '') || a.localeCompare(b));
      F.project = fillSelect($('#fProject'), T('sessions.filter_project_all'), keys, F.project, (k) => m.get(k));
    }
    {
      const tags = [...new Set(DATA.sessions.flatMap((x) => x.tags))].sort((a, b) => a.localeCompare(b, I18N.lang));
      const el = $('#fTag');
      F.tag = fillSelect(el, T('sessions.filter_tag_all'), tags, F.tag, (t) => '#' + t);
      el.hidden = !tags.length;
      $('#tStar').hidden = !META_OK && !DATA.sessions.some((x) => x.starred);
    }
    F.model = fillSelect($('#fModel'), T('sessions.filter_model_all'), [...new Set(DATA.sessions.flatMap(modelsOf))].sort(modelCmp), F.model);
    syncToggles();
  }
  function syncToggles() {
    for (const [id, k] of [['#tStar', 'star'], ['#tGroup', 'group'], ['#tHide', 'hide']]) { $(id).setAttribute('aria-pressed', String(F[k])); $(id).classList.toggle('on', F[k]); }
  }

  function filtered() {
    const q = $('#q').value.trim().toLowerCase();
    const h = (k, v) => (hidden('filter-' + k) ? '' : v);
    const fs = h('source', F.source), fp = h('project', F.project), fm = h('model', F.model), tag = h('tag', F.tag);
    const star = F.star && !$('#tStar').hidden, hideEmpty = F.hide;
    return DATA.sessions.filter((s) =>
      (!fs || s.source === fs) && (F.status === 'all' || s.status === F.status) && (!fp || projKey(s) === fp) &&
      (!fm || s.models[fm] || s.modelsTok[fm]) && (!hideEmpty || s.users > 0) && (!star || s.starred) && (!tag || s.tags.includes(tag)) &&
      (!q || [s.title, s.customTitle, s.note, s.tags.join(' '), s.cwd, s.projectName, s.branch, s.firstPrompt, s.lastPrompt, s.sessionId, s.source].join(' ').toLowerCase().includes(q)));
  }

  // [key, class, hide-key]; header text = sessions.col_<key>, tooltip = sessions.col_<key>_tip (only for COL_TIPS)
  const COLS = [
    ['star', 'star'], ['title'], ['tags', '', 'col-tags'], ['proj', '', 'col-proj'],
    ['source', '', 'col-source'], ['status', '', 'col-status'], ['last'],
    ['users', 'num'], ['in', 'num', 'col-tokens'], ['out', 'num', 'col-tokens'],
    ['cost', 'num', 'col-cost'],
    ['total', 'num', 'col-total'], ['size', 'num', 'col-size'],
    ['cache', 'num', 'col-tokens'], ['compact', 'num', 'col-tokens'], ['model', '', 'col-model'],
  ];
  const COL_TIPS = new Set(['star', 'users', 'in', 'out', 'cost', 'total', 'cache', 'compact', 'model']);
  const visCols = () => COLS.filter((c) => (!c[2] || !hidden(c[2])) && !(c[0] === 'tags' && !DATA.sessions.some((x) => x.tags.length)));
  const val = {
    star: (s) => (s.starred ? 1 : 0), tags: (s) => s.tags.join(' ').toLowerCase(), title: (s) => s.title.toLowerCase(), proj: (s) => projLabel(s).toLowerCase(), source: (s) => s.source, status: (s) => s.status,
    last: (s) => s.last, users: (s) => s.users, in: (s) => s.tok.i, out: (s) => s.tok.o, cost: (s) => (s.costUsd == null ? -1 : s.costUsd), total: totalTok, size: (s) => s.size, model: mainModel,
    cache: (s) => (s.cacheHitPct == null ? -1 : s.cacheHitPct), compact: (s) => s.compacts || 0,
  };

  const sorter = () => { const f = val[sort.key] || val.last; return (a, b) => (f(a) > f(b) ? 1 : f(a) < f(b) ? -1 : 0) * sort.dir; };
  function render() {
    const rows = filtered();
    rows.sort(sorter());
    const visible = new Set(rows.map((r) => r.id));
    sel = new Set([...sel].filter((id) => visible.has(id)));

    const t = rows.reduce((a, s) => (a.users += s.users, a.in += s.tok.i, a.out += s.tok.o, a.tot += totalTok(s), a.size += s.size,
      s.costUsd == null ? a.unp++ : (a.cost += s.costUsd, a.priced++), a), { users: 0, in: 0, out: 0, tot: 0, size: 0, cost: 0, unp: 0, priced: 0 });
    $('#cards').innerHTML = [
      [rows.length, T('sessions.card_sessions')], [fmt(t.users), T('sessions.card_prompts')], [fmt(t.in), T('sessions.card_input')], [fmt(t.out), T('sessions.card_output')],
      [t.priced ? usd(t.cost) : '-', T('sessions.card_cost'), t.priced && t.unp ? T('sessions.card_cost_unpriced', { n: t.unp }) : ''],
      [new Set(rows.map(projLabel)).size, T('sessions.card_projects')], [fmtSize(t.size), T('sessions.card_size')],
    ].map(([b, s, sub]) => `<div class="card"><b>${esc(b)}</b><span>${esc(s)}</span>${sub ? `<small>${esc(sub)}</small>` : ''}</div>`).join('');

    if (!$('#tab-sessions').hidden) renderTable(rows);
  }

  function expBadge(s) {
    const x = expById.get(s.id);
    if (!x) return '';
    const n = Number(x.daysLeft) || 0;
    return `<span class="badge exp" title="${esc(T('sessions.exp_badge_tip', { when: daysText(n) }))}">${esc(T('sessions.exp_badge', { when: n <= 0 ? T('time.today') : T('time.days_short', { n }) }))}</span>`;
  }

  function renderTable(rows) {
    // keep keyboard focus across the full re-render
    const ae = document.activeElement;
    let keep = null;
    if (ae && $('#tbl').contains(ae)) {
      keep = { id: ae.closest('tr[data-id]')?.dataset.id || '', k: ae.closest('th[data-k]')?.dataset.k || '', cb: ae.classList.contains('cb'), all: ae.id === 'all' };
    }
    const cols = visCols();
    $('#tbl thead').innerHTML = '<tr><th><input type="checkbox" id="all" aria-label="' + esc(T('sessions.select_all')) + '"></th>' + cols.map(([k, c]) => {
      const l = k === 'star' ? '★' : esc(T('sessions.col_' + k)), tip = COL_TIPS.has(k) ? T('sessions.col_' + k + '_tip') : '';
      return `<th class="${c || ''}" data-k="${k}" tabindex="0" ${tip ? `title="${esc(tip)}"` : ''} aria-sort="${sort.key === k ? (sort.dir > 0 ? 'ascending' : 'descending') : 'none'}">${l}${sort.key === k ? (sort.dir > 0 ? ' ▲' : ' ▼') : ''}</th>`;
    }).join('') + '</tr>';
    const cell = {
      star: (s) => `<td class="star">${META_OK ? `<button type="button" class="ibtn star${s.starred ? ' on' : ''}" data-star="1" aria-pressed="${s.starred}" aria-label="${esc(T('sessions.star_row', { title: s.title }))}" title="${esc(T('sessions.star'))}">${s.starred ? '★' : '☆'}</button>` : (s.starred ? '<span class="starmark">★</span>' : '')}</td>`,
      title: (s) => `<td class="title" title="${esc(s.title)}">${s.liveState === 'running' ? `<span class="live-dot" title="${esc(T('sessions.live_tip'))}"></span> ` : ''}${esc(s.title)}${s.titlePending ? ` <span class="muted" title="${esc(T('sessions.title_pending_tip'))}">${esc(T('sessions.title_pending'))}</span>` : ''}${s.hookFailures ? ` <span class="badge trashed" style="background:#c62828;color:#fff;border-color:#c62828" title="${esc(T('sessions.hook_failures_tip', { n: s.hookFailures, names: (s.hookNames || []).join(', ') }))}">${esc(T('sessions.hook_badge', { n: s.hookFailures }))}</span>` : ''}${s.continuesFrom ? ` <span class="cont" title="${esc(T('sessions.continues_from_tip'))}">↩</span>` : ''}${s.continuedBy ? ` <span class="cont" title="${esc(T('sessions.continued_by_tip'))}">↪</span>` : ''}</td>`,
      tags: (s) => `<td class="tagcell">${s.tags.map((t) => `<span class="tag">#${esc(t)}</span>`).join('')}</td>`,
      proj: (s) => `<td class="proj muted" title="${esc(projLabel(s))}">&lrm;${esc(projLabel(s))}&lrm;</td>`,
      source: (s) => `<td><span class="badge ${srcClass(s.source)}">${esc(s.source)}</span></td>`,
      status: (s) => `<td>${s.status === 'active' ? expBadge(s) : `<span class="badge ${s.status === 'archived' ? 'archived' : 'trashed'}">${esc(s.status === 'archived' ? T('sessions.status_badge_archived') : T('sessions.status_badge_trashed'))}</span>`}</td>`,
      last: (s) => `<td class="muted" title="${esc(s.last)}">${ago(s.last)}</td>`,
      users: (s) => `<td class="num">${s.users}</td>`, in: (s) => `<td class="num">${fmt(s.tok.i)}</td>`, out: (s) => `<td class="num">${fmt(s.tok.o)}</td>`,
      cost: (s) => (s.costUsd == null ? `<td class="num muted" title="${esc(T('sessions.no_price_tip'))}">${esc(T('sessions.no_price'))}</td>` : `<td class="num">${esc(usd(s.costUsd))}</td>`),
      cache: (s) => `<td class="num muted">${s.cacheHitPct == null ? '-' : esc(s.cacheHitPct.toFixed(0)) + '%'}</td>`,
      compact: (s) => `<td class="num${s.compacts >= 3 ? '' : ' muted'}" ${s.compacts >= 3 ? `title="${esc(T('sessions.many_compacts_tip'))}"` : ''}>${s.compacts || '-'}</td>`,
      total: (s) => `<td class="num">${fmt(totalTok(s))}</td>`, size: (s) => `<td class="num muted">${fmtSize(s.size)}</td>`,
      model: (s) => `<td class="mcell">${modelsOf(s).map((m) => `<span class="chip" title="${esc(m)}">${esc(shortModel(m))}</span>`).join('')}${s.efforts.map((e) => `<span class="chip eff" title="${esc(T('sessions.effort_tip'))}">${esc(e)}</span>`).join('')}</td>`,
    };
    const rowHtml = (s) => `<tr data-id="${esc(s.id)}" tabindex="0" class="${sel.has(s.id) ? 'sel' : ''}">
      <td><input type="checkbox" class="cb" aria-label="${esc(T('sessions.select_row', { title: s.title }))}" ${sel.has(s.id) ? 'checked' : ''}></td>${cols.map((c) => cell[c[0]](s)).join('')}</tr>`;
    if (F.group) {
      const groups = new Map();
      for (const r of rows) { const k = projKey(r); (groups.get(k) || groups.set(k, []).get(k)).push(r); }
      const ncol = cols.length + 1;
      $('#tbl tbody').innerHTML = [...groups.entries()].map(([k, list]) => {
        const off = collapsedGroups.has(k);
        const last = list.reduce((a, x) => (x.last > a ? x.last : a), '');
        return `<tr class="grp" data-grp="${esc(k)}" tabindex="0" aria-expanded="${!off}"><td colspan="${ncol}"><b>${off ? '▸' : '▾'} ${esc(projName(list[0]))}</b> <span class="muted">${esc(k)}</span> <span class="badge">${esc(T('sessions.group_sessions', { n: list.length }))}</span> <span class="muted small">${esc(ago(last))}</span></td></tr>` + (off ? '' : list.map(rowHtml).join(''));
      }).join('');
    } else $('#tbl tbody').innerHTML = rows.map(rowHtml).join('');
    $('#tbl').classList.toggle('no-meta', !META_OK && !DATA.sessions.some((x) => x.starred || x.tags.length));
    $('#empty').hidden = rows.length > 0;
    $('#all').checked = rows.length > 0 && sel.size === rows.length;
    renderBulk(rows);
    if (keep) {
      let el = null;
      if (keep.all) el = $('#all');
      else if (keep.id) { const tr = $(`#tbl tbody tr[data-id="${CSS.escape(keep.id)}"]`); el = tr && (keep.cb ? $('.cb', tr) : tr); }
      else if (keep.k) el = $(`#tbl th[data-k="${keep.k}"]`);
      if (el) el.focus({ preventScroll: true });
    }
  }

  function renderBulk(rows) {
    const b = $('#bulk');
    b.hidden = sel.size === 0;
    if (!sel.size) return;
    const chosen = rows.filter((r) => sel.has(r.id));
    const has = (st) => chosen.some((r) => r.status === st);
    const bt = (a, ic, tx, cls) => `<button type="button" data-a="${a}" class="${cls || ''}" title="${esc(tx)}" aria-label="${esc(tx)}">${lb(ic, tx)}</button>`;
    b.innerHTML = `<b>${esc(T('sessions.bulk_selected', { n: sel.size }))}</b><span class="muted">${fmtSize(chosen.reduce((a, s) => a + s.size, 0))}</span><span class="sp"></span>
      ${has('active') ? bt('archive', '▣', T('sessions.bulk_archive')) : ''}
      ${has('archived') || has('trashed') ? bt('restore', '↺', T('sessions.bulk_restore')) : ''}
      ${chosen.some((r) => r.status !== 'trashed') ? bt('trash', '🗑', T('sessions.bulk_trash'), 'danger') : ''}
      ${has('trashed') ? bt('purge', '✕', T('sessions.bulk_purge'), 'danger') : ''}
      ${bt('clear', '○', T('sessions.bulk_clear'))}`;
  }

  async function act(action, ids) {
    const n = ids.length;
    if (action === 'purge' && !(await confirmDialog(T('sessions.confirm_purge', { n }), { okText: T('sessions.bulk_purge'), danger: true }))) return;
    if (action === 'trash' && n > 5 && !(await confirmDialog(T('sessions.confirm_trash', { n }), { okText: T('sessions.bulk_trash'), danger: true }))) return;
    try {
      const res = await api('/api/action', { method: 'POST', body: { action, ids }, quiet: true });
      const results = (res && res.results) || [];
      const failed = new Set();
      results.forEach((r, i) => { if (r && !r.ok) failed.add(r.id && ids.includes(r.id) ? r.id : ids[i]); });
      const ok = results.filter((r) => r && r.ok).length, bad = results.filter((r) => r && !r.ok);
      if (res && res.error && !bad.length) { toast(String(res.error), 'err'); ids.forEach((id) => failed.add(id)); }
      else toast(bad.length ? T('toast.sessions_action_result_error', { ok, n, error: bad[0].error || T('common.unknown') }) : T('toast.sessions_action_result', { ok, n }), bad.length ? 'err' : undefined);
      if (failed.size) { sel = new Set([...failed].filter(Boolean)); }   // keep what failed selected so it can be retried
      else { sel.clear(); closeDrawer(); }
      await load();
    } catch (e) { toast(T('common.error_prefix', { message: e.message }), 'err'); }   // selection and drawer stay as they were
  }

  /** Open a folder in VS Code (POST /api/open-editor). */
  async function openEditor(spec) {
    try {
      const r = await api('/api/open-editor', { method: 'POST', body: spec, quiet: true });
      if (r && r.error) throw new Error(r.error);
      toast(T('toast.sessions_vscode_opening'));
    } catch (e) { toast(T('toast.sessions_vscode_failed', { detail: e.detail || e.message }), 'err'); }
  }

  async function setMeta(s, patch) {
    const before = { title: s.title, customTitle: s.customTitle, tags: s.tags, note: s.note, starred: s.starred };
    if ('starred' in patch) s.starred = !!patch.starred;
    if ('tags' in patch) s.tags = patch.tags;
    if ('note' in patch) s.note = patch.note || undefined;
    if ('title' in patch && patch.title) { s.customTitle = patch.title; s.title = patch.title; s.titlePending = true; }
    render(); fillFilters();
    if (cur && cur.id === s.id) drawerHead();
    try {
      const res = await api('/api/meta', { method: 'PUT', body: Object.assign({ id: s.id }, patch), quiet: true });
      const m = res && res.meta;
      if (m) {
        if (Array.isArray(m.tags)) s.tags = m.tags;
        if (typeof m.starred === 'boolean') s.starred = m.starred;
        s.note = m.note || undefined;
        if (m.title) { s.customTitle = m.title; s.title = m.title; }
      }
      if ('title' in patch && !patch.title) { delete s.customTitle; s.titlePending = false; await load(); }
    } catch (e) {
      Object.assign(s, before);
      toast(T('toast.sessions_save_failed', { message: e.message }), 'err');
    }
    render(); fillFilters();
    if (cur && cur.id === s.id) drawerHead();
  }
  const cleanTag = (t) => String(t || '').replace(/^#+/, '').replace(/\s+/g, ' ').trim().slice(0, 40);

  let drawerEditing = false;
  function drawerHead() {
    const s = cur;
    if (!s || drawerEditing) return;
    $('#dTitle').textContent = s.title;
    const st = $('#dStar');
    st.hidden = !META_OK; st.textContent = s.starred ? '★' : '☆'; st.classList.toggle('on', s.starred); st.setAttribute('aria-pressed', String(s.starred));
    $('#dRename').hidden = !META_OK;
    const link = (id, label, icon) => {
      const t = DATA.sessions.find((x) => x.id === id || x.sessionId === id);
      return `<button type="button" class="cont-btn" data-goto="${esc(t ? t.id : id)}" ${t ? '' : 'disabled'} title="${esc(t ? t.title : T('drawer.goto_missing'))}">${icon} ${label}</button>`;
    };
    $('#dBadges').innerHTML = `<span class="badge ${srcClass(s.source)}">${esc(s.source)}</span> <span class="badge ${esc(s.status)}">${esc(s.status)}</span>` +
      (s.liveState === 'running' ? ' <span class="badge live"><span class="live-dot"></span> ' + esc(T('drawer.badge_running')) + '</span>' : '') +
      (s.continuesFrom ? ' ' + link(s.continuesFrom, esc(T('drawer.goto_prev')), '↩') : '') +
      (s.continuedBy ? ' ' + link(s.continuedBy, esc(T('drawer.goto_next')), '↪') : '') +
      s.tags.map((t) => ` <span class="tag">#${esc(t)}${META_OK ? `<button type="button" class="x" data-rmtag="${esc(t)}" aria-label="${esc(T('drawer.remove_tag', { tag: t }))}">×</button>` : ''}</span>`).join('') +
      (META_OK ? ' <button type="button" class="tagadd" data-addtag="1">' + esc(T('drawer.add_tag')) + '</button>' : '');
    const n = $('#dNote');
    n.hidden = !META_OK && !s.note;
    n.innerHTML = s.note ? `<span class="nl">${esc(T('drawer.note_label'))}</span> <span class="nt">${esc(s.note)}</span>${META_OK ? ` <button type="button" class="ghost small" data-editnote="1">${esc(T('drawer.note_edit'))}</button>` : ''}`
      : (META_OK ? `<button type="button" class="ghost small" data-editnote="1">${esc(T('drawer.note_add'))}</button>` : '');
  }

  function startRename() {
    const s = cur;
    if (!s || drawerEditing) return;
    drawerEditing = true;
    const h = $('#dTitle');
    const inp = document.createElement('input');
    inp.type = 'text'; inp.className = 'edit-title'; inp.value = s.customTitle || s.title; inp.maxLength = 200;
    inp.setAttribute('aria-label', T('drawer.rename_label'));
    h.replaceChildren(inp); inp.focus(); inp.select();
    let done = false;
    const end = (save) => {
      if (done) return; done = true;
      drawerEditing = false;
      const v = inp.value.trim();
      drawerHead();
      if (save && v !== (s.customTitle || s.title)) setMeta(s, { title: v || null });
    };
    inp.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); end(true); } else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); end(false); } });
    inp.addEventListener('blur', () => end(false));
  }
  function startAddTag(btn) {
    const s = cur;
    if (!s || drawerEditing) return;
    drawerEditing = true;
    const inp = document.createElement('input');
    inp.type = 'text'; inp.className = 'edit-tag'; inp.maxLength = 40; inp.placeholder = T('drawer.tag_ph');
    inp.setAttribute('aria-label', T('drawer.tag_label'));
    const dl = document.createElement('datalist'); dl.id = 'tagSuggest';
    for (const t of new Set(DATA.sessions.flatMap((x) => x.tags))) { const o = document.createElement('option'); o.value = t; dl.appendChild(o); }
    inp.setAttribute('list', 'tagSuggest');
    btn.replaceWith(inp); inp.after(dl); inp.focus();
    let done = false;
    const end = (save) => {
      if (done) return; done = true;
      drawerEditing = false;
      const t = cleanTag(inp.value);
      drawerHead();
      if (save && t && !s.tags.includes(t)) {
        if (s.tags.length >= 20) toast(T('toast.sessions_max_tags'), 'err'); else setMeta(s, { tags: [...s.tags, t] });
      }
    };
    inp.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ',') { e.preventDefault(); end(true); } else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); end(false); } });
    inp.addEventListener('blur', () => end(true));
  }
  function startEditNote() {
    const s = cur;
    if (!s || drawerEditing) return;
    drawerEditing = true;
    const n = $('#dNote');
    n.hidden = false;
    const ta = document.createElement('textarea');
    ta.className = 'edit-note'; ta.rows = 3; ta.maxLength = 10000; ta.value = s.note || ''; ta.placeholder = T('drawer.note_ph');
    ta.setAttribute('aria-label', T('drawer.note_input_label'));
    n.replaceChildren(ta); ta.focus();
    let done = false;
    const end = (save) => {
      if (done) return; done = true;
      drawerEditing = false;
      const v = ta.value.trim();
      drawerHead();
      if (save && v !== (s.note || '')) setMeta(s, { note: v });
    };
    ta.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); end(true); } else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); end(false); } });
    ta.addEventListener('blur', () => end(true));
  }

  function closeDrawer() {
    const d = $('#drawer');
    if (CM.viewer) CM.viewer.close();
    drawerEditing = false;
    if (!d.classList.contains('open')) { cur = null; return; }
    d.classList.remove('open');
    d.setAttribute('aria-hidden', 'true');
    d.inert = true;
    cur = null;
    const opId = drawerOpener; drawerOpener = null;
    if (opId && !$('#tab-sessions').hidden) {
      const tr = $(`#tbl tbody tr[data-id="${CSS.escape(opId)}"]`);
      if (tr) tr.focus({ preventScroll: true });
    }
  }

  function modelTable(s) {
    const names = modelsOf(s);
    if (!names.length) return '-';
    const hasCost = names.some((m) => s.modelsTok[m] && s.modelsTok[m].costUsd != null);
    return `<table class="mtab"><thead><tr><th>${esc(T('drawer.mt_model'))}</th><th class="num">${esc(T('drawer.mt_msg'))}</th><th class="num">${esc(T('drawer.mt_in'))}</th><th class="num">${esc(T('drawer.mt_out'))}</th><th class="num">${esc(T('drawer.mt_cache'))}</th>${hasCost ? `<th class="num">${esc(T('drawer.mt_cost'))}</th>` : ''}</tr></thead><tbody>` + names.map((m) => {
      const t = s.modelsTok[m] || {};
      return `<tr><td>${esc(m)}</td><td class="num">${s.models[m] != null ? esc(s.models[m]) : '-'}</td><td class="num">${fmt(+t.in || 0)}</td><td class="num">${fmt(+t.out || 0)}</td><td class="num">${fmt((+t.cc || 0) + (+t.cr || 0))}</td>${hasCost ? `<td class="num">${t.costUsd == null ? '-' : esc(usd(t.costUsd))}</td>` : ''}</tr>`;
    }).join('') + '</tbody></table>';
  }

  async function openDrawer(id, opts = {}) {
    const s = DATA.sessions.find((x) => x.id === id);
    if (!s) return;
    if (CM.viewer) CM.viewer.close();
    drawerEditing = false;
    cur = s;
    drawerOpener = s.id;
    drawerHead();
    const row = (k, v) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`;
    $('#dMeta').innerHTML = row(T('drawer.meta_project'), projLabel(s)) + row(T('drawer.meta_branch'), s.branch || '-') + row(T('drawer.meta_session_id'), s.sessionId) +
      row(T('drawer.meta_started'), s.first ? new Date(s.first).toLocaleString(I18N.locale) : '-') + row(T('drawer.meta_last'), s.last ? new Date(s.last).toLocaleString(I18N.locale) : '-') +
      row(T('drawer.meta_messages'), T('drawer.meta_messages_value', { users: s.users, assistants: s.assistants, tools: s.tools })) +
      row(T('drawer.meta_tokens'), T('drawer.meta_tokens_value', { in: fmt(s.tok.i), out: fmt(s.tok.o), cc: fmt(s.tok.cc), cr: fmt(s.tok.cr) })) +
      row(T('drawer.meta_cost'), s.costUsd == null ? T('sessions.no_price') : T('drawer.meta_cost_estimated', { cost: usd(s.costUsd) })) +
      `<dt>${esc(T('drawer.meta_model'))}</dt><dd>${modelTable(s)}</dd>` + (s.efforts.length ? row(T('drawer.meta_effort'), s.efforts.join(', ')) : '') +
      row(T('drawer.meta_version'), `${s.version || '?'} · ${s.entrypoint || '?'}`) + row(T('drawer.meta_size'), fmtSize(s.size));
    const canOpen = s.status === 'active' && !!s.cwd;
    const whyNot = s.status !== 'active' ? T('drawer.act_workspace_restore_first') : !s.cwd ? T('drawer.act_no_cwd') : T('drawer.act_workspace_tip');
    const ab = (attr, ic, tx, cls, tip) => `<button type="button" ${attr} class="${cls || ''}" title="${esc(tip || tx)}" aria-label="${esc(tx)}">${lb(ic, tx)}</button>`;
    $('#dActions').innerHTML = [
      ab('data-ws="1"' + (canOpen ? '' : ' disabled'), '▣', T('drawer.act_workspace'), 'primary', whyNot),
      ab('data-editor="1"' + (s.cwd ? '' : ' disabled'), '⟨/⟩', T('drawer.act_vscode'), '', s.cwd ? T('drawer.act_vscode_tip') : T('drawer.act_no_cwd')),
      s.status === 'active' ? ab('data-a="archive"', '▣', T('drawer.act_archive')) : '',
      s.status !== 'active' ? ab('data-a="restore"', '↺', T('drawer.act_restore')) : '',
      s.status !== 'trashed' ? ab('data-a="trash"', '🗑', T('drawer.act_trash'), 'danger') : ab('data-a="purge"', '✕', T('drawer.act_purge'), 'danger'),
      ab(`data-copy="${esc(s.sessionId)}"`, '⧉', T('drawer.act_copy_id')),
      ab(`data-copy="${esc('claude --resume ' + s.sessionId)}"`, '⧉', T('drawer.act_copy_resume')),
      s.cwd ? ab(`data-copy="${esc(s.cwd)}"`, '⧉', T('drawer.act_copy_cwd')) : '',
    ].join('');
    $('#drawer .hd-more').open = false;
    const d = $('#drawer');
    d.inert = false;
    d.setAttribute('aria-hidden', 'false');
    d.classList.add('open');
    $('#dClose').focus({ preventScroll: true });
    $('#msgs').scrollTop = 0;
    $('#msgs').className = 'hide-meta';
    $('#msgs').innerHTML = `<div class="empty">${esc(T('common.loading'))}</div>`;
    $('#vbar').hidden = true; $('#legacyBar').hidden = true;
    let handled = false;
    if (CM.viewer) handled = await CM.viewer.open(id, opts);
    if (!cur || cur.id !== id) return;
    if (handled) { $('#legacyBar').hidden = true; return; }
    // older backend (no /turns): legacy flat message list
    $('#vbar').hidden = true; $('#legacyBar').hidden = false;
    $('#msgs').className = ''; syncMsgClass();
    try {
      const res = await api('/api/messages?id=' + encodeURIComponent(id), { quiet: true });
      if (!cur || cur.id !== id) return;
      const items = (res && res.items) || [];
      $('#msgs').innerHTML = items.length ? items.map((m) => {
        const role = ROLES.has(m.role) ? m.role : 'meta';
        return `<div class="m ${role}"><span class="r">${role}${m.name ? ' · ' + esc(m.name) : ''}${m.model ? ' · ' + esc(m.model) : ''}${m.ts ? ' · ' + esc(new Date(m.ts).toLocaleTimeString(I18N.locale)) : ''}</span>${esc(m.text)}</div>`;
      }).join('') : `<div class="empty">${esc(T('common.no_content'))}</div>`;
    } catch (e) {
      if (cur && cur.id === id) $('#msgs').innerHTML = `<div class="empty">${esc(T('common.error_prefix', { message: e.message }))}</div>`;
      toast(T('toast.sessions_load_failed', { message: e.message }), 'err');
    }
  }
  function syncMsgClass() {
    const c = $('#msgs').classList;
    if (c.contains('viewer')) return;
    c.toggle('hide-meta', !$('#showMeta').checked);
    c.toggle('hide-tools', !$('#showTools').checked);
  }

  /** Open a session in the viewer from anywhere (search palette, links). */
  function openSession(id, opts) {
    if (!DATA.sessions.some((x) => x.id === id)) { toast(T('toast.sessions_not_in_list'), 'err'); return; }
    if (curTab !== 'sessions') showTab('sessions');
    return openDrawer(id, opts || {});
  }

  const TABS = ['sessions', 'usage', 'workspace'];
  let curTab = null;
  function showTab(name) {
    if (!TABS.includes(name)) name = 'sessions';
    const changed = name !== curTab;
    curTab = name;
    closeDrawer();
    $$('.tab').forEach((t) => {
      const on = t.dataset.tab === name;
      t.classList.toggle('on', on);
      t.setAttribute('aria-selected', String(on));
      t.tabIndex = on ? 0 : -1;
    });
    $('#data-view').hidden = name === 'workspace';
    $('#filters').hidden = $('#cards').hidden = name !== 'sessions';
    $('#tab-sessions').hidden = name !== 'sessions';
    $('#tab-usage').hidden = name !== 'usage';
    $('#tab-workspace').hidden = name !== 'workspace';
    document.body.classList.toggle('is-ws', name === 'workspace');
    store.set('cm-tab', name);
    try { history.replaceState(null, '', '#' + name); } catch { /* ignore */ }
    if (name !== 'workspace') render();
    if (changed) document.dispatchEvent(new CustomEvent('cm:tab', { detail: name }));
  }

  const THEME_MODES = [['auto', '◐', 'header.theme_auto'], ['light', '☀', 'header.theme_light'], ['dark', '☾', 'header.theme_dark']];
  function themeMode() { const t = store.get('cm-theme'); return t === 'light' || t === 'dark' ? t : 'auto'; }
  function applyTheme(mode) {
    const r = document.documentElement;
    if (mode === 'auto') r.removeAttribute('data-theme'); else r.setAttribute('data-theme', mode);
    const [, ic, key] = THEME_MODES.find((m) => m[0] === mode);
    const b = $('#themeBtn');
    b.innerHTML = lb(ic, T(key));
    b.title = T('header.theme_current', { mode: T(key) });
    b.setAttribute('aria-label', T('header.theme_current', { mode: T(key) }));
  }

  $('#tbl').addEventListener('click', (e) => {
    const th = e.target.closest('th[data-k]');
    if (th) { sortBy(th.dataset.k); return; }
    if (e.target.id === 'all') { const rows = filtered(); sel = e.target.checked ? new Set(rows.map((r) => r.id)) : new Set(); render(); return; }
    const g = e.target.closest('tr[data-grp]');
    if (g) { const k = g.dataset.grp; collapsedGroups.has(k) ? collapsedGroups.delete(k) : collapsedGroups.add(k); renderTable(filtered().sort(sorter())); return; }
    const tr = e.target.closest('tr[data-id]');
    if (!tr) return;
    const starBtn = e.target.closest('button[data-star]');
    if (starBtn) { const x = DATA.sessions.find((y) => y.id === tr.dataset.id); if (x) setMeta(x, { starred: !x.starred }); return; }
    if (e.target.classList.contains('cb')) { const id = tr.dataset.id; sel.has(id) ? sel.delete(id) : sel.add(id); render(); return; }
    openDrawer(tr.dataset.id);
  });
  function sortBy(k) {
    sort = { key: k, dir: sort.key === k ? -sort.dir : (['title', 'proj', 'source', 'model', 'status', 'tags'].includes(k) ? 1 : -1) };
    render();
  }
  $('#tbl').addEventListener('keydown', (e) => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    const th = e.target.closest('th[data-k]');
    if (th && e.target === th) { e.preventDefault(); sortBy(th.dataset.k); return; }
    const g = e.target.closest('tr[data-grp]');
    if (g && e.target === g) { e.preventDefault(); g.click(); return; }
    const tr = e.target.closest('tr[data-id]');
    if (tr && e.target === tr && e.key === 'Enter') { e.preventDefault(); openDrawer(tr.dataset.id); }
  });
  $('#bulk').addEventListener('click', (e) => {
    const a = e.target.dataset.a;
    if (!a) return;
    if (a === 'clear') { sel.clear(); render(); } else act(a, [...sel]);
  });
  $('#dActions').addEventListener('click', (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.copy) {
      navigator.clipboard.writeText(b.dataset.copy).then(() => toast(T('toast.copied')), () => toast(T('toast.copy_failed'), 'err'));
      return;
    }
    if (b.dataset.ws && cur) {
      const s = cur;
      closeDrawer();
      if (CM.workspace && CM.workspace.openSession) CM.workspace.openSession(s);
      else toast(T('toast.sessions_workspace_not_ready'), 'err');
      return;
    }
    if (b.dataset.editor && cur) { openEditor({ id: cur.sessionId, source: cur.source, cwd: cur.cwd }); return; }
    if (b.dataset.a && cur) act(b.dataset.a, [cur.id]);
  });
  $('#dClose').addEventListener('click', closeDrawer);
  $('#dStar').addEventListener('click', () => { if (cur) setMeta(cur, { starred: !cur.starred }); });
  $('#dRename').addEventListener('click', startRename);
  $('#dTitle').addEventListener('dblclick', () => { if (META_OK) startRename(); });
  $('#dNote').addEventListener('click', (e) => { if (e.target.closest('[data-editnote]')) startEditNote(); });
  $('#dBadges').addEventListener('click', (e) => {
    const rm = e.target.closest('[data-rmtag]');
    if (rm && cur) { setMeta(cur, { tags: cur.tags.filter((t) => t !== rm.dataset.rmtag) }); return; }
    const add = e.target.closest('[data-addtag]');
    if (add) { startAddTag(add); return; }
    const go = e.target.closest('[data-goto]');
    if (go && !go.disabled) openSession(go.dataset.goto);
  });
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || e.defaultPrevented) return;
    if (e.target.closest && e.target.closest('.xterm')) return; // Esc belongs to the terminal
    if (document.querySelector('dialog[open]')) return;
    if ($('#drawer').classList.contains('open')) closeDrawer();
  });
  $('#showMeta').addEventListener('change', syncMsgClass);
  $('#showTools').addEventListener('change', syncMsgClass);
  for (const [id, k] of [['#tStar', 'star'], ['#tGroup', 'group'], ['#tHide', 'hide']]) {
    $(id).addEventListener('click', () => { F[k] = !F[k]; saveF(); syncToggles(); render(); });
  }
  for (const [id, k] of [['#fSource', 'source'], ['#fStatus', 'status'], ['#fProject', 'project'], ['#fModel', 'model'], ['#fTag', 'tag']]) {
    $(id).addEventListener('change', () => { F[k] = $(id).value; saveF(); render(); });
  }
  $('#q').addEventListener('input', render);
  /** Settings: default view (status / group / starred). reset=true overrides the remembered filters. */
  function applyDefaultView(reset) {
    if (fStored && !reset) return;
    const d = (CM.settings && CM.settings.get().ui.defaults) || {};
    if (['active', 'archived', 'trashed', 'all'].includes(d.status)) F.status = d.status;
    F.group = d.group === 'project' || d.group === true; F.star = !!d.starredOnly;
    if (reset) { F.source = F.project = F.model = F.tag = ''; F.hide = false; saveF(); }
    fillFilters(); render();
  }
  document.addEventListener('cm:settings', (e) => {
    if (e.detail && e.detail.first) applyDefaultView(false);
    fillFilters(); render(); renderExpiring();
    if (cur && $('#drawer').classList.contains('open')) drawerHead();
  });
  $('#refresh').addEventListener('click', async () => {
    try {
      await api('/api/rescan', { method: 'POST', body: {} });
      toast(T('toast.header_rescanning'));
      setTimeout(load, 400);
    } catch { /* api() already toasted */ }
  });
  $('#authReload').addEventListener('click', () => location.reload());
  $('#themeBtn').addEventListener('click', () => {
    const i = THEME_MODES.findIndex((m) => m[0] === themeMode());
    const next = THEME_MODES[(i + 1) % THEME_MODES.length][0];
    store.set('cm-theme', next);
    applyTheme(next);
  });
  $$('.tab').forEach((t) => {
    t.addEventListener('click', () => showTab(t.dataset.tab));
    t.addEventListener('keydown', (e) => {
      const i = TABS.indexOf(t.dataset.tab);
      let n = -1;
      if (e.key === 'ArrowRight') n = (i + 1) % TABS.length;
      else if (e.key === 'ArrowLeft') n = (i + TABS.length - 1) % TABS.length;
      else if (e.key === 'Home') n = 0;
      else if (e.key === 'End') n = TABS.length - 1;
      if (n < 0) return;
      e.preventDefault();
      showTab(TABS[n]);
      $(`.tab[data-tab="${TABS[n]}"]`).focus();
    });
  });
  window.addEventListener('hashchange', () => { const h = location.hash.slice(1); if (TABS.includes(h) && h !== curTab) showTab(h); });

  const CM = window.CM = {
    $, $$, esc, fmt, fmtSize, ago, toast, api, ApiError, TOKEN, store, authLost,
    usd, lb, T, openEditor, applyDefaultView, confirm: confirmDialog, showTab, getTab: () => curTab,
    getData: () => DATA, srcClass, projLabel, openSession,
    workspace: null,   // filled in by workspace.js
  };

  applyTheme(themeMode());
  syncMsgClass();
  fillFilters(); syncToggles();
  const startTab = TABS.includes(location.hash.slice(1)) ? location.hash.slice(1) : (TABS.includes(store.get('cm-tab')) ? store.get('cm-tab') : 'sessions');
  $('#drawer').inert = true;
  showTab(startTab);
  load();

  api('/api/meta', { quiet: true }).then(() => { META_OK = true; }, () => { META_OK = false; }).then(() => { fillFilters(); render(); if (cur) drawerHead(); });

  let gen = null, pollOff = false;
  async function poll() {
    if (pollOff || document.hidden) return;
    let c;
    try { c = await api('/api/changes', { quiet: true }); } catch (e) { if (e.status === 404 || e.status === 405) pollOff = true; return; }
    if (!c || typeof c.gen !== 'number') return;
    const changed = gen !== null && c.gen !== gen;
    gen = c.gen;
    if (changed) {
      await load();
      if (CM.viewer && CM.viewer.isActive()) CM.viewer.refresh();
    }
  }
  setInterval(poll, 5000);
  poll();
  document.addEventListener('visibilitychange', () => { if (!document.hidden) poll(); });
})();
