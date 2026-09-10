# 十件工具迁成内置脚本的端到端检查(09-10 分层架构阶段 3)

`get_weather` `divine` `query_moegirl` `codec` `scientific_calculator` `game_compat`
`fcitx5_input_method_wiki_qurey` `online_man` `query_deepseek_status` `read_clipboard`
的 Rust 实现已删,功能由 `src/scripts/personas/default/` 下同名 Python 脚本提供。

```sh
BIN=target/release/miyu bash testkit/scripts-migration/e2e.sh      # 十件各调一次
BIN=target/release/miyu bash testkit/scripts-migration/one.sh divine '{"method":"tarot"}'
```

- 隔离 `MIYU_HOME` 与 **`XDG_RUNTIME_DIR`**(工具桥先找 daemon 的 IPC socket;不隔离运行目录
  会连上本机真 daemon,列出并调用的是它那份工具面——09-10 第一轮 e2e 就是这样白过的),
  `MIYU_SYSTEM_SCRIPTS_DIR` 指向仓库 `src/scripts`,不吃已安装的 `/usr/share/miyu/scripts`。
- `manifest_probe.sh`:给默认人格写一份只留 files 的 persona.toml,脚本/记账/记忆工具应从
  工具面消失,删掉清单后恢复(09-10 实测 6→0→6 PASS)。
- 走 `miyu tool-call <名字> <JSON>`(工具桥;daemon 不在就本地装配注册表执行)。
  **别用 `miyu tool …`**:那不是子命令,整串会被当成一句聊天发给模型(09-10 踩过,
  沙箱默认配置里 claude-code 订阅线可用,白跑了六轮);直调的隐藏名是 `__tool`。
- 桥的返回体把脚本 stdout 包在 JSON 字符串里,断言子串别带引号。
- 联网的七件只看能否调通、无 `ok: false`;离线三件(codec / calculator / divine)看值。
- 09-10 实测:10/10 PASS,`tool-call --list` 列出全部十个名字。
