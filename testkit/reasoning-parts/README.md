# 终端「逐 delta 成段」复现(推理分段)

09-10 截图:终端里正文被拆成一行一段,连行内代码都被劈开。根因两层:

1. SSE 解析把 `reasoning_content: ""` 当成一段推理,每个正文 delta 都被包上一对
   ReasoningPartStart/End(`src/llm/openai_compatible/sse/mod.rs`);
2. 终端渲染收到分段开始就无条件截断正文行(`src/render/stream/reasoning_phase.rs`)。

## 跑法

```sh
# 隔离 home + 独立 daemon + PTY REPL + OpenAI 桩,pyte 渲染屏幕后数正文占了几行
MODE=empty      BIN=target/release/miyu python3 testkit/reasoning-parts/repro_openai.py
MODE=interleave BIN=target/release/miyu python3 testkit/reasoning-parts/repro_openai.py
MODE=seq        BIN=target/release/miyu python3 testkit/reasoning-parts/repro_openai.py
```

| MODE | 桩的流 | 修前 | 修后 |
|---|---|---|---|
| empty | 每个 content delta 附带 `reasoning_content:""` | 7 行(复现) | 1 行 |
| seq | 先 reasoning 后 content(DeepSeek 常规) | 1 行 | 1 行 |
| interleave | reasoning 与 content 逐 token 交替 | 5 行 | 5 行(真交错思考要给摘要行腾位置,按设计) |
| plain | 只有 content | 1 行 | 1 行 |

产物在 `~/.cache/miyu-wrap-repro-openai/<MODE>/`:`raw.bin`(PTY 原始字节)、`screen.txt`。
桩(`stub_reasoning.py`)另有 `long` / `job` 两种剧本,供 `testkit/webui-fixes` 用。

## `/models` 结果行位置(repl_models_probe.py)

```sh
BIN=target/release/miyu python3 testkit/reasoning-parts/repl_models_probe.py
```

敲 `/models stub-model` 与 `/models default`,断言两条结果行都独占一行、在输入框之上。
修前:「当前会话已恢复跟随全局」被画在输入框那一行上(`┃ 1u当前会话已恢复…`),
「当前会话模型: …」整个被活动区盖掉;修后两条都在正文区、顺序正确。

**PTY 夹具答光标位置要按真实光标答**(`Repl` 边收边喂 pyte,ESC[6n 回当前坐标):
固定答 `1;1` 会让「挂起活动区 → println → 按光标重新挂回」这类流程在测具里错位,
真终端里却是对的——旧的 repl-smoke/claude-code 夹具就是固定答 1;1。
