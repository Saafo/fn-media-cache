#!/usr/bin/env python3
"""Multi-route download tests. All configured origins are local fixtures."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer

from integration import DATA, port, request, until
from native_integration import Origin, TOKEN

PAYLOAD = DATA[:2 * 1024 * 1024]


class RouteOrigin(Origin):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', 0))).decode()
        self.server.calls.append((self.command, self.path, None))
        if self.path == '/v/api/v1/play/record':
            self.server.progress.append(body)
            return self.json({'code': 0})
        if not self.allowed() or not self.signed(body):
            return self.json({'code': -1})
        guid = json.loads(body)['media_guid']
        self.json({'code': 0, 'data': {'file_stream': {
            'guid': guid, 'path': '/fixture/movie.mp4', 'size': len(PAYLOAD),
            'type': 1, 'can_play': 1, 'timestamp': self.server.version,
            'create_time': 1, 'update_time': self.server.version, 'file_birth_time': 1,
        }}})

    def allowed(self):
        return self.server.authorized and self.headers.get('Authorization') == TOKEN

    def do_GET(self):
        span = self.headers.get('Range')
        self.server.calls.append((self.command, self.path, span))
        if not self.allowed():
            return self.json({'code': -1}, 401)
        if self.path == '/v/api/v1/user/info':
            if not self.signed():
                return self.json({'code': -1})
            return self.json({'code': 0, 'data': {'username': 'route-fixture'}})
        if not self.path.startswith('/v/api/v1/media/range/'):
            return self.json({'code': -1}, 404)
        if self.server.down:
            return self.json({'code': -1}, 503)
        first, last = span.removeprefix('bytes=').split('-') if span else ('0', '')
        start, end = int(first), int(last) if last else len(PAYLOAD) - 1
        self.send_response(206 if span else 200)
        self.send_header('Content-Type', 'video/mp4')
        self.send_header('Content-Length', str(end - start + 1))
        if span:
            self.send_header('Content-Range', f'bytes {start}-{end}/{len(PAYLOAD)}')
        self.end_headers()
        payload = PAYLOAD if not self.server.corrupt else b'x' + PAYLOAD[1:]
        interrupt = self.server.interrupt and span == 'bytes=0-'
        if interrupt:
            self.server.interrupt = False
            end = 131071
        for offset in range(start, end + 1, 65536):
            time.sleep(self.server.delay)
            self.wfile.write(payload[offset:min(offset + 65536, end + 1)])
            self.wfile.flush()
        if interrupt:
            self.close_connection = True
            self.connection.shutdown(2)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    binary = parser.parse_args().binary.resolve()
    servers = []
    for delay in [0.025, 0.001, 0, 0]:
        server = ThreadingHTTPServer(('127.0.0.1', 0), RouteOrigin)
        server.delay, server.version, server.authorized = delay, 1000, True
        server.corrupt = server.interrupt = server.down = False
        server.calls, server.progress = [], []
        threading.Thread(target=server.serve_forever, daemon=True).start()
        servers.append(server)
    primary, fast, wrong, unauthorized = servers
    wrong.corrupt = True
    unauthorized.authorized = False
    origins = [f'http://127.0.0.1:{s.server_port}' for s in servers]
    checks = []
    proxy, admin = port(), port()
    with tempfile.TemporaryDirectory(prefix='fn-routes-test-') as temp:
        root = Path(temp)
        config = root / 'config.toml'
        config.write_text(f'upstream={json.dumps(origins[0])}\nupstream_alternates={json.dumps(origins[1:])}\nproxy_listen="127.0.0.1:{proxy}"\nadmin_listen="127.0.0.1:{admin}"\ndata_dir="data"\n')
        log = (root / 'service.log').open('wb')
        process = subprocess.Popen([str(binary), 'serve', '--config', str(config)], stdout=log, stderr=log)

        def api(path, body=None):
            headers = {'Authorization': 'Bearer ' + (root / 'data/admin-token').read_text().strip(), 'Content-Type': 'application/json'}
            status, _, data = request(admin, '/api' + path, headers, 'POST' if body is not None else 'GET', json.dumps(body) if body is not None else None)
            assert status == 200, (status, data)
            return json.loads(data)

        def routes_ready():
            result = api('/routes')
            return result if result['measured_at_ms'] and not result['measuring'] else None

        def task(ident):
            return next(x for x in api('/downloads') if x['id'] == ident)

        endpoint = '/v/api/v1/media/range/movie'
        try:
            until(lambda: process.poll() is None and request(admin, '/health')[0] == 200)
            assert request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=0-15'})[0] == 206
            until(routes_ready)
            report = routes_ready()
            assert report['selected'] == origins[1], report
            assert [r['status'] for r in report['results']] == ['ready', 'ready', 'mismatch', 'failed'], report
            assert all(r['sample_bytes'] <= 1048576 for r in report['results'])
            checks.append('first video automatically benchmarks routes; fastest valid route wins; mismatched content and denied session are excluded')

            ident = next(x['id'] for x in api('/observations') if x['path'] == endpoint)
            api('/downloads', {'observation_id': ident, 'paused': True})
            full_calls = [len(s.calls) for s in servers]
            samples = api(f'/downloads/{ident}/cache-samples', {})
            assert samples['download_origin'] == origins[1]
            assert samples['downloaded_bytes'] <= 3 * 1024 * 1024
            assert task(ident)['downloaded'] == 0
            assert len(primary.calls) == full_calls[0]
            status, headers, body = request(proxy, endpoint, {'Authorization': TOKEN, 'Range': 'bytes=0-31', 'X-Fn-Media-Cache-Only': '1'})
            assert status == 206 and headers['x-fn-media-cache'] == 'HIT' and body == PAYLOAD[:32]
            checks.append('sample download uses fastest route, shares the primary cache identity, and leaves the full task paused')

            fast.interrupt = True
            before_calls = [len(s.calls) for s in servers]
            api(f'/downloads/{ident}/resume', {})
            until(lambda: task(ident)['state'] in ('complete', 'failed'))
            assert task(ident)['state'] == 'complete', task(ident)
            assert (root / f'data/cache/{ident}.bin').read_bytes() == PAYLOAD
            assert any(call[2] == 'bytes=0-' for call in fast.calls[before_calls[1]:])
            assert any(call[2] == 'bytes=131072-' for call in primary.calls[before_calls[0]:])
            assert not any(call[2] and call[2].endswith('-') for call in wrong.calls)
            assert len(api('/downloads')) == 1
            checks.append('interrupted fastest route automatically resumes on the next verified route with identical complete bytes and no duplicate task')

            progress = '{"media_guid":"movie","ts":42,"duration":100}'
            assert request(proxy, '/v/api/v1/play/record', {'Authorization': TOKEN, 'Content-Type': 'application/json'}, 'POST', progress)[0] == 200
            assert primary.progress == [progress] and not any(s.progress for s in servers[1:])
            assert len(api('/progress-events')) == 1
            checks.append('playback progress is forwarded once to the primary, never replayed by measurement or failover')

            fast.version = 2000
            changed = api('/routes/probe', {'download_id': ident})
            assert changed['selected'] == origins[0] and changed['results'][1]['status'] == 'mismatch'
            checks.append('matching bytes with a different file version cannot be selected')

            fast.version = 1000
            primary.down = True
            available = api('/routes/probe', {'download_id': ident})
            assert available['selected'] == origins[1] and available['results'][0]['status'] == 'failed'
            checks.append('a previously validated alternate remains usable if the primary video route later fails')
        finally:
            process.send_signal(signal.SIGTERM) if os.name != 'nt' else process.kill()
            process.wait(timeout=8)
            log.close()
            for server in servers:
                server.shutdown()
    for i, check in enumerate(checks, 1):
        print(f'PASS {i}: {check}')


if __name__ == '__main__':
    main()
