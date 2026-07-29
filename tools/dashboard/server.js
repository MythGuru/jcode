const http = require('http');
const fs = require('fs');
const fsp = require('fs/promises');
const path = require('path');
const os = require('os');
const net = require('net');
const crypto = require('crypto');

const HOST = '127.0.0.1';
const PORT = Number(process.env.PORT || 7333);
const JC = path.join(os.homedir(), '.jcode');
const ROOT = __dirname;
const HTML = path.join(ROOT, 'dashboard.html');
const SECTION_ORDER = ['structure', 'decision', 'rule', 'problem', 'responsibility'];
let socketCache = { at: 0, sessions: [], servers: [], errors: [] };

async function exists(p) { try { await fsp.access(p, fs.constants.R_OK); return true; } catch { return false; } }
async function readJson(file, errors) {
  try { return JSON.parse(await fsp.readFile(file, { encoding: 'utf8', flag: 'r' })); }
  catch (e) { if (e.code !== 'ENOENT') errors.push(`bad json ${file}: ${e.message}`); return null; }
}
async function safeDir(dir, errors) {
  try { return await fsp.readdir(dir, { withFileTypes: true }); }
  catch (e) { if (e.code !== 'ENOENT') errors.push(`cannot read ${dir}: ${e.message}`); return []; }
}
async function statMtime(file) { try { return (await fsp.stat(file)).mtimeMs; } catch { return 0; } }
async function latestMtime(dir) {
  let max = 0;
  for (const ent of await safeDir(dir, [])) {
    const p = path.join(dir, ent.name);
    if (ent.isDirectory()) max = Math.max(max, await latestMtime(p));
    else max = Math.max(max, await statMtime(p));
  }
  return max;
}

async function loadKnowledge(errors) {
  const dir = path.join(JC, 'knowledge', 'projects');
  const out = [];
  for (const ent of await safeDir(dir, errors)) {
    if (!ent.isFile() || !ent.name.endsWith('.json') || ent.name.endsWith('.bak')) continue;
    const file = path.join(dir, ent.name);
    const json = await readJson(file, errors);
    if (!json) continue;
    out.push({ hash: path.basename(ent.name, '.json'), updated_at: json.updated_at || null, entries: Array.isArray(json.entries) ? json.entries : [], mtime: await statMtime(file) });
  }
  out.sort((a, b) => (b.mtime || 0) - (a.mtime || 0));
  return out;
}

async function loadTodoBundle(sessionId, errors) {
  const dir = path.join(JC, 'todos');
  const todos = await readJson(path.join(dir, `${sessionId}.json`), errors);
  const plan = await readJson(path.join(dir, `${sessionId}-plan.json`), errors);
  const goals = await readJson(path.join(dir, `${sessionId}-goals.json`), errors);
  return { todos: Array.isArray(todos) ? todos : [], plan: plan && !Array.isArray(plan) ? plan : null, goals: Array.isArray(goals) ? goals : [] };
}

async function recentTodoIds(errors) {
  const dir = path.join(JC, 'todos');
  const cutoff = Date.now() - 48 * 60 * 60 * 1000;
  const ids = [];
  for (const ent of await safeDir(dir, errors)) {
    if (!ent.isFile() || !/^session_[^-]+_\d+_[0-9a-f]+\.json$/i.test(ent.name)) continue;
    const file = path.join(dir, ent.name);
    if ((await statMtime(file)) >= cutoff) ids.push(path.basename(ent.name, '.json'));
  }
  return ids;
}

async function collectJsonFiles(dir, errors) {
  const out = [];
  for (const ent of await safeDir(dir, errors)) {
    const p = path.join(dir, ent.name);
    if (ent.isDirectory()) out.push(...await collectJsonFiles(p, errors));
    else if (ent.isFile() && ent.name.endsWith('.json') && !ent.name.endsWith('.bak')) out.push(p);
  }
  return out;
}
async function loadInitiatives(errors) {
  const roots = [path.join(JC, 'goals', 'global'), path.join(JC, 'goals', 'projects'), path.join(JC, 'goals', 'sessions')];
  const out = [];
  for (const root of roots) for (const file of await collectJsonFiles(root, errors)) {
    const json = await readJson(file, errors);
    if (json) out.push({ source: path.relative(JC, file), data: json, mtime: await statMtime(file) });
  }
  return out.sort((a, b) => (b.mtime || 0) - (a.mtime || 0));
}

function pipeNameFromSocket(p) {
  const stem = path.basename(p, path.extname(p)).replace(/[^A-Za-z0-9_-]/g, '').slice(0, 32) || 'jcode-debug';
  const normalized = String(p).replace(/\\/g, '/').toLowerCase();
  const hash = crypto.createHash('sha256').update(normalized).digest('hex').slice(0, 16);
  return `\\\\.\\pipe\\${stem}-${hash}`;
}
function sendDebug(pipe, command, sessionId) {
  return new Promise((resolve) => {
    const sock = net.connect({ path: pipe });
    let buf = '', done = false;
    const id = Math.floor(Math.random() * 1e9);
    const finish = (v) => { if (done) return; done = true; clearTimeout(timer); sock.destroy(); resolve(v); };
    const timer = setTimeout(() => finish({ error: 'timeout' }), 5000);
    sock.setTimeout(3000, () => finish({ error: 'offline' }));
    sock.on('error', () => finish({ error: 'offline' }));
    sock.on('connect', () => sock.write(JSON.stringify({ type: 'debug_command', id, command, ...(sessionId ? { session_id: sessionId } : {}) }) + '\n'));
    sock.on('data', (d) => {
      buf += d.toString('utf8');
      while (buf.includes('\n')) {
        const nl = buf.indexOf('\n');
        const line = buf.slice(0, nl).replace(/\r$/, '');
        buf = buf.slice(nl + 1);
        if (!line) continue;
        try {
          const msg = JSON.parse(line);
          if (msg.id === id) finish(msg.ok ? { output: msg.output } : { error: msg.error || 'failed' });
        } catch (e) { finish({ error: e.message }); }
        if (done) return;
      }
    });
  });
}
function parseOutput(v) {
  if (typeof v !== 'string') return v;
  try { return JSON.parse(v); } catch { return v; }
}
async function loadLiveSessions(errors) {
  if (Date.now() - socketCache.at < 2000) return socketCache;
  const reg = await readJson(path.join(JC, 'servers.json'), errors) || {};
  const sessions = [], servers = [], sockErrors = [];
  for (const srv of Object.values(reg)) {
    if (!srv || !srv.debug_socket) continue;
    const serverChip = { name: srv.name || 'server', icon: srv.icon || '', version: srv.version || '', started_at: srv.started_at || null };
    servers.push(serverChip);
    const pipe = pipeNameFromSocket(srv.debug_socket);
    const res = await sendDebug(pipe, 'sessions');
    if (res.error) { sockErrors.push(`${serverChip.name}: ${res.error}`); continue; }
    const list = parseOutput(res.output);
    if (!Array.isArray(list)) { sockErrors.push(`${serverChip.name}: malformed sessions`); continue; }
    for (const s of list) {
      let wm = null;
      let stmReadAt = null;
      const wmRes = await sendDebug(pipe, 'tool:memory {"action":"working"}', s.session_id);
      if (wmRes.error) wm = null;
      else {
        const parsed = parseOutput(wmRes.output);
        wm = typeof parsed === 'string' ? parsed : (parsed && typeof parsed.output === 'string' ? parsed.output : JSON.stringify(parsed, null, 2));
        stmReadAt = new Date().toISOString();
      }
      sessions.push({ ...s, live: true, server: serverChip.name, working_memory: wm, stm_read_at: stmReadAt });
    }
  }
  socketCache = { at: Date.now(), sessions, servers, errors: sockErrors };
  return socketCache;
}

async function state() {
  const errors = [];
  const [knowledge, live, initiatives] = await Promise.all([loadKnowledge(errors), loadLiveSessions(errors), loadInitiatives(errors)]);
  errors.push(...live.errors.map(e => `debug ${e}`));
  const seen = new Set(live.sessions.map(s => s.session_id));
  const sessions = [];
  for (const s of live.sessions) sessions.push({ ...s, ...await loadTodoBundle(s.session_id, errors) });
  for (const id of await recentTodoIds(errors)) if (!seen.has(id)) sessions.push({ session_id: id, friendly_name: id, status: 'offline', live: false, working_memory: null, stm_read_at: null, ...await loadTodoBundle(id, errors) });
  return { generated_at: new Date().toISOString(), rss_mb: Math.round(process.memoryUsage().rss / 1024 / 1024 * 10) / 10, knowledge, sessions, initiatives, errors, staleness: { knowledge_mtime: await latestMtime(path.join(JC, 'knowledge', 'projects')), todos_mtime: await latestMtime(path.join(JC, 'todos')), goals_mtime: await latestMtime(path.join(JC, 'goals')) }, servers: live.servers };
}

const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url, `http://${req.headers.host}`);
    if (req.method !== 'GET') { res.writeHead(405); return res.end('method not allowed'); }
    if (url.pathname === '/') {
      const html = await fsp.readFile(HTML, { encoding: 'utf8', flag: 'r' });
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
      return res.end(html);
    }
    if (url.pathname === '/api/state') {
      res.writeHead(200, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' });
      return res.end(JSON.stringify(await state()));
    }
    res.writeHead(404); res.end('not found');
  } catch (e) { res.writeHead(500, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: e.message })); }
});
server.listen(PORT, HOST, () => console.log(`JCODE MIND http://${HOST}:${PORT}`));
