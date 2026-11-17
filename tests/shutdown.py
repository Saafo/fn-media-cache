#!/usr/bin/env python3
"""launchd sends SIGTERM: the service must exit cleanly rather than be killed."""
import argparse
import json
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="fn-media-cache-signal-") as directory:
        root = Path(directory)
        proxy, admin = port(), port()
        config = root / "config.toml"
        config.write_text(f'upstream="http://127.0.0.1:1"\nproxy_listen="127.0.0.1:{proxy}"\nadmin_listen="127.0.0.1:{admin}"\ndata_dir="data"\n')
        process = subprocess.Popen([str(args.binary.resolve()), "serve", "--config", str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            for _ in range(100):
                try:
                    with opener.open(f"http://127.0.0.1:{admin}/health", timeout=0.2) as response:
                        if json.load(response)["ok"]:
                            break
                except OSError:
                    time.sleep(0.05)
            else:
                raise AssertionError("service did not become ready")
            process.send_signal(signal.SIGTERM)
            result = process.wait(timeout=8)
            assert result == 0, f"SIGTERM did not shut down cleanly: exit={result}"
            print("PASS: SIGTERM produces a clean exit")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
