#!/usr/bin/env python3
"""多用户(09-10 分层架构阶段 5)端到端:隔离 daemon + 桩模型,走 HTTP 接口。

BIN=~/.cache/miyu-arch-fixes/target/release/miyu python3 testkit/multi-user/e2e.py

检查项:
  1. `-p` 起 daemon → 账号表里出现管理员 admin;只填口令能登录,用户名+口令也能登录
  2. 管理员生成邀请码 → 凭邀请码注册成员 → 邀请码用过一次即失效
  3. 成员的会话列表是空的,看不到管理员的会话;按 id 打开管理员会话 404
  4. 成员建会话 → 只在成员列表里;管理员列表里看不到;bootstrap 各归各
  5. 成员打不开管理台(/api/config、/api/dash/*、邀请码接口 403)
  6. 成员跑一轮(桩模型)→ usage-history 记到成员账号;成员 /api/usage/stats 只看自己;
     管理员 stats.accounts 按人拆分
  7. SSE:成员那条流里看不到管理员会话的事件;管理员流里看不到成员的
  8. 管理员停用成员 → 成员登录失败;恢复 → 能登录
"""
import http.client
import json
import os
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
BIN = Path(os.environ["BIN"]).expanduser()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-multi-user")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18491"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18497"))
BASE = f"http://127.0.0.1:{PORT}"
ADMIN_PASSWORD = "hunter2-admin"
# MIYU_ADMIN_USER 固定成 admin:管理员用户名 = 家目录名,不能随跑测试的系统用户名变。
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME),
           MIYU_SYSTEM_SCRIPTS_DIR=str(REPO / "src/scripts"), MIYU_ADMIN_USER="admin")
STUB_SYSTEM_DUMP = OUT / "stub-system.jsonl"

results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail else ""), flush=True)


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-a"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-a"],
            "model_context_window": {"stub-a": 100000},
        }],
        "memory": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def wait_http(url, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except urllib.error.HTTPError:
            return True
        except Exception:
            time.sleep(0.25)
    return False


class Client:
    """带 cookie 的极简客户端;Origin 头对齐 Host,过来源校验。"""

    def __init__(self):
        self.cookie = None

    def call(self, method, path, body=None, raw=False):
        conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=20)
        headers = {"Accept": "application/json", "Origin": f"http://127.0.0.1:{PORT}"}
        if self.cookie:
            headers["Cookie"] = self.cookie
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        conn.request(method, path, body=data, headers=headers)
        response = conn.getresponse()
        set_cookie = response.getheader("Set-Cookie")
        if set_cookie:
            self.cookie = set_cookie.split(";", 1)[0]
        payload = response.read()
        conn.close()
        if raw:
            return response.status, payload
        try:
            return response.status, json.loads(payload) if payload else {}
        except json.JSONDecodeError:
            return response.status, {"_raw": payload.decode("utf-8", "replace")}

    def login(self, password, username=None):
        body = {"password": password}
        if username:
            body["username"] = username
        return self.call("POST", "/api/auth/login", body)


def sse_collect(cookie, seconds):
    """开一条 SSE 流,收 `seconds` 秒内的事件种类 + 载荷。"""
    events = []

    def run():
        conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=seconds + 2)
        conn.request("GET", "/api/events", headers={"Cookie": cookie, "Accept": "text/event-stream"})
        response = conn.getresponse()
        deadline = time.time() + seconds
        kind = None
        try:
            while time.time() < deadline:
                line = response.fp.readline()
                if not line:
                    break
                text = line.decode("utf-8", "replace").rstrip("\n")
                if text.startswith("event:"):
                    kind = text[6:].strip()
                elif text.startswith("data:") and kind:
                    events.append((kind, text[5:].strip()))
                    kind = None
        except Exception:
            pass
        finally:
            conn.close()

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return events, thread


def run_turn(client, session_id, text):
    status, data = client.call("POST", "/api/turns", {"content": text, "session_id": session_id})
    assert status in (200, 201, 202), f"turn start {status} {data}"
    deadline = time.time() + 30
    while time.time() < deadline:
        status, view = client.call("GET", f"/api/sessions/{session_id}/turns")
        if status == 200 and not view.get("runs") and view.get("turns"):
            return view
        time.sleep(0.4)
    raise AssertionError("turn did not finish")


def main():
    if OUT.exists():
        import shutil
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_config()
    password_file = OUT / "web-password"
    password_file.write_text(ADMIN_PASSWORD + "\n")
    stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), MODE="plain", STUB_CHUNK_SLEEP="0.01",
                    STUB_DUMP_SYSTEM=str(STUB_SYSTEM_DUMP))
    stub = subprocess.Popen([sys.executable, str(REPO / "testkit/webui-fixes/stub_reasoning.py")], env=stub_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT), "--bind", "127.0.0.1",
                                   "--password-file", str(password_file)],
                                  env=ENV, cwd=str(HOME), stdout=(OUT / "daemon.log").open("w"),
                                  stderr=subprocess.STDOUT)
        assert wait_http(f"{BASE}/api/health"), "daemon not up"
        time.sleep(1)

        # 0. 家目录布局(阶段 6):新装即新布局
        marker = HOME / ".home-layout-v1"
        check("新装即家目录布局(标记=admin)", marker.is_file() and marker.read_text().strip() == "admin",
              marker.read_text().strip() if marker.exists() else "missing")
        check("会话库在管理员家目录", (HOME / "home/admin/conversation.db").is_file())
        check("state 里没有会话库", not (HOME / "state/conversation.db").exists())

        # 1. 登录
        anon = Client()
        status, _ = anon.call("GET", "/api/bootstrap")
        check("未登录 bootstrap 401", status == 401, str(status))
        admin = Client()
        status, _ = admin.login(ADMIN_PASSWORD)
        check("只填口令登录(机器级管理员)", status == 204, str(status))
        status, boot = admin.call("GET", "/api/bootstrap")
        check("管理员 bootstrap admin=true", status == 200 and boot.get("capabilities", {}).get("admin") is True
              and boot.get("capabilities", {}).get("multi_user") is True, json.dumps(boot.get("capabilities")))
        admin_named = Client()
        status, _ = admin_named.login(ADMIN_PASSWORD, "admin")
        check("用户名 admin + 口令登录", status == 204, str(status))
        status, me = admin_named.call("GET", "/api/account")
        check("admin 账号行存在且是管理员", status == 200 and me.get("account", {}).get("admin") is True
              and me.get("account", {}).get("username") == "admin", json.dumps(me))
        status, _ = Client().login("wrong-password", "admin")
        check("错误密码 401", status == 401, str(status))

        # 管理员先建一条会话
        status, created = admin.call("POST", "/api/sessions", {"name": "管理员的会话"})
        admin_session = created.get("session", {}).get("session_id")
        check("管理员建会话", status == 201 and bool(admin_session), str(status))

        # 2. 邀请码 → 注册
        status, invite = admin.call("POST", "/api/admin/invites", {})
        code = invite.get("code", "")
        check("生成邀请码", status == 201 and len(code) == 8, json.dumps(invite))
        member = Client()
        status, data = member.call("POST", "/api/auth/register",
                                   {"invite": code, "username": "alice", "display_name": "爱丽丝", "password": "alice-pass"})
        check("凭邀请码注册成员", status == 204, f"{status} {data}")
        check("注册即建家目录 home/alice", (HOME / "home/alice").is_dir())
        status, data = Client().call("POST", "/api/auth/register",
                                     {"invite": code, "username": "ADMIN", "password": "admin-pass-2"})
        check("与管理员家目录同名的用户名被拒", status == 400, f"{status} {data}")
        status, data = Client().call("POST", "/api/auth/register",
                                     {"invite": code, "username": "bob", "password": "bob-pass-1"})
        check("邀请码二次使用被拒", status == 400, f"{status} {data}")
        status, data = Client().call("POST", "/api/auth/register",
                                     {"invite": "NOPE1234", "username": "carol", "password": "carol-pass"})
        check("无效邀请码被拒", status == 400, f"{status}")
        status, invites = admin.call("GET", "/api/admin/invites")
        used = [item for item in invites.get("invites", []) if item.get("status") == "used"]
        check("邀请码列表标记已使用", status == 200 and len(used) == 1, json.dumps(invites)[:200])

        # 3. 成员视角
        status, boot = member.call("GET", "/api/bootstrap")
        member_current = boot.get("current_session_id")
        check("成员 bootstrap admin=false 且自带当前会话", status == 200
              and boot.get("capabilities", {}).get("admin") is False and bool(member_current),
              f"{status} current={member_current}")
        member_ids = {item["session_id"] for item in boot.get("sessions", [])}
        check("成员列表里没有管理员的会话", admin_session not in member_ids and member_current in member_ids,
              json.dumps(sorted(member_ids)))
        status, _ = member.call("GET", f"/api/sessions/{admin_session}/turns")
        check("成员按 id 打开管理员会话 404", status == 404, str(status))
        status, _ = member.call("PATCH", f"/api/sessions/{admin_session}", {"name": "篡改"})
        check("成员改名管理员会话 404", status == 404, str(status))
        status, _ = member.call("POST", "/api/sessions", {"mode": "dev"})
        check("成员建 dev 会话 403", status == 403, str(status))

        # 4. 成员建会话
        status, created = member.call("POST", "/api/sessions", {"name": "爱丽丝的会话"})
        member_session = created.get("session", {}).get("session_id")
        check("成员建会话", status == 201 and bool(member_session), str(status))
        status, listing = admin.call("GET", "/api/sessions")
        admin_ids = {item["session_id"] for item in listing.get("sessions", [])}
        check("管理员列表里看不到成员的会话", member_session not in admin_ids and admin_session in admin_ids,
              json.dumps(sorted(admin_ids)))
        status, _ = admin.call("GET", f"/api/sessions/{member_session}/turns")
        check("管理员按 id 打开成员会话 404", status == 404, str(status))
        status, listing = member.call("GET", "/api/sessions")
        member_ids = {item["session_id"] for item in listing.get("sessions", [])}
        check("成员列表含自己两条会话", member_session in member_ids and member_current in member_ids
              and admin_session not in member_ids, json.dumps(sorted(member_ids)))

        # 5. 管理台闸
        for path in ["/api/config", "/api/dash/memory/personas", "/api/admin/invites", "/api/admin/accounts",
                     "/api/voice/status", "/api/dash/scripts/personas"]:
            status, _ = member.call("GET", path)
            check(f"成员 GET {path} 403", status == 403, str(status))
        status, _ = member.call("POST", "/api/admin/invites", {})
        check("成员生成邀请码 403", status == 403, str(status))
        status, _ = member.call("POST", "/api/usage/clear")
        check("成员清空统计 403", status == 403, str(status))
        status, _ = member.call("GET", "/api/usage/stats?range=all")
        check("成员看自己的统计 200", status == 200, str(status))

        # 档案(阶段 6):各写各的,回合里只带自己的
        status, data = member.call("PATCH", "/api/account", {"profile": "请叫我爱丽丝,我养了两只猫。"})
        check("成员写档案", status == 200, f"{status} {data}")
        check("档案落在 home/alice/profile.md",
              (HOME / "home/alice/profile.md").read_text().startswith("请叫我爱丽丝") if (HOME / "home/alice/profile.md").exists() else False)
        status, data = admin.call("PATCH", "/api/account", {"profile": "我是管理员 shorin,喜欢 Arch Linux。"})
        check("管理员(口令登录)写属主档案", status == 200, f"{status} {data}")
        check("属主档案落在 home/admin/profile.md",
              (HOME / "home/admin/profile.md").read_text().startswith("我是管理员") if (HOME / "home/admin/profile.md").exists() else False)
        status, me_admin = admin.call("GET", "/api/account")
        check("GET /api/account 带档案", status == 200 and me_admin.get("profile", "").startswith("我是管理员"), json.dumps(me_admin)[:120])

        # 7. SSE 准备:两条流同时收
        member_events, member_thread = sse_collect(member.cookie, 14)
        admin_events, admin_thread = sse_collect(admin.cookie, 14)
        time.sleep(0.5)

        # 6. 成员与管理员各跑一轮
        member_view = run_turn(member, member_session, "你好,我是爱丽丝")
        admin_view = run_turn(admin, admin_session, "你好,我是管理员")
        check("成员回合完成", bool(member_view.get("turns")), str(len(member_view.get("turns", []))))
        check("管理员回合完成", bool(admin_view.get("turns")), str(len(admin_view.get("turns", []))))
        dumps = [json.loads(line)["system"] for line in STUB_SYSTEM_DUMP.read_text().splitlines() if line.strip()] \
            if STUB_SYSTEM_DUMP.exists() else []
        member_prompts = [s for s in dumps if "请叫我爱丽丝" in s]
        admin_prompts = [s for s in dumps if "我是管理员 shorin" in s]
        check("成员回合的系统提示词带成员档案、不带属主档案",
              len(member_prompts) == 1 and "我是管理员" not in member_prompts[0], f"{len(dumps)} requests")
        check("管理员回合的系统提示词带属主档案、不带成员档案",
              len(admin_prompts) == 1 and "爱丽丝" not in admin_prompts[0], f"{len(admin_prompts)}")
        time.sleep(1.5)
        history = HOME / "state" / "usage-history.jsonl"
        if not history.exists():
            candidates = list(HOME.rglob("usage-history.jsonl"))
            history = candidates[0] if candidates else history
        records = [json.loads(line) for line in history.read_text().splitlines() if line.strip()] if history.exists() else []
        member_records = [record for record in records if record.get("acct")]
        status, me = member.call("GET", "/api/account")
        member_id = me.get("account", {}).get("account_id")
        check("成员回合记到成员账号", any(record.get("acct") == member_id for record in member_records),
              f"acct={member_id} records={[r.get('acct') for r in records]}")
        check("管理员回合记在空账号", any(not record.get("acct") for record in records), str(len(records)))
        status, stats = member.call("GET", "/api/usage/stats?range=all")
        totals = stats.get("stats", {}).get("totals", {})
        check("成员 stats 只含自己的一次调用", status == 200 and totals.get("requests") == 1
              and not stats.get("stats", {}).get("accounts"), json.dumps(totals))
        status, stats = admin.call("GET", "/api/usage/stats?range=all")
        accounts = stats.get("stats", {}).get("accounts", [])
        check("管理员 stats 按人拆分(两行)", status == 200 and len(accounts) == 2
              and {item.get("acct") for item in accounts} == {"", member_id}, json.dumps(accounts)[:200])
        status, per = admin.call("GET", "/api/admin/usage/accounts?range=all")
        named = [item for item in per.get("accounts", []) if item.get("acct") == member_id]
        check("管理台按人用量带用户名", status == 200 and named and named[0].get("username") == "alice",
              json.dumps(per)[:200])
        status, details = member.call("GET", "/api/usage/details?limit=50")
        check("成员明细只含自己的记录", status == 200 and details.get("records")
              and all(record.get("acct") == member_id for record in details["records"]),
              str(len(details.get("records", []))))

        # 7. SSE 归属
        member_thread.join(timeout=16)
        admin_thread.join(timeout=16)

        def sessions_in(events):
            seen = set()
            for _, data in events:
                try:
                    payload = json.loads(data)
                except json.JSONDecodeError:
                    continue
                if isinstance(payload, dict) and payload.get("session_id"):
                    seen.add(payload["session_id"])
            return seen

        member_seen = sessions_in(member_events)
        admin_seen = sessions_in(admin_events)
        check("成员 SSE 只见自己的会话", member_session in member_seen and admin_session not in member_seen,
              f"seen={sorted(member_seen)} kinds={sorted({k for k, _ in member_events})}")
        check("管理员 SSE 不见成员的会话", admin_session in admin_seen and member_session not in admin_seen,
              f"seen={sorted(admin_seen)}")
        member_deltas = [k for k, _ in member_events if k in ("assistant.delta", "run.completed")]
        check("成员 SSE 收到自己回合的增量", bool(member_deltas), str(len(member_deltas)))

        # 8. 停用/恢复
        status, _ = admin.call("PATCH", f"/api/admin/accounts/{member_id}", {"disabled": True})
        check("管理员停用成员", status == 200, str(status))
        status, _ = Client().login("alice-pass", "alice")
        check("停用后成员登录 401", status == 401, str(status))
        status, _ = admin.call("PATCH", f"/api/admin/accounts/{member_id}", {"disabled": False})
        status, _ = Client().login("alice-pass", "alice")
        check("恢复后成员能登录", status == 204, str(status))
        status, _ = admin.call("PATCH", f"/api/admin/accounts/{member_id}", {"password": "alice-new-pass"})
        status, _ = Client().login("alice-new-pass", "alice")
        check("管理员重设密码后新密码可登录", status == 204, str(status))
        status, data = member.call("PATCH", "/api/account", {"display_name": "Alice", "current_password": "alice-new-pass",
                                                             "password": "alice-final"})
        check("成员改自己的显示名+密码", status == 200 and data.get("account", {}).get("display_name") == "Alice",
              f"{status} {data}")
        status, _ = member.call("POST", "/api/auth/logout")
        status2, _ = member.call("GET", "/api/bootstrap")
        check("退出登录后 401", status == 204 and status2 == 401, f"{status} {status2}")
        status, _ = admin_named.call("PATCH", f"/api/admin/accounts/{me.get('account', {}).get('account_id')}",
                                     {"disabled": True})
        # 最后一个管理员不能停用自己
        status, admins = admin_named.call("GET", "/api/admin/accounts")
        admin_id = next((item["id"] for item in admins.get("accounts", []) if item.get("admin")), None)
        status, data = admin_named.call("PATCH", f"/api/admin/accounts/{admin_id}", {"disabled": True})
        check("不能停用自己/最后一个管理员", status == 400, f"{status} {data}")
    finally:
        if daemon:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        stub.terminate()
    failed = [name for name, ok, _ in results if not ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} PASS")
    (OUT / "report.json").write_text(json.dumps(results, ensure_ascii=False, indent=2))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
