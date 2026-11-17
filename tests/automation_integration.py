#!/usr/bin/env python3
"""Catalog, scheduling, pacing and retention through the public local interfaces."""
import argparse
import hashlib
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
from urllib.parse import parse_qs, urlsplit

from integration import DATA, port, request, until
from native_integration import Origin, TOKEN, KEY, SUFFIX, md5

ITEMS = {
    'library': ('MediaDB', '', 0, 0), 'show': ('TV', 'library', 0, 0),
    'season-one': ('Season', 'show', 1, 0), 'season-two': ('Season', 'show', 2, 0),
    'specials': ('Season', 'show', 0, 0),
    'one': ('Episode', 'season-one', 1, 1), 'two': ('Episode', 'season-one', 1, 2),
    'three': ('Episode', 'season-one', 1, 3), 'four': ('Episode', 'season-two', 2, 1),
    'special': ('Episode', 'specials', 0, 1),
    'alias-one': ('Movie', 'library', 0, 0), 'alias-two': ('Movie', 'library', 0, 0),
    'space': ('Movie', 'library', 0, 0),
}
MEDIA = {name: ('shared' if name.startswith('alias-') else name) for name in ITEMS}
state = {'media_requests': [], 'writes': [], 'hold': threading.Event(), 'watched': set()}


def item(guid):
    kind, parent, season, episode = ITEMS[guid]
    return {'guid': guid, 'type': kind, 'parent_guid': parent, 'title': '剧集 ' + guid,
            'season_number': season, 'episode_number': episode, 'watched': int(guid in state['watched']),
            'is_watched': int(guid in state['watched']), 'watched_ts': 999999}


def payload(guid):
    return DATA[:2*1024*1024] if guid in ('two', 'space') else DATA[:512*1024]


class CatalogOrigin(Origin):
    def allowed(self):
        return self.headers.get('Authorization') == TOKEN

    def signed(self, body=''):
        url = urlsplit(self.path)
        query = parse_qs(url.query)
        signed_body = '&'.join(f'{key}={query[key][-1]}' for key in sorted(query)) if self.command == 'GET' else body
        values = parse_qs(self.headers.get('Authx', ''))
        try:
            return values['sign'][0] == md5('_'.join([KEY, url.path, values['nonce'][0], values['timestamp'][0], md5(signed_body), SUFFIX]))
        except KeyError:
            return False

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get('Content-Length', 0))).decode()
        body = json.loads(raw)
        if self.path == '/v/api/v1/play/record':
            state['writes'].append((self.path, raw))
            return self.json({'code': 0, 'data': True})
        if not self.allowed() or not self.signed(raw):
            return self.json({'code': -1}, 401)
        if self.path == '/v/api/v1/play/info':
            guid = body['item_guid']
            return self.json({'code': 0, 'data': {'guid': guid, 'media_guid': MEDIA[guid], 'item': item(guid)}})
        if self.path == '/v/api/v1/stream':
            guid = body['media_guid']
            return self.json({'code': 0, 'data': {'file_stream': {
                'guid': guid, 'path': '/fixture/' + guid, 'type': 1, 'can_play': 1,
                'size': len(payload(guid)), 'timestamp': 1000, 'update_time': 1000,
                'create_time': 1000, 'file_birth_time': 1000,
            }}})
        if self.path == '/v/api/v1/item/list':
            parent = body.get('parent_guid') or body.get('ancestor_guid')
            rows = [item(guid) for guid in ITEMS if ITEMS[guid][1] == parent]
            return self.json({'code': 0, 'data': {'list': rows, 'total': len(rows)}})
        self.json({'code': -1}, 404)

    def do_GET(self):
        url = urlsplit(self.path)
        path = url.path
        if path == '/v/api/v1/session-context':
            return self.json({'code': 0})
        if path.startswith('/v/api/v1/media/range/'):
            guid = path.rsplit('/', 1)[-1]
            span = self.headers.get('Range')
            state['media_requests'].append((guid, span))
            first, last = span.removeprefix('bytes=').split('-') if span else ('0', '')
            data = payload(guid)
            start, end = int(first), int(last) if last else len(data)-1
            self.send_response(206 if span else 200)
            self.send_header('Content-Length', str(end-start+1))
            self.send_header('Content-Type', 'video/mp4')
            if span: self.send_header('Content-Range', f'bytes {start}-{end}/{len(data)}')
            self.end_headers()
            if url.query: state['hold'].wait(timeout=12)
            self.wfile.write(data[start:end+1])
            return
        if not self.allowed() or not self.signed():
            return self.json({'code': -1}, 401)
        if path == '/v/api/v1/user/info':
            data = {'username': 'fixture'}
        elif path == '/v/api/v1/mediadb/list':
            data = [item('library')]
        elif path == '/v/api/v1/search/list':
            assert parse_qs(url.query)['q'][0] == '剧集'
            data = [item('show')]
        elif path.startswith('/v/api/v1/item/'):
            data = item(path.rsplit('/', 1)[-1])
        elif path.startswith('/v/api/v1/season/list/'):
            data = [item('specials'), item('season-two'), item('season-one')]
        elif path.startswith('/v/api/v1/episode/list/'):
            parent = path.rsplit('/', 1)[-1]
            data = [item(guid) for guid in reversed(ITEMS) if ITEMS[guid][0] == 'Episode' and ITEMS[guid][1] == parent]
        else:
            return self.json({'code': -1}, 404)
        self.json({'code': 0, 'data': data})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    binary = parser.parse_args().binary.resolve()
    server = ThreadingHTTPServer(('127.0.0.1', 0), CatalogOrigin)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    proxy, admin = port(), port()
    checks = []
    with tempfile.TemporaryDirectory(prefix='fn-automation-test-') as directory:
        root = Path(directory)
        config = root/'config.toml'
        config.write_text(f'upstream="http://127.0.0.1:{server.server_port}"\nproxy_listen="127.0.0.1:{proxy}"\nadmin_listen="127.0.0.1:{admin}"\ndata_dir="data"\n')
        log = (root/'service.log').open('ab')
        process = None

        def start():
            child = subprocess.Popen([str(binary), 'serve', '--config', str(config)], stdout=log, stderr=log)
            until(lambda: child.poll() is None and request(admin, '/health')[0] == 200)
            return child

        def api(path, body=None):
            headers = {'Authorization': 'Bearer '+(root/'data/admin-token').read_text().strip(), 'Content-Type': 'application/json'}
            status, _, data = request(admin, '/api'+path, headers, 'POST' if body is not None else 'GET', json.dumps(body) if body is not None else None)
            assert status == 200, (path, status, data)
            return json.loads(data)

        def update(**changes):
            policy = api('/automation')['policy']
            policy.update(changes)
            api('/automation/policy', policy)

        def queue(*guids):
            result = api('/library/download', {'item_guids': list(guids)})['results']
            assert all(row['ok'] for row in result), result
            return [row['id'] for row in result]

        def task(ident):
            return next(row for row in api('/downloads') if row['id'] == ident)

        def managed(guid):
            return next(row for row in api('/automation')['managed'] if row['item']['guid'] == guid)

        def retain(guid, **change):
            row = managed(guid)
            api('/managed/retention', {'session_id': row['session_id'], 'item_guid': guid, **change})

        def progress(guid):
            raw = json.dumps({'item_guid': guid, 'media_guid': MEDIA[guid], 'ts': 999999, 'duration': 1000000})
            assert request(proxy, '/v/api/v1/play/record', {'Authorization': TOKEN, 'Content-Type': 'application/json'}, 'POST', raw)[0] == 200

        def backdate(guid, seconds):
            with sqlite3.connect(root/'data/state.sqlite3') as db:
                db.execute('UPDATE managed_media SET completed_at=? WHERE item_guid=?', (int(time.time())-seconds, guid))
            update()

        try:
            process = start()
            assert request(proxy, '/v/api/v1/session-context', {'Authorization': TOKEN})[0] == 200
            update(bytes_per_second=0, scheduler_interval_seconds=1, playback_cooldown_seconds=0)
            assert api('/library/search', {'query': '剧集'})['items'][0]['guid'] == 'show'
            assert api('/library/browse', {'item_guid':'season-one'})['items'][0]['guid'] == 'one'
            first = queue('one')[0]
            until(lambda: task(first)['state'] == 'complete')
            assert (root/f'data/cache/{first}.bin').read_bytes() == payload('one')
            assert TOKEN not in json.dumps(api('/sessions'))
            checks.append('search with signed Unicode query and download an episode without any prior playback request')

            state['hold'].clear()
            holder = threading.Thread(target=lambda: request(proxy, '/v/api/v1/media/range/hold?hold=1', {'Authorization':TOKEN,'Range':'bytes=0-31'}), daemon=True)
            holder.start()
            until(lambda: api('/automation')['playing'])
            update(bytes_per_second=1024*1024)
            second = queue('two')[0]
            time.sleep(0.4)
            assert task(second)['downloaded'] == 0
            started = time.monotonic()
            state['hold'].set(); holder.join(timeout=3)
            until(lambda: task(second)['state'] == 'complete')
            assert time.monotonic()-started >= 1.8
            checks.append('an active player stops background transfer; release resumes it with the configured byte rate')

            update(auto_prefetch=True, bytes_per_second=0)
            progress('one')
            until(lambda: any(row['item']['guid']=='three' for row in api('/automation')['managed']))
            assert managed('one')['completed_at'] is None
            progress('three')
            until(lambda: any(row['item']['guid']=='four' for row in api('/automation')['managed']))
            assert not any(row['item']['guid']=='special' for row in api('/automation')['managed'])
            checks.append('prefetch follows explicit episode and season ordering, skips specials, and near-end progress never marks completion')

            update(retention_seconds=3600)
            retain('two', completed=True, keep=True)
            original = managed('two')['completed_at']
            progress('one'); time.sleep(0.1)
            assert managed('two')['completed_at'] == original
            backdate('two', 4000)
            time.sleep(1.2)
            assert task(second)['state'] == 'complete'
            state['hold'].clear()
            holder = threading.Thread(target=lambda: request(proxy, '/v/api/v1/media/range/two?hold=1', {'Authorization':TOKEN,'Range':'bytes=0-31'}), daemon=True)
            holder.start(); until(lambda: api('/automation')['playing'])
            retain('two', keep=False)
            time.sleep(1.2)
            assert task(second)['state'] == 'complete'
            state['hold'].set(); holder.join(timeout=3)
            until(lambda: task(second)['state'] == 'removed')
            assert not (root/f'data/cache/{second}.bin').exists() and managed('two')['suppressed']
            progress('one'); time.sleep(1.2)
            assert task(second)['state'] == 'removed'
            checks.append('configurable retention honors keep and active readers, preserves first completion time, and suppresses automatic redownload')

            shared = queue('alias-one', 'alias-two')
            assert shared[0] == shared[1]
            until(lambda: task(shared[0])['state'] == 'complete')
            retain('alias-one', completed=True); backdate('alias-one', 4000)
            time.sleep(1.2)
            assert task(shared[0])['state'] == 'complete'
            retain('alias-two', completed=True); backdate('alias-two', 4000)
            until(lambda: task(shared[0])['state'] == 'removed')
            checks.append('a shared media file is retained until every managed item referencing it is eligible')

            update(cache_limit_bytes=1024)
            space = queue('space')[0]
            until(lambda: task(space)['state'] == 'waiting_space')
            update(cache_limit_bytes=100*1024*1024)
            until(lambda: task(space)['state'] == 'complete')
            checks.append('insufficient cache space waits and resumes automatically after raising the configured limit')

            retained_policy = api('/automation')['policy']
            process.send_signal(signal.SIGTERM) if os.name != 'nt' else process.kill()
            process.wait(timeout=8)
            process = start()
            assert api('/automation')['policy'] == retained_policy
            assert managed('two')['suppressed']
            retain('two', completed=False)
            assert not managed('two')['suppressed'] and managed('two')['completed_at'] is None
            assert queue('two')[0] == second
            until(lambda: task(second)['state'] == 'complete')
            assert all(path == '/v/api/v1/play/record' for path, _ in state['writes'])
            checks.append('policy and tombstones survive restart; explicit re-download works; local completion and cleanup never write upstream state')

            update(cleanup_enabled=False)
            process.send_signal(signal.SIGTERM) if os.name != 'nt' else process.kill()
            process.wait(timeout=8)
            with sqlite3.connect(root/'data/state.sqlite3') as db:
                db.execute("UPDATE downloads SET state='deleting' WHERE id=?", (first,))
                db.execute('UPDATE managed_media SET suppressed=1 WHERE download_id=?', (first,))
            process = start()
            until(lambda: task(first)['state'] == 'removed')
            assert not (root/f'data/cache/{first}.bin').exists()
            checks.append('restart finishes an already committed local deletion even after cleanup policy changes')
        finally:
            state['hold'].set()
            if process and process.poll() is None:
                process.send_signal(signal.SIGTERM) if os.name != 'nt' else process.kill()
                process.wait(timeout=8)
            log.close()
            server.shutdown()
    for number, check in enumerate(checks, 1):
        print(f'PASS {number}: {check}')


if __name__ == '__main__':
    main()
