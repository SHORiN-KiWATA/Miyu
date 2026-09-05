#!/usr/bin/env python3
"""agy 中转「重启不全量重放」+「全量重放不越 192K」的真机验收(09-05)。

两个场景都用**单次命令**形态(`miyu -c '…'`,落在当前常驻会话,MIYU_DIRECT=1):
每条命令一个进程,进程之间正是 daemon 重启的等价物——续传映射若只在内存里,
第二条命令必然全量重放。

  A. 重启续传(真 agy,两次小调用):同一会话两条命令,断言第二条带
     --conversation、stdin 无 <conversation-history>。修复前必红。
  B. 预算收口(假 agy 堆历史,最后一轮真 agy):用假 agy 免费堆出 >192K 的
     历史,再换回真 agy 发一条带暗号的问题——映射指向假会话,真 agy 静默
     新开,Miyu 判出续传丢失后全量重放。断言 stdin ≤ 预算、带 omitted 标记、
     暗号在末尾;再读 agy 落盘的 transcript_full.jsonl,断言第 0 条输入没有
     `<truncated N bytes>`,且回复答出暗号。修复前 stdin 超线、转录里有截断标记。

用法:
    MIYU_HOME=/tmp/miyu-agy-guard/home python3 testkit/antigravity/replay_guard.py [A|B|AB]
    (该 home 是 /tmp/miyu-agy/home 的副本;别的会话可能正拿原件跑隔离 daemon)

前提:MIYU_HOME 下 config.jsonc 的 active_provider 是 antigravity 协议供应商,
本机 agy 已登录,该 home 没有 daemon 在跑(直连互斥)。
"""

import glob
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-agy-guard/home"))
CONFIG = HOME / "config" / "config.jsonc"
BRAIN = Path.home() / ".gemini" / "antigravity-cli" / "brain"
BUDGET = 172_800
FAKE = HOME / "fake-agy.sh"

FAKE_SCRIPT = r"""#!/usr/bin/env bash
# 假 agy:吞掉 stdin,回一段 5KB 正文。只认自己的会话 id:别家(真 agy)的
# --conversation 在这里不存在,照真 agy 的行为静默新开(init 报自己的 id)。
cat > /dev/null
sid="fake-conv-1"
agent=""
prev=""
for a in "$@"; do
  [ "$prev" = "--agent" ] && agent="$a"
  prev="$a"
done
body="$(for n in $(seq 1 120); do printf '第%d点：这一段回复只是占位，编号不同以免复读。' "$n"; done)"
echo "{\"event\":\"init\",\"conversation_id\":\"$sid\",\"init\":{\"model\":\"m\",\"cwd\":\"/\",\"agent\":\"$agent\",\"tools\":[]}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":0,\"state\":\"DONE\",\"step_type\":\"user_input\"}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":1,\"state\":\"DONE\",\"step_type\":\"agent_response\",\"text_delta\":\"$body\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"thinking_tokens\":0,\"cache_read_tokens\":0,\"total_tokens\":12}}}"
echo "{\"event\":\"result\",\"result\":{\"conversation_id\":\"$sid\",\"status\":\"SUCCESS\",\"response\":\"$body\",\"num_turns\":1,\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"thinking_tokens\":0,\"cache_read_tokens\":0,\"total_tokens\":12}}}"
"""


def env():
    e = dict(os.environ)
    e["MIYU_HOME"] = str(HOME)
    e["MIYU_DIRECT"] = "1"
    e["MIYU_LOG_REQUESTS"] = "1"
    e["MIYU_LOG"] = "info"
    e.setdefault("LANG", "zh_CN.UTF-8")
    return e


def run_turn(text: str, timeout: int = 400) -> str:
    proc = subprocess.run(
        [str(BIN), "--stdout", "-c", text],
        env=env(),
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if proc.returncode != 0:
        print(proc.stdout[-2000:], proc.stderr[-2000:])
        raise RuntimeError(f"turn failed rc={proc.returncode}")
    return proc.stdout


def records():
    out = []
    for f in sorted(glob.glob(str(HOME / "cache" / "logs" / "requests-*.jsonl"))):
        for line in open(f, encoding="utf-8"):
            try:
                v = json.loads(line)
            except json.JSONDecodeError:
                continue
            if v.get("kind") == "antigravity" and v.get("scope") == "chat":
                out.append(v)
    return out


def stdin_text_bytes(record) -> int:
    line = json.loads(record["body"]["stdin"].strip())
    return sum(len(b.get("text", "").encode()) for b in line["message"]["content"])


def set_binary(path):
    raw = CONFIG.read_text(encoding="utf-8")
    cfg = json.loads(raw)
    plugin = cfg.setdefault("plugins", {}).setdefault("antigravity", {})
    if path is None:
        plugin.pop("binary", None)
    else:
        plugin["binary"] = str(path)
    CONFIG.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")


def log_lines(pattern: str):
    hits = []
    for f in sorted(glob.glob(str(HOME / "cache" / "logs" / "miyu.*.log"))):
        for line in open(f, encoding="utf-8", errors="replace"):
            if re.search(pattern, line):
                hits.append(line.rstrip()[:220])
    return hits


def newest_transcript(after: float):
    best = None
    for f in glob.glob(str(BRAIN / "*" / ".system_generated" / "logs" / "transcript_full.jsonl")):
        mt = os.stat(f).st_mtime
        if mt >= after and (best is None or mt > best[0]):
            best = (mt, f)
    return best[1] if best else None


def scenario_a():
    print("== A. 重启续传(真 agy)")
    base = len(records())
    t0 = time.time()
    print("  turn1:", run_turn("只回复字母 C").strip()[:80])
    print("  turn2:", run_turn("只回复字母 D").strip()[:80])
    recs = records()[base:]
    assert len(recs) >= 2, f"应有两条请求记录,实得 {len(recs)}"
    a1, a2 = recs[0]["body"]["args"], recs[-1]["body"]["args"]
    print("  turn1 --conversation:", "--conversation" in a1)
    print("  turn2 --conversation:", "--conversation" in a2)
    print("  turn2 stdin bytes:", stdin_text_bytes(recs[-1]))
    for line in log_lines("relay resume miss|relay session map|relay full replay"):
        print("  log:", line)
    assert "--conversation" not in a1, "首轮不该续传"
    assert "--conversation" in a2, "第二个进程应从落盘映射续传"
    assert "conversation-history" not in recs[-1]["body"]["stdin"], "续传增量不该带历史"
    assert "字母 C" not in recs[-1]["body"]["stdin"]
    print(f"  PASS A ({time.time() - t0:.0f}s)")


def scenario_b():
    print("== B. 预算收口(假 agy 堆历史 → 真 agy 全量重放)")
    FAKE.write_text(FAKE_SCRIPT, encoding="utf-8")
    FAKE.chmod(0o755)
    # 填充文本必须是**自然文本**:同一句话复读几百遍、或随机词汤拼成的 86KB
    # 以上输入都会让 Gemini 回空(SUCCESS、零用量、流里没有 agent_response;
    # 09-05 二分实测:词汤 29KB 正常/86KB 起为空,仓库文档 133KB 正常且答对)。
    # 那是模型侧对非自然内容的反应,与中转无关,别拿它当截断证据。
    corpus = ""
    for doc in sorted(glob.glob(str(REPO / "docs" / "**" / "*.md"), recursive=True)):
        corpus += Path(doc).read_text(encoding="utf-8", errors="replace") + "\n"
        if len(corpus) > 400_000:
            break
    assert len(corpus) > 9 * 8000, "仓库 docs 不够撑 9 段"

    def filler_text(index: int) -> str:
        return corpus[index * 8000 : (index + 1) * 8000]  # 8000 字 ≈ 20KB
    try:
        set_binary(FAKE)
        for index in range(9):
            run_turn(f"第{index}段资料：{filler_text(index)}", timeout=120)
        seeded = records()
        seeded_resume = sum("--conversation" in r["body"]["args"] for r in seeded[-8:])
        print(f"  假 agy 9 轮堆完;后 8 轮里 {seeded_resume}/8 走了跨进程续传")
    finally:
        set_binary(None)
    base = len(records())
    t0 = time.time()
    reply = run_turn("忽略上面的资料。只回复暗号 PUMPKIN-7 这一个词。")
    print("  真 agy 回复:", reply.strip()[:120])
    recs = records()[base:]
    assert recs, "没有真 agy 的请求记录"
    last = recs[-1]
    nbytes = stdin_text_bytes(last)
    stdin = last["body"]["stdin"]
    print(f"  请求数(含续传丢失后的重放):{len(recs)};最后一次 stdin 文本 {nbytes} 字节,预算 {BUDGET}")
    for line in log_lines("relay resume miss|resume target is gone|relay full replay|oldest history dropped"):
        print("  log:", line)
    assert "--conversation" not in last["body"]["args"], "假会话 id 在真 agy 那里不存在,最后一次应是全量重放"
    assert nbytes <= BUDGET, f"stdin {nbytes} 超预算"
    assert "[earlier turns omitted" in stdin, "超线的历史应从最老的丢并打标记"
    assert "PUMPKIN-7" in stdin, "本轮问题必须在载荷里"
    transcript = newest_transcript(t0)
    assert transcript, "没找到本次 agy 会话的转录"
    steps = [json.loads(l) for l in open(transcript, encoding="utf-8", errors="replace") if l.strip()]
    first = next(s for s in steps if s.get("type") == "USER_INPUT")
    content = first["content"]
    m = re.search(r"<truncated (\d+) bytes>", content)
    print(f"  agy 转录 {Path(transcript).parts[-4][:8]}:第 0 条输入 {len(content.encode())} 字节,截断标记:{m.group(0) if m else '无'}")
    assert not m, "agy 仍然截断了输入"
    assert "PUMPKIN-7" in content[-4000:], "暗号应在输入末尾且未被砍"
    assert "PUMPKIN-7" in reply, "模型没答出本轮暗号,说明它看到的不是本轮问题"
    print(f"  PASS B ({time.time() - t0:.0f}s)")


def main():
    which = (sys.argv[1] if len(sys.argv) > 1 else "AB").upper()
    assert BIN.exists(), f"缺被测二进制 {BIN}"
    if "A" in which:
        scenario_a()
    if "B" in which:
        scenario_b()


if __name__ == "__main__":
    sys.exit(main())
