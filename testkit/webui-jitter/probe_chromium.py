#!/usr/bin/env python3
"""Chromium 对照:同一段采样脚本,Playwright 驱动。

    python3 probe_chromium.py <baseUrl> <outDir>
"""

import json
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = sys.argv[1]
OUT = Path(sys.argv[2])
OUT.mkdir(parents=True, exist_ok=True)
SAMPLER = (Path(__file__).parent / "sampler.js").read_text(encoding="utf-8")
SAMPLE_SECONDS = float(sys.argv[3]) if len(sys.argv) > 3 else 16.0


def main():
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        page = browser.new_page(viewport={"width": 1280, "height": 800})
        page.goto(BASE + "/")
        page.wait_for_function("() => !document.getElementById('composerInput').disabled", timeout=30000)
        page.evaluate(SAMPLER)
        page.fill("#composerInput", "JITTER 来一段很长的装机教程")
        page.evaluate("() => document.getElementById('composerForm').requestSubmit()")
        time.sleep(SAMPLE_SECONDS)
        samples = page.evaluate("() => { window.__jit.done = true; window.__jit.meta = [...document.querySelectorAll('.assistant-meta')].map(e => e.textContent); return window.__jit; }")
        (OUT / "chromium-samples.json").write_text(json.dumps(samples), encoding="utf-8")
        print(f"{len(samples)} samples written")
        browser.close()


if __name__ == "__main__":
    main()
