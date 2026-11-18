'use strict';
/* claude-manager UI - full-text search palette (Ctrl/Cmd+K) on GET /api/search.
   Hidden entirely when the backend has no /api/search. */
(() => {
  const CM = window.CM;
  const { $, esc, api, T } = CM;
  const dlg = $('#searchDlg'), qi = $('#sQ'), list = $('#sList'), stat = $('#sStat');
  const btn = $('#searchBtn');
  const PAGE = 30;
  let available = null;          // null = unknown (probe pending), true / false
  let seq = 0, timer = 0, pollTimer = 0;
  let hits = [], total = 0, active = -1, lastQ = '';

  const baseName = (p) => String(p || '').replace(/[\\/]+$/, '').split(/[\\/]/).pop() || '';

  /** Snippet with <mark> built from [start,end) char offsets - DOM nodes only, no innerHTML of untrusted text. */
  function snippetNode(text, matches) {
    const out = document.createElement('div');
    out.className = 's-snip';
    const cps = Array.from(String(text || ''));
    const ms = (Array.isArray(matches) ? matches : []).filter((m) => Array.isArray(m) && m[0] >= 0 && m[1] > m[0]).sort((a, b) => a[0] - b[0]);
    let pos = 0;
    for (const [s, e] of ms) {
      if (s < pos) continue;
      if (s > pos) out.append(document.createTextNode(cps.slice(pos, s).join('')));
      const mk = document.createElement('mark');
      mk.textContent = cps.slice(s, Math.min(e, cps.length)).join('');
      out.append(mk);
      pos = Math.min(e, cps.length);
    }
    if (pos < cps.length) out.append(document.createTextNode(cps.slice(pos).join('')));
    return out;
  }

  function hitNode(h, i) {
    const li = document.createElement('li');
    li.id = 'sr-' + i; li.setAttribute('role', 'option'); li.dataset.i = i; li.setAttribute('aria-selected', 'false');
    const top = document.createElement('div'); top.className = 's-top';
    const t = document.createElement('span'); t.className = 's-title'; t.textContent = h.title || '(empty session)';
    top.append(t);
    const role = document.createElement('span'); role.className = 'badge'; role.textContent = h.role === 'user' ? T('search.role_user') : h.role === 'assistant' ? T('search.role_assistant') : (h.role || '');
    top.append(role);
    if (h.status && h.status !== 'active') { const b = document.createElement('span'); b.className = 'badge ' + (h.status === 'archived' ? 'archived' : 'trashed'); b.textContent = h.status === 'archived' ? T('search.status_archived') : T('search.status_trashed'); top.append(b); }
    li.append(top);
    li.append(snippetNode(h.snippet, h.matches));
    const sub = document.createElement('div'); sub.className = 's-sub muted small';
    const when = h.ts ? new Date(h.ts) : null;
    sub.textContent = [h.source, baseName(h.cwd), when && !isNaN(when) ? when.toLocaleString(I18N.locale) : ''].filter(Boolean).join(' · ');
    li.append(sub);
    return li;
  }

  function setActive(i, scroll = true) {
    const items = list.querySelectorAll('li[role=option]');
    if (!items.length) { active = -1; qi.removeAttribute('aria-activedescendant'); return; }
    active = Math.max(0, Math.min(items.length - 1, i));
    items.forEach((li, k) => li.setAttribute('aria-selected', String(k === active)));
    qi.setAttribute('aria-activedescendant', items[active].id);
    if (scroll) items[active].scrollIntoView({ block: 'nearest' });
  }

  function message(text) {
    list.replaceChildren();
    const li = document.createElement('li'); li.className = 's-msg'; li.textContent = text;
    list.append(li);
  }

  function params(offset) {
    const p = new URLSearchParams({ q: qi.value.trim(), limit: String(PAGE), offset: String(offset) });
    if ($('#sSource').value) p.set('source', $('#sSource').value);
    if ($('#sRole').value) p.set('role', $('#sRole').value);
    if ($('#sFrom').value) p.set('from', $('#sFrom').value);
    if ($('#sTo').value) p.set('to', $('#sTo').value);
    return p.toString();
  }

  function renderStat(res) {
    const idx = res && res.index;
    let t = qi.value.trim() ? T('search.stat_results', { n: total }) : '';
    if (idx && idx.running) t += (t ? ' · ' : '') + T('search.stat_indexing', { done: idx.filesDone || 0, total: idx.filesTotal || 0 });
    stat.textContent = t;
    clearTimeout(pollTimer);
    if (idx && idx.running && dlg.open) pollTimer = setTimeout(() => run(false), 3000);
  }

  async function run(more) {
    const q = qi.value.trim();
    const my = ++seq;
    if (!q) { hits = []; total = 0; lastQ = ''; stat.textContent = ''; message(T('search.hint')); return; }
    if (!more && q !== lastQ) message(T('search.searching'));
    let res;
    try { res = await api('/api/search?' + params(more ? hits.length : 0), { quiet: true }); } catch (e) {
      if (my !== seq) return;
      if (e.status === 404 || e.status === 405) { setAvailable(false); dlg.close(); return; }
      message(T('search.error', { message: e.message })); return;
    }
    if (my !== seq) return;
    const got = Array.isArray(res && res.hits) ? res.hits : [];
    total = Number(res && res.total) || got.length;
    const prevActive = active, sameQ = q === lastQ;
    hits = more ? hits.concat(got) : got;
    lastQ = q;
    list.replaceChildren();
    if (!hits.length) message(T('search.no_results', { q }));
    else {
      hits.forEach((h, i) => list.append(hitNode(h, i)));
      if (hits.length < total) {
        const li = document.createElement('li'); li.className = 's-more'; li.setAttribute('role', 'presentation');
        const b = document.createElement('button'); b.type = 'button'; b.textContent = T('search.more', { shown: hits.length, total }); b.dataset.more = '1';
        li.append(b); list.append(li);
      }
      setActive(more || sameQ ? Math.max(prevActive, 0) : 0, false);
    }
    renderStat(res);
  }

  function openHit(i) {
    const h = hits[i];
    if (!h) return;
    const q = qi.value.trim();
    dlg.close();
    CM.openSession(h.id, q.length >= 2 ? { find: q } : {});
  }

  function setAvailable(v) {
    available = v;
    btn.hidden = !v;
  }

  function fillSources() {
    const el = $('#sSource'), v = el.value;
    const srcs = CM.getData().sources || [];
    el.innerHTML = `<option value="">${esc(T('search.filter_source_all'))}</option>` + srcs.map((x) => `<option value="${esc(x)}">${esc(x)}</option>`).join('');
    if (srcs.includes(v)) el.value = v;
  }

  async function openPalette() {
    if (available === null) await probe();
    if (!available) return;
    if (dlg.open) { qi.focus(); qi.select(); return; }
    fillSources();
    dlg.showModal();
    qi.focus(); qi.select();
    if (qi.value.trim()) run(false); else { message(T('search.hint')); stat.textContent = ''; }
  }

  let probing = null;
  function probe() {
    if (probing) return probing;
    probing = api('/api/search?q=cm&limit=1', { quiet: true }).then(() => setAvailable(true), (e) => setAvailable(!(e.status === 404 || e.status === 405)));
    return probing;
  }

  qi.addEventListener('input', () => { clearTimeout(timer); timer = setTimeout(() => run(false), 200); });
  for (const id of ['#sSource', '#sRole', '#sFrom', '#sTo']) $(id).addEventListener('change', () => run(false));
  qi.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); setActive(active + 1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); setActive(active - 1); }
    else if (e.key === 'Enter') { e.preventDefault(); clearTimeout(timer); if (active >= 0 && hits.length && qi.value.trim() === lastQ) openHit(active); else run(false); }
  });
  list.addEventListener('click', (e) => {
    if (e.target.closest('[data-more]')) { run(true); return; }
    const li = e.target.closest('li[data-i]');
    if (li) openHit(Number(li.dataset.i));
  });
  list.addEventListener('mousemove', (e) => { const li = e.target.closest('li[data-i]'); if (li && Number(li.dataset.i) !== active) setActive(Number(li.dataset.i), false); });
  $('#sClose').addEventListener('click', () => dlg.close());
  dlg.addEventListener('close', () => { clearTimeout(pollTimer); clearTimeout(timer); seq++; });
  dlg.addEventListener('click', (e) => { if (e.target === dlg) dlg.close(); });   // click on backdrop
  btn.addEventListener('click', openPalette);
  document.addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && (e.key === 'k' || e.key === 'K')) {
      if (available === false) return;
      if (document.querySelector('dialog[open]') && !dlg.open) return;
      e.preventDefault();
      if (dlg.open) dlg.close(); else openPalette();
    }
  });

  probe();
})();
