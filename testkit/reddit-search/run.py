#!/usr/bin/env python3
"""reddit-search 黑盒测试。

真的 Arctic Shift 不会听你的：限流、查询超时、返回畸形、字段缺失这些恰恰
是最需要测的分支。所以把脚本复制一份、只把 ARCTIC_BASE 常量改指到本地假
服务器，再用子进程跑它——走的是真正的 stdin JSON 契约和真正的 HTTP 栈，
只有对面是假的。

用法: python3 testkit/reddit-search/run.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fake_arctic  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCRIPT = os.path.join(REPO, "src", "scripts", "personas", "default", "reddit-search")

PASS, FAIL = [], []


def patch_script(base, workdir, name="reddit-search"):
    """只改 ARCTIC_BASE 一个常量，其它一个字不动。"""
    patched = open(SCRIPT, encoding="utf-8").read().replace(
        'ARCTIC_BASE = "https://arctic-shift.photon-reddit.com"',
        f'ARCTIC_BASE = "{base}"')
    line = next(l for l in patched.splitlines() if l.startswith("ARCTIC_BASE = "))
    assert "127.0.0.1" in line, "ARCTIC_BASE 没被替换（脚本里的常量改名了？）"
    target = os.path.join(workdir, name)
    open(target, "w", encoding="utf-8").write(patched)
    os.chmod(target, 0o755)
    return target


class Runner:
    def __init__(self, script, cache_root):
        self.script = script
        self.cache_root = cache_root
        self.serial = 0
        self.cache = None

    def fresh_cache(self):
        self.serial += 1
        self.cache = os.path.join(self.cache_root, f"c{self.serial}")
        os.makedirs(self.cache, exist_ok=True)
        return self.cache

    def call(self, args, argv=None):
        environ = dict(os.environ)
        environ["MIYU_SCRIPT_CACHE_DIR"] = self.cache or self.fresh_cache()
        environ.pop("MIYU_ARGS_JSON", None)
        proc = subprocess.run([self.script] + (argv or []),
                              input=json.dumps(args), capture_output=True,
                              text=True, timeout=60, env=environ)
        return proc.returncode, proc.stdout, proc.stderr

    def json_call(self, args):
        code, out, err = self.call(dict(args, format="json"))
        try:
            return code, json.loads(out), out
        except json.JSONDecodeError:
            return code, {}, out + err


def check(name, condition, detail=""):
    if condition:
        PASS.append(name)
        print(f"  ✅ {name}")
    else:
        FAIL.append((name, detail))
        print(f"  ❌ {name}\n     {detail}")


def titles(payload):
    return [r["title"] for r in payload.get("results", [])]


def main():
    server, base = fake_arctic.serve()
    workdir = tempfile.mkdtemp(prefix="reddit-search-test-")
    try:
        run = Runner(patch_script(base, workdir), os.path.join(workdir, "cache"))
        run.fresh_cache()

        # -------- 1 基本检索
        print("\n[1] 版块检索")
        fake_arctic.reset()
        code, payload, raw = run.json_call({"subreddit": "r/LocalLLaMA",
                                            "limit": 3, "full": False})
        check("按时间倒序拿到结果",
              code == 0 and len(payload.get("results", [])) == 3, raw[:300])
        check("r/ 前缀被剥掉", fake_arctic.LAST_QUERY.get("subreddit") == "LocalLLaMA",
              str(dict(fake_arctic.LAST_QUERY)))
        check("没有 permalink 时自己拼链接",
              payload["results"][0]["permalink"] ==
              "https://www.reddit.com/r/LocalLLaMA/comments/ccc333",
              payload["results"][0]["permalink"])
        check("36 小时内的算未结算",
              payload["results"][0]["counts_settled"] is False, raw[:200])
        check("36 小时外的算已结算",
              payload["results"][1]["counts_settled"] is True, raw[:200])
        code, out, _ = run.call({"subreddit": "LocalLLaMA", "limit": 3, "full": False})
        check("md 里不把未结算写成 9999↑",
              "赞数未结算" in out and "9999↑" not in out, out[:400])
        check("md 里已结算的照常显示", "4200↑" in out and "90↑" in out, out[:400])

        # -------- 2 全站关键词搜索：明确报错，不发请求
        print("\n[2] 全站关键词搜索（归档做不到）")
        fake_arctic.reset()
        code, payload, raw = run.json_call({"query": "deepseek"})
        check("直接失败", code == 2 and payload.get("ok") is False, raw[:300])
        check("fix 指向补 subreddit/author",
              "subreddit" in (payload.get("fix") or ""), raw[:300])
        check("一个请求都没发（不白等）", sum(fake_arctic.HITS.values()) == 0,
              str(dict(fake_arctic.HITS)))

        # -------- 3 sort=top 的窗口采样
        print("\n[3] sort=top 窗口采样")
        fake_arctic.reset()
        code, payload, raw = run.json_call({"subreddit": "LocalLLaMA",
                                            "sort": "top", "time": "month",
                                            "limit": 5, "full": False})
        check("分 5 段取样", fake_arctic.HITS["/api/posts/search"] == 5,
              f"实际 {fake_arctic.HITS['/api/posts/search']} 次")
        check("高分已结算的排第一",
              titles(payload)[:1] == ["settled high score"], str(titles(payload)))
        check("未结算的 9999 分没混进结果（窗口已排除最近 36h）",
              "unsettled but looks high" not in titles(payload), str(titles(payload)))
        check("说明里点出这是采样不是排名",
              any("取样" in n for n in payload.get("notes", [])),
              str(payload.get("notes")))
        check("说明里点出排除了最近 36 小时",
              any("36 小时" in n for n in payload.get("notes", [])),
              str(payload.get("notes")))
        code, payload, raw = run.json_call({"subreddit": "LocalLLaMA",
                                            "sort": "top", "time": "day",
                                            "limit": 3, "full": False})
        check("窗口整个落在结算期内时如实说明",
              any("结算期" in n for n in payload.get("notes", [])),
              str(payload.get("notes")))

        # -------- 4 超时自动重试一次
        print("\n[4] 查询超时自动重试")
        fake_arctic.reset(posts="timeout_then_ok")
        code, payload, raw = run.json_call({"subreddit": "LocalLLaMA", "limit": 1,
                                            "full": False})
        check("首次 Timeout 后重试成功", code == 0 and payload.get("results"), raw[:300])
        check("正好打了两次", fake_arctic.HITS["/api/posts/search"] == 2,
              f"实际 {fake_arctic.HITS['/api/posts/search']} 次")
        fake_arctic.reset(posts="always_timeout")
        code, payload, raw = run.json_call({"subreddit": "LocalLLaMA", "limit": 1})
        check("一直超时就如实报错",
              code == 2 and "Timeout" in payload.get("error", ""), raw[:300])
        check("超时退避重试有上限（4 次后放弃）",
              fake_arctic.HITS["/api/posts/search"] == 4,
              f"实际 {fake_arctic.HITS['/api/posts/search']} 次")
        check("放弃时说明重试过几次",
              "退避重试 3 次" in (payload.get("fix") or ""), str(payload.get("fix")))
        check("超时的 fix 教人收窄时间范围",
              "after/before" in (payload.get("fix") or ""), str(payload.get("fix")))
        fake_arctic.reset(posts="error")
        code, payload, raw = run.json_call({"subreddit": "LocalLLaMA", "limit": 1})
        check("非超时的错误不重试", fake_arctic.HITS["/api/posts/search"] == 1,
              f"实际 {fake_arctic.HITS['/api/posts/search']} 次")

        # -------- 5 HTTP 层故障
        print("\n[5] 限流与服务端故障")
        for mode, name, expect in (("429", "429 限流", "429"),
                                   ("500", "500 故障", "服务端故障 (HTTP 500)"),
                                   ("garbage", "返回不是 JSON", "HTTP 200")):
            fake_arctic.reset(posts=mode)
            code, payload, raw = run.json_call({"subreddit": "LocalLLaMA", "limit": 1})
            check(f"{name}：结构化失败",
                  code == 2 and payload.get("ok") is False, raw[:200])
            check(f"{name}：错误里点明原因", expect in payload.get("error", ""),
                  payload.get("error", raw[:200]))
            check(f"{name}：带 fix", bool(payload.get("fix")), raw[:200])
        fake_arctic.reset(posts="empty")
        code, out, _ = run.call({"subreddit": "LocalLLaMA", "limit": 1})
        check("空结果是成功不是失败", code == 0 and "没有结果" in out, out[:200])

        # -------- 6 连不上
        print("\n[6] 服务连不上")
        import socket
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        dead = sock.getsockname()[1]
        sock.close()
        dead_run = Runner(patch_script(f"http://127.0.0.1:{dead}", workdir,
                                       name="reddit-search-dead"),
                          os.path.join(workdir, "cache-dead"))
        dead_run.fresh_cache()
        code, payload, raw = dead_run.json_call({"subreddit": "LocalLLaMA", "limit": 1})
        check("连不上时结构化失败",
              code == 2 and "连不上" in payload.get("error", ""), raw[:300])
        check("连不上时给的是网络类 fix",
              "网络" in (payload.get("fix") or ""), str(payload.get("fix")))

        # -------- 7 评论树
        print("\n[7] 评论树")
        fake_arctic.reset()
        code, payload, raw = run.json_call({"query": "aaa111", "limit": 20, "depth": 3})
        bodies = [c.get("body") for c in payload["comments"] if c["kind"] == "comment"]
        depths = [c["depth"] for c in payload["comments"] if c["kind"] == "comment"]
        check("裸 id 自动切到 comments", payload.get("type") == "thread", raw[:200])
        check("整棵树按层展开",
              bodies == ["top level", "nested reply", "deep reply", "another top level"],
              str(bodies))
        check("层级正确", depths == [0, 1, 2, 0], str(depths))
        check("折叠节点如实标出",
              any(c["kind"] == "more" and c["count"] == 3 for c in payload["comments"]),
              raw[:400])
        code, payload, _ = run.json_call({"query": "aaa111", "limit": 20, "depth": 1})
        check("depth 限深生效",
              [c["body"] for c in payload["comments"] if c["kind"] == "comment"] ==
              ["top level", "nested reply", "another top level"], "")
        code, payload, _ = run.json_call({"query": "aaa111", "limit": 2, "depth": 3})
        check("limit 限总条数生效", len(payload["comments"]) == 2, "")
        for name, url in (
                ("版块帖链接", "https://www.reddit.com/r/LocalLLaMA/comments/aaa111/slug/"),
                ("用户帖链接", "https://www.reddit.com/user/spez/comments/aaa111/slug/"),
                ("u/ 短形式", "https://www.reddit.com/u/spez/comments/aaa111/"),
                ("redd.it 短链", "https://redd.it/aaa111"),
                ("评论定位链接",
                 "https://www.reddit.com/r/LocalLLaMA/comments/aaa111/slug/c1/")):
            code, payload, raw = run.json_call({"query": url})
            check(f"{name}认得出帖子 id",
                  payload.get("type") == "thread"
                  and payload.get("post", {}).get("id") == "aaa111", raw[:200])
        code, payload, raw = run.json_call({"query": "t3_aaa111"})
        check("t3_ 前缀也认", payload.get("type") == "thread", raw[:200])
        fake_arctic.reset(ids="empty")
        code, payload, raw = run.json_call({"query": "aaa111"})
        check("归档里没有这帖时说人话",
              code == 2 and "没有这个帖子" in payload.get("error", ""), raw[:300])

        # -------- 8 user
        print("\n[8] 用户")
        fake_arctic.reset()
        code, payload, raw = run.json_call({"action": "user", "author": "u/tester",
                                            "limit": 4})
        check("同时拿到帖子和评论",
              bool(payload.get("posts")) and bool(payload.get("comments")), raw[:300])
        check("u/ 前缀被剥掉", fake_arctic.LAST_QUERY.get("author") == "tester",
              str(dict(fake_arctic.LAST_QUERY)))
        code, payload, _ = run.json_call({"action": "user", "author": "tester",
                                          "kind": "posts", "limit": 4})
        check("kind=posts 只取帖子",
              bool(payload.get("posts")) and not payload.get("comments"), "")
        code, payload, raw = run.json_call({"action": "user"})
        check("缺 author 时报错",
              code == 2 and "author" in payload.get("error", ""), raw[:200])

        # -------- 9 版块搜索
        print("\n[9] 版块搜索")
        fake_arctic.reset()
        code, payload, raw = run.json_call({"action": "subreddits", "query": "localll"})
        check("前缀命中就用前缀结果",
              payload["results"][0]["description"] == "prefix hit", raw[:300])
        fake_arctic.reset(subs="prefix_empty")
        code, payload, raw = run.json_call({"action": "subreddits", "query": "localll"})
        check("前缀没命中就退回全名匹配",
              payload["results"][0]["description"] == "exact hit", raw[:300])
        check("确实打了两次", fake_arctic.HITS["/api/subreddits/search"] == 2,
              f"实际 {fake_arctic.HITS['/api/subreddits/search']} 次")

        # -------- 10 doctor
        print("\n[10] doctor 自检")
        fake_arctic.reset()
        code, out, _ = run.call({"action": "doctor"})
        check("两项都健康时全绿",
              code == 0 and out.count("✅") == 2 and "❌" not in out, out[:400])
        fake_arctic.reset(posts="always_timeout")
        code, out, _ = run.call({"action": "doctor"})
        check("超时时如实标红并给下一步",
              "❌" in out and "下一步" in out, out[:400])

        # -------- 11 参数与边界
        print("\n[11] 参数与边界")
        fake_arctic.reset()
        for name, args, expect in (
                ("空参数", {}, "至少要给"),
                ("未知 action", {"action": "nope"}, "未知的 action"),
                ("看不懂的时间", {"subreddit": "x", "after": "下周二"}, "看不懂的时间"),
                ("comments 缺 query", {"action": "comments"}, "需要帖子链接"),
                ("认不出的 query", {"action": "comments", "query": "这是什么"}, "认不出")):
            code, payload, raw = run.json_call(args)
            check(f"{name}：报错并带 fix",
                  code == 2 and expect in payload.get("error", "")
                  and bool(payload.get("fix")), raw[:200])
        code, out, _ = run.call({"stdin": "not json"})
        check("坏 JSON 也给结构化输出", '"ok": false' in out, out[:200])
        fake_arctic.reset()
        run.json_call({"subreddit": "LocalLLaMA", "limit": 9999})
        check("limit 上界夹到 100", fake_arctic.LAST_QUERY.get("limit") == "100",
              str(dict(fake_arctic.LAST_QUERY)))
        run.json_call({"subreddit": "LocalLLaMA", "limit": -5})
        check("limit 下界夹到 1", fake_arctic.LAST_QUERY.get("limit") == "1",
              str(dict(fake_arctic.LAST_QUERY)))
        fake_arctic.reset()
        run.json_call({"subreddit": "LocalLLaMA", "after": "7d", "limit": 2})
        check("相对时间原样交给服务端（它自己认 7d）",
              fake_arctic.LAST_QUERY.get("after") == "7d",
              str(dict(fake_arctic.LAST_QUERY)))

        # -------- 12 命令行形态
        print("\n[12] 命令行形态")
        fake_arctic.reset()
        code, out, err = run.call({}, argv=["-r", "LocalLLaMA", "-n", "2", "--no-full"])
        check("argv 模式能出结果", code == 0 and "# Reddit ·" in out, (out + err)[:300])
        code, out, err = run.call({}, argv=["--help"])
        check("--help 可用", code == 0 and "usage: reddit-search" in out, out[:200])
    finally:
        server.shutdown()
        shutil.rmtree(workdir, ignore_errors=True)

    print(f"\n{'=' * 50}\n通过 {len(PASS)} / {len(PASS) + len(FAIL)}")
    for name, detail in FAIL:
        print(f"  失败: {name}\n    {detail}")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
