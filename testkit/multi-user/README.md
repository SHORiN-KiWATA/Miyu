# 多用户(09-10 分层架构阶段 5)端到端

```sh
BIN=~/.cache/miyu-arch-fixes/target/release/miyu python3 testkit/multi-user/e2e.py      # 接口层,60 项
BIN=~/.cache/miyu-arch-fixes/target/release/miyu python3 testkit/multi-user/ui.py       # 浏览器:登录页/注册/成员控制台截图
BIN=~/.cache/miyu-arch-fixes/target/release/miyu python3 testkit/multi-user/layout_e2e.py  # 阶段 6:家目录布局 新装→回滚→再搬
```

- 三个脚本都固定 `MIYU_ADMIN_USER=admin`:管理员用户名 = 家目录名,不能随跑测试的系统用户名变。
- e2e.py 里的档案断言靠桩模型的 `STUB_DUMP_SYSTEM=<文件>`(每个请求的 system 消息一行 JSON):
  成员回合带成员档案、不带属主档案;管理员回合反之。

- 隔离 `MIYU_HOME` 与 `XDG_RUNTIME_DIR`(见 testkit/scripts-migration 的教训),桩模型复用
  `testkit/webui-fixes/stub_reasoning.py`(MODE=plain)。daemon 用 `__daemon --port --bind 127.0.0.1 --password-file`
  起,`-p` 不带值会进交互式提问。
- 请求要带 `Origin: http://127.0.0.1:<port>`,不然改动类接口被来源校验挡(403)。
- SSE 归属检查:两条流同时开着,各跑一轮,看对方会话 id 有没有漏进来;成员流里应有
  自己回合的 `assistant.delta` / `run.completed`。
- 用量断言读 `MIYU_HOME/state/usage-history.jsonl` 的 `acct` 列:成员回合是账号 id,管理员回合空。
- 产物在 `~/.cache/miyu-multi-user/`(daemon.log、report.json、截图)。
