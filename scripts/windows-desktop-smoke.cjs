// Opt-in live smoke: creates a fresh project and desktop thread, then runs a real Probe.
// The diagnostic MCP client supplies that new thread's metadata; no model ack is simulated.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const net = require('node:net');
const { randomUUID } = require('node:crypto');
const { spawn, execFile } = require('node:child_process');
const { createInterface } = require('node:readline');
const { promisify } = require('node:util');

const execute = promisify(execFile);
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const log = (event, details = {}) => console.log(JSON.stringify({ event, ...details }));

function options() {
  const values = { '--bin-dir': path.resolve(__dirname, '../target/debug') };
  for (let i = 2; i < process.argv.length; i += 2) {
    const key = process.argv[i];
    if (!['--bin-dir', '--codex-core', '--artifact-parent'].includes(key) || !process.argv[i + 1]) {
      throw new Error('Usage: node scripts/windows-desktop-smoke.cjs --codex-core <codex.exe> --artifact-parent <existing-directory> [--bin-dir <directory>]');
    }
    values[key] = path.resolve(process.argv[i + 1]);
  }
  if (process.platform !== 'win32') throw new Error('This smoke requires the verified Windows desktop distribution');
  for (const key of ['--bin-dir', '--artifact-parent']) {
    if (!values[key] || !fs.statSync(values[key]).isDirectory()) throw new Error(`${key} must name an existing directory`);
  }
  if (!values['--codex-core'] || !fs.statSync(values['--codex-core']).isFile()) throw new Error('--codex-core must name the actual desktop core executable');
  return values;
}

function start(program, args, cwd) {
  const env = { ...process.env, RUST_LOG: 'error' };
  delete env.CODEX_THREAD_ID;
  delete env.CLAUDE_CODE_SESSION_ID;
  const child = spawn(program, args, { cwd, env, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
  child.stderr.on('data', () => {});
  return child;
}

async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.stdin?.end();
  const exited = new Promise(resolve => child.once('exit', resolve));
  const timer = setTimeout(() => child.kill(), 3000);
  try { await exited; } finally { clearTimeout(timer); }
}

class StdioRpc {
  constructor(child) {
    this.child = child;
    this.pending = new Map();
    this.nextId = 1;
    this.lines = createInterface({ input: child.stdout });
    this.lines.on('line', line => {
      let message;
      try { message = JSON.parse(line); } catch { return this.fail(new Error('RPC stdout was not JSON')); }
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      clearTimeout(pending.timer);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
    });
    child.stdin.on('error', error => this.fail(error));
    child.once('error', error => this.fail(error));
    child.once('exit', () => this.fail(new Error('RPC subprocess exited')));
  }
  fail(error) {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
  }
  request(method, params = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`RPC timeout: ${method}`));
      }, 30000);
      this.pending.set(id, { resolve, reject, timer });
      this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }
  notify(method) { this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params: {} }) + '\n'); }
  async close() { await stop(this.child); this.lines.close(); }
}

class DesktopObserver {
  constructor() {
    this.clientId = 'runspur-smoke-initializing';
    this.pending = new Map();
    this.buffer = Buffer.alloc(0);
    this.socket = net.createConnection('\\\\.\\pipe\\codex-ipc');
    this.connected = new Promise((resolve, reject) => {
      this.socket.once('connect', resolve);
      this.socket.once('error', reject);
    });
    this.socket.on('data', chunk => this.receive(chunk));
    this.socket.on('error', error => this.fail(error));
    this.socket.on('close', () => this.fail(new Error('Desktop observer disconnected')));
  }
  fail(error) {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
  }
  write(message) {
    const body = Buffer.from(JSON.stringify(message));
    const prefix = Buffer.alloc(4);
    prefix.writeUInt32LE(body.length);
    this.socket.write(Buffer.concat([prefix, body]));
  }
  receive(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    while (this.buffer.length >= 4) {
      const length = this.buffer.readUInt32LE(0);
      if (!length || length > 16 * 1024 * 1024) {
        this.socket.destroy(new Error('Invalid desktop frame length'));
        return;
      }
      if (this.buffer.length < length + 4) return;
      let message;
      try { message = JSON.parse(this.buffer.subarray(4, length + 4)); }
      catch { this.socket.destroy(new Error('Invalid desktop JSON')); return; }
      this.buffer = this.buffer.subarray(length + 4);
      if (message.type === 'client-discovery-request') {
        this.write({ type: 'client-discovery-response', requestId: message.requestId, response: { canHandle: false } });
      }
      const pending = this.pending.get(message.requestId);
      if (message.type !== 'response' || !pending) continue;
      this.pending.delete(message.requestId);
      clearTimeout(pending.timer);
      if (message.resultType === 'error') pending.reject(new Error(String(message.error)));
      else pending.resolve(message);
    }
  }
  request(method, params, version = 1) {
    const requestId = randomUUID();
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        reject(new Error(`Desktop observer timeout: ${method}`));
      }, 12000);
      this.pending.set(requestId, { resolve, reject, timer });
      this.write({ type: 'request', requestId, sourceClientId: this.clientId, method, params, version, timeoutMs: 10000 });
    });
  }
  async initialize() {
    await this.connected;
    const response = await this.request('initialize', { clientType: 'cli' }, 0);
    this.clientId = response.result.clientId;
  }
  async owner(threadId) {
    const response = await this.request('thread-owner-discovery', { hostId: 'local', conversationId: threadId });
    assert.equal(response.result.supportsUntrustedAppInput, true);
    assert.ok(response.handledByClientId);
    return response.handledByClientId;
  }
  close() { this.fail(new Error('Observer closed')); this.socket.destroy(); }
}

async function main() {
  const args = options();
  const root = fs.mkdtempSync(path.join(args['--artifact-parent'], 'runspur-windows-smoke-'));
  const project = path.join(root, 'project 项目');
  const data = path.join(root, 'state');
  fs.mkdirSync(project);
  const binary = name => path.join(args['--bin-dir'], `${name}.exe`);
  for (const name of ['schedulerd', 'schedulerctl', 'scheduler-mcp']) assert.ok(fs.statSync(binary(name)).isFile());
  const report = { started_at: new Date().toISOString(), artifact_root: root, checks: {},
    metadata_source: 'diagnostic client, using only the newly created desktop thread',
    parent_ack: 'not_obtained', model_consumption: 'not_verified' };
  const record = (name, details = true) => { report.checks[name] = details; log(name, typeof details === 'object' ? details : {}); };
  const control = async (...parameters) => {
    const result = await execute(binary('schedulerctl'), ['--data-dir', data, ...parameters], {
      cwd: project, windowsHide: true, timeout: 10000, maxBuffer: 2 * 1024 * 1024,
    });
    return JSON.parse(result.stdout);
  };
  const daemon = () => {
    const child = start(binary('schedulerd'), ['--data-dir', data], project);
    child.stdin.end();
    child.stdout.resume();
    return child;
  };
  const core = async () => {
    const rpc = new StdioRpc(start(args['--codex-core'], ['app-server'], project));
    try {
      await rpc.request('initialize', { clientInfo: { name: 'runspur_windows_smoke', version: '0.1.0' }, capabilities: { experimentalApi: true } });
      rpc.notify('initialized');
      return rpc;
    } catch (error) { await rpc.close(); throw error; }
  };
  const healthy = async () => {
    const deadline = Date.now() + 15000;
    let last;
    do {
      try { return await control('doctor'); }
      catch (error) { last = error; await delay(100); }
    } while (Date.now() < deadline);
    throw last;
  };
  let service;
  let creator;
  let mcp;
  let observer;
  let reader;
  try {
    service = daemon();
    record('daemon_health', (await healthy()).service);
    const saved = await control('init', '--project-root', project, '--name', 'Runspur Windows smoke');
    report.project_id = saved.config.project_id;
    record('project_initialized');
    const installation = await control('integrate', '--project-root', project, '--mcp-program', binary('scheduler-mcp'));
    assert.equal(installation.state, 'saved_requires_host_reload');
    assert.equal(installation.session_id_in_config, false);
    assert.ok(fs.readFileSync(path.join(project, '.agents/skills/agent-scheduler/SKILL.md'), 'utf8').includes('scheduler_submit'));
    record('project_skill_and_mcp_installed');

    creator = await core();
    const created = await creator.request('thread/start', { cwd: project, sandbox: 'read-only', approvalPolicy: 'on-request', ephemeral: false });
    const threadId = created.thread.id;
    assert.match(threadId, /^[0-9a-f-]{36}$/i);
    report.thread_id = threadId;
    await creator.request('thread/name/set', { threadId, name: `Runspur Windows smoke ${path.basename(root)}` });
    await creator.request('thread/unsubscribe', { threadId });
    await creator.close();
    creator = null;
    // The creation server is gone before the existing desktop loads the test thread.
    await execute('powershell.exe', ['-NoLogo', '-NoProfile', '-NonInteractive', '-Command', `Start-Process "codex://threads/${threadId}"`], { windowsHide: true, timeout: 15000 });
    observer = new DesktopObserver();
    await observer.initialize();
    const ownerDeadline = Date.now() + 45000;
    while (!report.owner_client_id) {
      try { report.owner_client_id = await observer.owner(threadId); }
      catch (error) { if (Date.now() >= ownerDeadline) throw error; await delay(500); }
    }
    record('desktop_exact_owner', { thread_id: threadId, owner_client_id: report.owner_client_id });

    mcp = new StdioRpc(start(binary('scheduler-mcp'), ['--project-root', project, '--data-dir', data], project));
    await mcp.request('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'runspur_windows_smoke', version: '0.1.0' } });
    mcp.notify('notifications/initialized');
    const tools = await mcp.request('tools/list');
    assert.equal(tools.tools.length, 7);
    const missing = await mcp.request('tools/call', { name: 'scheduler_context', arguments: {} });
    assert.equal(missing.structuredContent.code, 'origin_session_unavailable');
    const tool = async (name, parameters = {}) => {
      const result = await mcp.request('tools/call', { name, arguments: parameters, _meta: { threadId } });
      if (result.isError) throw new Error(`${name}: ${JSON.stringify(result.structuredContent)}`);
      return result.structuredContent;
    };
    const context = await tool('scheduler_context');
    assert.equal(context.project.config.project_id, report.project_id);
    assert.equal(context.origin.route.session_id, threadId);
    record('mcp_automatic_origin', { missing_metadata_rejected: true });

    const marker = `RUNSPUR_SMOKE_${randomUUID()}`;
    const gate = path.join(project, 'release-probe');
    const parameters = { worker_profile_id: 'probe', prompt: JSON.stringify({ summary: marker, wait_for_file: gate, delay_ms: 1, attempt_control: data }),
      idempotency_key: 'live-smoke', timeout_seconds: 30 };
    const accepted = await tool('scheduler_submit', parameters);
    report.task_id = accepted.task_id;
    const duplicate = await tool('scheduler_submit', parameters);
    assert.equal(duplicate.task_id, accepted.task_id);
    assert.equal(duplicate.deduplicated, true);
    const before = await tool('scheduler_task', { task_id: accepted.task_id });
    assert.equal(before.result, null);
    assert.equal(accepted.result_pending, true);
    record('nonblocking_durable_submission', { task_id: accepted.task_id, deduplicated: true });
    fs.writeFileSync(gate, 'release', { flag: 'wx' });
    let task;
    const deliveryDeadline = Date.now() + 60000;
    do {
      task = await control('status', accepted.task_id);
      if (task.delivery_state === 'accepted_by_transport') break;
      if (['uncertain', 'refused'].includes(task.delivery_state)) throw new Error(`Delivery stopped: ${task.delivery_state}: ${task.delivery_error}`);
      await delay(250);
    } while (Date.now() < deliveryDeadline);
    assert.equal(task.execution_state, 'succeeded');
    assert.equal(task.result.summary, marker);
    assert.ok(task.result.verification.some(v => v.description === 'worker_control_plane_access' && v.outcome === 'denied'));
    assert.equal(task.delivery_state, 'accepted_by_transport', task.delivery_error);
    assert.ok(task.parent_turn_id);
    report.delivery_id = task.delivery_id;
    report.parent_turn_id = task.parent_turn_id;
    record('real_worker_and_active_desktop_push', { execution_state: task.execution_state, delivery_state: task.delivery_state,
      parent_turn_id: task.parent_turn_id, worker_dispatch_denied: true });

    reader = await core();
    let turn;
    let item;
    const historyDeadline = Date.now() + 15000;
    do {
      const history = await reader.request('thread/turns/list', { threadId, limit: 5, sortDirection: 'desc', itemsView: 'full' });
      turn = history.data.find(value => value.id === task.parent_turn_id);
      item = turn?.items.find(value => value.type === 'functionCallOutput' && value.name === 'runspur_worker_result');
      // A turn header can be visible before its persisted items. Wait for the correlated output as well.
      if (item && ['completed', 'failed', 'interrupted'].includes(turn.status)) break;
      await delay(250);
    } while (Date.now() < historyDeadline);
    assert.ok(turn, 'The accepted turn must exist in the original thread');
    assert.ok(item, 'Persisted result must keep its tool-output identity');
    const output = JSON.parse(item.output);
    assert.equal(output.task_id, task.task_id);
    assert.equal(output.delivery_id, task.delivery_id);
    assert.equal(output.result.summary, marker);
    assert.equal(output.project_id, report.project_id);
    assert.ok(output.ack_nonce);
    report.model_error = turn.error?.codexErrorInfo ?? null;
    record('original_thread_tool_output', { item_type: item.type, name: item.name, turn_status: turn.status, model_error: report.model_error });
    await reader.close();
    reader = null;
    await mcp.close();
    mcp = null;

    // Only the daemon launched by this script is stopped. Accepted results must survive restart without re-sending.
    service.kill();
    await stop(service);
    service = daemon();
    await healthy();
    const recovered = await control('status', task.task_id);
    assert.equal(recovered.delivery_state, 'accepted_by_transport');
    assert.equal(recovered.parent_turn_id, task.parent_turn_id);
    assert.equal(recovered.delivery_id, task.delivery_id);
    record('accepted_delivery_survives_daemon_restart');
    report.result = 'passed_transport_and_worker';
  } catch (error) {
    report.result = 'failed';
    report.error = error.message;
    process.exitCode = 1;
  } finally {
    observer?.close();
    for (const rpc of [reader, mcp, creator]) if (rpc) await rpc.close();
    if (service) { service.kill(); await stop(service); }
    report.finished_at = new Date().toISOString();
    fs.writeFileSync(path.join(root, 'report.json'), JSON.stringify(report, null, 2), { flag: 'wx' });
    log('smoke_report', report);
  }
}

main().catch(error => { log('smoke_error', { message: error.message }); process.exitCode = 1; });
