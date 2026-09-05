# 本地 embedding 模型资产

每个子目录是一个可被 Miyu 直接加载的本地嵌入模型，目录名即模型 id。目录里必须有：

| 文件 | 作用 |
|---|---|
| `manifest.json` | 模型元数据：维度、pooling（`cls`/`mean`）、是否归一化、最大 token 数、查询前缀、默认相似度下限、文件 sha256 |
| `model.onnx` | ONNX 图（`manifest.model_file` 可改名） |
| `tokenizer.json` | Hugging Face tokenizers 格式的分词器 |
| `LICENSE` | 上游许可证 |

运行时查找顺序（与 `assets/fonts` 一致）：`MIYU_EMBEDDING_MODELS_DIR` → `~/.miyu/models/<id>` →
源码树 `assets/models/<id>` → `/usr/share/miyu/models/<id>` → 可执行文件所在前缀的 `share/miyu/models/<id>`。
`embedding.local_model` 也可以直接写一个目录路径。

## 换模型 / 重新生成

模型目录**只增不改**：换模型就新建一个目录名，旧目录整个删掉。git 历史会永久保留旧 blob，所以进仓库前先跑检索质量回归。

`bge-small-zh-v1.5-int8` 由 [export-embedding-model.py](export-embedding-model.py) 从 BAAI 原始权重导出并做 int8 动态量化，manifest 里的 sha256 可以对上。自己导出别的模型也照这个脚本改。

不进仓库的东西：ONNX Runtime 动态库（按平台各一份、更新频繁），Arch 走 `onnxruntime-cpu` 包，其他发行版自行安装，或设置 `MIYU_ONNXRUNTIME_LIB` 指向 `libonnxruntime.so`。找不到运行库时语义检索静默退回关键词，`miyu embed status` 会说明原因。
