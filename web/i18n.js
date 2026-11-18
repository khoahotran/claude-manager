'use strict';
/* claude-manager UI - i18n (vi / en). Loads locales/<lang>.json (plus vi.json as the fallback) before any other
   script runs (see the bootstrap at the end of index.html), then exposes:
     t(key, params)   {name} interpolation; plural via params.n -> key_one / key_other (Intl.PluralRules); fallback vi -> key.
                      NOT escaped: callers that insert into innerHTML must esc() the result (except *_html keys, trusted markup).
     applyI18n(root)  static markup: data-i18n (textContent), data-i18n-html, data-i18n-title / -placeholder / -aria-label.
     I18N.lang / I18N.locale / I18N.setLang(lang) / I18N.fmtDate / I18N.fmtNum
   The language lives in Settings `ui.lang` (mirrored in localStorage `cm-ui`); changing it reloads the page. */
(() => {
  const LANGS = ['vi', 'en'];
  const store = {
    get(k) { try { return localStorage.getItem(k); } catch { return null; } },
    set(k, v) { try { localStorage.setItem(k, v); } catch { /* ignore */ } },
  };
  function storedLang() {
    try { const u = JSON.parse(store.get('cm-ui') || 'null'); if (u && LANGS.includes(u.lang)) return u.lang; } catch { /* ignore */ }
    return 'vi';
  }
  let lang = storedLang();
  let dict = {}, fallback = {};
  let rules = null;

  const lookup = (d, key) => {
    let o = d;
    for (const p of key.split('.')) { if (o == null || typeof o !== 'object') return undefined; o = o[p]; }
    return typeof o === 'string' ? o : undefined;
  };
  const get = (key) => { const v = lookup(dict, key); return v !== undefined ? v : lookup(fallback, key); };

  function t(key, params) {
    let s;
    if (params && params.n != null) {
      let cat = 'other';
      try { cat = rules.select(Number(params.n)); } catch { /* keep other */ }
      s = get(key + '_' + cat);
      if (s === undefined) s = get(key + '_other');
    }
    if (s === undefined) s = get(key);
    if (s === undefined) return key;
    if (params) s = s.replace(/\{(\w+)\}/g, (m, k) => (k in params && params[k] != null ? String(params[k]) : m));
    return s;
  }

  async function fetchJson(l) {
    const r = await fetch('locales/' + l + '.json', { cache: 'no-cache' });
    if (!r.ok) throw new Error('locale ' + l + ': HTTP ' + r.status);
    return r.json();
  }
  async function init() {
    try {
      const [vi, cur] = await Promise.all([fetchJson('vi'), lang === 'vi' ? null : fetchJson(lang).catch(() => null)]);
      fallback = vi;
      dict = cur || vi;
    } catch { dict = {}; fallback = {}; }
    try { rules = new Intl.PluralRules(lang); } catch { rules = new Intl.PluralRules('en'); }
    document.documentElement.lang = lang;
    applyI18n(document);
  }

  function applyI18n(root) {
    root = root || document;
    const each = (attr, fn) => root.querySelectorAll('[' + attr + ']').forEach((el) => fn(el, t(el.getAttribute(attr))));
    each('data-i18n', (el, v) => { el.textContent = v; });
    each('data-i18n-html', (el, v) => { el.innerHTML = v; });
    each('data-i18n-title', (el, v) => { el.title = v; });
    each('data-i18n-placeholder', (el, v) => { el.placeholder = v; });
    each('data-i18n-aria-label', (el, v) => { el.setAttribute('aria-label', v); });
  }

  const locale = () => t('time.locale');
  const I18N = {
    get lang() { return lang; },
    get locale() { return locale(); },
    init,
    /** persist the language (localStorage `cm-ui`); the caller reloads the page */
    setLang(l) {
      if (!LANGS.includes(l)) return;
      let u = {};
      try { u = JSON.parse(store.get('cm-ui') || '{}') || {}; } catch { /* ignore */ }
      u.lang = l;
      store.set('cm-ui', JSON.stringify(u));
      lang = l;
    },
    fmtDate: (v, opts) => new Date(v).toLocaleDateString(locale(), opts),
    fmtDateTime: (v, opts) => new Date(v).toLocaleString(locale(), opts),
    fmtNum: (n, opts) => Number(n).toLocaleString(locale(), opts),
  };
  window.I18N = I18N;
  window.t = t;
  window.applyI18n = applyI18n;
})();
