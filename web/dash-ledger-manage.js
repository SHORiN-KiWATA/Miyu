/*
 * 记账面板的后四个视图:账户、分类、预算、数据(09-09)。
 *
 * 拆出来一是主文件已经够长,二是这四块与流水的读写节奏完全不同——它们
 * 是「配置」,一次设好很久不动。
 *
 * 每个视图的形状一致:说明一句 → 列表 → 一个新建表单(就地展开,不再套
 * 抽屉)。原来这四块全塞在一个「管理」抽屉里,谁都找不到自己要的东西。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const ACCOUNT_KIND_LABEL = {
    cash: "现金",
    bank: "银行卡",
    ewallet: "电子钱包",
    credit: "信用卡",
    other: "其他",
  };

  /*
   * 就地展开的新建表单。返回按钮与表单两截:按钮挂在抬头右边,表单落在
   * 抬头下方通栏展开——挤在右上角那一小条里既窄又扎眼。
   */
  function inlineForm({ label, fields, submit }) {
    const form = D.el("div.dash-led-form.dash-led-inline-form", { hidden: true });
    for (const field of fields) form.append(field.node);

    const close = () => { form.hidden = true; toggle.hidden = false; };
    const save = D.el("button.dash-button.is-primary", {
      type: "button",
      text: "建立",
      onclick: async () => {
        save.disabled = true;
        try {
          await submit();
        } finally {
          save.disabled = false;
        }
      },
    });
    const toggle = D.el("button.dash-button.is-primary", {
      type: "button",
      onclick: () => {
        form.hidden = false;
        toggle.hidden = true;
        fields[0]?.node.querySelector("input,select")?.focus();
      },
    });
    toggle.append(D.icon("plus"), label);
    const cancel = D.el("button.dash-button", { type: "button", text: "取消", onclick: close });
    form.append(D.el("div.dash-actions", null, save, cancel));
    return { toggle, form };
  }

  function head(title, hint, action) {
    return D.el("div.dash-led-head", null,
      D.el("div", null,
        D.el("h3.dash-section", { text: title }),
        hint ? D.el("p.dash-led-hint", { text: hint }) : null),
      action || null);
  }

  /* ── 账户 ───────────────────────────────────────────────── */

  function accountsView(ctx) {
    const { state, accounts, currencyOptions, reload } = ctx;
    const root = D.el("div");

    const name = D.el("input.dash-input", { type: "text", placeholder: "微信" });
    const kind = D.select(
      Object.entries(ACCOUNT_KIND_LABEL).map(([value, label]) => ({ value, label })),
      "ewallet", () => {}, "类型");
    const currencyPicker = D.select(currencyOptions(), ctx.currency(), () => {}, "币种");
    const opening = D.el("input.dash-input", { type: "text", inputmode: "decimal", placeholder: "0" });

    const { toggle, form } = inlineForm({
      label: "新建账户",
      fields: [
        { node: D.field("名称", name) },
        { node: D.field("类型", kind) },
        { node: D.field("币种", currencyPicker, "与账本币种不同时才需要改") },
        { node: D.field("当前余额", opening, "可留空;之后的收支会在它上面加减") },
      ],
      submit: async () => {
        try {
          await D.api("/api/dash/ledger/accounts", {
            method: "POST",
            body: {
              book: state.book,
              name: name.value.trim(),
              kind: kind.value,
              currency: currencyPicker.value,
              opening_balance: opening.value.trim(),
            },
          });
          D.toast("已建立");
          reload();
        } catch (error) { D.toast(error.message, "error"); }
      },
    });

    root.append(head(
      "账户",
      "账户回答的是「这笔钱从哪付的」。只想记花了多少的话，一个都不用建——记账时这一栏会自动消失。",
      toggle
    ), form);

    const list = accounts();
    if (!list.length) {
      root.append(D.el("p.dash-empty", { text: "还没有账户。" }));
      return root;
    }
    root.append(D.el("div.dash-led-cards", null, ...list.map((account) =>
      D.el("div.dash-led-card", null,
        D.el("span.dash-led-card-name", { text: account.name }),
        D.el("span.dash-chip", { text: ACCOUNT_KIND_LABEL[account.kind] || "其他" }),
        D.el("strong.dash-led-card-value", { text: `${account.balance_text} ${account.currency}` })))));
    return root;
  }

  /* ── 分类 ───────────────────────────────────────────────── */

  function categoriesView(ctx) {
    const { state, categories, reload } = ctx;
    const root = D.el("div");

    const name = D.el("input.dash-input", { type: "text", placeholder: "健身" });
    const direction = D.select(
      [{ value: "expense", label: "支出" }, { value: "income", label: "收入" }],
      "expense", () => {}, "方向");
    const parent = D.select([{ value: "", label: "作为一级分类" }], "", () => {}, "上级");
    const icon = D.el("input.dash-input", { type: "text", placeholder: "🏋️", maxlength: "4" });

    // 上级只能选同方向的一级分类，改方向要跟着换一批。
    const syncParents = () => {
      const roots = categories().filter((c) => c.direction === direction.value && !c.parent_id);
      parent.replaceChildren(D.el("option", { value: "", text: "作为一级分类" }));
      for (const root of roots) {
        parent.append(D.el("option", { value: root.id, text: root.name }));
      }
    };
    direction.addEventListener("change", syncParents);
    syncParents();

    const { toggle, form } = inlineForm({
      label: "新建分类",
      fields: [
        { node: D.field("名称", name) },
        { node: D.field("方向", direction) },
        { node: D.field("上级", parent, "选了就成为二级分类，花销照样算进上级") },
        { node: D.field("图标", icon, "一个 emoji，可留空") },
      ],
      submit: async () => {
        try {
          await D.api("/api/dash/ledger/categories", {
            method: "POST",
            body: {
              book: state.book,
              name: name.value.trim(),
              direction: direction.value,
              parent: parent.value,
              icon: icon.value.trim(),
            },
          });
          D.toast("已建立");
          reload();
        } catch (error) { D.toast(error.message, "error"); }
      },
    });

    root.append(head("分类", "收和支各一棵树，最多两层。二级分类的花销会算进它的上级。", toggle), form);

    for (const [dir, title] of [["expense", "支出分类"], ["income", "收入分类"]]) {
      const all = categories().filter((c) => c.direction === dir);
      const roots = all.filter((c) => !c.parent_id);
      const block = D.el("section.dash-led-block", null, D.el("h3.dash-section", { text: title }));
      if (!roots.length) {
        block.append(D.el("p.dash-empty", { text: "还没有分类" }));
      } else {
        block.append(D.el("div.dash-led-chips", null, ...roots.flatMap((root) => {
          const children = all.filter((c) => c.parent_id === root.id);
          return [
            D.el("span.dash-chip.is-active", { text: `${root.icon || ""} ${root.name}`.trim() }),
            ...children.map((child) =>
              D.el("span.dash-chip", { text: `${root.name}/${child.name}` })),
          ];
        })));
      }
      root.append(block);
    }
    return root;
  }

  /* ── 预算 ───────────────────────────────────────────────── */

  function budgetsView(ctx) {
    const { state, categories, reload } = ctx;
    const root = D.el("div");
    const budgets = state.overview?.budgets || [];

    const scope = D.select([{ value: "", label: "整本账（总预算）" }], "", () => {}, "作用范围");
    for (const category of categories().filter((c) => c.direction === "expense" && !c.parent_id)) {
      scope.append(D.el("option", { value: category.id, text: `${category.icon || ""} ${category.name}`.trim() }));
    }
    const amount = D.el("input.dash-input", { type: "text", inputmode: "decimal", placeholder: "3000" });

    const { toggle, form } = inlineForm({
      label: "设定预算",
      fields: [
        { node: D.field("作用范围", scope) },
        { node: D.field("每月额度", amount, `以 ${ctx.currency()} 计`) },
      ],
      submit: async () => {
        try {
          await D.api("/api/dash/ledger/budgets", {
            method: "POST",
            body: { book: state.book, category: scope.value, amount: amount.value.trim() },
          });
          D.toast("已设定");
          reload();
        } catch (error) { D.toast(error.message, "error"); }
      },
    });

    root.append(head(
      "月度预算",
      "每月重新计算。超过八成她记账时会提一句，超支会直接报出来。",
      toggle
    ), form);

    if (!budgets.length) {
      root.append(D.el("p.dash-empty", { text: "还没有设预算。" }));
      return root;
    }
    root.append(D.el("div", null, ...budgets.map((budget) => {
      const percent = budget.limit_minor > 0
        ? Math.min(100, Math.round((budget.used_minor / budget.limit_minor) * 100))
        : 0;
      const fill = D.el(`i.dash-meter-fill.is-${budget.state}`);
      fill.style.setProperty("width", `${percent}%`);
      return D.el("div.dash-meter.dash-led-budget", null,
        D.el("span.dash-meter-label", {
          text: budget.scope === "total" ? "整本账" : budget.scope.replace("category:", ""),
        }),
        D.el("span.dash-meter-track", null, fill),
        D.el("span.dash-cell-mono", { text: `${budget.used_text} / ${budget.limit_text}` }),
        D.iconButton("trash-2", "删除预算", async () => {
          if (!(await D.confirmAction("删除这条预算?"))) return;
          try {
            await D.api(`/api/dash/ledger/budgets/${encodeURIComponent(budget.id)}`, { method: "DELETE" });
            D.toast("已删除");
            reload();
          } catch (error) { D.toast(error.message, "error"); }
        }));
    })));
    return root;
  }

  /* ── 数据:账本 + 导入导出 ───────────────────────────────── */

  function dataView(ctx) {
    const { state, reload, query } = ctx;
    const root = D.el("div");

    // 账本
    const bookName = D.el("input.dash-input", { type: "text", placeholder: "工作" });
    const bookCurrency = D.el("input.dash-input", { type: "text", placeholder: "CNY", maxlength: "3" });
    const { toggle: bookToggle, form: bookForm } = inlineForm({
      label: "新建账本",
      fields: [
        { node: D.field("名称", bookName) },
        { node: D.field("目标币种", bookCurrency, "这本账的所有统计都换算成它，之后不便更改") },
      ],
      submit: async () => {
        try {
          await D.api("/api/dash/ledger/books", {
            method: "POST",
            body: { name: bookName.value.trim(), currency: bookCurrency.value.trim().toUpperCase() },
          });
          D.toast("已建立");
          reload();
        } catch (error) { D.toast(error.message, "error"); }
      },
    });
    root.append(head("账本", "每本账有自己的币种、账户、分类和预算，互不干扰。", bookToggle), bookForm);
    root.append(D.el("div.dash-led-cards", null, ...(state.overview?.books || []).map((book) =>
      D.el(`div.dash-led-card${book.id === state.book ? ".is-current" : ""}`, null,
        D.el("span.dash-led-card-name", { text: book.name }),
        D.el("span.dash-chip", { text: book.currency }),
        book.id === state.book ? D.el("span.dash-chip.is-active", { text: "当前" }) : null))));

    // 导入导出
    // 原生 file 控件的「Choose File / No file chosen」是浏览器给的英文，
    // 改不了，只能把它藏起来自己做一个。
    const fileName = D.el("span.dash-led-filename", { text: "未选择文件" });
    const file = D.el("input", {
      type: "file",
      accept: ".csv,text/csv",
      hidden: true,
      onchange: () => { fileName.textContent = file.files?.[0]?.name || "未选择文件"; },
    });
    const pick = D.el("div.dash-led-file", null,
      D.el("button.dash-button", { type: "button", text: "选择文件", onclick: () => file.click() }),
      fileName, file);
    const createMissing = D.el("input", { type: "checkbox" });
    const importButton = D.el("button.dash-button", {
      type: "button",
      text: "开始导入",
      onclick: async () => {
        const picked = file.files?.[0];
        if (!picked) { D.toast("先选一个 CSV 文件", "error"); return; }
        importButton.disabled = true;
        try {
          const csv = await picked.text();
          const result = await D.api("/api/dash/ledger/import", {
            method: "POST",
            body: { book: state.book, csv, create_missing: createMissing.checked },
          });
          const parts = [`导入 ${result.imported} 笔`];
          if (result.skipped) parts.push(`跳过重复 ${result.skipped} 笔`);
          if (result.failed) parts.push(`失败 ${result.failed} 笔`);
          D.toast(parts.join("，"), result.failed ? "error" : undefined);
          if (result.failures?.length) console.warn("记账导入失败明细", result.failures);
          reload();
        } catch (error) {
          D.toast(error.message, "error");
        } finally {
          importButton.disabled = false;
        }
      },
    });

    root.append(
      D.el("section.dash-led-block", null,
        D.el("h3.dash-section", { text: "导入导出" }),
        D.el("p.dash-led-hint", { text: "导出的是当前月份；导入按内容去重，同一份文件导两次不会重复。" }),
        D.el("div.dash-actions", null,
          D.el("button.dash-button", {
            type: "button",
            text: "导出本月 CSV",
            onclick: () => window.open(`/api/dash/ledger/export?${query()}`, "_blank"),
          })),
        D.el("div.dash-led-form", null,
          D.field("导入 CSV", pick, "表头至少要有 date、kind、amount 三列"),
          D.el("label.dash-check", null, createMissing,
            D.el("span", { text: "自动建立文件里没见过的分类与账户" })),
          D.el("div.dash-actions", null, importButton))));

    return root;
  }

  window.MiyuLedgerViews = {
    accounts: accountsView,
    categories: categoriesView,
    budgets: budgetsView,
    data: dataView,
  };
})();
