'use strict';
/* claude-manager UI - session Viewer (drawer body). Talks to GET /api/session/{id}/turns (+ /export).
   open() resolves false when the endpoint does not exist (older backend): app.js then falls back to /api/messages. */
(() => {
  const CM = window.CM;
  const { $, esc, fmt, api, T } = CM;
  const PAGE = 30; // 100 turns of a huge session can be ~10 MB; 30 keeps pages light
  const MAX_FIND_TURNS = 3000;
  const KIND_ICON = { shell: '>_', file_read: 'R', file_write: 'W', file_edit: '±', search: '?', web: '@', subagent: '⇉', skill: '✦', mcp: '⚙', todo: '☑', generic: '•' };
  const META_KINDS = ['compact_boundary', 'api_error', 'turn_duration', 'stop_hook', 'local_command', 'interrupted', 'bash'];

  const V = { id: '', agent: '', next: 0, total: 0, loading: false, seq: 0, subagents: [], active: false, promptsOnly: false, find: { q: '', marks: [], idx: -1 }, io: null };
  const toolData = new WeakMap();

  const el = {
    msgs: () => $('#msgs'), bar: () => $('#vbar'), list: () => $('#msgs .v-list'),
  };

  const fmtDur = (ms) => {
    if (ms == null || isNaN(ms)) return '';
    if (ms < 1000) return Math.round(ms) + ' ms';
    const s = ms / 1000;
    if (s < 60) return s.toFixed(s < 10 ? 1 : 0).replace('.', I18N.lang === 'vi' ? ',' : '.') + ' s';
    const m = Math.floor(s / 60);
    return T('viewer.dur_min_sec', { m, s: Math.round(s - m * 60) });
  };
  const fmtTime = (ts) => { const d = ts ? new Date(ts) : null; return d && !isNaN(d) ? d.toLocaleTimeString(I18N.locale, { hour: '2-digit', minute: '2-digit', second: '2-digit' }) : ''; };
  const asStr = (v) => (v == null ? '' : typeof v === 'string' ? v : JSON.stringify(v));
  const pretty = (v) => { try { return JSON.stringify(v, null, 2); } catch { return String(v); } };

  /** Minimal safe formatting: ``` fences and `inline code`; everything else is escaped text (pre-wrap by CSS). */
  function rich(text) {
    const t = String(text ?? '');
    let out = '', last = 0, m;
    const re = /```([^\n`]*)\n([\s\S]*?)(?:```|$)/g;
    const inline = (s) => esc(s).replace(/`([^`\n]+)`/g, '<code>$1</code>').replace(/\*\*([^*\n]+)\*\*/g, '<b>$1</b>');
    while ((m = re.exec(t))) {
      out += inline(t.slice(last, m.index));
      out += `<pre class="v-pre code">${m[1].trim() ? `<span class="lang">${esc(m[1].trim())}</span>` : ''}${esc(m[2].replace(/\n$/, ''))}</pre>`;
      last = re.lastIndex;
      if (m[0].length === 0) re.lastIndex++;
    }
    return out + inline(t.slice(last));
  }

  const pre = (text, cls = '') => `<pre class="v-pre ${cls}">${esc(text)}</pre>`;
  const lbl = (t) => `<div class="v-lbl">${esc(t)}</div>`;

  function resultHtml(b) {
    const r = b.result;
    if (!r) return `<div class="v-note warn">${esc(T('viewer.no_result'))}</div>`;
    const flags = (r.isError ? T('viewer.flag_error') : '') + (r.truncated ? T('viewer.flag_truncated') : '');
    return `<div class="v-res${r.isError ? ' err' : ''}">${lbl(T('viewer.result_label') + flags)}${pre(r.text || T('viewer.empty_text'), r.isError ? 'errtx' : '')}</div>`;
  }

  function diffHtml(d) {
    const CAP = 600;
    let rows = '', shown = 0, add = 0, del = 0, cut = false;
    for (const h of d.hunks || []) {
      if (shown >= CAP) { cut = true; break; }
      rows += `<div class="dl hk">@@ -${esc(h.oldStart)},${esc(h.oldLines)} +${esc(h.newStart)},${esc(h.newLines)} @@</div>`;
      let o = Number(h.oldStart) || 1, n = Number(h.newStart) || 1;
      for (const line of h.lines || []) {
        const c = line[0];
        let cls = 'ctx', ln;
        if (c === '+') { cls = 'add'; ln = n++; add++; } else if (c === '-') { cls = 'del'; ln = o++; del++; } else { ln = n; o++; n++; }
        if (shown < CAP) rows += `<div class="dl ${cls}"><span class="ln">${ln}</span><span class="lt">${esc(line)}</span></div>`;
        shown++;
      }
    }
    if (shown > CAP) cut = true;
    return `<div class="v-diff">${lbl(`${d.path || ''}  +${add} −${del}`)}<div class="dbody">${rows}</div>${cut ? `<div class="v-note">${esc(T('viewer.diff_truncated', { n: CAP }))}</div>` : ''}</div>`;
  }

  function kvHtml(inp) {
    const keys = Object.keys(inp || {});
    if (!keys.length) return '';
    return `<dl class="v-kv">${keys.slice(0, 12).map((k) => {
      const v = asStr(inp[k]);
      return `<dt>${esc(k)}</dt><dd>${v.length > 400 ? pre(v.slice(0, 400) + '…') : esc(v)}</dd>`;
    }).join('')}</dl>`;
  }

  function visualHtml(b) {
    const inp = b.input && typeof b.input === 'object' ? b.input : {};
    const path = inp.file_path || inp.path || inp.notebook_path || '';
    let h = '';
    switch (b.kind) {
      case 'shell': {
        const cmd = inp.command ?? inp.cmd;
        h += cmd != null ? lbl(T('viewer.lbl_command')) + pre('$ ' + asStr(cmd), 'cmd') : kvHtml(inp);
        return h + resultHtml(b);
      }
      case 'file_edit': case 'file_write':
        if (b.diff) return diffHtml(b.diff) + resultHtml(b);
        return (path ? lbl(T('viewer.lbl_file', { path })) : '') + kvHtml(inp) + resultHtml(b);
      case 'file_read':
        return (path ? lbl(T('viewer.lbl_file', { path }) + (inp.offset ? T('viewer.lbl_from_line', { n: inp.offset }) : '') + (inp.limit ? T('viewer.lbl_lines', { n: inp.limit }) : '')) : kvHtml(inp)) + resultHtml(b);
      case 'search':
        return kvHtml(inp) + resultHtml(b);
      case 'web':
        return kvHtml(inp) + resultHtml(b);
      case 'todo':
        if (Array.isArray(inp.todos)) {
          return `<ul class="v-todo">${inp.todos.map((t) => {
            const st = t && t.status; const tx = t && (t.content || t.activeForm || t.subject || '');
            return `<li class="${esc(st)}"><span>${st === 'completed' ? '☑' : st === 'in_progress' ? '◐' : '☐'}</span> ${esc(tx)}</li>`;
          }).join('')}</ul>` + resultHtml(b);
        }
        return kvHtml(inp) + resultHtml(b);
      case 'subagent': {
        let s = '';
        if (inp.description) s += lbl(T('viewer.lbl_description')) + `<div class="v-p">${esc(inp.description)}</div>`;
        if (inp.prompt) s += lbl(T('viewer.lbl_subagent_prompt')) + pre(asStr(inp.prompt));
        if (!s) s = kvHtml(inp);
        if (b.agentId) s += `<div class="v-act"><button type="button" data-agent="${esc(b.agentId)}">${esc(T('viewer.open_subagent'))}</button></div>`;
        return s + resultHtml(b);
      }
      default:
        return kvHtml(inp) + resultHtml(b);
    }
  }

  function rawHtml(b) {
    let h = lbl(T('viewer.raw_input') + (b.inputTruncated ? T('viewer.raw_truncated') : '')) + pre(pretty(b.input));
    if (b.diff) h += lbl(T('viewer.raw_diff')) + pre(pretty(b.diff).slice(0, 20000));
    h += b.result ? lbl(T('viewer.raw_result') + (b.result.isError ? T('viewer.raw_result_error') : '')) + pre(b.result.text || '') : '<div class="v-note warn">result: null (orphan)</div>';
    return h;
  }

  function toolEl(b) {
    const kind = KIND_ICON[b.kind] ? b.kind : 'generic';
    const d = document.createElement('details');
    d.className = `tool k-${kind}${b.result && b.result.isError ? ' err' : ''}${!b.result ? ' orphan' : ''}`;
    const dur = b.durationMs != null ? `<span class="tdur">${esc(fmtDur(b.durationMs))}</span>` : '';
    const state = !b.result ? `<span class="tbadge warn" title="${esc(T('viewer.tool_orphan_tip'))}">${esc(T('viewer.tool_orphan'))}</span>`
      : b.result.isError ? `<span class="tbadge err">${esc(T('viewer.tool_error'))}</span>` : '';
    d.innerHTML = `<summary><span class="ticon" aria-hidden="true">${esc(KIND_ICON[kind])}</span><span class="tname">${esc(b.name || kind)}</span><span class="tsum">${esc(b.summary || '')}</span>${state}${b.agentId ? '<span class="tbadge sub">subagent</span>' : ''}${dur}</summary><div class="tbody"></div>`;
    toolData.set(d, b);
    return d;
  }

  function fillTool(d) {
    const body = d.querySelector('.tbody');
    if (!body || body.dataset.f) return;
    body.dataset.f = '1';
    const b = toolData.get(d);
    if (!b) return;
    body.innerHTML = `<div class="v-seg" role="group" aria-label="${esc(T('viewer.view_mode'))}"><button type="button" data-vt="v" aria-pressed="true">${esc(T('viewer.view_visual'))}</button><button type="button" data-vt="r" aria-pressed="false">${esc(T('viewer.view_raw'))}</button></div>
      <div class="vt-v">${visualHtml(b)}</div><div class="vt-r" hidden>${rawHtml(b)}</div>`;
  }

  function metaRow(b) {
    const label = META_KINDS.includes(b.subtype) ? T('viewer.meta_' + b.subtype) : (b.subtype || T('viewer.meta_default'));
    let extra = '';
    if (b.subtype === 'turn_duration' && b.data && b.data.durationMs != null) extra = fmtDur(b.data.durationMs);
    const text = b.text || extra;
    const cls = b.subtype === 'api_error' ? ' err' : b.subtype === 'compact_boundary' ? ' compact' : '';
    return `<div class="v-meta${cls}"><b>${esc(label)}</b>${text ? `<span>${esc(text)}</span>` : ''}</div>`;
  }

  function blockHtml(b, kind) {
    switch (b && b.type) {
      case 'text': {
        const long = kind === 'prompt' && (b.text || '').length > 1200;
        return `<div class="v-text${long ? ' clamp' : ''}">${rich(b.text)}</div>${long ? '<button type="button" class="v-more" data-more="1">' + esc(T('viewer.show_more')) + '</button>' : ''}`;
      }
      case 'thinking':
        return `<details class="think"><summary>${esc(T('viewer.thinking', { n: fmt((b.text || '').length) }))}</summary><div class="v-text">${esc(b.text)}</div></details>`;
      case 'meta': return metaRow(b);
      case 'parseError':
        return `<div class="v-perr"><b>${esc(T('viewer.parse_error', { line: b.line }))}</b> <span>${esc(b.error)}</span>${b.raw ? pre(b.raw) : ''}</div>`;
      case 'tool': return '<div class="tool-slot"></div>';
      default: return `<div class="v-meta"><b>${esc(b && b.type || 'block')}</b><span>${esc(T('viewer.unsupported_block'))}</span></div>`;
    }
  }

  function turnEl(t) {
    const kind = ['prompt', 'reply', 'command', 'meta', 'error'].includes(t.kind) ? t.kind : 'reply';
    const div = document.createElement('article');
    div.className = `turn k-${kind}`;
    div.dataset.idx = t.idx;
    const who = kind === 'prompt' ? T('viewer.who_you') : kind === 'command' ? T('viewer.who_command') : kind === 'meta' ? '' : kind === 'error' ? T('viewer.who_error') : T('viewer.who_claude');
    const blocks = Array.isArray(t.blocks) ? t.blocks : [];
    const head = who ? `<div class="thd"><span class="who">${who}</span>${t.model ? `<span class="tmodel">${esc(t.model)}</span>` : ''}<span class="sp"></span><span class="tts">${esc(fmtTime(t.ts))}</span><span class="tidx">#${esc(t.idx)}</span></div>` : '';
    let body = '';
    if (kind === 'command') {
      const txt = blocks.filter((b) => b.type === 'text').map((b) => b.text).join('\n');
      body = `<div class="v-cmd"><code>${esc(txt)}</code></div>` + blocks.filter((b) => b.type !== 'text').map((b) => blockHtml(b, kind)).join('');
    } else body = blocks.map((b) => blockHtml(b, kind)).join('');
    const u = t.usage;
    const foot = [];
    if (u) foot.push(T('viewer.usage_foot', { in: fmt(u.in || 0), out: fmt(u.out || 0) }) + ((u.cc || u.cr) ? T('viewer.usage_foot_cache', { cc: fmt(u.cc || 0), cr: fmt(u.cr || 0) }) : ''));
    if (t.durationMs != null) foot.push(fmtDur(t.durationMs));
    div.innerHTML = head + `<div class="tbodyw">${body}</div>` + (foot.length ? `<div class="tfoot">${esc(foot.join('  ·  '))}</div>` : '');
    const slots = div.querySelectorAll('.tool-slot');
    let si = 0;
    for (const b of blocks) {
      if (b && b.type === 'tool') { const s = slots[si++]; if (s) s.replaceWith(toolEl(b)); }
    }
    return div;
  }

  const turnsUrl = (offset) => `/api/session/${encodeURIComponent(V.id)}/turns?offset=${offset}&limit=${PAGE}` + (V.agent ? '&agent=' + encodeURIComponent(V.agent) : '');

  function updateInfo() {
    const subs = V.subagents.length;
    $('#vInfo').textContent = T('viewer.info_turns', { n: V.total }) + (subs ? T('viewer.info_subagents', { n: subs }) : '') + (V.next < V.total ? T('viewer.info_loaded', { n: V.next }) : '');
    const crumb = $('#vCrumb');
    if (V.agent) {
      const sa = V.subagents.find((x) => x.agentId === V.agent) || V.mainSubs && V.mainSubs.find((x) => x.agentId === V.agent);
      crumb.hidden = false;
      crumb.innerHTML = `<button type="button" data-back="1">${esc(T('viewer.crumb_back'))}</button> <span>${esc(T('viewer.crumb_viewing'))}${sa ? ': ' + esc(sa.type || '') + ' - ' + esc(sa.description || '') : ' ' + esc(V.agent)}</span>`;
    } else { crumb.hidden = true; crumb.innerHTML = ''; }
    const ex = $('#vExport');
    ex.hidden = !!V.agent;
    if (!V.agent) {
      const base = `/api/session/${encodeURIComponent(V.id)}/export?format=`;
      ex.innerHTML = `${esc(T('viewer.export'))} <a href="${base}md" download>Markdown</a> · <a href="${base}json" download>JSON</a>`;
    }
  }

  function sentinel() { return $('#msgs .v-sentinel'); }
  function setSentinel(text) {
    const s = sentinel();
    if (!s) return;
    s.textContent = text;
    s.hidden = !text;
  }

  async function loadMore() {
    if (!V.active || V.loading || V.next >= V.total && V.total > 0) return;
    const my = V.seq;
    V.loading = true;
    setSentinel(T('viewer.loading_more'));
    try {
      const res = await api(turnsUrl(V.next), { quiet: true });
      if (my !== V.seq) return;
      appendTurns(res);
    } catch (e) {
      if (my !== V.seq) return;
      setSentinel(T('viewer.load_error', { message: e.message }));
    } finally { if (my === V.seq) V.loading = false; }
    if (my === V.seq) {
      setSentinel(V.next < V.total ? T('viewer.scroll_more') : (V.total ? T('viewer.the_end') : ''));
      if (V.find.q) runFind(true);
    }
  }

  function appendTurns(res) {
    const turns = Array.isArray(res && res.turns) ? res.turns : [];
    if (res && typeof res.total === 'number') V.total = res.total;
    if (res && Array.isArray(res.subagents)) { V.subagents = res.subagents; if (!V.agent) V.mainSubs = res.subagents; }
    const list = el.list();
    const frag = document.createDocumentFragment();
    for (const t of turns) frag.appendChild(turnEl(t));
    list.appendChild(frag);
    V.next += turns.length;
    if (!turns.length) V.total = V.next;   // defensive: avoid infinite loop
    updateInfo();
  }

  async function open(id, opts = {}) {
    const my = ++V.seq;
    V.id = id; V.agent = opts.agent || ''; V.next = 0; V.total = 0; V.loading = true; V.subagents = []; V.active = false;
    if (!opts.agent) V.mainSubs = null;
    V.find = { q: '', marks: [], idx: -1 };
    closeFind(true);
    $('#vToggleTools').textContent = T('viewer.expand_tools');
    const msgs = el.msgs();
    msgs.scrollTop = 0;
    msgs.innerHTML = `<div class="empty">${esc(T('common.loading'))}</div>`;
    let res;
    try {
      res = await api(turnsUrl(0), { quiet: true });
    } catch (e) {
      if (my !== V.seq) return true;
      V.loading = false;
      if (e.status === 404) {
        if (opts.agent) { CM.toast(T('toast.viewer_subagent_missing'), 'err'); return open(id); }
        return false;                                // endpoint missing -> legacy drawer
      }
      msgs.innerHTML = `<div class="empty">${esc(T('common.error_prefix', { message: e.message }))}</div>`;
      CM.toast(T('toast.sessions_load_failed', { message: e.message }), 'err');
      return true;
    }
    if (my !== V.seq) return true;
    V.active = true; V.loading = false;
    msgs.className = 'viewer' + (V.promptsOnly ? ' po' : '');
    msgs.innerHTML = '<div class="v-list"></div><div class="v-sentinel" hidden></div>';
    $('#vbar').hidden = false;
    appendTurns(res);
    if (!V.total) { msgs.querySelector('.v-list').innerHTML = `<div class="empty">${esc(T('common.no_content'))}</div>`; }
    setSentinel(V.next < V.total ? T('viewer.scroll_more') : (V.total ? T('viewer.the_end') : ''));
    watchSentinel();
    if (opts.find) { $('#findBar').hidden = false; $('#findQ').value = opts.find; runFind(false); }
    return true;
  }

  function watchSentinel() {
    if (V.io) V.io.disconnect();
    const s = sentinel();
    if (!s || !('IntersectionObserver' in window)) return;
    V.io = new IntersectionObserver((ents) => { if (ents.some((e) => e.isIntersecting)) loadMore(); }, { root: el.msgs(), rootMargin: '500px' });
    V.io.observe(s);
  }

  function close() {
    V.seq++; V.active = false; V.id = ''; V.agent = '';
    if (V.io) { V.io.disconnect(); V.io = null; }
    closeFind(true);
    const b = $('#vbar'); if (b) b.hidden = true;
  }

  /** Tail refresh after the backend reported a change: re-fetch the last loaded turn and anything after it. */
  async function refresh() {
    if (!V.active || V.loading) return;
    const my = V.seq;
    V.loading = true;
    try {
      const allLoaded = V.next >= V.total;
      const from = Math.max(0, V.next - 1);
      const res = await api(turnsUrl(from), { quiet: true });
      if (my !== V.seq) return;
      const total = typeof res.total === 'number' ? res.total : V.total;
      if (total < V.next) { V.loading = false; await open(V.id, { agent: V.agent }); return; }
      V.total = total;
      if (res.subagents) V.subagents = res.subagents;
      if (allLoaded) {
        const turns = Array.isArray(res.turns) ? res.turns.slice() : [];
        const list = el.list();
        if (list && turns.length && turns[0].idx === from) {
          const old = list.querySelector('.turn[data-idx="' + from + '"]');
          const n = turnEl(turns.shift());
          if (old && old.innerHTML !== n.innerHTML) old.replaceWith(n);
        }
        if (list && turns.length) {
          const frag = document.createDocumentFragment();
          for (const t of turns) frag.appendChild(turnEl(t));
          list.appendChild(frag);
          V.next += turns.length;
        }
      }
      updateInfo();
      setSentinel(V.next < V.total ? T('viewer.scroll_more') : T('viewer.the_end'));
      if (V.next < V.total) watchSentinel();
    } catch { /* transient: next poll retries */ } finally { if (my === V.seq) V.loading = false; }
  }

  async function jumpPrompt(dir) {
    const box = el.msgs();
    const top = box.getBoundingClientRect().top;
    const find = () => {
      const cards = Array.from(box.querySelectorAll('.turn.k-prompt'));
      if (dir > 0) return cards.find((c) => c.getBoundingClientRect().top > top + 6);
      for (let i = cards.length - 1; i >= 0; i--) if (cards[i].getBoundingClientRect().top < top - 6) return cards[i];
      return null;
    };
    let c = find();
    while (!c && dir > 0 && V.next < V.total && !V.loading) { await loadMore(); c = find(); }
    if (c) { box.scrollTop += c.getBoundingClientRect().top - top - 4; c.classList.add('flash'); setTimeout(() => c.classList.remove('flash'), 900); }
  }

  function clearMarks() {
    const root = el.msgs();
    root.querySelectorAll('mark.fm').forEach((m) => { const p = m.parentNode; p.replaceChild(document.createTextNode(m.textContent), m); p.normalize(); });
    V.find.marks = []; V.find.idx = -1;
  }
  function markAll(q) {
    const root = el.list();
    if (!root) return;
    const ql = q.toLowerCase();
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    const nodes = [];
    for (let n = walker.nextNode(); n; n = walker.nextNode()) nodes.push(n);
    for (const n of nodes) {
      if (V.find.marks.length >= 1000) break;
      const txt = n.nodeValue, low = txt.toLowerCase();
      let i = low.indexOf(ql);
      if (i < 0) continue;
      const frag = document.createDocumentFragment();
      let last = 0;
      while (i >= 0 && V.find.marks.length < 1000) {
        frag.appendChild(document.createTextNode(txt.slice(last, i)));
        const m = document.createElement('mark'); m.className = 'fm'; m.textContent = txt.slice(i, i + q.length);
        frag.appendChild(m); V.find.marks.push(m);
        last = i + q.length;
        i = low.indexOf(ql, last);
      }
      frag.appendChild(document.createTextNode(txt.slice(last)));
      n.parentNode.replaceChild(frag, n);
    }
  }
  function goMark(i) {
    const ms = V.find.marks;
    if (!ms.length) { $('#findCount').textContent = V.find.q ? '0/0' : ''; return; }
    if (V.find.idx >= 0 && ms[V.find.idx]) ms[V.find.idx].classList.remove('cur');
    V.find.idx = (i + ms.length) % ms.length;
    const m = ms[V.find.idx];
    m.classList.add('cur');
    for (let d = m.closest('details'); d; d = d.parentElement && d.parentElement.closest('details')) d.open = true;
    m.scrollIntoView({ block: 'center' });
    $('#findCount').textContent = `${V.find.idx + 1}/${ms.length}${ms.length >= 1000 ? '+' : ''}`;
  }
  let findSeq = 0;
  async function runFind(keepPos) {
    const q = $('#findQ').value;
    const my = ++findSeq;
    const prev = V.find.idx;
    clearMarks();
    V.find.q = q;
    if (!q) { $('#findCount').textContent = ''; return; }
    // search needs everything in the DOM: load the remaining pages (bounded) and render lazy tool bodies
    if (!keepPos && V.next < V.total) {
      $('#findCount').textContent = T('viewer.finding_loading');
      while (V.next < V.total && V.next < MAX_FIND_TURNS && my === findSeq && V.active) {
        if (V.loading) { await new Promise((r) => setTimeout(r, 60)); continue; }
        await loadMore();
      }
      if (my !== findSeq) return;
      clearMarks();
    }
    el.list().querySelectorAll('details.tool').forEach(fillTool);
    markAll(q);
    goMark(keepPos && prev >= 0 ? prev : 0);
  }
  function openFind() {
    if (!V.active) return;
    $('#findBar').hidden = false;
    const i = $('#findQ'); i.focus(); i.select();
  }
  function closeFind(silent) {
    const fb = $('#findBar');
    if (!fb) return;
    fb.hidden = true; findSeq++;
    if (!silent || V.find.marks.length) { try { clearMarks(); } catch { /* ignore */ } }
    $('#findCount').textContent = ''; V.find.q = '';
  }

  const msgsEl = $('#msgs');
  msgsEl.addEventListener('toggle', (e) => { if (e.target.matches && e.target.matches('details.tool') && e.target.open) fillTool(e.target); }, true);
  msgsEl.addEventListener('click', (e) => {
    const t = e.target;
    const vt = t.closest && t.closest('[data-vt]');
    if (vt) {
      const body = vt.closest('.tbody');
      const raw = vt.dataset.vt === 'r';
      body.querySelector('.vt-v').hidden = raw; body.querySelector('.vt-r').hidden = !raw;
      body.querySelectorAll('[data-vt]').forEach((b) => b.setAttribute('aria-pressed', String(b === vt)));
      return;
    }
    const ag = t.closest && t.closest('[data-agent]');
    if (ag) { open(V.id, { agent: ag.dataset.agent }); return; }
    const more = t.closest && t.closest('[data-more]');
    if (more) { const x = more.previousElementSibling; const on = x.classList.toggle('clamp'); more.textContent = on ? T('viewer.show_more') : T('viewer.show_less'); }
  });
  $('#vbar').addEventListener('click', (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.back) { open(V.id); return; }
    if (b.id === 'vToggleTools') {
      const list = el.list(); if (!list) return;
      const ds = Array.from(list.querySelectorAll('details.tool'));
      const open = !ds.every((d) => d.open);
      ds.slice(0, 400).forEach((d) => { d.open = open; });
      b.textContent = open ? T('viewer.collapse_tools') : T('viewer.expand_tools');
    } else if (b.id === 'vFind') openFind();
  });
  $('#vPrompts').addEventListener('change', (e) => {
    V.promptsOnly = e.target.checked;
    el.msgs().classList.toggle('po', V.promptsOnly);
  });
  let findTimer = 0;
  $('#findQ').addEventListener('input', () => { clearTimeout(findTimer); findTimer = setTimeout(() => runFind(false), 220); });
  $('#findQ').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); clearTimeout(findTimer); if (V.find.q === $('#findQ').value && V.find.marks.length) goMark(V.find.idx + (e.shiftKey ? -1 : 1)); else runFind(false); }
    else if (e.key === 'Escape') { e.preventDefault(); closeFind(); }
  });
  $('#findPrev').addEventListener('click', () => goMark(V.find.idx - 1));
  $('#findNext').addEventListener('click', () => goMark(V.find.idx + 1));
  $('#findClose').addEventListener('click', () => closeFind());
  document.addEventListener('keydown', (e) => {
    if (!V.active || !$('#drawer').classList.contains('open') || document.querySelector('dialog[open]')) return;
    if (e.altKey && (e.key === 'ArrowDown' || e.key === 'ArrowUp') && !e.ctrlKey && !e.metaKey) { e.preventDefault(); jumpPrompt(e.key === 'ArrowDown' ? 1 : -1); }
    else if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && (e.key === 'f' || e.key === 'F')) { e.preventDefault(); openFind(); }
  });

  CM.viewer = { open, close, refresh, isActive: () => V.active, sessionId: () => V.id };
})();
