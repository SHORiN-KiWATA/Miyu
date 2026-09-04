/*
 * 好感·情绪面板(09-04)。
 *
 * 好感度标签:账号 + 人格作用域 → 统计卡(档案数、等级分布、今日增减预算、
 * 自动更新关闭数)→ 等级/标签过滤 + 搜索 → 榜单 → 抽屉(分数曲线、事件审计、
 * 编辑分数/备注/标签/自动更新、清空事件、删除)。
 * 情绪标签:功能落地前显示占位说明。数据来自 /api/dash/affection/*。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = {
    account: D.recall("affection.account"),
    persona: D.recall("affection.persona"),
    scopes: null,
    listing: null,
    tab: "affection",
    level: "all",
    sort: "score",
    q: "",
    loadSeq: 0
  };
  const ui = {};
  const LEVELS = ["刻意疏远", "冷漠", "中立", "认识", "好友", "信任", "亲近"];
  const LEVEL_CLASS = { "刻意疏远": "is-danger", "冷漠": "is-muted", "中立": "", "认识": "", "好友": "is-active", "信任": "is-active", "亲近": "is-builtin" };
  const ts = (unix) => unix ? D.formatTime(unix * 1000) : "—";
  const scopeQuery = () => new URLSearchParams({ account: state.account, persona: state.persona });

  function mount(root) {
    root.textContent = "";
    ui.stamp = D.el("small", { text: "" });
    ui.account = D.select([], state.account, (value) => { state.account = value; D.remember("affection.account", value); load(); }, "bot 账号");
    ui.persona = D.select([], state.persona, (value) => { state.persona = value; D.remember("affection.persona", value); load(); }, "人格");
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "好感·情绪" }),
      D.iconButton("refresh-cw", "刷新", () => reloadAll()),
      ui.stamp,
      D.el("span.dash-scope", null, D.el("span.dash-scope-label", { text: "账号" }), ui.account, D.el("span.dash-scope-label", { text: "人格" }), ui.persona));
    ui.tabs = D.segmented([{ value: "affection", label: "好感度" }, { value: "emotion", label: "情绪" }], state.tab, (value) => { state.tab = value; render(); });
    ui.cards = D.el("div");
    ui.filters = D.el("div.dash-filters");
    ui.search = D.el("input.dash-search", { type: "search", placeholder: "昵称 / QQ / 标签 / 备注…", oninput: () => { clearTimeout(ui.searchTimer); ui.searchTimer = setTimeout(() => { state.q = ui.search.value.trim().toLowerCase(); renderList(); }, 200); } });
    ui.toolbar = D.el("div.dash-toolbar", null, ui.tabs.el, ui.filters, D.el("label.dash-search-box", null, D.icon("search"), ui.search));
    ui.body = D.el("div");
    root.append(head, ui.cards, ui.toolbar, ui.body);
    renderFilters();
    reloadAll();
  }

  function renderFilters() {
    ui.filters.textContent = "";
    ui.filters.append(
      D.select([{ value: "all", label: "全部等级" }, ...LEVELS.map((l) => ({ value: l, label: l }))], state.level, (value) => { state.level = value; renderList(); }),
      D.select([{ value: "score", label: "按分数" }, { value: "recent", label: "按最近互动" }, { value: "messages", label: "按消息数" }, { value: "events", label: "按事件数" }], state.sort, (value) => { state.sort = value; renderList(); }));
  }

  async function reloadAll() {
    try {
      const scopes = await D.api("/api/dash/affection/scopes");
      state.scopes = scopes;
      const accounts = scopes.accounts || [];
      const personas = scopes.personas || [];
      if (!personas.includes(scopes.active_persona)) personas.push(scopes.active_persona);
      if (!state.account || !accounts.includes(state.account)) state.account = scopes.connected?.[0] || accounts[0] || "";
      if (!state.persona || !personas.includes(state.persona)) state.persona = scopes.active_persona;
      ui.account.textContent = ""; ui.persona.textContent = "";
      for (const a of accounts) ui.account.append(D.el("option", { value: a, text: `${a}${scopes.connected?.includes(a) ? "(在线)" : ""}` }));
      for (const p of personas) ui.persona.append(D.el("option", { value: p, text: p === scopes.active_persona ? `${p}(当前)` : p }));
      ui.account.value = state.account; ui.persona.value = state.persona;
      await load();
    } catch (error) {
      ui.stamp.textContent = `作用域加载失败:${error.message}`;
    }
  }

  async function load() {
    if (!state.account) { ui.cards.replaceChildren(D.el("p.dash-empty", { text: "还没有任何好感度档案。" })); ui.body.textContent = ""; return; }
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    try {
      const listing = await D.api(`/api/dash/affection/items?${scopeQuery()}`);
      if (seq !== state.loadSeq) return;
      state.listing = listing;
      render();
      ui.stamp.textContent = `${listing.items.length} 份档案`;
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.body.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
    }
  }

  function render() {
    ui.toolbar.querySelectorAll(".dash-filters, .dash-search-box").forEach((node) => { node.hidden = state.tab !== "affection"; });
    if (state.tab === "emotion") return renderEmotion();
    renderCards();
    renderList();
  }

  function renderCards() {
    const l = state.listing;
    if (!l) return;
    const dist = D.el("div.dash-level-bar", null, LEVELS.map((name) => {
      const count = l.levels?.[name] || 0;
      const cell = D.el("span.dash-level-cell", { title: `${name} ${count}` });
      cell.style.flexGrow = String(Math.max(count, 0.15));
      cell.append(D.el("i", { text: count ? `${name} ${count}` : "" }));
      cell.classList.toggle("is-empty", !count);
      return cell;
    }));
    const cards = D.statCards([
      { label: "档案", value: l.items.length, hint: l.enabled ? `自动更新 ${l.update_enabled ? "开" : "关"} · 已关闭 ${l.auto_update_off} 人` : "功能未启用" },
      { label: "今日增益", value: l.today_gain.toFixed(1), hint: `全员合计 · 单人上限 ${l.limits.daily_gain}` },
      { label: "今日亏损", value: l.today_loss.toFixed(1), hint: `全员合计 · 单人上限 ${l.limits.daily_loss}` },
      { label: "普通上限", value: l.limits.regular_max, hint: `初始 ${l.limits.initial} · 区间 ${l.limits.min}~${l.limits.max}` }
    ]);
    ui.cards.replaceChildren(...[
      l.enabled ? null : D.el("p.dash-banner", { text: "好感度在配置里是关闭的(affection_enable=false):不会注入关系提示,也不会自动更新;这里仍可查看与整理旧档案。" }),
      cards,
      D.el("div.u-card", null, D.el("div.u-card-head", null, D.el("h3", { text: "等级分布" }), D.el("span.u-hint", { text: "七档,普通用户到不了亲近" })), dist)
    ].filter(Boolean));
  }

  function visible() {
    let items = (state.listing?.items || []).filter((p) => (state.level === "all" || p.level === state.level)
      && (!state.q || [p.sender_name, p.user_id, p.note, ...(p.tags || [])].some((f) => String(f || "").toLowerCase().includes(state.q))));
    const key = { score: (p) => p.score, recent: (p) => p.last_interaction_at, messages: (p) => p.message_count, events: (p) => p.event_count }[state.sort];
    items.sort((a, b) => key(b) - key(a));
    return items;
  }

  function renderList() {
    const items = visible();
    if (!items.length) { ui.body.replaceChildren(D.el("p.dash-empty", { text: state.listing?.items.length ? "没有匹配的档案。" : "这个账号 + 人格下还没有档案。" })); return; }
    const grid = D.table([{ label: "#", width: "40px" }, { label: "成员", width: "minmax(170px, 2fr)" }, { label: "分数", width: "72px" }, { label: "等级", width: "84px" }, { label: "标签", width: "minmax(160px, 2fr)" }, { label: "互动", width: "150px" }, { label: "最近", width: "128px" }, { label: "自动", width: "56px" }, { label: "", width: "40px" }]);
    items.forEach((p, index) => {
      grid.append(D.el("div.dash-row", { role: "row", tabindex: "0", onclick: () => openProfile(p), onkeydown: (event) => { if (event.key === "Enter") openProfile(p); } },
        D.el("span.dash-cell-mono", { text: String(index + 1) }),
        D.el("span", null, D.el("span.dash-sender", null, D.el("span", { text: p.sender_name || p.user_id })), D.el("small.dash-cell-muted", { text: p.user_id })),
        D.el("span.dash-cell-mono", { text: p.score.toFixed(1) }),
        D.el("span", null, D.el(`span.dash-chip.${LEVEL_CLASS[p.level] || ""}`, { text: p.level })),
        D.el("span.dash-media-chips", null, (p.tags || []).slice(0, 5).map((t) => D.el("span.dash-chip", { text: t }))),
        D.el("span.dash-cell-muted", { text: `${p.message_count} 消息 · ${p.direct_interaction_count} 直接 · ${p.bot_reply_count} 回复` }),
        D.el("span.dash-cell-muted", { text: ts(p.last_interaction_at) }),
        D.el("span", null, D.el(`span.dash-chip.${p.auto_update_enabled ? "is-active" : "is-muted"}`, { text: p.auto_update_enabled ? "开" : "关" })),
        D.el("span.dash-cell-actions", null, D.iconButton("trash-2", "删除档案", (event) => { event.stopPropagation(); removeProfile(p); }, "is-danger"))));
    });
    ui.body.replaceChildren(grid);
  }

  async function openProfile(summary) {
    let p;
    try {
      p = (await D.api(`/api/dash/affection/items/${encodeURIComponent(summary.user_id)}?${scopeQuery()}`)).profile;
    } catch (error) {
      D.toast(`加载失败:${error.message}`, "error");
      return;
    }
    const l = state.listing.limits;
    const events = (p.events || []).slice().reverse();
    const points = events.map((e, i) => ({ x: i, y: e.score_after }));
    if (points.length) points.unshift({ x: -1, y: events[0].score_before });
    const form = {
      score: D.el("input.dash-select.dash-wide", { type: "number", step: "0.5", min: String(l.min), max: String(p.max_score), value: p.score.toFixed(1) }),
      reason: D.el("input.dash-select.dash-wide", { type: "text", placeholder: "改分理由(写进事件)" }),
      note: D.el("textarea.dash-textarea", { rows: "3", placeholder: "备注:会进关系提示" }),
      tags: D.el("input.dash-select.dash-wide", { type: "text", value: (p.tags || []).join(", "), placeholder: `逗号分隔,最多 ${l.max_tags} 个` }),
      auto: D.el("input", { type: "checkbox" })
    };
    form.note.value = p.note || "";
    form.auto.checked = Boolean(p.auto_update_enabled);
    const body = D.el("div", null,
      D.el("div.dash-score-head", null,
        D.el("strong.dash-score-big", { text: p.score.toFixed(1) }),
        D.el(`span.dash-chip.${LEVEL_CLASS[p.level] || ""}`, { text: p.level }),
        D.el("span.dash-cell-muted", { text: `回复倾向 ${p.reply_bias >= 0 ? "+" : ""}${p.reply_bias.toFixed(3)} · 增益系数 ${p.gain_multiplier.toFixed(2)} · 上限 ${p.max_score}` })),
      points.length > 1 ? D.sparkline(points, { baseline: l.initial, height: 70 }) : D.el("p.dash-field-hint", { text: "还没有分数变化。" }),
      D.el("div.dash-field-row", null, D.field("分数", form.score), D.field("理由", form.reason)),
      D.field("备注", form.note),
      D.field("标签", form.tags),
      D.el("label.dash-check", null, form.auto, D.el("span", { text: "允许自动更新(关掉后模型不再改这个人的分)" })),
      D.el("h4.dash-section", { text: "档案" }),
      D.el("dl.dash-meta", null, [["QQ", p.user_id], ["消息", p.message_count], ["直接互动", p.direct_interaction_count], ["bot 回复", p.bot_reply_count],
        ["最近会话", `${p.last_conversation_kind || "—"} ${p.last_conversation_id || ""}`], ["今日", `${p.daily_date || "—"} · +${Number(p.daily_gain).toFixed(1)} / -${Number(p.daily_loss).toFixed(1)}`],
        ["最近互动", ts(p.last_interaction_at)], ["建档", ts(p.created_at)], ["更新", ts(p.updated_at)]]
        .flatMap(([k, v]) => [D.el("dt", { text: k }), D.el("dd", { text: String(v) })])),
      D.el("h4.dash-section", { text: `事件审计(${(p.events || []).length},上限 50)` }),
      D.timeline((p.events || []).map((e) => ({
        time: ts(e.created_at),
        chip: `${e.delta >= 0 ? "+" : ""}${e.delta.toFixed(2)} → ${e.score_after.toFixed(1)}`,
        chipClass: e.delta > 0 ? "is-active" : (e.delta < 0 ? "is-danger" : ""),
        body: D.el("div", null,
          D.el("p.dash-timeline-body", { text: e.reason || "(无理由)" }),
          D.el("small.dash-cell-muted", { text: `置信 ${e.confidence.toFixed(2)}${e.tags_add?.length ? ` · +${e.tags_add.join("、")}` : ""}${e.tags_remove?.length ? ` · −${e.tags_remove.join("、")}` : ""} · ${e.message_id === "dashboard" ? "手动" : `消息 ${e.message_id || "—"}`}` }))
      })), "还没有事件。"));

    const clear = D.el("button.dash-button", { type: "button", text: "清空事件", onclick: async () => {
      if (!(await D.confirmAction("清空这个人的事件审计?分数与标签不变。", "清空"))) return;
      await patch(p.user_id, { clear_events: true });
    } });
    const remove = D.el("button.dash-button.is-danger", { type: "button", text: "删除档案", onclick: () => removeProfile(p) });
    const save = D.el("button.dash-button.is-primary", { type: "button", text: "保存", onclick: () => patch(p.user_id, {
      score: Number(form.score.value), reason: form.reason.value, note: form.note.value,
      tags: form.tags.value.split(/[,,、]/).map((t) => t.trim()).filter(Boolean), auto_update_enabled: form.auto.checked
    }) });
    D.openDrawer(p.sender_name || p.user_id, body, [clear, remove, save]);
  }

  async function patch(userId, body) {
    try {
      await D.api(`/api/dash/affection/items/${encodeURIComponent(userId)}?${scopeQuery()}`, { method: "PATCH", body });
      D.toast("已保存");
      D.closeDrawer();
      await load();
    } catch (error) {
      D.toast(`保存失败:${error.message}`, "error");
    }
  }

  async function removeProfile(p) {
    if (!(await D.confirmAction(`删除 ${p.sender_name || p.user_id} 的好感度档案?下次互动会从初始分重新建档。`))) return;
    try {
      await D.api(`/api/dash/affection/items/${encodeURIComponent(p.user_id)}?${scopeQuery()}`, { method: "DELETE" });
      D.toast("已删除");
      D.closeDrawer();
      await load();
    } catch (error) {
      D.toast(`删除失败:${error.message}`, "error");
    }
  }

  /* ── 情绪标签 ────────────────────────────────────────── */
  const EMO_LABEL_CLASS = { "烦躁": "is-danger", "低落": "is-muted", "疲惫": "is-muted", "兴奋": "is-active", "调皮": "is-active", "愉快": "is-active", "平静": "" };
  const EMO_SOURCE = { reply: "回复", llm: "模型", moderation: "违规处理", manual: "手动" };
  const fmtDur = (secs) => {
    if (!secs || secs <= 0) return "已在基线";
    if (secs < 3600) return `${Math.round(secs / 60)} 分钟`;
    if (secs < 86400) return `${(secs / 3600).toFixed(1)} 小时`;
    return `${(secs / 86400).toFixed(1)} 天`;
  };

  async function renderEmotion() {
    ui.cards.textContent = "";
    ui.body.replaceChildren(D.el("p.dash-empty", { text: "载入中…" }));
    let e;
    try {
      e = await D.api(`/api/dash/affection/emotion?${scopeQuery()}`);
    } catch (error) {
      ui.body.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
      return;
    }
    if (state.tab !== "emotion") return;
    const ef = e.effective, st = e.stored;
    const pct = (v, min, max) => `${Math.round(((v - min) / (max - min)) * 100)}%`;

    // 二维方格:横轴心情 −1..1,纵轴表达欲 1..0;七个区域用 title 标出。
    const grid = D.el("div.dash-emo-grid");
    const stored = D.el("i.dash-emo-dot.is-stored", { title: `存储态 ${st.label} (${st.valence.toFixed(2)}, ${st.arousal.toFixed(2)})` });
    stored.style.left = pct(st.valence, -1, 1); stored.style.top = pct(1 - st.arousal, 0, 1);
    const eff = D.el("i.dash-emo-dot.is-effective", { title: `有效态 ${ef.label} (${ef.valence.toFixed(2)}, ${ef.arousal.toFixed(2)})` });
    eff.style.left = pct(ef.valence, -1, 1); eff.style.top = pct(1 - ef.arousal, 0, 1);
    for (const [name, left, top] of [["烦躁", 12, 20], ["低落", 12, 78], ["疲惫", 50, 90], ["兴奋", 86, 8], ["调皮", 74, 22], ["愉快", 84, 55], ["平静", 50, 52]]) {
      const tag = D.el("span.dash-emo-region", { text: name }); tag.style.left = `${left}%`; tag.style.top = `${top}%`; grid.append(tag);
    }
    grid.append(D.el("span.dash-emo-axis.is-x", { text: "心情 →" }), D.el("span.dash-emo-axis.is-y", { text: "表达欲 ↑" }), stored, eff);

    const meter = (label, value, min, max, baseline, back) => {
      const fill = D.el("i.dash-meter-fill"); fill.style.left = pct(Math.min(value, baseline), min, max); fill.style.width = `${Math.abs(value - baseline) / (max - min) * 100}%`;
      const base = D.el("i.dash-meter-base"); base.style.left = pct(baseline, min, max);
      return D.el("div.dash-meter", null, D.el("span.dash-meter-label", { text: label }), D.el("span.dash-meter-track", null, base, fill), D.el("span.dash-cell-mono", { text: value.toFixed(2) }), D.el("small.dash-cell-muted", { text: `回归基线还需 ${back}` }));
    };
    const stateCard = D.el("div.u-card", null,
      D.el("div.u-card-head", null, D.el("h3", { text: "当前状态" }), D.el("span.u-hint", { text: "空心 = 存储态(已衰减),实心 = 叠加时段与冷清后的有效态" })),
      D.el("div.dash-emo-layout", null, grid,
        D.el("div.dash-emo-side", null,
          D.el("div.dash-score-head", null, D.el("strong.dash-score-big", { text: ef.label }), D.el(`span.dash-chip.${EMO_LABEL_CLASS[ef.label] || ""}`, { text: ef.label_en }), ef.label !== st.label ? D.el("span.dash-cell-muted", { text: `存储态 ${st.label}` }) : null),
          D.el("dl.dash-meta", null, [["心情", ef.valence_text], ["精神", ef.arousal_text], ["阈值修正", `${ef.threshold_adjust >= 0 ? "+" : ""}${ef.threshold_adjust.toFixed(3)}(负=更想接话)`],
            ["时段修正", `${ef.time_arousal_adjust >= 0 ? "+" : ""}${ef.time_arousal_adjust.toFixed(2)} 表达欲`], ["冷清", `${ef.idle_hours.toFixed(1)} 小时无人 → 心情 ${ef.idle_valence_adjust.toFixed(2)} · 表达欲 ${ef.idle_arousal_adjust.toFixed(2)}`],
            ["注入提示", ef.tone_hint || "(平静时不注入)"], ["今日", `${e.daily.date || "—"} · ${e.daily.interactions} 次互动 · +${e.daily.gain.toFixed(2)}/${e.daily.gain_limit} · −${e.daily.loss.toFixed(2)}/${e.daily.loss_limit}`],
            ["上次写入", st.updated_at ? ts(st.updated_at) : "从未"]].flatMap(([k, v]) => [D.el("dt", { text: k }), D.el("dd", { text: String(v) })])),
          meter("心情", st.valence, -1, 1, 0, fmtDur(e.return_secs.valence)),
          meter("表达欲", st.arousal, 0, 1, 0.5, fmtDur(e.return_secs.arousal)))));

    const events = e.events || [];
    const rev = events.slice().reverse();
    const curves = D.el("div.u-card", null, D.el("div.u-card-head", null, D.el("h3", { text: "曲线" }), D.el("span.u-hint", { text: `最近 ${events.length} 次变化(上限 100)` })),
      D.el("div.dash-two-col", null,
        D.el("div", null, D.el("span.dash-cell-muted", { text: "心情" }), rev.length > 1 ? D.sparkline(rev.map((ev, i) => ({ x: i, y: ev.valence_after })), { min: -1, max: 1, baseline: 0, height: 64 }) : D.el("p.dash-field-hint", { text: "—" })),
        D.el("div", null, D.el("span.dash-cell-muted", { text: "表达欲" }), rev.length > 1 ? D.sparkline(rev.map((ev, i) => ({ x: i, y: ev.arousal_after })), { min: 0, max: 1, baseline: 0.5, height: 64 }) : D.el("p.dash-field-hint", { text: "—" }))));

    const vInput = D.el("input.dash-select", { type: "range", min: "-1", max: "1", step: "0.05", value: String(st.valence) });
    const aInput = D.el("input.dash-select", { type: "range", min: "0", max: "1", step: "0.05", value: String(st.arousal) });
    const vOut = D.el("span.dash-cell-mono", { text: st.valence.toFixed(2) }), aOut = D.el("span.dash-cell-mono", { text: st.arousal.toFixed(2) });
    vInput.oninput = () => { vOut.textContent = Number(vInput.value).toFixed(2); };
    aInput.oninput = () => { aOut.textContent = Number(aInput.value).toFixed(2); };
    const reason = D.el("input.dash-select.dash-wide", { type: "text", placeholder: "理由(必填)" });
    const setBtn = D.el("button.dash-button.is-primary", { type: "button", text: "设值", onclick: async () => {
      if (!reason.value.trim()) { D.toast("写个理由", "error"); return; }
      try {
        await D.api(`/api/dash/affection/emotion?${scopeQuery()}`, { method: "PUT", body: { valence: Number(vInput.value), arousal: Number(aInput.value), reason: reason.value.trim() } });
        D.toast("已设值"); renderEmotion();
      } catch (error) { D.toast(`失败:${error.message}`, "error"); }
    } });
    const resetBtn = D.el("button.dash-button", { type: "button", text: "回到基线", onclick: async () => {
      if (!(await D.confirmAction("把心情与表达欲重置到基线?事件保留。", "重置"))) return;
      try { await D.api(`/api/dash/affection/emotion/reset?${scopeQuery()}`, { method: "POST", body: { clear_events: false } }); D.toast("已重置"); renderEmotion(); } catch (error) { D.toast(`失败:${error.message}`, "error"); }
    } });
    const clearBtn = D.el("button.dash-button.is-danger", { type: "button", text: "清空状态与事件", onclick: async () => {
      if (!(await D.confirmAction("删除这个账号 + 人格的情绪状态与全部事件?", "清空"))) return;
      try { await D.api(`/api/dash/affection/emotion/reset?${scopeQuery()}`, { method: "POST", body: { clear_events: true } }); D.toast("已清空"); renderEmotion(); } catch (error) { D.toast(`失败:${error.message}`, "error"); }
    } });
    const manual = D.el("div.u-card", null, D.el("div.u-card-head", null, D.el("h3", { text: "手动设值" }), D.el("span.u-hint", { text: "写一条 manual 事件,不受日限幅" })),
      D.el("div.dash-emo-form", null, D.el("label", null, D.el("span", { text: "心情" }), vInput, vOut), D.el("label", null, D.el("span", { text: "表达欲" }), aInput, aOut), reason),
      D.el("div.dash-actions", null, setBtn, resetBtn, D.el("span.dash-actions-gap"), clearBtn));

    const timeline = D.el("div.u-card", null, D.el("div.u-card-head", null, D.el("h3", { text: "事件" })),
      D.timeline(events.slice(0, 60).map((ev) => ({
        time: ts(ev.created_at),
        chip: `${ev.delta_valence >= 0 ? "+" : ""}${ev.delta_valence.toFixed(3)} / ${ev.delta_arousal >= 0 ? "+" : ""}${ev.delta_arousal.toFixed(3)}`,
        chipClass: ev.delta_valence > 0 ? "is-active" : (ev.delta_valence < 0 ? "is-danger" : ""),
        body: D.el("div", null,
          D.el("p.dash-timeline-body", { text: `${ev.label_before} → ${ev.label_after} · ${ev.reason || "(无理由)"}` }),
          D.el("small.dash-cell-muted", { text: `${EMO_SOURCE[ev.source] || ev.source}${ev.group_id ? ` · 群 ${ev.group_id}` : ""}${ev.message_id && ev.message_id !== "dashboard" ? ` · 消息 ${ev.message_id}` : ""}` }))
      })), "还没有情绪变化。"));

    ui.body.replaceChildren(...[
      e.enabled ? null : D.el("p.dash-banner", { text: "情绪功能在配置里是关闭的(emotion_enable=false):不会更新也不会影响判官与语气;下面是旧状态或空态。" }),
      stateCard, curves, manual, timeline].filter(Boolean));
  }

  D.register({ name: "affection", root: "dashAffectionRoot", mount, refresh: () => reloadAll() });
})();
