'use strict';
if (window.__TAURI__?.core) {
  const invoke=window.__TAURI__.core.invoke;
  let initialized=false;
  const get=id=>document.getElementById(id);
  get('desktop-panel').hidden=false;
  get('login').hidden=true;
  async function updateDesktop() {
    try {
      const status=await invoke('service_status');
      const wasOnline=window.fnMediaCacheOnline;
      window.fnMediaCacheOnline=status.running;
      get('desktop-status').textContent=status.running?`后台运行中 · ${status.proxy_address}`:status.configured?'后台已停止':'请先配置服务器';
      if (!status.running) get('content').hidden=true;
      if (!initialized) {
        const settings=status.settings;
        get('server-origin').value=settings.upstream;
        get('server-alternates').value=settings.upstream_alternates.join('\n');
        get('server-port').value=settings.proxy_port;
        get('server-lan').checked=settings.lan_access;
        get('server-log-size').value=settings.log_max_megabytes;
        get('server-log-keep').value=settings.log_keep_files;
        get('server-settings').open=!status.configured;
        initialized=true;
      }
      if(status.last_error) get('server-message').textContent=status.last_error;
      if(status.running && !wasOnline) window.dispatchEvent(new Event('fn-backend-ready'));
    } catch(error) { get('desktop-status').textContent=`无法连接桌面服务：${error}`; }
  }
  async function action(button,command,args) {
    button.disabled=true;get('server-message').textContent='正在处理…';
    try { await invoke(command,args);get('server-message').textContent='已完成';await updateDesktop(); }
    catch(error) { get('server-message').textContent=String(error); }
    finally { button.disabled=false; }
  }
  get('service-logs').onclick=()=>action(get('service-logs'),'open_logs');
  get('server-save').onclick=()=>{
    const number=id=>{const input=get(id);if(!input.value || !input.checkValidity()) throw new Error('请输入有效的端口或日志配置');return Number(input.value);};
    try {
      const settings={upstream:get('server-origin').value.trim(),upstream_alternates:get('server-alternates').value.split('\n').map(v=>v.trim()).filter(Boolean),
        proxy_port:number('server-port'),lan_access:get('server-lan').checked,
        log_max_megabytes:number('server-log-size'),log_keep_files:number('server-log-keep')};
      action(get('server-save'),'save_server',{settings});
    } catch(error) { get('server-message').textContent=String(error); }
  };
  updateDesktop();setInterval(updateDesktop,3000);
}
