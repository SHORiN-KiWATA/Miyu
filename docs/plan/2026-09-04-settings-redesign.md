# WebUI 设置页重做方案（2026-09-04）

## 一、现在的设置页为什么「离谱」

跑了一遍 `web/app.js` 里设置区（约 1100 行，`renderGeneralConfig` → `renderPlugins`）、后端 `src/config/*` 全部结构体和本机 `~/.miyu/config/config.jsonc`，问题分五类：

**1. 大片字段只能手写 JSON。**

| 位置 | 字段 | 现状 |
| --- | --- | --- |
| 供应商卡 | `model_context_window` / `model_costs` / `model_modalities` / `extra_body` | 四个 textarea 手填 JSON 对象 |
| 供应商卡 | `models` | 每行一个模型名手敲，没有「拉取模型列表」按钮（TUI 里有，`config_tui::providers::fetch_models`） |
| 供应商卡 | `model_temperature` / `model_tools_loading_mode` / `tool_result_media` | 表单根本没露出，只能去「高级」改整份 JSON |
| 全局 › MCP | `mcp.servers` | 整个数组手写 JSON |
| 插件 › 表情包 | `persona_libraries` | JSON |
| **QQ 平台** | `platforms.qq` 全部 | **WebUI 没有这一页**。白名单、管理员、限流、会话专属路由、六个 QQ 插件（真实上下文 120 项、回复处理、入群审批、定时消息、表情口袋、消息历史）全部只能在「高级」JSON 里改 |

**2. 插件页是按 JSON 类型自动生成的。** 标签是英文 key 直接大写（"Max Results"、"Source Mode"），没有中文说明，枚举字段（`source_mode`、`thinking_depth`、`provider_type`、`backend`、`permission_mode`…）是裸文本框，识图模型/嵌入模型没有级联下拉，只能手敲 provider id 和模型名。

**3. 全平铺。** 记忆 22 个数字框一屏排开；模型池五个复选框列表（文本/多模态/cheap/balanced/strong）同一个模型出现五次；供应商 25 张 `<details>` 卡叠在一起。

**4. 任何一次输入都整页重渲染**（`renderConfigEditors()` 把五个区全部 `replaceChildren`），焦点和滚动位置靠运气，加不了动画。

**5. 没有「点点就好」的自动化。** models.dev 目录（`src/models_cache`）已经能查每个模型的上下文窗口、输入模态、价格、思考档位，TUI 加模型时会自动写 `model_modalities`，WebUI 一样没用。

## 二、设计原则

1. **概览 → 详情 → 微调三层。** 每页只放卡片或行（概览）；点开进右侧抽屉编辑（详情）；细项用弹出菜单/弹出框（微调）。任何页面首屏不超过一屏。
2. **零手写 JSON。** 所有 JSON 字段换成结构化编辑器（键值表、标签芯片、列表编辑器、级联选择器）。「高级」页保留原样当逃生舱。
3. **按钮替代手敲。** 拉取模型列表；models.dev 一键补全模型元数据；模型/供应商一律下拉；QQ 号/群号用芯片列表。
4. **就地校验，局部刷新。** 每个字段绑定路径，改了只刷新自己和依赖它的区块，不整页重画。
5. **动效克制统一。** 复用 dashboard 动效层（`dash-fade-up` / `dash-slide-in` / `dash-fade-in`），加：抽屉滑入滑出、弹出菜单缩放淡入、开关/分段滑动、卡片 hover 上浮、保存栏在有改动时从底部升起、错误字段抖动一下。`prefers-reduced-motion` 全关。

## 三、信息架构（新导航）

```
界面        配色 / 字号 / 展开思考 / 展开工具         （现状保留，换成卡片语言）
人格        人格卡片墙 + 用户身份                      （卡片点开抽屉：内容 / 看板 / 预设问题 三个标签）
供应商      供应商卡片墙                                （卡片点开抽屉：连接 / 模型 / 高级 三个标签）
模型池      一张矩阵：行=模型，列=文本·多模态·cheap·balanced·strong
全局        工具 / Skills / 思考 / 上下文 / 记忆 / 通知  （settings-card 行；记忆的 18 个数字折进「高级参数」）
MCP         服务器列表 + 新增/编辑对话框
插件        插件卡片墙（卡上直接开关）                   （卡片点开抽屉：中文标签 + 说明 + 正确控件）
QQ 平台     连接 / 权限与白名单 / 限流 / 模型 / 会话专属 / QQ 插件  （新页）
高级        完整 JSON（保留）
```

### 3.1 通用组件（新建 `web/settings.js`，从 app.js 拆出）

| 组件 | 用途 | 来源 |
| --- | --- | --- |
| `drawer(title, tabs, footer)` | 右侧编辑抽屉，宽 520px，Esc/遮罩关闭，内有标签页 | 扩展 `MiyuDash.openDrawer` |
| `popover(anchor, content)` | 锚定弹出框，用于单模型微调、限流编辑 | 新写，参考 `.model-menu` |
| `menu(anchor, items)` | 下拉菜单（协议、枚举、模型选择） | 参考 `.model-menu` |
| `modelPicker(options)` | 级联「供应商 → 模型」选择器，支持多选和「继承」项 | 新写 |
| `idChips(path, kind)` | QQ 号 / 群号芯片列表，回车添加，×删除，粘贴多个自动拆 | 新写 |
| `kvTable(path)` | 键值表（env、extra_body、persona_libraries） | 新写 |
| `stringList(path)` | 字符串列表（args、关键词、command_deny、允许扩展名） | 新写 |
| `field(schema)` | 按 schema 出控件：toggle / number(带 min max step) / select / text / secret / duration | 重写现有 `textConfigField` 等 |
| 保存栏 | 底部浮条：改动数、「放弃」「保存」，有错时列出错误字段并可点击跳转 | 重写 `settings-footer` |

字段 schema 是一张 JS 表，每条 `{path, label, hint, kind, choices, min, max, step, depends}`，中文标签直接搬 `src/config_tui/*` 里现成的 `t("…", "中文")`。

### 3.2 供应商页

- **卡片墙**：每张卡显示名称、协议标记、模型数、是否在池中、密钥是否已配。右上「添加」弹菜单：OpenAI 兼容 / Anthropic / Claude Code / Antigravity / Codex（后三种走内置模板）。
- **抽屉 › 连接**：ID、显示名、Base URL、协议（菜单）、API Key（现有 secret 编辑器）、超时。
- **抽屉 › 模型**：
  - 顶部「拉取模型列表」按钮 → 新后端接口，把供应商 `/models`（或 CLI 目录）返回的模型列出来，勾选即加入 `models`。
  - 已选模型列表，每行：模型名、模态图标（文/图/音/视频/pdf）、上下文窗口、价格；行尾「⋯」弹出框微调：默认模型、上下文窗口、输入模态（多选芯片）、温度覆盖、工具加载模式（full/stub/继承）、价格（币种+输入+输出+缓存读）、工具结果带媒体（是/否/自动）。
  - 每行「从目录补全」按钮，用 models.dev 数据一键写入上下文窗口/模态/价格；拉取列表时对新模型自动做一遍。
- **抽屉 › 高级**：Temperature、Anthropic 最大 Token、额外请求体（键值表，值支持 JSON 字面量）。

### 3.3 模型池

一张矩阵，行是所有已配置模型（按供应商分组，可折叠），列是五个池，格子是开关芯片。多模态列对不支持图片的模型置灰并提示。每列表头显示已选数。

### 3.4 插件页

- 卡片墙，卡上有开关，分组：联网 / 视觉与生图 / 知识与记忆 / 系统工具 / CLI 中转（claude_code、antigravity、codex）。
- 抽屉按 schema 表渲染。关键改动：
  - 识图模型、视频模型、嵌入模型 → 级联选择器，只列有对应能力的模型。
  - `source_mode`、`thinking_depth`、`provider_type`、`backend`、`permission_mode`、`native_tools`、`miyu_tools`、`sandbox_mode` → 菜单。
  - Tavily/Firecrawl/Exa 等多密钥 → 密钥列表（保留现有 secret 语义）。
  - `persona_libraries` → 键值表（人格名下拉 → 库目录）。
  - 数字字段带单位和范围提示（秒 / MB / 字符）。

### 3.5 QQ 平台页（新）

按 TUI 的分组做六段 settings-card，每段可折叠：

| 段 | 内容 |
| --- | --- |
| 连接 | 启用、反向 WebSocket 端口、Token（secret）、资源基地址、单条回复最大字数 |
| 权限与白名单 | 管理员芯片、私聊白名单芯片、群白名单芯片、允许非白名单私聊/群、非管理员可用主机工具、好友申请需白名单、额外触发关键词 |
| 限流与并发 | 三组限流（弹出框编辑「N 条 / M 秒」）、并行运行数/队列数、会话内并行、群上下文溢出策略 |
| 模型 | 文本池 / 多模态池 / 非白名单池，三个级联多选，带「继承全局」项 |
| 会话专属配置 | 列表（群/私聊 + ID + 覆盖了什么），点开抽屉：人格覆盖（菜单）、文本/多模态池（继承平台/继承全局/自定义）、额外提示词、并发覆盖 |
| QQ 插件 | 六张卡：真实上下文（抽屉分七标签，照 TUI：基础/群成员/主动回复/引用艾特/违规/好感度/情绪/识人映射）、回复处理、入群审批（群条件列表）、定时消息（任务列表 + 表单）、表情口袋、消息历史 |

### 3.6 其它页

- **人格**：卡片墙显示头像和名字，当前人格打标；抽屉三标签。用户身份同样处理。
- **全局**：记忆的布尔项放前面，18 个数字折进「高级参数」disclosure；补上 `display.language`、`tools.subagent_concurrency`、`tools.default_timeout_secs`、`tools.command_deny`（列表）、`context.default_context_window`、`notifications.*`、`embedding.*`（级联选择器）。
- **MCP**：服务器列表行（名字、命令、状态开关），新增/编辑对话框：ID、显示名、命令、参数列表、环境变量键值表、超时。

## 四、后端改动（小）

| 接口 | 用途 |
| --- | --- |
| `POST /api/providers/models` | 入参供应商草稿（含未保存的 base_url / key），出参模型列表，每个附 models.dev 元数据（上下文窗口、输入模态、价格、思考档位）。复用 `config_tui::providers::fetch_models` 与 `cli_catalog`，改成 `pub(crate)`，`spawn_blocking` 包一层。密钥「留空保留现有值」时用当前配置里的 key。 |
| `GET /api/providers/catalog?provider=&model=` | 单模型目录查询，给「从目录补全」用（也可以并进上一个接口）。 |

`assets.rs` 的 `DASH_SCRIPTS` 表加一行 `settings.js`；`index.html` 设置区的静态 HTML 换成空挂载点。

## 五、分期

| 期 | 内容 | 估量 |
| --- | --- | --- |
| P0 | `settings.js` 骨架、通用组件、schema 引擎、局部刷新、动效层、保存栏 | JS ~900 行 / CSS ~500 行 |
| P1 | 供应商卡片墙 + 抽屉 + 拉取模型 + 目录补全 + 按模型微调；后端两个接口 | JS ~700 / Rust ~150 |
| P2 | 模型池矩阵 | JS ~200 |
| P3 | 插件卡片墙 + schema 中文化 + 级联选择器 | JS ~600 |
| P4 | QQ 平台页全部 | JS ~1100 |
| P5 | 人格卡片化、全局重排、MCP 编辑器 | JS ~450 |
| P6 | 收尾：高级页、错误跳转、reduced-motion、窄屏适配、删掉 app.js 旧代码 | — |

每期做完在隔离 daemon（`MIYU_HOME=testkit/...`）里真机点一遍，截图核对。

## 六、待拍板

1. 编辑容器：主用**右侧抽屉**（与 dashboard 一致），小编辑用弹出框；还是全部居中模态？（推荐抽屉）
2. 模型池改成**矩阵**，还是保留五个列表只做折叠？（推荐矩阵）
3. **QQ 平台页**这次一起做（工作量最大的一段，但也是现在唯一完全没有表单的区域）？（推荐做）
4. 拉取模型列表需要**新增后端接口并重建 daemon**，接受？（推荐接受，否则模型名仍要手敲）
5. 真实上下文 120 项：照 TUI 七组全部做成表单，还是只做常用项、其余折进「高级」？（推荐七组全做，标签已有现成中文）

## 七、落地记录（09-04 当天）

五个决策点全部按推荐项拍板，六期一次做完，未提交。

| 文件 | 改动 |
| --- | --- |
| `web/settings.js`（新，约 2500 行） | 设置页渲染层：抽屉/菜单/弹出框/对话框、schema 字段引擎、七个页面、供应商引用维护（从 app.js 搬出） |
| `web/settings-schema.js`（新，约 3150 行） | 全部字段的中文标签、说明、范围、枚举、默认值；真实上下文 94 项分八组 |
| `web/app.js` | 删掉旧渲染器约 1160 行；保留草稿/载入/保存/高级 JSON；`init(ctx)` 把 state 与回调交给 settings.js |
| `web/index.html` | 导航九项；七个空挂载点；加载两个新脚本 |
| `web/styles.css` | 追加约 330 行 `st-*` 样式与动效层 |
| `src/web/providers_api.rs`（新） | `POST /api/providers/models`：拉取供应商 `/models` 或 CLI 目录，附 models.dev 元数据 |
| `src/models_cache/lookup.rs` | `describe_models`：读磁盘全量目录（常驻缓存是裁剪过的） |
| `src/config_tui/{mod,providers,cli_catalog}.rs` | `fetch_models` / `builtin_cli_binary` 放开成 `pub(crate)` |
| `src/web/{mod,server,assets}.rs` | 模块、路由、静态表 |
| `testkit/settings-ui/` | 隔离 home + Playwright 走查脚本 `shoot.js`（29 张截图、真实拉取、保存回读） |

**真机验证**：隔离 daemon（`MIYU_HOME=testkit/settings-ui/home`，端口 18391，QQ 停用）跑 `node shoot.js`：九页全部渲染，抽屉/菜单/弹出框/对话框各开一遍，DeepSeek 真实拉取到 3 个模型并由目录补上 1M 上下文与价格，改「最大工具轮数」保存后 `/api/config` 回读一致、再改回。控制台零 JS 错误（仅 `/theme.css` 404 为原有 matugen 链接）。

**没做的**：界面页保持原样；「高级」JSON 页保留作逃生舱；QQ `platforms.commands` 命令权限表未做表单（仍在高级页）。

## 八、第二轮反馈（09-04 晚）

1. 「关侧边栏就刷新、动画重放」——根因是**关抽屉时整页 rerender**，卡片入场动画跟着重放。现在局部重画打 `is-settled` 不放动画，只有导航切页才放；抽屉内 tab 原地刷新同理。实测关抽屉后 `st-fade-up` 动画数为 0。
2. 「添加供应商」去掉模板菜单，直接建空白供应商开抽屉；关抽屉时没填 ID 的自动丢弃（否则整份保存会被校验拒绝）。
3. TUI 编辑模型表单：打开时从 models.dev 目录预填上下文窗口（未手填时）、价格货币的「目录价」选项直接显示目录单价；激活模型时 `auto_configure_model_tags` 除模态外也补上下文窗口。共用 `models_cache::describe_models`。
4. 模型池改成「池子装模型」：五张池卡各列成员（模型 + 供应商 + 移出），「添加模型」弹勾选清单；文本池未显式设置时显示当前供应商默认模型并标「默认模型」。矩阵删除。
