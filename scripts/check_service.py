#!/usr/bin/env python3
"""Check local service readiness without loading or printing credentials."""
import json
import sys
import urllib.error
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
checks = [
    ("management", "http://127.0.0.1:28006/health", "GET"),
    ("upstream_via_proxy", "http://127.0.0.1:28005/", "HEAD"),
]
passed = True
for name, url, method in checks:
    try:
        request = urllib.request.Request(url, method=method)
        with opener.open(request, timeout=5) as response:
            ok = response.status == 200
            if name == "management":
                ok = ok and json.load(response).get("ok") is True
            print(f"{'PASS' if ok else 'FAIL'} {name}: HTTP {response.status}")
            passed = passed and ok
    except urllib.error.HTTPError as error:
        ok = name == "upstream_via_proxy" and 200 <= error.code < 500
        print(f"{'PASS' if ok else 'FAIL'} {name}: HTTP {error.code}")
        passed = passed and ok
    except (OSError, ValueError) as error:
        print(f"FAIL {name}: {type(error).__name__}")
        passed = False
sys.exit(0 if passed else 1)
