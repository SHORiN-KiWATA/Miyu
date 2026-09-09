// WebUI 的斜杠命令层。
//
// 单独一个文件而不是塞进 app.js：后者已经 9500 行，再往里长就没人找得到东西了。
// 这里自己管住命令目录、`/` 补全菜单、以及「这一行是不是命令」的判定；app.js
// 只留三个挂钩（启动时 load、输入时 onInput、提交时 tryRun）。
//
// 命令清单来自 `GET /api/commands`，服务端从 REPL 那张同一张表里按 `web` 标记
// 过滤。前端**不**维护第二份——两份清单迟早分叉（加一条命令忘了改另一边）。
window.MiyuCommands = (() => {
  "use strict";

  let catalog = [];
  let menu = null;
  let highlighted = 0;
  let onPick = null;

  // 与 Rust 侧 `split_repl_command` 同语义：按第一个空白切成 (名字, 参数)。
  function split(input) {
    const text = String(input ?? "");
    const at = text.search(/\s/);
    if (at < 0) return [text, ""];
    return [text.slice(0, at), text.slice(at + 1)];
  }

  // 只认**完整**命令名。不命中就不是命令，照常发给模型——与 REPL 同一语义
  // （`slash_commands::parse_repl_input`）。两个界面在这件事上分叉的话，同一
  // 句 `/home/x 这是什么` 在一边能发出去、在另一边被吞掉。
  function match(input) {
    const text = String(input ?? "").trim();
    if (!text.startsWith("/")) return null;
    const [name] = split(text);
    const lowered = name.toLowerCase();
    return catalog.find((spec) => spec.name === lowered) || null;
  }

  // 补全候选：前缀匹配，只在菜单里用。回车执行走 match()，不做前缀展开。
  function suggestions(input) {
    const text = String(input ?? "");
    if (!text.startsWith("/") || /\s/.test(text)) return [];
    const lowered = text.toLowerCase();
    return catalog.filter((spec) => spec.name.startsWith(lowered));
  }

  async function load(apiRequest) {
    try {
      const response = await apiRequest("/api/commands");
      const payload = await response.json();
      catalog = Array.isArray(payload?.commands) ? payload.commands : [];
      // /stop 是纯前端命令（取消当前 run 的 HTTP 调用就在本页），REPL 没有
      // 对应物（那边是 Ctrl+C），所以不进服务端那张表，在这里补进目录。
      catalog.push({ name: "/stop", arg_hint: "", help: "停止当前正在运行的回复" });
    } catch (_) {
      // 拿不到目录就退化成「没有命令」：所有 / 开头的输入照常发给模型。
      catalog = [];
    }
    return catalog.length;
  }

  function ensureMenu(anchor) {
    if (menu) return menu;
    menu = document.createElement("div");
    menu.className = "commandMenu";
    menu.hidden = true;
    menu.setAttribute("role", "listbox");
    anchor.appendChild(menu);
    return menu;
  }

  function hide() {
    if (menu) menu.hidden = true;
    highlighted = 0;
  }

  function visibleItems() {
    return menu && !menu.hidden ? Array.from(menu.children) : [];
  }

  function render(items) {
    if (!menu) return;
    if (!items.length) {
      hide();
      return;
    }
    highlighted = Math.min(highlighted, items.length - 1);
    menu.replaceChildren();
    items.forEach((spec, index) => {
      const row = document.createElement("button");
      row.type = "button";
      row.className = "commandMenuItem";
      row.dataset.name = spec.name;
      if (index === highlighted) row.classList.add("isActive");
      const label = document.createElement("span");
      label.className = "commandMenuName";
      label.textContent = spec.arg_hint ? `${spec.name} ${spec.arg_hint}` : spec.name;
      const help = document.createElement("span");
      help.className = "commandMenuHelp";
      help.textContent = spec.help || "";
      row.append(label, help);
      // mousedown 而不是 click：click 之前输入框会先失焦，菜单已经关掉了。
      row.addEventListener("mousedown", (event) => {
        event.preventDefault();
        if (onPick) onPick(spec.name);
        hide();
      });
      menu.appendChild(row);
    });
    menu.hidden = false;
  }

  // 输入变化时刷新菜单。`anchor` 是菜单挂靠的容器，`pick` 是选中后回填输入框。
  function onInput(value, anchor, pick) {
    onPick = pick;
    ensureMenu(anchor);
    render(suggestions(value));
  }

  // 菜单开着时接管上下键与 Tab/Enter。返回 true 表示这次按键已被吃掉。
  function handleKey(event) {
    const items = visibleItems();
    if (!items.length) return false;
    if (event.key === "Escape") {
      hide();
      return true;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      const step = event.key === "ArrowDown" ? 1 : -1;
      highlighted = (highlighted + step + items.length) % items.length;
      items.forEach((item, index) => item.classList.toggle("isActive", index === highlighted));
      return true;
    }
    // Tab 补全；Enter 只在菜单开着且候选唯一/已选中时补全，不直接执行——
    // 与 REPL 一致：补全后再按一次回车才执行，用户有机会反悔。
    if (event.key === "Tab" || event.key === "Enter") {
      const picked = items[highlighted];
      if (!picked) return false;
      if (onPick) onPick(picked.dataset.name);
      hide();
      return true;
    }
    return false;
  }

  // 执行一条命令。`ctx` 提供 { apiRequest, sessionId, mode, notify, confirm, clearView }。
  // 返回 true 表示已处理（调用方应当清空输入框、不再当消息发）。
  async function tryRun(input, ctx) {
    const spec = match(input);
    if (!spec) return false;
    const line = String(input).trim();
    const [, args] = split(line);
    // 先占位再执行：命令要等服务端干完活，这中间对话流里必须有个东西告诉
    // 用户「收到了，在跑」。原来是跑完才出现回执，看着就像回车丢了。
    const pending = note(line, "执行中…", "pending", ctx.anchorTurnId, ctx.sessionId);
    ctx.redraw();
    const done = (text, tone) => {
      pending.text = text;
      pending.tone = tone || "info";
      ctx.redraw();
      return true;
    };
    if (!spec.arg_hint && args.trim()) {
      return done(`${spec.name} 不接受参数`, "error");
    }
    // 与 REPL 同一条规矩：会重排上下文的命令不插在运行中的回合上
    // （/compact 折叠消息、/reset 清库、/pop 弹轮次，正在跑的回合手里那份
    // 消息数组会成悬空引用）。后端也会 409，这里提前拦下给出中文说法。
    const blockedWhileRunning = ["/compact", "/reset", "/pop"];
    if (blockedWhileRunning.includes(spec.name) && ctx.isRunning?.()) {
      return done("命令要在两轮之间执行，等这一轮跑完再试", "error");
    }
    try {
      if (spec.name === "/compact") {
        const response = await ctx.apiRequest("/api/conversation/compact", {
          method: "POST",
          body: JSON.stringify({ session_id: ctx.sessionId }),
        });
        // compact_now 返回 Option。它**不是**被水位线拦着——手动 /compact
        // 已经是 force（agent/mod.rs 跳过了「折叠够不够本」那道闸）。真正的
        // None 出口在 compact.rs 的 `cut == 0`：压缩是把**比保留尾巴更老**的
        // 回合折成摘要，整段对话都还在尾巴以内时，没有更老的可折。
        // 尾巴预算 = min(16384, 窗口/4)，1M 窗口下就是 16k。
        const compacted = (await response.json())?.result?.compacted === true;
        return done(compacted ? "上下文已压缩" : "当前上下文过少");
      }
      if (spec.name === "/reset") {
        await ctx.apiRequest("/api/conversation/reset", {
          method: "POST",
          body: JSON.stringify({ session_id: ctx.sessionId }),
        });
        // 回执连同对话一起作废：它们锚在已经被删掉的回合上，留着就是一堆
        // 无主的旧提示，而且每次重画都会原样再来一遍——清空之后满屏「本会话
        // 已有未完成的目标」正是这么来的。占位回执也一起撤：清空后的空白本身
        // 就是最清楚的反馈，再挂一条「已清空」和一个 /reset 的回显反而是垃圾。
        clearNotices(ctx.sessionId);
        await ctx.reload();
        ctx.toast?.("已清空当前会话");
        return true;
      }
      if (spec.name === "/goal") {
        const response = await ctx.apiRequest("/api/goal", {
          method: "POST",
          body: JSON.stringify({ session_id: ctx.sessionId, input: args }),
        });
        // 服务端把整段人类可读的文本拼好（和 REPL 同一份实现），这里原样贴出，
        // 不在前端二次拼装——两边分叉的话，同一个 /goal 在两个界面说法不一样。
        const text = (await response.json())?.text || "";
        ctx.reloadGoal?.();
        // 服务端把「拒绝」也当成 HTTP 200 + 一段说明文字，只能按开头判断
        // 是不是出错——与 app.js 的 runGoalAction 同一套启发式。
        return done(text, /^(用法|\/goal |本会话)/.test(text) ? "error" : undefined);
      }
      if (spec.name === "/reset-memory") {
        // session_id 必须带上：WebUI 同时开着好几个会话，daemon 的全局指针
        // 未必是发命令的这一个，不带就会清到别的会话头上。
        const response = await ctx.apiRequest("/api/memory/reset", {
          method: "POST",
          body: JSON.stringify({ mode: ctx.mode, session_id: ctx.sessionId }),
        });
        // 回执文案由服务端拼（和 REPL/QQ 同一份实现），前端原样贴出。
        const text = (await response.json())?.text || "已清空本次会话记下的记忆";
        return done(text);
      }
      if (spec.name === "/reset-all-memory") {
        await ctx.apiRequest("/api/memory/reset-all", {
          method: "POST",
          body: JSON.stringify({ mode: ctx.mode }),
        });
        return done("已清空全部长期记忆");
      }
      if (spec.name === "/pop") {
        // /pop 全程不在对话流里留任何东西（回显和回执都不留）：
        // 无参数时选择器本身就是交互，按数量时结果走居中 toast。
        const dropEcho = () => {
          notices.splice(notices.indexOf(pending), 1);
          ctx.redraw();
        };
        const trimmed = args.trim();
        if (!trimmed) {
          dropEcho();
          ctx.openPopPicker?.();
          return true;
        }
        const count = Number.parseInt(trimmed, 10);
        if (!Number.isFinite(count) || count < 1) {
          dropEcho();
          ctx.toast?.("用法：/pop [数量]（不带数量打开多选列表）");
          return true;
        }
        let response;
        try {
          response = await ctx.apiRequest("/api/conversation/pop", {
            method: "POST",
            body: JSON.stringify({ session_id: ctx.sessionId, count }),
          });
        } catch (error) {
          dropEcho();
          // 409 = 没有可弹出的轮次（服务端文案是英文的，翻一下）。
          ctx.toast?.(
            error?.status === 409 ? "当前上下文没有可弹出的轮次" : error?.message || "弹出失败"
          );
          return true;
        }
        const removed = (await response.json())?.result?.turns || 0;
        dropEcho();
        await ctx.reload();
        ctx.toast?.(`已从上下文弹出最旧的 ${removed} 轮`);
        return true;
      }
      if (spec.name === "/stop") {
        // 和点停止按钮完全一致：不留命令回显、不留回执（按钮也不留）。
        // 只有「其实没有在跑」这种落空才用 toast 提一句。
        notices.splice(notices.indexOf(pending), 1);
        ctx.redraw();
        const outcome = await ctx.stopRun?.();
        if (!outcome) ctx.toast?.("当前没有正在运行的回复");
        return true;
      }
    } catch (error) {
      return done(error?.message || "命令执行失败", "error");
    }
    // 目录里有、这里却没实现：说明服务端开了 web 标记但前端没接上。
    // 当成命令吃掉并报错，比静默发给模型强——后者会让用户以为命令生效了。
    return done(`${spec.name} 在 WebUI 里还没有实现`, "error");
  }

  // ── 命令回执：像消息一样留在对话流里 ─────────────────────────────
  //
  // 命令不是回合，不能落库、更不能进模型上下文（它是客户端操作）。但只弹一个
  // 转瞬即逝的 toast 也不对：用户敲了字、按了回车，对话流里却什么都没发生，
  // 看起来就像没生效。REPL 那边是 `repl_note` 写进滚动区，这里对齐。
  //
  // 存成数组而不是直接往 DOM 里塞：`renderConversation` 每次都从 state.turns
  // 重建整个 timeline，直接塞的节点会被冲掉——而 /compact 成功后正好会触发
  // 一次重建。按会话记账、按会话渲染：切走再切回来回执还在原位（页面刷新
  // 才消失——它们本来就不落库）。
  const notices = [];

  // `anchorTurnId` = 敲这条命令时对话流里最后一个回合。渲染时插到那个回合
  // **之后**，而不是无脑 append 到末尾——否则之后来的新回合会把回执顶下去，
  // 看起来像是「先说话后执行的命令」，时间顺序全乱。
  function note(command, text, tone, anchorTurnId, sessionId) {
    const entry = {
      command,
      text,
      tone: tone || "info",
      anchorTurnId: anchorTurnId ? String(anchorTurnId) : "",
      sessionId: sessionId ? String(sessionId) : "",
    };
    notices.push(entry);
    return entry;
  }

  // 只清一个会话的回执（/reset 用）；不传就全清。
  function clearNotices(sessionId) {
    if (!sessionId) {
      notices.length = 0;
      return;
    }
    const scope = String(sessionId);
    for (let index = notices.length - 1; index >= 0; index -= 1) {
      if (notices[index].sessionId === scope) notices.splice(index, 1);
    }
  }

  // 非命令来源的系统回执：没有命令回显气泡，只有一条结果小条。
  function systemNote(text, anchorTurnId, sessionId) {
    return note("", String(text || ""), "info", anchorTurnId, sessionId);
  }

  function renderNotices(timeline, sessionId) {
    if (!timeline) return;
    const scope = String(sessionId || "");
    const visible = notices.filter((entry) => entry.sessionId === scope);
    if (!visible.length) return;
    // 锚点回合的最后一个 DOM 节点；找不到（回合被 undo/pop 掉了，或者当时
    // 对话是空的）就退回末尾。
    const anchorOf = (turnId) => {
      if (!turnId) return null;
      const nodes = timeline.querySelectorAll(`[data-turn-id="${CSS.escape(turnId)}"]`);
      return nodes.length ? nodes[nodes.length - 1] : null;
    };
    for (const entry of visible) {
      const parts = [];
      // systemNote 没有命令原文，只画结果小条。
      if (entry.command) {
        const echo = document.createElement("article");
        echo.className = "message user-message commandEcho";
        echo.dataset.role = "user";
        const bubble = document.createElement("div");
        bubble.className = "user-bubble";
        const line = document.createElement("p");
        line.textContent = entry.command;
        bubble.appendChild(line);
        echo.appendChild(bubble);
        parts.push(echo);
      }

      // 复用系统事件那套（后台任务完成用的就是它）：居中、带底色的小条，
      // 而不是一行裸文字——它不是谁说的话，是一次操作的回执。
      const reply = document.createElement("div");
      reply.className = "system-event is-command-result";
      if (entry.tone === "error") reply.classList.add("is-error");
      // 「执行中」要和结果长得不一样，否则用户分不清是跑完了还是还在跑。
      if (entry.tone === "pending") reply.classList.add("is-pending");
      const label = document.createElement("span");
      // 服务端回执按行组织（REPL 直接多行打印）；小条里换行显示不开，
      // 折成 `目标已设定 · <目标> · 进行中` 这样的一行。
      label.textContent = entry.text.replace(/\n+/g, " · ");
      reply.appendChild(label);
      parts.push(reply);

      const anchor = anchorOf(entry.anchorTurnId);
      if (anchor) {
        anchor.after(...parts);
      } else if (entry.anchorTurnId) {
        // 锚点回合被 undo/pop 掉了：退回末尾。
        timeline.append(...parts);
      } else {
        // 敲命令时对话还是空的：它是时间线上最早的事件，插在第一个回合
        // 之前（日期分隔条之后，分隔条属于那一天的回合）。append 的话，
        // 之后出现的每个回合都会把它一路顶到最后。多条无锚回执都插在同一
        // 参照点前，先后顺序自然保持。
        const firstTurn = timeline.querySelector("[data-turn-id]");
        if (firstTurn) firstTurn.before(...parts);
        else timeline.append(...parts);
      }
    }
    timeline.hidden = false;
  }

  return {
    load,
    split,
    match,
    suggestions,
    onInput,
    handleKey,
    hide,
    tryRun,
    renderNotices,
    clearNotices,
    systemNote,
  };
})();
