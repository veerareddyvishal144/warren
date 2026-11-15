/* Warren dashboard. Vanilla JavaScript, no build step, no external resources.
 *
 * Every Sandbox API call goes to window.WARREN.apiUrl with
 * `Authorization: Bearer <key>`; calls to an instance (chat, sessions, files)
 * go to the instance URL with `X-Warren-Key: <key>`. The key lives in
 * localStorage only.
 */
(() => {
  'use strict';

  // ---------------------------------------------------------------- config
  const W = Object.assign(
    {
      baseDomain: location.hostname,
      publicScheme: location.protocol.replace(':', ''),
      publicPort: 0,
      apiUrl: location.origin + '/v1',
      version: '',
    },
    window.WARREN || {}
  );
  const API = String(W.apiUrl).replace(/\/+$/, '');
  const KEY_STORAGE = 'warren.apiKey';
  const POLL_MS = 5000;
  const TERMINAL_PORT = 7681;
  const TABS = ['exec', 'chat', 'sessions', 'files', 'logs', 'metrics', 'backups'];
  const AGENT_TABS = ['chat', 'sessions'];
  const DEFAULT_RATES = { vcpu_month_micros: 800000, ram_gb_month_micros: 700000, disk_gb_month_micros: 90000, vm_multiplier: 2.5 };
  const BUSY = ['provisioning', 'starting', 'restarting', 'updating', 'stopping', 'waking', 'deleting'];

  const state = {
    key: null,
    workspace: null,
    health: null,
    billing: null,
    templates: [],
    instances: [],
    selectedId: null,
    selected: null,
    view: 'instances', // instances | billing
    panel: 'empty', // empty | create | detail
    tab: 'exec',
    listSig: '',
    pollTimer: null,
    chat: { sessionId: null, controller: null },
    files: { path: '.', open: null, pending: null },
    sessions: { openId: null },
  };

  // ------------------------------------------------------------ DOM helpers
  const el = (id) => document.getElementById(id);

  function h(tag, attrs, ...children) {
    const node = document.createElement(tag);
    if (attrs) {
      for (const [k, v] of Object.entries(attrs)) {
        if (v == null || v === false) continue;
        if (k === 'class') node.className = v;
        else if (k === 'text') node.textContent = v;
        else if (k.startsWith('on') && typeof v === 'function') node.addEventListener(k.slice(2), v);
        else node.setAttribute(k, v === true ? '' : v);
      }
    }
    for (const c of children.flat()) {
      if (c == null || c === false) continue;
      node.append(c instanceof Node ? c : String(c));
    }
    return node;
  }

  function svg(tag, attrs) {
    const node = document.createElementNS('http://www.w3.org/2000/svg', tag);
    for (const [k, v] of Object.entries(attrs || {})) node.setAttribute(k, v);
    return node;
  }

  function clear(node) {
    while (node.firstChild) node.removeChild(node.firstChild);
  }

  function show(node, visible) {
    node.hidden = !visible;
  }

  // ------------------------------------------------------------- formatting
  function fmtUsd(micros) {
    const v = (Number(micros) || 0) / 1e6;
    const abs = Math.abs(v);
    const digits = abs !== 0 && abs < 0.01 ? 6 : 2;
    return (v < 0 ? '-' : '') + '$' + abs.toFixed(digits);
  }

  function fmtBytes(n) {
    n = Number(n) || 0;
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let i = 0;
    while (n >= 1024 && i < units.length - 1) {
      n /= 1024;
      i += 1;
    }
    return (i === 0 ? String(n) : n.toFixed(1)) + ' ' + units[i];
  }

  function fmtTime(secs) {
    return secs ? new Date(Number(secs) * 1000).toLocaleString() : '—';
  }

  function fmtTimeMs(ms) {
    return ms ? new Date(Number(ms)).toLocaleString() : '—';
  }

  function fmtDuration(secs) {
    secs = Number(secs) || 0;
    if (secs % 3600 === 0) return secs / 3600 + ' h';
    if (secs % 60 === 0) return secs / 60 + ' min';
    return secs + ' s';
  }

  function shapeText(s) {
    return s ? `${s.cpu} vCPU · ${s.memory} GB RAM · ${s.disk} GB disk` : '—';
  }

  function label(inst) {
    return inst.name ? `${inst.name} (${inst.id})` : inst.id;
  }

  // ------------------------------------------------------------------ toast
  let toastTimer = null;
  function toast(message, kind) {
    const t = el('toast');
    t.textContent = message;
    t.className = 'toast ' + (kind || 'info');
    show(t, true);
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => show(t, false), kind === 'error' ? 9000 : 4000);
  }

  function errText(e) {
    if (e instanceof ApiError) return (e.code ? e.code + ': ' : '') + e.message;
    return e && e.message ? e.message : String(e);
  }

  function toastError(e) {
    console.error(e);
    toast(errText(e), 'error');
  }

  // ------------------------------------------------------------- API client
  class ApiError extends Error {
    constructor(code, message, status, details) {
      super(message);
      this.name = 'ApiError';
      this.code = code;
      this.status = status;
      this.details = details;
    }
  }

  async function errorFrom(res) {
    const text = await res.text();
    let data = null;
    try {
      data = JSON.parse(text);
    } catch (_) {
      data = null;
    }
    const e = data && data.error ? data.error : {};
    return new ApiError(e.code || 'http_' + res.status, e.message || text || res.statusText || 'Request failed', res.status, e.details);
  }

  /** Call the Sandbox API. `path` starts with a slash and is relative to apiUrl. */
  async function api(method, path, body, opts) {
    opts = opts || {};
    const headers = { Accept: 'application/json' };
    if (opts.auth !== false && state.key) headers.Authorization = 'Bearer ' + state.key;
    const init = { method, headers };
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }
    let res;
    try {
      res = await fetch(API + path, init);
    } catch (err) {
      throw new ApiError('network', `Cannot reach the API at ${API} (${err.message})`, 0);
    }
    if (!res.ok) {
      const err = await errorFrom(res);
      if (err.status === 401 && opts.auth !== false) onUnauthorized(err);
      throw err;
    }
    if (res.status === 204) return null;
    const text = await res.text();
    return text ? JSON.parse(text) : null;
  }

  function instanceUrl(inst, path) {
    return String(inst.url).replace(/\/+$/, '') + path;
  }

  /** Fetch from an instance URL through the edge with the workspace key. */
  async function instFetch(inst, path, init) {
    init = Object.assign({}, init || {});
    init.headers = Object.assign({ 'X-Warren-Key': state.key }, init.headers || {});
    let res;
    try {
      res = await fetch(instanceUrl(inst, path), init);
    } catch (err) {
      if (err.name === 'AbortError') throw err;
      throw new ApiError('network', `Cannot reach ${inst.url} (${err.message})`, 0);
    }
    if (!res.ok) throw await errorFrom(res);
    return res;
  }

  async function instJson(inst, method, path, body) {
    const init = { method, headers: { Accept: 'application/json' } };
    if (body !== undefined) {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }
    const res = await instFetch(inst, path, init);
    const text = await res.text();
    return text ? JSON.parse(text) : null;
  }

  // ------------------------------------------------------------------- auth
  function setKey(key) {
    state.key = key || null;
    if (key) localStorage.setItem(KEY_STORAGE, key);
    else localStorage.removeItem(KEY_STORAGE);
  }

  function showLogin(error) {
    stopPolling();
    show(el('login'), true);
    show(el('main'), false);
    show(el('nav'), false);
    show(el('who'), false);
    const err = el('login-error');
    err.textContent = error ? errText(error) : '';
    show(err, Boolean(error));
    if (state.key) el('login-key').value = state.key;
  }

  function showApp() {
    show(el('login'), false);
    show(el('main'), true);
    show(el('nav'), true);
    show(el('who'), true);
    const ws = state.workspace || {};
    el('workspace-name').textContent = ws.name || ws.id || '';
    el('key-prefix').textContent = state.key ? state.key.slice(0, 14) + '…' : '';
  }

  async function enter() {
    state.workspace = await api('GET', '/workspace');
    showApp();
    showView('instances');
    await Promise.all([loadTemplates(), refreshInstances(), loadBilling(false)]);
    startPolling();
  }

  function onUnauthorized(err) {
    if (!state.key) return;
    setKey(null);
    state.workspace = null;
    showLogin(err);
  }

  async function onLogin(ev) {
    ev.preventDefault();
    const key = el('login-key').value.trim();
    if (!key) return;
    setKey(key);
    try {
      await enter();
    } catch (e) {
      setKey(null);
      showLogin(e);
      el('login-key').value = key;
    }
  }

  async function onSignup(ev) {
    ev.preventDefault();
    const name = el('signup-name').value.trim();
    const btn = ev.submitter || null;
    if (btn) btn.disabled = true;
    try {
      const r = await api('POST', '/signup', name ? { name } : {}, { auth: false });
      setKey(r.api_key);
      el('signup-key').textContent = r.api_key;
      el('signup-wallet').textContent = `Workspace ${r.workspace && r.workspace.id ? r.workspace.id : ''} · starting balance ${fmtUsd(r.wallet_micros)}.`;
      show(el('signup-result'), true);
      show(el('login-error'), false);
      el('signup-continue').focus();
    } catch (e) {
      showLogin(e);
    } finally {
      if (btn) btn.disabled = false;
    }
  }

  function logout() {
    setKey(null);
    state.workspace = null;
    state.instances = [];
    state.selectedId = null;
    state.selected = null;
    state.listSig = '';
    show(el('signup-result'), false);
    el('login-key').value = '';
    showLogin(null);
  }

  async function loadHealth() {
    try {
      state.health = await api('GET', '/health', undefined, { auth: false });
      const hc = state.health || {};
      const parts = ['warrend ' + (hc.version || W.version || '')];
      if (Array.isArray(hc.runtimes) && hc.runtimes.length) parts.push(hc.runtimes.join(', '));
      if (hc.dev_mode) parts.push('dev mode');
      el('server-info').textContent = parts.join(' · ');
    } catch (e) {
      el('server-info').textContent = W.version ? 'warrend ' + W.version : '';
    }
    updateTypeOptions();
    show(el('topup-section'), Boolean(state.health && state.health.dev_mode));
  }

  // ------------------------------------------------------------- navigation
  function showView(view) {
    state.view = view;
    el('nav-instances').setAttribute('aria-current', view === 'instances' ? 'page' : 'false');
    el('nav-billing').setAttribute('aria-current', view === 'billing' ? 'page' : 'false');
    show(el('billing-panel'), view === 'billing');
    if (view === 'billing') {
      show(el('empty-panel'), false);
      show(el('create-panel'), false);
      show(el('detail-panel'), false);
      loadBilling(true);
    } else {
      showPanel(state.panel);
    }
  }

  function showPanel(panel) {
    state.panel = panel;
    if (state.view !== 'instances') {
      state.view = 'instances';
      el('nav-instances').setAttribute('aria-current', 'page');
      el('nav-billing').setAttribute('aria-current', 'false');
      show(el('billing-panel'), false);
    }
    show(el('empty-panel'), panel === 'empty');
    show(el('create-panel'), panel === 'create');
    show(el('detail-panel'), panel === 'detail');
    if (panel === 'create') {
      updatePrice();
      el('create-template').focus();
    }
  }

  // ---------------------------------------------------------- instance list
  async function refreshInstances() {
    const r = await api('GET', '/instances');
    state.instances = Array.isArray(r && r.data) ? r.data : [];
    if (state.selectedId) {
      const cur = state.instances.find((i) => i.id === state.selectedId);
      if (cur) {
        state.selected = cur;
        if (state.panel === 'detail') renderDetail(cur);
      } else if (state.panel === 'detail') {
        state.selectedId = null;
        state.selected = null;
        showPanel('empty');
        toast('The selected instance is gone.');
      }
    }
    renderList();
  }

  function badge(status) {
    return h('span', { class: 'badge st-' + status, text: status });
  }

  function renderList() {
    const list = el('instance-list');
    const sig = JSON.stringify(state.instances.map((i) => [i.id, i.name, i.status, i.template_name])) + '|' + state.selectedId;
    if (sig === state.listSig) return;
    state.listSig = sig;
    clear(list);
    for (const inst of state.instances) {
      const selected = inst.id === state.selectedId;
      list.append(
        h(
          'li',
          null,
          h(
            'button',
            { type: 'button', class: 'inst' + (selected ? ' selected' : ''), 'aria-current': selected ? 'true' : null, onclick: () => selectInstance(inst.id) },
            h('span', { class: 'inst-name', text: inst.name || inst.id }),
            h('span', { class: 'inst-meta' }, h('code', { text: inst.id }), ' · ', inst.template_name || inst.template, ' ', badge(inst.status))
          )
        )
      );
    }
    el('list-status').textContent = state.instances.length ? '' : 'No instances yet. Create one to get started.';
  }

  function startPolling() {
    stopPolling();
    state.pollTimer = setInterval(poll, POLL_MS);
  }

  function stopPolling() {
    if (state.pollTimer) clearInterval(state.pollTimer);
    state.pollTimer = null;
  }

  let pollFailures = 0;
  async function poll() {
    if (document.hidden || !state.key) return;
    try {
      await refreshInstances();
      if (state.view === 'billing') await loadBilling(false);
      pollFailures = 0;
    } catch (e) {
      pollFailures += 1;
      if (pollFailures === 1) toastError(e);
    }
  }

  // ------------------------------------------------------------ create form
  async function loadTemplates() {
    const r = await api('GET', '/templates');
    state.templates = Array.isArray(r && r.data) ? r.data : [];
    const sel = el('create-template');
    clear(sel);
    for (const t of state.templates) {
      sel.append(h('option', { value: t.name, text: `${t.name}${t.agent ? ' (agent)' : ''}${t.scope === 'workspace' ? ' · workspace' : ''}` }));
    }
    onTemplateChange();
  }

  function selectedTemplate() {
    return state.templates.find((t) => t.name === el('create-template').value) || null;
  }

  function onTemplateChange() {
    const t = selectedTemplate();
    el('create-template-desc').textContent = t ? `${t.description || ''} Image ${t.image_ref}${t.agent ? ' · serves the Agent API' : ''}.`.trim() : '';
    if (t && t.default_shape) {
      const want = `${t.default_shape.cpu},${t.default_shape.memory},${t.default_shape.disk}`;
      const shape = el('create-shape');
      for (const o of shape.options) {
        if (o.value === want) shape.value = want;
      }
    }
    updateTypeOptions();
    updatePrice();
  }

  function updateTypeOptions() {
    const t = selectedTemplate();
    const runtimes = state.health && Array.isArray(state.health.runtimes) ? state.health.runtimes : null;
    const sel = el('create-type');
    for (const o of sel.options) {
      let ok = true;
      if (t && Array.isArray(t.types) && t.types.length && !t.types.includes(o.value)) ok = false;
      if (runtimes && o.value === 'vm' && !runtimes.includes('firecracker')) ok = false;
      if (runtimes && o.value === 'default' && !runtimes.includes('docker')) ok = false;
      o.disabled = !ok;
      o.textContent = o.textContent.replace(/ — unavailable$/, '') + (ok ? '' : ' — unavailable');
    }
    if (sel.selectedOptions.length && sel.selectedOptions[0].disabled) {
      const first = Array.from(sel.options).find((o) => !o.disabled);
      if (first) sel.value = first.value;
    }
  }

  function createShape() {
    const [cpu, memory, disk] = el('create-shape').value.split(',').map(Number);
    const override = el('create-disk').value.trim();
    return { cpu, memory, disk: override ? Number(override) : disk };
  }

  function updatePrice() {
    const rates = Object.assign({}, DEFAULT_RATES, (state.billing && state.billing.rates) || {});
    const s = createShape();
    const mult = el('create-type').value === 'vm' ? Number(rates.vm_multiplier) || 1 : 1;
    const compute = (s.cpu * rates.vcpu_month_micros + s.memory * rates.ram_gb_month_micros) * mult;
    const disk = s.disk * rates.disk_gb_month_micros;
    el('create-price').textContent = `Price: ${fmtUsd(compute + disk)}/month running · ${fmtUsd(disk)}/month asleep or stopped. Creating requires one day of the running rate in the wallet.`;
  }

  async function onCreate(ev) {
    ev.preventDefault();
    const errBox = el('create-error');
    show(errBox, false);
    const body = {
      template: el('create-template').value,
      resources: createShape(),
      type: el('create-type').value,
      auto_sleep: el('create-auto-sleep').checked,
      idle_timeout_seconds: Number(el('create-idle').value) || 900,
      network: el('create-network').value,
    };
    const name = el('create-name').value.trim();
    if (name) body.name = name;
    const submit = el('create-submit');
    submit.disabled = true;
    try {
      const inst = await api('POST', '/instances', body);
      toast(`Created ${label(inst)} — provisioning.`);
      el('create-form').reset();
      el('create-idle').disabled = true;
      onTemplateChange();
      await refreshInstances();
      selectInstance(inst.id);
    } catch (e) {
      let msg = errText(e);
      if (e.code === 'insufficient_balance') msg += ' Add funds under Billing.';
      if (e.code === 'instance_limit_reached') msg += ' Sleep or stop another instance, or top up to raise the limit.';
      errBox.textContent = msg;
      show(errBox, true);
    } finally {
      submit.disabled = false;
    }
  }

  // ---------------------------------------------------------------- detail
  function current() {
    return state.selected;
  }

  async function selectInstance(id) {
    const changed = id !== state.selectedId;
    state.selectedId = id;
    let inst = state.instances.find((i) => i.id === id) || null;
    if (!inst) {
      try {
        inst = await api('GET', `/instances/${encodeURIComponent(id)}`);
      } catch (e) {
        toastError(e);
        return;
      }
    }
    state.selected = inst;
    if (changed) resetInstanceViews();
    renderList();
    renderDetail(inst);
    showPanel('detail');
    if (changed) showTab(inst.agent_api || !AGENT_TABS.includes(state.tab) ? state.tab : 'exec');
    else if (!inst.agent_api && AGENT_TABS.includes(state.tab)) showTab('exec');
  }

  function resetInstanceViews() {
    clear(el('exec-output'));
    clear(el('chat-messages'));
    setChatSession(null);
    if (state.chat.controller) state.chat.controller.abort();
    state.files = { path: '.', open: null, pending: null };
    el('files-path').value = '.';
    el('files-upload').value = '';
    el('files-upload-name').textContent = '';
    el('files-upload-btn').disabled = true;
    show(el('file-editor'), false);
    clear(el('files-list'));
    el('files-status').textContent = '';
    clear(el('sessions-list'));
    show(el('session-detail'), false);
    el('sessions-status').textContent = '';
    el('logs-output').textContent = '';
    clear(el('metrics-cpu'));
    clear(el('metrics-mem'));
    el('metrics-cpu-value').textContent = '';
    el('metrics-mem-value').textContent = '';
    el('metrics-empty').textContent = '';
    clear(el('backups-body'));
    el('backups-status').textContent = '';
  }

  function renderDetail(inst) {
    el('detail-name').textContent = inst.name || inst.id;
    el('detail-id').textContent = inst.id;
    const st = el('detail-status');
    st.textContent = inst.status;
    st.className = 'badge st-' + inst.status;
    el('detail-reason').textContent = inst.status_reason ? `(${inst.status_reason})` : '';
    const link = el('detail-url');
    link.textContent = inst.url || '';
    link.href = inst.url || '#';
    el('detail-template').textContent = `${inst.template_name || inst.template}@${inst.template_revision}`;
    el('detail-type').textContent = inst.type;
    el('detail-shape').textContent = shapeText(inst.resources);
    el('detail-region').textContent = inst.region || '—';
    el('detail-runtime').textContent = inst.runtime || '—';
    el('detail-sleep').textContent = inst.auto_sleep ? `auto after ${fmtDuration(inst.idle_timeout_seconds)} idle` : 'manual';
    el('detail-disk').textContent = fmtBytes(inst.disk_used_bytes);
    el('detail-created').textContent = fmtTime(inst.created_at);

    const s = inst.status;
    const gone = s === 'deleting' || s === 'deleted';
    el('act-start').disabled = gone || !['stopped', 'sleeping', 'failed'].includes(s);
    el('act-stop').disabled = gone || ['stopped', 'stopping', 'provisioning'].includes(s);
    el('act-sleep').disabled = gone || s !== 'running';
    el('act-restart').disabled = gone || !['running', 'sleeping', 'stopped', 'failed'].includes(s);
    el('act-fork').disabled = gone || s === 'provisioning';
    el('act-backup').disabled = gone || s === 'provisioning';
    el('act-terminal').disabled = gone || ['stopped', 'stopping', 'failed', 'provisioning'].includes(s);
    el('act-delete').disabled = gone;
    el('detail-panel').classList.toggle('busy', BUSY.includes(s));

    show(el('tab-chat'), Boolean(inst.agent_api));
    show(el('tab-sessions'), Boolean(inst.agent_api));
    show(el('chat-unavailable'), !inst.agent_api);
    show(el('chat-wrap'), Boolean(inst.agent_api));
  }

  async function action(name) {
    const inst = current();
    if (!inst) return;
    const id = inst.id;
    try {
      if (name === 'delete') {
        if (!confirm(`Delete ${label(inst)}? Its disk and backups are destroyed. This cannot be undone.`)) return;
        await api('DELETE', `/instances/${id}`);
        toast(`Deleting ${label(inst)}.`);
        state.selectedId = null;
        state.selected = null;
        showPanel('empty');
      } else if (name === 'fork') {
        const name2 = prompt('Name for the fork (optional). A running source is slept during the copy.', '');
        if (name2 === null) return;
        const body = name2.trim() ? { name: name2.trim() } : {};
        const fork = await api('POST', `/instances/${id}/fork`, body);
        toast(`Forked as ${label(fork)}.`);
        await refreshInstances();
        selectInstance(fork.id);
        return;
      } else if (name === 'backup') {
        const note = prompt('Backup note (optional):', '');
        if (note === null) return;
        const b = await api('POST', `/instances/${id}/backups`, note.trim() ? { note: note.trim() } : {});
        toast(`Backup ${b.id} taken (${fmtBytes(b.size_bytes)}).`);
        if (state.tab === 'backups') loadBackups();
      } else {
        const r = await api('POST', `/instances/${id}/${name}`);
        toast(`${label(inst)}: ${r && r.status ? r.status : name}.`);
      }
      await refreshInstances();
    } catch (e) {
      toastError(e);
    }
  }

  /** Open `port` of the current instance (0 = default port) in a new tab via a signed URL. */
  async function openSigned(port) {
    const inst = current();
    if (!inst) return;
    // Open synchronously inside the click so pop-up blockers allow it, then
    // navigate once the signed URL arrives.
    const win = window.open('about:blank', '_blank');
    if (win) win.opener = null;
    try {
      const r = await api('GET', `/instances/${inst.id}/signed-url${port ? '?port=' + port : ''}`);
      if (win) {
        win.location.href = r.url;
      } else {
        const a = h('a', { href: r.url, target: '_blank', rel: 'noopener noreferrer', text: port === TERMINAL_PORT ? 'open the terminal' : 'open the instance' });
        const t = el('toast');
        clear(t);
        t.append('Pop-up blocked: ', a);
        t.className = 'toast info';
        show(t, true);
        clearTimeout(toastTimer);
        toastTimer = setTimeout(() => show(t, false), 15000);
      }
    } catch (e) {
      if (win) win.close();
      toastError(e);
    }
  }

  // ------------------------------------------------------------------- tabs
  function showTab(name) {
    if (!TABS.includes(name)) name = 'exec';
    state.tab = name;
    for (const t of TABS) {
      const btn = document.getElementById('tab-' + t);
      const panel = document.getElementById('panel-' + t);
      const on = t === name;
      btn.setAttribute('aria-selected', on ? 'true' : 'false');
      btn.tabIndex = on ? 0 : -1;
      show(panel, on);
    }
    loadTab(name);
  }

  function loadTab(name) {
    const inst = current();
    if (!inst) return;
    if (name === 'sessions') loadSessions();
    else if (name === 'files') loadFiles(state.files.path);
    else if (name === 'logs') loadLogs();
    else if (name === 'metrics') loadMetrics();
    else if (name === 'backups') loadBackups();
    else if (name === 'exec') el('exec-command').focus();
    else if (name === 'chat' && inst.agent_api) el('chat-input').focus();
  }

  function onTabKey(ev) {
    const visible = TABS.filter((t) => !document.getElementById('tab-' + t).hidden);
    const i = visible.indexOf(state.tab);
    let next = null;
    if (ev.key === 'ArrowRight') next = visible[(i + 1) % visible.length];
    else if (ev.key === 'ArrowLeft') next = visible[(i - 1 + visible.length) % visible.length];
    else if (ev.key === 'Home') next = visible[0];
    else if (ev.key === 'End') next = visible[visible.length - 1];
    if (next) {
      ev.preventDefault();
      showTab(next);
      document.getElementById('tab-' + next).focus();
    }
  }

  // ------------------------------------------------------------------- exec
  async function runExec(ev) {
    ev.preventDefault();
    const inst = current();
    const input = el('exec-command');
    const command = input.value;
    if (!inst || !command.trim()) return;
    const user = el('exec-user').value;
    const out = el('exec-output');
    const pending = h('div', { class: 'muted small', text: 'running…' });
    const entry = h('div', { class: 'exec-entry' }, h('div', { class: 'exec-cmd' }, h('span', { class: 'prompt', text: (user || 'agent') + '$ ' }), h('code', { text: command })), pending);
    out.append(entry);
    out.scrollTop = out.scrollHeight;
    el('exec-run').disabled = true;
    try {
      const body = { command };
      if (user) body.user = user;
      const r = await api('POST', `/instances/${inst.id}/exec`, body);
      pending.remove();
      if (r.stdout) entry.append(h('pre', { class: 'stdout', text: r.stdout }));
      if (r.stderr) entry.append(h('pre', { class: 'stderr', text: r.stderr }));
      entry.append(h('div', { class: 'small ' + (r.exit_code === 0 ? 'muted' : 'error'), text: `exit ${r.exit_code} · ${r.duration_ms} ms${r.truncated ? ' · output truncated at 1 MiB' : ''}` }));
      input.select();
    } catch (e) {
      pending.remove();
      entry.append(h('div', { class: 'error small', text: errText(e) }));
    } finally {
      el('exec-run').disabled = false;
      out.scrollTop = out.scrollHeight;
    }
  }

  // ------------------------------------------------------------------- chat
  function setChatSession(id) {
    state.chat.sessionId = id || null;
    el('chat-session').textContent = id || 'new';
  }

  function bubble(role, text) {
    return h('div', { class: 'msg ' + role }, h('div', { class: 'msg-role', text: role }), h('div', { class: 'msg-text', text }));
  }

  function scrollChat() {
    const m = el('chat-messages');
    m.scrollTop = m.scrollHeight;
  }

  async function sendChat(ev) {
    ev.preventDefault();
    const inst = current();
    const input = el('chat-input');
    const text = input.value.trim();
    if (!inst || !text || state.chat.controller) return;
    if (!inst.agent_api) {
      toast('This instance does not serve the Agent API.', 'error');
      return;
    }
    input.value = '';
    const messages = el('chat-messages');
    messages.append(bubble('user', text));
    const reply = bubble('assistant', '');
    const replyText = reply.querySelector('.msg-text');
    messages.append(reply);
    scrollChat();

    const controller = new AbortController();
    state.chat.controller = controller;
    show(el('chat-stop'), true);
    el('chat-send').disabled = true;
    const req = { input: text, stream: true };
    if (state.chat.sessionId) req.session_id = state.chat.sessionId;
    let acc = '';
    try {
      await streamResponses(inst, req, controller.signal, (event, data) => {
        data = data && typeof data === 'object' ? data : {};
        if (event === 'response.created') {
          if (data.session_id) setChatSession(data.session_id);
        } else if (event === 'response.output_text.delta') {
          acc += typeof data.delta === 'string' ? data.delta : '';
          replyText.textContent = acc;
          scrollChat();
        } else if (event === 'response.tool_call') {
          const args = typeof data.arguments === 'string' ? data.arguments : JSON.stringify(data.arguments || {});
          reply.insertBefore(h('div', { class: 'tool small muted', text: `tool ${data.name || '?'}(${args})` }), replyText);
          scrollChat();
        } else if (event === 'response.completed') {
          if (typeof data.output_text === 'string' && data.output_text && data.output_text !== acc) {
            acc = data.output_text;
            replyText.textContent = acc;
          }
          if (data.session_id) setChatSession(data.session_id);
          if (data.status && data.status !== 'completed') reply.append(h('div', { class: 'error small', text: 'status: ' + data.status }));
        } else if (event === 'response.failed') {
          reply.append(h('div', { class: 'error small', text: 'failed: ' + (data.error || 'unknown error') }));
        }
      });
      if (!acc && !reply.querySelector('.error')) replyText.textContent = '(no output)';
    } catch (e) {
      if (e && e.name === 'AbortError') reply.append(h('div', { class: 'muted small', text: '(stopped)' }));
      else reply.append(h('div', { class: 'error small', text: errText(e) }));
    } finally {
      state.chat.controller = null;
      show(el('chat-stop'), false);
      el('chat-send').disabled = false;
      scrollChat();
      input.focus();
    }
  }

  /** POST /v1/responses with stream:true and dispatch each SSE event as it arrives. */
  async function streamResponses(inst, body, signal, onEvent) {
    const res = await instFetch(inst, '/v1/responses', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'text/event-stream' },
      body: JSON.stringify(body),
      signal,
    });
    const ctype = res.headers.get('content-type') || '';
    if (!ctype.includes('text/event-stream') || !res.body) {
      // A gateway that ignored stream:true answers with the response object.
      const data = await res.json();
      onEvent('response.created', data);
      onEvent(data.status === 'failed' ? 'response.failed' : 'response.completed', data);
      return;
    }
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = '';
    const dispatch = (chunk) => {
      const ev = parseSse(chunk);
      if (ev) onEvent(ev.event, ev.data);
    };
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      buf = buf.replace(/\r\n/g, '\n');
      let i;
      while ((i = buf.indexOf('\n\n')) !== -1) {
        dispatch(buf.slice(0, i));
        buf = buf.slice(i + 2);
      }
    }
    buf += decoder.decode();
    if (buf.trim()) dispatch(buf.replace(/\r\n/g, '\n'));
  }

  /** Parse one SSE event block (`event: x\ndata: {...}`) into { event, data }. */
  function parseSse(chunk) {
    let event = 'message';
    const data = [];
    for (const line of chunk.split('\n')) {
      if (!line || line.startsWith(':')) continue;
      const i = line.indexOf(':');
      const field = i < 0 ? line : line.slice(0, i);
      let value = i < 0 ? '' : line.slice(i + 1);
      if (value.startsWith(' ')) value = value.slice(1);
      if (field === 'event') event = value;
      else if (field === 'data') data.push(value);
    }
    if (!data.length && event === 'message') return null;
    const raw = data.join('\n');
    let parsed;
    try {
      parsed = raw ? JSON.parse(raw) : {};
    } catch (_) {
      parsed = { delta: raw };
    }
    return { event, data: parsed };
  }

  // --------------------------------------------------------------- sessions
  async function loadSessions() {
    const inst = current();
    if (!inst || !inst.agent_api) return;
    const status = el('sessions-status');
    status.textContent = 'Loading…';
    try {
      const r = await instJson(inst, 'GET', '/v1/sessions');
      const list = el('sessions-list');
      clear(list);
      const sessions = Array.isArray(r && r.data) ? r.data : [];
      for (const s of sessions) {
        list.append(
          h(
            'li',
            null,
            h(
              'button',
              { type: 'button', class: 'row-btn' + (s.id === state.sessions.openId ? ' selected' : ''), 'data-id': s.id, onclick: () => openSession(s.id) },
              h('span', { class: 'grow', text: s.name || s.id }),
              h('span', { class: 'muted small', text: `${s.message_count} msg · ${fmtTimeMs(s.updated_at)}` })
            )
          )
        );
      }
      status.textContent = sessions.length ? `${sessions.length} session${sessions.length === 1 ? '' : 's'}` : 'No sessions yet. Send a chat message to start one.';
    } catch (e) {
      status.textContent = errText(e);
    }
  }

  function renderMessages(container, messages) {
    clear(container);
    for (const m of messages || []) {
      const b = bubble(m.role || 'assistant', m.content || '');
      if (m.at) b.append(h('div', { class: 'muted small', text: fmtTimeMs(m.at) }));
      container.append(b);
    }
  }

  async function openSession(id) {
    const inst = current();
    if (!inst) return;
    try {
      const d = await instJson(inst, 'GET', `/v1/sessions/${encodeURIComponent(id)}`);
      state.sessions.openId = id;
      el('session-title').textContent = d.name || d.id;
      renderMessages(el('session-messages'), d.messages);
      show(el('session-detail'), true);
      for (const b of el('sessions-list').querySelectorAll('button')) b.classList.toggle('selected', b.dataset.id === id);
    } catch (e) {
      toastError(e);
    }
  }

  function useSessionInChat() {
    const id = state.sessions.openId;
    if (!id) return;
    setChatSession(id);
    const msgs = el('chat-messages');
    clear(msgs);
    for (const node of el('session-messages').children) msgs.append(node.cloneNode(true));
    showTab('chat');
    scrollChat();
  }

  async function deleteSession() {
    const inst = current();
    const id = state.sessions.openId;
    if (!inst || !id || !confirm(`Delete session ${id}?`)) return;
    try {
      await instJson(inst, 'DELETE', `/v1/sessions/${encodeURIComponent(id)}`);
      if (state.chat.sessionId === id) setChatSession(null);
      state.sessions.openId = null;
      show(el('session-detail'), false);
      loadSessions();
    } catch (e) {
      toastError(e);
    }
  }

  // ------------------------------------------------------------------ files
  function normPath(p) {
    p = String(p || '.').trim().replace(/\/+$/, '').replace(/^\.\//, '');
    return p === '' || p === '/' ? '.' : p;
  }

  function joinPath(dir, name) {
    return dir === '.' ? name : dir + '/' + name;
  }

  function parentPath(p) {
    if (p === '.') return '.';
    const i = p.lastIndexOf('/');
    return i < 0 ? '.' : p.slice(0, i) || '.';
  }

  async function loadFiles(path) {
    const inst = current();
    if (!inst) return;
    path = normPath(path);
    const status = el('files-status');
    status.textContent = 'Loading…';
    try {
      const d = await instJson(inst, 'GET', '/v1/files?path=' + encodeURIComponent(path));
      state.files.path = normPath(d && d.path ? d.path : path);
      el('files-path').value = state.files.path;
      const list = el('files-list');
      clear(list);
      const entries = (d && d.entries ? d.entries : []).slice().sort((a, b) => (a.type === b.type ? a.name.localeCompare(b.name) : a.type === 'dir' ? -1 : 1));
      if (state.files.path !== '.') {
        list.append(h('li', null, h('button', { type: 'button', class: 'row-btn', onclick: () => loadFiles(parentPath(state.files.path)) }, h('span', { class: 'grow mono', text: '../' }))));
      }
      for (const e of entries) {
        const full = joinPath(state.files.path, e.name);
        const isDir = e.type === 'dir';
        list.append(
          h(
            'li',
            null,
            h(
              'button',
              { type: 'button', class: 'row-btn', onclick: () => (isDir ? loadFiles(full) : openFile(full)) },
              h('span', { class: 'grow mono', text: isDir ? e.name + '/' : e.name }),
              h('span', { class: 'muted small', text: isDir ? fmtTimeMs(e.modified_at) : `${fmtBytes(e.size)} · ${fmtTimeMs(e.modified_at)}` })
            )
          )
        );
      }
      status.textContent = entries.length ? `${entries.length} entr${entries.length === 1 ? 'y' : 'ies'}` : 'Empty directory';
    } catch (e) {
      status.textContent = errText(e);
    }
  }

  async function openFile(path) {
    const inst = current();
    if (!inst) return;
    const status = el('file-editor-status');
    try {
      const res = await instFetch(inst, '/v1/files/content?path=' + encodeURIComponent(path));
      const blob = await res.blob();
      state.files.open = path;
      el('file-editor-path').textContent = path;
      const editor = el('file-editor-text');
      if (blob.size > 2 * 1024 * 1024) {
        editor.value = '';
        editor.disabled = true;
        status.textContent = `${fmtBytes(blob.size)} is too large to edit here; use Download.`;
      } else {
        const text = await blob.text();
        editor.disabled = false;
        editor.value = text;
        status.textContent = text.includes('\u0000') ? `${fmtBytes(blob.size)} · looks binary; saving will corrupt it.` : `${fmtBytes(blob.size)} · ${res.headers.get('content-type') || ''}`;
      }
      show(el('file-editor'), true);
      editor.focus();
    } catch (e) {
      toastError(e);
    }
  }

  async function putFile(path, body) {
    const inst = current();
    const res = await instFetch(inst, '/v1/files/content?path=' + encodeURIComponent(path), {
      method: 'PUT',
      headers: { 'Content-Type': 'application/octet-stream' },
      body,
    });
    const text = await res.text();
    return text ? JSON.parse(text) : { path, size: 0 };
  }

  async function saveFile() {
    const path = state.files.open;
    if (!path || !current()) return;
    const status = el('file-editor-status');
    status.textContent = 'Saving…';
    try {
      const r = await putFile(path, el('file-editor-text').value);
      status.textContent = `Saved ${r.path} (${fmtBytes(r.size)}).`;
      loadFiles(state.files.path);
    } catch (e) {
      status.textContent = errText(e);
    }
  }

  async function uploadFile() {
    const f = state.files.pending;
    if (!f || !current()) return;
    const path = joinPath(state.files.path, f.name);
    const status = el('files-status');
    status.textContent = `Uploading ${f.name}…`;
    el('files-upload-btn').disabled = true;
    try {
      const r = await putFile(path, f);
      toast(`Uploaded ${r.path} (${fmtBytes(r.size)}).`);
      state.files.pending = null;
      el('files-upload').value = '';
      el('files-upload-name').textContent = '';
      loadFiles(state.files.path);
    } catch (e) {
      status.textContent = errText(e);
      el('files-upload-btn').disabled = false;
    }
  }

  function newFile() {
    if (!current()) return;
    const name = prompt('New file name, relative to the current directory:', 'notes.txt');
    if (!name || !name.trim()) return;
    state.files.open = joinPath(state.files.path, normPath(name));
    el('file-editor-path').textContent = state.files.open;
    const editor = el('file-editor-text');
    editor.disabled = false;
    editor.value = '';
    el('file-editor-status').textContent = 'New file — Save creates it.';
    show(el('file-editor'), true);
    editor.focus();
  }

  async function deleteFile() {
    const inst = current();
    const path = state.files.open;
    if (!inst || !path || !confirm(`Delete ${path}?`)) return;
    try {
      await instJson(inst, 'DELETE', '/v1/files?path=' + encodeURIComponent(path));
      toast(`Deleted ${path}.`);
      closeEditor();
      loadFiles(state.files.path);
    } catch (e) {
      toastError(e);
    }
  }

  async function downloadFile() {
    const inst = current();
    const path = state.files.open;
    if (!inst || !path) return;
    try {
      const res = await instFetch(inst, '/v1/files/content?path=' + encodeURIComponent(path));
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      const a = h('a', { href: url, download: path.split('/').pop() || 'file' });
      document.body.append(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 10000);
    } catch (e) {
      toastError(e);
    }
  }

  function closeEditor() {
    state.files.open = null;
    show(el('file-editor'), false);
    el('file-editor-text').value = '';
    el('file-editor-status').textContent = '';
  }

  // ------------------------------------------------------------------- logs
  async function loadLogs() {
    const inst = current();
    if (!inst) return;
    const out = el('logs-output');
    try {
      const r = await api('GET', `/instances/${inst.id}/logs?tail=${encodeURIComponent(el('logs-tail').value)}`);
      out.textContent = r && r.logs ? r.logs : '(no log output)';
      out.scrollTop = out.scrollHeight;
    } catch (e) {
      out.textContent = errText(e);
    }
  }

  // ---------------------------------------------------------------- metrics
  async function loadMetrics() {
    const inst = current();
    if (!inst) return;
    const hours = el('metrics-hours').value;
    const empty = el('metrics-empty');
    try {
      const r = await api('GET', `/instances/${inst.id}/metrics?hours=${encodeURIComponent(hours)}`);
      const pts = (Array.isArray(r && r.data) ? r.data : []).slice().sort((a, b) => a.at - b.at);
      if (!pts.length) {
        clear(el('metrics-cpu'));
        clear(el('metrics-mem'));
        el('metrics-cpu-value').textContent = '';
        el('metrics-mem-value').textContent = '';
        empty.textContent = 'No samples yet; metrics are sampled every minute while running.';
        return;
      }
      empty.textContent = `${pts.length} sample${pts.length === 1 ? '' : 's'}, ${fmtTime(pts[0].at)} – ${fmtTime(pts[pts.length - 1].at)}`;
      const cpu = pts.map((p) => Number(p.cpu_percent) || 0);
      const mem = pts.map((p) => Number(p.memory_bytes) || 0);
      const cpuLast = cpu[cpu.length - 1];
      const memLast = mem[mem.length - 1];
      el('metrics-cpu-value').textContent = `${cpuLast.toFixed(1)}% now · peak ${Math.max(...cpu).toFixed(1)}%`;
      el('metrics-mem-value').textContent = `${fmtBytes(memLast)} now · peak ${fmtBytes(Math.max(...mem))}`;
      sparkline(el('metrics-cpu'), cpu, { floor: 0, ceil: Math.max(100, ...cpu), label: `CPU over ${hours} h: now ${cpuLast.toFixed(1)}%, peak ${Math.max(...cpu).toFixed(1)}%` });
      sparkline(el('metrics-mem'), mem, { floor: 0, label: `Memory over ${hours} h: now ${fmtBytes(memLast)}, peak ${fmtBytes(Math.max(...mem))}` });
    } catch (e) {
      empty.textContent = errText(e);
    }
  }

  /** Draw an inline SVG sparkline of `values` into `container`. */
  function sparkline(container, values, opts) {
    clear(container);
    const width = 320;
    const height = 64;
    const pad = 3;
    const lo = opts.floor != null ? opts.floor : Math.min(...values);
    const hi = Math.max(opts.ceil != null ? opts.ceil : -Infinity, ...values, lo + 1e-9);
    const n = values.length;
    const x = (i) => (n === 1 ? width / 2 : pad + (i * (width - 2 * pad)) / (n - 1));
    const y = (v) => height - pad - ((v - lo) / (hi - lo)) * (height - 2 * pad);
    const pts = values.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`);
    const root = svg('svg', { viewBox: `0 0 ${width} ${height}`, preserveAspectRatio: 'none', role: 'img', 'aria-label': opts.label || '' });
    root.append(svg('path', { class: 'spark-area', d: `M${x(0).toFixed(1)},${height} L${pts.join(' L')} L${x(n - 1).toFixed(1)},${height} Z` }));
    root.append(svg('polyline', { class: 'spark-line', points: pts.join(' ') }));
    if (n === 1) root.append(svg('circle', { class: 'spark-dot', cx: x(0), cy: y(values[0]), r: 3 }));
    container.append(root);
  }

  // ---------------------------------------------------------------- backups
  async function loadBackups() {
    const inst = current();
    if (!inst) return;
    const status = el('backups-status');
    status.textContent = 'Loading…';
    try {
      const r = await api('GET', `/instances/${inst.id}/backups`);
      const rows = Array.isArray(r && r.data) ? r.data : [];
      const body = el('backups-body');
      clear(body);
      for (const b of rows) {
        body.append(
          h(
            'tr',
            null,
            h('td', { text: fmtTime(b.created_at) }),
            h('td', { text: fmtBytes(b.size_bytes) }),
            h('td', { text: b.note || '' }),
            h('td', null, h('code', { class: 'small', text: b.id })),
            h('td', null, h('button', { type: 'button', class: 'btn small', onclick: () => restoreBackup(b) }, 'Restore'))
          )
        );
      }
      status.textContent = rows.length ? `${rows.length} backup${rows.length === 1 ? '' : 's'}` : 'No backups yet. Use the Backup button above.';
    } catch (e) {
      status.textContent = errText(e);
    }
  }

  async function restoreBackup(b) {
    const inst = current();
    if (!inst || !confirm(`Restore ${label(inst)} from the backup taken ${fmtTime(b.created_at)}? Current files are replaced.`)) return;
    try {
      const r = await api('POST', `/instances/${inst.id}/restore`, { backup_id: b.id });
      toast(`Restored ${label(inst)}; status ${r.status}.`);
      await refreshInstances();
    } catch (e) {
      toastError(e);
    }
  }

  // ---------------------------------------------------------------- billing
  async function loadBilling(showErrors) {
    if (!state.key) return;
    try {
      const [b, l] = await Promise.all([api('GET', '/billing'), state.view === 'billing' ? api('GET', '/billing/ledger?limit=100') : Promise.resolve(null)]);
      state.billing = b;
      el('billing-wallet').textContent = fmtUsd(b.wallet_micros);
      el('billing-awake').textContent = `${b.awake_count} / ${b.awake_limit}`;
      el('billing-awake-note').textContent = 'awake limit rises with cumulative top-ups';
      el('billing-topups').textContent = fmtUsd(b.top_up_total_micros);
      el('billing-credit').textContent = b.free_credit_granted ? 'granted' : 'not granted';
      const r = Object.assign({}, DEFAULT_RATES, b.rates || {});
      el('billing-rates').textContent = `Rates per month: ${fmtUsd(r.vcpu_month_micros)} per vCPU, ${fmtUsd(r.ram_gb_month_micros)} per GB RAM, ${fmtUsd(r.disk_gb_month_micros)} per GB disk; vm type bills compute at ${r.vm_multiplier}×. Billed per minute; sleeping and stopped instances pay for disk only.`;
      show(el('topup-section'), Boolean(state.health && state.health.dev_mode));
      if (l) renderLedger(Array.isArray(l.data) ? l.data : []);
      if (state.panel === 'create') updatePrice();
    } catch (e) {
      if (showErrors) toastError(e);
    }
  }

  function renderLedger(entries) {
    const body = el('ledger-body');
    clear(body);
    for (const e of entries) {
      body.append(
        h(
          'tr',
          null,
          h('td', { text: fmtTime(e.at) }),
          h('td', null, h('span', { class: 'badge kind-' + e.kind, text: e.kind })),
          h('td', null, e.instance_id ? h('button', { type: 'button', class: 'link', onclick: () => selectInstance(e.instance_id) }, h('code', { text: e.instance_id })) : '—'),
          h('td', { class: 'num', text: e.minutes ? Number(e.minutes).toFixed(2) : '' }),
          h('td', { class: 'num ' + (e.micros < 0 ? 'neg' : 'pos'), text: fmtUsd(e.micros) }),
          h('td', { class: 'small muted', text: e.note || '' })
        )
      );
    }
    el('ledger-status').textContent = entries.length ? `${entries.length} most recent entries` : 'No ledger entries yet.';
  }

  async function onTopUp(ev) {
    ev.preventDefault();
    const err = el('topup-error');
    show(err, false);
    const usd = Number(el('topup-amount').value);
    if (!(usd >= 5 && usd <= 10000)) {
      err.textContent = 'Amount must be between $5 and $10,000.';
      show(err, true);
      return;
    }
    const btn = el('topup-submit');
    btn.disabled = true;
    try {
      const r = await api('POST', '/billing/top-up', { amount_micros: Math.round(usd * 1e6), provider: el('topup-provider').value || 'dev' });
      toast(`Added ${fmtUsd(r.entry ? r.entry.micros : usd * 1e6)}; balance ${fmtUsd(r.workspace ? r.workspace.wallet_micros : 0)}.`);
      await loadBilling(true);
    } catch (e) {
      err.textContent = errText(e);
      show(err, true);
    } finally {
      btn.disabled = false;
    }
  }

  // ------------------------------------------------------------------- init
  function wire() {
    el('login-form').addEventListener('submit', onLogin);
    el('signup-form').addEventListener('submit', onSignup);
    el('signup-continue').addEventListener('click', () => enter().catch((e) => showLogin(e)));
    el('logout-btn').addEventListener('click', logout);
    el('nav-instances').addEventListener('click', () => showView('instances'));
    el('nav-billing').addEventListener('click', () => showView('billing'));
    el('new-instance-btn').addEventListener('click', () => showPanel('create'));
    el('empty-new-btn').addEventListener('click', () => showPanel('create'));

    el('create-form').addEventListener('submit', onCreate);
    el('create-cancel').addEventListener('click', () => showPanel(state.selectedId ? 'detail' : 'empty'));
    el('create-template').addEventListener('change', onTemplateChange);
    el('create-shape').addEventListener('change', updatePrice);
    el('create-disk').addEventListener('input', updatePrice);
    el('create-type').addEventListener('change', updatePrice);
    el('create-auto-sleep').addEventListener('change', (ev) => {
      el('create-idle').disabled = !ev.target.checked;
    });

    el('act-start').addEventListener('click', () => action('start'));
    el('act-stop').addEventListener('click', () => action('stop'));
    el('act-sleep').addEventListener('click', () => action('sleep'));
    el('act-restart').addEventListener('click', () => action('restart'));
    el('act-delete').addEventListener('click', () => action('delete'));
    el('act-fork').addEventListener('click', () => action('fork'));
    el('act-backup').addEventListener('click', () => action('backup'));
    el('act-terminal').addEventListener('click', () => openSigned(TERMINAL_PORT));
    el('detail-url').addEventListener('click', (ev) => {
      ev.preventDefault();
      openSigned(0);
    });

    const tabs = el('tabs');
    tabs.addEventListener('click', (ev) => {
      const btn = ev.target.closest('[role="tab"]');
      if (btn) showTab(btn.id.replace(/^tab-/, ''));
    });
    tabs.addEventListener('keydown', onTabKey);

    el('exec-form').addEventListener('submit', runExec);
    el('exec-clear').addEventListener('click', () => clear(el('exec-output')));

    el('chat-form').addEventListener('submit', sendChat);
    el('chat-input').addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter' && !ev.shiftKey) {
        ev.preventDefault();
        el('chat-form').requestSubmit();
      }
    });
    el('chat-new-session').addEventListener('click', () => {
      setChatSession(null);
      clear(el('chat-messages'));
      el('chat-input').focus();
    });
    el('chat-stop').addEventListener('click', () => {
      if (state.chat.controller) state.chat.controller.abort();
    });

    el('sessions-refresh').addEventListener('click', loadSessions);
    el('session-use').addEventListener('click', useSessionInChat);
    el('session-delete').addEventListener('click', deleteSession);

    el('files-path-form').addEventListener('submit', (ev) => {
      ev.preventDefault();
      loadFiles(el('files-path').value);
    });
    el('files-up').addEventListener('click', () => loadFiles(parentPath(state.files.path)));
    el('files-refresh').addEventListener('click', () => loadFiles(state.files.path));
    el('files-upload').addEventListener('change', (ev) => {
      const f = ev.target.files && ev.target.files[0];
      state.files.pending = f || null;
      el('files-upload-name').textContent = f ? `${f.name} (${fmtBytes(f.size)})` : '';
      el('files-upload-btn').disabled = !f;
    });
    el('files-upload-btn').addEventListener('click', uploadFile);
    el('files-new-btn').addEventListener('click', newFile);
    el('file-save').addEventListener('click', saveFile);
    el('file-download').addEventListener('click', downloadFile);
    el('file-delete').addEventListener('click', deleteFile);
    el('file-close').addEventListener('click', closeEditor);
    el('file-editor-text').addEventListener('keydown', (ev) => {
      if ((ev.ctrlKey || ev.metaKey) && ev.key === 's') {
        ev.preventDefault();
        saveFile();
      }
    });

    el('logs-refresh').addEventListener('click', loadLogs);
    el('logs-tail').addEventListener('change', loadLogs);
    el('metrics-refresh').addEventListener('click', loadMetrics);
    el('metrics-hours').addEventListener('change', loadMetrics);
    el('backups-refresh').addEventListener('click', loadBackups);

    el('billing-refresh').addEventListener('click', () => loadBilling(true));
    el('topup-form').addEventListener('submit', onTopUp);

    document.addEventListener('visibilitychange', () => {
      if (!document.hidden && state.key) poll();
    });
  }

  async function boot() {
    wire();
    loadHealth();
    const stored = localStorage.getItem(KEY_STORAGE);
    if (stored) {
      state.key = stored;
      try {
        await enter();
        return;
      } catch (e) {
        if (!(e instanceof ApiError && e.status === 401)) {
          // Keep the key for a transient failure; the user can retry.
          showLogin(e);
          return;
        }
      }
    }
    showLogin(null);
  }

  boot();
})();
