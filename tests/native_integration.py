#!/usr/bin/env python3
"""FN metadata-backed cache tests. Every network connection is loopback-only."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

from integration import DATA, port, request, until

TOKEN = 'native-test-session'
KEY = 'NDzZTVxnRKP8Z0jXg1VAMonaG8akvh'
SUFFIX = '16CCEB3D-AB42-077D-36A1-F355324E4237'
state = {'allowed': True, 'block': False, 'version': 1, 'ranges': [], 'interrupt': False}


def md5(value):
    return hashlib.md5(value.encode()).hexdigest()


def data_for(guid):
    data = DATA[:512 * 1024] if guid == 'small' else DATA
    return bytes([state['version']]) + data[1:]


class Origin(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def json(self, data, status=200):
        raw = json.dumps(data).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def signed(self, body=''):
        values = parse_qs(self.headers.get('Authx', ''))
        try:
            expected = md5('_'.join([KEY, self.path, values['nonce'][0], values['timestamp'][0], md5(body), SUFFIX]))
            return values['sign'][0] == expected
        except (KeyError, IndexError):
            return False

    def allowed(self):
        return state['allowed'] and self.headers.get('Authorization') in (TOKEN, 'another-session')

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get('Content-Length', 0))).decode()
        if not self.allowed() or not self.signed(raw):
            return self.json({'code': -1, 'data': None})
        if self.path != '/v/api/v1/stream':
            return self.json({'code': -2, 'data': None})
        body = json.loads(raw)
        guid = body['media_guid']
        self.json({'code': 0, 'data': {'file_stream': {
            'guid': guid, 'path': '/fixture/' + guid + '.mp4', 'size': len(data_for(guid)),
            'type': 1, 'can_play': 1, 'timestamp': 1000 + state['version'],
            'create_time': 1000, 'update_time': 2000 + state['version'], 'file_birth_time': 1000,
        }}})

    def do_HEAD(self):
        self.send_response(501)
        self.send_header('Content-Length', '0')
        self.end_headers()

    def do_GET(self):
        if not self.allowed():
            return self.json({'code': -1}, 401)
        if self.path == '/v/api/v1/user/info':
            if not self.signed():
                return self.json({'code': -1, 'data': None})
            return self.json({'code': 0, 'data': {'username': 'fixture-user'}})
        if not self.path.startswith('/v/api/v1/media/range/'):
            return self.json({'code': -2}, 404)
        span = self.headers.get('Range')
        state['ranges'].append(span)
        if state['block'] and span != 'bytes=0-0':
            return self.json({'code': -1}, 503)
        payload = data_for(self.path.rsplit('/', 1)[-1])
        if span:
            first, last = span.removeprefix('bytes=').split('-')
            start = int(first)
            end = min(int(last), len(payload) - 1) if last else len(payload) - 1
        else:
            start, end = 0, len(payload) - 1
        self.send_response(206 if span else 200)
        self.send_header('Content-Type', 'video/mp4')
        self.send_header('Content-Length', str(end - start + 1))
        if span:
            self.send_header('Content-Range', f'bytes {start}-{end}/{len(payload)}')
        self.end_headers()
        interrupt = state['interrupt'] and span == 'bytes=0-'
        if interrupt:
            state['interrupt'] = False
            end = min(end, start + 65535)
        try:
            self.wfile.write(payload[start:end + 1])
            self.wfile.flush()
            if interrupt:
                self.close_connection = True
                self.connection.shutdown(2)
        except (BrokenPipeError, ConnectionResetError):
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    origin = ThreadingHTTPServer(('127.0.0.1', 0), Origin)
    threading.Thread(target=origin.serve_forever, daemon=True).start()
    proxy, admin = port(), port()
    checks = []
    with tempfile.TemporaryDirectory(prefix='fn-native-test-') as temp:
        root = Path(temp)
        config = root / 'config.toml'
        config.write_text(f'upstream="http://127.0.0.1:{origin.server_port}"\nproxy_listen="127.0.0.1:{proxy}"\nadmin_listen="127.0.0.1:{admin}"\ndata_dir="data"\n')
        log = (root / 'service.log').open('ab')
        process = None

        def start():
            result = subprocess.Popen([str(binary), 'serve', '--config', str(config)], stdout=log, stderr=log)
            until(lambda: result.poll() is None and request(admin, '/health')[0] == 200)
            return result

        def stop():
            if process and process.poll() is None:
                process.send_signal(signal.SIGTERM) if os.name != 'nt' else process.kill()
                process.wait(timeout=8)

        def api(route, body=None):
            headers = {'Authorization': 'Bearer ' + (root / 'data/admin-token').read_text().strip(), 'Content-Type': 'application/json'}
            status, _, raw = request(admin, '/api' + route, headers, 'POST' if body is not None else 'GET', json.dumps(body) if body is not None else None)
            assert status == 200, (status, raw)
            return json.loads(raw)

        def observe(guid):
            endpoint = '/v/api/v1/media/range/' + guid
            assert request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=0-31'})[0] == 206
            return next(x['id'] for x in api('/observations') if x['path'] == endpoint)

        def job(ident):
            return next(x for x in api('/downloads') if x['id'] == ident)

        try:
            process = start()
            policy=api('/automation')['policy'];policy['retry_max_attempts']=0;api('/automation/policy',policy)
            ident = observe('sample')
            api('/downloads', {'observation_id': ident, 'paused': True})
            assert job(ident)['state'] == 'paused' and job(ident)['downloaded'] == 0
            cached = api(f'/downloads/{ident}/cache-samples', {})
            assert cached['downloaded_bytes'] <= 3 * 1024 ** 2 and len(cached['segments']) == 3
            assert job(ident)['state'] == 'paused' and job(ident)['downloaded'] == 0
            checks.append('paused source capture and bounded native sample caching without HEAD/ETag')

            state['block'] = True
            endpoint = '/v/api/v1/media/range/sample'
            payload = data_for('sample')
            for offset in [17, len(payload) // 2 + 7, len(payload) - 80]:
                state['ranges'].clear()
                status, headers, body = request(proxy, endpoint, {'Authorization': TOKEN, 'Range': f'bytes={offset}-{offset+31}'})
                assert status == 206 and body == payload[offset:offset+32]
                assert headers.get('x-fn-media-cache-kind') == 'segment'
                assert headers['content-range'] == f'bytes {offset}-{offset+31}/{len(payload)}'
                assert state['ranges'] == ['bytes=0-0'], state['ranges']
            status, headers, _ = request(proxy, endpoint, {'Authorization': TOKEN}, 'HEAD')
            assert status == 200 and int(headers['content-length']) == len(payload)
            assert request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=2097152-2097183'})[0] == 503
            checks.append('cached head/middle/tail reads work when all origin reads except one-byte authorization are blocked')
            status, headers, body = request(proxy, endpoint, {'Authorization': TOKEN, 'Play-Link': 'new-playback-session', 'Range': 'bytes=0-31'})
            assert status == 206 and headers.get('x-fn-media-cache') == 'HIT' and body == payload[:32]
            checks.append('new playback session token reuses the same authorized file cache')
            assert request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=0-31', 'X-Fn-Media-Cache-Only':'1'})[0] == 206
            state['ranges'].clear()
            assert request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=2097152-2097183', 'X-Fn-Media-Cache-Only':'1'})[0] == 504
            assert not state['ranges']
            checks.append('cache-only requests cannot fall back to a remote video body')

            state['allowed'] = False
            assert request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=0-31'})[0] == 401
            state['allowed'] = True
            state['version'] = 2
            assert request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=0-31'})[0] == 503
            assert request(proxy, endpoint, {'Authorization': 'another-session', 'Range': 'bytes=0-31'})[0] == 503
            checks.append('permission revocation, metadata version changes and other sessions cannot use stale segments')

            state['block'] = False
            api(f'/downloads/{ident}/cache-samples', {})
            stop()
            process = start()
            state['block'] = True
            assert request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=0-31'})[2] == data_for('sample')[:32]
            checks.append('sample cache and version metadata survive process restart')

            state['block'] = False
            small = observe('small')
            state['interrupt'] = True
            api('/downloads', {'observation_id': small})
            until(lambda: job(small)['state'] == 'failed')
            state['ranges'].clear()
            api(f'/downloads/{small}/resume', {})
            until(lambda: job(small)['state'] == 'complete')
            assert any(s and s.startswith('bytes=65536-') for s in state['ranges']), state['ranges']
            state['block'] = True
            status, headers, body = request(proxy, '/v/api/v1/media/range/small', {'Authorization': TOKEN})
            assert status == 200 and headers.get('x-fn-media-cache') == 'HIT' and body == data_for('small')
            checks.append('complete native file resumes by metadata version and plays without HEAD or HTTP validators')
        finally:
            stop()
            log.close()
            origin.shutdown()
    for i, check in enumerate(checks, 1):
        print(f'PASS {i}: {check}')


if __name__ == '__main__':
    main()
