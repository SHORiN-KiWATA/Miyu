#!/usr/bin/env python3
"""WebUI 09-09 四项改动的真机走查：链接成链 / 链接卡片 / 芯片截断 / 表情包瀑布流。

不花额度：模型换成本目录下的桩，回一段内容固定、含各种链接形态的正文。
链接卡片那一步**会真的联网**：卡片走的出站请求过 SSRF 闸门，而闸门明确禁止
本机与内网地址——拿本地桩当被抓的站，等于要求闸门放行 127.0.0.1，那是它存在的
理由本身。所以这一步用几个公网地址真抓一次，正好也验了闸门没误伤正常站点。

    python3 testkit/webui-links/run.py

前置：`cargo build`（静态资源编进二进制，改了 JS/CSS 必须重新构建），
      以及 playwright（`NODE_PATH=$(npm root -g)` 或 python 的 playwright 均可）。
"""

import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-webui-links/home"))
RUNTIME = "/tmp/mx-wl"
PORT = int(os.environ.get("MIYU_WL_PORT", "18411"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18495"))
SHOTS = Path(os.environ.get("MIYU_WL_SHOTS", Path.home() / ".cache" / "miyu-webui-links"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)


def api(path, body=None, method="GET"):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(
        BASE + path, data=data, method=method,
        headers={"Content-Type": "application/json", "Origin": BASE},
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        raw = response.read()
    return json.loads(raw or b"null")


def upload_attachment(session_id):
    """传一份 markdown 附件。附件预览那项要的就是「非图片、可读文本」这一类。"""
    body = ("# todolist\n\n"
            "- [x] 裸链接成链\n- [ ] 链接卡片\n- [ ] 附件预览\n\n"
            "这份文件是走查用的，点开芯片应该直接看到这段文字，而不是触发下载。\n"
            ).encode()
    request = urllib.request.Request(
        f"{BASE}/api/attachments?session_id={session_id}", data=body, method="POST",
        headers={"Content-Type": "application/octet-stream", "Origin": BASE,
                 "x-miyu-filename": urllib.parse.quote("todolist.md")},
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.loads(response.read())["id"]
    except Exception as error:
        print(f"! 附件上传失败：{error}", file=sys.stderr)
        return None


def upload_clip(session_id):
    """一段一秒的 mp4。视频附件以前点了只会下载，现在应该给播放器。"""
    clip = Path("/tmp/miyu-webui-links-clip.mp4")
    if not clip.exists():
        rendered = subprocess.run(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "lavfi",
             "-i", "testsrc=size=160x120:rate=10:duration=1",
             "-pix_fmt", "yuv420p", "-y", str(clip)],
            capture_output=True,
        )
        if rendered.returncode != 0 or not clip.exists():
            print("! ffmpeg 不可用，跳过视频附件那一步", file=sys.stderr)
            return None
    request = urllib.request.Request(
        f"{BASE}/api/attachments?session_id={session_id}", data=clip.read_bytes(),
        method="POST",
        headers={"Content-Type": "application/octet-stream", "Origin": BASE,
                 "x-miyu-filename": urllib.parse.quote("clip.mp4")},
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.loads(response.read())["id"]
    except Exception as error:
        print(f"! 视频附件上传失败：{error}", file=sys.stderr)
        return None


def write_config():
    """把模型池指到桩上。表情包插件开着，供瀑布流那步用。"""
    HOME.mkdir(parents=True, exist_ok=True)
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-model"}],
        "providers": [{
            "id": "stub",
            "display_name": "Stub",
            "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat",
            "api_key": "stub",
            "models": ["stub-model"],
        }],
        "memory": {"enabled": False},
        "tools": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(config, ensure_ascii=False, indent=2), encoding="utf-8"
    )


def seed_memes():
    """三张比例迥异的图，专门用来看瀑布流有没有把行高绑在一起。"""
    try:
        from PIL import Image
    except ImportError:
        print("! Pillow 缺失，跳过表情包瀑布流这一步", file=sys.stderr)
        return False
    library = HOME / "data" / "memes" / "miyu"
    images = library / "images"
    images.mkdir(parents=True, exist_ok=True)
    shapes = [("tall", (120, 420), (210, 90, 160)),
              ("square", (300, 300), (80, 150, 220)),
              ("wide", (480, 180), (110, 190, 120)),
              ("tall2", (140, 380), (230, 170, 80)),
              ("square2", (280, 280), (150, 120, 200)),
              ("wide2", (460, 200), (120, 200, 200))]
    entries = []
    for name, size, color in shapes:
        path = images / f"{name}.png"
        Image.new("RGB", size, color).save(path)
        # id 必须是真的 sha256 十六进制：面板的取图接口会校验它，随手编一个
        # 会被 400 挡掉，图出不来、比例也就量不到。
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        entries.append({
            "id": f"sha256:{digest}",
            "file": f"images/{name}.png",
            "mime_type": "image/png",
            "animated": False,
            "name": {"zh": f"{name} 形状", "en": name},
            "description": f"{size[0]}x{size[1]} 的测试图",
            "usage": "布局测试",
            "tags": ["测试"],
        })
    (library / "index.json").write_text(
        json.dumps({"library": "miyu", "version": 2, "memes": entries, "disabled_ids": []},
                   ensure_ascii=False, indent=2),
        encoding="utf-8",
    )
    return True


def spawn(script, port_env):
    return subprocess.Popen(
        [sys.executable, str(Path(__file__).parent / script)],
        env=dict(os.environ, **port_env),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def wait_http(url, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except urllib.error.HTTPError:
            return True
        except Exception:
            time.sleep(0.2)
    return False


def main():
    if not BIN.exists():
        print(f"! 先 cargo build：{BIN} 不存在", file=sys.stderr)
        return 2
    if HOME.exists():
        shutil.rmtree(HOME)
    Path(RUNTIME).mkdir(exist_ok=True)
    write_config()
    has_memes = seed_memes()

    stub = spawn("stub_llm.py", {"STUB_PORT": str(STUB_PORT)})
    daemon = None
    try:
        if not wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"):
            print("! 桩模型没起来", file=sys.stderr)
            return 2
        daemon = subprocess.Popen(
            [str(BIN), "__daemon", "--port", str(PORT)],
            env=ENV, cwd=str(HOME),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if not wait_http(f"{BASE}/api/config", timeout=30):
            print("! daemon 没起来", file=sys.stderr)
            return 2

        # 必须先建一个 WebUI 自己的会话:默认那条是「终端集成会话」,WebUI 有意
        # 不显示它(isTerminalSession 判定的隐藏车道)。往它里面发,页面永远是空的。
        session = api("/api/sessions", {"name": "链接走查", "switch": True}, "POST")
        session_id = session.get("session_id") or session.get("session", {}).get("session_id")
        print(f"· 会话 {session_id}")

        print("· 上传附件（文本 + 视频，验附件预览）")
        attachment = upload_attachment(session_id)
        clip = upload_clip(session_id)

        print("· 发一轮对话（桩模型，不花额度）")
        api("/api/turns", {
            "content": "LINKTEST 随意发几个网站链接。",
            "session_id": session_id,
            "attachment_ids": [a for a in (attachment, clip) if a],
        }, "POST")
        # 等回合落库。桩很快，2 秒足够。
        time.sleep(2.5)

        SHOTS.mkdir(parents=True, exist_ok=True)
        print(f"· 走查 {BASE} → {SHOTS}")
        result = subprocess.run(
            [sys.executable, str(Path(__file__).parent / "shoot.py"), BASE, str(SHOTS)],
            cwd=str(REPO),
        )
        if not has_memes:
            print("! 表情包那步被跳过（缺 Pillow）")
        return result.returncode
    finally:
        for process in (daemon, stub):
            if process is None:
                continue
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()


if __name__ == "__main__":
    sys.exit(main())
