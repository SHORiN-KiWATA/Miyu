# 多用户(09-10 分层架构阶段 5)端到端

```sh
BIN=~/.cache/miyu-arch-fixes/target/release/miyu python3 testkit/multi-user/e2e.py      # 接口层,106 项(含内置账号建号/按人分会话库/可勾清单/脚本/成员 dashboard/表情包分库/工具桥按人格)
BIN=~/.cache/miyu-arch-fixes/target/release/miyu python3 testkit/multi-user/ui.py       # 浏览器:登录页/注册/成员控制台截图
BIN=~/.cache/miyu-arch-fixes/target/release/miyu python3 testkit/multi-user/layout_e2e.py  # 阶段 6:家目录布局 新装→回滚→再搬
```

- 三个脚本都固定 `MIYU_ADMIN_USER=admin`:管理员用户名 = 家目录名,不能随跑测试的系统用户名变。
- 09-11 起 daemon 不带口令:测具先用内置账号 miyu/miyu 登录、`POST /api/auth/setup-admin` 建号(`e2e.bootstrap_admin`),之后用账号登录;WebUI 测具(webui-timeline / webui-fixes)共用 `testkit/webui-fixes/authlib.py`。
- `typing_probe.py`:打字抖动取证(逐字敲、采滚动/几何/layout-shift,`SPAWN=1 TURN=1` 自起桩 daemon 并在流式中/结束后各敲一遍;`WEB=` 指向哪份前端就测哪份,A/B 用)。
- e2e.py 里的档案断言靠桩模型的 `STUB_DUMP_SYSTEM=<文件>`(每个请求的 system 消息一行 JSON):
  成员回合带成员档案、不带属主档案;管理员回合反之。

- 隔离 `MIYU_HOME` 与 `XDG_RUNTIME_DIR`(见 testkit/scripts-migration 的教训),桩模型复用
  `testkit/webui-fixes/stub_reasoning.py`(MODE=plain)。daemon 用 `__daemon --port --bind 127.0.0.1 --password-file`
  起,`-p` 不带值会进交互式提问。
- 请求要带 `Origin: http://127.0.0.1:<port>`,不然改动类接口被来源校验挡(403)。
- SSE 归属检查:两条流同时开着,各跑一轮,看对方会话 id 有没有漏进来;成员流里应有
  自己回合的 `assistant.delta` / `run.completed`。
- 用量断言读 `MIYU_HOME/state/usage-history.jsonl` 的 `acct` 列:成员回合是账号 id,管理员回合空。
- `cross_session_probe.py`:会话 A 在跑(桩 400 行慢流)时对 B 做 reset/改名/起回合/compact/pop/改模型/建删会话都该照常;桩的 long 模式见到用户消息里有 "short" 只出 3 行。
- `jump_probe.py`:对着任意跑着的 daemon(缺省沙盒 8388)发一句话,每 60ms 采样正文气泡位置/文本长度/行内 code 数与滚动位置,逐帧录像;`MOBILE=1` 换手机视口。09-10 用它坐实「字会跳」= 行内反引号/粗体闭合瞬间整行重排。
- 产物在 `~/.cache/miyu-multi-user/`(daemon.log、report.json、截图)。
