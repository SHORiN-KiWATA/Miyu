# 2026-09-05 · 是否内置本地 embedding 模型 —— 调研报告

> 目标：别人装上 Miyu 时没有 embedding，只有关键词检索；评估「内置一个本地嵌入模型」
> 在检索质量上值不值，以及资源上扛不扛得住。全部数据在本机（Ryzen 9 7940H, 16 线程,
> AVX-512）实测，语料是本机真实数据。基准工程与脚本在会话 scratchpad（`embed-bench/`、
> `candle-bench/`、`run_bench*.sh`、`run_quality.sh`、`gen_queries.py`、`distill.py`）。

## 0. 结论先行

1. **值得做，而且资源问题有解**。用 `bge-small-zh-v1.5` 的 int8 ONNX（24 MB）走 ONNX Runtime：
   查询 1–2 ms、索引 12 ms/条（单线程）、常驻 +60 MB、空闲卸载后 +24 MB。检索质量：
   表情包 hit@3 从 16% → 33%，记忆 hit@3 从 51% → 69%（关键词+语义融合）。
2. **「内置」应该是内置推理后端 + 首次启用时下载模型**，不是把模型塞进二进制：
   二进制只多 ~5 MB；ONNX Runtime 走动态加载（Arch `extra/onnxruntime-cpu` 现成，
   其他发行版由 Miyu 自动下载官方预编译库到 `~/.miyu/lib`）。静态链接 ORT 会让二进制
   +22 MB，且 `build.rs` 联网下载与 AUR `cargo build --locked` 离线构建冲突。
3. **三个坑必须避开**：批量推理（batch 32 → RSS 2.1 GB，逐条最快且只 129 MB）、
   ORT 线程数不锁（默认吃满 16 核）、向量继续存 JSON 文本（现状 136 MB，每次查询解析 2.1 s）。
4. 纯 Rust 路线（candle）慢 10 倍、静态嵌入（model2vec）在长文本记忆上崩盘、多语言
   模型体积超标——都不推荐做主方案。

## 1. 现状摸底（代码事实）

| 检索面 | 现状 | 语义检索 |
|---|---|---|
| 知识库 `kb search` | 关键词 + 文件名打分 (`knowledge_base/search.rs`) | **有**，需远程 `/embeddings`（`embedding.provider_id/model`），`semantic_chunks.embedding_json` 存 JSON 文本；关键词分弱时才混合（权重 0.6） |
| 被淘汰上下文 `search_evicted_context` | FTS5 | **有**，同一远程接口，按需嵌入（每次 64 条封顶、语料 500 条） |
| 记忆联想 `association()` | `score_text` 子串打分，全表扫 5000 行 (`memory/recall.rs`) | **没有**。wiki 08 写「Embedding 是可选增强」与代码不符 |
| 表情包 `search_meme` | 标签/名称/用法/描述子串 + 双字 bigram (`memes/library.rs:379`) | **没有** |

- 远程嵌入接口的实现是通用的（`knowledge_base::embed_text`），任何 OpenAI 兼容 `/embeddings` 都能接，Ollama/LM Studio 也行——但默认配置里为空，新用户等于没有。
- `kb embed reindex` 已经是**子进程**形态（`spawn_embedding_reindex` 起 `miyu kb embed reindex --quiet`），本地模型的索引负载天然不落在 daemon 上。

本机真实数据规模（默认人格）：facts 193 条（均 154 字）、episodes 904 条（均 185 字，p99 613 字）；表情包 83 张（说明文本均 117 字）；知识库 6507 文件 / 10548 语义块（均 337 字，bge-m3 1024 维）。

## 2. 候选方案资源实测

模型文件（Hugging Face，hf-mirror.com 可达）：

| 模型 | 文件 | 维度 | 备注 |
|---|---|---|---|
| bge-small-zh-v1.5 int8 ONNX | 23.9 MB + 0.4 MB tokenizer | 512 | MIT，中文专用 |
| bge-small-zh-v1.5 fp32 | 94.9 MB | 512 | 同上 |
| multilingual-e5-small int8 | 118 MB + 17 MB tokenizer | 384 | 多语言 |
| potion-multilingual-128M（静态） | 512 MB + 18.6 MB | 256 | model2vec |
| 自蒸馏静态模型（bge-small-zh → model2vec int8） | **5.4 MB** + 0.27 MB | 256 | 本次用 Python 蒸馏，18 s |
| bge-m3（你现在远程用的） | 2.27 GB | 1024 | 本地跑不现实 |

运行时（记忆语料 1101 条 / 19.8 万字；`RSS` 为进程常驻，`HWM` 为峰值）：

| 后端 | 线程 | 加载 | 加载后 RSS | 单条查询 p50 | 索引 ms/条 | 索引后 RSS |
|---|---|---|---|---|---|---|
| ORT bge int8, batch 32 | 16 | 0.5 s | 60 MB | 0.8 ms | 11 | **2164 MB** ⚠ |
| ORT bge int8, batch 8 | 1 | 0.9 s | 66 MB | 1.4 ms | 30 | 586 MB |
| ORT bge int8, batch 8, max_len 256 | 1 | 0.05 s | 66 MB | 1.4 ms | 20 | 300 MB |
| **ORT bge int8, batch 1** | **1** | 0.1 s | **68 MB** | **2.0 ms** | **12** | **129 MB** |
| ORT bge int8, batch 4, KB 语料 2000×512 字 | 2 | 0.05 s | 70 MB | 0.8 ms | 22 | 308 MB |
| ORT bge fp32, batch 32 | 16 | 0.7 s | 141 MB | 2.0 ms | 22 | 1839 MB |
| ORT e5-small int8 | 1 | 2.5 s | 638 MB | 2.7 ms | 83 | 1789 MB |
| candle bge fp32（纯 Rust） | 16 | 1.3 s | 107 MB | 13.9 ms | 84 | HWM 891 MB |
| candle bge fp32 | 1 | 2.7 s | 103 MB | 13.5 ms | 139 | HWM 866 MB |
| model2vec potion-multilingual | — | 11 s | **1064 MB** | 0.0 ms | 0.06 | 1072 MB |
| model2vec 自蒸馏 5.4 MB | — | 0.03 s | 39 MB | 0.0 ms | 0.05 | 45 MB |

要点：
- ORT 的内存池会把峰值一直攥着，**批量推理是 RSS 炸掉的根源**；逐条推理反而最快（padding 是浪费）。
- 卸载测试（用完 drop 模型 + `malloc_trim`）：ORT 91 → 33 MB，静态模型 39 → 13 MB。所以「空闲 N 分钟卸载」策略能把常驻压回 +24 MB。
- 二进制体积：ORT 静态链接 +22 MB（基准工程 28.3 MB vs 动态加载 5.9 MB）；candle 版 5.8 MB。动态加载/纯 Rust 两条路对 Miyu 二进制都只是 +4~5 MB。
- 现有 KB 语义索引：10548 条 JSON 文本 136 MB，每次查询读+解析 2.1 s；同样数据存 f32 BLOB 只要 43 MB（512 维模型 21 MB），全量点积毫秒级。

## 3. 检索质量实测

方法：从真实表情库/记忆库随机抽样，用 deepseek-v4-flash 为每条写 2 句「换一种说法、不照抄原词」的自然查询，看能否把原条目找回来。表情包 70 条查询 / 83 张；记忆 96 条查询 / 1101 条。`hybrid` = 关键词排名与语义排名做 RRF 融合（k=60）。

表情包：

| 方式 | hit@1 | hit@3 | MRR |
|---|---|---|---|
| 现状：关键词 | 11% | 16% | 0.149 |
| bge-small-zh int8 | 21% | 30% | 0.283 |
| **关键词 + bge 融合** | 21% | **33%** | **0.303** |
| e5-small int8 | 13% | 23% | 0.194 |
| 自蒸馏静态模型 | 17% | 26% | 0.236 |
| 关键词 + 静态模型融合 | 24% | 31% | 0.296 |
| potion-multilingual | 16% | 31% | 0.259 |

记忆（facts + episodes）：

| 方式 | hit@1 | hit@3 | MRR |
|---|---|---|---|
| 现状：关键词 | 31% | 51% | 0.442 |
| bge-small-zh int8 | 35% | 61% | 0.486 |
| **关键词 + bge 融合** | **40%** | **69%** | **0.563** |
| e5-small int8 | 34% | 58% | 0.489 |
| 关键词 + e5 融合 | 41% | 71% | 0.566 |
| 自蒸馏静态模型 | 11% | 20% | 0.173 |
| 关键词 + 静态模型融合 | 18% | **40%** ⚠ 比纯关键词还差 | 0.336 |
| potion-multilingual | 20% | 39% | 0.319 |

要点：
- 表情包绝对值低是因为库里同情绪表情太多（一堆「惊讶/大笑」），前三没命中的多是同类另一张；相对现状翻倍这个趋势很稳。
- 静态嵌入（model2vec）在短标签文本上勉强够用，在 185 字均长的日记上彻底失效，而且**融合一个烂 ranker 会拖垮关键词**——只能融合达标的模型。
- bge 的查询指令前缀（「为这个句子生成表示…」）在两组里都没有稳定收益，可不加。
- e5 与 bge 质量相当，但 118 MB、慢 7 倍、常驻 638 MB，中文场景没理由选它。

## 4. 推荐方案

**方案 A（推荐）：内置本地推理后端，模型按需下载。**

- 依赖：`ort`（`load-dynamic`）+ `tokenizers`（或 fastembed 薄封装），二进制 +~5 MB，`--locked` 离线可构建。
- 运行库：Arch 加 `optdepends=('onnxruntime-cpu: 本地语义检索')`；找不到系统库时由 Miyu 下载 ONNX Runtime 官方预编译包到 `~/.miyu/lib`（`ORT_DYLIB_PATH`）。
- 模型：`bge-small-zh-v1.5` int8 + tokenizer，约 24 MB，首次启用时下载到 `~/.miyu/models/`，来源顺序 hf-mirror → huggingface → GitHub Release 资产（校验 sha256）。
- 运行策略：单线程逐条推理、max_length 512、**空闲 10 分钟卸载**（与 renderer worker 同款）；索引走现有 `kb embed reindex` 子进程形态，记忆/表情包的索引也复用它。
- 存储：向量一律 f32 BLOB + `(model, dims, content_sha256)` 键，替换 `embedding_json`；记忆库新增 `memory_embeddings` 表按内容哈希缓存（904 条一次性 11 s）。
- 融合：RRF 取代现在的固定权重（`SEMANTIC_SCORE_WEIGHT=30`、`0.6` 是给远程 bge-m3 调的），远程模型与本地模型共用同一融合逻辑。
- 配置形态：`embedding.backend = "remote" | "local"`；`local` 时 `provider_id/model` 无意义，`min_score` 按模型另设默认（bge-small 余弦分布与 bge-m3 不同，0.25/0.35 不能直接沿用，落地时要用真实语料标定）。
- 接入面（按收益排序）：表情包 `search_meme` → 记忆联想 `association()`（每回合 +1–2 ms）→ KB / evicted（换后端即可）。

**不推荐**：静态链接 ORT（+22 MB、AUR 离线构建冲突）；candle（慢 10 倍，fp32 96 MB 模型，+100 MB 常驻）；potion-multilingual（1 GB 常驻）；把自蒸馏静态模型当唯一方案（记忆组崩）。

**方案 B（最小改动）**：不内置，只把「接 Ollama/LM Studio 本地嵌入」做成一键引导。零资源风险，但新用户仍然默认没有语义检索，与你的设计目标不符。

## 5. 待拍板

1. 形态：方案 A（内置后端 + 按需下载模型）还是方案 B（只做引导）。
2. 默认开关：首次启动就下载 24 MB 模型并默认开，还是默认关、设置页一键开。
3. 空闲卸载时长默认值（建议 10 分钟）。
4. 记忆联想是否接语义（每回合多一次 1–2 ms 嵌入 + 一次全表点积；联想块本来就是回合尾部追加的化石，不影响缓存）。
5. KB 语义索引改 BLOB 存储（需要迁移你现有 136 MB 的 bge-m3 索引，或直接重建）。

## 6. 复现

```
scratchpad/
  download.sh / download2.sh   # 拉模型文件
  export_corpus.py             # 导出真实记忆/表情包/KB 语料到 corpus/*.jsonl
  gen_queries.py               # deepseek-v4-flash 造改写查询（PROVIDER/MODEL/WORKERS 环境变量）
  distill.py                   # model2vec 蒸馏静态模型
  embed-bench/                 # Rust：bench / search / keyword / unload 子命令（fastembed + model2vec-rs）
  embed-bench-dyn/             # 同上，ort-load-dynamic 变体（量二进制体积）
  candle-bench/                # Rust：candle 纯 Rust 后端对照
  run_bench.sh run_bench2.sh run_bench3.sh run_quality.sh
```

## 7. 施工记录（09-05 下午，分支 `embedding-research`）

用户拍板：模型做成可换、内置一个优秀模型默认启用、可关、占用轻量；模型当资产进仓库；其他发行版由用户装运行库；embedding 不可用绝不能拖垮 Miyu 或检索。

落地形态：

- **资产**：`assets/models/bge-small-zh-v1.5-int8/`（int8 ONNX 23.9 MB + tokenizer + manifest.json + MIT LICENSE），照 `assets/fonts` 的查找链（`MIYU_EMBEDDING_MODELS_DIR` → `~/.miyu/models` → 源码树 → `/usr/share/miyu/models` → 可执行文件前缀）。`assets/models/export-embedding-model.py` 可复现导出。三份 PKGBUILD 装到 `/usr/share/miyu/models/`，`onnxruntime-cpu` 进 depends。
- **推理**：`src/embedding/`（`local.rs` ORT 会话单线程逐条、`worker.rs` 独立子进程 + 长度前缀帧 + 握手 + 空闲退出 + 失败冷却 5 分钟、`remote.rs` 原 `/embeddings`、`manifest.rs` 资产查找、`vectors.rs` BLOB/余弦/RRF）。`ort` 走 `load-dynamic`，显式 `init_from(路径)`，找不到库只是 `Err`。二进制 +~5 MB。
- **配置**：`embedding.{enabled=true, backend=auto|local|remote, local_model, idle_unload_seconds=600}`；`auto` = 配了远程就远程否则本地，老配置零迁移。`plugins.knowledge_base.embedding_enabled` 默认改为 true。设置页与 config TUI 同步。
- **接入**：表情包 `use_meme search` 关键词 + 语义 RRF（向量缓存 `cache/meme-embeddings/<lib>.db`）；记忆联想 `association_with_semantic()`（`memory_embeddings` 表，每轮补 32 条 + 后台补齐）；知识库/被淘汰上下文改用统一 Embedder，向量改 f32 BLOB 并按模型过滤；`miyu embed status|models|reindex`。
- **退化**：任何一环失败 → debug 日志 + 关键词结果；测试 `a_missing_runtime_degrades_to_keyword_results`、`disabled_semantic_matches_keyword_only_exactly` 锁死。

验收：`testkit/embedding/run.sh`（沙箱黑盒）+ `cargo test --lib`（真实推理用例在没装运行库的机器上自动跳过）+ `memory::tests::semantic::eval_real_corpus`（真实语料量尺）。
