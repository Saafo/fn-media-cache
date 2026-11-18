'use strict';

if (window.__TAURI__?.core) {
  const invoke = window.__TAURI__.core.invoke;
  const get = id => document.getElementById(id);
  let initialized = false;
  let updating = false;
  let saving = false;
  get('desktop-panel').hidden = false;
  get('login').hidden = true;

  function applyStatus(status, restarted = false) {
    window.fnMediaCacheOnline = Boolean(status?.running);
    if (status && !initialized) {
      const settings = status.settings;
      get('server-origin').value = settings.upstream;
      get('server-alternates').value = settings.upstream_alternates.join('\n');
      get('server-port').value = settings.proxy_port;
      get('server-lan').checked = settings.lan_access;
      get('server-log-size').value = settings.log_max_megabytes;
      get('server-log-keep').value = settings.log_keep_files;
      initialized = true;
    }
    get('service-logs').hidden = !status?.configured;
    if (status?.last_error) get('server-message').textContent = status.last_error;
    window.dispatchEvent(new CustomEvent('fn-backend-status', { detail: { status, restarted } }));
  }

  async function updateDesktop() {
    if (updating || saving) return;
    updating = true;
    try {
      const status = await invoke('service_status');
      if (!saving) applyStatus(status);
    } catch {
      if (!saving) applyStatus(null);
    } finally {
      updating = false;
    }
  }

  get('service-logs').onclick = async () => {
    try {
      await invoke('open_logs');
    } catch (error) {
      get('message').textContent = String(error);
    }
  };

  get('desktop-panel').onsubmit = async event => {
    event.preventDefault();
    if (saving) return;
    const upstream = get('server-origin').value.trim();
    if (!upstream) return;
    const settings = {
      upstream,
      upstream_alternates: get('server-alternates').value.split('\n').map(value => value.trim()).filter(Boolean),
      proxy_port: Number(get('server-port').value),
      lan_access: get('server-lan').checked,
      log_max_megabytes: Number(get('server-log-size').value),
      log_keep_files: Number(get('server-log-keep').value)
    };
    saving = true;
    get('server-save').disabled = true;
    get('server-message').textContent = '正在保存…';
    try {
      const status = await invoke('save_server', { settings });
      applyStatus(status, true);
      get('server-message').textContent = '已保存。';
    } catch (error) {
      get('server-message').textContent = String(error);
    } finally {
      saving = false;
      get('server-save').disabled = false;
    }
  };

  updateDesktop();
  setInterval(updateDesktop, 3000);
}
