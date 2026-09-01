#!/usr/bin/env python3
# 显示名称：游戏破解状态查询
# 描述：通过游戏名或关键词搜索游戏破解状态，返回是否已破解、DRM保护、破解日期、发布日期、场景组等信息。优先搜索 crackrelease.com（收录较全），无结果时回退到 isitcracked.com 补充。
# Description: Search game crack status by title or keyword. Primary source: crackrelease.com (more comprehensive). Fallback: isitcracked.com Supabase API. Returns crack status, DRM, dates, scene group, etc.

import sys
import json
import urllib.request
import urllib.parse
import urllib.error
import re
import html as html_mod
import time

# --- isitcracked.com (Supabase) config ---
SUPABASE_URL = "https://lhvknkrfhehcclzlabsl.supabase.co"
SUPABASE_KEY = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6Imxodmtua3JmaGVoY2NsemxhYnNsIiwicm9sZSI6ImFub24iLCJpYXQiOjE3Nzg4NzM0MTksImV4cCI6MjA5NDQ0OTQxOX0.B7YOW_hpn2zHxR-sfHgiNgqidpfESwJpixLrh-MevE8"
SUPABASE_SELECT = "title,slug,status,crack_date,release_date,drm_protection,scene_group,secondary_protections"

HTTP_HEADERS = {
    "User-Agent": "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "Accept-Language": "en-US,en;q=0.5",
}

REQUEST_TIMEOUT = 15
DETAIL_DELAY = 0.3  # seconds between detail page requests

STATUS_ICONS = {
    "CRACKED": "✅ 已破解",
    "UNCRACKED": "❌ 未破解",
    "UNRELEASED": "⏳ 未发布",
    "cracked": "✅ 已破解",
    "uncracked": "❌ 未破解",
    "unreleased": "⏳ 未发布",
    "hypervisor": "🔒 Hypervisor保护",
}


def http_get(url, headers=None, timeout=REQUEST_TIMEOUT):
    h = dict(HTTP_HEADERS)
    if headers:
        h.update(headers)
    req = urllib.request.Request(url, headers=h)
    return urllib.request.urlopen(req, timeout=timeout)


# ============================================================
# Source 1: crackrelease.com (web scraping)
# ============================================================

def search_crackrelease(query, max_details=5):
    """Search crackrelease.com, return list of game dicts."""
    search_url = f"https://crackrelease.com/?s={urllib.parse.quote(query)}&post_type=post"
    resp = http_get(search_url)
    html_text = resp.read().decode("utf-8", errors="replace")

    # Extract Games-only links (filter out News articles)
    entries = re.findall(
        r'(Games|News)</a>.*?<a[^>]*href="(https://crackrelease\.com/[^"]+)"[^>]*>([^<]+)</a>',
        html_text, re.DOTALL
    )
    game_links = []
    seen = set()
    for cat, url, title in entries:
        if cat != "Games":
            continue
        if url in seen:
            continue
        seen.add(url)
        game_links.append((html_mod.unescape(title.strip()), url))

    if not game_links:
        return []

    # Fetch detail pages
    results = []
    for title, url in game_links[:max_details]:
        time.sleep(DETAIL_DELAY)
        try:
            info = parse_crackrelease_game_page(url)
            results.append(info)
        except Exception:
            # If detail page fails, still include basic info
            results.append({
                "title": title,
                "status": "UNKNOWN",
                "source": "crackrelease.com",
                "url": url,
                "drm": "N/A",
                "release_date": "N/A",
                "crack_date": "N/A",
                "scene_group": "N/A",
            })
    return results


def parse_crackrelease_game_page(url):
    """Parse a crackrelease.com game page, return game info dict."""
    resp = http_get(url)
    html_text = resp.read().decode("utf-8", errors="replace")

    # Status: <h2>CRACKED</h2> etc.
    status_m = re.search(r"<h2[^>]*>\s*(CRACKED|UNCRACKED|UNRELEASED)\s*</h2>", html_text, re.IGNORECASE)
    status = status_m.group(1).upper() if status_m else "UNKNOWN"

    after_m = re.search(r"(AFTER\s+\d+\s+DAYS?|BEFORE\s+\d+\s+DAYS?)", html_text, re.IGNORECASE)
    after_text = after_m.group(1) if after_m else ""

    # Parse label-value pairs
    info = {}
    label_map = {
        "GAME": "title",
        "RELEASE DATE": "release_date",
        "CRACK DATE": "crack_date",
        "DRM PROTECTION": "drm",
        "SCENE GROUP": "scene_group",
    }
    for label, key in label_map.items():
        pattern = (
            rf"{label}\s*</(?:div|h\d|span|p)>"
            rf"\s*<(?:div|span|p)[^>]*>\s*"
            rf"(?:<a[^>]*>([^<]+)</a>|([^<]+))\s*<"
        )
        m = re.search(pattern, html_text, re.IGNORECASE)
        if m:
            val = (m.group(1) or m.group(2) or "").strip()
            if val:
                info[key] = html_mod.unescape(val)

    return {
        "title": info.get("title", "未知"),
        "status": status,
        "status_note": after_text,
        "drm": info.get("drm", "N/A"),
        "release_date": info.get("release_date", "N/A"),
        "crack_date": info.get("crack_date", "N/A"),
        "scene_group": info.get("scene_group", "N/A"),
        "source": "crackrelease.com",
        "url": url,
    }


# ============================================================
# Source 2: isitcracked.com (Supabase REST API)
# ============================================================

def search_isitcracked(query, limit=10):
    """Search isitcracked.com via Supabase API, return list of game dicts."""
    encoded = urllib.parse.quote(f"*{query}*")
    url = (
        f"{SUPABASE_URL}/rest/v1/games"
        f"?select={SUPABASE_SELECT}"
        f"&or=(title.ilike.{encoded},scene_group.ilike.{encoded})"
        f"&order=title&limit={limit}"
    )

    resp = http_get(url, headers={
        "apikey": SUPABASE_KEY,
        "Authorization": f"Bearer {SUPABASE_KEY}",
        "Content-Type": "application/json",
    })
    data = json.loads(resp.read().decode("utf-8"))

    results = []
    for game in data:
        drm = game.get("drm_protection") or "未知"
        secondary = game.get("secondary_protections") or []
        if secondary:
            drm = f"{drm} + {', '.join(secondary)}"
        slug = game.get("slug", "")
        results.append({
            "title": game.get("title", "未知"),
            "status": game.get("status", "unknown").upper(),
            "status_note": "",
            "drm": drm,
            "release_date": game.get("release_date") or "未知",
            "crack_date": game.get("crack_date") or "无",
            "scene_group": game.get("scene_group") or "无",
            "source": "isitcracked.com",
            "url": f"https://isitcracked.com/game/{slug}" if slug else "",
        })
    return results


# ============================================================
# Output formatting
# ============================================================

def format_status(status, note):
    label = STATUS_ICONS.get(status, status)
    if note:
        label = f"{label} ({note.lower()})"
    return label


def print_results(query, games, source_tag):
    if not games:
        print(f'搜索 "{query}" — 来源: {source_tag} — 未找到结果')
        return

    print(f'搜索 "{query}" — 来源: {source_tag} — 找到 {len(games)} 个游戏\n')

    for i, g in enumerate(games, 1):
        status_str = format_status(g["status"], g.get("status_note", ""))
        print(f"{i}. {g['title']}")
        print(f"   状态: {status_str} | DRM: {g['drm']} | 发布: {g['release_date']} | 破解: {g['crack_date']} | 场景组: {g['scene_group']}")
        if g.get("url"):
            print(f"   链接: {g['url']}")
        print()


def dedup_by_title(games):
    """Remove duplicates by lowercased title."""
    seen = set()
    result = []
    for g in games:
        key = g["title"].lower().strip()
        if key not in seen:
            seen.add(key)
            result.append(g)
    return result


# ============================================================
# Main
# ============================================================

def main():
    raw = sys.stdin.read().strip()
    if not raw:
        print("错误：未收到输入。请通过 stdin 传入 JSON，包含 query 字段。", file=sys.stderr)
        sys.exit(1)

    try:
        args = json.loads(raw)
    except json.JSONDecodeError:
        print("错误：stdin 输入不是有效的 JSON。", file=sys.stderr)
        sys.exit(1)

    query = ""
    if isinstance(args, dict):
        query = args.get("query", "").strip()
    if not query:
        print("错误：缺少 query 参数。", file=sys.stderr)
        sys.exit(1)

    limit = 5
    if isinstance(args, dict) and args.get("limit"):
        try:
            limit = max(1, min(int(args["limit"]), 20))
        except (ValueError, TypeError):
            pass

    # --- Source 1: crackrelease.com ---
    cr_games = []
    try:
        cr_games = search_crackrelease(query, max_details=limit)
    except Exception as e:
        print(f"(crackrelease.com 不可用: {e})", file=sys.stderr)

    if cr_games:
        print_results(query, cr_games, "crackrelease.com")

    # --- Source 2: isitcracked.com (fallback or supplement) ---
    # Run fallback if crackrelease had no results, or always supplement
    if not cr_games:
        try:
            ic_games = search_isitcracked(query, limit=limit)
            if ic_games:
                print_results(query, ic_games, "isitcracked.com")
            else:
                print(f'搜索 "{query}" — 两个数据源均未找到结果。')
                print("建议：尝试用英文游戏名搜索，或更换关键词。")
        except Exception as e:
            print(f'搜索 "{query}" — crackrelease.com 无结果，isitcracked.com 也不可用: {e}', file=sys.stderr)
            sys.exit(1)
    else:
        # Supplement: also check isitcracked for games not in crackrelease results
        try:
            ic_games = search_isitcracked(query, limit=limit)
            if ic_games:
                # Deduplicate: only show isitcracked games not already in crackrelease
                cr_titles = {g["title"].lower().strip() for g in cr_games}
                extra = [g for g in ic_games if g["title"].lower().strip() not in cr_titles]
                if extra:
                    print(f'--- 补充结果 (isitcracked.com) ---\n')
                    print_results(query, extra, "isitcracked.com")
        except Exception:
            pass  # Supplement is best-effort, don't error out


if __name__ == "__main__":
    main()
