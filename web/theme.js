/* Applies the saved manual theme override before first paint (blocking, tiny, no deps). */
try {
  var t = localStorage.getItem('cm-theme');
  if (t === 'light' || t === 'dark') document.documentElement.setAttribute('data-theme', t);
} catch (e) { /* storage unavailable: follow the OS theme */ }
