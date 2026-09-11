#!/usr/bin/env python3
"""Console Go(`/zen/go/v1`)对 `x-opencode-session` 的真机 A/B。

用法:
    python3 go_endpoint_headers.py [供应商 id]     # 默认 opencodego

从 `$MIYU_HOME/config/config.jsonc`(默认 ~/.miyu)取该供应商的 key 与地址,
对同一条最短请求打四次,只改头:

    A 裸请求           —— 修复前 Miyu 的形态
    B 只有对的 UA      —— 隔离出「缺 session」这一项
    C UA + session     —— 最小可用集
    D 五头全带         —— 修复后 Miyu 的形态

期望:B 回 400 MissingSessionID,C/D 回 200。A 看运气(默认 UA 会先被 CF 1010 挡)。
"""

import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.request

PROVIDER = sys.argv[1] if len(sys.argv) > 1 else "opencodego"
UA = "opencode/1.18.29 ai-sdk/provider-utils/4.0.46 runtime/bun/1.4.0"
SESSION = "ses_f7f57a497ffeztcKAAwwE6ZtFU"
REQUEST = "msg_080a85b8d001GbUgeBrzPv21j7"


def load_provider():
    home = pathlib.Path(os.environ.get("MIYU_HOME", pathlib.Path.home() / ".miyu"))
    raw = (home / "config" / "config.jsonc").read_text(encoding="utf-8")
    config = json.loads(re.sub(r"^\s*//.*$", "", raw, flags=re.M))
    for provider in config.get("providers", []):
        if provider.get("id") == PROVIDER:
            return provider
    raise SystemExit(f"config 里没有供应商 {PROVIDER}")


def call(name, base, key, model, extra):
    headers = {"Content-Type": "application/json", "Authorization": f"Bearer {key}"}
    headers.update(extra)
    body = json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 8,
            "stream": False,
        }
    ).encode()
    request = urllib.request.Request(f"{base}/chat/completions", data=body, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            print(f"[{name}] HTTP {response.status}: {response.read()[:160].decode('utf-8', 'replace')}")
    except urllib.error.HTTPError as error:
        print(f"[{name}] HTTP {error.code}: {error.read()[:260].decode('utf-8', 'replace')}")
    except Exception as error:  # noqa: BLE001 - 探针,出什么都要看见
        print(f"[{name}] ERR {error!r}")


def main():
    provider = load_provider()
    base = provider["base_url"].rstrip("/")
    key = provider["api_key"]
    model = provider.get("default_model") or provider["models"][0]
    print(f"provider={PROVIDER} base={base} model={model}")
    call("A 裸请求", base, key, model, {})
    call("B UA 对但无 session", base, key, model, {"User-Agent": UA})
    call("C UA + session", base, key, model, {"User-Agent": UA, "x-opencode-session": SESSION})
    call(
        "D 五头全带",
        base,
        key,
        model,
        {
            "User-Agent": UA,
            "x-opencode-client": "cli",
            "x-opencode-project": "global",
            "x-opencode-session": SESSION,
            "x-opencode-request": REQUEST,
        },
    )


if __name__ == "__main__":
    main()
