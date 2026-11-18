'use strict';
let token = '';
const nativeBridge=window.__TAURI__?.core;
let polling = false;
let lastNotice = '';
let pendingDeleteId = null;
let settingsLoaded = false;
let activePolicy = null;
let catalogStarted = false;
let catalogView = {mode:'search',value:'',page:1};
let catalogHistory = [];
let catalogSelection = new Set();
const $ = id => document.getElementById(id);
const sizes = bytes => bytes == null ? '未知' : bytes < 1024 ? `${bytes} B` : bytes < 1048576 ? `${(bytes / 1024).toFixed(1)} KiB` : bytes < 1073741824 ? `${(bytes / 1048576).toFixed(1)} MiB` : `${(bytes / 1073741824).toFixed(2)} GiB`;
const duration = seconds => seconds == null ? '估算中' : seconds < 60 ? `${seconds} 秒` : seconds < 3600 ? `${Math.ceil(seconds / 60)} 分钟` : `${(seconds / 3600).toFixed(1)} 小时`;
async function api(path, body) {
  if (nativeBridge) {
    const result=await nativeBridge.invoke('admin_request',{path,body:body??null});
    if (result.status<200 || result.status>=300) throw new Error(`${result.status}: ${typeof result.body==='string'?result.body:JSON.stringify(result.body)}`);
    return result.body;
  }
  const response = await fetch(`/api${path}`, {method: body ? 'POST' : 'GET', headers: {Authorization: `Bearer ${token}`, ...(body ? {'Content-Type': 'application/json'} : {})}, body: body ? JSON.stringify(body) : undefined});
  if (!response.ok) throw new Error(`${response.status}: ${await response.text()}`);
  return response.json();
}
function cell(row, value, className) { const td = document.createElement('td'); td.textContent = value; if (className) td.className = className; row.append(td); return td; }
function action(parent, title, callback) {
  const button = document.createElement('button'); button.textContent = title;
  button.onclick = async () => { button.disabled = true; try { await callback(); await refresh(); } catch (e) { $('message').textContent = e.message; } finally { button.disabled = false; } };
  parent.append(button);
}
async function refresh() {
  if (polling || (!nativeBridge && !token) || (nativeBridge && !window.fnMediaCacheOnline)) return;
  polling = true;
  try {
    const [stats, observations, downloads, routes, automation, sessions] = await Promise.all([api('/stats'), api('/observations'), api('/downloads'), api('/routes'), api('/automation'), api('/sessions')]);
    activePolicy = automation.policy;
    $('privacy-banner').hidden=!activePolicy.incognito;
    $('local-history-banner').hidden=!activePolicy.local_history;
    if (!settingsLoaded) { fillSettings(activePolicy, automation.cache_limit_bytes); settingsLoaded = true; }
    const currentSession = $('session').value;
    const sessionSignature=sessions.map(item=>`${item.id}:${item.status}`).join(',');
    if ($('session').dataset.signature !== sessionSignature) {
      $('session').replaceChildren(...sessions.map(item => { const option=document.createElement('option'); option.value=item.id; option.textContent=`${item.origin} · ${{needs_login:'需要重新登录',offline:'连接暂不可用',needs_attention:'会话待验证'}[item.status]||`会话 ${item.id.slice(0,6)}`}`; return option; }));
      if (sessions.some(item => item.id === currentSession)) $('session').value=currentSession;
      $('session').dataset.signature=sessionSignature;
    }
    $('login').hidden = true; $('content').hidden = false; $('message').textContent = lastNotice;
    $('metrics').replaceChildren();
    for (const [label, value] of [['请求', stats.requests], ['缓存命中', stats.cache_hits], ['上游读取', sizes(stats.origin_bytes)], ['本地供流', sizes(stats.cache_bytes)], ['跨域跳转', stats.external_redirects]]) {
      const div = document.createElement('div'); div.className = 'metric muted'; div.textContent = label; const b = document.createElement('b'); b.textContent = value; div.append(b); $('metrics').append(div);
    }
    $('playback').replaceChildren();
    for (const item of stats.playback || []) {
      const title = automation.managed.find(value => value.download_id === item.download_id)?.item.title;
      const row = document.createElement('tr');
      cell(row, title || item.path, 'path');
      cell(row, item.active_requests ? `传输中（${item.active_requests}）` : '最近读过');
      cell(row, sizes(item.local_bytes)); cell(row, sizes(item.upstream_bytes));
      const total = item.local_bytes + item.upstream_bytes;
      cell(row, total ? `${(100 * item.local_bytes / total).toFixed(1)}%` : '—');
      $('playback').append(row);
    }
    $('route-summary').textContent = routes.measuring ? `正在依次测速，每条线路最多读取 ${sizes(activePolicy.route_sample_bytes)}。` : routes.selected ? `下载首选：${routes.selected}。测速时间：${new Date(routes.measured_at_ms).toLocaleTimeString()}；结果保留 ${activePolicy.route_result_ttl_seconds/60} 分钟，下次下载时按需重测。` : '等待可用的视频测速结果。登录和观看记录仍走主地址。';
    $('routes').replaceChildren();
    const routeNames = {unmeasured: '未测速', ready: '可用', failed: '失败', mismatch: '内容不一致或未验证'};
    for (const route of routes.results) {
      const row = document.createElement('tr');
      cell(row, route.origin); cell(row, `${routeNames[route.status] || route.status}${route.origin === routes.selected ? ' · 首选' : ''}`);
      cell(row, route.mib_per_second == null ? '—' : `${route.mib_per_second.toFixed(2)} MiB/s`);
      cell(row, route.elapsed_ms == null ? '—' : `${route.elapsed_ms} ms`);
      if (route.error) row.title = route.error;
      $('routes').append(row);
    }
    $('observations').replaceChildren();
    for (const item of observations) {
      const row = document.createElement('tr'); cell(row, item.path, 'path'); cell(row, item.status); cell(row, sizes(item.total)); const td = cell(row, '');
      if (item.cacheable && item.path.startsWith('/v/api/v1/media/range/')) action(td, '样本缓存', async () => {
        const saved = await api('/downloads', {observation_id: item.id, paused: true});
        const job = (await api('/downloads')).find(value => value.id === saved.id);
        if (job && ['queued', 'downloading'].includes(job.state)) {
          lastNotice = '这份视频已有完整下载任务，未另外执行样本缓存。';
          return;
        }
        const result = await api(`/downloads/${saved.id}/cache-samples`, {});
        lastNotice = `已保存 ${sizes(result.downloaded_bytes)} 样本，未启动整部下载。`;
      });
      if (item.cacheable) action(td, '完整下载', async () => {
        const saved = await api('/downloads', {observation_id: item.id});
        const job = (await api('/downloads')).find(value => value.id === saved.id);
        if (job && ['paused', 'failed', 'cancelled', 'removed'].includes(job.state)) await api(`/downloads/${saved.id}/resume`, {});
        lastNotice = job?.state === 'complete' ? '这份视频已经完整缓存。' : '完整下载任务已提交。';
      });
      $('observations').append(row);
    }
    const names = {queued:'排队',downloading:'下载中',complete:'已缓存',paused:'已暂停',failed:'失败',waiting_space:'等待空间',waiting_login:'等待同账号重新登录',retry_wait:'等待自动重试',deleting:'等待释放缓存',removed:'已清理',cancelled:'已取消'};
    $('download-status').textContent = automation.playing && activePolicy.pause_during_playback ? '播放器正在读取视频，后台下载等待播放结束。' : `后台限速：${activePolicy.bytes_per_second ? sizes(activePolicy.bytes_per_second)+'/s' : '不限速'}；看完后保留 ${activePolicy.retention_seconds/3600} 小时。`;
    $('downloads').replaceChildren();
    for (const item of downloads) {
      const managed = automation.managed.filter(value => value.download_id === item.id);
      const row = document.createElement('tr'); cell(row, managed.length ? managed.map(value=>value.item.title || value.item.guid).join(' / ') : item.path, 'path'); cell(row, `${names[item.state] || item.state}${item.error ? ` · ${item.error}` : ''}`);
      if(item.retry_at) row.cells[1].append(document.createTextNode(` · 下次 ${new Date(item.retry_at*1000).toLocaleTimeString()}（第 ${item.attempts} 次）`));
      cell(row, `${sizes(item.downloaded)} / ${sizes(item.total)}`);
      cell(row, item.waiting_reason === 'playback' ? '为播放器让行' : item.state === 'downloading' ? `${sizes(item.bytes_per_second)}/s · ${duration(item.eta_seconds)}` : '—');
      const td = cell(row, '');
      if (['queued','paused','failed','retry_wait','waiting_space','waiting_login'].includes(item.state)) {
        action(td, '提前', () => api(`/downloads/${item.id}/up`, {}));
        action(td, '推后', () => api(`/downloads/${item.id}/down`, {}));
        action(td, '优先下载', () => api(`/downloads/${item.id}/first`, {}));
      }
      if (!['complete','cancelled','removed','deleting'].includes(item.state)) action(td, '取消任务', () => api(`/downloads/${item.id}/cancel`, {}));
      if (!['removed','deleting'].includes(item.state)) {
        if (pendingDeleteId === item.id) {
          const note=document.createElement('div'); note.textContent='删除本地文件并停止下载？上游影片不受影响。'; note.className='muted'; td.append(note);
          action(td, '确认删除', async () => {
            await api(`/downloads/${item.id}/delete`, {}); pendingDeleteId = null;
            lastNotice = '已请求删除本地缓存，使用中的文件会在读取结束后释放。';
          });
          action(td, '保留缓存', () => { pendingDeleteId = null; });
        } else action(td, '删除缓存', () => { pendingDeleteId = item.id; });
      }
      if (['queued','downloading','waiting_space','retry_wait','waiting_login'].includes(item.state)) action(td, '暂停', () => api(`/downloads/${item.id}/pause`, {}));
      if (['paused','failed','retry_wait','waiting_login','cancelled','removed'].includes(item.state)) action(td, item.state === 'removed' ? '重新下载' : '继续下载', () => api(`/downloads/${item.id}/resume`, {}));
      for (const entry of managed) {
        const identity={session_id:entry.session_id,item_guid:entry.item.guid};
        action(td, entry.keep?'取消保留':'保留', () => api('/managed/retention',{...identity,keep:!entry.keep}));
        action(td, entry.completed_at?'本地标记未看完':'本地标记看完', () => api('/managed/retention',{...identity,completed:!entry.completed_at}));
        if (item.state==='removed') action(td,'重新下载',()=>api('/library/download',{session_id:entry.session_id,item_guids:[entry.item.guid]}));
        if (entry.completed_at) { const note=document.createElement('div'); note.className='muted'; note.textContent=entry.keep?'已保留，不自动清理':`清理时间：${new Date((entry.completed_at+activePolicy.retention_seconds)*1000).toLocaleString()}`; td.append(note); }
      }
      $('downloads').append(row);
    }
    if (!catalogStarted && sessions.length) { catalogStarted=true; await loadCatalog('search','',1,false); }
  } catch (e) { $('message').textContent = `连接失败：${e.message}`; }
  finally { polling = false; }
}
$('connect').onclick = () => { token = $('token').value.trim(); $('token').value = ''; refresh(); };
$('token').addEventListener('keydown', e => { if (e.key === 'Enter') $('connect').click(); });
function fillSettings(policy, cacheLimit) {
  $('history-mode').value=policy.incognito ? 'incognito' : policy.local_history ? 'local' : 'sync';
  $('auto-prefetch').checked=policy.auto_prefetch;
  $('pause-playing').checked=policy.pause_during_playback;
  $('cleanup-enabled').checked=policy.cleanup_enabled;
  const values={
    'prefetch-count':policy.prefetch_count,'rate-limit':policy.bytes_per_second/1048576,
    'playback-cooldown':policy.playback_cooldown_seconds,'cache-limit':cacheLimit/1073741824,
    'disk-reserve':policy.reserved_disk_bytes/1048576,'retention-hours':policy.retention_seconds/3600,
    'scheduler-interval':policy.scheduler_interval_seconds,'route-ttl':policy.route_result_ttl_seconds/60,
    'route-timeout':policy.route_timeout_seconds,'route-sample':policy.route_sample_bytes/1024,
    'retry-count':policy.retry_max_attempts,'retry-base':policy.retry_base_seconds,'retry-max':policy.retry_max_seconds,
    'session-check':policy.session_check_seconds,'resume-threshold':policy.resume_detection_seconds,
  };
  for (const [key,value] of Object.entries(values)) $(key).value=value;
}

async function loadCatalog(mode,value,page=1,push=true) {
  if (!$('session').value) { $('catalog-status').textContent='先在播放器里通过代理登录一次。'; return; }
  $('catalog-status').textContent='正在读取…';
  const input={session_id:$('session').value,page,...(mode==='search'?{query:value}:{item_guid:value})};
  const result=await api(mode==='search'?'/library/search':'/library/browse',input);
  if (push) catalogHistory.push({...catalogView});
  catalogView={mode,value,page}; catalogSelection.clear(); renderSelection();
  $('catalog').replaceChildren();
  const kinds={TV:'剧集',Season:'季',Episode:'集',Movie:'电影',Video:'视频',Directory:'目录',MediaDB:'媒体库'};
  for (const item of result.items) {
    const row=document.createElement('tr'); const select=cell(row,'');
    const playable=['Episode','Movie','Video'].includes(item.kind);
    if (playable) {
      const checkbox=document.createElement('input'); checkbox.type='checkbox'; checkbox.setAttribute('aria-label',`选择 ${item.title}`);
      checkbox.onchange=()=>{ if (checkbox.checked && catalogSelection.size>=20) { checkbox.checked=false; $('catalog-status').textContent='每次最多选择 20 项。'; return; } checkbox.checked?catalogSelection.add(item.guid):catalogSelection.delete(item.guid); renderSelection(); };
      select.append(checkbox);
    }
    cell(row,item.title || item.guid,'path');
    cell(row,item.kind==='Episode'?`S${item.season_number} E${item.episode_number}${item.watched===true?' · 已看':''}`:(kinds[item.kind]||item.kind));
    const controls=cell(row,'');
    if (playable) action(controls,'下载',()=>queueSelection([item.guid]));
    else action(controls,'打开',()=>loadCatalog('browse',item.guid));
    $('catalog').append(row);
  }
  $('catalog-status').textContent=result.items.length?`当前显示 ${result.items.length} 项`:'没有找到可用内容。';
  $('catalog-page').textContent=`第 ${page} 页`;
  $('catalog-prev').disabled=page<=1;
  $('catalog-next').disabled=!result.has_more;
  $('catalog-back').disabled=!catalogHistory.length;
}

function renderSelection() { $('download-selection').textContent=`下载选中项（${catalogSelection.size}）`; $('download-selection').disabled=!catalogSelection.size; }
async function queueSelection(ids) {
  const result=await api('/library/download',{session_id:$('session').value,item_guids:ids});
  const successful=result.results.filter(item=>item.ok).length;
  lastNotice=`已提交 ${successful} 项下载${successful<ids.length?`，${ids.length-successful} 项无法取得原始文件`:''}。`;
  await refresh();
}
async function catalogAction(callback) { try { await callback(); } catch(e) { $('catalog-status').textContent=`读取失败：${e.message}`; } }
$('search').onclick=()=>catalogAction(()=>loadCatalog('search',$('search-query').value.trim()));
$('search-query').addEventListener('keydown',e=>{if(e.key==='Enter') $('search').click();});
$('library-root').onclick=()=>catalogAction(()=>loadCatalog('search',''));
$('session').onchange=()=>{catalogHistory=[];catalogAction(()=>loadCatalog('search','',1,false));};
$('catalog-back').onclick=()=>{const previous=catalogHistory.pop();if(previous)catalogAction(()=>loadCatalog(previous.mode,previous.value,previous.page,false));};
$('catalog-prev').onclick=()=>catalogAction(()=>loadCatalog(catalogView.mode,catalogView.value,catalogView.page-1,false));
$('catalog-next').onclick=()=>catalogAction(()=>loadCatalog(catalogView.mode,catalogView.value,catalogView.page+1,false));
$('download-selection').onclick=()=>catalogAction(()=>queueSelection(Array.from(catalogSelection)));
$('save-settings').onclick=async()=>{
  const number=id=>{const input=$(id);const value=Number(input.value);if(!input.value.trim() || !Number.isFinite(value) || !input.checkValidity())throw new Error('请输入范围内的有效数字');return value;};
  $('save-settings').disabled=true;
  try {
    const policy={...activePolicy,
      incognito:$('history-mode').value==='incognito',local_history:$('history-mode').value==='local',
      auto_prefetch:$('auto-prefetch').checked,pause_during_playback:$('pause-playing').checked,cleanup_enabled:$('cleanup-enabled').checked,
      prefetch_count:number('prefetch-count'),bytes_per_second:Math.round(number('rate-limit')*1048576),
      playback_cooldown_seconds:number('playback-cooldown'),cache_limit_bytes:Math.round(number('cache-limit')*1073741824),
      reserved_disk_bytes:Math.round(number('disk-reserve')*1048576),retention_seconds:Math.round(number('retention-hours')*3600),
      scheduler_interval_seconds:number('scheduler-interval'),route_result_ttl_seconds:Math.round(number('route-ttl')*60),
      route_timeout_seconds:number('route-timeout'),route_sample_bytes:Math.round(number('route-sample')*1024),
      retry_max_attempts:number('retry-count'),retry_base_seconds:number('retry-base'),retry_max_seconds:number('retry-max'),
      session_check_seconds:number('session-check'),resume_detection_seconds:number('resume-threshold'),
    };
    await api('/automation/policy',policy); settingsLoaded=false; $('settings-state').textContent='已保存并生效'; await refresh();
  } catch(e) { $('settings-state').textContent=`保存失败：${e.message}`; }
  finally { $('save-settings').disabled=false; }
};
$('probe-routes').onclick = async () => {
  $('probe-routes').disabled = true;
  try {
    await api('/routes/probe', {});
    lastNotice = '线路测速完成，后续下载会优先使用最快的已验证线路。';
    await refresh();
  } catch (e) { $('message').textContent = e.message; }
  finally { $('probe-routes').disabled = false; }
};
setInterval(refresh, 1500);
window.addEventListener('fn-backend-ready',()=>{settingsLoaded=false;catalogStarted=false;refresh();});
