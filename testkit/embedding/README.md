# 语义检索黑盒验收（09-05）

在 `MIYU_HOME` 沙箱里（不碰 `~/.miyu`、不连线上 daemon）跑一遍本地 embedding 链路：

1. `miyu embed models` / `miyu embed status`：内置模型可见、运行库找得到、真实推理探针 512 维；
2. 表情包检索：同一句改写查询分别在「语义关闭」与「默认（融合）」下跑 `use_meme search`，对比候选，并确认 `cache/meme-embeddings/*.db` 生成；
3. 知识库：加两篇小文档，`kb embed reindex` 后用不含原词的改写去 `kb search`，语义应把 `gpu.md` 找回；
4. `miyu embed reindex` 一次补齐记忆/表情/知识库；
5. 退化：运行库路径指向不存在的文件 → `status` 说明原因、表情检索仍有结果；`embedding.enabled=false` → `status` 报 disabled。

```sh
cargo build --release
testkit/embedding/run.sh                      # 默认 target/release/miyu
ORT_LIB=/path/to/libonnxruntime.so testkit/embedding/run.sh   # 系统没装 onnxruntime 时
```

判据：末行 `failed=0`；`out-meme-fused.txt` 的候选比 `out-meme-keyword.txt` 更贴合「吐槽」语义；`out-status.txt` 有 `probe: 512 dims`。

真实语料量尺（需要 `MIYU_EMBED_EVAL_DIR` 指向含 `memory.jsonl` + `memory-q.jsonl` 的目录）：

```sh
MIYU_EMBED_EVAL_DIR=/path/to/corpus cargo test --lib memory::tests::semantic::eval_real_corpus -- --ignored --nocapture
```

沙箱目录 `home/`、输出 `out-*.txt`、`kbsrc/` 都是一次性产物，已 gitignore。
