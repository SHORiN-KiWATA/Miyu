# dev 模式提示词瘦身（2026-09-09）

目标：**减少 dev 模式下模型看到的一切提示词**（system 段、工具目录、逐轮注入），让模型少受与编码无关的指令干扰。不是单纯省 token——凡是「dev 里没有对象」的文本都要退场。

基线提交：`1ffa72d9`，二进制 `target/release/miyu`。

## 一、实测基线（隔离 MIYU_HOME + 桩 LLM，抓完整请求体）

dev 一轮的固定开销（直连线，OpenAI 兼容协议）：

| 段 | 字符 | ≈tok |
|---|---|---|
| dev-prompt（一行，用户可编辑） | 46 | 12 |
| `<host-environment>` | 178 | 45 |
| `<associative-memory>` 前言 | 395 | 99 |
| 工具目录（15 件） | 14,970 | 3,743 |
| 合计 | 15,593 | **3,899** |

对照 normal：system 1,208 tok + 工具目录 15,592 tok（63 件）+ 40 条人格预设对话。
dev 侧的 mode reminder / style-lock / LaTeX / voice-protocol / persona-reminder / 预设对话 / 表情包提醒**已经全部关断**，工具轮的 tool 结果也无附加说明。**剩余空间 96% 在工具目录里。**

工具目录拆解：描述 1,160 tok / schema 2,238 tok / JSON 壳 344 tok。schema 无水分，可省的是**整件工具**。

claude-code 中转线（用户当前活跃模型 opus）：Miyu 用 `--system-prompt` 整体替换掉 CC 官方提示词，模型看到的是 CC 原生工具面 + MCP 桥挂进去的 **11 件 Miyu 工具 ≈2,890 tok**（`mcp-serve` tools/list 实测）。

## 二、五项改动

### P1 dev 不再注册 load_skill（直连 −255 / 桥 −388）

**依据（实测）**：埋三个探针技能后对比清单——

| 技能位置 | dev 清单里 |
|---|---|
| `skills/personas/dev/` | ❌ 不出现 |
| `skills/personas/default/` | ✅ 出现 |
| `skills/`（全局） | ✅ 出现 |

根因：`src/tools/mod.rs` 的 `build_tool_registry` 调 `register_skills(&mut registry, config, paths)` 传的是**未经 `dev_scoped()` 的 config**，于是 `skill_roots` 的 persona 根解析成默认人格。dev 看得见 Miyu 人格的技能（真机上是 `douyin-tiktok-dl`、`gpu-passthrough`），自己的目录反而没被扫。而 dev 又没有 `manage_skill`（authoring 仅 Normal），拿到 `load_skill` 也只能加载「怎么写 Miyu 技能」。

**做法**：`build_tool_registry` 里 `register_skills` 只对 `AgentMode::Normal` 调用。不修作用域——修好了 dev 也只是看见一个空目录。dev 要用技能，用户在 `config/dev-prompt.md` 自己写一行路径即可（那本来就是给用户编辑的文件），不进代码、不占每轮字节。

**连带**：`refresh_skills` 开头 `if !registry.contains("load_skill") { return Ok(false) }` 早退，dev 每轮顺带省掉一次技能目录指纹扫描。

### P2 load_tools 条件注册（直连 −168 / 桥 −160）

`dev_registry` 里 `load_tools` 常驻，注释理由是「会话中途从需加载模型切到完整模型时，历史里的 load_tools 调用记录必须仍然可执行」。full 档下模型看不到它就不会调用，所以这个理由只在**本会话真调用过**时成立。

**判据**：`stub 档` 或 `state.load_session_loaded_tools()` 非空 → 注册；否则不注册。

**落点**：判据要拿会话状态，`dev_registry` 拿不到 → 放在回合装配处（`Agent::prepare_for_turn`，两条形态——daemon 回合与 CLI 直连——共用入口，且持有 `self.state` 与 `self.mode`）。

### P3 search_evicted_context 条件注册（直连 −110 / 桥 −102）

**依据**：`remember_evicted_turns` 的唯一上游是 `archive_and_delete_visible_turns`，其调用者只有三处——`trim_visible_context`（`agent/history.rs:82`）、`miyu pop`（`cli/pop_cmds.rs:147`）、WebUI 删轮（`web/actor/mod.rs:346`）。而 `trim_visible_context` 只在 `on_overflow="pop"` 档跑（`agent/pruning.rs:279`）。用户配置是 `compact`，compact 走摘要不写 evicted 库 → **该工具在实际配置下永远查不到东西**，还会诱导模型空跑一次。

**缓存**：三条写入路径全是「删可见历史」，那一轮前缀必然断；工具面在同一轮内改（`turn_loop/mod.rs` 取 `definitions()` 之前有技能目录刷新的先例）→ 注册时机与断裂时机重合，零额外损失。

**判据**：本会话/人格的 `evicted_context.db` 有行 → 注册。需要给 `MemoryStore` 加一个便宜的 `has_evicted_context()`（存在性查询，不读内容）。

**范围**：本次只改 dev，normal 不动（见「待拍板」）。

### P4 BRIDGE_DUPLICATE_TOOLS 补 edit（桥 −148）

原以为代价是 diff 渲染，**实测推翻**：diff 卡片走 progress 侧信道（`tools/apply_patch.rs` 发 `ToolProgressEvent::Message("__patch_preview__…")`），而桥的 progress 只转发 `Image`/`Artifact`/`PrepareForExternalOutput`（`web/bridge_progress.rs`），`Message` 直接丢弃；工具结果回程还要过 `shape_remote_output` 的 `compact_line` 压成一行。**中转线上 `mcp__miyu__edit` 本来就没有 diff**，补进去重清单零功能损失。直连线的 `edit` 保留不动。

**落点**：`src/llm/openai_compatible/claude_code/mod.rs` 的 `BRIDGE_DUPLICATE_TOOLS`。codex/antigravity 是否同补见「待拍板」。

### P5 dev 联想只留事实 + preamble 精简（system −84，每轮块变小）

**实测污染**：d1 会话说了两句闲聊，换到新会话 d2 时请求里出现
`<associative-memory> 近期发生的事情：- [e2] 对方说：再看下第二个问题；我回：ok`
——编码会话被回灌跨会话闲聊记录，还配了 99 tok 前言专门解释它。

**做法**：dev 下 `association_episodes = 0`。底层已就位：`association_candidates` 分两路检索（`memory/recall.rs:141`）、`search_table` 对 `limit==0` 早退（`recall.rs:337`）、`append_association_section` 空段跳过（`association.rs:113`）、`finish_association` 两路都空返回 `None`。

**前提**：episodes 继续**写**，只是不参与自动联想——organizer 蒸馏事实要拿它当原料，`recall_memories scope=episode` 显式查也保留。

**preamble**：现在三句里两句在 dev 没有对象——「别把块里的人当成当前用户 / 别模仿记录的对话风格」冲着 episodes 说；「principal 归属」只在多主体群聊有对象（owner 会话是 `MemoryAccess::Privileged`，`format_association` 根本不会写 `principal=` 行）。dev 版只留第一句（≈15 tok）。落点 `agent/prompt.rs::with_memory_preamble`，加 mode 参数。

## 三、账

| 项 | 直连 | claude-code 桥 |
|---|---|---|
| P1 load_skill | −255 | −388 |
| P2 load_tools | −168 | −160 |
| P3 search_evicted_context | −110 | −102 |
| P4 edit 去重 | 0 | −148 |
| P5 preamble | −84 | −84 |
| **工具目录** | 3,743 → 3,210（−14%） | 2,890 → 2,092（−28%） |
| **一轮固定开销** | 3,899 → 3,282（−16%） | — |

P5 的每轮块缩减不计入上表（随会话增长）。

## 四、施工中的裁定变更（用户 09-09）

- **P5 作废，改成「dev 不带记忆」**：不是「联想只留事实」，而是整套退场——记忆工具、联想注入、自动日记、`<associative-memory>` 前言。落点收在 `dev_scoped()` 的 `memory.enabled = false`，`memory_config()` 是全链唯一判据，一处关全链停。
- **P3 随之作废**：`search_evicted_context` 本来就注册在 `memory::register` 里，跟着记忆一起走，不需要单独的情境判据。`Agent` 上的情境化工具机制保留，现在只服务 `load_tools`。
- **连带**：dev 的 `miyu pop` 不再把弹出的回合写进逐出库，也就找不回来了。清理入口不受影响——`reset_all` 不看 `enabled`，关掉之前写下的旧记忆照样清得掉（已加回归用例）。
- **未做**：normal 侧的 `search_evicted_context` 同样在 `compact` 档下查不到东西，本次不动；`codex`/`antigravity` 的桥去重清单是否同补 `edit` 待定（两条线各有一份常量，原生编辑工具的语义要另外核实）。

## 五、测试（AGENTS.md §5.1：先证明不修时报红）

- `dev_registry_has_no_load_skill`：dev 表不含 `load_skill`，normal 表仍含。
- `load_tools_absent_until_the_session_loaded_one`：full 档 + 无已加载记录 → 不含；有记录 → 含；stub 档恒含。
- `search_evicted_context_registers_only_after_an_eviction`：空 evicted 库 → 不含；写入一条后 → 含。
- `bridge_duplicate_tools_cover_edit`：常量含 `edit`（沿用现有 claude-code args 测试的形状）。
- `dev_association_excludes_episodes`：dev 作用域配置下 `association_episodes == 0`，且有 episode 命中时块里只出现事实段。
- `dev_memory_preamble_is_the_short_one`：dev 版 preamble 不含 principal / 风格两句，normal 版不变。

## 六、验收流程（跑给用户看）

1. `cargo fmt && cargo clippy --all-targets && cargo test`（AGENTS.md §5.5：涉及 agent/llm/registry/提示词 → `scripts/refactor-check.sh` 五道门禁）
2. token 量尺：`cargo test --lib token_diet_baseline -- --ignored --nocapture`
3. 隔离 home + 桩 LLM 复跑请求体审计，对照本文档第一张表（测具见下）
4. AGENTS.md §1.6：手测两轮真实请求，看 cache-usage jsonl 第二轮 `cache_read` 无异常下降
5. 面向用户的改动写进主检出的 `next-release-note.md`

测具：`testkit/dev-prompt-audit/`（隔离 home + 桩 LLM + 请求体逐段拆解 + mcp-serve 桥面探针），本次一并进仓库，以后每次改工具面能直接复跑对账。


---

## 七、施工结果（09-09，分支 `worktree-dev-diet-2026-09-09`）

### 落地清单

| 项 | 落点 |
|---|---|
| dev 不注册技能面 | `tools/mod.rs::build_tool_registry`（`register_skills` 只给 Normal） |
| dev 不带记忆 | `config/persona_paths.rs::dev_scoped`（`memory.enabled=false`）+ `tools/mod.rs::dev_registry` 删 `memory::register` |
| 关闭态不建库 | `agent/setup.rs::new_for_audience`（`init`/`identity` 按 `enabled` 跳过） |
| `load_tools` 情境注册 | `agent/setup.rs::apply_situational_tools`（`prepare_for_turn` 调用，`switch_mode` 同步原件） |
| 工具原件存取 | `tools/registry/mod.rs::shared` / `register_shared` |
| 桥去重补 `edit` | `llm/openai_compatible/claude_code/mod.rs::BRIDGE_DUPLICATE_TOOLS` |

### 实测复验（同一套隔离 home + 桩 LLM，改前 vs 改后）

| 指标 | 改前 | 改后 |
|---|---|---|
| dev system | 623c / 156 tok | **226c / 57 tok** |
| dev 工具目录 | 14,970c / 3,743 tok（15 件） | **11,583c / 2,896 tok（10 件）** |
| dev 一轮固定开销 | ≈3,899 tok | **≈2,953 tok（−24.3%）** |
| 新会话被回灌的闲聊日记 | 有（跨会话） | **没有** |
| claude-code 桥面 | 11 件 / 2,890 tok | **6 件 / 1,956 tok（−32%）** |
| normal 侧 | 63 件 / system 3,171c | **63 件 / system 3,171c（一字未动）** |

dev 剩下的 10 件：`task`、`ask_question`、`goal`、`todowrite`、`run_command`、`job`、`vision_analyze`、`edit`、`web_fetch`、`web_search`。
桥面剩下的 6 件：`task`、`ask_question`、`goal`、`job`、`vision_analyze`、`load_tools`。

`load_tools` 在桥面仍在：桥走 `build_tool_registry` + `attach_owner_turn_tools`，不过 `prepare_for_turn`，所以情境判据够不着它（160 tok）。两条路的工具面不会被模型同时看到（claude-code 线忽略请求里的 tools 数组），所以不构成「工具面换脸」风险；要收这 160 tok 得把判据下沉到 `attach_owner_turn_tools`，本次未做。

### 测试

新增/更新：
- `tools::tests::dev_registry_drops_skills_and_memory`（新）
- `tools::tests::dev_scoped_config_turns_memory_off`（新）
- `agent::tests::prompt::dev_load_tools_registers_only_after_the_session_used_it`（新）
- `agent::tests::prompt::dev_mode_uses_one_line_prompt_and_skips_persona_family`（加断言：system 不含前言）
- `llm::openai_compatible::tests::claude_code`（加断言：剔除表含 `edit`）
- `tools::tests::skill_authoring_tools_are_normal_mode_only` → `skill_tools_are_normal_mode_only`（dev 不再有 `load_skill`）
- `web::tests::commands::web_memory_reset_all_clears_the_mode_it_was_asked_for`（种子数据手工开开关：守「关掉功能后历史遗留仍清得掉」）

`cargo test --lib`：2081 passed / 0 failed。`refactor-check.sh` 五道门禁：格式、编译、测试（用例数 2084 ≥ 基线 1948）、模型面语言、文件规模全过；**总行数那条在 HEAD 上就已经红**（198,995 → 257,288，+29.3%，基线是远古值），与本次改动无关（本次 +214 行）。

### next-release-note 草稿（worktree 写不了主检出，合并后贴过去）

见本文件同目录的施工记录末尾，文案已写好，合并回 main 时追加进「重要更新」。
