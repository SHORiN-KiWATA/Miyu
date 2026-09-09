/*
 * 赞助面板(09-09)。
 *
 * 统计卡(人民币折算总额 / 人数 / 笔数 / 最近一笔)→ 排行榜(前三上领奖台,
 * 其余压成一行一个的名次条,可切按金额 / 按笔数 / 按最近)→ 明细分页表
 * (改备注 / 删一笔)→ 右上「记一笔」抽屉。数据来自 /api/dash/sponsors/*。
 *
 * 金额一律用后端算好的 *_text 显示。这里不做除以 100 那步:舍入规则复制一份
 * 出来,两份迟早对不上账。JS 只在一个地方碰数字——人在表单里手打的那串,
 * 见 toMinor(),纯整数解析,不经过浮点。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const PAGE_SIZE = 50;
  const state = {
    overview: null,
    page: null,
    order: D.recall("sponsors.order") || "amount",
    filter: D.recall("sponsors.filter"),
    offset: 0,
    loadSeq: 0
  };
  const ui = {};

  const MEDALS = ["🥇", "🥈", "🥉"];
  const ORDERS = [{ value: "amount", label: "按金额" }, { value: "count", label: "按笔数" }, { value: "recent", label: "按最近" }];
  const UNCONVERTED_HINT = "记账时没有汇率,这笔不计入人民币榜;补个折算值就删掉重记一笔";

  const day = (value) => (value ? D.formatTime(value).slice(0, 10) : "—");
  const stop = (handler) => (event) => { event.stopPropagation(); handler(); };
  /* 没有头像端点(CSP 只放行同源图片,QQ 头像在腾讯 CDN 上),用名字首字顶上。 */
  const initial = (row) => (String(row.sponsor_name || row.sponsor_id || "?").trim()[0] || "?").toUpperCase();
  const avatar = (row) => D.el("span.dash-avatar", { text: initial(row), title: row.sponsor_name || row.sponsor_id });
  const currencyChips = (rows) => D.el("span.dash-sponsor-chips", null,
    (rows || []).map((entry) => D.el("span.dash-chip", { text: entry.text, title: `${entry.currency} 原始合计` })));
  /* 折算合计是 0 的人:他名下的笔一条都没折算过,这个 0 不是「赞助了 0 元」,
     跟明细里那笔一样得挂牌子,不然榜尾看起来像有人白嫖了一个名次。 */
  const cnyTotal = (row, cls) => (row.cny_minor > 0
    ? D.el(`strong.${cls}`, { text: row.cny_text })
    : D.el(`span.dash-chip.is-warn.${cls}`, { text: "未折算", title: UNCONVERTED_HINT }));

  /* 人手打的金额 → 分。整数进整数出,不走 Number("30.5")*100 那条会掉零头的路。 */
  function toMinor(text) {
    const cleaned = String(text ?? "").replace(/[,，¥$￥\s]/g, "");
    const matched = /^(\d{1,12})(?:[.．](\d{0,2}))?$/.exec(cleaned);
    if (!matched) return null;
    const minor = Number(matched[1]) * 100 + Number((matched[2] || "").padEnd(2, "0"));
    return minor > 0 ? minor : null;
  }

  /* ── 挂载 ─────────────────────────────────────────────── */
  function mount(root) {
    root.textContent = "";
    ui.stamp = D.el("small", { text: "" });
    const record = D.el("button.dash-button.is-primary", { type: "button", onclick: () => openCreate() });
    record.append(D.icon("plus"), "记一笔");
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "赞助" }),
      D.iconButton("refresh-cw", "刷新", () => load()),
      ui.stamp,
      D.el("span.dash-scope", null, record));
    ui.cards = D.el("div");
    ui.order = D.segmented(ORDERS, state.order, (value) => {
      state.order = value;
      D.remember("sponsors.order", value);
      load();
    });
    ui.board = D.el("div");
    ui.records = D.el("div");
    root.append(head, ui.cards, ui.board, ui.records);
    load();
  }

  async function load() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    try {
      const [overview, page] = await Promise.all([
        D.api(`/api/dash/sponsors/overview?order=${encodeURIComponent(state.order)}&limit=100`),
        fetchRecords()
      ]);
      if (seq !== state.loadSeq) return;
      state.overview = overview;
      state.page = page;
      renderCards();
      renderBoard();
      renderRecords();
      ui.stamp.textContent = `${overview.summary.sponsor_count} 人 · ${overview.summary.record_count} 笔`;
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.stamp.textContent = `加载失败:${error.message}`;
      ui.board.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
      ui.records.textContent = "";
    }
  }

  function fetchRecords() {
    const query = new URLSearchParams({ limit: String(PAGE_SIZE), offset: String(state.offset) });
    if (state.filter) query.set("sponsor_id", state.filter);
    return D.api(`/api/dash/sponsors/records?${query}`);
  }

  /* 明细单独翻页/换筛选时不必重拉概览。 */
  async function reloadRecords() {
    const seq = ++state.loadSeq;
    try {
      const page = await fetchRecords();
      if (seq !== state.loadSeq) return;
      state.page = page;
      renderRecords();
    } catch (error) {
      if (seq !== state.loadSeq) return;
      D.toast(`加载失败:${error.message}`, "error");
    }
  }

  /* ── 统计卡 ───────────────────────────────────────────── */
  function renderCards() {
    const summary = state.overview.summary;
    // 人民币是折算值,原始币种分列摆在副行:总额那个数是怎么来的,得看得见。
    const raw = summary.by_currency.map((entry) => entry.text).join(" · ") || "还没有记录";
    ui.cards.replaceChildren(D.statCards([
      { label: "总赞助额", value: summary.cny_text, hint: `人民币折算 · ${raw}` },
      { label: "赞助人数", value: summary.sponsor_count, hint: "按 QQ 号去重" },
      { label: "笔数", value: summary.record_count, hint: "含未折算的笔" },
      { label: "最近一笔", value: day(summary.last_sponsored_at), hint: summary.last_sponsored_at ? D.formatTime(summary.last_sponsored_at) : "还没有记录" }
    ]));
  }

  /* ── 排行榜 ───────────────────────────────────────────── */
  function renderBoard() {
    const rows = state.overview.leaderboard || [];
    const toolbar = D.el("div.dash-toolbar", null, ui.order.el);
    const card = D.el("div.u-card", null,
      D.el("div.u-card-head", null,
        D.el("h3", { text: "排行榜" }),
        D.el("span.u-hint", { text: "名次按记账当刻折算的人民币;未折算的笔不进这个数" })));
    if (!rows.length) {
      card.append(D.el("p.dash-empty", { text: "还没有赞助记录。点右上角「记一笔」记下第一笔。" }));
      ui.board.replaceChildren(toolbar, card);
      return;
    }
    const top = rows.slice(0, 3);
    const rest = rows.slice(3);
    card.append(D.el("div.dash-sponsor-podium", null, top.map((row, index) => podium(row, index))));
    if (rest.length) {
      // 条长按榜尾这一段自己的头名算。拿全榜第一当尺,冠军一个人就能把余下
      // 所有人压成同一根看不出差别的线(实测 12345 : 118)。
      const peak = Math.max(1, ...rest.map((row) => row.cny_minor));
      card.append(D.el("div.dash-sponsor-list", null, rest.map((row) => rankRow(row, peak))));
    }
    ui.board.replaceChildren(toolbar, card);
  }

  function podium(row, index) {
    const item = D.el(`div.dash-sponsor-podium-item.is-${index + 1}`, {
      role: "button", tabindex: "0",
      title: `只看 ${row.sponsor_name || row.sponsor_id} 的明细`,
      onclick: () => filterBy(row.sponsor_id),
      onkeydown: (event) => { if (event.key === "Enter") filterBy(row.sponsor_id); }
    },
      D.el("div.dash-sponsor-head", null,
        D.el("span.dash-sponsor-medal", { text: MEDALS[index] }),
        avatar(row),
        D.el("span.dash-sponsor-name", { text: row.sponsor_name || row.sponsor_id, title: row.sponsor_name || row.sponsor_id })),
      cnyTotal(row, "dash-sponsor-amount"),
      currencyChips(row.by_currency),
      D.el("div.dash-sponsor-sub", null,
        D.el("span.dash-sponsor-cell", { text: row.sponsor_id }),
        D.el("span", { text: `${row.record_count} 笔` }),
        D.el("span", { text: day(row.last_sponsored_at) })));
    return item;
  }

  function rankRow(row, peak) {
    const fill = D.el("i.dash-hbar-fill");
    fill.style.width = row.cny_minor > 0 ? `${Math.max(2, Math.round((row.cny_minor / peak) * 100))}%` : "0";
    return D.el("div.dash-sponsor-row", {
      role: "button", tabindex: "0",
      title: `只看 ${row.sponsor_name || row.sponsor_id} 的明细`,
      onclick: () => filterBy(row.sponsor_id),
      onkeydown: (event) => { if (event.key === "Enter") filterBy(row.sponsor_id); }
    },
      D.el("span.dash-sponsor-rank", { text: `#${row.rank}` }),
      avatar(row),
      D.el("span.dash-sponsor-who", null,
        D.el("span.dash-sponsor-cell", { text: row.sponsor_name || row.sponsor_id, title: row.sponsor_name || row.sponsor_id }),
        D.el("small.dash-cell-muted.dash-sponsor-cell", { text: row.sponsor_id })),
      D.el("span.dash-hbar-track", null, fill),
      cnyTotal(row, "dash-sponsor-total"),
      D.el("span.dash-sponsor-cell.dash-cell-muted", { text: `${row.record_count} 笔` }),
      D.el("span.dash-sponsor-cell.dash-cell-muted", { text: day(row.last_sponsored_at) }));
  }

  function filterBy(sponsorId) {
    state.filter = state.filter === sponsorId ? "" : sponsorId;
    state.offset = 0;
    D.remember("sponsors.filter", state.filter);
    reloadRecords();
  }

  /* ── 明细 ─────────────────────────────────────────────── */
  function renderRecords() {
    const page = state.page;
    const records = page.records || [];
    ui.records.textContent = "";
    const headRow = D.el("div.dash-toolbar", null, D.el("h3.dash-section", { text: "明细" }));
    if (state.filter) {
      headRow.append(D.el("span.dash-chip.is-clickable.is-builtin", {
        text: `只看 ${state.filter} ✕`, title: "点掉筛选,看全部",
        onclick: () => filterBy(state.filter)
      }));
    }
    ui.records.append(headRow);
    if (!records.length) {
      ui.records.append(D.el("p.dash-empty", { text: state.filter ? "这个人名下没有记录。" : "还没有赞助记录。" }));
      return;
    }
    const grid = D.table([
      { label: "日期", width: "112px" },
      { label: "赞助人", width: "minmax(150px, 1.6fr)" },
      { label: "金额", width: "minmax(110px, 1fr)" },
      { label: "人民币", width: "minmax(110px, 1fr)" },
      { label: "备注", width: "minmax(180px, 2fr)" },
      { label: "", width: "84px" }
    ]);
    for (const record of records) grid.append(recordRow(record));
    // 表自己横向滚,不让整页跟着横着跑(手机上尤其明显)。
    ui.records.append(D.el("div.dash-table-wrap", null, grid), pager());
  }

  function recordRow(record) {
    const name = record.sponsor_name || record.sponsor_id;
    // 未折算的那笔不能显示成 ¥0.00——那会读成"他赞助了 0 元"。
    const cny = record.converted
      ? D.el("span.dash-cell-mono", { text: record.cny_text })
      : D.el("span.dash-chip.is-warn", { text: "未折算", title: UNCONVERTED_HINT });
    return D.el("div.dash-row", { role: "row" },
      D.el("span.dash-cell-muted", { text: day(record.sponsored_at), title: D.formatTime(record.sponsored_at) }),
      D.el("span.dash-cell-main", null,
        D.el("strong.dash-sponsor-cell", { text: name, title: name }),
        D.el("small.dash-cell-muted", { text: record.sponsor_id })),
      D.el("span", null,
        D.el("span.dash-cell-mono", { text: record.amount_text }),
        record.currency === "CNY" ? null : D.el("span.dash-chip", { text: record.currency })),
      D.el("span", null, cny),
      // 备注套一层再省略号:格子本身是 .dash-row > span 的 flex,text-overflow
      // 对 flex 容器的匿名子项不生效,只会硬切一刀。
      D.el("span.dash-cell-muted", { title: record.note },
        D.el("span.dash-sponsor-cell", { text: record.note || "—" })),
      D.el("span.dash-cell-actions", null,
        D.iconButton("pencil", "编辑备注", stop(() => openEdit(record))),
        D.iconButton("trash-2", "删除", stop(() => remove(record)), "is-danger")));
  }

  /* 存储层按 LIMIT n+1 判断有没有下一页,所以这里只有 has_more 没有总数。 */
  function pager() {
    const page = state.page;
    const start = page.offset + 1;
    const end = page.offset + page.records.length;
    const bar = D.el("div.dash-pager");
    const prev = D.iconButton("chevron-left", "上一页", () => { state.offset = Math.max(0, state.offset - PAGE_SIZE); reloadRecords(); });
    const next = D.iconButton("chevron-right", "下一页", () => { state.offset += PAGE_SIZE; reloadRecords(); });
    prev.disabled = page.offset <= 0;
    next.disabled = !page.has_more;
    bar.append(D.el("span.dash-pager-text", { text: `第 ${start}–${end} 条${page.has_more ? "" : " · 到底了"}` }), prev, next);
    return bar;
  }

  /* ── 记一笔 ───────────────────────────────────────────── */
  function openCreate() {
    const form = {
      sponsor: D.el("input.dash-input", { type: "text", placeholder: "QQ 号,例如 10001" }),
      name: D.el("input.dash-input", { type: "text", placeholder: "留空就用上一次记的名字" }),
      amount: D.el("input.dash-input", { type: "text", inputmode: "decimal", placeholder: "30 或 9.99" }),
      currency: D.select([{ value: "CNY", label: "人民币 CNY" }, { value: "USD", label: "美元 USD" }], "CNY", (value) => {
        form.cnyField.hidden = value !== "USD";
      }),
      cny: D.el("input.dash-input", { type: "text", inputmode: "decimal", placeholder: "留空则这笔不计入人民币榜" }),
      note: D.el("textarea.dash-textarea", { rows: "3", placeholder: "这笔是为了什么,最多 200 字" })
    };
    form.cnyField = D.field("人民币折算(可选)", form.cny, "面板不联网取汇率:填了就按这个数进榜,不填这笔挂在榜外");
    form.cnyField.hidden = true;
    const body = D.el("div", null,
      D.field("QQ 号", form.sponsor),
      D.field("显示名", form.name),
      D.el("div.dash-field-row", null, D.field("金额", form.amount), D.field("币种", form.currency)),
      form.cnyField,
      D.field("备注", form.note));
    const submit = D.el("button.dash-button.is-primary", { type: "button", text: "记下", onclick: async () => {
      const sponsorId = form.sponsor.value.trim();
      if (!sponsorId) { D.toast("QQ 号不能为空", "error"); return; }
      const amountMinor = toMinor(form.amount.value);
      if (!amountMinor) { D.toast("金额要是正数,最多两位小数", "error"); return; }
      const payload = {
        sponsor_id: sponsorId,
        sponsor_name: form.name.value.trim(),
        amount_minor: amountMinor,
        currency: form.currency.value,
        note: form.note.value.trim()
      };
      if (form.currency.value === "USD" && form.cny.value.trim()) {
        const cnyMinor = toMinor(form.cny.value);
        if (!cnyMinor) { D.toast("人民币折算要是正数,最多两位小数", "error"); return; }
        payload.cny_minor = cnyMinor;
      }
      try {
        await D.api("/api/dash/sponsors/records", { method: "POST", body: payload });
        D.toast("已记下");
        D.closeDrawer();
        state.offset = 0;
        await load();
      } catch (error) {
        D.toast(`记账失败:${error.message}`, "error");
      }
    } });
    D.openDrawer("记一笔", body, [submit]);
  }

  /* ── 改备注 / 删一笔 ─────────────────────────────────── */
  function openEdit(record) {
    const form = {
      name: D.el("input.dash-input", { type: "text", value: record.sponsor_name }),
      note: D.el("textarea.dash-textarea", { rows: "4" })
    };
    form.note.value = record.note || "";
    const body = D.el("div", null,
      D.el("p.dash-drawer-desc", { text: `${record.amount_text}${record.currency === "CNY" ? "" : ` ${record.currency}`} · ${D.formatTime(record.sponsored_at)}` }),
      record.converted ? null : D.el("p.dash-banner", { text: UNCONVERTED_HINT }),
      D.el("dl.dash-meta", null, [
        ["记录号", record.record_id], ["QQ 号", record.sponsor_id],
        ["人民币", record.converted ? record.cny_text : "未折算"],
        ["汇率", record.converted ? (record.fx_source ? `${record.fx_rate.toFixed(4)}(${record.fx_source})` : "无需折算") : "记账时没拉到"],
        ["记账人", record.recorded_by || "—"]
      ].flatMap(([key, value]) => [D.el("dt", { text: key }), D.el("dd", { text: String(value) })])),
      D.field("显示名", form.name, "改这一笔留下的名字;榜上显示的是最近一笔的名字"),
      D.field("备注", form.note));
    const save = D.el("button.dash-button.is-primary", { type: "button", text: "保存", onclick: async () => {
      try {
        await D.api(`/api/dash/sponsors/records/${record.record_id}`, {
          method: "PATCH", body: { note: form.note.value.trim(), sponsor_name: form.name.value.trim() }
        });
        D.toast("已保存");
        D.closeDrawer();
        await load();
      } catch (error) {
        D.toast(`保存失败:${error.message}`, "error");
      }
    } });
    const drop = D.el("button.dash-button.is-danger", { type: "button", text: "删除", onclick: () => remove(record) });
    // 金额与汇率是记账当刻冻结的事实,面板不给改:记错了就删了重记。
    D.openDrawer(record.sponsor_name || record.sponsor_id, body, [drop, save]);
  }

  async function remove(record) {
    const name = record.sponsor_name || record.sponsor_id;
    if (!(await D.confirmAction(`删除 ${name} 的这笔 ${record.amount_text}?总额与名次会跟着变,不可撤销。`))) return;
    try {
      await D.api(`/api/dash/sponsors/records/${record.record_id}`, { method: "DELETE" });
      D.toast("已删除");
      D.closeDrawer();
      await load();
    } catch (error) {
      D.toast(`删除失败:${error.message}`, "error");
    }
  }

  D.register({ name: "sponsors", root: "dashSponsorsRoot", mount, refresh: () => load() });
})();
