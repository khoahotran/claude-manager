'use strict';
/* claude-manager UI - "Kho" tab: inventory (skills / plugins / MCP / hooks / CLAUDE.md) + spec-kit dashboard.
   Data: GET /api/inventory, GET /api/speckit, GET /api/usage?from=2000-01-01 (usage counts). All strings go through esc(). */
(() => {
  const CM = window.CM;
  const { $, $$, esc, api, T } = CM;
  const root = $('#tab-inventory');
  const btn = $('#tabbtn-inventory');
  const I = { inv: null, spec: null, usage: null, err: '', loading: false, q: '', unused: false, shown: false, open: {} };

  const num = (v) => (typeof v === 'number' && isFinite(v) ? v : 0);
  const usd = (n) => (CM.usd ? CM.usd(n) : '$' + num(n).toFixed(2));
  const ymd = (d) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  const STAGES = ['spec', 'plan', 'tasks', 'in-progress', 'done'];
  const stageLabel = (s) => (STAGES.includes(s) ? T('inventory.stage_' + s.replace('-', '_')) : s);
  // the server sends the English text "Over 200 lines" in claudeMd[].warn[]; map it to a key
  const warnLabel = (w) => (w === 'Over 200 lines' ? T('inventory.warn_over_200_lines') : w);

  const others = () => ['#tab-sessions', '#tab-usage', '#tab-workspace'];
  function enter() {
    I.shown = true;
    $$('.tab').forEach((t) => {
      const on = t === btn;
      t.classList.toggle('on', on);
      t.setAttribute('aria-selected', String(on));
      t.tabIndex = on ? 0 : -1;
    });
    others().forEach((s) => { $(s).hidden = true; });
    $('#data-view').hidden = false;
    $('#filters').hidden = true;
    $('#cards').hidden = true;
    document.body.classList.remove('is-ws');
    root.hidden = false;
    load();
  }
  function leave(name) {
    if (!I.shown) return;
    I.shown = false;
    root.hidden = true;
    if (name === 'sessions') {
      $('#tab-sessions').hidden = false;
      $('#filters').hidden = false;
      $('#cards').hidden = false;
    }
  }
  btn.addEventListener('click', enter);
  $$('.tab').forEach((t) => { if (t !== btn) t.addEventListener('click', () => leave(t.dataset.tab)); });
  document.addEventListener('cm:tab', (e) => { if (e.detail !== 'sessions' || !I.shown) leave(e.detail); });
  function openSession(id) {
    leave('sessions');
    CM.openSession(id);
  }

  async function load(force) {
    if (I.loading) return;
    if (!force && I.inv && Date.now() - I.at < 30000) { render(); return; }
    I.loading = true; I.err = '';
    render();
    const today = ymd(new Date());
    const [inv, spec, usage] = await Promise.all([
      api('/api/inventory', { quiet: true }).catch((e) => { I.err = T('inventory.load_failed', { message: e.message }); return null; }),
      api('/api/speckit', { quiet: true }).catch(() => null),
      api('/api/usage?' + new URLSearchParams({ from: '2000-01-01', to: today }), { quiet: true }).catch(() => null),
    ]);
    I.inv = inv; I.spec = spec; I.usage = usage; I.at = Date.now(); I.loading = false;
    render();
  }

  function counts() {
    const u = I.usage || {};
    const toMap = (a) => { const m = new Map(); for (const x of a || []) m.set(String(x.name), num(x.count)); return m; };
    const mcp = new Map();
    for (const x of u.tools || []) {
      const m = /^mcp__(.+?)__/.exec(String(x.name));
      if (m) mcp.set(m[1], (mcp.get(m[1]) || 0) + num(x.count));
    }
    return { skills: toMap(u.skills), subs: toMap(u.subagents), mcp };
  }
  const norm = (s) => String(s).toLowerCase().replace(/[^a-z0-9]+/g, '');
  const skillUses = (c, name) => {
    let n = c.skills.get(name) || 0;
    const short = name.includes(':') ? name.split(':').pop() : name;
    if (!n) n = c.skills.get(short) || 0;
    return n;
  };
  const mcpUses = (c, name) => {
    let n = 0;
    const k = norm(name);
    for (const [s, v] of c.mcp) {
      const ks = norm(s);
      if (ks === k || ks.endsWith(k) || k.endsWith(ks)) n += v;
    }
    return n;
  };
  const pluginUses = (c, p) => {
    let n = mcpUses(c, p.name);
    for (const [s, v] of c.skills) if (s.startsWith(p.name + ':')) n += v;
    return n;
  };

  const match = (...f) => !I.q || f.some((x) => String(x || '').toLowerCase().includes(I.q));
  const usesCell = (n) => (n > 0 ? `<td class="n">${n.toLocaleString(I18N.locale)}</td>` : `<td class="n"><span class="badge unused">${esc(T('inventory.unused_badge'))}</span></td>`);

  const cut = (t, cls) => `<td class="cut ${cls || ''}" title="${esc(t)}">${esc(t)}</td>`;
  const usedFirst = (arr) => arr.map((x, i) => ({ x, i })).sort((p, q) => (q.x.n > 0) - (p.x.n > 0) || p.i - q.i).map((o) => o.x);
  function table(head, rows, empty) {
    if (!rows.length) return `<p class="dim">${esc(empty)}</p>`;
    const cg = head.map((h) => `<col${h[2] ? ` style="width:${h[2]}"` : ''}>`).join('');
    return `<div class="tablewrap"><table class="fx"><colgroup>${cg}</colgroup><thead><tr>${head.map((h) => `<th${h[1] ? ' class="n"' : ''}>${esc(h[0])}</th>`).join('')}</tr></thead><tbody>${rows.join('')}</tbody></table></div>`;
  }

  function render() {
    if (!I.shown && root.hidden) return;
    let h = `<div class="inv-bar"><input type="search" id="invQ" placeholder="${esc(T('inventory.filter_ph'))}" aria-label="${esc(T('inventory.filter_label'))}" style="min-width:220px">` +
      `<label><input type="checkbox" id="invUnused"${I.unused ? ' checked' : ''}> ${esc(T('inventory.only_unused'))}</label>` +
      `<button type="button" id="invReload">${esc(T('inventory.reload'))}</button>` +
      `<span class="dim" role="status">${I.loading ? esc(T('inventory.scanning')) : ''}</span></div>`;
    if (I.err) h += `<p class="warn" role="alert">${esc(I.err)}</p>`;
    if (I.inv) h += invHtml();
    h += specHtml();
    const keep = document.activeElement && document.activeElement.id === 'invQ';
    root.innerHTML = h;
    const q = $('#invQ');
    q.value = I.q;
    if (keep) { q.focus(); q.setSelectionRange(q.value.length, q.value.length); }
  }

  function invHtml() {
    const c = counts();
    const srcs = (I.inv.sources || []).map((s) => ({ s, tot: (s.skills || []).length + (s.plugins || []).length + (s.mcp || []).length + (s.hooks || []).length }))
      .sort((p, q) => q.tot - p.tot).map((o) => o.s);
    let h = '';
    const keep = (n) => !I.unused || n === 0;
    for (const s of srcs) {
      const nS = (s.skills || []).length, nP = (s.plugins || []).length, nM = (s.mcp || []).length, nH = (s.hooks || []).length;
      const empty = nS + nP + nM + nH + (s.claudeMd || []).length === 0;
      const key = String(s.name);
      const open = key in I.open ? I.open[key] : !empty;
      h += `<section class="env${open ? '' : ' closed'}"><button type="button" class="env-h" data-env="${esc(key)}" aria-expanded="${open}"><span class="chev">${open ? '▾' : '▸'}</span><span class="env-name">${esc(s.name)}</span><span class="env-sum">${esc(empty ? T('inventory.env_empty') : T('inventory.env_summary', { skills: nS, plugins: nP, mcp: nM, hooks: nH }))}</span></button>`;
      if (!open) { h += '</section>'; continue; }
      h += '<div class="env-b">';
      const skills = usedFirst((s.skills || []).map((k) => ({ k, n: skillUses(c, k.name) })).filter((x) => keep(x.n) && match(x.k.name, x.k.description, x.k.origin)));
      h += `<h4>${esc(T('inventory.skills_title', { shown: skills.length, total: nS }))}</h4>` + table([[T('inventory.skills_col_skill'), 0, '22%'], [T('inventory.skills_col_origin'), 0, '14%'], [T('inventory.skills_col_desc')], [T('inventory.skills_col_uses'), 1, '96px']],
        skills.map(({ k, n }) => `<tr><td class="cut" title="${esc(k.name)}">${esc(k.name)}</td>${cut(k.origin, 'dim')}${cut(k.description, 'dim desc')}${usesCell(n)}</tr>`), T('inventory.skills_none'));
      const pl = usedFirst((s.plugins || []).map((p) => ({ p, n: pluginUses(c, p) })).filter((x) => keep(x.n) && match(x.p.id)));
      h += `<h4>${esc(T('inventory.plugins_title', { shown: pl.length, total: nP }))}</h4>` + table([[T('inventory.plugins_col_plugin')], [T('inventory.plugins_col_state'), 0, '110px'], [T('inventory.plugins_col_skills'), 1, '70px'], [T('inventory.plugins_col_uses'), 1, '96px']],
        pl.map(({ p, n }) => `<tr>${cut(p.id)}<td>${p.enabled === true ? esc(T('inventory.plugins_enabled')) : p.enabled === false ? `<span class="dim">${esc(T('inventory.plugins_disabled'))}</span>` : `<span class="dim">${esc(T('inventory.plugins_installed'))}</span>`}</td><td class="n">${num(p.skills)}</td>${usesCell(n)}</tr>`), T('inventory.plugins_none'));
      const mc = usedFirst((s.mcp || []).map((m) => ({ m, n: mcpUses(c, m.name) })).filter((x) => keep(x.n) && match(x.m.name, x.m.scope)));
      h += `<h4>${esc(T('inventory.mcp_title', { shown: mc.length, total: nM }))}</h4><p class="dim">${esc(T('inventory.mcp_note'))}</p>` + table([[T('inventory.mcp_col_name'), 0, '24%'], [T('inventory.mcp_col_scope'), 0, '14%'], [T('inventory.mcp_col_file')], [T('inventory.mcp_col_calls'), 1, '110px']],
        mc.map(({ m, n }) => `<tr>${cut(m.name)}${cut(m.scope, 'dim')}${cut(m.file, 'dim')}${usesCell(n)}</tr>`), T('inventory.mcp_none'));
      const hk = (s.hooks || []).filter((x) => match(x.event, x.matcher, x.command, x.file));
      h += `<h4>${esc(T('inventory.hooks_title', { n: hk.length }))}</h4>` + table([[T('inventory.hooks_col_event'), 0, '16%'], [T('inventory.hooks_col_matcher'), 0, '14%'], [T('inventory.hooks_col_command')], [T('inventory.hooks_col_file'), 0, '22%']],
        hk.map((x) => `<tr>${cut(x.event)}${cut(x.matcher || '*')}${cut(x.command, 'mono')}${cut(x.file, 'dim')}</tr>`), T('inventory.hooks_none'));
      const md = (s.claudeMd || []).filter((x) => match(x.path, x.project, x.scope));
      h += `<h4>${esc(T('inventory.claudemd_title', { n: md.length }))}</h4>` + table([[T('inventory.claudemd_col_file')], [T('inventory.claudemd_col_type'), 0, '90px'], [T('inventory.claudemd_col_lines'), 1, '64px'], [T('inventory.claudemd_col_bytes'), 1, '76px'], [T('inventory.claudemd_col_tokens'), 1, '76px'], [T('inventory.claudemd_col_imports'), 1, '76px'], [T('inventory.claudemd_col_warn'), 0, '120px']],
        md.map((x) => `<tr>${cut(x.path, 'mono')}${cut(x.scope, 'dim')}<td class="n">${num(x.lines)}</td><td class="n">${num(x.bytes).toLocaleString(I18N.locale)}</td><td class="n">${num(x.tokens).toLocaleString(I18N.locale)}</td><td class="n">${num(x.imports)}</td><td class="warn">${(x.warn || []).map((w) => esc(warnLabel(w))).join(', ')}</td></tr>`), T('inventory.claudemd_none'));
      h += '</div></section>';
    }
    return h;
  }

  const bar = (d, t) => (t > 0 ? `<span class="pbar" title="${d}/${t}"><i style="width:${Math.round((100 * d) / t)}%"></i></span> <span class="dim">${d}/${t}</span>` : '<span class="dim">-</span>');
  function sessLinks(ids) {
    if (!ids || !ids.length) return '<span class="dim">-</span>';
    const sh = ids.slice(0, 5).map((id, i) => `<button type="button" class="sess-link" data-sid="${esc(id)}">#${i + 1}</button>`).join('');
    return sh + (ids.length > 5 ? `<span class="dim">+${ids.length - 5}</span>` : '');
  }
  function specHtml() {
    let h = `<h3 class="sec">${esc(T('inventory.spec_title'))}</h3>`;
    const repos = (I.spec && I.spec.repos) || [];
    if (!repos.length) return h + `<p class="dim">${esc(I.loading ? T('inventory.scanning') : T('inventory.spec_none'))}</p>`;
    for (const r of repos) {
      const locs = r.locations.map((l) => ({ l, f: (l.features || []).filter((f) => match(f.name, l.name, r.name) && (!I.unused || f.stage !== 'done')) })).filter((x) => x.f.length || !I.q);
      if (!locs.length) continue;
      h += `<div class="repo"><h4>${esc(r.name)} <span class="dim">· ${esc(r.repo)} · ${esc(T('inventory.spec_repo_cost', { cost: usd(r.costUsd) }))}</span></h4>`;
      for (const { l, f } of locs) {
        h += `<div class="loc-h">${esc(l.worktree ? T('inventory.spec_worktree') : T('inventory.spec_repo'))}: ${esc(T('inventory.spec_loc_summary', { name: l.name, n: num(l.sessions), cost: usd(l.costUsd) }))} ${sessLinks(l.sessionIds)}</div>`;
        h += table([[T('inventory.spec_col_feature')], [T('inventory.spec_col_stage')], [T('inventory.spec_col_tasks')], [T('inventory.spec_col_checklist')], [T('inventory.spec_col_files')], [T('inventory.spec_col_cost'), 1], [T('inventory.spec_col_sessions')], [T('inventory.spec_col_modified')]],
          f.map((x) => `<tr><td>${esc(x.name)}</td><td><span class="badge st-${esc(x.stage)}">${esc(stageLabel(x.stage))}</span></td><td>${bar(num(x.tasksDone), num(x.tasksTotal))}</td><td>${bar(num(x.checklistsDone), num(x.checklistsTotal))}</td><td class="dim">${esc((x.artifacts || []).filter((a) => !a.startsWith('checklists/')).map((a) => a.replace('.md', '')).join(', '))}${x.checklistFiles ? esc(T('inventory.spec_checklist_files', { n: num(x.checklistFiles) })) : ''}</td><td class="n">${x.sessionIds && x.sessionIds.length ? esc(usd(x.costUsd)) : '<span class="dim">-</span>'}</td><td>${sessLinks(x.sessionIds)}</td><td class="dim">${x.lastMs ? esc(new Date(x.lastMs).toLocaleDateString(I18N.locale)) : ''}</td></tr>`),
          T('inventory.spec_features_none'));
      }
      h += '</div>';
    }
    return h;
  }

  root.addEventListener('input', (e) => {
    if (e.target.id === 'invQ') { I.q = e.target.value.trim().toLowerCase(); render(); }
  });
  root.addEventListener('change', (e) => { if (e.target.id === 'invUnused') { I.unused = e.target.checked; render(); } });
  root.addEventListener('click', (e) => {
    const cell = e.target.closest('td.cut');
    if (cell && !e.target.closest('button')) { cell.classList.toggle('x'); return; }
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.env !== undefined) {
      const sec = b.closest('.env');
      I.open[b.dataset.env] = sec.classList.contains('closed');
      render();
    } else if (b.id === 'invReload') load(true);
    else if (b.dataset.sid) openSession(b.dataset.sid);
  });
  CM.inventory = { reload: () => load(true) };
  if (window.__cmHash === '#inventory') enter();
})();
