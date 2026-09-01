---
name: linux-game-compatibility
description: 调查某款游戏在 Linux 下的兼容性（Proton/反作弊/多人联机/性能），产出红绿灯结论与可执行的游玩步骤。Use when the user asks whether a game runs on Linux, Steam Deck, Proton, Wine, Lutris, Heroic, or anti-cheat support.
compatibility: Miyu built-in research workflow
---

# Linux 游戏兼容性调查

## 采集

先调用 `game_compat`，游戏名或 Steam AppID 都行。它会汇总三个来源并返回 Markdown：

- `protondb`——评级、近期趋势、历史最佳、报告总数，以及带日期的玩家评论
  （Proton 版本、启动参数、故障类型、原文）
- `areweanticheatyet`——反作弊状态与厂商，附状态变更日期和近期事件
- `caniplayonlinux`——一句话结论快照，附验证日期

评论条数用 `max_reports` 调（默认 8）。只想要结论时填 0；玩家报告分歧大、需要
判断具体故障时可调多。

**只在下列情况追加 `web_search` / `web_fetch`：**

- 三个来源互相冲突，需要第三方佐证
- 用户明确问性能、Mod、启动器、多人联机等工具没覆盖的细节
- 工具返回了误配警告，或该游戏根本不在 Steam
- 有迹象表明近期发生重大变更，工具数据可能过期

优先官方页面、PCGamingWiki、GamingOnLinux、Steam 社区、玩家社区。
不要为补全细节反复搜索；查不到就明说不确定，不要编造。

## 读工具返回值

几个容易读错的地方：

- **`native_linux`**：出现这个标记说明有原生 Linux 版，直接就能跑，不需要
  讨论 Proton。这是最强的绿灯信号。
- **`— no result`**：该来源没有这个游戏的记录，不是抓取失败（失败会显示
  `— error` 并附原因）。任何一个来源 `no result` 都不构成否定结论。
- **`areweanticheatyet — no result`** 尤其要小心：只说明 AWACY 没收录，
  **不等于该游戏没有反作弊**。AWACY 只记录带反作弊的游戏，纯单机通常不在库里，
  但冷门条目也可能只是漏收。不能把"没查到"说成"没有反作弊"——要判断有无反作弊，
  得看另两个来源的正文或另行搜索。
- **`trending` 与 `best`**：只在不同于当前评级时才出现，一出现就是重要信号。
  `silver · trending bronze · best platinum` 意味着这游戏正在变坏，多半是反作弊
  或引擎更新导致的，要结合 AWACY 的变更日期一起读。
- **`⚠ search ... may not be the same game`**：Steam 搜索没找对游戏，通常因为
  它不在 Steam 上。此时 protondb 那节的数据不可信，但另两节仍按你问的名字查，
  可以采信。
- **各节的错误行**：某个来源失败只代表这个来源获取数据失败，不能因此断言不可玩。
- **日期**：`caniplayonlinux` 的 `verified` 是快照日期，protondb 评论各带日期。
  两者结论不同时，时效性高的更可信。

## 判断规则

最终必须给出红绿灯结论：🟢 可玩 / 🟡 不一定能玩 / 🔴 不可玩。

按优先级：

1. 有原生 Linux 版 → 🟢。
2. AWACY `Denied` 或 `Broken`，且游戏以多人为主 → 🔴。
3. AWACY `Denied` 或 `Broken`，但游戏有可玩的单机内容 → 拆开说：单机与多人分别
   给结论，不要笼统下判断。
4. AWACY `Supported` 或 `Running`，且 ProtonDB 为 Platinum / Gold → 🟢。
   注意 `Running` 只表示社区层面目前能跑，不是官方承诺，报告里要写清。
5. ProtonDB `Borked` → 🔴；`Bronze` → 🟡 且说明主要故障。
6. ProtonDB `Silver` → 🟡，需要结合评论说明代价（要不要换 GE-Proton、加启动
   参数、性能损失多少）。
7. 报告总数很少（个位数）时评级不可靠，倾向 🟡 并说明样本量。
8. `trending` 明显低于当前评级 → 至少降到 🟡，并指出正在变坏。
9. 来源冲突、反作弊状态不明、近期变化多、玩家报告分裂 → 🟡。
10. 某个来源缺失 → 不构成任何否定结论。

其它必须守住的分寸：

- Steam Deck 的评级不等于桌面 Linux 没问题，也不等于 Valve Verified。
- 社区经验不是官方保证；"目前能玩"不是"永远稳定可玩"。
- 非 Steam 版本（Epic、国服客户端、官方启动器）与 Steam 版可能完全不同，
  要分开说。
- 用户问"怎么玩"时，必须给可执行路线，不能只回答能不能玩。

## 输出格式

只输出报告本身。不要输出内部思考、工具调用过程，不要写"以下是报告"这类
元话语，不要在开头加分隔线。

按以下结构，用中文：

```
## 调查结果

<第一行是红绿灯结论，例如「🟢 黑神话悟空 可玩」或
 「🟡 Apex Legends 单机可玩 / 多人被反作弊阻断」>

<1-3 句话说明总体判断>

## 依据

<关键证据。每条写明：来源、关键信息、支撑了什么判断、时效性。
 来源冲突时单独说明冲突点和你的取舍。>

## 怎么玩

<可执行路线。按实际情况取舍：Steam 安装方式、Proton/GE-Proton 版本选择、
 启动参数、是否需要 Heroic / Lutris / Flatpak、第一次启动的注意事项。
 原生 Linux 版直接说明装了就能跑。>

## 注意事项

<只写这个游戏真正适用的风险，不要凑数。无反作弊的单机游戏就不要写反作弊
 更新风险。可能适用的有：反作弊策略随时可变、官方未承诺 Linux 支持、
 账号/ToS 风险、Steam Deck 与桌面 Linux 的差异、非 Steam 版本差异、
 性能不确定性、数据时效性。>
```

只有拿到明确的 FPS、硬件、画质或对比数据时，才额外添加 `## 性能表现`。没有
数据就不要写这个章节，更不要编造帧数。

## 禁止事项

不要编造来源、Proton 版本、FPS、官方声明或封号案例。不要把社区经验说成官方
保证。不要把 Steam Deck 的评级说成 Valve Verified。不要因为某个来源查不到就
断言不可玩。不要把 AWACY 未收录说成没有反作弊。
