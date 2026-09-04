/*
 * 群管面板(09-04):禁言 / 踢人 / 头衔的统一时间线,违规者榜单,踢人记录。
 *
 * 作用域 = 账号 + 群(只列有记录的群)。时间线来自三份记录合并去重,禁言事件带
 * 派生状态;违规者抽屉展开 reason_history;清空动作复用既有
 * /api/qq-group-management/* 端点,事件流清空走 /api/dash/qq/management/*。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = {
    account: D.recall("groups.account"),
    group: D.recall("groups.group"),
    groups: [],
    data: null,
    tab: "timeline",
    action: "all",
    q: "",
    loadSeq: 0
  };
  const ui = {};
  const ACTION = { ban: "禁言", unban: "解禁", kick: "踢出", kick_black: "踢黑", title_set: "设头衔", title_clear: "清头衔" };
  const ACTION_CLASS = { ban: "is-warn", unban: "is-active", kick: "is-danger", kick_black: "is-danger", title_set: "is-builtin", title_clear: "" };
  const STATUS = { active: "进行中", expired: "已过期", unmuted: "已解除", overridden: "被覆盖" };
  const STATUS_CLASS = { active: "is-warn", expired: "is-muted", unmuted: "is-active", overridden: "is-muted" };
  const SOURCE = { llm_tool: "Miyu 工具", onebot_notice: "外部管理员", offender_history: "旧记录" };
  const ROLE = { owner: "群主", admin: "管理员", member: "成员", unknown: "未知" };
  const ts = (unix) => unix ? D.formatTime(unix * 1000) : "—";
  function duration(seconds) {
    if (!seconds) return "—";
    if (seconds % 86400 === 0) return `${seconds / 86400} 天`;
    if (seconds % 3600 === 0) return `${seconds / 3600} 小时`;
    if (seconds % 60 === 0) return `${seconds / 60} 分钟`;
    return `${seconds} 秒`;
  }
  const scopeQuery = () => new URLSearchParams({ account: state.account, group: state.group });
  const legacyQuery = () => new URLSearchParams({ account_id: state.account, group_id: state.group });

  function mount(root) {
    root.textContent = "";
    ui.stamp = D.el("small", { text: "" });
    ui.group = D.select([], "", (value) => { const [account, group] = value.split(":"); state.account = account; state.group = group; D.remember("groups.account", account); D.remember("groups.group", group); load(); }, "群");
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "群管" }),
      D.iconButton("refresh-cw", "刷新", () => reloadAll()),
      ui.stamp,
      D.el("span.dash-scope", null, D.el("span.dash-scope-label", { text: "群" }), ui.group));
    ui.cards = D.el("div");
    ui.tabs = D.segmented([{ value: "timeline", label: "时间线" }, { value: "offenders", label: "违规者" }, { value: "kicks", label: "踢人" }], state.tab, (value) => { state.tab = value; renderToolbar(); render(); });
    ui.filters = D.el("div.dash-filters");
    ui.search = D.el("input.dash-search", { type: "search", placeholder: "按昵称 / QQ / 理由过滤…", oninput: () => { clearTimeout(ui.searchTimer); ui.searchTimer = setTimeout(() => { state.q = ui.search.value.trim().toLowerCase(); render(); }, 200); } });
    const toolbar = D.el("div.dash-toolbar", null, ui.tabs.el, ui.filters, D.el("label.dash-search-box", null, D.icon("search"), ui.search));
    ui.body = D.el("div");
    ui.danger = D.el("div");
    root.append(head, ui.cards, toolbar, ui.body, ui.danger);
    renderToolbar();
    reloadAll();
  }

  function renderToolbar() {
    ui.filters.textContent = "";
    if (state.tab === "timeline") {
      ui.filters.append(D.select([{ value: "all", label: "全部动作" }, { value: "ban", label: "禁言 / 解禁" }, { value: "kick", label: "踢出 / 踢黑" }, { value: "title", label: "头衔" }], state.action, (value) => { state.action = value; render(); }));
    }
  }

  async function reloadAll() {
    try {
      const payload = await D.api("/api/dash/qq/groups");
      state.groups = payload.groups || [];
      ui.group.textContent = "";
      for (const g of state.groups) ui.group.append(D.el("option", { value: `${g.account_id}:${g.group_id}`, text: `${g.name ? `${g.name}(${g.group_id})` : g.group_id} · bot ${g.account_id}${payload.connected?.includes(g.account_id) ? "" : "(离线)"}` }));
      const keys = state.groups.map((g) => `${g.account_id}:${g.group_id}`);
      const current = `${state.account}:${state.group}`;
      if (!keys.includes(current)) { const [account, group] = (keys[0] || ":").split(":"); state.account = account; state.group = group; }
      ui.group.value = `${state.account}:${state.group}`;
      if (!state.group) { ui.cards.replaceChildren(D.el("p.dash-empty", { text: "还没有任何群管记录。Miyu 在群里禁言、踢人或设头衔后这里才会有数据。" })); ui.body.textContent = ""; return; }
      await load();
    } catch (error) {
      ui.stamp.textContent = `群清单加载失败:${error.message}`;
    }
  }

  async function load() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    try {
      const data = await D.api(`/api/dash/qq/management?${scopeQuery()}`);
      if (seq !== state.loadSeq) return;
      state.data = data;
      renderCards();
      render();
      renderDanger();
      ui.stamp.textContent = `${data.group_name ? `${data.group_name} · ` : ""}${data.events.length} 条事件`;
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.body.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
    }
  }

  function renderCards() {
    const d = state.data;
    const monthAgo = Date.now() / 1000 - 30 * 86400;
    const recent = d.events.filter((e) => e.happened_at > monthAgo).length;
    const active = d.events.filter((e) => e.action === "ban" && e.status === "active").length;
    ui.cards.replaceChildren(D.statCards([
      { label: "bot 身份", value: ROLE[d.bot_role] || d.bot_role || "未知", hint: "上次处理时记录" },
      { label: "违规者", value: Object.keys(d.offenders || {}).length, hint: "上限 500,超出淘汰最少违规的" },
      { label: "踢人", value: (d.kicks || []).length, hint: `踢黑 ${(d.kicks || []).filter((k) => k.reject_add_request).length}` },
      { label: "进行中禁言", value: active, hint: `事件总数 ${d.events.length}` },
      { label: "近 30 天动作", value: recent }
    ]));
  }

  const matches = (...fields) => !state.q || fields.some((f) => String(f || "").toLowerCase().includes(state.q));

  function render() {
    if (!state.data) return;
    if (state.tab === "timeline") return renderTimeline();
    if (state.tab === "offenders") return renderOffenders();
    return renderKicks();
  }

  function actionMatches(action) {
    if (state.action === "all") return true;
    if (state.action === "ban") return action === "ban" || action === "unban";
    if (state.action === "kick") return action === "kick" || action === "kick_black";
    return action === "title_set" || action === "title_clear";
  }

  function renderTimeline() {
    const events = state.data.events.filter((e) => actionMatches(e.action) && matches(e.user_name, e.user_id, e.reason, e.detail)).slice().reverse();
    ui.body.replaceChildren(D.timeline(events.map((e) => ({
      time: ts(e.happened_at),
      body: D.el("div.dash-event", null,
        D.el("div.dash-event-head", null,
          D.el(`span.dash-chip.${ACTION_CLASS[e.action] || ""}`, { text: ACTION[e.action] || e.action }),
          D.el("strong", { text: e.user_name || e.user_id }),
          D.el("span.dash-cell-muted", { text: e.user_id }),
          e.action === "ban" ? D.el("span.dash-cell-muted", { text: duration(e.duration) }) : null,
          e.status ? D.el(`span.dash-chip.${STATUS_CLASS[e.status] || ""}`, { text: STATUS[e.status] || e.status }) : null,
          D.el("span.dash-actions-gap"),
          D.el("span.dash-chip", { text: SOURCE[e.source] || e.source })),
        e.detail ? D.el("p.dash-timeline-body", { text: `头衔:${e.detail}` }) : null,
        e.reason ? D.el("p.dash-timeline-body", { text: e.reason }) : D.el("p.dash-timeline-body.is-old", { text: "(无理由)" }),
        e.operator_id ? D.el("small.dash-cell-muted", { text: `操作者 ${e.operator_id} · ${e.record_id}` }) : null)
    })), "没有匹配的事件。"));
  }

  function renderOffenders() {
    const list = Object.values(state.data.offenders || {}).filter((o) => matches(o.user_name, o.user_id, o.last_reason)).sort((a, b) => b.ban_count - a.ban_count || b.last_ban_at - a.last_ban_at);
    if (!list.length) { ui.body.replaceChildren(D.el("p.dash-empty", { text: "没有违规者记录。" })); return; }
    const grid = D.table([{ label: "#", width: "40px" }, { label: "成员", width: "minmax(160px, 2fr)" }, { label: "次数", width: "64px" }, { label: "累计时长", width: "110px" }, { label: "首次 / 最近", width: "220px" }, { label: "最近理由", width: "minmax(200px, 3fr)" }, { label: "", width: "40px" }]);
    list.forEach((o, index) => {
      grid.append(D.el("div.dash-row", { role: "row", tabindex: "0", onclick: () => openOffender(o), onkeydown: (event) => { if (event.key === "Enter") openOffender(o); } },
        D.el("span.dash-cell-mono", { text: String(index + 1) }),
        D.el("span", null, D.el("span.dash-sender", null, D.el("span", { text: o.user_name || o.user_id })), D.el("small.dash-cell-muted", { text: o.user_id })),
        D.el("span.dash-cell-mono", { text: String(o.ban_count) }),
        D.el("span.dash-cell-muted", { text: duration(o.total_duration) }),
        D.el("span.dash-cell-muted", { text: `${ts(o.first_ban_at).slice(0, 10)} / ${ts(o.last_ban_at)}` }),
        D.el("span.dash-cell-main", { text: o.last_reason || "—" }),
        D.el("span.dash-cell-actions", null, D.iconButton("trash-2", "删除该违规者记录", (event) => { event.stopPropagation(); removeOffender(o); }, "is-danger"))));
    });
    ui.body.replaceChildren(grid);
  }

  function openOffender(o) {
    const history = (o.reason_history || []).slice().reverse();
    const member = (state.data.members || []).find((m) => m.user_id === o.user_id);
    const body = D.el("div", null,
      D.el("dl.dash-meta", null, [["QQ", o.user_id], ["禁言次数", o.ban_count], ["累计时长", duration(o.total_duration)], ["首次", ts(o.first_ban_at)], ["最近", ts(o.last_ban_at)],
        member ? ["踢出", member.kick_count] : null, member ? ["头衔操作", member.title_count] : null].filter(Boolean)
        .flatMap(([key, value]) => [D.el("dt", { text: key }), D.el("dd", { text: String(value) })])),
      D.el("h4.dash-section", { text: `禁言历史(${history.length})` }),
      D.timeline(history.map((h) => ({ time: ts(h.banned_at), chip: duration(h.duration), chipClass: "is-warn", body: h.reason || "(无理由)" })), "没有明细。"));
    const remove = D.el("button.dash-button.is-danger", { type: "button", text: "删除记录", onclick: () => removeOffender(o) });
    D.openDrawer(o.user_name || o.user_id, body, [remove]);
  }

  function renderKicks() {
    const list = (state.data.kicks || []).filter((k) => matches(k.user_name, k.user_id, k.reason)).slice().reverse();
    if (!list.length) { ui.body.replaceChildren(D.el("p.dash-empty", { text: "没有踢人记录。" })); return; }
    const grid = D.table([{ label: "时间", width: "132px" }, { label: "成员", width: "minmax(160px, 2fr)" }, { label: "类型", width: "72px" }, { label: "来源", width: "96px" }, { label: "操作者", width: "110px" }, { label: "理由", width: "minmax(200px, 3fr)" }]);
    for (const k of list) {
      grid.append(D.el("div.dash-row", { role: "row" },
        D.el("span.dash-cell-muted", { text: ts(k.kicked_at) }),
        D.el("span", null, D.el("span.dash-sender", null, D.el("span", { text: k.user_name || k.user_id })), D.el("small.dash-cell-muted", { text: k.user_id })),
        D.el("span", null, D.el(`span.dash-chip.${k.reject_add_request ? "is-danger" : "is-warn"}`, { text: k.reject_add_request ? "踢黑" : "踢出" })),
        D.el("span.dash-cell-muted", { text: SOURCE[k.source] || k.source || "—" }),
        D.el("span.dash-cell-mono", { text: k.operator_id || "—" }),
        D.el("span.dash-cell-main", { text: k.reason || "—" })));
    }
    ui.body.replaceChildren(grid);
  }

  function renderDanger() {
    ui.danger.replaceChildren(D.el("div.dash-danger", null,
      D.el("h4.dash-section", { text: "清理" }),
      D.el("div.dash-actions", null,
        D.el("button.dash-button.is-danger", { type: "button", text: "清空违规者", onclick: () => clearLegacy("offenders", "违规者名单") }),
        D.el("button.dash-button.is-danger", { type: "button", text: "清空踢人记录", onclick: () => clearLegacy("kicks", "踢人记录") }),
        D.el("button.dash-button.is-danger", { type: "button", text: "清空事件流", onclick: clearEvents })),
      D.el("p.dash-field-hint", { text: "禁言、踢人、改头衔本身只能由 Miyu 在群里带着在场消息执行,这里不提供。" })));
  }

  async function removeOffender(o) {
    const ok = await D.confirmAction(`删除 ${o.user_name || o.user_id} 的违规者记录(${o.ban_count} 次)?事件流里的历史不受影响。`);
    if (!ok) return;
    try {
      await D.api(`/api/qq-group-management/offenders/${encodeURIComponent(o.user_id)}?${legacyQuery()}`, { method: "DELETE" });
      D.closeDrawer();
      D.toast("已删除");
      await load();
    } catch (error) {
      D.toast(`删除失败:${error.message}`, "error");
    }
  }

  async function clearLegacy(kind, label) {
    const ok = await D.confirmAction(`清空本群的${label}?不可恢复。`, "清空");
    if (!ok) return;
    try {
      await D.api("/api/qq-group-management/history/clear", { method: "POST", body: { account_id: state.account, group_id: state.group, kind } });
      D.toast(`已清空${label}`);
      await load();
    } catch (error) {
      D.toast(`失败:${error.message}`, "error");
    }
  }

  async function clearEvents() {
    const ok = await D.confirmAction("清空本群的统一事件流?违规者名单与踢人记录里的旧条目仍会在时间线里补回。", "清空");
    if (!ok) return;
    try {
      await D.api(`/api/dash/qq/management/events/clear?${scopeQuery()}`, { method: "POST" });
      D.toast("已清空事件流");
      await load();
    } catch (error) {
      D.toast(`失败:${error.message}`, "error");
    }
  }

  D.register({ name: "groups", root: "dashGroupsRoot", mount, refresh: () => reloadAll() });
})();
