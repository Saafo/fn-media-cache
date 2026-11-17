#!/usr/bin/env python3
"""Task controls and prefix playback, against synthetic loopback servers only."""
import argparse
import http.client
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer

import native_integration as native
import integration as generic
from integration import DATA, AUTH, port, request, until

calls = []
held = threading.Event()
release = threading.Event()
hold_span = 'bytes=65536-1048575'

class Origin(native.Origin):
    reply = generic.Fixture.reply
    def do_HEAD(self):
        if self.path.startswith('/v/'): return super().do_HEAD()
        generic.Fixture.respond(self, True)
    def do_GET(self):
        if not self.path.startswith('/v/'):
            return generic.Fixture.respond(self, False)
        span = self.headers.get('Range', '')
        if self.path.startswith('/v/api/v1/media/range/'):
            calls.append((self.path.rsplit('/', 1)[-1], span))
            if span == hold_span:
                payload = native.data_for('held')
                self.send_response(206)
                self.send_header('Content-Type', 'video/mp4')
                self.send_header('Content-Range', f'bytes 65536-1048575/{len(payload)}')
                self.send_header('Content-Length', str(1048576-65536))
                self.end_headers()
                held.set()
                release.wait(8)
                self.wfile.write(payload[65536:1048576])
                return
        super().do_GET()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True, type=Path)
    binary = parser.parse_args().binary.resolve()
    origin = ThreadingHTTPServer(('127.0.0.1', 0), Origin)
    threading.Thread(target=origin.serve_forever, daemon=True).start()
    proxy, admin = port(), port()
    checks = []
    with tempfile.TemporaryDirectory(prefix='fn-task-playback-') as temp:
        root = Path(temp)
        config = root/'config.toml'
        config.write_text(f'upstream="http://127.0.0.1:{origin.server_port}"\nproxy_listen="127.0.0.1:{proxy}"\nadmin_listen="127.0.0.1:{admin}"\ndata_dir="data"\n')
        log = (root/'service-output.log').open('ab')
        process = None
        def start():
            child = subprocess.Popen([str(binary), 'serve', '--config', str(config)], stdout=log, stderr=log)
            until(lambda: child.poll() is None and request(admin, '/health')[0] == 200)
            return child
        def stop():
            if process and process.poll() is None:
                process.send_signal(signal.SIGTERM) if os.name != 'nt' else process.kill()
                process.wait(timeout=10)
        def api(path, body=None):
            headers = {'Authorization':'Bearer '+(root/'data/admin-token').read_text().strip(), 'Content-Type':'application/json'}
            status, _, payload = request(admin, '/api'+path, headers, 'POST' if body is not None else 'GET', json.dumps(body) if body is not None else None)
            assert status == 200, (path, status, payload)
            return json.loads(payload)
        def job(ident): return next(item for item in api('/downloads') if item['id'] == ident)
        def capture(guid, paused=True, generic_path=False):
            path = '/'+guid+'.mp4' if generic_path else '/v/api/v1/media/range/'+guid
            token = AUTH if generic_path else native.TOKEN
            assert request(proxy, path, {'Authorization':token, 'Range':'bytes=0-31'})[0] == 206
            found = next(item for item in api('/observations') if item['path'] == path)
            ident = api('/downloads', {'observation_id':found['id'], 'paused':paused})['id']
            return ident, path, token
        def prefix(guid, generic_path=False):
            if generic_path: generic.state['interrupt'] = '/'+guid+'.mp4'
            else: native.state['interrupt'] = True
            ident, path, token = capture(guid, False, generic_path)
            until(lambda: job(ident)['state'] == 'failed')
            assert 0 < job(ident)['downloaded'] < len(DATA)
            api(f'/downloads/{ident}/cancel', {})
            return ident, path, token
        try:
            process = start()
            policy = api('/automation')['policy']
            policy.update(bytes_per_second=0, retry_max_attempts=0, pause_during_playback=False, scheduler_interval_seconds=1)
            api('/automation/policy', policy)
            ident, path, token = prefix('prefix')
            size = job(ident)['downloaded']
            native.state['block'] = True
            status, headers, body = request(proxy, path, {'Authorization':token,'Range':'bytes=0-31','X-Fn-Media-Cache-Only':'1'})
            assert status == 206 and headers['x-fn-media-cache-kind'] == 'partial-file' and body == native.data_for('prefix')[:32]
            assert request(proxy, path, {'Authorization':token,'Range':f'bytes={size-1}-{size+31}','X-Fn-Media-Cache-Only':'1'})[0] == 504
            native.state['block'] = False
            before = len(calls)
            status, headers, body = request(proxy, path, {'Authorization':token,'Range':f'bytes={size-32}-{size+31}'})
            assert status == 206 and headers['x-fn-media-cache'] == 'PARTIAL'
            assert body == native.data_for('prefix')[size-32:size+32]
            assert ('prefix', f'bytes={size}-{size+31}') in calls[before:]
            status, headers, body = request(proxy, path, {'Authorization':token,'Range':'bytes=0-'})
            assert status == 206 and body == native.data_for('prefix') and headers['x-fn-media-cache'] == 'PARTIAL'
            playback = next(p for p in api('/stats')['playback'] if p['download_id'] == ident)
            assert playback['local_bytes'] >= size and playback['upstream_bytes'] > 0
            assert native.TOKEN not in json.dumps(api('/stats'))
            checks.append('native cached prefix, cross-boundary and open-ended ranges return exact bytes; only the missing suffix is fetched')

            native.state['version'] = 2
            assert request(proxy,path,{'Authorization':token,'Range':'bytes=0-31','X-Fn-Media-Cache-Only':'1'})[0] == 504
            native.state['version'] = 1
            native.state['allowed'] = False
            assert request(proxy,path,{'Authorization':token,'Range':'bytes=0-31','X-Fn-Media-Cache-Only':'1'})[0] == 504
            native.state['allowed'] = True
            checks.append('partial cache retains file-version and authorization checks')

            generic_id, generic_path, generic_token = prefix('generic', True)
            size = job(generic_id)['downloaded']
            status, headers, body = request(proxy,generic_path,{'Authorization':generic_token,'Range':f'bytes={size-8}-{size+7}'})
            assert status == 206 and headers['x-fn-media-cache'] == 'PARTIAL' and body == DATA[size-8:size+8]
            generic.state['version'] = '"changed"'
            assert request(proxy,generic_path,{'Authorization':generic_token,'Range':'bytes=0-7','X-Fn-Media-Cache-Only':'1'})[0] == 504
            generic.state['version'] = '"v1"'
            checks.append('generic ETag-backed partial files follow the same range rules')

            a, _, _ = capture('queue-a'); b, _, _ = capture('queue-b'); c, _, _ = capture('queue-c')
            api(f'/downloads/{c}/first', {})
            api(f'/downloads/{a}/down', {})
            expected = [c,b,a]
            assert [p['id'] for p in api('/downloads') if p['id'] in expected] == expected
            stop()
            with sqlite3.connect(root/'data/state.sqlite3') as db:
                db.execute("UPDATE downloads SET state='queued' WHERE id IN (?,?,?)", expected)
            policy['bytes_per_second'] = 128*1024
            # Update the persisted policy without starting a job before the restart.
            with sqlite3.connect(root/'data/state.sqlite3') as db:
                db.execute("UPDATE settings SET value=? WHERE key='automation_policy'", [json.dumps(policy)])
            process = start()
            until(lambda:job(c)['state']=='downloading' and job(c)['downloaded']>0)
            assert job(c)['bytes_per_second'] > 0 and job(c)['eta_seconds'] is not None
            api(f'/downloads/{c}/cancel', {})
            until(lambda:job(b)['state']=='downloading')
            api(f'/downloads/{b}/cancel', {}); api(f'/downloads/{a}/cancel', {})
            stop(); process=start()
            assert all(job(item)['state']=='cancelled' for item in expected)
            checks.append('queue order survives restart; speed and ETA are reported; cancellation does not auto-resume')

            # A new file version must not truncate a prefix an older reader still uses.
            reset_id, reset_path, reset_token = prefix('generation')
            held.clear(); release.clear()
            connection = http.client.HTTPConnection('127.0.0.1',proxy,timeout=10)
            connection.request('GET',reset_path,headers={'Authorization':reset_token,'Range':'bytes=0-1048575'})
            response=connection.getresponse(); first=response.read(32)
            assert held.wait(2)
            original=native.data_for('generation')[:1048576]
            native.state['version']=2
            policy=api('/automation')['policy'];policy['bytes_per_second']=0;api('/automation/policy',policy)
            api(f'/downloads/{reset_id}/resume', {})
            time.sleep(0.15)
            assert (root/f'data/cache/{reset_id}.part').read_bytes()[0]==original[0]
            release.set()
            assert first+response.read()==original
            connection.close()
            until(lambda:job(reset_id)['state']=='complete')
            assert (root/f'data/cache/{reset_id}.bin').read_bytes()==native.data_for('generation')
            native.state['version']=1
            checks.append('resuming a replaced file waits for prior readers and never mixes file generations')

            held.clear(); release.clear()
            # This response holds its suffix open while deletion is requested.
            connection = http.client.HTTPConnection('127.0.0.1',proxy,timeout=10)
            connection.request('GET',path,headers={'Authorization':token,'Range':'bytes=0-1048575'})
            response=connection.getresponse()
            first=response.read(32)
            assert held.wait(2)
            api(f'/downloads/{ident}/delete', {})
            assert job(ident)['state']=='deleting' and (root/f'data/cache/{ident}.part').exists()
            policy=api('/automation')['policy'];policy['incognito']=True;api('/automation/policy',policy)
            policy['incognito']=False;api('/automation/policy',policy)
            release.set(); result=first+response.read(); connection.close()
            assert not any(item['download_id']==ident for item in api('/stats')['playback'])
            assert result==native.data_for('prefix')[:1048576]
            until(lambda:job(ident)['state']=='removed')
            assert not (root/f'data/cache/{ident}.part').exists()
            checks.append('deleting during playback waits for readers, then removes the local bytes without corrupting the response')

            policy=api('/automation')['policy']; policy['incognito']=True; api('/automation/policy',policy)
            assert api('/stats')['playback']==[]
            request(proxy,generic_path,{'Authorization':generic_token,'Range':'bytes=0-15'})
            assert api('/stats')['playback']==[]
            checks.append('incognito clears and suppresses per-media playback measurements')
        finally:
            release.set(); stop(); origin.shutdown(); origin.server_close(); log.close()
    for index, check in enumerate(checks,1): print(f'PASS {index}: {check}')

if __name__=='__main__': main()
