#!/usr/bin/env python3
"""脚本面板真机走查:开控制台 → 脚本面板 → 表格 / 抽屉 / 禁用 / 启用 / 补描述注册,
逐步截图并收集控制台错误与 4xx/5xx 响应。

用法:python3 shoot.py [baseUrl]   (默认 http://127.0.0.1:18402)
前提:隔离 home 的 daemon 已起(见 README 段),且脚本目录里有一个缺描述头的文件
      (quiet.sh)供「未注册 → 注册」流程使用。
"""
import json
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18402"
SHOTS = Path(__file__).resolve().parent / "shots"
SHOTS.mkdir(exist_ok=True)
errors = []


def shot(page, name):
    time.sleep(0.45)
    page.screenshot(path=str(SHOTS / f"{name}.png"))
    print("shot", name)


def overview(page):
    return page.evaluate("fetch('/api/dash/scripts/overview').then(r => r.json())")


with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    page = browser.new_page(viewport={"width": 1440, "height": 900})
    page.on("console", lambda m: errors.append(f"console: {m.text}") if m.type == "error" else None)
    page.on("pageerror", lambda e: errors.append(f"pageerror: {e}"))
    page.on("response", lambda r: errors.append(f"http {r.status}: {r.url}") if r.status >= 400 else None)
    page.goto(BASE, wait_until="networkidle")
    page.click("#sidebarSettingsButton")
    page.wait_for_selector('[data-console-panel="settings"]:not([hidden])')
    page.click('.con-rail-item[data-console-panel="scripts"]')
    page.wait_for_selector("#dashScriptsRoot .dash-table")
    shot(page, "01-scripts-table")

    before = overview(page)
    print("counts:", json.dumps(before["counts"], ensure_ascii=False))
    ids = [s["id"] for s in before["scripts"]]
    assert ids, "面板一个脚本都没有"

    # 抽屉:点第一行
    page.click("#dashScriptsRoot .dash-row:not(.is-head) >> nth=0")
    page.wait_for_selector(".dash-drawer")
    page.wait_for_selector(".dash-drawer .dash-code")
    shot(page, "02-drawer")
    drawer_title = page.text_content(".dash-drawer-head strong")
    print("drawer:", drawer_title)

    # 禁用(抽屉底部按钮 → 确认框)
    page.click(".dash-drawer-foot >> text=禁用")
    page.wait_for_selector("dialog.dash-confirm[open]")
    shot(page, "03-confirm-disable")
    page.click("dialog.dash-confirm[open] .dash-button.is-danger")
    page.wait_for_selector(".dash-drawer", state="detached")
    page.wait_for_selector("#dashScriptsRoot >> text=已禁用(")
    shot(page, "04-after-disable")
    after_disable = overview(page)
    assert after_disable["counts"]["disabled"] == before["counts"]["disabled"] + 1, after_disable["counts"]
    disabled_id = after_disable["disabled"][-1]["id"]
    print("disabled:", disabled_id)

    # 启用
    page.click("#dashScriptsRoot >> text=启用")
    page.wait_for_function(
        "fetch('/api/dash/scripts/overview').then(r => r.json()).then(o => o.counts.disabled === %d)" % before["counts"]["disabled"],
        timeout=10000,
    )
    shot(page, "05-after-enable")

    # 未注册 → 补描述注册
    if before["counts"]["unregistered"]:
        # 「未注册(1)」的小节标题也含“注册”两个字,得点行里的主按钮。
        page.click('#dashScriptsRoot .dash-row button.dash-button.is-primary:has-text("注册") >> nth=0')
        page.wait_for_selector(".dash-drawer textarea")
        page.fill(".dash-drawer textarea", "Quiet helper registered from the dashboard.")
        shot(page, "06-register-form")
        page.click(".dash-drawer-foot >> text=注册")
        page.wait_for_selector(".dash-drawer", state="detached")
        page.wait_for_function(
            "fetch('/api/dash/scripts/overview').then(r => r.json()).then(o => o.counts.unregistered === %d)" % (before["counts"]["unregistered"] - 1),
            timeout=10000,
        )
        shot(page, "07-after-register")
        final = overview(page)
        print("registered ids:", [s["id"] for s in final["scripts"]])

    # 过滤与搜索
    page.click('#dashScriptsRoot .con-segmented button[data-value="user"]')
    time.sleep(0.3)
    shot(page, "08-filter-user")
    page.fill("#dashScriptsRoot input.dash-search", "bangumi")
    time.sleep(0.5)
    shot(page, "09-search")
    page.fill("#dashScriptsRoot input.dash-search", "")
    page.click('#dashScriptsRoot .con-segmented button[data-value="all"]')

    # 人格筛选:切到自定义人格 alter(隔离 home 里 personas/alter/ 放了一个脚本),
    # 内置脚本按人格规则隐藏,只剩它自己那层。
    personas = page.evaluate("fetch('/api/dash/scripts/personas').then(r => r.json())")
    print("personas:", personas)
    if "alter" in personas.get("personas", []):
        page.select_option("#dashScriptsRoot select.dash-select", "alter")
        # 页面 CSP 禁 eval,不能用 wait_for_function 传表达式,轮询读文本。
        for _ in range(40):
            if "alter" in (page.text_content("#dashScriptsRoot .con-head small") or ""):
                break
            time.sleep(0.25)
        else:
            raise AssertionError("切换人格后头部状态没有更新")
        time.sleep(0.4)
        shot(page, "10-persona-alter")
        alter = page.evaluate("fetch('/api/dash/scripts/overview?persona=alter').then(r => r.json())")
        print("alter counts:", json.dumps(alter["counts"], ensure_ascii=False), [s["id"] for s in alter["scripts"]])
        assert alter["counts"]["builtin"] == 0, "自定义人格不该看到内置脚本"
        assert "alter_tool" in [s["id"] for s in alter["scripts"]]
        page.select_option("#dashScriptsRoot select.dash-select", personas["active"])
        time.sleep(0.4)

    browser.close()

print("errors:", len(errors))
for line in errors:
    print("  ", line)
sys.exit(1 if errors else 0)
