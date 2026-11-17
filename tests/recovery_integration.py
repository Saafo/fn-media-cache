#!/usr/bin/env python3
"""Bounded retries, restart recovery and verified same-account token replacement."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer

import native_integration as native
from integration import DATA, port, request, until

PAYLOAD=DATA[:1024*1024]
native.data_for=lambda _guid:PAYLOAD
TOKENS={'old-session':'account-a','renewed-session':'account-a','other-account':'account-b'}
revoked=set()
blocked=set()
permanent=set()
interrupted=set()
calls=[]


class Origin(native.Origin):
    def allowed(self):return self.headers.get('Authorization') in TOKENS and self.headers.get('Authorization') not in revoked
    def do_GET(self):
        if self.path=='/v/api/v1/session-context':
            return self.json({'code':0}) if self.allowed() else self.json({'code':-1},401)
        if self.path=='/v/api/v1/user/info':
            if not self.allowed():return self.json({'code':-1},401)
            if not self.signed():return self.json({'code':-1})
            return self.json({'code':0,'data':{'username':TOKENS[self.headers['Authorization']]}})
        if self.path.startswith('/v/api/v1/media/range/'):
            guid=self.path.rsplit('/',1)[-1];span=self.headers.get('Range','')
            if span.endswith('-'):
                calls.append((guid,span,time.time()))
                if guid in blocked:return self.json({'code':-1},503)
                if guid in permanent:return self.json({'code':-1},404)
                if guid in interrupted:
                    interrupted.remove(guid);native.state['interrupt']=True
        super().do_GET()


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--binary',type=Path,required=True)
    binary=parser.parse_args().binary.resolve()
    origin=ThreadingHTTPServer(('127.0.0.1',0),Origin)
    threading.Thread(target=origin.serve_forever,daemon=True).start()
    proxy,admin=port(),port();checks=[]
    with tempfile.TemporaryDirectory(prefix='fn-recovery-test-') as temp:
        root=Path(temp);config=root/'config.toml'
        config.write_text(f'upstream="http://127.0.0.1:{origin.server_port}"\nproxy_listen="127.0.0.1:{proxy}"\nadmin_listen="127.0.0.1:{admin}"\ndata_dir="data"\nlog_max_bytes=1024\nlog_keep_files=2\n')
        log=(root/'launcher.log').open('ab');process=None
        def start():
            child=subprocess.Popen([str(binary),'serve','--config',str(config)],stdout=log,stderr=log)
            until(lambda: child.poll() is None and request(admin,'/health')[0]==200)
            return child
        def stop():
            if process and process.poll() is None:
                process.send_signal(signal.SIGTERM) if os.name!='nt' else process.kill();process.wait(timeout=8)
        def api(path,body=None):
            headers={'Authorization':'Bearer '+(root/'data/admin-token').read_text().strip(),'Content-Type':'application/json'}
            status,_,raw=request(admin,'/api'+path,headers,'POST' if body is not None else 'GET',json.dumps(body) if body is not None else None)
            assert status==200,(path,status,raw)
            return json.loads(raw)
        def login(token):
            assert request(proxy,'/v/api/v1/session-context',{'Authorization':token})[0]==200
            api('/sessions/refresh',{})
        def capture(guid,paused=False):
            endpoint='/v/api/v1/media/range/'+guid
            assert request(proxy,endpoint,{'Authorization':'old-session','Range':'bytes=0-31'})[0]==206
            observed=next(row for row in api('/observations') if row['path']==endpoint)
            return api('/downloads',{'observation_id':observed['id'],'paused':paused})['id']
        def job(ident):return next(row for row in api('/downloads') if row['id']==ident)
        try:
            process=start();login('old-session')
            policy=api('/automation')['policy'];policy.update(bytes_per_second=0,retry_base_seconds=1,retry_max_seconds=3,retry_max_attempts=3,session_check_seconds=1,scheduler_interval_seconds=1,playback_cooldown_seconds=0)
            api('/automation/policy',policy)
            interrupted.add('resume')
            ident=capture('resume')
            until(lambda:job(ident)['state']=='retry_wait')
            assert job(ident)['attempts']==1 and job(ident)['retry_at']
            stop();process=start()
            until(lambda:job(ident)['state']=='complete')
            assert (root/f'data/cache/{ident}.bin').read_bytes()==PAYLOAD
            assert any(g=='resume' and span=='bytes=65536-' for g,span,_ in calls)
            checks.append('interrupted body is retried automatically, with persisted retry timing and exact resume bytes after restart')

            blocked.add('outage');outage=capture('outage')
            until(lambda:job(outage)['state']=='retry_wait')
            blocked.remove('outage')
            until(lambda:job(outage)['state']=='complete')
            checks.append('temporary HTTP outage recovers without a manual resume')

            permanent.add('missing');missing=capture('missing')
            until(lambda:job(missing)['state']=='failed')
            count=sum(g=='missing' for g,_,_ in calls);time.sleep(1.2)
            assert sum(g=='missing' for g,_,_ in calls)==count
            checks.append('permanent HTTP failures do not enter an endless retry loop')

            auth=capture('auth',True);api(f'/downloads/{auth}/cache-samples',{})
            revoked.add('old-session');api(f'/downloads/{auth}/resume',{})
            until(lambda:job(auth)['state']=='waiting_login')
            blocked.add('auth');login('other-account')
            assert job(auth)['state']=='waiting_login'
            assert request(proxy,'/v/api/v1/media/range/auth',{'Authorization':'other-account','Range':'bytes=0-31','X-Fn-Media-Cache-Only':'1'})[0]==504
            login('renewed-session')
            until(lambda:job(auth)['state'] in ('retry_wait','downloading'))
            status,headers,body=request(proxy,'/v/api/v1/media/range/auth',{'Authorization':'renewed-session','Range':'bytes=0-31','X-Fn-Media-Cache-Only':'1'})
            assert status==206 and headers['x-fn-media-cache']=='HIT' and body==PAYLOAD[:32]
            blocked.remove('auth');until(lambda:job(auth)['state']=='complete')
            assert len(api('/downloads'))==4
            checks.append('only a verified same-account renewal resumes old jobs and reuses old sample files; another account stays isolated')

            for _ in range(35):request(proxy,'/v/api/v1/session-context',{'Authorization':'renewed-session'})
            paths=list((root/'data/logs').glob('service.log*'))
            assert len(paths)<=3 and all(p.stat().st_size<=1024 for p in paths)
            content=''.join(p.read_text() for p in paths)
            assert all(token not in content for token in TOKENS)
            checks.append('operational logs rotate within configured limits and contain no session credentials')
        finally:
            stop();log.close();origin.shutdown()
    for i,check in enumerate(checks,1):print(f'PASS {i}: {check}')


if __name__=='__main__':main()
