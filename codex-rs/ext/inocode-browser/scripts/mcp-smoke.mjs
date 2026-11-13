// Smoke test: talk MCP (stdio) to `inocode-browser mcp playwright`, open a page, read it back.
// usage: node mcp-smoke.mjs <inocode-browser.exe> <url>
// The first tool call launches the browser window (and fetches CEF if missing).
import { spawn } from 'node:child_process';

const [exe, url = 'data:text/html,<title>smoke</title><h1 id=h>hello</h1><button onclick="document.getElementById(\'h\').textContent=\'clicked\'">go</button>'] = process.argv.slice(2);
const child = spawn(exe, ['mcp', 'playwright'], { stdio: ['pipe', 'pipe', 'inherit'] });
let buf = '';
const waiting = new Map();
child.stdout.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    const msg = JSON.parse(line);
    if (msg.id != null && waiting.has(msg.id)) waiting.get(msg.id)(msg);
  }
});
let n = 0;
const call = (method, params, timeoutMs = 900000) =>
  new Promise((resolve, reject) => {
    const id = ++n;
    const t = setTimeout(() => reject(new Error(`timeout ${method}`)), timeoutMs);
    waiting.set(id, (m) => { clearTimeout(t); resolve(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
const text = (m) => JSON.stringify(m.result ?? m.error).slice(0, 600);

const t0 = Date.now();
const init = await call('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'smoke', version: '0' } });
console.log('initialize', init.result?.serverInfo, `${Date.now() - t0}ms`);
child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
const t1 = Date.now();
console.log('navigate', text(await call('tools/call', { name: 'browser_navigate', arguments: { url } })), `${Date.now() - t1}ms`);
const snap = await call('tools/call', { name: 'browser_snapshot', arguments: {} });
const s = text(snap);
console.log('snapshot', s);
const ref = /button[^\[]*\[ref=(\w+)\]/.exec(JSON.stringify(snap.result))?.[1];
if (ref) {
  console.log('click', ref, text(await call('tools/call', { name: 'browser_click', arguments: { element: 'go', target: ref } })));
  console.log('after', text(await call('tools/call', { name: 'browser_evaluate', arguments: { function: '() => document.getElementById("h").textContent' } })));
}
child.stdin.end();
setTimeout(() => process.exit(0), 3000);
