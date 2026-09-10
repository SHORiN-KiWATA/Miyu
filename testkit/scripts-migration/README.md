# 十件工具迁成内置脚本的端到端检查(09-10 分层架构阶段 3)

`get_weather` `divine` `query_moegirl` `codec` `scientific_calculator` `game_compat`
`fcitx5_input_method_wiki_qurey` `online_man` `query_deepseek_status` `read_clipboard`
的 Rust 实现已删,功能由 `src/scripts/personas/default/` 下同名 Python 脚本提供。

```sh
BIN=target/release/miyu bash testkit/scripts-migration/e2e.sh      # 十件各调一次
BIN=target/release/miyu bash testkit/scripts-migration/one.sh divine '{"method":"tarot"}'
```

- 隔离 `MIYU_HOME`,`MIYU_SYSTEM_SCRIPTS_DIR` 指向仓库 `src/scripts`,不吃已安装的
  `/usr/share/miyu/scripts`。
- 走 `miyu tool-call <名字> <JSON>`(工具桥;daemon 不在就本地装配注册表执行)。
  **别用 `miyu tool …`**:那不是子命令,整串会被当成一句聊天发给模型(09-10 踩过,
  沙箱默认配置里 claude-code 订阅线可用,白跑了六轮);直调的隐藏名是 `__tool`。
- 桥的返回体把脚本 stdout 包在 JSON 字符串里,断言子串别带引号。
- 联网的七件只看能否调通、无 `ok: false`;离线三件(codec / calculator / divine)看值。
- 09-10 实测:10/10 PASS,`tool-call --list` 列出全部十个名字。
