'use strict';
/* UAT for the Settings dialog (F-24). Node 24 (global WebSocket) + headless Edge over CDP, no npm deps.
   Starts the real exe on port 8796 with CM_HOME=<temp dir> (copy of bin/config.json), logs in with the printed
   `Open:` link, changes every setting one by one through the UI, saves, reloads, checks the value persisted
   (GET /api/settings + ui.json) and that the UI reflects it, then restores the defaults.
   Usage: node tests/uat/settings-uat.js [path\to\claude-manager.exe]   (env CM_EXE, EDGE also accepted)
   Exits non-zero on failure. Only the exe / Edge processes started here are killed (by PID tree). */
const { spawn, execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const ROOT = path.resolve(__dirname, '..', '..');
const PORT = 8796;
const CDP_PORT = 9796;
const EXE = process.argv[2] || process.env.CM_EXE || path.join(ROOT, 'bin', 'claude-manager.exe');
const EDGE = process.env.EDGE || ['C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe', 'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe'].find((p) => fs.existsSync(p));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const started = [];
let tmp;

function killTree(pid) { try { execFileSync('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' }); } catch { /* already gone */ } }
function cleanup() {
  for (const p of started.splice(0)) killTree(p);
  if (tmp) { for (let i = 0; i < 5; i++) { try { fs.rmSync(tmp, { recursive: true, force: true }); break; } catch { /* locked for a moment */ } } }
}
process.on('exit', cleanup);
process.on('SIGINT', () => process.exit(130));

let checks = 0;
function assert(cond, msg) { if (!cond) throw new Error('ASSERT: ' + msg); checks++; }
const canon = (v) => (Array.isArray(v) ? v.map(canon) : v && typeof v === 'object' ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, canon(v[k])])) : v);
const eq = (a, b) => JSON.stringify(canon(a)) === JSON.stringify(canon(b));

// ------------------------------------------------------------------ CDP
class Cdp {
  constructor(url) { this.ws = new WebSocket(url); this.id = 0; this.wait = new Map(); this.errors = [];
    this.ws.onmessage = (m) => { const d = JSON.parse(m.data);
      if (d.id && this.wait.has(d.id)) { const [ok, ko] = this.wait.get(d.id); this.wait.delete(d.id); d.error ? ko(new Error(d.error.message)) : ok(d.result); }
      else if (d.method === 'Runtime.exceptionThrown') this.errors.push(d.params.exceptionDetails.exception?.description || d.params.exceptionDetails.text); };
  }
  open() { return new Promise((ok, ko) => { this.ws.onopen = ok; this.ws.onerror = ko; }); }
  send(method, params = {}) { const id = ++this.id; this.ws.send(JSON.stringify({ id, method, params })); return new Promise((ok, ko) => this.wait.set(id, [ok, ko])); }
  async eval(expr) {
    const r = await this.send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error('page: ' + (r.exceptionDetails.exception?.description || r.exceptionDetails.text) + '\n  in: ' + expr.slice(0, 200));
    return r.result.value;
  }
  async until(expr, what, ms = 15000) {
    const t = Date.now();
    for (;;) { let v; try { v = await this.eval(expr); } catch { v = null; } if (v) return v; if (Date.now() - t > ms) throw new Error('timeout waiting for ' + what); await sleep(100); }
  }
}

async function main() {
  assert(fs.existsSync(EXE), 'exe not found: ' + EXE);
  assert(EDGE, 'Microsoft Edge not found (set EDGE)');
  tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'cm-uat-'));
  const home = path.join(tmp, 'home');
  fs.mkdirSync(home);
  const cfg = JSON.parse(fs.readFileSync(path.join(ROOT, 'bin', 'config.json'), 'utf8'));
  cfg.port = PORT;
  fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify(cfg, null, 2));
  const extra = path.join(tmp, 'extra-claude');
  fs.mkdirSync(path.join(extra, 'projects'), { recursive: true });

  // ---- server
  const srv = spawn(EXE, ['--no-open', '--port', String(PORT)], { env: { ...process.env, CM_HOME: home }, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
  started.push(srv.pid);
  let out = '';
  srv.stdout.on('data', (d) => { out += d; });
  srv.stderr.on('data', (d) => { out += d; });
  const login = await (async () => { const t = Date.now(); for (;;) { const m = /Open: (\S+)/.exec(out); if (m) return m[1]; if (srv.exitCode != null || Date.now() - t > 30000) throw new Error('server did not start:\n' + out); await sleep(100); } })();
  assert(login.startsWith(`http://127.0.0.1:${PORT}/`), 'server on port ' + PORT + ' (not another instance): ' + login);

  // ---- browser
  const edge = spawn(EDGE, ['--headless=new', `--remote-debugging-port=${CDP_PORT}`, `--user-data-dir=${path.join(tmp, 'edge')}`, '--no-first-run', '--no-default-browser-check', '--disable-extensions', '--window-size=1400,1000', 'about:blank'], { stdio: 'ignore', windowsHide: true });
  started.push(edge.pid);
  const wsUrl = await (async () => { const t = Date.now(); for (;;) { try { const l = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json(); const p = l.find((x) => x.type === 'page'); if (p) return p.webSocketDebuggerUrl; } catch { /* not up yet */ } if (Date.now() - t > 20000) throw new Error('Edge CDP not reachable'); await sleep(200); } })();
  const c = new Cdp(wsUrl);
  await c.open();
  await c.send('Runtime.enable');
  await c.send('Page.enable');

  const ready = () => c.until(`document.readyState === 'complete' && window.CM && CM.settings && CM.settings.get().ok && !!document.querySelector('#fStatus option')`, 'app ready', 30000);
  await c.send('Page.navigate', { url: login });
  await ready();
  const base = `http://127.0.0.1:${PORT}/`;
  const reload = async () => { await c.send('Page.navigate', { url: base }); await sleep(300); await ready(); };
  const getSettings = () => c.eval(`fetch('/api/settings').then(r => r.json())`);
  const uiFile = () => JSON.parse(fs.readFileSync(path.join(home, 'ui.json'), 'utf8'));
  const cfgFile = () => JSON.parse(fs.readFileSync(path.join(home, 'config.json'), 'utf8'));

  const initial = await getSettings();
  const defaults = { mode: 'text', hidden: [], defaults: { status: 'active', group: '', starredOnly: false }, lang: 'vi' };
  assert(eq(initial.ui, defaults), 'fresh CM_HOME starts with default ui: ' + JSON.stringify(initial.ui));

  const openDlg = async () => { await c.eval(`document.querySelector('#settingsBtn').click()`); await c.until(`document.querySelector('#settingsDlg').open`, 'settings dialog'); };
  /** open the dialog, run `action` (page JS), save, wait for the dialog to close (fail on a shown error), reload */
  async function change(label, action) {
    await openDlg();
    await c.eval(`(() => { const $ = (s) => document.querySelector('#stBody ' + s); const fire = (el, t) => el.dispatchEvent(new Event(t, { bubbles: true })); ${action} })()`);
    await c.eval(`document.querySelector('#stSave').click()`);
    const t = Date.now();
    for (;;) {
      // a language change reloads the page: the dialog is then gone (or the document is mid-load) = saved
      const st = await c.eval(`({ open: document.querySelector('#settingsDlg').open, err: document.querySelector('#stErr').hidden ? '' : document.querySelector('#stErr').textContent })`).catch(() => ({ open: false, err: '' }));
      if (st.err) throw new Error(`[${label}] save failed: ${st.err}`);
      if (!st.open) break;
      if (Date.now() - t > 15000) throw new Error(`[${label}] dialog did not close`);
      await sleep(100);
    }
    await reload();
    process.stdout.write('.');
  }
  const dlgState = async (expr) => { await openDlg(); const v = await c.eval(`(() => { const $ = (s) => document.querySelector('#stBody ' + s); return ${expr}; })()`); await c.eval(`document.querySelector('#stCancel').click()`); return v; };

  // ---- 1. display mode
  for (const mode of ['icon', 'text']) {
    await change('mode=' + mode, `document.querySelector('#stMode [data-mode=${mode}]').click();`);
    const s = await getSettings();
    assert(s.ui.mode === mode && uiFile().mode === mode, `mode ${mode} persisted`);
    assert(await c.eval(`document.body.classList.contains('mode-icon')`) === (mode === 'icon'), `body reflects mode ${mode}`);
    assert(await dlgState(`document.querySelector('#stMode [data-mode=${mode}]').getAttribute('aria-pressed')`) === 'true', `dialog shows mode ${mode}`);
  }

  // ---- 2. every show/hide checkbox
  const keys = await dlgState(`[...document.querySelectorAll('#stBody [data-show]')].map((e) => e.dataset.show)`);
  assert(keys.length >= 20, 'found show/hide options: ' + keys.length);
  for (const k of keys) {
    for (const show of [false, true]) {
      await change(`hide ${k}=${!show}`, `const e = $('[data-show="${k}"]'); e.checked = ${show}; fire(e, 'change');`);
      const s = await getSettings();
      assert(s.ui.hidden.includes(k) === !show && uiFile().hidden.includes(k) === !show, `${k} hidden=${!show} persisted`);
      assert(await c.eval(`document.body.classList.contains('hide-${k}')`) === !show, `body reflects ${k}`);
      assert(await dlgState(`$('[data-show="${k}"]').checked`) === show, `dialog reflects ${k}`);
    }
  }

  // ---- 3. default view: status / group / starred
  for (const st of ['archived', 'trashed', 'all', 'active']) {
    await change('status=' + st, `const e = $('#stStatus'); e.value = '${st}'; fire(e, 'change');`);
    const s = await getSettings();
    assert(s.ui.defaults.status === st && uiFile().defaults.status === st, `status ${st} persisted`);
    assert(await c.eval(`document.querySelector('#fStatus').value`) === st, `Sessions status filter shows ${st}`);
    assert(await dlgState(`$('#stStatus').value`) === st, `dialog reflects status ${st}`);
  }
  for (const [id, field, btn, on, off] of [['stGroup', 'group', 'tGroup', 'project', ''], ['stStar', 'starredOnly', 'tStar', true, false]]) {
    for (const checked of [true, false]) {
      await change(`${field}=${checked}`, `const e = $('#${id}'); e.checked = ${checked}; fire(e, 'change');`);
      const want = checked ? on : off;
      const s = await getSettings();
      assert(s.ui.defaults[field] === want && uiFile().defaults[field] === want, `${field}=${JSON.stringify(want)} persisted (got ${JSON.stringify(s.ui.defaults[field])})`);
      assert(await c.eval(`document.querySelector('#${btn}').getAttribute('aria-pressed')`) === String(checked), `Sessions toggle #${btn} reflects ${field}`);
      assert(await dlgState(`$('#${id}').checked`) === checked, `dialog reflects ${field}`);
    }
  }

  // ---- 3b. language: vi (default) -> en -> vi; visible labels follow without a manual reload
  const loc = (l) => JSON.parse(fs.readFileSync(path.join(ROOT, 'web', 'locales', l + '.json'), 'utf8'));
  const L = { vi: loc('vi'), en: loc('en') };
  const labels = () => c.eval(`({ html: document.documentElement.lang, inv: document.querySelector('#tabbtn-inventory .tx').textContent, refresh: document.querySelector('#refresh .tx').textContent, settings: document.querySelector('#settingsBtn .tx').textContent, status: document.querySelector('#fStatus option').textContent })`);
  const checkLang = async (lang) => {
    const s = await getSettings();
    assert(s.ui.lang === lang && uiFile().lang === lang, `lang ${lang} persisted`);
    const v = await labels();
    assert(v.html === lang, `<html lang> is ${lang}: ${v.html}`);
    assert(v.inv === L[lang].header.tab_inventory && v.refresh === L[lang].header.refresh && v.settings === L[lang].header.settings, `header labels in ${lang}: ${JSON.stringify(v)}`);
    assert(v.status.startsWith(L[lang].sessions.status_active), `status filter in ${lang}: ${v.status}`);
    assert(await dlgState(`document.querySelector('#stTitle').textContent`) === L[lang].settings.title, `settings title in ${lang}`);
    assert(await dlgState(`$('#stLang').value`) === lang, `language select shows ${lang}`);
    assert((await dlgState(`$('#stMode').parentElement.querySelector('h3').textContent`)) === L[lang].settings.mode_title, `settings section title in ${lang}`);
  };
  assert(L.vi.header.tab_inventory !== L.en.header.tab_inventory && L.vi.settings.title !== L.en.settings.title, 'vi and en labels differ');
  await checkLang('vi');
  await change('lang=en', `const e = $('#stLang'); e.value = 'en'; fire(e, 'change');`);
  await checkLang('en');
  await change('lang=vi', `const e = $('#stLang'); e.value = 'vi'; fire(e, 'change');`);
  await checkLang('vi');

  // ---- 4. sources: rename, distro, add, remove, restore
  const srcOrig = initial.sources.map((s) => ({ ...s }));
  const setSrc = (i, f, v) => `{ const e = $('.src-row[data-i="${i}"] input[data-f=${f}]'); e.value = ${JSON.stringify(v)}; fire(e, 'input'); }`;
  const last = srcOrig.length - 1;
  const checkSources = async (want, label) => {
    const s = await getSettings();
    const norm = (v) => v.map((x) => ({ name: x.name, dir: x.dir, distro: x.distro || '' }));
    assert(eq(norm(s.sources), norm(want)), `${label}: GET sources ${JSON.stringify(s.sources)}`);
    assert(eq(norm(cfgFile().sources), norm(want)), `${label}: config.json`);
    assert(cfgFile().port === PORT, `${label}: config.json keeps port`);
    const ui = await dlgState(`[...document.querySelectorAll('#stBody .src-row')].map((r) => ({ name: r.querySelector('[data-f=name]').value, dir: r.querySelector('[data-f=dir]').value, distro: r.querySelector('[data-f=distro]').value }))`);
    assert(eq(ui, norm(want)), `${label}: dialog shows ${JSON.stringify(ui)}`);
  };
  let want = srcOrig.map((s) => ({ ...s }));
  want[last].name = 'UAT renamed';
  await change('source name', setSrc(last, 'name', 'UAT renamed'));
  await checkSources(want, 'rename');
  want[last].distro = 'UAT-distro';
  await change('source distro', setSrc(last, 'distro', 'UAT-distro'));
  await checkSources(want, 'distro');
  await change('add source (name+dir)', `document.querySelector('#stAddSrc').click(); const rows = document.querySelectorAll('#stBody .src-row'); const r = rows[rows.length - 1];
    for (const [f, v] of [['name', 'UAT extra'], ['dir', ${JSON.stringify(extra)}]]) { const e = r.querySelector('[data-f=' + f + ']'); e.value = v; fire(e, 'input'); }`);
  want.push({ name: 'UAT extra', dir: extra });
  await checkSources(want, 'add');
  await change('remove source', `$('[data-rm="${want.length - 1}"]').click();`);
  want.pop();
  await checkSources(want, 'remove');
  await change('restore sources', setSrc(last, 'name', srcOrig[last].name) + setSrc(last, 'distro', srcOrig[last].distro || ''));
  await checkSources(srcOrig, 'restore');

  // invalid input is reported in the dialog and nothing is stored
  await openDlg();
  await c.eval(`(() => { const e = document.querySelector('#stBody .src-row[data-i="0"] input[data-f=dir]'); e.value = 'Z:\\\\no\\\\such\\\\dir'; e.dispatchEvent(new Event('input', { bubbles: true })); document.querySelector('#stSave').click(); })()`);
  const errTxt = await c.until(`!document.querySelector('#stErr').hidden && document.querySelector('#stErr').textContent`, 'error for an invalid dir');
  assert(/projects|exist|absolute/i.test(errTxt), 'invalid dir error shown: ' + errTxt);
  await c.eval(`document.querySelector('#stCancel').click()`);
  await checkSources(srcOrig, 'after rejected save');

  // ---- 5. final state == defaults
  const fin = await getSettings();
  assert(eq(fin.ui, defaults), 'restored default ui: ' + JSON.stringify(fin.ui));
  assert(c.errors.length === 0, 'no uncaught page errors: ' + c.errors.join(' | '));
  console.log(`\nPASS settings UAT: ${checks} checks, ${keys.length} show/hide options, exe ${EXE}`);
}

main().then(() => { cleanup(); process.exit(0); }, (e) => { console.error('\nFAIL ' + (e && e.stack || e)); cleanup(); process.exit(1); });
