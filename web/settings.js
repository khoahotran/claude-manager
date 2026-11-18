'use strict';
/* claude-manager UI - Settings dialog (sources, icon/text mode, what to show, default view) and the per-tab Help panel.
   Backend: GET/PUT /api/settings. Feature-detected: without it the UI options still work and are kept in localStorage. */
(() => {
  const CM = window.CM;
  const { $, $$, esc, api, toast, store, T } = CM;

  // [group title key, [hide-key, ...]]; labels are settings.hide_<key with _ for ->
  const HIDE_GROUPS = [
    ['settings.group_filters', ['filter-source', 'filter-project', 'filter-model', 'filter-tag']],
    ['settings.group_stats', ['cards-sessions', 'cards-usage']],
    ['settings.group_columns', ['col-tags', 'col-proj', 'col-source', 'col-status', 'col-tokens', 'col-cost', 'col-total', 'col-size', 'col-model']],
    ['settings.group_usage_panels', ['usage-blocks', 'usage-chart', 'usage-active', 'usage-heat', 'usage-top', 'usage-tables']],
  ];
  const ALL_KEYS = new Set(HIDE_GROUPS.flatMap((g) => g[1]));
  const STATUS = ['active', 'archived', 'trashed', 'all'];
  const LANGS = ['vi', 'en'];

  const S = { ok: false, sources: [], models: [], efforts: [], ui: { mode: 'text', hidden: [], defaults: { status: 'active', group: '', starredOnly: false }, lang: I18N.lang } };
  /** defaults.group contract: '' (flat) or 'project'; older stored values may be booleans. */
  const grpVal = (g) => (g === true || g === 'project' ? 'project' : '');

  function cleanUi(u) {
    const d = (u && u.defaults) || {};
    return {
      mode: u && u.mode === 'icon' ? 'icon' : 'text',
      hidden: Array.isArray(u && u.hidden) ? [...new Set(u.hidden.filter((k) => typeof k === 'string' && ALL_KEYS.has(k)))] : [],
      defaults: { status: STATUS.includes(d.status) ? d.status : 'active', group: grpVal(d.group), starredOnly: !!d.starredOnly },
      lang: u && LANGS.includes(u.lang) ? u.lang : I18N.lang,   // an older server without ui.lang: keep the local choice
    };
  }
  try { const l = JSON.parse(store.get('cm-ui') || 'null'); if (l) S.ui = cleanUi(l); } catch { /* ignore */ }

  function applyUi() {
    const b = document.body;
    b.classList.toggle('mode-icon', S.ui.mode === 'icon');
    for (const c of [...b.classList]) if (c.startsWith('hide-')) b.classList.remove(c);
    for (const k of S.ui.hidden) b.classList.add('hide-' + k);
  }
  applyUi();

  CM.settings = {
    get: () => S,
    isHidden: (k) => S.ui.hidden.includes(k),
    models: () => S.models,
    efforts: () => S.efforts,
    open: openSettings,
  };

  async function load(first) {
    try {
      const r = await api('/api/settings', { quiet: true });
      if (r && typeof r === 'object') {
        S.ok = true;
        S.sources = Array.isArray(r.sources) ? r.sources : [];
        S.models = Array.isArray(r.models) ? r.models.filter((m) => typeof m === 'string') : [];
        S.efforts = Array.isArray(r.efforts) ? r.efforts.filter((m) => typeof m === 'string') : [];
        if (r.ui && typeof r.ui === 'object') S.ui = cleanUi(r.ui);
        store.set('cm-ui', JSON.stringify(S.ui));
        if (S.ui.lang !== I18N.lang) { I18N.setLang(S.ui.lang); location.reload(); return; }   // server language differs from this browser's
      }
    } catch { S.ok = false; }
    applyUi();
    document.dispatchEvent(new CustomEvent('cm:settings', { detail: { first: !!first } }));
  }

  const dlg = $('#settingsDlg'), body = $('#stBody'), err = $('#stErr');
  let draft = null;

  function sourceRow(s, i) {
    return `<div class="src-row" data-i="${i}">
      <label>${esc(T('settings.src_name'))}<input type="text" data-f="name" value="${esc(s.name || '')}" placeholder="${esc(T('settings.src_name_ph'))}"></label>
      <label class="wide">${esc(T('settings.src_dir'))}<input type="text" data-f="dir" value="${esc(s.dir || '')}" placeholder="${esc(T('settings.src_dir_ph'))}" spellcheck="false"></label>
      <label>${esc(T('settings.src_distro'))}<input type="text" data-f="distro" value="${esc(s.distro || '')}" placeholder="${esc(T('settings.src_distro_ph'))}"></label>
      <button type="button" class="danger" data-rm="${i}" title="${esc(T('settings.src_remove_tip'))}" aria-label="${esc(T('settings.src_remove_label', { name: s.name || '' }))}">${esc(T('settings.src_remove'))}</button>
    </div>`;
  }
  function renderSettings() {
    const d = draft;
    const grp = HIDE_GROUPS.map(([tk, items]) => `<fieldset><legend>${esc(T(tk))}</legend><div class="chk-grid">` +
      items.map((k) => `<label class="chk"><input type="checkbox" data-show="${k}" ${d.ui.hidden.includes(k) ? '' : 'checked'}> ${esc(T('settings.hide_' + k.replace(/-/g, '_')))}</label>`).join('') + '</div></fieldset>').join('');
    body.innerHTML = `
      <section class="st-sec"><h3>${esc(T('settings.sources_title'))}</h3>
        ${S.ok ? `<p class="muted small">${T('settings.sources_help_html')}</p>
        <div id="stSources">${d.sources.map(sourceRow).join('') || `<p class="muted small">${esc(T('settings.sources_none'))}</p>`}</div>
        <button type="button" id="stAddSrc"${d.sources.length >= 10 ? ' disabled' : ''}>${esc(T('settings.sources_add'))}</button>`
        : `<p class="muted small">${T('settings.sources_unsupported_html')}</p>`}
      </section>
      <section class="st-sec"><h3>${esc(T('settings.lang_title'))}</h3>
        <div class="def-row">
          <label>${esc(T('settings.lang_label'))} <select id="stLang">${LANGS.map((v) => `<option value="${v}" ${d.ui.lang === v ? 'selected' : ''}>${esc(T('settings.lang_' + v))}</option>`).join('')}</select></label>
        </div>
        <p class="muted small">${esc(T('settings.lang_hint'))}</p>
      </section>
      <section class="st-sec"><h3>${esc(T('settings.mode_title'))}</h3>
        <div class="seg" id="stMode" role="group" aria-label="${esc(T('settings.mode_label'))}">
          <button type="button" data-mode="text" aria-pressed="${d.ui.mode === 'text'}">${esc(T('settings.mode_text'))}</button>
          <button type="button" data-mode="icon" aria-pressed="${d.ui.mode === 'icon'}">${esc(T('settings.mode_icon'))}</button>
        </div>
        <p class="muted small">${esc(T('settings.mode_hint'))}</p>
      </section>
      <section class="st-sec"><h3>${esc(T('settings.showhide_title'))}</h3><p class="muted small">${esc(T('settings.showhide_hint'))}</p>${grp}</section>
      <section class="st-sec"><h3>${esc(T('settings.defaults_title'))}</h3>
        <div class="def-row">
          <label>${esc(T('settings.defaults_status'))} <select id="stStatus">${STATUS.map((v) => `<option value="${v}" ${d.ui.defaults.status === v ? 'selected' : ''}>${esc(T('sessions.status_' + v))}</option>`).join('')}</select></label>
          <label class="chk"><input type="checkbox" id="stGroup" ${d.ui.defaults.group === 'project' ? 'checked' : ''}> ${esc(T('settings.defaults_group'))}</label>
          <label class="chk"><input type="checkbox" id="stStar" ${d.ui.defaults.starredOnly ? 'checked' : ''}> ${esc(T('settings.defaults_starred'))}</label>
        </div>
        <p class="muted small">${esc(T('settings.defaults_hint'))}</p>
      </section>`;
  }
  function openSettings() {
    if (dlg.open) return;
    draft = { sources: S.sources.map((s) => ({ name: s.name || '', dir: s.dir || '', distro: s.distro || '' })), ui: JSON.parse(JSON.stringify(S.ui)) };
    err.hidden = true;
    renderSettings();
    dlg.showModal();
  }
  body.addEventListener('input', (e) => {
    const row = e.target.closest('.src-row');
    if (row && e.target.dataset.f) draft.sources[+row.dataset.i][e.target.dataset.f] = e.target.value;
  });
  body.addEventListener('change', (e) => {
    const t = e.target;
    if (t.dataset.show) {
      const h = draft.ui.hidden, k = t.dataset.show;
      if (t.checked) draft.ui.hidden = h.filter((x) => x !== k); else if (!h.includes(k)) h.push(k);
    } else if (t.id === 'stLang') draft.ui.lang = t.value;
    else if (t.id === 'stStatus') draft.ui.defaults.status = t.value;
    else if (t.id === 'stGroup') draft.ui.defaults.group = t.checked ? 'project' : '';
    else if (t.id === 'stStar') draft.ui.defaults.starredOnly = t.checked;
  });
  body.addEventListener('click', (e) => {
    const m = e.target.closest('[data-mode]');
    if (m) { draft.ui.mode = m.dataset.mode; $$('#stMode button').forEach((b) => b.setAttribute('aria-pressed', String(b === m))); return; }
    const rm = e.target.closest('[data-rm]');
    if (rm) { draft.sources.splice(+rm.dataset.rm, 1); renderSettings(); return; }
    if (e.target.id === 'stAddSrc') { draft.sources.push({ name: '', dir: '', distro: '' }); renderSettings(); const r = $$('.src-row input[data-f=name]', body); if (r.length) r[r.length - 1].focus(); }
  });
  $('#stCancel').addEventListener('click', () => dlg.close());

  $('#stForm').addEventListener('submit', async (e) => {
    e.preventDefault();
    const btn = $('#stSave');
    err.hidden = true;
    const ui = cleanUi(draft.ui);
    let sources = null;
    if (S.ok) {
      sources = draft.sources.map((s) => { const o = { name: s.name.trim(), dir: s.dir.trim() }; if (s.distro.trim()) o.distro = s.distro.trim(); return o; });
      const bad = sources.findIndex((s) => !s.name || !s.dir);
      if (bad >= 0) { err.textContent = T('settings.err_source_incomplete', { n: bad + 1 }); err.hidden = false; return; }
      if (new Set(sources.map((s) => s.name.toLowerCase())).size !== sources.length) { err.textContent = T('settings.err_source_dup'); err.hidden = false; return; }
    }
    const srcChanged = S.ok && JSON.stringify(sources.map((s) => [s.name, s.dir, s.distro || ''])) !== JSON.stringify(S.sources.map((s) => [s.name || '', s.dir || '', s.distro || '']));
    const defChanged = JSON.stringify(ui.defaults) !== JSON.stringify(S.ui.defaults);
    const langChanged = ui.lang !== I18N.lang;
    btn.disabled = true;
    try {
      if (S.ok) {
        const r = await api('/api/settings', { method: 'PUT', body: srcChanged ? { sources, ui } : { ui }, quiet: true });
        if (r && r.ui) { S.sources = Array.isArray(r.sources) ? r.sources : S.sources; S.ui = cleanUi(r.ui); }
        else { if (srcChanged) S.sources = sources; S.ui = ui; }
      } else S.ui = ui;
      store.set('cm-ui', JSON.stringify(S.ui));
      applyUi();
      document.dispatchEvent(new CustomEvent('cm:settings', { detail: {} }));
      if (defChanged) CM.applyDefaultView(true);
      dlg.close();
      if (langChanged) { I18N.setLang(ui.lang); location.reload(); return; }   // re-render everything in the new language
      toast(T('toast.settings_saved'));
      if (srcChanged) { const rf = $('#refresh'); if (rf) rf.click(); }
    } catch (ex) {
      err.textContent = ex.detail || ex.message || T('settings.save_failed');
      err.hidden = false;
    } finally { btn.disabled = false; }
  });

  const hdlg = $('#helpDlg');
  // help.* values are trusted markup (<b> <u> <code> <kbd>): items go in unescaped, params are esc()'d by the caller
  const sec = (title, items) => `<section class="st-sec"><h3>${esc(title)}</h3><ul class="help-list">${items.map((i) => `<li>${i}</li>`).join('')}</ul></section>`;
  const hs = (area, group, n) => sec(T('help.' + area + '.' + group + '_title'), Array.from({ length: n }, (_, i) => T('help.' + area + '.' + group + '_' + (i + 1))));

  function helpSessions() {
    return hs('sessions', 'table', 5) + hs('sessions', 'filters', 2) + hs('sessions', 'expiring', 1) + hs('sessions', 'shortcuts', 2);
  }
  function helpUsage() {
    const d = CM.usageInfo ? CM.usageInfo() : null;
    const tz = d && d.tz ? 'UTC' + d.tz : T('help.usage.calc_tz_local');
    const rt = d && d.retention;
    return hs('usage', 'modes', 2) + sec(T('help.usage.calc_title'), [
      T('help.usage.calc_1'), T('help.usage.calc_2'), T('help.usage.calc_3', { tz: esc(tz) }), T('help.usage.calc_4'), T('help.usage.calc_5'),
    ]) + sec(T('help.usage.old_title'), [
      rt ? T('help.usage.old_retention', { days: esc(rt.cleanupPeriodDays) }) + (rt.oldestLiveDay ? T('help.usage.old_oldest', { day: esc(rt.oldestLiveDay) }) : '') +
        (Number(rt.frozenDays) > 0 ? T('help.usage.old_frozen', { n: esc(rt.frozenDays) }) : '') + T('help.usage.old_archive')
        : T('help.usage.old_generic'),
    ]);
  }
  function helpWorkspace() {
    return hs('workspace', 'shortcuts', 3) + hs('workspace', 'newpane', 2) + hs('workspace', 'cli', 2);
  }
  function openHelp() {
    if (hdlg.open || document.querySelector('dialog[open]')) return;
    const tab = CM.getTab();
    const names = { sessions: 'header.tab_sessions', usage: 'header.tab_usage', workspace: 'header.tab_workspace' };
    $('#hpTitle').textContent = T('help.title_tab', { tab: names[tab] ? T(names[tab]) : '' });
    $('#hpBody').innerHTML = (tab === 'usage' ? helpUsage() : tab === 'workspace' ? helpWorkspace() : helpSessions()) +
      sec(T('help.general.title'), [T('help.general.1'), T('help.general.2'), T('help.general.3')]);
    hdlg.showModal();
  }
  $('#helpBtn').addEventListener('click', openHelp);
  $('#hpClose').addEventListener('click', () => hdlg.close());
  hdlg.addEventListener('click', (e) => { if (e.target === hdlg) hdlg.close(); });
  $('#settingsBtn').addEventListener('click', openSettings);

  load(true);
})();
