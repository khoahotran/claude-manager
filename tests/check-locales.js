// Fails when vi.json and en.json differ in key set or in {placeholder} names.
const fs = require('fs');
const path = require('path');

const dir = path.join(__dirname, '..', 'web', 'locales');
const load = (n) => JSON.parse(fs.readFileSync(path.join(dir, n + '.json'), 'utf8'));

function flat(obj, prefix, out) {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? prefix + '.' + k : k;
    if (v && typeof v === 'object') flat(v, key, out);
    else out[key] = String(v);
  }
  return out;
}

const vi = flat(load('vi'), '', {});
const en = flat(load('en'), '', {});
const ph = (s) => [...new Set(s.match(/\{[a-zA-Z0-9_]+\}/g) || [])].sort().join(',');
const errors = [];

for (const k of Object.keys(vi)) if (!(k in en)) errors.push('missing in en.json: ' + k);
for (const k of Object.keys(en)) if (!(k in vi)) errors.push('missing in vi.json: ' + k);
for (const k of Object.keys(vi)) {
  if (k in en && ph(vi[k]) !== ph(en[k])) {
    errors.push(`placeholder mismatch in ${k}: vi [${ph(vi[k])}] en [${ph(en[k])}]`);
  }
}

if (errors.length) {
  console.error(errors.join('\n'));
  process.exit(1);
}
console.log(`locales OK (${Object.keys(vi).length} keys)`);
