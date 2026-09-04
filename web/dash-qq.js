/*
 * 群聊面板(09-04):消息历史 / 发言统计 / 撤回记录。
 *
 * 作用域 = 账号 + 会话;消息用游标向前翻,关键词 ≥3 字走全文索引;统计复用
 * 用量页的条形图与热力样式;删除历史与重置上下文边界放底部危险区。
 * 数据来自 /api/dash/qq/*。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = {
    account: D.recall("qq.account"),
    conversation: D.recall("qq.conversation"),  // "kind:id"
    accounts: [],
    conversations: [],
    tab: "messages",
    q: "",
    filter: { sender: "", startDate: "", endDate: "", recalled: false, media: false },
    pages: [],          // 游标栈:每页的 next_cursor
    cursor: null,
    items: [],
    days: 30,
    loadSeq: 0
  };
  const ui = {};
  const KIND_LABEL = { group: "群", private: "私聊" };
  const MEDIA_LABEL = { image: "图片", sticker: "表情", file: "文件", audio: "语音", video: "视频", other: "其他" };
  const ts = (unix) => unix ? D.formatTime(unix * 1000) : "—";
  const conv = () => {
    const [kind, id] = (state.conversation || ":").split(":");
    return { kind: kind || "group", id: id || "" };
  };
  const scopeQuery = () => { const c = conv(); return new URLSearchParams({ account: state.account, kind: c.kind, id: c.id }); };

  /* ── 挂载 ─────────────────────────────────────────────── */
  function mount(root) {
    root.textContent = "";
    ui.stamp = D.el("small", { text: "" });
    ui.account = D.select([], state.account, (value) => { state.account = value; D.remember("qq.account", value); state.conversation = ""; loadConversations(); }, "bot 账号");
    ui.conversation = D.select([], state.conversation, (value) => { state.conversation = value; D.remember("qq.conversation", value); resetPaging(); loadTab(); }, "会话");
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "群聊" }),
      D.iconButton("refresh-cw", "刷新", () => reloadAll()),
      ui.stamp,
      D.el("span.dash-scope", null, D.el("span.dash-scope-label", { text: "账号" }), ui.account, D.el("span.dash-scope-label", { text: "会话" }), ui.conversation));
    ui.cards = D.el("div");
    ui.tabs = D.segmented([{ value: "messages", label: "消息" }, { value: "stats", label: "统计" }, { value: "recalls", label: "撤回" }], state.tab, (value) => { state.tab = value; resetPaging(); renderToolbar(); loadTab(); });
    ui.filters = D.el("div.dash-filters");
    ui.search = D.el("input.dash-search", { type: "search", placeholder: "搜索消息(3 字以上走全文索引)…", oninput: () => {
      clearTimeout(ui.searchTimer);
      ui.searchTimer = setTimeout(() => { state.q = ui.search.value.trim(); resetPaging(); loadTab(); }, 280);
    } });
    ui.searchBox = D.el("label.dash-search-box", null, D.icon("search"), ui.search);
    const toolbar = D.el("div.dash-toolbar", null, ui.tabs.el, ui.filters, ui.searchBox);
    ui.body = D.el("div");
    ui.pager = D.el("div");
    ui.danger = D.el("div");
    root.append(head, ui.cards, toolbar, ui.body, ui.pager, ui.danger);
    renderToolbar();
    reloadAll();
  }

  function resetPaging() { state.pages = []; state.cursor = null; }

  function renderToolbar() {
    ui.filters.textContent = "";
    ui.searchBox.hidden = state.tab !== "messages";
    const f = state.filter;
    if (state.tab === "messages") {
      const sender = D.el("input.dash-select.dash-tag-input", { type: "text", placeholder: "发送者 QQ", value: f.sender, oninput: () => { clearTimeout(ui.senderTimer); ui.senderTimer = setTimeout(() => { f.sender = sender.value.trim(); resetPaging(); loadTab(); }, 300); } });
      const start = D.el("input.dash-select", { type: "date", value: f.startDate, onchange: () => { f.startDate = start.value; resetPaging(); loadTab(); } });
      const end = D.el("input.dash-select", { type: "date", value: f.endDate, onchange: () => { f.endDate = end.value; resetPaging(); loadTab(); } });
      const recalled = D.el(`button.dash-chip.is-clickable${f.recalled ? ".is-active" : ""}`, { type: "button", text: "仅撤回", onclick: () => { f.recalled = !f.recalled; resetPaging(); renderToolbar(); loadTab(); } });
      const media = D.el(`button.dash-chip.is-clickable${f.media ? ".is-active" : ""}`, { type: "button", text: "含媒体", onclick: () => { f.media = !f.media; resetPaging(); renderToolbar(); loadTab(); } });
      ui.filters.append(sender, start, D.el("span.dash-filter-sep", { text: "→" }), end, recalled, media);
    } else if (state.tab === "stats") {
      ui.filters.append(D.segmented([{ value: 7, label: "7 天" }, { value: 30, label: "30 天" }, { value: 90, label: "90 天" }, { value: 0, label: "全部" }].map((o) => ({ value: String(o.value), label: o.label })), String(state.days), (value) => { state.days = Number(value); loadTab(); }).el);
    }
  }

  /* ── 作用域 ───────────────────────────────────────────── */
  async function reloadAll() {
    await loadAccounts();
    await loadConversations();
  }

  async function loadAccounts() {
    try {
      const payload = await D.api("/api/dash/qq/accounts");
      state.accounts = payload.accounts || [];
      if (!state.account || !state.accounts.includes(state.account)) state.account = payload.connected?.[0] || state.accounts[0] || "";
      ui.account.textContent = "";
      for (const account of state.accounts) ui.account.append(D.el("option", { value: account, text: `${account}${payload.connected?.includes(account) ? "(在线)" : ""}` }));
      ui.account.value = state.account;
    } catch (error) {
      ui.stamp.textContent = `账号列表加载失败:${error.message}`;
    }
  }

  async function loadConversations() {
    if (!state.account) { ui.cards.replaceChildren(D.el("p.dash-empty", { text: "还没有任何 QQ 消息记录。" })); return; }
    try {
      const payload = await D.api(`/api/dash/qq/conversations?account=${encodeURIComponent(state.account)}`);
      state.conversations = payload.conversations || [];
      ui.conversation.textContent = "";
      for (const item of state.conversations) {
        const label = `${KIND_LABEL[item.kind] || item.kind} ${item.name ? `${item.name}(${item.id})` : item.id} · ${item.messages}`;
        ui.conversation.append(D.el("option", { value: `${item.kind}:${item.id}`, text: label }));
      }
      const keys = state.conversations.map((c) => `${c.kind}:${c.id}`);
      if (!state.conversation || !keys.includes(state.conversation)) state.conversation = keys[0] || "";
      ui.conversation.value = state.conversation;
      const current = state.conversations.find((c) => `${c.kind}:${c.id}` === state.conversation);
      const mb = (payload.db_bytes / 1024 / 1024).toFixed(1);
      ui.cards.replaceChildren(D.statCards([
        { label: "会话", value: state.conversations.length, hint: `${state.conversations.filter((c) => c.kind === "group").length} 群 · ${state.conversations.filter((c) => c.kind === "private").length} 私聊` },
        { label: "本账号消息", value: payload.total_messages, hint: `库文件 ${mb} MB · 无自动清理` },
        { label: "当前会话", value: current ? current.messages : 0, hint: current ? `bot ${current.bot_messages} · 撤回 ${current.recalled}` : "—" },
        { label: "时间跨度", value: current ? `${Math.max(1, Math.round((current.last_at - current.first_at) / 86400))} 天` : "—", hint: current ? `${ts(current.first_at).slice(0, 10)} → ${ts(current.last_at).slice(0, 10)}` : "" }
      ]));
      resetPaging();
      await loadTab();
    } catch (error) {
      ui.stamp.textContent = `会话列表加载失败:${error.message}`;
    }
  }

  async function loadTab() {
    if (!state.conversation) { ui.body.replaceChildren(D.el("p.dash-empty", { text: "选择一个会话。" })); ui.pager.replaceChildren(); ui.danger.replaceChildren(); return; }
    renderDanger();
    if (state.tab === "messages") return loadMessages();
    if (state.tab === "stats") return loadStats();
    return loadRecalls();
  }

  /* ── 消息 ─────────────────────────────────────────────── */
  function dayBounds() {
    const f = state.filter;
    const since = f.startDate ? Math.floor(new Date(`${f.startDate}T00:00:00`).getTime() / 1000) : "";
    const until = f.endDate ? Math.floor(new Date(`${f.endDate}T23:59:59`).getTime() / 1000) : "";
    return { since, until };
  }

  async function loadMessages() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    const params = scopeQuery();
    const { since, until } = dayBounds();
    params.set("q", state.q); params.set("sender", state.filter.sender); params.set("limit", "50");
    if (since) params.set("since", String(since));
    if (until) params.set("until", String(until));
    if (state.filter.recalled) params.set("recalled", "true");
    if (state.filter.media) params.set("media", "true");
    if (state.cursor) { params.set("before_sent", String(state.cursor.sent_at)); params.set("before_row", String(state.cursor.row_id)); }
    try {
      const payload = await D.api(`/api/dash/qq/messages?${params}`);
      if (seq !== state.loadSeq) return;
      state.items = payload.items || [];
      renderMessages(payload);
      ui.stamp.textContent = `${state.items.length} 条${state.q ? (payload.fts ? " · 全文索引" : " · 模糊匹配") : ""}`;
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.body.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
    }
  }

  function renderMessages(payload) {
    ui.body.textContent = "";
    if (!state.items.length) {
      ui.body.append(D.el("p.dash-empty", { text: state.q ? "没有匹配的消息。" : "这个会话还没有消息。" }));
      ui.pager.replaceChildren();
      return;
    }
    const grid = D.table([{ label: "时间", width: "132px" }, { label: "发送者", width: "150px" }, { label: "内容", width: "minmax(300px, 4fr)" }, { label: "媒体", width: "120px" }, { label: "", width: "56px" }]);
    for (const m of state.items) {
      const media = D.el("span.dash-media-chips", null, (m.media || []).slice(0, 4).map((item) => D.el("span.dash-chip", { text: MEDIA_LABEL[item.kind] || item.kind, title: item.label || "" })));
      const flags = D.el("span", null, m.recalled_at ? D.el("span.dash-chip.is-danger", { text: "撤回" }) : null);
      const row = D.el("div.dash-row", { role: "row", tabindex: "0", onclick: () => openMessage(m), onkeydown: (event) => { if (event.key === "Enter") openMessage(m); } },
        D.el("span.dash-cell-muted", { text: ts(m.sent_at) }),
        D.el("span", null, D.el("span.dash-sender", null, D.el("span", { text: m.sender_name || m.sender_id }), m.is_bot ? D.el("span.dash-chip.is-builtin", { text: "bot" }) : null), D.el("small.dash-cell-muted", { text: m.sender_id })),
        D.el("span.dash-cell-main", { text: m.text || (m.media?.length ? "[非文本消息]" : "") }),
        media, flags);
      row.classList.toggle("is-recalled", Boolean(m.recalled_at));
      grid.append(row);
    }
    ui.body.append(grid);
    const bar = D.el("div.dash-pager");
    const newer = D.iconButton("chevron-left", "较新", () => { state.pages.pop(); state.cursor = state.pages[state.pages.length - 1] || null; loadMessages(); });
    const older = D.iconButton("chevron-right", "较早", () => { state.pages.push(payload.next_cursor); state.cursor = payload.next_cursor; loadMessages(); });
    newer.disabled = !state.pages.length;
    older.disabled = !payload.next_cursor;
    bar.append(D.el("span.dash-pager-text", { text: `第 ${state.pages.length + 1} 页 · 每页 50 · 新在前` }), newer, older);
    ui.pager.replaceChildren(bar);
  }

  function openMessage(m) {
    const meta = [["消息 ID", m.message_id], ["发送者", `${m.sender_name || "—"}(${m.sender_id})${m.is_bot ? " · bot" : ""}`], ["时间", ts(m.sent_at)]];
    if (m.reply_to) meta.push(["回复", m.reply_to]);
    if (m.recalled_at) meta.push(["撤回于", ts(m.recalled_at)]);
    if (m.mentions?.length) meta.push(["@", m.mentions.map((u) => u.display_name ? `${u.display_name}(${u.user_id})` : u.user_id).join("、")]);
    const body = D.el("div", null,
      D.el("p.dash-detail-content", { text: m.text || "[非文本消息]" }),
      m.media?.length ? D.el("div.dash-media-chips", null, m.media.map((item) => D.el("span.dash-chip", { text: `${MEDIA_LABEL[item.kind] || item.kind}${item.label ? ` · ${item.label}` : ""}` }))) : null,
      D.el("p.dash-field-hint", { text: "历史库只存媒体占位符,没有图片字节与路径。" }),
      D.el("dl.dash-meta", null, meta.flatMap(([key, value]) => [D.el("dt", { text: key }), D.el("dd", { text: String(value) })])));
    D.openDrawer(`消息 ${m.message_id}`, body, []);
  }

  /* ── 统计 ─────────────────────────────────────────────── */
  async function loadStats() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "统计中…";
    ui.pager.replaceChildren();
    const params = scopeQuery(); params.set("days", String(state.days)); params.set("limit", "20");
    try {
      const s = await D.api(`/api/dash/qq/stats?${params}`);
      if (seq !== state.loadSeq) return;
      renderStats(s);
      ui.stamp.textContent = `${state.days ? `近 ${state.days} 天` : "全部"} ${s.total} 条`;
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.body.replaceChildren(D.el("p.dash-empty", { text: `统计失败:${error.message}` }));
    }
  }

  function renderStats(s) {
    ui.body.textContent = "";
    const r = s.ranking || { items: [] };
    const max = Math.max(1, ...r.items.map((i) => i.message_count));
    const podium = D.el("div.dash-podium", null, r.items.slice(0, 3).map((i, index) => D.el("div.dash-podium-item", null,
      D.el("span.dash-podium-rank", { text: ["🥇", "🥈", "🥉"][index] }),
      D.el("strong", { text: i.sender_name || i.sender_id }),
      D.el("span.dash-cell-muted", { text: `${i.message_count} 条 · ${i.active_days} 天活跃` }))));
    const bars = D.el("div.dash-hbars", null, r.items.map((i) => {
      const fill = D.el("i.dash-hbar-fill");
      fill.style.width = `${Math.round((i.message_count / max) * 100)}%`;
      return D.el("div.dash-hbar", null,
        D.el("span.dash-hbar-label", { text: `${i.rank}. ${i.sender_name || i.sender_id}` }),
        D.el("span.dash-hbar-track", null, fill),
        D.el("span.dash-cell-mono", { text: String(i.message_count) }));
    }));
    const rankCard = D.el("div.u-card", null, D.el("div.u-card-head", null, D.el("h3", { text: "发言榜" }), D.el("span.u-hint", { text: `${r.participant_count} 人参与 · bot 计入` })), podium, bars.childElementCount ? bars : D.el("p.dash-empty", { text: "这段时间没有消息。" }));

    // 星期 × 小时热力:7 行 24 列。
    const heatMax = Math.max(1, ...s.heat.flat());
    const heat = D.el("div.dash-heat");
    const days = ["日", "一", "二", "三", "四", "五", "六"];
    heat.append(D.el("span"));
    for (let h = 0; h < 24; h += 1) heat.append(D.el("span.dash-heat-axis", { text: h % 3 === 0 ? String(h) : "" }));
    s.heat.forEach((row, d) => {
      heat.append(D.el("span.dash-heat-axis", { text: days[d] }));
      row.forEach((count, h) => {
        const level = count === 0 ? 0 : Math.min(4, Math.ceil((count / heatMax) * 4));
        heat.append(D.el("i", { dataset: { l: String(level) }, title: `周${days[d]} ${h}:00 · ${count} 条` }));
      });
    });
    const heatCard = D.el("div.u-card", null, D.el("div.u-card-head", null, D.el("h3", { text: "活跃时段" }), D.el("span.u-hint", { text: "星期 × 小时,本地时间" })), heat);

    const media = Object.entries(s.media || {}).sort((a, b) => b[1] - a[1]);
    const mixCard = D.el("div.u-card", null, D.el("div.u-card-head", null, D.el("h3", { text: "构成" })),
      D.el("div.dash-kv", null,
        D.el("span", { text: "人类" }), D.el("strong", { text: String(s.human) }),
        D.el("span", { text: "bot" }), D.el("strong", { text: String(s.bot) }),
        ...media.flatMap(([kind, count]) => [D.el("span", { text: MEDIA_LABEL[kind] || kind }), D.el("strong", { text: String(count) })])));
    ui.body.append(rankCard, D.el("div.dash-two-col", null, heatCard, mixCard));
  }

  /* ── 撤回 ─────────────────────────────────────────────── */
  async function loadRecalls() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    const offset = state.pages.length * 50;
    const params = scopeQuery(); params.set("limit", "50"); params.set("offset", String(offset));
    try {
      const payload = await D.api(`/api/dash/qq/recalls?${params}`);
      if (seq !== state.loadSeq) return;
      ui.body.textContent = "";
      if (!payload.items.length) { ui.body.append(D.el("p.dash-empty", { text: "没有撤回记录。" })); ui.pager.replaceChildren(); ui.stamp.textContent = "0 条"; return; }
      const grid = D.table([{ label: "撤回时间", width: "132px" }, { label: "操作者", width: "120px" }, { label: "发送者", width: "150px" }, { label: "原文", width: "minmax(300px, 4fr)" }]);
      for (const r of payload.items) {
        grid.append(D.el("div.dash-row", { role: "row" },
          D.el("span.dash-cell-muted", { text: ts(r.recalled_at) }),
          D.el("span.dash-cell-mono", { text: r.operator_id || "—" }),
          D.el("span", { text: r.sender_name ? `${r.sender_name}(${r.sender_id})` : (r.sender_id || "未知") }),
          D.el("span.dash-cell-main", { text: r.text ?? "(原消息未记录)" })));
      }
      ui.body.append(grid);
      ui.pager.replaceChildren(D.pager({ offset, limit: 50, total: payload.total, onChange: (next) => { state.pages = new Array(Math.floor(next / 50)).fill(null); loadRecalls(); } }));
      ui.stamp.textContent = `${payload.total} 条撤回`;
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.body.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
    }
  }

  /* ── 危险区 ───────────────────────────────────────────── */
  function renderDanger() {
    const c = conv();
    ui.danger.replaceChildren(D.el("div.dash-danger", null,
      D.el("h4.dash-section", { text: "维护" }),
      D.el("div.dash-actions", null,
        D.el("button.dash-button", { type: "button", text: "重置上下文边界", title: "让当前人格从现在起不再读取这个会话更早的历史", onclick: resetBoundary }),
        D.el("button.dash-button.is-danger", { type: "button", text: "删除本会话历史…", onclick: () => openDelete(false) }),
        D.el("button.dash-button.is-danger", { type: "button", text: `删除账号 ${state.account} 全部群聊历史…`, onclick: () => openDelete(true) })),
      D.el("p.dash-field-hint", { text: `当前会话:${KIND_LABEL[c.kind] || c.kind} ${c.id}。历史库没有自动保留策略,这里是唯一的清理入口。` })));
  }

  async function resetBoundary() {
    const c = conv();
    const ok = await D.confirmAction(`重置 ${KIND_LABEL[c.kind]} ${c.id} 的上下文边界?当前人格之后只会读到这一刻之后的消息,历史本身不删。`, "重置");
    if (!ok) return;
    try {
      await D.api(`/api/dash/qq/boundary?${scopeQuery()}`, { method: "POST" });
      D.toast("已重置边界");
    } catch (error) {
      D.toast(`失败:${error.message}`, "error");
    }
  }

  function openDelete(wholeAccount) {
    const c = conv();
    const target = wholeAccount ? `账号 ${state.account} 下全部群聊` : `${KIND_LABEL[c.kind]} ${c.id}`;
    const expected = wholeAccount ? state.account : c.id;
    let mode = "all";
    const modeSeg = D.segmented([{ value: "all", label: "全部" }, { value: "keep", label: "只保留最近 N 天" }, { value: "range", label: "按条件" }], mode, (value) => { mode = value; keepBox.hidden = value !== "keep"; rangeBox.hidden = value !== "range"; });
    const keepDays = D.el("input.dash-select.dash-wide", { type: "number", min: "1", value: "30" });
    const keepBox = D.el("div", { hidden: true }, D.field("保留天数", keepDays));
    const sender = D.el("input.dash-select.dash-wide", { type: "text", placeholder: "发送者 QQ(可选)" });
    const start = D.el("input.dash-select.dash-wide", { type: "date" });
    const end = D.el("input.dash-select.dash-wide", { type: "date" });
    const rangeBox = D.el("div", { hidden: true }, D.field("发送者", sender), D.el("div.dash-field-row", null, D.field("起", start), D.field("止", end)));
    const confirm = D.el("input.dash-select.dash-wide", { type: "text", placeholder: expected });
    const body = D.el("div", null,
      D.el("p.dash-banner", { text: `即将删除 ${target} 的消息记录。这不可恢复,也不会通知模型。` }),
      D.field("范围", modeSeg.el), keepBox, rangeBox,
      D.field("确认", confirm, `输入 ${expected} 以确认`));
    const submit = D.el("button.dash-button.is-danger", { type: "button", text: "删除", onclick: async () => {
      if (confirm.value.trim() !== expected) { D.toast("确认文字不匹配", "error"); return; }
      const payload = { account: state.account, kind: wholeAccount ? "" : c.kind, id: wholeAccount ? "" : c.id, confirm: confirm.value.trim(), sender: "" };
      if (mode === "keep") payload.keep_days = Number(keepDays.value) || 30;
      if (mode === "range") {
        payload.sender = sender.value.trim();
        if (start.value) payload.since = Math.floor(new Date(`${start.value}T00:00:00`).getTime() / 1000);
        if (end.value) payload.until = Math.floor(new Date(`${end.value}T23:59:59`).getTime() / 1000);
      }
      submit.disabled = true;
      try {
        const result = await D.api("/api/dash/qq/messages/delete", { method: "POST", body: payload });
        D.toast(`已删除 ${result.messages_deleted} 条消息 · ${result.recalls_deleted} 条撤回`);
        D.closeDrawer();
        await loadConversations();
      } catch (error) {
        D.toast(`删除失败:${error.message}`, "error");
        submit.disabled = false;
      }
    } });
    D.openDrawer("删除历史", body, [submit]);
  }

  D.register({ name: "qq", root: "dashQqRoot", mount, refresh: () => reloadAll() });
})();
