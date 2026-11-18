'use strict';
/* claude-manager UI - Usage tab: cost, 5-hour blocks, heatmap, tools, pricing editor.
   Data comes from GET /api/usage, /api/blocks, /api/pricing (PUT to save overrides). Plain DOM + inline SVG, no deps.
   Every string that originates in session data (project paths, model names, tool names, titles) goes through esc(). */
(() => {
  const CM = window.CM;
  const { $, esc, fmt, api, toast, store, T } = CM;

  const num = (v) => (typeof v === 'number' && isFinite(v) ? v : 0);
  const pad = (n) => String(n).padStart(2, '0');
  const ymd = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const parseDay = (s) => { const [y, m, d] = String(s).split('-').map(Number); return new Date(y, (m || 1) - 1, d || 1, 12); };
  const addDays = (s, n) => { const d = parseDay(s); d.setDate(d.getDate() + n); return ymd(d); };
  const dm = (s) => { const p = String(s).split('-'); return p.length === 3 ? `${p[2]}/${p[1]}` : String(s); };
  const dmy = (s) => { const p = String(s).split('-'); return p.length === 3 ? `${p[2]}/${p[1]}/${p[0]}` : String(s); };
  const WD = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'].map((d) => T('usage.weekday_' + d));
  const wdOf = (s) => WD[(parseDay(s).getDay() + 6) % 7];
  const hm = (ms) => `${pad(new Date(ms).getHours())}:${pad(new Date(ms).getMinutes())}`;
  const dmHm = (ms) => { const d = new Date(ms); return `${pad(d.getDate())}/${pad(d.getMonth() + 1)} ${hm(ms)}`; };
  const dur = (min) => {
    min = Math.max(0, Math.round(min));
    if (min < 60) return T('usage.unit_min', { n: min });
    return Math.floor(min / 60) + 'h' + (min % 60 ? pad(min % 60) : '');
  };
  /** USD with 2 decimals from $1 up, 4 below. Never used for unpriced values (those render the "no price" label). */
  const usd = (n) => {
    n = num(n);
    if (n >= 1) return '$' + n.toLocaleString(I18N.locale, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    if (n === 0) return '$0.00';
    return n < 0.0001 ? '<$0.0001' : '$' + n.toFixed(4);
  };
  const usdAxis = (n) => (n >= 100 ? '$' + Math.round(n) : n >= 10 ? '$' + n.toFixed(0) : n >= 1 ? '$' + n.toFixed(1) : '$' + n.toFixed(2));
  const NA = T('usage.no_price');
  const niceMax = (v) => {
    if (!(v > 0)) return 1;
    const e = Math.pow(10, Math.floor(Math.log10(v)));
    const f = v / e;
    return ([1, 2, 4, 8, 10].find((c) => f <= c + 1e-9) || 10) * e;
  };
  const tokTotal = (r) => num(r.in) + num(r.out) + num(r.cc5) + num(r.cc1) + num(r.cr);
  const sum = (a, f) => a.reduce((s, x) => s + f(x), 0);

  const U = {
    range: store.get('cm-u-range') || '30', from: store.get('cm-u-from') || '', to: store.get('cm-u-to') || '',
    mode: store.get('cm-u-mode') === 'conversation' ? 'conversation' : 'billing',
    metric: store.get('cm-u-metric') || 'cost',
    data: null, err: '', loading: false, loadedAt: 0, seq: 0,
    blocks: null, blocksErr: '', blocksSeq: 0, showAllBlocks: false,
  };
  if (!['7', '14', '30', '90', '0', 'custom'].includes(U.range)) U.range = '30';
  if (!['cost', 'in', 'out', 'io', 'all'].includes(U.metric)) U.metric = 'cost';

  const isUsageTab = () => CM.getTab() === 'usage';

  function resolveRange() {
    const today = ymd(new Date());
    if (U.range === 'custom') return { from: U.from || addDays(today, -29), to: U.to || today };
    if (U.range === '0') return { from: '2000-01-01', to: today };
    return { from: addDays(today, -(Number(U.range) - 1)), to: today };
  }

  const tipEl = $('#tip');
  let tips = [];
  const tipPush = (html) => { tips.push(html); return tips.length - 1; };
  function tipShow(html, x, y) {
    tipEl.innerHTML = html;
    tipEl.classList.add('on');
    const w = tipEl.offsetWidth, h = tipEl.offsetHeight;
    let left = x + 14, top = y + 14;
    if (left + w > window.innerWidth - 8) left = Math.max(8, x - w - 14);
    if (top + h > window.innerHeight - 8) top = Math.max(8, y - h - 14);
    tipEl.style.transform = `translate(${Math.round(left)}px,${Math.round(top)}px)`;
  }
  const tipHide = () => tipEl.classList.remove('on');
  const tipRow = (label, val, swatch) => `<div class="tr"><span>${swatch ? `<i style="background:${swatch}"></i>` : ''}${esc(label)}</span><b>${esc(val)}</b></div>`;
  function bindTips(root) {
    const at = (e) => { const t = e.target.closest ? e.target.closest('[data-ti]') : null; return t && root.contains(t) ? t : null; };
    const move = (e) => { const t = at(e); if (!t) { tipHide(); return; } const h = tips[+t.dataset.ti]; if (h) tipShow(h, e.clientX, e.clientY); };
    root.addEventListener('pointermove', move);
    root.addEventListener('pointerdown', move);
    root.addEventListener('pointerleave', tipHide);
    window.addEventListener('scroll', tipHide, { passive: true, capture: true });
  }

  function normUsage(d) {
    const o = d && typeof d === 'object' ? d : {};
    o.rows = (Array.isArray(o.rows) ? o.rows : []).filter((r) => r && r.day);
    o.totals = Object.assign({ requests: 0, in: 0, out: 0, cc5: 0, cc1: 0, cr: 0, costUsd: 0, unpricedTokens: 0 }, o.totals);
    o.unpricedModels = Array.isArray(o.unpricedModels) ? o.unpricedModels : [];
    o.heat = Array.isArray(o.heat) ? o.heat : [];
    for (const k of ['tools', 'skills', 'subagents']) o[k] = Array.isArray(o[k]) ? o[k] : [];
    o.activeMinutesByDay = o.activeMinutesByDay && typeof o.activeMinutesByDay === 'object' ? o.activeMinutesByDay : {};
    o.retention = o.retention || null;
    return o;
  }

  async function loadUsage() {
    const my = ++U.seq;
    const { from, to } = resolveRange();
    if (from > to) {
      U.err = T('usage.range_invalid');
      renderAll();
      return;
    }
    U.loading = true; U.err = '';
    syncToolbar();
    loadBlocks();
    try {
      const d = await api('/api/usage?' + new URLSearchParams({ from, to, mode: U.mode }), { quiet: true });
      if (my !== U.seq) return;
      U.data = normUsage(d);
      U.loadedAt = Date.now();
    } catch (e) {
      if (my !== U.seq) return;
      U.err = T('usage.load_failed', { message: e.message });
    }
    U.loading = false;
    renderAll();
  }

  async function loadBlocks() {
    const my = ++U.blocksSeq;
    try {
      const d = await api('/api/blocks', { quiet: true });
      if (my !== U.blocksSeq) return;
      U.blocks = Array.isArray(d && d.blocks) ? d.blocks : [];
      U.blocksErr = '';
    } catch (e) {
      if (my !== U.blocksSeq) return;
      U.blocksErr = e.message;
    }
    renderBlocks();
  }

  function aggregate(rows, keyFn) {
    const m = new Map();
    for (const r of rows) {
      const k = keyFn(r);
      let a = m.get(k);
      if (!a) m.set(k, a = { key: k, n: 0, pricedN: 0, unpTok: 0, requests: 0, in: 0, out: 0, cache: 0, cost: 0, sample: r });
      a.n++; a.requests += num(r.requests); a.in += num(r.in); a.out += num(r.out); a.cache += num(r.cc5) + num(r.cc1) + num(r.cr);
      if (r.priced && r.costUsd != null) { a.pricedN++; a.cost += num(r.costUsd); } else a.unpTok += tokTotal(r);
    }
    return [...m.values()];
  }
  const costHtml = (a) => {
    if (a.pricedN === 0) return `<span class="unp">${NA}</span>`;
    return esc(usd(a.cost)) + (a.pricedN < a.n ? `<span class="unp" title="${esc(T('usage.unpriced_tokens_tip', { tokens: fmt(a.unpTok) }))}">${esc(T('usage.no_price_plus'))}</span>` : '');
  };
  const byCost = (x, y) => (y.cost - x.cost) || ((y.in + y.out + y.cache) - (x.in + x.out + x.cache));

  const sources = () => [...new Set((U.data ? U.data.rows : []).map((r) => r.source || ''))].sort();
  const metricVal = (r) => U.metric === 'cost' ? (r.priced && r.costUsd != null ? num(r.costUsd) : 0)
    : U.metric === 'in' ? num(r.in) : U.metric === 'out' ? num(r.out) : U.metric === 'io' ? num(r.in) + num(r.out) : tokTotal(r);
  const metricFmt = (v) => (U.metric === 'cost' ? usd(v) : fmt(Math.round(v)));
  const metricAxis = (v) => (U.metric === 'cost' ? usdAxis(v) : fmt(Math.round(v)));

  /** Continuous list of days to draw: the chosen range when it is short, otherwise first..last day with data. */
  function dayList(dataDays) {
    const sorted = [...dataDays].sort();
    let a, b;
    if (U.range === '0' || !sorted.length) { a = sorted[0]; b = sorted[sorted.length - 1]; }
    else { ({ from: a, to: b } = resolveRange()); }
    if (!a || !b) return [];
    const span = Math.round((parseDay(b) - parseDay(a)) / 86400000) + 1;
    if (span > 400 || span < 1) return sorted;
    const out = [];
    for (let d = a, i = 0; i < span; i++, d = addDays(d, 1)) out.push(d);
    return out;
  }

  const chartWidth = (el) => Math.max(280, Math.round(el.clientWidth || 600));

  function syncToolbar() {
    $('#uRange').value = U.range;
    $('#uCustom').hidden = U.range !== 'custom';
    const r = resolveRange();
    if (U.range === 'custom') { $('#uFrom').value = U.from || r.from; $('#uTo').value = U.to || r.to; }
    $('#uMetric').value = U.metric;
    for (const b of $('#uMode').querySelectorAll('button')) b.setAttribute('aria-pressed', String(b.dataset.m === U.mode));
    $('#uMeta').textContent = U.loading ? T('common.loading') : U.data ? T('usage.updated', { time: hm(U.loadedAt), tz: U.data.tz || T('usage.this_machine') }) : '';
  }

  function renderNotices() {
    const el = $('#uNotices');
    let h = '';
    if (U.err) {
      h += `<div class="notice err" role="alert"><div class="notice-main"><span class="notice-ic" aria-hidden="true">!</span><div class="notice-tx"><b>${esc(U.err)}</b></div><button type="button" data-act="retry">${esc(T('usage.retry'))}</button></div></div>`;
    }
    const d = U.data;
    if (d && d.unpricedModels.length) {
      const list = d.unpricedModels.slice(0, 8).map(esc).join(', ') + (d.unpricedModels.length > 8 ? ' ' + esc(T('usage.unpriced_models_more', { n: d.unpricedModels.length - 8 })) : '');
      h += `<div class="notice warn"><div class="notice-main"><span class="notice-ic" aria-hidden="true">!</span>
        <div class="notice-tx"><b>${esc(T('usage.unpriced_models', { n: d.unpricedModels.length }))}</b>
        <span class="muted">${list}${d.totals.unpricedTokens ? esc(T('usage.unpriced_tokens_note', { tokens: fmt(num(d.totals.unpricedTokens)) })) : '.'}</span></div>
        <button type="button" class="primary" data-act="pricing-unpriced">${esc(T('usage.add_price'))}</button></div></div>`;
    }
    el.innerHTML = h;
  }

  function renderCards() {
    const el = $('#uCards');
    const d = U.data;
    if (!d) { el.innerHTML = ''; return; }
    const t = d.totals;
    const anyPriced = d.rows.some((r) => r.priced && r.costUsd != null);
    const costB = !d.rows.length ? '-' : !anyPriced ? `<span class="na">${NA}</span>` : esc(usd(t.costUsd));
    const costSub = (num(t.unpricedTokens) > 0 && anyPriced ? T('usage.card_cost_sub_unpriced', { tokens: fmt(num(t.unpricedTokens)) }) : T('usage.card_cost_sub'));
    const card = (b, s, sub, cls) => `<div class="card ${cls || ''}"><b>${b}</b><span>${esc(s)}</span>${sub ? `<small>${esc(sub)}</small>` : ''}</div>`;
    el.innerHTML =
      card(costB, T('usage.card_cost'), costSub, 'cost') +
      card(esc(fmt(num(t.requests))), T('usage.card_requests')) +
      card(esc(fmt(num(t.in))), T('usage.card_input')) +
      card(esc(fmt(num(t.out))), T('usage.card_output')) +
      card(esc(fmt(num(t.cc5) + num(t.cc1))), T('usage.card_cache_write'), T('usage.card_cache_write_sub', { m5: fmt(num(t.cc5)), h1: fmt(num(t.cc1)) })) +
      card(esc(fmt(num(t.cr))), T('usage.card_cache_read')) +
      insightCards(d, card);
  }

  const pct = (v) => (typeof v === 'number' && isFinite(v) ? v.toFixed(1) + '%' : '-');
  function insightCards(d, card) {
    if (d.cacheHitPct === undefined && !d.toolErrors) return '';
    const te = d.toolErrors || {}, hk = d.hooks || {};
    return card(esc(pct(d.cacheHitPct)), T('usage.card_cache_hit'), T('usage.card_cache_hit_sub')) +
      card(d.costPerPrompt == null ? '-' : esc(usd(d.costPerPrompt)), T('usage.card_cost_per_prompt'), d.costPerSession == null ? '' : T('usage.card_cost_per_prompt_sub', { perSession: usd(d.costPerSession), n: fmt(num(d.sessionsInRange)) })) +
      card(esc(pct(te.rate)), T('usage.card_tool_error_rate'), T('usage.card_tool_error_rate_sub', { errors: fmt(num(te.errors)), calls: fmt(num(te.calls)) })) +
      card(esc(pct(d.subagentShare)), T('usage.card_subagent_cost'), d.subagentCostUsd ? usd(d.subagentCostUsd) : '') +
      card(esc(fmt(num(hk.failingSessions))), T('usage.card_hook_sessions'), T('usage.card_hook_sessions_sub', { n: fmt(num(hk.failures)) })) +
      card(esc(fmt(num(d.compacts))), T('usage.card_compacts'), T('usage.card_compacts_sub', { n: fmt(num(d.sessions3plusCompacts)) }));
  }

  function insightGrid() {
    let g = $('#gInsights');
    if (g) return g;
    const top = $('#gTop');
    if (!top) return null;
    g = document.createElement('div');
    g.className = 'grid3'; g.id = 'gInsights';
    const sec = (id, title) => `<section class="panel" aria-labelledby="h${id}"><div class="panel-hd"><h3 id="h${id}">${esc(title)}</h3></div><div id="${id}"></div></section>`;
    g.innerHTML = sec('uTopSess', T('usage.ins_top_sessions')) + sec('uToolErr', T('usage.ins_tool_errors')) + sec('uHooks', T('usage.ins_hooks')) + sec('uSubCost', T('usage.ins_subagent_cost'));
    top.after(g);
    g.addEventListener('click', (e) => {
      const a = e.target.closest('[data-open]');
      if (a && window.CM && CM.openSession) { e.preventDefault(); CM.openSession(a.dataset.open); }
    });
    return g;
  }
  const emptyList = () => `<div class="chart-empty" style="padding:14px">${esc(T('usage.empty_data'))}</div>`;
  function barList(items, nameHtml, valOf, valText) {
    if (!items.length) return emptyList();
    const mx = Math.max(1e-9, ...items.map(valOf));
    return '<ul class="toplist">' + items.map((x) =>
      `<li><span class="nm">${nameHtml(x)}</span><span class="ct">${esc(valText(x))}</span><span class="tr" aria-hidden="true"><i style="width:${(valOf(x) / mx * 100).toFixed(1)}%"></i></span></li>`).join('') + '</ul>';
  }
  const NODE_HINT = /node: not found|node: command not found|Bun not found/i;
  function renderInsights() {
    const g = insightGrid();
    if (!g) return;
    const d = U.data;
    g.hidden = !d || !d.toolErrors;
    if (g.hidden) return;
    const arr = (v) => (Array.isArray(v) ? v : []);
    $('#uTopSess').innerHTML = barList(arr(d.topSessions), (x) => `<a href="#" data-open="${esc(x.id)}" title="${esc(x.title)} - ${esc(x.project || '')}">${esc(x.title)}</a>`,
      (x) => num(x.costUsd), (x) => usd(num(x.costUsd)) + (x.compacts ? ' · ' + T('usage.ins_compact_count', { n: x.compacts }) : ''));
    const te = d.toolErrors;
    $('#uToolErr').innerHTML = barList(arr(te.top), (x) => esc(x.tool), (x) => num(x.count), (x) => fmt(num(x.count))) +
      (arr(te.topMessages).length ? `<h4 class="muted small" style="margin:10px 0 4px">${esc(T('usage.ins_error_messages'))}</h4>` +
        barList(arr(te.topMessages), (x) => `<span title="${esc(x.text)}">${esc(x.text || T('usage.ins_empty_text'))}</span>`, (x) => num(x.count), (x) => fmt(num(x.count))) : '');
    const hk = d.hooks || {};
    $('#uHooks').innerHTML = barList(arr(hk.top), (x) => `<span title="${esc(x.example)}">${esc(x.name)}</span>` +
      (NODE_HINT.test(x.example || '') ? ` <span class="badge unp" title="${esc(x.example)}">${esc(T('usage.ins_node_missing'))}</span>` : ''),
      (x) => num(x.sessions), (x) => T('usage.ins_hook_count', { sessions: fmt(num(x.sessions)), count: fmt(num(x.count)) })) +
      (arr(hk.top).some((x) => NODE_HINT.test(x.example || '')) ? `<p class="blk-note" style="margin:6px 0 0">${esc(T('usage.ins_node_hint'))}</p>` : '');
    $('#uSubCost').innerHTML = barList(arr(d.subagentCostByType).filter((x) => num(x.costUsd) > 0), (x) => esc(x.name), (x) => num(x.costUsd), (x) => usd(num(x.costUsd)));
  }

  function renderBlocks() {
    const el = $('#uBlocks');
    if (U.blocksErr && !U.blocks) { el.innerHTML = `<div class="chart-empty">${esc(T('usage.blocks_load_failed', { message: U.blocksErr }))}</div>`; return; }
    if (!U.blocks) { el.innerHTML = `<div class="chart-empty">${esc(T('common.loading'))}</div>`; return; }
    const all = U.blocks.slice().sort((a, b) => num(a.startMs) - num(b.startMs));
    const live = all.find((b) => b.isActive && !b.isGap);
    const costTxt = (v) => (v == null ? NA : usd(v));
    let h = '';
    if (live) {
      const now = Date.now();
      const total = Math.max(1, num(live.endMs) - num(live.startMs));
      const pct = Math.max(0, Math.min(100, (now - live.startMs) / total * 100));
      const leftMin = Math.max(0, (live.endMs - now) / 60000);
      const b = live.burn, p = live.projected;
      h += `<div class="blk-active"><div class="hd"><span class="live-dot" aria-hidden="true"></span><b>${esc(T('usage.blocks_active_title'))}</b>
        <span>${esc(dmHm(live.startMs))} → ${esc(hm(live.endMs))}</span><span class="muted">${esc((live.models || []).join(', '))}</span></div>
        <div><div class="pbar" role="progressbar" aria-label="${esc(T('usage.blocks_progress_label'))}" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${Math.round(pct)}"><i style="width:${pct.toFixed(1)}%"></i></div>
        <div class="pbar-lbl"><span>${esc(T('usage.blocks_elapsed', { time: dur((now - live.startMs) / 60000), pct: Math.round(pct) }))}</span><span>${esc(T('usage.blocks_left', { time: dur(leftMin) }))}</span></div></div>
        <div class="kv">
          <div><b>${esc(fmt(num(live.in) + num(live.out)))}</b><span>${esc(T('usage.blocks_tokens_io', { total: fmt(tokTotal(live)) }))}</span></div>
          <div><b>${live.costUsd == null ? `<span class="unp" style="margin:0;font-size:13px">${NA}</span>` : esc(usd(live.costUsd))}</b><span>${esc(T('usage.blocks_cost_requests', { n: fmt(num(live.requests)) }))}</span></div>
          <div><b>${b ? esc(T('usage.blocks_per_min', { n: fmt(Math.round(num(b.tokensPerMin))) })) : '-'}</b><span>${esc(b ? T('usage.blocks_burn_detail', { n: fmt(Math.round(num(b.tokensPerMinNoCache))), cost: b.costPerHour == null ? NA : T('usage.blocks_per_hour', { cost: usd(b.costPerHour) }) }) : T('usage.blocks_burn_none'))}</span></div>
          <div><b>${p ? '~' + esc(fmt(Math.round(num(p.tokens)))) : '-'}</b><span>${esc(p ? T('usage.blocks_projected', { cost: p.costUsd == null ? NA : '~' + usd(p.costUsd) }) : T('usage.blocks_projected_none'))}</span></div>
        </div></div>`;
    } else {
      h += `<p class="blk-note">${esc(T('usage.blocks_no_active'))}</p>`;
    }
    const real = all.filter((b) => !b.isGap);
    if (!real.length) { el.innerHTML = h + `<div class="chart-empty">${esc(T('usage.blocks_none'))}</div>`; return; }
    const desc = all.slice().reverse();
    // the default view keeps the 14 most recent REAL blocks (gap rows used to eat half of the 14 rows, so
    // only ~3 days were visible); "show all" lists every block of the 30 days returned by /api/blocks.
    const RECENT = 14;
    let cut = desc.length;
    if (!U.showAllBlocks) { let n = 0; cut = desc.findIndex((b) => !b.isGap && ++n > RECENT); if (cut < 0) cut = desc.length; }
    const shown = desc.slice(0, cut);
    const oldest = real[0], newest = real[real.length - 1];
    const maxTok = Math.max(1, ...real.map((b) => num(b.in) + num(b.out)));
    h += `<div class="tablewrap scroll"><table class="blk-list"><thead><tr><th>${esc(T('usage.blocks_col_start'))}</th><th class="num">${esc(T('usage.blocks_col_requests'))}</th><th class="num">${esc(T('usage.blocks_col_io'))}</th><th class="num">${esc(T('usage.blocks_col_cache'))}</th><th class="cost">${esc(T('usage.blocks_col_cost'))}</th><th>${esc(T('usage.blocks_col_model'))}</th></tr></thead><tbody>`;
    for (const b of shown) {
      if (b.isGap) { h += `<tr class="gap static"><td colspan="6">${esc(T('usage.blocks_gap', { time: dur((num(b.endMs) - num(b.startMs)) / 60000), range: dmHm(b.startMs) + ' → ' + hm(b.endMs) }))}</td></tr>`; continue; }
      const io = num(b.in) + num(b.out);
      h += `<tr class="static ${b.isActive ? 'live' : ''}"><td>${esc(dmHm(b.startMs))} → ${esc(hm(b.endMs))}${b.isActive ? ` <span class="badge exp">${esc(T('usage.blocks_running'))}</span>` : ''}</td>
        <td class="num">${esc(fmt(num(b.requests)))}</td><td class="num">${esc(fmt(io))}<span class="minibar" aria-hidden="true"><i style="width:${Math.max(2, io / maxTok * 100).toFixed(0)}%"></i></span></td>
        <td class="num muted">${esc(fmt(num(b.cc5) + num(b.cc1) + num(b.cr)))}</td>
        <td class="cost">${b.costUsd == null ? `<span class="unp">${NA}</span>` : esc(usd(b.costUsd))}</td>
        <td class="name muted">${esc((b.models || []).join(', '))}</td></tr>`;
    }
    h += '</tbody></table></div>';
    const nShown = shown.filter((b) => !b.isGap).length;
    h += `<p class="blk-note" style="margin:8px 0 0">${esc(T('usage.blocks_showing', { shown: nShown, total: real.length, from: dmHm(oldest.startMs), to: dmHm(newest.startMs) }))}`;
    if (real.length > RECENT) h += ` <button type="button" data-act="blocks-more">${esc(U.showAllBlocks ? T('usage.blocks_show_recent') : T('usage.blocks_show_all', { n: real.length }))}</button>`;
    h += '</p>';
    el.innerHTML = h;
  }

  function renderChart() {
    const el = $('#uChart'), lg = $('#uLegend');
    const d = U.data;
    if (!d) { el.innerHTML = `<div class="chart-empty">${esc(T('common.loading'))}</div>`; lg.innerHTML = ''; return; }
    if (!d.rows.length) { el.innerHTML = `<div class="chart-empty">${esc(T('usage.empty_range'))}</div>`; lg.innerHTML = ''; return; }
    const srcs = sources();
    const byDay = new Map();
    for (const r of d.rows) {
      let o = byDay.get(r.day);
      if (!o) byDay.set(r.day, o = { src: {}, unp: 0 });
      o.src[r.source || ''] = (o.src[r.source || ''] || 0) + metricVal(r);
      if (!(r.priced && r.costUsd != null)) o.unp += tokTotal(r);
    }
    const days = dayList(byDay.keys());
    const tot = (o) => (o ? Object.values(o.src).reduce((a, b) => a + b, 0) : 0);
    const max = niceMax(Math.max(0, ...days.map((k) => tot(byDay.get(k)))));
    const W = chartWidth(el), H = W < 520 ? 190 : 230, pl = U.metric === 'cost' ? 50 : 44, pr = 4, pt = 8, pb = 22;
    const iw = W - pl - pr, slot = iw / Math.max(1, days.length), bw = Math.max(2, Math.min(36, slot * 0.72));
    tips = [];
    let svg = `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(U.metric === 'cost' ? T('usage.chart_aria_cost') : T('usage.chart_aria_tokens'))}">`;
    for (let i = 0; i <= 4; i++) {
      const y = pt + (H - pb - pt) * (1 - i / 4);
      svg += `<line class="grid-line" x1="${pl}" x2="${W - pr}" y1="${y}" y2="${y}"/><text x="${pl - 6}" y="${y + 3}" text-anchor="end">${esc(metricAxis(max * i / 4))}</text>`;
    }
    const lblEvery = Math.max(1, Math.ceil(days.length / Math.max(2, Math.floor(iw / 46))));
    days.forEach((k, i) => {
      const o = byDay.get(k);
      const x = pl + i * slot + (slot - bw) / 2;
      let y = H - pb;
      let bars = '';
      if (o) {
        srcs.forEach((s, si) => {
          const v = o.src[s] || 0;
          const h = v / max * (H - pb - pt);
          if (h > 0.4) bars += `<rect class="sg f${si % 5}" x="${x.toFixed(1)}" y="${(y - h).toFixed(1)}" width="${bw.toFixed(1)}" height="${h.toFixed(1)}" rx="${Math.min(2, h / 2)}"/>`;
          y -= h;
        });
      }
      let tip = `<div class="tt">${esc(dmy(k))} (${esc(wdOf(k))})</div>`;
      if (o) {
        const act = srcs.filter((s) => o.src[s] > 0);
        for (const s of act) tip += tipRow(s || T('common.unknown_paren'), metricFmt(o.src[s]), `var(--s${srcs.indexOf(s) % 5})`);
        if (act.length > 1) tip += tipRow(T('usage.tip_total'), metricFmt(tot(o)));
        if (!act.length) tip += `<div>${esc(T('usage.tip_no_activity'))}</div>`;
        if (U.metric === 'cost' && o.unp > 0) tip += `<div class="tw">${esc(T('usage.tip_unpriced_tokens', { tokens: fmt(o.unp) }))}</div>`;
        if (U.metric === 'cost') tip += `<div class="muted">${esc(T('usage.tip_estimated'))}</div>`;
      } else tip += `<div>${esc(T('usage.tip_no_activity'))}</div>`;
      svg += bars + `<rect class="hit" data-ti="${tipPush(tip)}" x="${(pl + i * slot).toFixed(1)}" y="${pt}" width="${slot.toFixed(1)}" height="${H - pb - pt}"/>`;
      if (i % lblEvery === 0) svg += `<text x="${(x + bw / 2).toFixed(1)}" y="${H - 6}" text-anchor="middle">${esc(dm(k))}</text>`;
    });
    el.innerHTML = `<div class="chart-wrap">${svg}</svg></div>`;
    lg.innerHTML = srcs.length > 1 ? srcs.map((s, i) => `<span><i class="lg${i % 5}"></i>${esc(s || T('common.unknown_paren'))}</span>`).join('') : '';
  }

  function renderActive() {
    const el = $('#uActive');
    const d = U.data;
    if (!d) { el.innerHTML = `<div class="chart-empty">${esc(T('common.loading'))}</div>`; return; }
    const map = d.activeMinutesByDay;
    const keys = Object.keys(map);
    if (!keys.length) { el.innerHTML = `<div class="chart-empty">${esc(T('usage.empty_active'))}</div>`; return; }
    const days = dayList(keys);
    const mx = Math.max(1, ...days.map((k) => num(map[k])));
    const useH = mx >= 120;
    const max = niceMax(useH ? mx / 60 : mx) * (useH ? 60 : 1);
    const W = chartWidth(el), H = 150, pl = 36, pr = 4, pt = 8, pb = 22;
    const iw = W - pl - pr, slot = iw / Math.max(1, days.length), bw = Math.max(2, Math.min(30, slot * 0.72));
    let svg = `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(T('usage.active_aria'))}">`;
    for (let i = 0; i <= 2; i++) {
      const y = pt + (H - pb - pt) * (1 - i / 2);
      const v = max * i / 2;
      svg += `<line class="grid-line" x1="${pl}" x2="${W - pr}" y1="${y}" y2="${y}"/><text x="${pl - 6}" y="${y + 3}" text-anchor="end">${esc(useH ? T('usage.unit_hour_axis', { n: (v / 60).toFixed(v % 60 ? 1 : 0) }) : T('usage.unit_min_axis', { n: Math.round(v) }))}</text>`;
    }
    const lblEvery = Math.max(1, Math.ceil(days.length / Math.max(2, Math.floor(iw / 46))));
    days.forEach((k, i) => {
      const v = num(map[k]);
      const x = pl + i * slot + (slot - bw) / 2;
      const h = v / max * (H - pb - pt);
      const tip = `<div class="tt">${esc(dmy(k))} (${esc(wdOf(k))})</div>${tipRow(T('usage.active_tip_label'), v ? dur(v) : T('usage.active_none'))}`;
      if (h > 0.4) svg += `<rect class="f-act" x="${x.toFixed(1)}" y="${(H - pb - h).toFixed(1)}" width="${bw.toFixed(1)}" height="${h.toFixed(1)}" rx="${Math.min(2, h / 2)}"/>`;
      svg += `<rect class="hit" data-ti="${tipPush(tip)}" x="${(pl + i * slot).toFixed(1)}" y="${pt}" width="${slot.toFixed(1)}" height="${H - pb - pt}"/>`;
      if (i % lblEvery === 0) svg += `<text x="${(x + bw / 2).toFixed(1)}" y="${H - 6}" text-anchor="middle">${esc(dm(k))}</text>`;
    });
    const totalMin = sum(days, (k) => num(map[k]));
    el.innerHTML = `<div class="chart-wrap">${svg}</svg></div><p class="blk-note" style="margin:6px 0 0">${esc(T('usage.active_summary', { total: dur(totalMin), avg: dur(totalMin / Math.max(1, days.filter((k) => num(map[k]) > 0).length)) }))}</p>`;
  }

  function renderHeat() {
    const el = $('#uHeat');
    const d = U.data;
    if (!d) { el.innerHTML = `<div class="chart-empty">${esc(T('common.loading'))}</div>`; return; }
    const grid = Array.from({ length: 7 }, (_, r) => Array.from({ length: 24 }, (_, c) => num(d.heat[r] && d.heat[r][c])));
    const mx = Math.max(...grid.flat());
    $('#uHeatTz').textContent = T('usage.heat_subtitle_tz', { tz: d.tz ? 'UTC' + d.tz : T('usage.heat_tz_local') });
    if (!(mx > 0)) { el.innerHTML = `<div class="chart-empty">${esc(T('usage.empty_data'))}</div>`; return; }
    const W = chartWidth(el), lw = 24, th = 16;
    const cw = (W - lw) / 24, chh = Math.max(12, Math.min(22, cw)), gap = 2;
    const H = th + 7 * (chh + gap);
    const hourEvery = W < 520 ? 6 : 3;
    let svg = `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(T('usage.heat_aria'))}">`;
    for (let c = 0; c < 24; c += hourEvery) svg += `<text x="${(lw + c * cw + cw / 2).toFixed(1)}" y="10" text-anchor="middle">${c}h</text>`;
    let peak = { v: -1, r: 0, c: 0 };
    for (let r = 0; r < 7; r++) {
      const y = th + r * (chh + gap);
      svg += `<text x="${lw - 5}" y="${(y + chh / 2 + 3).toFixed(1)}" text-anchor="end">${WD[r]}</text>`;
      for (let c = 0; c < 24; c++) {
        const v = grid[r][c];
        if (v > peak.v) peak = { v, r, c };
        const lvl = v > 0 ? Math.min(4, Math.max(1, Math.ceil(Math.sqrt(v / mx) * 4))) : 0;
        const tip = `<div class="tt">${WD[r]} · ${pad(c)}:00-${pad((c + 1) % 24)}:00</div>${tipRow(T('usage.heat_tip_tokens'), v ? fmt(v) : T('usage.heat_none'))}`;
        svg += `<rect class="hm-cell hm-${lvl}" data-ti="${tipPush(tip)}" x="${(lw + c * cw + gap / 2).toFixed(1)}" y="${y.toFixed(1)}" width="${(cw - gap).toFixed(1)}" height="${chh}" rx="3"/>`;
      }
    }
    el.innerHTML = `<div class="chart-wrap">${svg}</svg></div><div class="hm-leg"><span>${esc(T('usage.heat_less'))}</span><i class="hm-0b"></i><i class="hm-1b"></i><i class="hm-2b"></i><i class="hm-3b"></i><i class="hm-4b"></i><span>${esc(T('usage.heat_more'))}</span><span class="sp"></span><span>${esc(T('usage.heat_peak', { day: WD[peak.r], hour: peak.c, value: fmt(peak.v) }))}</span></div>`;
  }

  function renderTop(id, list) {
    const el = $(id);
    const items = list.filter((x) => x && x.name).sort((a, b) => num(b.count) - num(a.count)).slice(0, 10);
    if (!items.length) { el.innerHTML = emptyList(); return; }
    const mx = Math.max(1, ...items.map((x) => num(x.count)));
    el.innerHTML = '<ul class="toplist">' + items.map((x) =>
      `<li><span class="nm" title="${esc(x.name)}">${esc(x.name)}</span><span class="ct">${esc(num(x.count).toLocaleString(I18N.locale))}</span><span class="tr" aria-hidden="true"><i style="width:${(num(x.count) / mx * 100).toFixed(1)}%"></i></span></li>`).join('') + '</ul>';
  }

  function table(el, head, rows, extra) {
    el.innerHTML = `<thead><tr>${head.map(([t, c]) => `<th class="${c || ''}">${esc(t)}</th>`).join('')}</tr></thead><tbody>` +
      (rows.length ? rows.map((r) => `<tr class="static">${r.map((c, i) => `<td class="${head[i][1] || 'name'}">${c}</td>`).join('')}</tr>`).join('') +
        (extra ? `<tr class="static"><td colspan="${head.length}" class="muted">${extra}</td></tr>` : '')
        : `<tr class="static"><td colspan="${head.length}" class="muted">${esc(T('usage.empty_data'))}</td></tr>`) + '</tbody>';
  }
  const std = (a) => [esc(fmt(a.requests)), esc(fmt(a.in)), esc(fmt(a.out)), esc(fmt(a.cache)), costHtml(a)];
  const stdHead = () => [[T('usage.tbl_col_requests'), 'num'], [T('usage.tbl_col_input'), 'num'], [T('usage.tbl_col_output'), 'num'], [T('usage.tbl_col_cache'), 'num'], [T('usage.tbl_col_cost'), 'cost']];

  function renderTables() {
    const d = U.data;
    const rows = d ? d.rows : [];
    const proj = aggregate(rows, (r) => (r.source || '') + '\u0000' + (r.project || '')).sort(byCost);
    table($('#byProject'), [[T('usage.tbl_col_project')], ...stdHead()], proj.slice(0, 30).map((a) => {
      const [src, p] = a.key.split('\u0000');
      return [`<span class="badge ${CM.srcClass(src)}">${esc(src)}</span> <span title="${esc(p)}">${esc(p || T('common.unknown_paren'))}</span>`, ...std(a)];
    }), proj.length > 30 ? esc(T('usage.tbl_more_projects', { n: proj.length - 30 })) : '');
    table($('#byModel'), [[T('usage.tbl_col_model')], ...stdHead()], aggregate(rows, (r) => r.model || T('common.unknown_paren')).sort(byCost)
      .map((a) => [esc(a.key) + (a.pricedN === 0 ? ` <span class="badge unp">${NA}</span>` : ''), ...std(a)]));
    table($('#bySource'), [[T('usage.tbl_col_source')], ...stdHead()], aggregate(rows, (r) => r.source || '').sort(byCost)
      .map((a) => [`<span class="badge ${CM.srcClass(a.key)}">${esc(a.key || T('common.unknown_paren'))}</span>`, ...std(a)]));
    const act = d ? d.activeMinutesByDay : {};
    table($('#byDay'), [[T('usage.tbl_col_day')], ...stdHead(), [T('usage.tbl_col_active'), 'num']], aggregate(rows, (r) => r.day).sort((x, y) => (x.key < y.key ? 1 : -1))
      .map((a) => [esc(dmy(a.key)) + ` <span class="muted">${esc(wdOf(a.key))}</span>`, ...std(a), act[a.key] ? esc(dur(act[a.key])) : '<span class="muted">-</span>']));
  }

  function renderAll() {
    tips = []; tipHide();
    syncToolbar();
    renderNotices();
    renderCards();
    renderBlocks();
    renderChart();
    renderActive();
    renderHeat();
    const d = U.data;
    renderTop('#uTools', d ? d.tools : []);
    renderTop('#uSkills', d ? d.skills : []);
    renderTop('#uSubs', d ? d.subagents : []);
    renderInsights();
    renderTables();
  }

  const dlg = $('#pricingDlg');
  const P = { data: null, rows: [], aliases: [], seq: 0, loading: false, loadErr: '' };
  const numStr = (v) => (v == null || v === '' ? '' : String(v));
  const rowFrom = (m, isNew) => ({
    k: ++P.seq, id: m.id || '', match: m.match === 'exact' ? 'exact' : 'prefix',
    inP: numStr(m.inPerM), outP: numStr(m.outPerM), cr: numStr(m.cacheReadPerM), cw5: numStr(m.cacheWrite5mPerM), cw1: numStr(m.cacheWrite1hPerM),
    fast: numStr(m.fastMultiplier), derived: !!m.derived, lc: m.longContext || null, isNew: !!isNew,
  });

  const FIELDS = [
    ['inP', 'pricing.field_in'], ['outP', 'pricing.field_out'], ['cr', 'pricing.field_cache_read'],
    ['cw5', 'pricing.field_cache_write_5m'], ['cw1', 'pricing.field_cache_write_1h'], ['fast', 'pricing.field_fast'],
  ];

  function rowHtml(r) {
    const f = (key, label) => {
      const dis = r.derived && ['cr', 'cw5', 'cw1'].includes(key);
      return `<div class="field"><label for="pr-${r.k}-${key}">${esc(T(label))}</label><input id="pr-${r.k}-${key}" data-k="${r.k}" data-f="${key}" type="number" inputmode="decimal" step="any" min="0" value="${esc(r[key])}" ${dis ? 'disabled' : ''}></div>`;
    };
    return `<div class="pr-row" role="group" aria-label="${esc(T('pricing.row_label', { model: r.id || T('pricing.new_model') }))}">
      <div class="pr-grid">
        <div class="field wide"><label for="pr-${r.k}-id">${esc(T('pricing.model_id'))}</label><input id="pr-${r.k}-id" data-k="${r.k}" data-f="id" type="text" list="prIds" spellcheck="false" autocapitalize="off" value="${esc(r.id)}" placeholder="${esc(T('pricing.model_id_ph'))}"></div>
        <div class="field"><label for="pr-${r.k}-match">${esc(T('pricing.match'))}</label><select id="pr-${r.k}-match" data-k="${r.k}" data-f="match"><option value="prefix" ${r.match === 'prefix' ? 'selected' : ''}>${esc(T('pricing.match_prefix'))}</option><option value="exact" ${r.match === 'exact' ? 'selected' : ''}>${esc(T('pricing.match_exact'))}</option></select></div>
        ${FIELDS.map(([k, l]) => f(k, l)).join('')}
      </div>
      <div class="foot"><label><input type="checkbox" data-k="${r.k}" data-f="derived" ${r.derived ? 'checked' : ''}> ${esc(T('pricing.derived'))}</label>
        ${r.lc ? `<span class="muted">${esc(T('pricing.keep_long_context'))}</span>` : ''}
        <button type="button" class="danger" data-pr="del" data-k="${r.k}">${esc(T('pricing.delete_row'))}</button></div></div>`;
  }

  function renderPricing() {
    const body = $('#prBody');
    if (P.loading) { body.innerHTML = `<div class="chart-empty">${esc(T('usage.loading_pricing'))}</div>`; return; }
    if (P.loadErr) { body.innerHTML = `<div class="chart-empty">${esc(T('pricing.load_failed', { message: P.loadErr }))}</div>`; return; }
    const d = P.data || {};
    const seen = Array.isArray(d.seenModels) ? d.seenModels : [];
    const base = (Array.isArray(d.models) ? d.models : []).filter((m) => m.source !== 'override');
    const ids = [...new Set([...seen.map((s) => s.model), ...base.map((m) => m.id)])].filter(Boolean);
    const rate = (v) => (v == null ? '-' : esc(String(v)));
    body.innerHTML = `
      <p class="pr-meta">${esc(T('pricing.meta', { source: d.snapshotSource || 'litellm', date: d.snapshotDate ? ' (' + d.snapshotDate + ')' : '' }))}</p>
      ${seen.length ? `<div><h3>${esc(T('pricing.seen_title'))}</h3><div class="pr-seen">${seen.slice().sort((a, b) => num(b.tokens) - num(a.tokens)).map((s) =>
        `<div><span class="nm" title="${esc(s.model)}">${esc(s.model)}</span><span class="muted">${esc(T('pricing.seen_tokens', { n: fmt(num(s.tokens)) }))}</span>${s.priced ? `<span class="badge win">${esc(T('pricing.priced'))}</span>`
          : `<span class="badge unp">${NA}</span><button type="button" data-pr="add" data-id="${esc(s.model)}">${esc(T('pricing.add_price'))}</button>`}</div>`).join('')}</div></div>` : ''}
      <div><h3>${esc(T('pricing.custom_title'))}</h3><div id="prRows" style="display:grid;gap:8px;margin-top:6px">${P.rows.length ? P.rows.map(rowHtml).join('') : `<p class="pr-meta">${esc(T('pricing.custom_none'))}</p>`}</div>
        <p style="margin:8px 0 0"><button type="button" data-pr="add" data-id="">${esc(T('pricing.add_model'))}</button></p></div>
      <div><h3>${esc(T('pricing.aliases_title'))}</h3><div id="prAliases" style="display:grid;gap:6px;margin-top:6px">${P.aliases.map((a) =>
        `<div class="pr-alias"><div class="field"><label for="pa-${a.k}-a">${esc(T('pricing.alias'))}</label><input id="pa-${a.k}-a" type="text" data-ak="${a.k}" data-af="alias" spellcheck="false" autocapitalize="off" value="${esc(a.alias)}"></div>
        <div class="field"><label for="pa-${a.k}-t">${esc(T('pricing.alias_target'))}</label><input id="pa-${a.k}-t" type="text" list="prIds" data-ak="${a.k}" data-af="target" spellcheck="false" autocapitalize="off" value="${esc(a.target)}"></div>
        <button type="button" class="danger" data-pr="delalias" data-ak="${a.k}" aria-label="${esc(T('pricing.alias_delete_label', { alias: a.alias }))}">${esc(T('pricing.alias_delete'))}</button></div>`).join('')}</div>
        <p style="margin:8px 0 0"><button type="button" data-pr="addalias">${esc(T('pricing.add_alias'))}</button></p></div>
      ${base.length ? `<details><summary class="muted small" style="cursor:pointer">${esc(T('pricing.defaults_summary', { n: base.length }))}</summary>
        <div class="tablewrap scroll" style="margin-top:6px;max-height:240px"><table><thead><tr><th>${esc(T('pricing.defaults_col_model'))}</th><th class="num">${esc(T('pricing.defaults_col_in'))}</th><th class="num">${esc(T('pricing.defaults_col_out'))}</th><th class="num">${esc(T('pricing.defaults_col_cache_read'))}</th><th class="num">${esc(T('pricing.defaults_col_w5'))}</th><th class="num">${esc(T('pricing.defaults_col_w1'))}</th><th></th></tr></thead><tbody>${base.map((m) =>
          `<tr class="static"><td class="name" title="${esc(m.id)}">${esc(m.id)}</td><td class="num">${rate(m.inPerM)}</td><td class="num">${rate(m.outPerM)}</td><td class="num">${rate(m.cacheReadPerM)}</td><td class="num">${rate(m.cacheWrite5mPerM)}</td><td class="num">${rate(m.cacheWrite1hPerM)}</td>
          <td><button type="button" data-pr="tpl" data-id="${esc(m.id)}" title="${esc(T('pricing.use_template_tip'))}">${esc(T('pricing.use_template'))}</button></td></tr>`).join('')}</tbody></table></div></details>` : ''}
      <datalist id="prIds">${ids.map((i) => `<option value="${esc(i)}"></option>`).join('')}</datalist>`;
  }

  async function openPricing(prefill) {
    P.loading = true; P.loadErr = ''; P.rows = []; P.aliases = [];
    $('#prErr').hidden = true;
    renderPricing();
    if (!dlg.open) dlg.showModal();
    try {
      const d = await api('/api/pricing', { quiet: true });
      P.data = d || {};
      P.rows = (Array.isArray(P.data.models) ? P.data.models : []).filter((m) => m.source === 'override').map((m) => rowFrom(m, false));
      P.aliases = Object.entries(P.data.aliases && typeof P.data.aliases === 'object' ? P.data.aliases : {}).map(([alias, target]) => ({ k: ++P.seq, alias, target: String(target) }));
      for (const id of prefill || []) if (!P.rows.some((r) => r.id === id)) P.rows.push(rowFrom({ id }, true));
    } catch (e) { P.loadErr = e.message; }
    P.loading = false;
    renderPricing();
    $('#prSave').disabled = !!P.loadErr;
    const first = $('#prBody input[data-f="inP"][value=""]') || $('#prBody input, #prBody button');
    if (first && prefill && prefill.length) first.focus();
  }

  function syncRow(e) {
    const t = e.target;
    if (t.dataset.k) {
      const r = P.rows.find((x) => x.k === +t.dataset.k);
      if (!r) return;
      const f = t.dataset.f;
      if (f === 'derived') {
        r.derived = t.checked;
        for (const k of ['cr', 'cw5', 'cw1']) { const i = $(`#pr-${r.k}-${k}`); if (i) i.disabled = r.derived; }
      } else r[f] = t.value;
    } else if (t.dataset.ak) {
      const a = P.aliases.find((x) => x.k === +t.dataset.ak);
      if (a) a[t.dataset.af] = t.value;
    }
  }
  $('#prBody').addEventListener('input', syncRow);
  $('#prBody').addEventListener('change', syncRow);
  $('#prBody').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-pr]');
    if (!b) return;
    const act = b.dataset.pr;
    if (act === 'add') {
      const id = b.dataset.id;
      const ex = id && P.rows.find((r) => r.id === id);
      if (!ex) P.rows.push(rowFrom({ id }, true));
      renderPricing();
      const r = ex || P.rows[P.rows.length - 1];
      const inp = $(`#pr-${r.k}-${id ? 'inP' : 'id'}`);
      if (inp) { inp.scrollIntoView({ block: 'center' }); inp.focus(); }
    } else if (act === 'del') {
      P.rows = P.rows.filter((r) => r.k !== +b.dataset.k);
      renderPricing();
    } else if (act === 'addalias') {
      P.aliases.push({ k: ++P.seq, alias: '', target: '' });
      renderPricing();
      const a = P.aliases[P.aliases.length - 1];
      $(`#pa-${a.k}-a`).focus();
    } else if (act === 'delalias') {
      P.aliases = P.aliases.filter((a) => a.k !== +b.dataset.ak);
      renderPricing();
    } else if (act === 'tpl') {
      const m = (P.data.models || []).find((x) => x.id === b.dataset.id);
      if (!m) return;
      const r = rowFrom(Object.assign({}, m, { id: m.id }), true);
      P.rows.push(r);
      renderPricing();
      const inp = $(`#pr-${r.k}-id`);
      if (inp) { inp.scrollIntoView({ block: 'center' }); inp.focus(); }
    }
  });

  function buildPayload() {
    const errs = [];
    const toNum = (s) => { const t = String(s).trim().replace(',', '.'); return t === '' ? null : Number(t); };
    const seen = new Set();
    const models = [];
    P.rows.forEach((r, i) => {
      const id = r.id.trim();
      const tag = T('pricing.err_row_tag', { n: i + 1 }) + (id ? ' (' + id + ')' : '');
      if (!id) { errs.push(T('pricing.err_missing_id', { tag })); return; }
      if (seen.has(id)) { errs.push(T('pricing.err_dup_id', { tag })); return; }
      seen.add(id);
      const v = { inP: toNum(r.inP), outP: toNum(r.outP), cr: toNum(r.cr), cw5: toNum(r.cw5), cw1: toNum(r.cw1), fast: toNum(r.fast) };
      for (const [k, label] of FIELDS) {
        const x = v[k];
        if (x != null && (!isFinite(x) || x < 0)) errs.push(T('pricing.err_non_negative', { tag, label: T(label) }));
      }
      if (v.inP == null || v.outP == null) errs.push(T('pricing.err_in_out', { tag }));
      if (!r.derived && (v.cr == null || v.cw5 == null || v.cw1 == null)) errs.push(T('pricing.err_cache', { tag }));
      if (v.fast != null && v.fast <= 0) errs.push(T('pricing.err_fast', { tag }));
      const base = v.inP || 0;
      models.push({
        id, match: r.match, inPerM: v.inP, outPerM: v.outP,
        cacheReadPerM: r.derived ? base * 0.1 : v.cr, cacheWrite5mPerM: r.derived ? base * 1.25 : v.cw5, cacheWrite1hPerM: r.derived ? base * 2 : v.cw1,
        longContext: r.lc || null, fastMultiplier: v.fast, source: 'override', derived: !!r.derived,
      });
    });
    const aliases = {};
    for (const a of P.aliases) {
      const k = a.alias.trim(), t = a.target.trim();
      if (!k && !t) continue;
      if (!k || !t) { errs.push(T('pricing.err_alias')); continue; }
      aliases[k] = t;
    }
    return { errs, body: { models, aliases } };
  }

  function showPrErr(msg) {
    const e = $('#prErr');
    e.textContent = msg;
    e.hidden = false;
    e.scrollIntoView({ block: 'nearest' });
  }

  $('#prForm').addEventListener('submit', async (e) => {
    e.preventDefault();
    if (P.loading || P.loadErr) return;
    $('#prErr').hidden = true;
    const { errs, body } = buildPayload();
    if (errs.length) { showPrErr(errs.slice(0, 4).join(' ')); return; }
    const btn = $('#prSave');
    btn.disabled = true;
    try {
      P.data = await api('/api/pricing', { method: 'PUT', body, quiet: true });
      dlg.close();
      toast(T('toast.pricing_saved'));
      loadUsage();
    } catch (err) {
      showPrErr(err.detail || err.message);   // the server's {"error": "..."} text
    } finally { btn.disabled = false; }
  });
  $('#prCancel').addEventListener('click', () => dlg.close());
  dlg.addEventListener('click', (e) => { if (e.target === dlg) dlg.close(); });   // click on the backdrop

  let customTimer = 0;
  $('#uRange').addEventListener('change', (e) => {
    U.range = e.target.value;
    store.set('cm-u-range', U.range);
    if (U.range === 'custom' && !U.from) { const r = resolveRange(); U.from = r.from; U.to = r.to; }
    syncToolbar();
    loadUsage();
  });
  for (const [id, key] of [['#uFrom', 'from'], ['#uTo', 'to']]) {
    $(id).addEventListener('change', (e) => {
      U[key] = e.target.value;
      store.set('cm-u-' + key, U[key]);
      clearTimeout(customTimer);
      customTimer = setTimeout(loadUsage, 200);
    });
  }
  $('#uMode').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-m]');
    if (!b || b.dataset.m === U.mode) return;
    U.mode = b.dataset.m;
    store.set('cm-u-mode', U.mode);
    loadUsage();
  });
  $('#uMetric').addEventListener('change', (e) => {
    U.metric = e.target.value;
    store.set('cm-u-metric', U.metric);
    tips = [];
    renderChart();
  });
  $('#uPricingBtn').addEventListener('click', () => openPricing());
  $('#tab-usage').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-act]');
    if (!b) return;
    if (b.dataset.act === 'pricing-unpriced') openPricing(U.data ? U.data.unpricedModels : []);
    else if (b.dataset.act === 'retry') loadUsage();
    else if (b.dataset.act === 'blocks-more') { U.showAllBlocks = !U.showAllBlocks; renderBlocks(); }
  });
  bindTips($('#tab-usage'));

  let resizeTimer = 0;
  let lastW = 0;
  window.addEventListener('resize', () => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      const w = $('#tab-usage').clientWidth;
      if (!isUsageTab() || !w || w === lastW) return;
      lastW = w;
      tips = [];
      renderChart(); renderActive(); renderHeat();
    }, 150);
  });

  // the active block moves on while the page is open: refresh it every minute (only while visible)
  setInterval(() => { if (isUsageTab() && !document.hidden) loadBlocks(); }, 60000);

  function onShow() {
    lastW = $('#tab-usage').clientWidth;
    if (!U.data || Date.now() - U.loadedAt > 30000) loadUsage(); else { syncToolbar(); renderAll(); }
  }
  document.addEventListener('cm:tab', (e) => { if (e.detail === 'usage') onShow(); });
  CM.usageInfo = () => U.data;
  syncToolbar();
  if (isUsageTab()) onShow();
})();
