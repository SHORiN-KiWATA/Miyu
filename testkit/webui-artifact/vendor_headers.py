#!/usr/bin/env python3
"""验 /vendor/ 的响应头:gzip 直发、不收压缩时现解、PNA 预检。

    BIN=<miyu 二进制> python3 testkit/webui-artifact/vendor_headers.py

「存 gzip、原样发出去让浏览器自己解」是这次唯一的新花样,单独立一道量尺:
一旦哪天有人给 vendor 加了鉴权、或者压缩存法被改回明文,这里会直接报红。
"""
import http.client
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

BIN = Path(os.environ["BIN"]).expanduser()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-vendor-headers")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18488"))
PATH = "/vendor/echarts/echarts.min.js"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))


def wait_http(url, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except Exception:
            time.sleep(0.3)
    return False


def request(method, headers):
    connection = http.client.HTTPConnection("127.0.0.1", PORT, timeout=20)
    connection.request(method, PATH, headers=headers)
    response = connection.getresponse()
    body = response.read()
    got = {key.lower(): value for key, value in response.getheaders()}
    connection.close()
    return response.status, got, body


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    (HOME / "config").mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    report = {}
    failures = []
    try:
        assert wait_http(f"http://127.0.0.1:{PORT}/api/config"), "daemon 没起来"

        status, headers, body = request("GET", {"Accept-Encoding": "gzip"})
        report["gzip"] = {"status": status, "bytes": len(body),
                          "content_encoding": headers.get("content-encoding"),
                          "vary": headers.get("vary"),
                          "acao": headers.get("access-control-allow-origin"),
                          "pna": headers.get("access-control-allow-private-network")}
        if status != 200:
            failures.append(f"收 gzip 的请求返回 {status}")
        if headers.get("content-encoding") != "gzip":
            failures.append("收 gzip 时没带 Content-Encoding: gzip")
        if (headers.get("vary") or "").lower() != "accept-encoding":
            failures.append("缺 Vary: accept-encoding")
        if headers.get("access-control-allow-origin") != "*":
            failures.append("缺 Access-Control-Allow-Origin(不透明源的 iframe 会取不到)")
        if headers.get("access-control-allow-private-network") != "true":
            failures.append("缺 Access-Control-Allow-Private-Network(会被 PNA 拦)")
        # 存进二进制的就该是压缩后的字节,发出去不多不少。
        if not 300_000 < len(body) < 500_000:
            failures.append(f"gzip 体积不对:{len(body)}")

        status, headers, plain = request("GET", {"Accept-Encoding": "identity"})
        report["plain"] = {"status": status, "bytes": len(plain),
                           "content_encoding": headers.get("content-encoding")}
        if headers.get("content-encoding"):
            failures.append("不收压缩时仍然带了 Content-Encoding")
        if len(plain) < 1_000_000:
            failures.append(f"不收压缩时没有现解压:{len(plain)} 字节")
        if not plain.lstrip().startswith(b"/*"):
            failures.append("现解出来的不是 JS 正文")

        status, headers, _ = request("OPTIONS", {"Origin": "null",
                                                 "Access-Control-Request-Method": "GET"})
        report["preflight"] = {"status": status,
                               "acao": headers.get("access-control-allow-origin"),
                               "pna": headers.get("access-control-allow-private-network")}
        if status not in (200, 204):
            failures.append(f"OPTIONS 预检返回 {status}")
        if headers.get("access-control-allow-private-network") != "true":
            failures.append("预检没带 PNA 放行头")
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except Exception:
            daemon.kill()
    report["failures"] = failures
    print(json.dumps(report, ensure_ascii=False, indent=2))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
