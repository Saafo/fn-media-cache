#!/usr/bin/env python3
"""Black-box tests against loopback-only fixtures; no real upstream is contacted."""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

DATA = bytes(range(256)) * 32768
AUTH = "Bearer local-fixture-secret"
TARGET = "/movie.mp4?signature=fixture-secret-query"
state = {"block": False, "revoke": False, "version": '"v1"', "interrupt": None, "slow": None, "ignore_range": None, "calls": []}
guard = threading.Lock()


class Fixture(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            # Cancelling downloads deliberately closes persistent fixture connections.
            pass

    def do_HEAD(self):
        self.respond(True)

    def do_GET(self):
        self.respond(False)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        if self.path == '/v/api/v1/play/record':
            state['last_progress_body'] = body
            self.reply(200, b'{"code":0,"data":true}', {'Content-Type':'application/json'})
        else:
            self.reply(404)

    def reply(self, status, body=b"", headers=None, head=False):
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def respond(self, head):
        path = urlsplit(self.path).path
        authorization = self.headers.get("Authorization")
        with guard:
            state["calls"].append((self.command, path, self.headers.get("Range"), self.headers.get("If-Range")))
        if authorization not in (AUTH, "Bearer another-account") or (state["revoke"] and authorization == AUTH):
            return self.reply(401, b"unauthorized", head=head)
        if head and path == "/nohead.mp4":
            return self.reply(501, head=True)
        if path == "/library":
            return self.reply(200, b'{"title":"Fixture library"}', {"Content-Type": "application/json"}, head)
        if path == "/redirect":
            return self.reply(302, headers={"Location": f"http://127.0.0.1:{self.server.server_port}{TARGET}"}, head=head)
        if path == "/external":
            return self.reply(302, headers={"Location": "https://example.invalid/movie.mp4"}, head=head)
        if not path.endswith(".mp4"):
            return self.reply(404, head=head)
        if state["block"] and not head:
            return self.reply(503, b"video reads blocked by test", head=head)
        size = len(DATA) if path != "/oversize.mp4" else 1024 ** 3
        start, end = 0, size - 1
        range_header = self.headers.get("Range")
        if range_header and state["ignore_range"] != path:
            if "," in range_header:
                return self.reply(400, b"fixture only handles a single range", head=head)
            first, last = range_header.removeprefix("bytes=").split("-")
            start = int(first or 0)
            end = min(int(last), size - 1) if last else size - 1
        partial = bool(range_header and state["ignore_range"] != path)
        self.send_response(206 if partial else 200)
        self.send_header("Content-Type", "video/mp4")
        self.send_header("Content-Length", str(end - start + 1))
        self.send_header("Accept-Ranges", "bytes")
        if path != "/novalidator.mp4":
            self.send_header("ETag", state["version"])
        if partial:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if head:
            return
        interrupt = state["interrupt"] == path and not range_header
        if interrupt:
            state["interrupt"] = None
            end = min(end, start + 256 * 1024 - 1)
        try:
            for offset in range(start, min(end + 1, len(DATA)), 16 * 1024):
                self.wfile.write(DATA[offset:min(offset + 16 * 1024, end + 1)])
                self.wfile.flush()
                if state["slow"] == path:
                    time.sleep(0.01)
            if interrupt:
                self.close_connection = True
                self.connection.shutdown(socket.SHUT_RDWR)
                self.connection.close()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(target_port, path, headers=None, method="GET", payload=None):
    connection = http.client.HTTPConnection("127.0.0.1", target_port, timeout=15)
    connection.request(method, path, body=payload, headers=headers or {})
    response = connection.getresponse()
    body = response.read()
    result = response.status, dict((k.lower(), v) for k, v in response.getheaders()), body
    connection.close()
    return result


def until(predicate, timeout=20):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return last
        except (ConnectionError, OSError, http.client.HTTPException, FileNotFoundError):
            pass
        time.sleep(0.05)
    raise AssertionError(f"condition timed out; last result: {last!r}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    fixture = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    checks = []
    with tempfile.TemporaryDirectory(prefix="fn-media-cache-test-") as temp:
        root = Path(temp)
        proxy_port, admin_port = port(), port()
        config = root / "config.toml"
        config.write_text(f'upstream = "http://127.0.0.1:{fixture.server_port}"\nproxy_listen = "127.0.0.1:{proxy_port}"\nadmin_listen = "127.0.0.1:{admin_port}"\ndata_dir = "data"\nmax_cache_bytes = {64 * 1024 * 1024}\nread_timeout_seconds = 5\n')
        log_path = root / "server.log"
        log = log_path.open("ab")
        process = None

        def start():
            child = subprocess.Popen([str(binary), "serve", "--config", str(config)], stdout=log, stderr=log)
            until(lambda: child.poll() is None and request(admin_port, "/health")[0] == 200)
            return child

        def stop(abrupt=False):
            if process is not None and process.poll() is None:
                if abrupt or os.name == "nt":
                    process.kill()
                else:
                    process.send_signal(signal.SIGINT)
                process.wait(timeout=10)

        def api(path, payload=None):
            token = (root / "data/admin-token").read_text().strip()
            headers = {"Authorization": f"Bearer {token}"}
            if payload is not None:
                headers["Content-Type"] = "application/json"
            status, _, body = request(admin_port, "/api" + path, headers, "POST" if payload is not None else "GET", json.dumps(payload) if payload is not None else None)
            assert status == 200, (status, body)
            return json.loads(body)

        def observe(path):
            assert request(proxy_port, path, {"Authorization": AUTH, "Range": "bytes=0-31"})[0] in (200, 206)
            return next(item for item in api("/observations") if item["path"] == urlsplit(path).path)

        def enqueue(path):
            observation = observe(path)
            return api("/downloads", {"observation_id": observation["id"]})["id"]

        def job(ident):
            return next(item for item in api("/downloads") if item["id"] == ident)

        try:
            process = start()
            policy=api('/automation')['policy'];policy['retry_max_attempts']=0;api('/automation/policy',policy)
            assert request(admin_port, "/api/stats")[0] == 401
            assert request(admin_port, "/")[0] == 200
            status, _, content = request(proxy_port, "/library", {"Authorization": AUTH})
            assert status == 200 and json.loads(content)["title"] == "Fixture library"
            status, headers, _ = request(proxy_port, "/redirect", {"Authorization": AUTH})
            assert status == 302 and headers["location"] == TARGET
            assert request(proxy_port, "/external", {"Authorization": AUTH})[0] == 302
            assert api("/stats")["external_redirects"] == 1
            checks.append("transparent control requests, redirect visibility, management authentication")

            progress_body = json.dumps({'media_guid':'fixture-media','ts':123,'duration':456,'private_field':'must-not-be-recorded'})
            status, _, _ = request(proxy_port, '/v/api/v1/play/record', {'Authorization':AUTH,'Content-Type':'application/json'}, 'POST', progress_body)
            assert status == 200 and state['last_progress_body'] == progress_body.encode()
            event = api('/progress-events')[0]
            assert event['origin'] == 'client' and event['position'] == 123 and event['duration'] == 456
            assert 'must-not-be-recorded' not in json.dumps(event) and 'source' not in event
            checks.append('client progress requests are forwarded unchanged and observed without private fields')

            probe_observation = observe("/nohead.mp4")
            probe = api(f"/observations/{probe_observation['id']}/probe", {})
            assert probe["total_bytes"] == len(DATA)
            assert probe["head_status"] == 501
            assert probe["upstream_sample_bytes"] <= 4 * 1024 * 1024
            assert probe["repeat_matches"] and all(s["local_read_matches"] for s in probe["samples"])
            assert probe["samples"][0]["sha256"] == hashlib.sha256(DATA[:1024 * 1024]).hexdigest()
            assert api("/downloads") == []
            checks.append("bounded probe reads at most 4 MiB and leaves the full-download queue empty")

            state["interrupt"] = "/movie.mp4"
            ident = enqueue(TARGET)
            until(lambda: job(ident)["state"] == "failed")
            assert (root / f"data/cache/{ident}.part").stat().st_size > 0
            api(f"/downloads/{ident}/resume", {})
            until(lambda: job(ident)["state"] == "complete")
            cached_file = root / f"data/cache/{ident}.bin"
            assert hashlib.sha256(cached_file.read_bytes()).digest() == hashlib.sha256(DATA).digest()
            assert any(path == "/movie.mp4" and span and span != "bytes=0-31" and condition == '"v1"' for _, path, span, condition in state["calls"])
            checks.append("interrupted download resumes with Range/If-Range and matching SHA-256")

            state["block"] = True
            for span, expected, code in [("bytes=512-1023", DATA[512:1024], 206), ("bytes=-37", DATA[-37:], 206), (f"bytes={len(DATA)-11}-", DATA[-11:], 206), (f"bytes={len(DATA)}-", b"", 416), (None, DATA, 200)]:
                headers = {"Authorization": AUTH}
                if span:
                    headers["Range"] = span
                before = len([c for c in state["calls"] if c[0] == "GET"])
                status, response_headers, body = request(proxy_port, TARGET, headers)
                assert status == code and body == expected and response_headers["x-fn-media-cache"] == "HIT"
                assert len([c for c in state["calls"] if c[0] == "GET"]) == before
            status, headers, body = request(proxy_port, TARGET, {"Authorization": AUTH}, "HEAD")
            assert status == 200 and int(headers["content-length"]) == len(DATA) and not body
            status, _, body = request(proxy_port, TARGET, {"Authorization": AUTH, "Range": "bytes=1-5", "If-Range": '"old"'})
            assert status == 200 and body == DATA
            checks.append("cached full/HEAD/suffix/open/unsatisfiable ranges work with upstream video reads blocked")

            outcomes = []
            threads = [threading.Thread(target=lambda: outcomes.append(request(proxy_port, TARGET, {"Authorization": AUTH, "Range": "bytes=0-65535"}))) for _ in range(6)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            assert len(outcomes) == 6 and all(code == 206 and body == DATA[:65536] for code, _, body in outcomes)
            checks.append("six concurrent local cache readers")

            assert request(proxy_port, TARGET, {"Authorization": "Bearer another-account"})[0] == 503
            assert request(proxy_port, TARGET.replace("fixture-secret-query", "different-token"), {"Authorization": AUTH})[0] == 503
            state["revoke"] = True
            assert request(proxy_port, TARGET, {"Authorization": AUTH})[0] == 401
            state["revoke"] = False
            state["version"] = '"v2"'
            assert request(proxy_port, TARGET, {"Authorization": AUTH})[0] == 503
            state["version"] = '"v1"'
            checks.append("account/query isolation, revoked access and changed ETag never serve stale cache")

            state["block"] = False
            state["interrupt"] = "/ignore.mp4"
            ignore = enqueue("/ignore.mp4")
            until(lambda: job(ignore)["state"] == "failed")
            state["ignore_range"] = "/ignore.mp4"
            api(f"/downloads/{ignore}/resume", {})
            until(lambda: job(ignore)["state"] == "complete")
            assert (root / f"data/cache/{ignore}.bin").read_bytes() == DATA
            state["ignore_range"] = None
            checks.append("server ignoring resume range safely restarts the partial file")

            state["slow"] = "/restart.mp4"
            restart = enqueue("/restart.mp4")
            until(lambda: job(restart)["downloaded"] >= 1024 * 1024)
            api(f"/downloads/{restart}/pause", {})
            until(lambda: job(restart)["state"] == "paused")
            api(f"/downloads/{restart}/resume", {})
            until(lambda: job(restart)["state"] == "downloading")
            stop(abrupt=True)
            process = start()
            until(lambda: job(restart)["state"] == "complete")
            assert (root / f"data/cache/{restart}.bin").read_bytes() == DATA
            state["slow"] = None
            checks.append("pause/resume and abrupt process restart preserve partial download state")

            no_validator = enqueue("/novalidator.mp4")
            until(lambda: job(no_validator)["state"] == "failed")
            assert "validator" in job(no_validator)["error"]
            oversize = enqueue("/oversize.mp4")
            until(lambda: job(oversize)["state"] == "waiting_space")
            assert "quota" in job(oversize)["error"]
            checks.append("unverifiable media fails and quota overflow waits for space without publishing a cache file")

            stop()
            cached_file.rename(cached_file.with_suffix(".part"))
            with sqlite3.connect(root / "data/state.sqlite3") as db:
                db.execute("UPDATE downloads SET state='downloading' WHERE id=?", (ident,))
            state["block"] = True
            process = start()
            until(lambda: job(ident)["state"] == "complete")
            assert cached_file.read_bytes() == DATA
            checks.append("crash after final partial-file write recovers without an invalid end-of-file Range")

            second = subprocess.run([str(binary), "serve", "--config", str(config)], capture_output=True, timeout=5)
            assert second.returncode != 0 and b"another instance" in second.stderr
            raw = json.dumps(api("/downloads")) + json.dumps(api("/observations"))
            assert "fixture-secret-query" not in raw and AUTH not in raw and '"source"' not in raw
            if os.name != "nt":
                assert (root / "data").stat().st_mode & 0o077 == 0
                assert (root / "data/state.sqlite3").stat().st_mode & 0o077 == 0
                assert (root / "data/admin-token").stat().st_mode & 0o077 == 0
            checks.append("single-instance lock, private local files, redacted management responses")

            stop()
            log.flush()
            assert AUTH not in log_path.read_text() and "fixture-secret-query" not in log_path.read_text()
            checks.append("request logs contain neither credentials nor signed query strings")
        finally:
            stop(abrupt=True)
            log.close()
            fixture.shutdown()
    for index, check in enumerate(checks, 1):
        print(f"PASS {index}: {check}")
    print(f"PASS: {len(checks)} integration scenarios; fixture payload {len(DATA)} bytes")


if __name__ == "__main__":
    main()
