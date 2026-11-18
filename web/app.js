'use strict';

const $ = id => document.getElementById(id);
const nativeBridge = window.__TAURI__?.core;
const MiB = 1_048_576;
const GiB = 1_073_741_824;

let token = '';
let page = 'tasks';
let filter = 'all';
let online = false;
let polling = false;
let connectionVersion = 0;
let snapshot = null;
let routesVersion = 0;
let probingRoutes = false;
let policyLoaded = false;
let sessionsSignature = '';
let catalogRequest = 0;
let catalogLoading = false;
let catalogSubmitting = false;
let catalogView = { mode: 'search', value: '', page: 1, title: '' };
let catalogHistory = [];
let selected = new Set();
let catalogItems = [];

const waitingStates = new Set(['queued', 'waiting_space', 'waiting_login', 'retry_wait']);
const pauseStates = new Set(['queued', 'downloading', 'waiting_space', 'retry_wait', 'waiting_login']);
const resumeStates = new Set(['paused', 'failed', 'retry_wait', 'waiting_login', 'cancelled', 'removed']);
const reorderStates = new Set([...waitingStates, 'paused', 'failed']);
const jobStates = {
  queued: '排队中', downloading: '下载中', complete: '已缓存', paused: '已暂停',
  failed: '失败', waiting_space: '磁盘空间不足', waiting_login: '等待登录',
  retry_wait: '等待重试', deleting: '等待释放缓存', removed: '已清理', cancelled: '已取消'
};

function size(bytes) {
  if (bytes == null) return '—';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < MiB) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < GiB) return `${(bytes / MiB).toFixed(1)} MiB`;
  return `${(bytes / GiB).toFixed(2)} GiB`;
}

function duration(seconds) {
  if (seconds == null) return '';
  if (seconds < 60) return `${seconds} 秒`;
  if (seconds < 3600) return `约 ${Math.ceil(seconds / 60)} 分钟`;
  return `约 ${(seconds / 3600).toFixed(1)} 小时`;
}

function node(tag, text = '', className = '') {
  const element = document.createElement(tag);
  element.textContent = text;
  if (className) element.className = className;
  return element;
}

function cell(row, value = '', className = '') {
  const td = node('td', value, className);
  row.append(td);
  return td;
}

function setNotice(text = '') {
  $('message').textContent = text;
}

function requestError(status, detail) {
  const message = status === 401 ? '管理令牌无效。' : `请求失败（${status}）`;
  const error = new Error(message);
  error.status = status;
  if (typeof detail === 'string' && /^[\u3400-\u9fff]/.test(detail)) error.message = detail;
  return error;
}

async function api(path, body) {
  if (nativeBridge) {
    const result = await nativeBridge.invoke('admin_request', { path, body: body ?? null });
    if (result.status < 200 || result.status >= 300) throw requestError(result.status, result.body);
    return result.body;
  }
  const response = await fetch(`/api${path}`, {
    method: body ? 'POST' : 'GET',
    headers: { Authorization: `Bearer ${token}`, ...(body ? { 'Content-Type': 'application/json' } : {}) },
    body: body ? JSON.stringify(body) : undefined
  });
  if (!response.ok) throw requestError(response.status, await response.text());
  return response.json();
}

function action(parent, title, callback, { className = '', report = setNotice } = {}) {
  const button = node('button', title, className);
  button.type = 'button';
  button.onclick = async () => {
    button.disabled = true;
    report('');
    try {
      const message = await callback();
      if (message) report(message);
      await refresh();
    } catch (error) {
      report(error.message || String(error));
    } finally {
      button.disabled = false;
    }
  };
  parent.append(button);
  return button;
}

function setOnline(value, text, proxy = '') {
  online = value;
  const status = $('service-state');
  status.textContent = text;
  status.dataset.running = String(value);
  $('proxy-address').textContent = proxy;
  $('proxy-address').hidden = !proxy;
  for (const element of document.querySelectorAll('[data-connected]')) element.hidden = !value;
  for (const element of document.querySelectorAll('[data-disconnected]')) element.hidden = value;
  $('login').hidden = Boolean(nativeBridge) || value;
}

function showPage(next) {
  page = next;
  for (const section of document.querySelectorAll('[data-view]')) section.hidden = section.id !== next;
  for (const button of document.querySelectorAll('[data-page]')) {
    if (button.dataset.page === next) button.setAttribute('aria-current', 'page');
    else button.removeAttribute('aria-current');
  }
  setNotice();
  if (snapshot && online) renderPage();
  refresh();
}

for (const button of document.querySelectorAll('[data-page]')) {
  button.onclick = () => showPage(button.dataset.page);
}

function resetConnection() {
  connectionVersion++;
  snapshot = null;
  policyLoaded = false;
  sessionsSignature = '';
  $('session').replaceChildren();
  resetCatalog();
}

$('login').onsubmit = event => {
  event.preventDefault();
  token = $('token').value.trim();
  if (!token) return;
  $('token').value = '';
  setNotice();
  resetConnection();
  refresh();
};

async function refresh() {
  if (polling || (nativeBridge ? !window.fnMediaCacheOnline : !token)) return;
  polling = true;
  const version = connectionVersion;
  try {
    const [stats, downloads, automation] = await Promise.all([api('/stats'), api('/downloads'), api('/automation')]);
    if (version !== connectionVersion) return;
    snapshot = { stats, downloads, automation };
    if (!online && !nativeBridge) setOnline(true, '已连接');
    if (!policyLoaded) {
      fillSettings(automation.policy, automation.cache_limit_bytes);
      policyLoaded = true;
    }
    renderPage();
    if (page === 'overview') await refreshOverview(version);
    if (page === 'library') {
      const sessions = await api('/sessions');
      if (version === connectionVersion && page === 'library') renderSessions(sessions);
    }
  } catch (error) {
    if (version !== connectionVersion) return;
    if (!nativeBridge && error.status === 401) {
      token = '';
      resetConnection();
      setOnline(false, '待连接');
    }
    setNotice(error.message || String(error));
  } finally {
    polling = false;
    if (version !== connectionVersion) refresh();
  }
}

function renderPage() {
  if (page === 'tasks') renderDownloads();
  if (page === 'overview') renderOverview();
  if (page === 'library') {
    renderQueuePreview();
    updateCatalogStates();
  }
}

function setMetrics(id, entries) {
  $(id).replaceChildren(...entries.map(([title, value]) => {
    const wrapper = node('div');
    wrapper.append(node('span', title), node('strong', value));
    return wrapper;
  }));
}

function jobTitle(job, managed) {
  return managed.map(entry => entry.item.title || entry.item.guid).join(' / ') || job.path;
}

function jobAction(job, name) {
  return api(`/downloads/${job.id}/${name}`, {});
}

function describeJob(job) {
  if (job.waiting_reason === 'playback') return '播放时暂停';
  if (job.retry_at) return `重试 ${new Date(job.retry_at * 1000).toLocaleTimeString()}`;
  return job.error || '';
}

function renderDownloads() {
  const { downloads, automation } = snapshot;
  const active = downloads.filter(job => job.state === 'downloading');
  const waiting = downloads.filter(job => waitingStates.has(job.state));
  const stored = downloads.reduce((sum, job) => sum + (job.state === 'removed' ? 0 : job.downloaded), 0);
  const speed = active.reduce((sum, job) => sum + (job.bytes_per_second || 0), 0);
  setMetrics('download-metrics', [
    ['下载中 / 等待中', `${active.length} / ${waiting.length}`],
    ['下载速度', speed ? `${size(speed)}/s` : '—'],
    ['任务缓存 / 上限', `${size(stored)} / ${size(automation.cache_limit_bytes)}`]
  ]);
  const policy = automation.policy;
  $('download-status').textContent = automation.playing && policy.pause_during_playback
    ? '播放时暂停下载' : policy.bytes_per_second ? `限速 ${size(policy.bytes_per_second)}/s` : '';
  const byState = {
    all: downloads, active, waiting,
    paused: downloads.filter(job => job.state === 'paused'),
    complete: downloads.filter(job => job.state === 'complete')
  };
  const jobs = byState[filter];
  const tbody = $('downloads');
  const previous = new Map(Array.from(tbody.rows, row => [row.dataset.id, row]));
  const order = new Map(waiting.filter(job => job.state === 'queued').map((job, index) => [job.id, index + 1]));
  const reorderable = downloads.filter(job => reorderStates.has(job.state));
  const reorderIndex = new Map(reorderable.map((job, index) => [job.id, index]));

  jobs.forEach((job, index) => {
    const managed = automation.managed.filter(entry => entry.download_id === job.id);
    const row = previous.get(job.id) || createJobRow(job.id);
    const title = jobTitle(job, managed);
    const notes = managed.flatMap(entry => {
      if (entry.keep) return ['已保留'];
      if (!entry.completed_at || !policy.cleanup_enabled) return [];
      if (policy.local_history) return [];
      return [`清理时间 ${new Date((entry.completed_at + policy.retention_seconds) * 1000).toLocaleString()}`];
    });
    if (row.cells[0].textContent !== title + notes.join(' · ')) {
      row.cells[0].replaceChildren(node('strong', title, 'title'));
      if (notes.length) row.cells[0].append(node('small', notes.join(' · ')));
    }
    const status = row.cells[1].firstElementChild;
    status.textContent = jobStates[job.state] || job.state;
    const style = job.state === 'downloading' ? 'active' : job.state === 'complete' ? 'complete' : ['failed','paused','waiting_login','waiting_space'].includes(job.state) ? 'warning' : '';
    status.className = `badge ${style}`;
    row.cells[1].lastElementChild.textContent = describeJob(job);
    const [bytes, bar] = row.cells[2].children;
    bytes.textContent = job.total ? `${size(job.downloaded)} / ${size(job.total)}` : job.downloaded ? size(job.downloaded) : '—';
    bar.max = job.total || 1;
    bar.value = Math.min(job.downloaded, bar.max);
    bar.hidden = !job.total;
    row.cells[3].textContent = job.state === 'downloading' && job.bytes_per_second
      ? [`${size(job.bytes_per_second)}/s`, duration(job.eta_seconds)].filter(Boolean).join(' · ')
      : order.has(job.id) ? `第 ${order.get(job.id)} 位` : '—';
    updateJobControls(row.cells[4], job, managed, reorderIndex.get(job.id) ?? -1, reorderable.length);
    if (tbody.rows[index] !== row) tbody.insertBefore(row, tbody.rows[index] || null);
    previous.delete(job.id);
  });
  for (const row of previous.values()) row.remove();
  $('downloads-empty').hidden = Boolean(jobs.length);
  $('downloads-empty').textContent = filter === 'all' ? '暂无下载任务。' : '此状态下没有任务。';
}

function createJobRow(id) {
  const row = document.createElement('tr');
  row.dataset.id = id;
  cell(row, '', 'path');
  cell(row).append(node('span', '', 'badge'), node('p', '', 'hint'));
  const progress = node('progress');
  progress.setAttribute('aria-label', '下载进度');
  cell(row, '', 'task-progress').append(node('span', '', 'hint number'), progress);
  cell(row);
  cell(row, '', 'task-actions');
  return row;
}

function updateJobControls(container, job, managed, position, queueLength) {
  const signature = JSON.stringify([job.state, position, queueLength, managed.map(entry => [entry.session_id, entry.item.guid, entry.keep, Boolean(entry.completed_at)])]);
  if (container.dataset.signature === signature) return;
  container.dataset.signature = signature;
  const toolbar = node('div', '', 'toolbar');
  const details = node('details');
  const menu = node('div');
  const direct = (parent, name, command) => action(parent, name, () => jobAction(job, command).then(() => ''));
  if (resumeStates.has(job.state)) direct(toolbar, job.state === 'removed' ? '重新下载' : '继续', 'resume');
  if (pauseStates.has(job.state)) direct(resumeStates.has(job.state) ? menu : toolbar, '暂停', 'pause');
  const showInRow = managed.length === 1 && job.state === 'complete';
  for (const entry of managed) {
    if (job.state === 'removed' || job.state === 'deleting') break;
    const identity = { session_id: entry.session_id, item_guid: entry.item.guid };
    const suffix = managed.length > 1 ? ` · ${entry.item.title || entry.item.guid}` : '';
    action(showInRow ? toolbar : menu, (entry.keep ? '取消保留' : '保留') + suffix,
      () => api('/managed/retention', { ...identity, keep: !entry.keep }).then(() => ''));
    const mark = action(menu, (entry.completed_at ? '标记未看完' : '标记看完') + suffix,
      () => api('/managed/retention', { ...identity, completed: !entry.completed_at }).then(() => ''));
    mark.title = '只影响本地缓存清理';
  }
  if (position > 0) direct(menu, '提前', 'up');
  if (position >= 0 && position < queueLength - 1) direct(menu, '推后', 'down');
  if (position > 1) direct(menu, '移到队首', 'first');
  if (!['complete','cancelled','removed','deleting'].includes(job.state)) direct(menu, '取消任务', 'cancel');
  if (!['removed','deleting'].includes(job.state)) {
    const confirm = node('div');
    confirm.hidden = true;
    confirm.append(node('p', '只删除本地文件。', 'hint'));
    const remove = action(confirm, '确认删除', async () => {
      await jobAction(job, 'delete');
      details.open = false;
      confirm.hidden = true;
      removeButton.hidden = false;
      return '已请求删除。';
    }, { className: 'danger' });
    const cancel = node('button', '返回');
    cancel.type = 'button';
    cancel.onclick = () => {
      confirm.hidden = true;
      removeButton.hidden = false;
      removeButton.focus();
    };
    confirm.append(cancel);
    const removeButton = node('button', '删除缓存', 'danger');
    removeButton.type = 'button';
    removeButton.onclick = () => {
      removeButton.hidden = true;
      confirm.hidden = false;
      remove.focus();
    };
    menu.append(removeButton, confirm);
  }
  if (job.state === 'removed' && managed.length) {
    // Catalog downloads need a fresh source after deletion.
    const entry = managed[0];
    toolbar.firstElementChild.remove();
    action(toolbar, '重新下载', async () => {
      const result = await api('/library/download', { session_id: entry.session_id, item_guids: [entry.item.guid] });
      if (!result.results.some(row => row.ok)) throw new Error('暂时无法重新下载。');
    });
  }
  if (menu.children.length) {
    details.append(node('summary', '更多'), menu);
    toolbar.append(details);
  }
  container.replaceChildren(toolbar);
}

for (const button of document.querySelectorAll('[data-filter]')) {
  button.onclick = () => {
    filter = button.dataset.filter;
    for (const candidate of document.querySelectorAll('[data-filter]')) {
      candidate.setAttribute('aria-pressed', String(candidate === button));
    }
    if (snapshot) renderDownloads();
  };
}

function renderOverview() {
  const { stats, automation } = snapshot;
  setMetrics('metrics', [
    ['代理请求', String(stats.requests)], ['本地供流', size(stats.cache_bytes)], ['回源读取', size(stats.origin_bytes)]
  ]);
  const playback = stats.playback || [];
  $('playback-empty').hidden = Boolean(playback.length);
  $('playback-table').hidden = !playback.length;
  $('playback').replaceChildren(...playback.map(item => {
    const row = node('tr');
    const title = automation.managed.find(entry => entry.download_id === item.download_id)?.item.title;
    cell(row, title || item.path, 'path');
    cell(row, item.active_requests ? '读取中' : '最近读过');
    cell(row, size(item.local_bytes), 'number');
    cell(row, size(item.upstream_bytes), 'number');
    return row;
  }));
}

async function refreshOverview(version) {
  const revision = ++routesVersion;
  const diagnosticsOpen = $('diagnostics').open;
  const [routes, observations] = await Promise.all([
    api('/routes'), diagnosticsOpen ? api('/observations') : Promise.resolve(null)
  ]);
  if (version !== connectionVersion || revision !== routesVersion || page !== 'overview') return;
  $('route-summary').textContent = routes.measuring ? '测速中…' : routes.selected
    ? `上次测速 ${new Date(routes.measured_at_ms).toLocaleTimeString()}` : '等待视频请求后测速。';
  $('probe-routes').disabled = probingRoutes || routes.measuring;
  const names = { unmeasured: '未测速', ready: '可用', failed: '失败', mismatch: '内容待验证' };
  $('routes').replaceChildren(...routes.results.map(route => {
    const row = node('tr');
    cell(row, route.origin, 'path');
    cell(row, route.origin === routes.selected ? '首选' : names[route.status] || route.status);
    cell(row, route.mib_per_second == null ? '—' : `${route.mib_per_second.toFixed(2)} MiB/s`, 'number');
    cell(row, route.elapsed_ms == null ? '—' : `${route.elapsed_ms} ms`, 'number');
    if (route.error) row.title = route.error;
    return row;
  }));
  if (observations) renderObservations(observations);
}

function renderObservations(observations) {
  const { stats } = snapshot;
  const summary = `缓存命中 ${stats.cache_hits} 次 · 跨域跳转 ${stats.external_redirects} 次`;
  $('diagnostic-summary').textContent = summary;
  $('observations-empty').hidden = Boolean(observations.length);
  $('observations-table').hidden = !observations.length;
  const rows = $('observations');
  const signature = JSON.stringify(observations.map(item => [item.id, item.path, item.status, item.total, item.cacheable]));
  if (rows.dataset.signature === signature) return;
  rows.dataset.signature = signature;
  rows.replaceChildren(...observations.map(item => {
    const row = node('tr');
    cell(row, item.path, 'path');
    cell(row, item.status == null ? '—' : String(item.status));
    cell(row, size(item.total), 'number');
    const toolbar = node('div', '', 'toolbar');
    cell(row).append(toolbar);
    if (item.cacheable && item.path.startsWith('/v/api/v1/media/range/')) {
      const sample = action(toolbar, '缓存样本', async () => {
        const saved = await api('/downloads', { observation_id: item.id, paused: true });
        const job = (await api('/downloads')).find(value => value.id === saved.id);
        if (job && ['queued', 'downloading'].includes(job.state)) return '已有完整下载任务。';
        const result = await api(`/downloads/${saved.id}/cache-samples`, {});
        return `已缓存 ${size(result.downloaded_bytes)} 样本。`;
      });
      sample.title = '最多读取 3 MiB';
    }
    if (item.cacheable) {
      action(toolbar, '完整下载', async () => {
        const saved = await api('/downloads', { observation_id: item.id });
        const job = (await api('/downloads')).find(value => value.id === saved.id);
        if (job && ['paused', 'failed', 'cancelled', 'removed'].includes(job.state)) await jobAction(job, 'resume');
        return job?.state === 'complete' ? '已完整缓存。' : '已加入队列。';
      });
    }
    return row;
  }));
}

$('diagnostics').ontoggle = () => { if ($('diagnostics').open) refresh(); };

$('probe-routes').onclick = async () => {
  probingRoutes = true;
  $('probe-routes').disabled = true;
  setNotice();
  try {
    await api('/routes/probe', {});
    setNotice('测速完成。');
  } catch (error) {
    setNotice(error.message || String(error));
  } finally {
    probingRoutes = false;
    $('probe-routes').disabled = false;
    refresh();
  }
};

function renderQueuePreview() {
  const { downloads, automation } = snapshot;
  const active = downloads.filter(job => job.state === 'downloading');
  const waiting = downloads.filter(job => waitingStates.has(job.state));
  const jobs = [...active, ...waiting].slice(0, 4);
  const list = $('queue-preview');
  $('queue-empty').hidden = Boolean(jobs.length);
  const rows = jobs.map(job => {
    const row = node('li');
    const managed = automation.managed.filter(entry => entry.download_id === job.id);
    row.append(node('span', jobTitle(job, managed), 'title'));
    const humanStatus = jobStates[job.state] || job.state;
    const progress = job.total && job.state === 'downloading' ? ` · ${Math.floor(100 * job.downloaded / job.total)}%` : '';
    row.append(node('small', humanStatus + progress));
    return row;
  });
  if (active.length + waiting.length > jobs.length) {
    rows.push(node('li', `另有 ${active.length + waiting.length - jobs.length} 项`, 'hint'));
  }
  list.replaceChildren(...rows);
}

function resetCatalog() {
  catalogRequest++;
  catalogLoading = false;
  catalogSubmitting = false;
  catalogView = { mode: 'search', value: '', page: 1, title: '' };
  catalogHistory = [];
  catalogItems = [];
  selected.clear();
  $('search-query').value = '';
  $('catalog-title').textContent = '全部';
  $('catalog-status').textContent = '';
  $('catalog').replaceChildren();
  $('catalog-table').hidden = true;
  $('catalog-footer').hidden = true;
  $('catalog-back').hidden = true;
  renderSelection();
}

const sessionNames = { needs_login: '请重新登录', offline: '暂不可用', needs_attention: '待验证' };

function renderSessions(sessions) {
  const select = $('session');
  const previous = select.value;
  const signature = JSON.stringify(sessions.map(session => [session.id, session.origin, session.status]));
  $('search').disabled = !sessions.length;
  $('search-query').disabled = !sessions.length;
  select.hidden = sessions.length < 2;
  if (signature === sessionsSignature) return;
  sessionsSignature = signature;
  select.replaceChildren(...sessions.map(session => {
    const label = sessionNames[session.status] || session.id.slice(0, 6);
    const option = node('option', `${session.origin} · ${label}`);
    option.value = session.id;
    return option;
  }));
  if (sessions.some(session => session.id === previous)) select.value = previous;
  if (!sessions.length) {
    resetCatalog();
    $('catalog-status').textContent = '请先让播放器通过代理登录。';
  } else if (previous !== select.value) {
    resetCatalog();
    loadCatalog(catalogView);
  }
}

function itemType(item) {
  const types = { TV: '剧集', Season: '季', Movie: '电影', Video: '视频', Directory: '目录', MediaDB: '媒体库' };
  if (item.kind === 'Episode') {
    const episode = item.episode_number ? `第 ${item.episode_number} 集` : '剧集';
    return item.season_number ? `第 ${item.season_number} 季 · ${episode}` : episode;
  }
  return types[item.kind] || item.kind;
}

const playable = item => ['Episode', 'Movie', 'Video'].includes(item.kind);

function updateCatalogStates() {
  if (!catalogItems.length || !snapshot) return;
  const session = $('session').value;
  const jobs = new Map(snapshot.downloads.map(job => [job.id, job]));
  const managed = new Map(snapshot.automation.managed.filter(entry => entry.session_id === session)
    .map(entry => [entry.item.guid, jobs.get(entry.download_id)]));
  Array.from($('catalog').rows).forEach((row, index) => {
    const item = catalogItems[index];
    const job = managed.get(item.guid);
    const state = job && job.state !== 'removed' ? jobStates[job.state] : item.watched ? '已看' : '';
    row.cells[3].textContent = state || '—';
  });
}

async function loadCatalog(next, navigation = 'replace') {
  const session = $('session').value;
  if (!session || catalogSubmitting) return;
  const request = ++catalogRequest;
  catalogLoading = true;
  renderSelection();
  $('catalog-status').textContent = '读取中…';
  try {
    const input = { session_id: session, page: next.page, ...(next.mode === 'search' ? { query: next.value } : { item_guid: next.value }) };
    const result = await api(next.mode === 'search' ? '/library/search' : '/library/browse', input);
    if (request !== catalogRequest) return;
    if (navigation === 'open') catalogHistory.push(catalogView);
    if (navigation === 'back') catalogHistory.pop();
    if (navigation === 'search') catalogHistory = [];
    catalogView = next;
    selected.clear();
    catalogItems = result.items;
    const trail = [...catalogHistory, next].map(view => view.title).filter(Boolean);
    $('catalog-title').textContent = trail.join(' / ') || (next.value ? '搜索结果' : '全部');
    $('catalog-back').hidden = !catalogHistory.length;
    $('catalog-status').textContent = result.items.length ? '' : '没有找到影片。';
    $('catalog-table').hidden = !result.items.length;
    $('catalog-footer').hidden = !result.items.length;
    $('catalog-prev').hidden = next.page <= 1;
    $('catalog-next').hidden = !result.has_more;
    $('catalog-page').hidden = next.page <= 1 && !result.has_more;
    $('catalog-page').textContent = `第 ${next.page} 页`;
    $('catalog').replaceChildren(...result.items.map(item => {
      const row = node('tr');
      const selection = cell(row);
      const title = item.title || item.guid;
      const titleCell = cell(row);
      if (playable(item)) {
        const checkbox = node('input');
        checkbox.type = 'checkbox';
        checkbox.setAttribute('aria-label', `选择${title}`);
        checkbox.dataset.guid = item.guid;
        checkbox.onchange = () => {
          if (checkbox.checked && selected.size >= 20) {
            checkbox.checked = false;
            $('catalog-status').textContent = '每次最多选 20 项。';
            return;
          }
          $('catalog-status').textContent = '';
          if (checkbox.checked) selected.add(item.guid);
          else selected.delete(item.guid);
          renderSelection();
        };
        selection.append(checkbox);
        titleCell.append(node('span', title, 'title'));
      } else {
        const open = node('button', title);
        open.type = 'button';
        open.onclick = () => loadCatalog({ mode: 'browse', value: item.guid, page: 1, title }, 'open');
        titleCell.append(open);
      }
      cell(row, itemType(item));
      cell(row);
      return row;
    }));
    updateCatalogStates();
  } catch (error) {
    if (request === catalogRequest) $('catalog-status').textContent = error.message || String(error);
  } finally {
    if (request === catalogRequest) {
      catalogLoading = false;
      renderSelection();
    }
  }
}

function renderSelection() {
  const button = $('download-selection');
  button.textContent = selected.size ? `加入队列（${selected.size}）` : '加入队列';
  button.disabled = !selected.size || catalogLoading || catalogSubmitting;
  $('session').disabled = catalogSubmitting;
}

$('catalog-search').onsubmit = event => {
  event.preventDefault();
  const query = $('search-query').value.trim();
  loadCatalog({ mode: 'search', value: query, page: 1, title: '' }, 'search');
};
$('session').onchange = () => {
  resetCatalog();
  loadCatalog(catalogView);
};
$('catalog-back').onclick = () => {
  const previous = catalogHistory[catalogHistory.length - 1];
  if (previous) loadCatalog(previous, 'back');
};
$('catalog-prev').onclick = () => loadCatalog({ ...catalogView, page: catalogView.page - 1 });
$('catalog-next').onclick = () => loadCatalog({ ...catalogView, page: catalogView.page + 1 });
$('download-selection').onclick = async () => {
  if (catalogSubmitting || !selected.size) return;
  catalogSubmitting = true;
  const request = catalogRequest;
  renderSelection();
  try {
    const { results } = await api('/library/download', { session_id: $('session').value, item_guids: Array.from(selected) });
    if (request !== catalogRequest) return;
    const successful = new Set(results.filter(result => result.ok).map(result => result.item_guid));
    for (const id of successful) selected.delete(id);
    for (const checkbox of $('catalog').querySelectorAll('input[type=checkbox]')) checkbox.checked = selected.has(checkbox.dataset.guid);
    const failed = results.length - successful.size;
    $('catalog-status').textContent = [successful.size ? `已加入 ${successful.size} 项。` : '', failed ? `${failed} 项暂时无法下载。` : ''].join('');
  } catch (error) {
    if (request === catalogRequest) $('catalog-status').textContent = error.message || String(error);
  } finally {
    if (request === catalogRequest) {
      catalogSubmitting = false;
      renderSelection();
      refresh();
    }
  }
};

const numericSettings = {
  'prefetch-count': ['prefetch_count', 1], 'rate-limit': ['bytes_per_second', MiB],
  'playback-cooldown': ['playback_cooldown_seconds', 1], 'cache-limit': ['cache_limit_bytes', GiB],
  'disk-reserve': ['reserved_disk_bytes', MiB], 'retention-hours': ['retention_seconds', 3600],
  'scheduler-interval': ['scheduler_interval_seconds', 1], 'route-ttl': ['route_result_ttl_seconds', 60],
  'route-timeout': ['route_timeout_seconds', 1], 'route-sample': ['route_sample_bytes', 1024],
  'retry-count': ['retry_max_attempts', 1], 'retry-base': ['retry_base_seconds', 1],
  'retry-max': ['retry_max_seconds', 1], 'session-check': ['session_check_seconds', 1],
  'resume-threshold': ['resume_detection_seconds', 1]
};
const checkboxSettings = {
  'pause-playing': 'pause_during_playback',
  'auto-prefetch': 'auto_prefetch',
  'cleanup-enabled': 'cleanup_enabled'
};

function historyNote() {
  const messages = {
    local: '最近播放和续播位置只保存在本机。',
    incognito: '停止上报播放记录。关闭前请先退出播放页，避免补报。'
  };
  $('history-note').textContent = messages[$('history-mode').value] || '';
}

function fillSettings(policy, cacheLimit) {
  $('history-mode').value = policy.incognito ? 'incognito' : policy.local_history ? 'local' : 'sync';
  for (const [id, key] of Object.entries(checkboxSettings)) $(id).checked = policy[key];
  const values = { ...policy, cache_limit_bytes: cacheLimit };
  for (const [id, [key, scale]] of Object.entries(numericSettings)) $(id).value = values[key] / scale;
  historyNote();
}

$('history-mode').onchange = historyNote;
$('policy-form').onsubmit = async event => {
  event.preventDefault();
  if (!snapshot) return;
  const mode = $('history-mode').value;
  const policy = { ...snapshot.automation.policy, incognito: mode === 'incognito', local_history: mode === 'local' };
  for (const [id, key] of Object.entries(checkboxSettings)) policy[key] = $(id).checked;
  for (const [id, [key, scale]] of Object.entries(numericSettings)) {
    policy[key] = Math.round(Number($(id).value) * scale);
  }
  $('save-settings').disabled = true;
  $('settings-state').textContent = '';
  try {
    await api('/automation/policy', policy);
    policyLoaded = false;
    $('settings-state').textContent = '已保存。';
    await refresh();
  } catch (error) {
    $('settings-state').textContent = error.message || String(error);
  } finally {
    $('save-settings').disabled = false;
  }
};

window.addEventListener('fn-backend-status', event => {
  const { status, restarted } = event.detail;
  const running = Boolean(status?.running);
  if (online !== running || restarted) resetConnection();
  const label = !status ? '后台不可用' : running ? '代理运行中' : status.configured ? '后台已停止' : '待配置';
  const proxy = running ? status.proxy_address.replace(/^https?:\/\//, '') : '';
  setOnline(running, label, proxy);
  if (status && !status.configured && !snapshot && page === 'tasks') showPage('settings');
  if (running) refresh();
});
setInterval(refresh, 1500);
