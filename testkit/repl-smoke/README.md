# REPL 走查

真终端里验两件事(沙箱 daemon + 桩模型,不花额度):

1. **上键历史带活占位符**:粘 4 行文本 → 输入框显示 `[粘贴 1: ~4 行]` → 回车发出
   → 按上键回忆,回来的仍是占位符而不是四行裸文本;
2. **footer 的每秒 token**:桩模型分块吐字(单块请求不计入速度),footer 应出现
   `NNN tok/s`。

## 跑法

```sh
cargo build
python3 testkit/repl-smoke/run.py
```

产物在 `~/.cache/miyu-repl-smoke/`:`raw.bin`(终端原始输出)、`report.json`、
`daemon.log`。

## 判定

`report.json` 里六项应全绿:

| 字段 | 期望 |
|---|---|
| `placeholder_on_paste` | true |
| `reply_seen` | true |
| `footer_speed` | 形如 `78 tok/s` |
| `placeholder_on_recall` | true |
| `raw_text_on_recall` | false(回忆到的是占位符,不是裸文本) |
| `repl_alive` | true |

## 两个坑

- 裸 `miyu` 在真终端里先弹模式选择再退出,走查要用 `miyu normal`。
- 回复刚打完时编辑器还在重绘,立刻按上键会被吞掉;脚本里先静置 1.5 秒。
