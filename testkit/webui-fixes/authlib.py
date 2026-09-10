"""WebUI 测具共用的登录小工具(09-11 起 WebUI 永远要登录)。

daemon 不带 -p 起来时,首次登录用内置口令 `miyu`,登录后创建管理员账号,之后内置
口令失效、只能用账号登录。这里把这套走一遍,给接口层一个带 cookie 的 opener,给
Playwright 页面一个「看到登录页就登进去」的帮手。
"""
import http.cookiejar
import json
import urllib.request

BUILTIN_PASSWORD = "miyu"
ADMIN_USERNAME = "admin"
ADMIN_PASSWORD = "miyu-test"

OPENER = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))


def _post(base, path, payload):
    req = urllib.request.Request(base + path, data=json.dumps(payload).encode(), method="POST",
                                 headers={"content-type": "application/json"})
    try:
        with OPENER.open(req, timeout=10) as resp:
            return resp.status
    except urllib.error.HTTPError as error:
        return error.code


def bootstrap(base, username=ADMIN_USERNAME, password=ADMIN_PASSWORD, builtin=BUILTIN_PASSWORD):
    """内置口令登录 → 建管理员 → 用账号重新登录。管理员已存在就直接账号登录。"""
    status = _post(base, "/api/auth/login", {"password": builtin})
    if status == 204:
        status = _post(base, "/api/auth/setup-admin", {"username": username, "display_name": "", "password": password})
        assert status == 204, f"setup-admin {status}"
    status = _post(base, "/api/auth/login", {"username": username, "password": password})
    assert status == 204, f"account login {status}"
    return OPENER


def ui_login(page, username=ADMIN_USERNAME, password=ADMIN_PASSWORD, timeout=4000):
    """页面停在登录页就登进去;已经登录(cookie 还在)就什么都不做。"""
    try:
        page.wait_for_selector("#loginForm:not([hidden])", timeout=timeout)
    except Exception:
        return False
    page.fill("#loginUsername", username)
    page.fill("#loginPassword", password)
    page.click("#loginSubmit")
    page.wait_for_function("() => !document.body.classList.contains('is-blocked')", timeout=20000)
    return True
