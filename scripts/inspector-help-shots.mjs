// Takes the screenshots of the inspector's help page (src/help/*.webp) from running
// inspectors, through the DevTools protocol of a headless Chromium.
//
//   node scripts/inspector-help-shots.mjs <base URL> <output dir> scripts/inspector-help-shots.json
//
// Each shot in the JSON names a view (the URL fragment), the server it is taken from (`base`,
// default: the first argument), what to wait for, the region to keep (`from`/`to`: a CSS
// selector or `card:<title>`), an optional script that shortens long tables (`before`), and
// the numbered callouts (`marks`: [number, selector, text, nth, side]) that src/help/help.html
// explains. The views name data that exists on the machine the shots were taken on (the
// mainnet address index of one owner, the bee-factory block store under the Anvil dev key):
// for other data, change the fragments. The first server ran with the mainnet index's owner
// and `--chain-id 1`, the second (`base` http://127.0.0.1:8192) with the Anvil dev key and
// `--chain-id 1337`. Needs Node 22+ (global WebSocket) and `chromium`.
import { spawn } from 'node:child_process';
import { writeFileSync, readFileSync, mkdirSync } from 'node:fs';

const [base, outDir, specFile] = process.argv.slice(2);
const shots = JSON.parse(readFileSync(specFile, 'utf8'));
const PORT = 9333;
const profile = `${process.env.HOME}/.cache/eth2swarm-shots-profile`;
mkdirSync(outDir, { recursive: true });

const chrome = spawn('chromium', ['--headless=new', `--remote-debugging-port=${PORT}`,
  `--user-data-dir=${profile}`, '--no-first-run', '--hide-scrollbars', 'about:blank'],
  { stdio: 'ignore' });
// However the run ends (an error before the loop too), the browser goes with it: a stale one
// would keep port 9333 and the profile locked for the next run.
const killChrome = () => { try { chrome.kill(); } catch { /* already gone */ } };
process.on('exit', killChrome);
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => { killChrome(); process.exit(130); });
const sleep = ms => new Promise(r => setTimeout(r, ms));

let target;
for (let i = 0; i < 60 && !target; i++) {
  try {
    const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
    target = list.find(t => t.type === 'page');
  } catch { /* not up yet */ }
  if (!target) await sleep(500);
}
if (!target) { console.error('no page target'); chrome.kill(); process.exit(1); }

const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error('the DevTools WebSocket did not open in 15 s')), 15000);
  ws.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
  for (const ev of ['error', 'close']) {
    ws.addEventListener(ev, () => { clearTimeout(timer); reject(new Error(`the DevTools WebSocket failed (${ev})`)); }, { once: true });
  }
});
let id = 0; const waiting = new Map();
ws.addEventListener('message', ev => {
  const m = JSON.parse(ev.data);
  if (m.id && waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); }
});
const send = (method, params = {}) => new Promise((res, rej) => {
  const i = ++id; waiting.set(i, m => m.error ? rej(new Error(`${method}: ${m.error.message}`)) : res(m.result));
  ws.send(JSON.stringify({ id: i, method, params }));
});
const evaluate = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) throw new Error(`${expr.slice(0, 80)}: ${JSON.stringify(r.exceptionDetails)}`);
  return r.result.value;
};

await send('Page.enable'); await send('Runtime.enable');
await send('Emulation.setDeviceMetricsOverride', { width: 1240, height: 900, deviceScaleFactor: 1, mobile: false });
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'light' }] });

// Helpers injected into the page: find an element by selector and text, mark it.
const HELPERS = `
window.__find = (sel, text, nth = 0) => {
  const all = [...document.querySelectorAll(sel)].filter(e => e.offsetParent !== null && (!text || e.textContent.includes(text)));
  return all[nth] || null;
};
window.__card = (t) => [...document.querySelectorAll('.card')].find(c => c.offsetParent !== null && c.querySelector('h2') && c.querySelector('h2').textContent.includes(t));
window.__el = (q) => q.startsWith('card:') ? __card(q.slice(5)) : document.querySelector(q);
// Keep the given rows of a table (0 = header) and put a row saying how many are left out.
window.__trim = (table, keep) => {
  const rows = [...table.rows]; const cols = rows[0].cells.length; let gap = 0;
  const flush = (before) => { if (!gap) return; const tr = document.createElement('tr');
    tr.innerHTML = '<td colspan="' + cols + '" style="color:#888;font-style:italic">… ' + gap + ' more rows …</td>';
    before ? before.before(tr) : table.tBodies[0].appendChild(tr); gap = 0; };
  rows.forEach((r, i) => { if (keep.includes(i)) flush(r); else { r.remove(); gap++; } });
  flush(null);
};
window.__mark = (n, sel, text, nth, side) => {
  const el = __find(sel, text, nth);
  if (!el) return 'missing: ' + sel + ' / ' + text;
  const r = el.getBoundingClientRect();
  const b = document.createElement('div');
  b.textContent = n;
  b.className = '__callout';
  const x = side === 'right' ? r.right + 4 : side === 'in' ? r.right - 28 : side === 'below' ? r.left + r.width / 2 - 11 : r.left - 26;
  const y = side === 'below' ? r.bottom + 3 : r.top + Math.max(0, (Math.min(r.height, 28) - 22) / 2);
  b.style.cssText = 'position:absolute;z-index:9999;width:22px;height:22px;border-radius:50%;background:#7b2cbf;color:#fff;' +
    'font:700 13px/22px -apple-system,Segoe UI,sans-serif;text-align:center;box-shadow:0 0 0 2px #fff;' +
    'left:' + Math.max(2, x + scrollX) + 'px;top:' + (y + scrollY) + 'px';
  document.body.appendChild(b);
  el.style.outline = '2px solid #7b2cbf'; el.style.outlineOffset = '-2px';
  return 'ok';
};`;

// Shots not retaken, and callouts not placed: any of them makes the run fail, since the help
// page explains each picture's numbers by position (an old picture, or one with a number
// missing, would no longer match its list).
const incomplete = [];
try {
for (const s of shots) {
  await send('Page.navigate', { url: `${s.base || base}/${s.hash ? '#' + s.hash : ''}` });
  await sleep(800);
  // A reload makes the page run the fragment's lookup; wait for its result.
  await send('Page.reload', { ignoreCache: true });
  let ready = false;
  for (let i = 0; i < 240 && !ready; i++) {
    await sleep(500);
    ready = await evaluate(`(() => {
      if (!document.getElementById('node') || !document.getElementById('node').options.length) return false;
      ${s.wait ? `const o = document.getElementById(${JSON.stringify(s.wait)});
      return !!o && o.innerHTML.length > 0 && !o.querySelector('.spinner');` : 'return true;'}
    })()`);
  }
  if (!ready) { console.error(`${s.name}: not ready`); incomplete.push(`${s.name}: not ready`); continue; }
  await evaluate(HELPERS);
  if (s.before) await evaluate(s.before);
  await sleep(300);
  let missing = 0;
  for (const [n, sel, text, nth, side] of s.marks || []) {
    const r = await evaluate(`__mark(${JSON.stringify(String(n))}, ${JSON.stringify(sel)}, ${JSON.stringify(text || '')}, ${nth || 0}, ${JSON.stringify(side || 'left')})`);
    if (r !== 'ok') { console.error(`${s.name}: ${r}`); incomplete.push(`${s.name}: ${r}`); missing++; }
  }
  // A picture with a number missing would no longer match its list: keep the old one.
  if (missing) continue;
  const clip = await evaluate(`(() => {
    const a = __el(${JSON.stringify(s.from || 'header')}).getBoundingClientRect();
    const z = __el(${JSON.stringify(s.to || s.from || 'header')}).getBoundingClientRect();
    const top = a.top + scrollY - 6, bottom = Math.min(z.bottom + scrollY + 10, top + ${s.maxHeight || 1500});
    return { x: 0, y: Math.max(0, top), width: 1240, height: bottom - Math.max(0, top), scale: 1 };
  })()`);
  const shot = await send('Page.captureScreenshot', { format: 'webp', quality: 82, clip, captureBeyondViewport: true });
  const buf = Buffer.from(shot.data, 'base64');
  writeFileSync(`${outDir}/${s.name}.webp`, buf);
  console.log(`${s.name}.webp ${clip.width}x${Math.round(clip.height)} ${buf.length} B`);
}
} finally {
  ws.close(); chrome.kill();
}
if (incomplete.length) {
  console.error(`incomplete retake (${incomplete.length}): ${incomplete.join('; ')}`);
  process.exit(1);
}
