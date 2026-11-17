#!/usr/bin/env python3
"""Incognito suppresses known viewing writes, without breaking media transfer."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
from http.server import ThreadingHTTPServer

from automation_integration import CatalogOrigin
from native_integration import TOKEN
from integration import port, request, until

calls = []


class Spy(CatalogOrigin):
    def do_POST(self):
        if self.path.endswith('/play/record') or self.path.endswith('/item/watched') or self.path.endswith('/playing') or self.path.endswith('/play/play'):
            raw = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            calls.append((self.command, self.path, raw))
            return self.json({'code':0,'data':True})
        super().do_POST()

    def do_DELETE(self):
        raw = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        calls.append((self.command,self.path,raw))
        self.json({'code':0,'data':True})


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binary',type=Path,required=True)
    binary=parser.parse_args().binary.resolve()
    origin=ThreadingHTTPServer(('127.0.0.1',0),Spy)
    threading.Thread(target=origin.serve_forever,daemon=True).start()
    proxy,admin=port(),port()
    checks=[]
    with tempfile.TemporaryDirectory(prefix='fn-privacy-test-') as temp:
        root=Path(temp)
        config=root/'config.toml'
        config.write_text(f'upstream="http://127.0.0.1:{origin.server_port}"\nproxy_listen="127.0.0.1:{proxy}"\nadmin_listen="127.0.0.1:{admin}"\ndata_dir="data"\n')
        log=(root/'service.log').open('ab')
        process=None
        def start():
            child=subprocess.Popen([str(binary),'serve','--config',str(config)],stdout=log,stderr=log)
            until(lambda: child.poll() is None and request(admin,'/health')[0]==200)
            return child
        def stop():
            if process and process.poll() is None:
                process.send_signal(signal.SIGTERM) if os.name!='nt' else process.kill()
                process.wait(timeout=8)
        def api(path,body=None):
            headers={'Authorization':'Bearer '+(root/'data/admin-token').read_text().strip(),'Content-Type':'application/json'}
            status,_,raw=request(admin,'/api'+path,headers,'POST' if body is not None else 'GET',json.dumps(body) if body is not None else None)
            assert status==200,(status,raw)
            return json.loads(raw)
        def mode(enabled):
            policy=api('/automation')['policy'];policy.update(incognito=enabled,auto_prefetch=enabled)
            api('/automation/policy',policy)
        headers={'Authorization':TOKEN,'Content-Type':'application/json'}
        try:
            process=start()
            assert request(proxy,'/v/api/v1/play/record',headers,'POST','{"item_guid":"one","media_guid":"one","ts":7}')[0]==200
            assert len(calls)==1 and len(api('/progress-events'))==1
            mode(True)
            assert len(calls)==1 and not api('/progress-events')
            for method,path in [('POST','/v/api/v1/play/record'),('POST','/v/api/v2/play/record'),('POST','/v/api/v1/item/watched'),('DELETE','/v/api/v1/item/watched'),('POST','/v/api/v1/item/one/playing')]:
                status,response_headers,raw=request(proxy,path,headers,method,'{"item_guid":"one","media_guid":"one","ts":123456789}')
                assert status==200 and json.loads(raw)['code']==0
                assert response_headers['x-fn-media-cache-privacy']=='suppressed'
            assert len(calls)==1 and not api('/progress-events')
            checks.append('known progress, watched and playing writes get local success responses and never reach the upstream')

            status,_,body=request(proxy,'/v/api/v1/media/range/one',{'Authorization':TOKEN,'Range':'bytes=0-31'})
            assert status==206 and len(body)==32
            assert request(proxy,'/v/api/v1/play/play',headers,'POST','{}')[0]==200
            assert len(calls)==2 and calls[-1][1].endswith('/play/play')
            assert not api('/observations') and not api('/progress-events') and not api('/downloads')
            checks.append('media bytes and playback setup still work; incognito requests create no observations, progress events or prefetch tasks')

            stop();process=start()
            assert api('/automation')['policy']['incognito'] is True
            assert request(proxy,'/v/api/v1/play/record',headers,'POST','{}')[0]==200
            assert len(calls)==2
            mode(False)
            request(proxy,'/v/api/v1/play/record',headers,'POST','{"item_guid":"one","media_guid":"one","ts":8}')
            assert len(calls)==3 and len(api('/progress-events'))==1
            checks.append('mode survives restart; turning it off forwards only new reports and does not replay suppressed ones')

            log.flush()
            assert '123456789' not in (root/'service.log').read_text()
            assert json.loads(calls[0][2])['ts']==7
            checks.append('old upstream history stays untouched and hidden progress is absent from local logs')
        finally:
            stop();log.close();origin.shutdown()
    for index,check in enumerate(checks,1):print(f'PASS {index}: {check}')


if __name__=='__main__':main()
