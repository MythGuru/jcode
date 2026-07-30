const http = require('http');
const fs = require('fs');
const fsp = require('fs/promises');
const path = require('path');
const os = require('os');
const net = require('net');
const crypto = require('crypto');
const { spawn } = require('child_process');

const HOST = '127.0.0.1';
const PORT = Number(process.env.PORT || 7333);
const JC = path.join(os.homedir(), '.jcode');
const ROOT = __dirname;
const HTML = path.join(ROOT, 'dashboard.html');
const SECTION_ORDER = ['structure', 'decision', 'rule', 'problem', 'responsibility'];
const ALLOWED_ORIGINS = new Set([`http://${HOST}:${PORT}`, `http://localhost:${PORT}`]);
let socketCache = { at: 0, sessions: [], servers: [], errors: [] };
const SESSION_ID_RE = /^session_[a-z0-9]+_\d+_[0-9a-f]+$/i;

function json(res, status, obj) {
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(obj));
}
function plainError(res, status, error) { return json(res, status, { ok: false, error }); }
function validateSessionId(sessionId) { return typeof sessionId === 'string' && SESSION_ID_RE.test(sessionId); }

async function exists(p) { try { await fsp.access(p, fs.constants.R_OK); return true; } catch { return false; } }
async function isDirectory(p) { try { return (await fsp.stat(p)).isDirectory(); } catch { return false; } }
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
    const mtime = await statMtime(file);
    if (mtime >= cutoff) ids.push({ id: path.basename(ent.name, '.json'), mtime });
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
function sendPipeRequest(pipe, request, matchResponse) {
  return new Promise((resolve) => {
    const sock = net.connect({ path: pipe });
    let buf = '', done = false;
    const finish = (v) => { if (done) return; done = true; clearTimeout(timer); sock.destroy(); resolve(v); };
    const timer = setTimeout(() => finish({ error: 'timeout' }), 5000);
    sock.setTimeout(3000, () => finish({ error: 'offline' }));
    sock.on('error', () => finish({ error: 'offline' }));
    sock.on('connect', () => sock.write(JSON.stringify(request) + '\n'));
    sock.on('data', (d) => {
      buf += d.toString('utf8');
      while (buf.includes('\n')) {
        const nl = buf.indexOf('\n');
        const line = buf.slice(0, nl).replace(/\r$/, '');
        buf = buf.slice(nl + 1);
        if (!line) continue;
        try {
          const msg = JSON.parse(line);
          const result = matchResponse(msg);
          if (result) finish(result);
        } catch (e) { finish({ error: e.message }); }
        if (done) return;
      }
    });
  });
}
function sendDebug(pipe, command, sessionId) {
  const id = Math.floor(Math.random() * 1e9);
  return sendPipeRequest(pipe,
    { type: 'debug_command', id, command, ...(sessionId ? { session_id: sessionId } : {}) },
    (msg) => msg.id === id ? (msg.ok ? { output: msg.output } : { error: msg.error || 'failed' }) : null);
}
// Types text into a live terminal as if the user typed it and pressed Enter.
// This starts a turn immediately, which is what an IDLE session needs; the
// queue_interrupt path only drains once a turn is already running.
function sendTranscript(pipe, text, sessionId) {
  const id = Math.floor(Math.random() * 1e9);
  return sendPipeRequest(pipe,
    { type: 'transcript', id, text, mode: 'send', session_id: sessionId },
    (msg) => {
      if (msg.id !== id && msg.type !== 'error') return null;
      if (msg.type === 'done') return { ok: true };
      if (msg.type === 'error') return { error: msg.message || 'failed' };
      return null;
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

function rejectBadOrigin(req, res) {
  if (req.method === 'GET' || req.method === 'HEAD') return false;
  const origin = req.headers.origin;
  if (origin && !ALLOWED_ORIGINS.has(origin)) {
    res.writeHead(403, { 'content-type': 'application/json; charset=utf-8' });
    res.end(JSON.stringify({ ok: false, error: 'Blocked: this action can only be started from this dashboard.' }));
    return true;
  }
  return false;
}

function readBody(req, limit = 65536) {
  return new Promise((resolve, reject) => {
    let body = '';
    let rejected = false;
    req.on('data', chunk => {
      body += chunk;
      if (!rejected && body.length > limit) { rejected = true; reject(new Error('Request is too large.')); }
    });
    req.on('end', () => { if (!rejected) resolve(body); });
    req.on('error', reject);
  });
}

async function parseJsonBody(req, res, limit) {
  try { return JSON.parse(await readBody(req, limit) || '{}'); }
  catch (e) {
    plainError(res, e.message === 'Request is too large.' ? 413 : 400, e.message === 'Request is too large.' ? 'Request is too large.' : 'Please send valid dashboard data.');
    return null;
  }
}

async function findLiveSession(sessionId, errors) {
  const live = await loadLiveSessions(errors);
  const session = live.sessions.find(s => s.session_id === sessionId);
  if (!session) return null;
  const reg = await readJson(path.join(JC, 'servers.json'), errors) || {};
  for (const srv of Object.values(reg)) {
    if (!srv || !srv.debug_socket) continue;
    if ((srv.name || 'server') === session.server) return { session, pipe: pipeNameFromSocket(srv.debug_socket) };
  }
  return null;
}

async function queueDashboardMessage(sessionId, text, urgent, errors) {
  if (!validateSessionId(sessionId)) return { ok: false, status: 400, error: 'Please choose a valid live terminal.' };
  const trimmed = typeof text === 'string' ? text.trim() : '';
  if (!trimmed) return { ok: false, status: 400, error: 'Please type a message before sending.' };
  if (trimmed.length > 8000) return { ok: false, status: 400, error: 'Please shorten the message to 8,000 characters or less.' };
  const target = await findLiveSession(sessionId, errors);
  if (!target) return { ok: false, status: 404, error: 'That terminal is not connected right now.' };
  const message = '[From Michael via dashboard] ' + trimmed;
  // Idle terminal: type the message in directly so the agent starts replying
  // now. Busy terminal: queue it so it is injected at the next safe point in
  // the ongoing work (typing into a busy terminal would also just queue).
  if (!target.session.is_processing && !urgent) {
    const typed = await sendTranscript(target.pipe, message, sessionId);
    if (typed.ok) return { ok: true, delivered: 'typed', urgent: false };
    // Fall through to the queue if the terminal client is not attached.
  }
  const command = (urgent ? 'queue_interrupt_urgent' : 'queue_interrupt') + ':' + message;
  const result = await sendDebug(target.pipe, command, sessionId);
  if (result.error) return { ok: false, status: 502, error: 'That terminal did not accept the message. Please try again.' };
  return { ok: true, delivered: 'queued', urgent: Boolean(urgent) };
}

function sanitizeFilename(name) {
  const base = path.basename(String(name || '')).replace(/[^A-Za-z0-9._ -]/g, '_').slice(0, 100).trim();
  return base || null;
}

async function uniqueInboxPath(filename) {
  const day = new Date().toISOString().slice(0, 10);
  const dir = path.join(JC, 'dashboard-inbox', day);
  await fsp.mkdir(dir, { recursive: true });
  const ext = path.extname(filename);
  const stem = path.basename(filename, ext) || 'file';
  for (let i = 1; i < 1000; i++) {
    const name = i === 1 ? filename : `${stem}-${i}${ext}`;
    const full = path.join(dir, name);
    if (!(await exists(full))) return full;
  }
  throw new Error('Could not save the file with a unique name.');
}

async function saveDashboardFile(body, errors) {
  if (!validateSessionId(body.session_id)) return { ok: false, status: 400, error: 'Please choose a valid live terminal.' };
  const filename = sanitizeFilename(body.filename);
  if (!filename) return { ok: false, status: 400, error: 'Please choose a file with a usable name.' };
  if (typeof body.content_base64 !== 'string') return { ok: false, status: 400, error: 'Please choose a file to share.' };
  const clean64 = body.content_base64.replace(/^data:[^,]*,/, '');
  let data;
  try { data = Buffer.from(clean64, 'base64'); }
  catch { return { ok: false, status: 400, error: 'The file could not be read.' }; }
  if (data.length > 10 * 1024 * 1024) return { ok: false, status: 413, error: 'Please choose a file smaller than 10 MB.' };
  const target = await findLiveSession(body.session_id, errors);
  if (!target) return { ok: false, status: 404, error: 'That terminal is not connected right now.' };
  const full = await uniqueInboxPath(filename);
  await fsp.writeFile(full, data);
  const note = typeof body.note === 'string' && body.note.trim() ? ` ${body.note.trim().slice(0, 1000)}.` : '';
  const msg = `I shared a file with you: ${full}.${note} Please read it and take it into account.`;
  const queued = await queueDashboardMessage(body.session_id, msg, body.urgent, errors);
  if (!queued.ok) return queued;
  return { ok: true, path: full };
}

function activityForSession(s) {
  if (!s.live) return { state: 'offline', headline: 'Offline - saved from an earlier session' };
  if (s.is_processing) {
    const todo = (s.todos || []).find(t => String(t.status).toLowerCase() === 'in_progress');
    return { state: 'working', headline: `Working: ${todo?.content || s.detail || 'thinking'}` };
  }
  return { state: 'idle', headline: 'Idle - waiting for input' };
}

function textFromMessage(m) {
  const c = m.content;
  if (Array.isArray(c)) return c.map(x => typeof x === 'string' ? x : (x && typeof x.text === 'string' ? x.text : '')).join(' ');
  if (typeof c === 'string') return c;
  if (m.name) return `[used tool: ${m.name}]`;
  return typeof m.text === 'string' ? m.text : '';
}
function reduceHistory(raw) {
  const arr = Array.isArray(raw) ? raw : [];
  return arr.slice(-6).map(m => {
    if (m && (m.role === 'tool' || m.type === 'tool_call' || m.type === 'tool_result' || m.tool_name)) return { role: 'tool', text: `[used tool: ${m.name || m.tool_name || 'tool'}]` };
    let text = textFromMessage(m || {}).replace(/<system-reminder>[\s\S]*?<\/system-reminder>/g, '').trim();
    if (text.length > 600) text = text.slice(0, 599) + '…';
    return { role: m.role || 'agent', text };
  }).filter(m => m.text);
}
async function sessionHistory(sessionId, errors) {
  if (!validateSessionId(sessionId)) return { ok: false, status: 400, error: 'Please choose a valid live terminal.' };
  const target = await findLiveSession(sessionId, errors);
  if (!target) return { ok: false, status: 404, error: 'That terminal is not connected right now.' };
  const res = await sendDebug(target.pipe, 'history', sessionId);
  if (res.error) return { ok: false, status: 502, error: 'Could not read that conversation right now.' };
  return { ok: true, messages: reduceHistory(parseOutput(res.output)) };
}

async function loadRegistryProjectDirs(errors) {
  const reg = await readJson(path.join(JC, 'servers.json'), errors) || {};
  const dirs = [];
  for (const srv of Object.values(reg)) {
    if (srv && typeof srv.working_dir === 'string') dirs.push(srv.working_dir);
    if (srv && Array.isArray(srv.sessions)) {
      for (const s of srv.sessions) if (s && typeof s.working_dir === 'string') dirs.push(s.working_dir);
    }
  }
  return dirs;
}

async function projectDirs() {
  const errors = [];
  const live = await loadLiveSessions(errors);
  const candidates = [
    ...live.sessions.map(s => s.working_dir).filter(Boolean),
    ...await loadRegistryProjectDirs(errors),
    path.join(os.homedir(), 'dev', 'jcode'),
    path.join(os.homedir(), 'Developer', 'healthview-app'),
    path.join(os.homedir(), 'Developer', 'healthview-platform'),
  ];
  const seen = new Set();
  const projects = [];
  for (const dir of candidates) {
    const full = path.resolve(String(dir));
    const key = full.toLowerCase();
    if (seen.has(key) || !(await isDirectory(full))) continue;
    seen.add(key);
    projects.push({ name: path.basename(full), dir: full });
  }
  return { ok: true, projects, errors };
}

async function state() {
  const errors = [];
  const [knowledge, live, initiatives] = await Promise.all([loadKnowledge(errors), loadLiveSessions(errors), loadInitiatives(errors)]);
  errors.push(...live.errors.map(e => `debug ${e}`));
  const seen = new Set(live.sessions.map(s => s.session_id));
  const sessions = [];
  for (const s of live.sessions) {
    const full = { ...s, ...await loadTodoBundle(s.session_id, errors) };
    sessions.push({ ...full, activity: activityForSession(full) });
  }
  for (const rec of await recentTodoIds(errors)) if (!seen.has(rec.id)) {
    const full = { session_id: rec.id, friendly_name: rec.id, status: 'offline', live: false, working_memory: null, stm_read_at: null, todos_mtime: rec.mtime, ...await loadTodoBundle(rec.id, errors) };
    sessions.push({ ...full, activity: activityForSession(full) });
  }
  return { generated_at: new Date().toISOString(), rss_mb: Math.round(process.memoryUsage().rss / 1024 / 1024 * 10) / 10, knowledge, sessions, initiatives, errors, staleness: { knowledge_mtime: await latestMtime(path.join(JC, 'knowledge', 'projects')), todos_mtime: await latestMtime(path.join(JC, 'todos')), goals_mtime: await latestMtime(path.join(JC, 'goals')) }, servers: live.servers };
}

const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url, `http://${req.headers.host}`);
    if (rejectBadOrigin(req, res)) return;
    if (url.pathname === '/') {
      if (req.method !== 'GET') { res.writeHead(405); return res.end('method not allowed'); }
      const html = await fsp.readFile(HTML, { encoding: 'utf8', flag: 'r' });
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
      return res.end(html);
    }
    if (url.pathname === '/api/state') {
      if (req.method !== 'GET') { res.writeHead(405); return res.end('method not allowed'); }
      res.writeHead(200, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' });
      return res.end(JSON.stringify(await state()));
    }
    if (url.pathname === '/api/projects') {
      if (req.method !== 'GET') { res.writeHead(405); return res.end('method not allowed'); }
      res.writeHead(200, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' });
      return res.end(JSON.stringify(await projectDirs()));
    }
    if (url.pathname === '/api/session/message') {
      if (req.method !== 'POST') { res.writeHead(405); return res.end('method not allowed'); }
      const body = await parseJsonBody(req, res, 16384);
      if (!body) return;
      const result = await queueDashboardMessage(body.session_id, body.text, body.urgent, []);
      if (!result.ok) return plainError(res, result.status || 400, result.error);
      return json(res, 200, result);
    }
    if (url.pathname === '/api/session/file') {
      if (req.method !== 'POST') { res.writeHead(405); return res.end('method not allowed'); }
      const body = await parseJsonBody(req, res, 16 * 1024 * 1024);
      if (!body) return;
      const result = await saveDashboardFile(body, []);
      if (!result.ok) return plainError(res, result.status || 400, result.error);
      return json(res, 200, result);
    }
    if (url.pathname === '/api/session/history') {
      if (req.method !== 'POST') { res.writeHead(405); return res.end('method not allowed'); }
      const body = await parseJsonBody(req, res, 4096);
      if (!body) return;
      const result = await sessionHistory(body.session_id, []);
      if (!result.ok) return plainError(res, result.status || 400, result.error);
      return json(res, 200, result);
    }
    if (url.pathname === '/api/session/new') {
      if (req.method !== 'POST') { res.writeHead(405); return res.end('method not allowed'); }
      let body;
      try { body = JSON.parse(await readBody(req) || '{}'); }
      catch { res.writeHead(400, { 'content-type': 'application/json; charset=utf-8' }); return res.end(JSON.stringify({ ok: false, error: 'Please choose a valid project folder.' })); }
      const dir = typeof body.dir === 'string' ? path.resolve(body.dir) : '';
      if (!dir || !(await isDirectory(dir))) {
        res.writeHead(400, { 'content-type': 'application/json; charset=utf-8' });
        return res.end(JSON.stringify({ ok: false, error: 'That folder does not exist on this computer.' }));
      }
      const name = path.basename(dir).replace(/[&|<>^"%]/g, '').slice(0, 60) || 'jcode';
      spawn('cmd.exe', ['/c', 'start', `jcode in ${name}`, 'cmd', '/k', 'jcode'], { cwd: dir, detached: true, stdio: 'ignore' }).unref();
      res.writeHead(200, { 'content-type': 'application/json; charset=utf-8' });
      return res.end(JSON.stringify({ ok: true }));
    }
    res.writeHead(404); res.end('not found');
  } catch (e) { res.writeHead(500, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: e.message })); }
});
server.listen(PORT, HOST, () => console.log(`JCODE MIND http://${HOST}:${PORT}`));
