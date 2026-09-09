#!/usr/bin/env python3
"""重建面板的浏览器侧断言 + 截图。由 run.py 起好 daemon 后调用。

    python3 shoot.py <baseUrl> <outDir>

走的是用户那条路:往知识库面板里投一批文件(合成 DataTransfer,和 kb-drop 同一套
手法),前端整批传完自己 POST 一次重建——然后盯着重建卡:它必须当场变成「进行中 /
百分比」并画出进度条,而不是转瞬回到「空闲」;跑完「未索引」必须归零。

退出码非 0 = 有断言没过。
"""

import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18436"
OUT = Path(sys.argv[2] if len(sys.argv) > 2 else "/tmp/miyu-kb-reindex")
# 面板一次最多收 MAX_DROP_FILES(200)个,多投的会被前端就地忽略。
DROP_COUNT = int(sys.argv[3]) if len(sys.argv) > 3 else 200
OUT.mkdir(parents=True, exist_ok=True)

PANEL = '.con-panel[data-console-panel="kb"]'
problems = []


def check(name, ok, detail=""):
    print(f"{'  ok ' if ok else 'FAIL '}{name}{f' — {detail}' if detail else ''}")
    if not ok:
        problems.append(f"{name}{f' — {detail}' if detail else ''}")


def shot(page, name):
    time.sleep(0.3)
    page.screenshot(path=str(OUT / f"{name}.png"))
    print(f"  shot {name}.png")


# 合成一批文件投进面板。内容各不相同,免得被同名覆盖或被去重逻辑吃掉。
DROP = """
({ selector, count, stamp }) => {
  const dt = new DataTransfer();
  for (let i = 0; i < count; i += 1) {
    const body = `# batch ${stamp} page ${i}\\n\\n`
      + `这是第 ${i} 篇走查用的资料,内容各不相同,便于生成互不重复的语义块。\\n`
      + "pacman systemd wayland btrfs nvidia pipewire 内核参数写在 /etc/default/grub 里。".repeat(70);
    dt.items.add(new File([body], `batch-${stamp}-${i}.md`, { type: "text/markdown" }));
  }
  const target = document.querySelector(selector);
  for (const kind of ["dragenter", "dragover", "drop"]) {
    target.dispatchEvent(new DragEvent(kind, { bubbles: true, cancelable: true, dataTransfer: dt }));
  }
  return dt.items.length;
}
"""

# 重建卡是最后一张 stat card。
CARD = """
() => {
  const cards = Array.from(document.querySelectorAll('#dashKbRoot .dash-card'));
  const card = cards[cards.length - 1];
  const pick = (label) => {
    const hit = cards.find((c) => c.querySelector('.dash-card-label')?.textContent === label);
    return hit ? hit.querySelector('.dash-card-value')?.textContent : '';
  };
  return {
    value: card?.querySelector('.dash-card-value')?.textContent || '',
    hint: card?.querySelector('.dash-card-hint')?.textContent || '',
    bar: card?.querySelector('.dash-card-progress > i')?.style.width || '',
    pending: pick('待重建'),
    chunks: pick('语义块'),
  };
}
"""


def main():
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        page = browser.new_page(viewport={"width": 1440, "height": 950})
        page.goto(BASE, wait_until="networkidle")
        page.click("#sidebarSettingsButton")
        page.wait_for_selector('[data-console-panel="settings"]:not([hidden])')
        page.click('.con-rail-item[data-console-panel="kb"]')
        page.wait_for_selector("#dashKbRoot .dash-split")
        page.wait_for_selector("#dashKbRoot .dash-card")
        before = page.evaluate(CARD)
        print(f"  投放前:{before}")
        shot(page, "01-idle")

        stamp = str(int(time.time()))
        page.evaluate(DROP, {"selector": PANEL, "count": DROP_COUNT, "stamp": stamp})
        # 整批传完前端才 POST 重建;等上传记录的标题回到「上传记录(N)」。
        try:
            # 结束态的标题是「上传记录(N)」,进行中是「上传记录(i/N)」——认没有斜杠
            # 的那一种,别去猜 N:超过上限的部分前端会自己扔掉。
            page.wait_for_function(
                "() => /^上传记录\\(\\d+\\)$/.test("
                "document.querySelector('#dashKbRoot .dash-upload-head strong')?.textContent || '')",
                timeout=240000)
        except Exception as error:
            check("整批上传完成", False, str(error).splitlines()[0])
        shot(page, "02-uploaded")

        # 关键一帧:传完之后重建卡不能是「空闲」——这一帧正是用户实拍里出错的那一帧。
        # 头几百毫秒是「正在启动」(子进程还没报出总数),所以先只判「不是空闲」。
        running = None
        deadline = time.time() + 25
        while time.time() < deadline:
            card = page.evaluate(CARD)
            if card["value"] not in ("空闲", ""):
                running = card
                break
            time.sleep(0.3)
        check("重建卡当场进入进行中", running is not None,
              "" if running else f"一直是空闲:{page.evaluate(CARD)}")
        if running:
            print(f"  第一帧:{running}")
            shot(page, "03-running")

        # 拿到总数之后才该有百分比与进度条。
        counted = None
        deadline = time.time() + 120
        while running and time.time() < deadline:
            card = page.evaluate(CARD)
            if card["bar"]:
                counted = card
                break
            if card["value"] == "空闲":
                break
            time.sleep(0.3)
        check("画出了进度条与百分比", counted is not None,
              "" if counted else f"没等到进度条:{page.evaluate(CARD)}")
        if counted:
            print(f"  进行中:{counted}")
            check("进度带上了 done/total", "个文件" in counted["hint"], counted["hint"])
            check("百分比是个百分比", counted["value"].endswith("%"), counted["value"])

        # 再抓一帧,证明数字确实在走。
        moved = False
        deadline = time.time() + 300
        while counted and time.time() < deadline:
            card = page.evaluate(CARD)
            if card["hint"] != counted["hint"]:
                moved = True
                print(f"  又一帧:{card}")
                shot(page, "04-progress")
                break
            if card["value"] == "空闲":
                break
            time.sleep(0.5)
        check("进度在往前走", moved or not counted)

        # 跑完:未索引归零。
        try:
            page.wait_for_function(
                "() => { const cards = Array.from(document.querySelectorAll('#dashKbRoot .dash-card'));"
                " const last = cards[cards.length - 1];"
                " return last && last.querySelector('.dash-card-value')?.textContent === '空闲'; }",
                timeout=600000)
        except Exception as error:
            check("重建跑完", False, str(error).splitlines()[0])
        time.sleep(1.5)
        after = page.evaluate(CARD)
        print(f"  跑完:{after}")
        shot(page, "05-done")
        check("未索引归零", after["pending"] == "0", f"待重建={after['pending']}")
        check("语义块涨了", int(after["chunks"] or 0) > int(before["chunks"] or 0),
              f"{before['chunks']} → {after['chunks']}")
        check("没有挂着失败提示", "失败" not in after["hint"], after["hint"])
        browser.close()

    for problem in problems:
        print(f"FAIL {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
