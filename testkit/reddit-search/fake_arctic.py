#!/usr/bin/env python3
"""假 Arctic Shift，用来复现真服务上没法按需触发的分支。

真服务器不会听你的：限流、查询超时、字段缺失、结构畸形这些恰恰是最需要
测的路径。这里起一个本地 HTTP 服务顶替 ARCTIC_BASE，行为由 MODE 控制，
HITS 记录每个端点被打了几次（用来验证「超时重试正好一次」这类看不见的行为）。
"""

import json
import threading
import time
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

MODE = {
    "posts": "ok",       # ok | timeout_then_ok | always_timeout | error | 429 | 500 | empty | garbage
    "tree": "ok",
    "ids": "ok",
    "subs": "prefix_ok",  # prefix_ok | prefix_empty | broken
}
HITS = Counter()
LAST_QUERY = {}
_timeout_used = {"value": False}


def reset(**overrides):
    MODE.update({"posts": "ok", "tree": "ok", "ids": "ok", "subs": "prefix_ok"})
    MODE.update(overrides)
    HITS.clear()
    LAST_QUERY.clear()
    _timeout_used["value"] = False


# ---------------------------------------------------------------- 假数据

def post(pid, title, score, comments, age_seconds, sub="LocalLLaMA", **extra):
    row = {
        "id": pid, "title": title, "author": "tester", "subreddit": sub,
        "score": score, "num_comments": comments,
        "created_utc": time.time() - age_seconds,
        "url": f"https://www.reddit.com/r/{sub}/comments/{pid}/slug/",
        "selftext": f"body of {pid}", "over_18": False,
        "link_flair_text": "Discussion",
    }
    row.update(extra)
    return row


DAY = 86400
# 两条已结算(>36h)、一条未结算(<36h)。未结算那条分数故意给得最高，
# 用来验证「未结算不参与排到前面」。
SETTLED_HIGH = post("aaa111", "settled high score", 4200, 380, 10 * DAY)
SETTLED_LOW = post("bbb222", "settled low score", 90, 4, 5 * DAY)
UNSETTLED = post("ccc333", "unsettled but looks high", 9999, 1, 3600)
POSTS = [UNSETTLED, SETTLED_LOW, SETTLED_HIGH]


def comment(cid, body, score, age_seconds=5 * DAY, replies=None):
    node = {
        "id": cid, "author": "commenter", "subreddit": "LocalLLaMA",
        "score": score, "created_utc": time.time() - age_seconds,
        "body": body, "link_id": "t3_aaa111",
    }
    node["replies"] = ({"kind": "Listing",
                        "data": {"children": [{"kind": "t1", "data": r}
                                              for r in replies]}}
                       if replies else "")
    return node


TREE = [
    {"kind": "t1", "data": comment("c1", "top level", 30, replies=[
        comment("c2", "nested reply", 12, replies=[
            comment("c3", "deep reply", 3)])])},
    {"kind": "t1", "data": comment("c4", "another top level", 7)},
    {"kind": "more", "data": {"children": ["x1", "x2", "x3"]}},
]

TIMEOUT_BODY = {"data": None, "error": "Timeout. Maybe slow down a bit"}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def _send(self, status, payload, headers=None, raw=None):
        body = raw if raw is not None else json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def _apply(self, mode, ok_payload):
        if mode == "429":
            return self._send(429, {"error": "slow down"},
                              {"x-ratelimit-reset": "42"})
        if mode == "500":
            return self._send(500, {"error": "boom"})
        if mode == "error":
            return self._send(200, {"data": None,
                                    "error": "'query' query parameter requires "
                                             "one of: author, subreddit"})
        if mode == "garbage":
            return self._send(200, None, raw=b"<html>not json</html>")
        if mode == "always_timeout":
            return self._send(200, TIMEOUT_BODY)
        if mode == "timeout_then_ok" and not _timeout_used["value"]:
            _timeout_used["value"] = True
            return self._send(200, TIMEOUT_BODY)
        if mode == "empty":
            return self._send(200, {"data": []})
        return self._send(200, {"data": ok_payload})

    def do_GET(self):
        parsed = urlparse(self.path)
        path, query = parsed.path, parse_qs(parsed.query)
        HITS[path] += 1
        LAST_QUERY.update({k: v[0] for k, v in query.items()})

        if path == "/api/posts/search":
            limit = int(query.get("limit", ["25"])[0])
            rows = POSTS[:limit]
            # after/before 服务端过滤：sort=top 的分段取样靠它把窗口切开
            lo = float(query.get("after", ["0"])[0] or 0)
            hi = float(query.get("before", [str(time.time() + 1)])[0] or time.time() + 1)
            rows = [r for r in rows if lo <= r["created_utc"] <= hi]
            return self._apply(MODE["posts"], rows)
        if path == "/api/posts/ids":
            return self._apply(MODE["ids"], [SETTLED_HIGH])
        if path == "/api/comments/tree":
            return self._apply(MODE["tree"], TREE)
        if path == "/api/comments/search":
            return self._apply(MODE["posts"], [
                comment("uc1", "user comment body", 5)])
        if path == "/api/subreddits/search":
            if MODE["subs"] == "broken":
                return self._apply("error", None)
            prefix = "subreddit_prefix" in query
            if MODE["subs"] == "prefix_empty" and prefix:
                return self._send(200, {"data": []})
            return self._send(200, {"data": [
                {"display_name": "LocalLLaMA", "subscribers": 328652,
                 "public_description": "prefix hit" if prefix else "exact hit",
                 "over18": False, "created_utc": 1678484104}]})
        return self._send(404, {"data": None, "error": "no such route"})


def serve():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}"
