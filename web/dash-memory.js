/*
 * 记忆浏览器面板(dashboard demo,09-03)。
 *
 * 顶部人格选择 + 事实/经历切换 + 状态过滤 + 搜索;统计卡;分页列表;点行开
 * 抽屉看全文与元数据,抽屉里删除。数据全部来自 /api/dash/memory/*。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = {
    persona: "",
    personas: [],
    table: "facts",
    status: "all",
    q: "",
    offset: 0,
    limit: 50,
    total: 0,
    items: [],
    loadSeq: 0
  };
  const ui = {};

  const STATUS_LABEL = { active: "活跃", archived: "已归档", pending: "待整理" };
  const TABLE_LABEL = { facts: "事实", episodes: "经历" };

  function mount(root) {
    root.textContent = "";
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "记忆" }),
      D.el("small", { id: "dashMemoryStamp", text: "" }));
    ui.stamp = head.querySelector("#dashMemoryStamp");
    const refresh = D.iconButton("refresh-cw", "刷新", () => reloadAll());
    head.append(refresh);

    ui.persona = D.el("select.dash-select", { title: "人格", onchange: () => { state.persona = ui.persona.value; state.offset = 0; reloadAll(); } });
    ui.tableSeg = D.el("div.con-segmented");
    for (const table of ["facts", "episodes"]) {
      const button = D.el("button", { type: "button", text: TABLE_LABEL[table], dataset: { table }, onclick: () => { state.table = table; state.offset = 0; syncSeg(); loadItems(); } });
      ui.tableSeg.append(button);
    }
    ui.status = D.el("select.dash-select", { title: "状态", onchange: () => { state.status = ui.status.value; state.offset = 0; loadItems(); } },
      D.el("option", { value: "all", text: "全部状态" }),
      D.el("option", { value: "active", text: "活跃" }),
      D.el("option", { value: "archived", text: "已归档" }));
    ui.search = D.el("input.dash-search", { type: "search", placeholder: "搜索内容…", oninput: () => {
      clearTimeout(ui.searchTimer);
      ui.searchTimer = setTimeout(() => { state.q = ui.search.value.trim(); state.offset = 0; loadItems(); }, 260);
    } });
    const searchBox = D.el("label.dash-search-box", null, D.icon("search"), ui.search);
    const toolbar = D.el("div.dash-toolbar", null, ui.persona, ui.tableSeg, ui.status, searchBox);

    ui.cards = D.el("div");
    ui.list = D.el("div.dash-table-wrap");
    ui.pager = D.el("div");
    root.append(head, toolbar, ui.cards, ui.list, ui.pager);
    syncSeg();
    reloadAll();
  }

  function syncSeg() {
    for (const button of ui.tableSeg.querySelectorAll("button")) {
      button.classList.toggle("on", button.dataset.table === state.table);
    }
  }

  async function reloadAll() {
    await loadPersonas();
    await Promise.all([loadStats(), loadItems()]);
  }

  async function loadPersonas() {
    try {
      const payload = await D.api("/api/dash/memory/personas");
      state.personas = payload.personas || [];
      if (!state.persona) state.persona = payload.active || "";
      ui.persona.textContent = "";
      for (const name of state.personas) {
        ui.persona.append(D.el("option", { value: name, text: name === payload.active ? `${name}(当前)` : name }));
      }
      ui.persona.value = state.persona;
    } catch (error) {
      ui.stamp.textContent = `人格列表加载失败:${error.message}`;
    }
  }

  function personaQuery() {
    return `persona=${encodeURIComponent(state.persona)}`;
  }

  async function loadStats() {
    try {
      const stats = await D.api(`/api/dash/memory/stats?${personaQuery()}`);
      ui.cards.replaceChildren(D.statCards([
        { label: "事实", value: stats.facts },
        { label: "经历", value: stats.episodes, hint: `短期 ${stats.short_diaries ?? 0} · 长期 ${stats.long_diaries ?? 0}` },
        { label: "待整理", value: stats.unconsolidated_diaries, hint: `待处理事件 ${stats.unprocessed_pending_events ?? 0}` },
        { label: "已逐出回合", value: stats.evicted_turns, hint: "可被 search_evicted_context 找回" }
      ]));
    } catch (error) {
      ui.cards.replaceChildren(D.el("p.dash-empty", { text: `统计加载失败:${error.message}` }));
    }
  }

  async function loadItems() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    const params = new URLSearchParams({
      persona: state.persona,
      table: state.table,
      q: state.q,
      status: state.status,
      limit: String(state.limit),
      offset: String(state.offset)
    });
    try {
      const payload = await D.api(`/api/dash/memory/items?${params}`);
      if (seq !== state.loadSeq) return;
      state.items = payload.items || [];
      state.total = payload.total || 0;
      renderList();
      ui.stamp.textContent = `${state.persona || "当前人格"} · ${TABLE_LABEL[state.table]} ${state.total} 条`;
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.list.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
      ui.stamp.textContent = "";
    }
  }

  function renderList() {
    ui.list.textContent = "";
    if (!state.items.length) {
      ui.list.append(D.el("p.dash-empty", { text: state.q ? "没有匹配的条目。" : "这个人格还没有这类记忆。" }));
      ui.pager.replaceChildren();
      return;
    }
    const table = D.el("div.dash-table", { role: "table" });
    table.append(D.el("div.dash-row.is-head", { role: "row" },
      D.el("span", { text: "内容" }),
      D.el("span", { text: "来源" }),
      D.el("span", { text: "状态" }),
      D.el("span", { text: state.table === "facts" ? "置信 / 强度" : "强度" }),
      D.el("span", { text: "召回" }),
      D.el("span", { text: "更新" }),
      D.el("span", { text: "" })));
    for (const item of state.items) {
      table.append(renderRow(item));
    }
    ui.list.append(table);
    ui.pager.replaceChildren(D.pager({
      offset: state.offset,
      limit: state.limit,
      total: state.total,
      onChange: (offset) => { state.offset = offset; loadItems(); }
    }));
  }

  function renderRow(item) {
    const strength = state.table === "facts"
      ? `${Number(item.confidence).toFixed(2)} / ${Number(item.strength).toFixed(2)}`
      : Number(item.strength).toFixed(2);
    const row = D.el("div.dash-row", { role: "row", tabindex: "0", onclick: () => openDetail(item), onkeydown: (event) => { if (event.key === "Enter") openDetail(item); } },
      D.el("span.dash-cell-main", { text: item.content }),
      D.el("span.dash-cell-muted", { text: item.source || "—" }),
      D.el("span", null, D.el(`span.dash-chip.is-${item.status}`, { text: STATUS_LABEL[item.status] || item.status })),
      D.el("span.dash-cell-mono", { text: strength }),
      D.el("span.dash-cell-mono", { text: String(item.recall_count ?? 0) }),
      D.el("span.dash-cell-muted", { text: D.formatTime(item.updated_at) }),
      D.el("span.dash-cell-actions", null, D.iconButton("trash-2", "删除", (event) => { event.stopPropagation(); removeItem(item); }, "is-danger")));
    return row;
  }

  function openDetail(item) {
    const meta = [
      ["ID", item.id],
      ["来源", item.source || "—"],
      ["状态", STATUS_LABEL[item.status] || item.status],
      state.table === "facts" ? ["置信度", Number(item.confidence).toFixed(2)] : ["保留", item.retention || "—"],
      ["强度", Number(item.strength).toFixed(2)],
      ["召回次数", item.recall_count ?? 0],
      ["可见性", item.visibility],
      ["归属", item.owner || "—"],
      ["主体", Array.isArray(item.subjects) && item.subjects.length ? item.subjects.join("、") : "—"],
      ["创建", D.formatTime(item.created_at)],
      ["更新", D.formatTime(item.updated_at)]
    ];
    const body = D.el("div", null,
      D.el("p.dash-detail-content", { text: item.content }),
      D.el("dl.dash-meta", null, meta.flatMap(([key, value]) => [D.el("dt", { text: key }), D.el("dd", { text: String(value) })])));
    const remove = D.el("button.dash-button.is-danger", { type: "button", text: "删除这条记忆", onclick: () => removeItem(item) });
    D.openDrawer(`${TABLE_LABEL[state.table]} #${item.id}`, body, [remove]);
  }

  async function removeItem(item) {
    const ok = await D.confirmAction(`删除这条${TABLE_LABEL[state.table]}?此操作不可撤销。\n\n${item.content.slice(0, 120)}`);
    if (!ok) return;
    try {
      await D.api(`/api/dash/memory/items/${state.table}/${item.id}?${personaQuery()}`, { method: "DELETE" });
      D.closeDrawer();
      await Promise.all([loadStats(), loadItems()]);
    } catch (error) {
      ui.stamp.textContent = `删除失败:${error.message}`;
    }
  }

  D.register({ name: "memory", root: "dashMemoryRoot", mount, refresh: () => reloadAll() });
})();
