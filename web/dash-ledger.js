/*
 * 记账面板(09-09)。
 *
 * 顶栏(账本 / 月份)固定,下面是五个视图:流水、账户、分类、预算、数据。
 * 数据全来自 /api/dash/ledger/*,写操作与 ledger 工具同一套后端逻辑。
 *
 * 视图分开而不是把配置一股脑塞进一个「管理」抽屉:账户、分类、预算是三种
 * 不同的东西,摞在一个抽屉里没人找得到(09-09 用户走查意见)。后四个视图在
 * dash-ledger-manage.js 里注册进 window.MiyuLedgerViews。
 *
 * 三条与别的面板不同的约定:
 * - 金额永远带 +/− 符号,不只靠红绿区分(色盲友好)。
 * - 外币条目并排显示原币金额与换算结果;取不到汇率的标「待换算」,
 *   并且明说它没进合计——统计宁可少算也不拿猜的汇率凑数。
 * - 数字用等宽字体右对齐,一列钱能竖着比大小。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = {
    book: D.recall("ledger.book") || "",
    period: "",
    view: D.recall("ledger.view") || "entries",
    kind: "all",
    q: "",
    offset: 0,
    overview: null,
    entries: [],
    total: 0,
    loadSeq: 0,
  };
  const ui = {};

  const VIEWS = [
    { value: "entries", label: "流水" },
    { value: "accounts", label: "账户" },
    { value: "categories", label: "分类" },
    { value: "budgets", label: "预算" },
    { value: "data", label: "数据" },
  ];
  const KIND_LABEL = { expense: "支出", income: "收入", transfer: "转账" };

  /* 记外币时最常用的几个。账本自己的币种由 currencyOptions 顶到最前。 */
  const COMMON_CURRENCIES = ["CNY", "JPY", "USD", "EUR", "HKD", "GBP", "KRW", "TWD"];

  /* ── 小工具 ─────────────────────────────────────────────── */

  const pad = (n) => String(n).padStart(2, "0");
  const monthOf = (date) => `${date.getFullYear()}-${pad(date.getMonth() + 1)}`;
  const thisMonth = () => monthOf(new Date());

  const currency = () => state.overview?.book?.currency || "CNY";
  const accounts = () => state.overview?.accounts || [];
  const categories = () => state.overview?.categories || [];

  function currencyOptions() {
    const seen = [currency(), ...accounts().map((a) => a.currency), ...COMMON_CURRENCIES];
    const out = [];
    for (const code of seen) {
      if (code && !out.includes(code)) out.push(code);
    }
    return out.map((value) => ({ value, label: value }));
  }

  /* 金额的显示形态:符号 + 数字。符号是第二个维度,颜色失效时还认得出方向。 */
  function signed(kind, text) {
    if (kind === "income") return `+${text}`;
    if (kind === "expense") return `−${text}`;
    return text;
  }

  function amountCell(entry) {
    const wrap = D.el("span.dash-led-amount");
    const primary = entry.rate_status === "pending" ? entry.amount_text : entry.base_amount_text;
    wrap.append(D.el(`strong.dash-led-money.is-${entry.kind}`, {
      text: signed(entry.kind, primary ?? entry.amount_text),
    }));
    // 外币账并排显示原币,让人看得见「实际花的是三千日元」。
    if (entry.currency !== entry.base_currency) {
      wrap.append(D.el("small.dash-led-origin", {
        text: entry.rate_status === "pending"
          ? `${entry.amount_text} ${entry.currency} · 待换算`
          : `${entry.amount_text} ${entry.currency}`,
      }));
    }
    return wrap;
  }

  /* ── 挂载 ───────────────────────────────────────────────── */

  function mount(root) {
    root.textContent = "";
    state.period = state.period || thisMonth();

    ui.books = D.select([], state.book, (value) => {
      state.book = value;
      D.remember("ledger.book", value);
      state.offset = 0;
      load();
    }, "账本");

    // 月份只列真的有账目的月份(加上当月),列表由后端给,所以这里先摆一个
    // 当前值占位,拿到 overview 再填。
    ui.month = D.select([{ value: state.period, label: state.period }], state.period, setPeriod, "月份");

    const head = D.el("div.con-head", null,
      D.el("h2", { text: "记账" }),
      D.iconButton("refresh-cw", "刷新", () => load()),
      D.el("span.dash-scope", null,
        D.el("span.dash-scope-label", { text: "账本" }), ui.books,
        D.el("span.dash-scope-label", { text: "月份" }), ui.month));

    ui.tabs = D.segmented(VIEWS, state.view, (value) => {
      state.view = value;
      D.remember("ledger.view", value);
      renderView();
    });
    ui.body = D.el("div");

    root.append(head, D.el("div.dash-led-tabs", null, ui.tabs.el), ui.body);
    load();
  }

  function setPeriod(value) {
    state.period = value;
    state.offset = 0;
    load();
  }

  /* ── 取数 ───────────────────────────────────────────────── */

  function query() {
    const parts = [`period=${encodeURIComponent(state.period)}`];
    if (state.book) parts.push(`book=${encodeURIComponent(state.book)}`);
    return parts.join("&");
  }

  async function load() {
    const seq = ++state.loadSeq;
    try {
      const overview = await D.api(`/api/dash/ledger/overview?${query()}`);
      if (seq !== state.loadSeq) return;
      state.overview = overview;
      // 记住的账本可能已经不在了(删掉了、换了机器、换了一份数据)。后端
      // 这时会回退到第一本并标出来——把记忆清掉并说一声,别让人以为自己
      // 看的还是原来那本账。
      if (overview.requested_book_missing) {
        D.remember("ledger.book", "");
        D.toast(`原来选的账本不在了，已切到「${overview.book.name}」`);
      }
      // 后端也可能替我们建了默认账本,把选中项对回去。
      state.book = overview.book.id;
      renderBooks();
      renderMonths();
      renderView(seq);
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.body.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
    }
  }

  /* 给别的视图用的上下文:它们只读状态、调 reload,不直接碰 ui。 */
  function context() {
    return {
      D,
      state,
      currency,
      accounts,
      categories,
      currencyOptions,
      reload: load,
      query,
    };
  }

  function renderView(seq = state.loadSeq) {
    ui.tabs.set(state.view);
    if (!state.overview) return;
    if (state.view === "entries") {
      renderEntriesView(seq);
      return;
    }
    const views = window.MiyuLedgerViews;
    const render = views && views[state.view];
    ui.body.replaceChildren(
      render ? render(context()) : D.el("p.dash-empty", { text: "这个视图没能加载" })
    );
  }

  /* ── 流水视图 ───────────────────────────────────────────── */

  function renderEntriesView(seq) {
    ui.cards = D.el("div");
    ui.banner = D.el("div");
    ui.charts = D.el("div");
    ui.list = D.el("div");
    ui.pager = D.el("div");

    ui.search = D.el("input.dash-search", {
      type: "search",
      placeholder: "按备注筛选…",
      value: state.q,
      oninput: () => {
        clearTimeout(ui.searchTimer);
        ui.searchTimer = setTimeout(() => {
          state.q = ui.search.value.trim();
          state.offset = 0;
          loadEntries();
        }, 250);
      },
    });
    ui.filter = D.segmented(
      [
        { value: "all", label: "全部" },
        { value: "expense", label: "支出" },
        { value: "income", label: "收入" },
        { value: "transfer", label: "转账" },
      ],
      state.kind,
      (value) => { state.kind = value; state.offset = 0; loadEntries(); }
    );

    const toolbar = D.el("div.dash-toolbar", null,
      ui.filter.el,
      D.el("label.dash-search-box", null, D.icon("search"), ui.search),
      D.el("span.dash-actions-gap"),
      D.el("button.dash-button.is-primary", { type: "button", onclick: () => openEntryForm() },
        D.icon("plus"), "记一笔"));

    ui.body.replaceChildren(ui.cards, ui.banner, ui.charts, toolbar, ui.list, ui.pager);
    renderCards();
    renderBanner();
    renderCharts();
    loadEntries(seq);
  }

  async function loadEntries(seq = state.loadSeq) {
    const parts = [query(), `offset=${state.offset}`];
    if (state.kind !== "all") parts.push(`kind=${state.kind}`);
    if (state.q) parts.push(`q=${encodeURIComponent(state.q)}`);
    try {
      const page = await D.api(`/api/dash/ledger/entries?${parts.join("&")}`);
      if (seq !== state.loadSeq || state.view !== "entries") return;
      state.entries = page.entries;
      state.total = page.total;
      renderList();
      ui.pager.replaceChildren(D.pager({
        offset: page.offset,
        limit: page.limit,
        total: page.total,
        onChange: (offset) => { state.offset = offset; loadEntries(); },
      }));
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.list.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
    }
  }

  function renderBooks() {
    ui.books.replaceChildren();
    for (const book of state.overview.books || []) {
      ui.books.append(D.el("option", { value: book.id, text: `${book.name} · ${book.currency}` }));
    }
    ui.books.value = state.book;
  }

  /* 月份选项来自后端:只有真的记过账的月份,外加当月。 */
  function renderMonths() {
    const periods = state.overview.periods || [state.period];
    ui.month.replaceChildren();
    for (const period of periods) {
      ui.month.append(D.el("option", { value: period, text: period }));
    }
    ui.month.value = state.period;
  }

  function renderCards() {
    const summary = state.overview.summary;
    const unit = currency();
    const total = (state.overview.budgets || []).find((budget) => budget.scope === "total");
    const cards = [
      /* 笔数只数支出笔:这行字挂在「本月支出」底下,用总笔数会让一个月记了
         几笔收入或转账之后,支出旁边出现一个更大的数。 */
      { label: `本月支出 (${unit})`, value: summary.expense_text, hint: `${summary.expense_entries} 笔` },
      {
        label: `本月收入 (${unit})`,
        value: summary.income_text,
        hint: summary.income_entries ? `${summary.income_entries} 笔` : undefined,
      },
    ];

    /* 定了预算就听预算的。只记支出的账本里「收入 − 支出」永远是支出的
       负数镜像,那一格屏幕换不来任何信息;这时候真正想知道的是额度还剩
       多少。没设预算才退回净额——那时它至少还说明收支谁大。 */
    if (total) {
      const over = total.left_minor < 0;
      const left = total.limit_minor > 0
        ? Math.round((total.left_minor / total.limit_minor) * 100)
        : 0;
      cards.push({
        label: over ? "预算超支" : "预算余额",
        value: `${over ? "−" : ""}${total.left_text}`,
        hint: over ? `超出额度 ${-left}%` : `还剩 ${left}%`,
      });
    } else {
      cards.push({
        label: "本月净额",
        value: `${summary.net_negative ? "−" : "+"}${summary.net_text}`,
        hint: summary.net_negative ? "本月支出多于收入" : undefined,
      });
    }

    if (total) {
      const percent = total.limit_minor > 0
        ? Math.round((total.used_minor / total.limit_minor) * 100)
        : 0;
      cards.push({ label: "总预算", value: `${percent}%`, hint: `${total.used_text} / ${total.limit_text}` });
    }
    ui.cards.replaceChildren(D.statCards(cards));
  }

  function renderBanner() {
    const pending = state.overview.summary.pending;
    if (!pending) { ui.banner.replaceChildren(); return; }
    // 有账没算进合计时必须说出来,否则上面那几个数字是在骗人。
    ui.banner.replaceChildren(
      D.el("div.dash-banner", null,
        D.el("span", { text: `有 ${pending} 笔外币账没取到汇率,未计入本月合计。` }),
        D.el("button.dash-button", {
          type: "button",
          text: "重新换算",
          onclick: async () => {
            try {
              const result = await D.api(`/api/dash/ledger/backfill-rates?${query()}`, { method: "POST" });
              D.toast(result.filled ? `补算了 ${result.filled} 笔` : "汇率仍然取不到");
              load();
            } catch (error) { D.toast(error.message, "error"); }
          },
        })));
  }

  function renderCharts() {
    const totals = state.overview.expense_categories || [];
    const max = totals.reduce((peak, item) => Math.max(peak, item.amount_minor), 0);
    const bars = totals.length
      ? D.el("div.dash-hbars", null, ...totals.slice(0, 8).map((item) => {
          const fill = D.el("i.dash-hbar-fill");
          fill.style.setProperty("width", max ? `${Math.max(2, (item.amount_minor / max) * 100)}%` : "0%");
          return D.el("div.dash-hbar.dash-led-hbar", null,
            D.el("span.dash-hbar-label", { text: `${item.icon || ""} ${item.name}`.trim() }),
            D.el("span.dash-hbar-track", null, fill),
            D.el("span.dash-cell-mono", { text: item.amount_text }));
        }))
      : D.el("p.dash-empty", { text: "本月还没有支出" });

    const daily = state.overview.daily || [];
    const points = daily.map((day) => ({ x: Number(day.day.slice(8, 10)), y: day.expense_minor / 100 }));
    const trend = points.length >= 2
      ? D.sparkline(points, { width: 320, height: 72, min: 0 })
      : D.el("p.dash-empty", { text: "还不够画出趋势" });

    ui.charts.replaceChildren(
      D.el("div.dash-two-col.dash-led-charts", null,
        D.el("section.dash-led-block.dash-led-chart", null,
          D.el("h3.dash-section", { text: "支出分类" }), bars),
        D.el("section.dash-led-block.dash-led-chart", null,
          D.el("h3.dash-section", { text: "每日支出" }), trend)));
  }

  function renderList() {
    if (!state.entries.length) {
      ui.list.replaceChildren(D.el("p.dash-empty", { text: "这段时间还没有账目" }));
      return;
    }
    const grid = D.table([
      { label: "日期", width: "72px" },
      { label: "分类", width: "minmax(96px, 1fr)" },
      { label: "备注", width: "minmax(120px, 1.6fr)" },
      { label: "金额", width: "minmax(120px, 0.8fr)" },
    ]);
    grid.classList.add("dash-led-table");
    for (const entry of state.entries) {
      const open = () => openEntryForm(entry);
      grid.append(D.el("div.dash-row", {
        role: "row",
        tabindex: "0",
        onclick: open,
        onkeydown: (event) => { if (event.key === "Enter") open(); },
      },
        D.el("span.dash-cell-muted", { text: entry.date.slice(5) }),
        D.el("span.dash-cell-main", { text: entry.category || KIND_LABEL[entry.kind] || "" }),
        D.el("span.dash-cell-muted", { text: entry.note || entry.merchant || "" }),
        amountCell(entry)));
    }
    ui.list.replaceChildren(D.el("div.dash-table-wrap", null, grid));
  }

  /* ── 记一笔 / 编辑 ──────────────────────────────────────── */

  function categoryOptions(direction) {
    const all = categories();
    const roots = all.filter((item) => item.direction === direction && !item.parent_id);
    const options = [{ value: "", label: "未分类" }];
    for (const root of roots) {
      options.push({ value: root.id, label: `${root.icon || ""} ${root.name}`.trim() });
      for (const child of all.filter((item) => item.parent_id === root.id)) {
        options.push({ value: child.id, label: `　${root.name}/${child.name}` });
      }
    }
    return options;
  }

  function accountOptions(includeEmpty = true) {
    const options = includeEmpty ? [{ value: "", label: "不指定" }] : [];
    for (const account of accounts()) {
      options.push({ value: account.id, label: `${account.name} · ${account.balance_text} ${account.currency}` });
    }
    return options;
  }

  function openEntryForm(entry) {
    const editing = Boolean(entry);
    const form = { kind: entry?.kind || "expense" };
    const body = D.el("div.dash-led-form");
    const hasAccounts = accounts().length > 0;

    const kindPicker = D.segmented(
      [
        { value: "expense", label: "支出" },
        { value: "income", label: "收入" },
        { value: "transfer", label: "转账" },
      ],
      form.kind,
      (value) => { form.kind = value; rebuild(); }
    );

    const amount = D.el("input.dash-input", {
      type: "text",
      inputmode: "decimal",
      placeholder: "35.50",
      value: entry?.amount_text || "",
    });
    // 币种做成下拉:手输 ISO 码是纯粹的出错机会,而常用的就那么几个。
    const currencyPicker = D.select(currencyOptions(), entry?.currency || currency(), () => {}, "币种");
    const note = D.el("input.dash-input", {
      type: "text",
      placeholder: "买了什么、在哪买的",
      value: entry?.note || "",
    });
    const date = D.el("input.dash-input", {
      type: "date",
      value: entry?.date || new Date().toISOString().slice(0, 10),
    });

    let category = null;
    let account = null;
    let toAccount = null;
    const fields = D.el("div");

    function rebuild() {
      fields.replaceChildren();
      if (form.kind === "transfer") {
        if (!hasAccounts) {
          fields.append(D.el("p.dash-empty", { text: "转账要先在「账户」里建两个账户。" }));
          account = null;
          toAccount = null;
          return;
        }
        account = D.select(accountOptions(false), entry?.account_id || "", () => {}, "转出账户");
        toAccount = D.select(accountOptions(false), entry?.to_account_id || "", () => {}, "转入账户");
        fields.append(D.field("从", account), D.field("到", toAccount));
        return;
      }
      const direction = form.kind === "income" ? "income" : "expense";
      category = D.select(categoryOptions(direction), entry?.category_id || "", () => {}, "分类");
      fields.append(D.field("分类", category));
      // 一个账户都没有时干脆不显示这一栏:只想记花了多少的人,不该先被问
      // 「这笔钱从哪个账户出的」。
      if (hasAccounts) {
        account = D.select(accountOptions(), entry?.account_id || "", () => {}, "账户");
        fields.append(D.field("账户", account, "从哪个账户付的,可以留空"));
      } else {
        account = null;
      }
    }
    rebuild();

    body.append(
      editing ? null : D.field("类型", kindPicker.el),
      D.el("div.dash-field-row", null,
        D.field("金额", amount),
        D.el("label.dash-field.dash-led-currency", null,
          D.el("span.dash-field-label", { text: "币种" }), currencyPicker)),
      fields,
      D.field("备注", note),
      D.field("日期", date)
    );

    const save = D.el("button.dash-button.is-primary", {
      type: "button",
      text: editing ? "保存" : "记下",
      onclick: async () => {
        save.disabled = true;
        try {
          if (editing) {
            await D.api(`/api/dash/ledger/entries/${encodeURIComponent(entry.id)}`, {
              method: "PATCH",
              body: {
                revision: entry.revision,
                amount: amount.value.trim(),
                category: category ? category.value : undefined,
                note: note.value,
                date: date.value,
              },
            });
            D.toast("已保存");
          } else {
            const payload = {
              book: state.book,
              kind: form.kind,
              amount: amount.value.trim(),
              currency: currencyPicker.value,
              note: note.value,
              date: date.value,
            };
            if (form.kind === "transfer") {
              if (!account || !toAccount) { D.toast("转账要先建账户", "error"); save.disabled = false; return; }
              payload.account = account.value;
              payload.to_account = toAccount.value;
            } else {
              payload.category = category.value;
              if (account) payload.account = account.value;
            }
            const result = await D.api("/api/dash/ledger/entries", { method: "POST", body: payload });
            // 重复闸挡下来时给人一次确认的机会,而不是默默记两笔。
            if (result.ok === false && result.reason === "possible_duplicate") {
              const again = await D.confirmAction(
                `几分钟前记过一笔一样的(${result.existing.amount_text})。仍然要记吗?`,
                "仍然记下"
              );
              if (!again) { save.disabled = false; return; }
              await D.api("/api/dash/ledger/entries", { method: "POST", body: { ...payload, force: true } });
            }
            const budget = result.budget;
            D.toast(budget && budget.state === "exceeded"
              ? `已记下 · ${budget.scope} 已超预算 ${budget.used_text}/${budget.limit_text}`
              : "已记下");
          }
          D.closeDrawer();
          load();
        } catch (error) {
          D.toast(error.message, "error");
          save.disabled = false;
        }
      },
    });

    const actions = [save];
    if (editing) {
      actions.unshift(D.el("button.dash-button.is-danger", {
        type: "button",
        text: "删除",
        onclick: async () => {
          if (!(await D.confirmAction("删除这笔账?"))) return;
          try {
            await D.api(`/api/dash/ledger/entries/${encodeURIComponent(entry.id)}`, { method: "DELETE" });
            D.toast("已删除");
            D.closeDrawer();
            load();
          } catch (error) { D.toast(error.message, "error"); }
        },
      }));
    }
    D.openDrawer(editing ? "编辑账目" : "记一笔", body, actions);
  }

  D.register({ name: "ledger", root: "dashLedgerRoot", mount, refresh: () => load() });
})();
