/*
 * 脚本面板(09-05)。
 *
 * 统计卡 → 过滤条(层 / 搜索)→ 已注册脚本表 → 未注册文件 → 禁用名单。
 * 行点开抽屉:描述、来源(头部 / index 覆盖)、参数 schema、头部源码预览、
 * 禁用 / 启用 / 删除。数据来自 /api/dash/scripts/*,写操作与 manage_script
 * 同一套后端逻辑,这里只是把它们摆出来给人看、给人点。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = { overview: null, filter: "all", q: "", loadSeq: 0, persona: D.recall("scripts.persona"), personas: [], active: "" };
  const ui = {};

  const LAYER_LABEL = { builtin: "内置", "builtin-persona": "内置", global: "全局", persona: "人格", unknown: "?" };
  const LAYER_CLASS = { builtin: "is-builtin", "builtin-persona": "is-builtin", global: "is-active", persona: "is-warn", unknown: "is-muted" };
  const FIELD_LABEL = { display_name: "显示名", description: "描述", parameters: "参数", timeout_seconds: "超时", always_loaded: "常驻", groups: "分组", argv: "argv" };

  function bytes(n) {
    if (n === null || n === undefined) return "—";
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(2)} MB`;
  }

  const chip = (label, cls) => D.el(`span.dash-chip${cls ? `.${cls}` : ""}`, { text: label });
  const rowAttrs = (open) => ({ role: "row", tabindex: "0", onclick: open, onkeydown: (event) => { if (event.key === "Enter") open(); } });
  const stop = (handler) => (event) => { event.stopPropagation(); handler(); };

  /* ── 挂载 ─────────────────────────────────────────────── */
  function mount(root) {
    root.textContent = "";
    ui.stamp = D.el("small", { text: "" });
    // 人格选择器:脚本四层里两层按人格分,切人格看到的就是那个人格上线时模型
    // 真正拿到的工具集;写操作也落到所选人格的目录。
    ui.persona = D.select([], state.persona, (value) => { state.persona = value; D.remember("scripts.persona", value); load(); }, "人格");
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "脚本" }),
      D.iconButton("refresh-cw", "刷新", () => load()),
      ui.stamp,
      D.el("span.dash-scope", null, D.el("span.dash-scope-label", { text: "人格" }), ui.persona));
    ui.cards = D.el("div");
    ui.banner = D.el("div");
    ui.search = D.el("input.dash-search", { type: "search", placeholder: "按 id、名称或描述筛选…", oninput: () => {
      clearTimeout(ui.searchTimer);
      ui.searchTimer = setTimeout(() => { state.q = ui.search.value.trim().toLowerCase(); renderList(); }, 250);
    } });
    ui.filter = D.segmented([
      { value: "all", label: "全部" }, { value: "builtin", label: "内置" }, { value: "user", label: "用户" }
    ], state.filter, (value) => { state.filter = value; renderList(); });
    const toolbar = D.el("div.dash-toolbar", null, ui.filter.el, D.el("label.dash-search-box", null, D.icon("search"), ui.search));
    ui.list = D.el("div");
    ui.unregistered = D.el("div");
    ui.disabled = D.el("div");
    root.append(head, ui.cards, ui.banner, toolbar, ui.list, ui.unregistered, ui.disabled);
    load();
  }

  const personaQuery = () => `persona=${encodeURIComponent(state.persona)}`;
  const withPersona = (body) => ({ ...body, persona: state.persona });

  async function loadPersonas() {
    try {
      const payload = await D.api("/api/dash/scripts/personas");
      state.personas = payload.personas || [];
      state.active = payload.active || "";
      if (!state.persona || !state.personas.includes(state.persona)) state.persona = state.active;
      ui.persona.textContent = "";
      for (const name of state.personas) {
        ui.persona.append(D.el("option", { value: name, text: name === state.active ? `${name}(当前)` : name }));
      }
      ui.persona.value = state.persona;
    } catch (error) {
      ui.stamp.textContent = `人格列表加载失败:${error.message}`;
    }
  }

  async function load() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    if (!state.personas.length) await loadPersonas();
    try {
      const overview = await D.api(`/api/dash/scripts/overview?${personaQuery()}`);
      if (seq !== state.loadSeq) return;
      state.overview = overview;
      renderCards();
      renderList();
      renderUnregistered();
      renderDisabled();
      const viewing = overview.persona === state.active ? "当前人格" : `人格 ${overview.persona}(非当前,内置脚本按该人格规则显示)`;
      ui.stamp.textContent = `${overview.counts.registered} 个工具 · ${viewing}`;
    } catch (error) {
      ui.stamp.textContent = `加载失败:${error.message}`;
    }
  }

  /* ── 统计卡 ───────────────────────────────────────────── */
  function renderCards() {
    const c = state.overview.counts;
    ui.cards.replaceChildren(D.statCards([
      { label: "脚本工具", value: c.registered, hint: `内置 ${c.builtin} · 用户 ${c.user}` },
      { label: "常驻", value: c.always_loaded, hint: "其余按需加载" },
      { label: "未注册", value: c.unregistered, hint: "缺 # Description: 头部" },
      { label: "已禁用", value: c.disabled, hint: "unregister 后的名单" }
    ]));
    ui.banner.textContent = "";
    if (c.unregistered) {
      ui.banner.append(D.el("p.dash-banner", { text: `有 ${c.unregistered} 个脚本文件没有描述头,不会成为工具。在下方「未注册」里补一句描述即可登记,或在文件开头加一行 # Description:。` }));
    }
  }

  /* ── 已注册表 ─────────────────────────────────────────── */
  function visibleScripts() {
    const q = state.q;
    return state.overview.scripts.filter((script) => {
      if (state.filter === "builtin" && !script.builtin) return false;
      if (state.filter === "user" && script.builtin) return false;
      if (!q) return true;
      return [script.id, script.display_name, script.description].some((text) => (text || "").toLowerCase().includes(q));
    });
  }

  function renderList() {
    ui.list.textContent = "";
    const scripts = visibleScripts();
    ui.list.append(D.el("h3.dash-section", { text: `已注册(${scripts.length})` }));
    if (!scripts.length) {
      ui.list.append(D.el("p.dash-empty", { text: state.q ? "没有匹配的脚本。" : "这一层还没有脚本工具。" }));
      return;
    }
    const grid = D.table([
      { label: "工具", width: "minmax(220px, 2fr)" }, { label: "层", width: "64px" }, { label: "参数", width: "minmax(140px, 1.4fr)" },
      { label: "超时", width: "64px" }, { label: "分组", width: "110px" }, { label: "加载", width: "72px" }, { label: "argv", width: "60px" }, { label: "", width: "88px" }]);
    for (const script of scripts) grid.append(scriptRow(script));
    ui.list.append(grid);
  }

  function scriptRow(script) {
    const params = script.parameter_names.length ? script.parameter_names.join(", ") : "自由 JSON(stdin)";
    const actions = D.el("span.dash-cell-actions", null,
      D.el("button.dash-button", { type: "button", text: "禁用", title: "从工具面上移除,文件保留", onclick: stop(() => disable(script)) }),
      script.builtin ? null : D.iconButton("trash-2", "删除文件", stop(() => remove(script)), "is-danger"));
    return D.el("div.dash-row", rowAttrs(() => openScript(script)),
      D.el("span.dash-cell-main", null,
        D.el("strong", { text: script.display_name || script.id }),
        D.el("span.dash-cell-muted", { text: ` ${script.id}` }),
        script.overrides.length ? chip("index 覆盖", "is-warn") : null),
      D.el("span", null, chip(LAYER_LABEL[script.layer] || script.layer, LAYER_CLASS[script.layer])),
      D.el("span.dash-cell-muted", { text: params, title: params }),
      D.el("span.dash-cell-mono", { text: `${script.timeout_seconds}s` }),
      D.el("span.dash-cell-muted", { text: script.groups.join(", ") || "—" }),
      D.el("span", null, chip(script.always_loaded ? "常驻" : "按需", script.always_loaded ? "is-active" : "is-muted")),
      D.el("span.dash-cell-mono", { text: script.argv === "flags" ? "flags" : "—" }),
      actions);
  }

  /* ── 未注册 / 禁用 ───────────────────────────────────── */
  function renderUnregistered() {
    ui.unregistered.textContent = "";
    const items = state.overview.unregistered;
    if (!items.length) return;
    ui.unregistered.append(D.el("h3.dash-section", { text: `未注册(${items.length})` }));
    const grid = D.table([{ label: "文件", width: "minmax(220px, 2fr)" }, { label: "层", width: "64px" }, { label: "路径", width: "minmax(240px, 3fr)" }, { label: "", width: "88px" }]);
    for (const item of items) {
      grid.append(D.el("div.dash-row", rowAttrs(() => openUnregistered(item)),
        D.el("span.dash-cell-main", { text: item.file_name }),
        D.el("span", null, chip(LAYER_LABEL[item.layer] || item.layer, LAYER_CLASS[item.layer])),
        D.el("span.dash-cell-mono", { text: item.path, title: item.path }),
        D.el("span.dash-cell-actions", null, D.el("button.dash-button.is-primary", { type: "button", text: "注册", onclick: stop(() => openUnregistered(item)) }))));
    }
    ui.unregistered.append(grid);
  }

  function renderDisabled() {
    ui.disabled.textContent = "";
    const items = state.overview.disabled;
    if (!items.length) return;
    ui.disabled.append(D.el("h3.dash-section", { text: `已禁用(${items.length})` }));
    const grid = D.table([{ label: "工具", width: "minmax(200px, 1.5fr)" }, { label: "记录在", width: "72px" }, { label: "路径", width: "minmax(240px, 3fr)" }, { label: "", width: "88px" }]);
    for (const item of items) {
      grid.append(D.el("div.dash-row", { role: "row" },
        D.el("span.dash-cell-main", null, D.el("strong", { text: item.id }), item.builtin ? chip("内置", "is-builtin") : null),
        D.el("span", null, chip(LAYER_LABEL[item.scope] || item.scope, LAYER_CLASS[item.scope])),
        D.el("span.dash-cell-mono", { text: item.path || "(按 id 屏蔽)" }),
        D.el("span.dash-cell-actions", null, D.el("button.dash-button.is-primary", { type: "button", text: "启用", onclick: () => enable(item.id) }))));
    }
    ui.disabled.append(grid);
  }

  /* ── 抽屉 ─────────────────────────────────────────────── */
  function metaList(pairs) {
    return D.el("dl.dash-meta", null, pairs.filter(([, value]) => value !== null && value !== undefined && value !== "")
      .flatMap(([key, value]) => [D.el("dt", { text: key }), D.el("dd", value instanceof Node ? null : { text: String(value) }, value instanceof Node ? value : null)]));
  }

  function codeBlock(text) {
    const pre = D.el("pre.dash-code");
    String(text).split("\n").forEach((line, index) => {
      pre.append(D.el("span.dash-code-line", null, D.el("span.dash-code-no", { text: String(index + 1) }), D.el("span.dash-code-text", { text: line || " " })));
    });
    return pre;
  }

  async function sourceBlock(query) {
    const holder = D.el("div", null, D.el("p.dash-empty", { text: "读取源码…" }));
    try {
      const source = await D.api(`/api/dash/scripts/source?${new URLSearchParams({ ...query, persona: state.persona })}`);
      holder.replaceChildren(codeBlock(source.lines.join("\n")));
      if (source.truncated) holder.append(D.el("p.dash-cell-muted", { text: `只显示前 ${source.shown} 行(文件 ${bytes(source.size_bytes)})` }));
    } catch (error) {
      holder.replaceChildren(D.el("p.dash-empty", { text: `读取失败:${error.message}` }));
    }
    return holder;
  }

  function sourceLabel(script, field) {
    return script.overrides.includes(field) ? `index(${script.override_scope})` : (script.header[`has_${field}`] || script.header[field] ? "头部" : "默认");
  }

  async function openScript(script) {
    const overrideText = script.overrides.length ? script.overrides.map((field) => FIELD_LABEL[field] || field).join("、") : "无";
    const body = D.el("div", null,
      D.el("p.dash-drawer-desc", { text: script.description }),
      metaList([
        ["id", script.id],
        ["层", `${LAYER_LABEL[script.layer] || script.layer}${script.builtin ? "(只读文件,可屏蔽)" : ""}`],
        ["路径", script.path],
        ["文件", `${bytes(script.size_bytes)} · ${script.modified ? D.formatTime(script.modified) : "—"} · ${script.executable ? "可执行" : "缺可执行位"}`],
        ["描述来源", sourceLabel(script, "description")],
        ["参数来源", script.parameters ? sourceLabel(script, "parameters") : "无 schema,自由 JSON 走 stdin"],
        ["超时", `${script.timeout_seconds}s${script.timeout_default ? "(默认)" : ""}`],
        ["分组 / 加载", `${script.groups.join(", ") || "—"} · ${script.load_policy} · ${script.always_loaded ? "常驻" : "按需"}`],
        ["argv", script.argv === "flags" ? "flags:参数同时展开为 --key=value" : "none:只走 stdin JSON 与 MIYU_ARGS_JSON"],
        ["index 覆盖", overrideText]
      ]),
      script.parameters ? D.el("h4.dash-section", { text: "参数 schema" }) : null,
      script.parameters ? codeBlock(JSON.stringify(script.parameters, null, 2)) : null,
      D.el("h4.dash-section", { text: "文件开头" }));
    const source = sourceBlock({ id: script.id, lines: "80" });
    body.append(await source);
    const actions = [D.el("button.dash-button", { type: "button", text: "禁用", onclick: () => disable(script) })];
    if (!script.builtin) actions.push(D.el("button.dash-button.is-danger", { type: "button", text: "删除文件", onclick: () => remove(script) }));
    D.openDrawer(script.display_name || script.id, body, actions);
  }

  async function openUnregistered(item) {
    const description = D.el("textarea.dash-textarea", { rows: "3", placeholder: "English, first sentence under 60 characters. What it does and when to use it." });
    const idInput = D.el("input.dash-input", { type: "text", placeholder: item.name.replace(/[^A-Za-z0-9]+/g, "_"), pattern: "^[a-zA-Z][a-zA-Z0-9_]*$" });
    const body = D.el("div", null,
      D.el("p.dash-banner", { text: "这个文件有 shebang 但没有 # Description: 头部,所以没成为工具。填一句描述就能登记(写进 index 覆盖层);更推荐直接在文件开头加 # Description: 行。" }),
      metaList([["文件", item.file_name], ["层", LAYER_LABEL[item.layer] || item.layer], ["路径", item.path]]),
      D.field("描述(必填)", description),
      D.field("工具名(可选)", idInput, "留空则由文件名推出"),
      D.el("h4.dash-section", { text: "文件开头" }));
    body.append(await sourceBlock({ path: item.path, lines: "60" }));
    const submit = D.el("button.dash-button.is-primary", { type: "button", text: "注册", onclick: async () => {
      if (!description.value.trim()) { D.toast("描述不能为空", "error"); return; }
      try {
        const result = await D.api("/api/dash/scripts/register", { method: "POST", body: withPersona({ path: item.path, description: description.value.trim(), id: idInput.value.trim() }) });
        D.toast(`已注册 ${result.id}`);
        D.closeDrawer();
        await load();
      } catch (error) {
        D.toast(`注册失败:${error.message}`, "error");
      }
    } });
    D.openDrawer(item.file_name, body, [submit]);
  }

  /* ── 动作 ─────────────────────────────────────────────── */
  async function disable(script) {
    const ok = await D.confirmAction(`禁用 ${script.id}?它会从工具面上消失,文件保留;之后可在「已禁用」里一键启用。${script.builtin ? `\n\n内置脚本只对所选人格(${state.persona})屏蔽。` : ""}`, "禁用");
    if (!ok) return;
    try {
      await D.api("/api/dash/scripts/disable", { method: "POST", body: withPersona({ id: script.id }) });
      D.toast(`已禁用 ${script.id}`);
      D.closeDrawer();
      await load();
    } catch (error) {
      D.toast(`禁用失败:${error.message}`, "error");
    }
  }

  async function enable(id) {
    try {
      const result = await D.api("/api/dash/scripts/enable", { method: "POST", body: withPersona({ id }) });
      D.toast(result.enabled ? `已启用 ${id}` : `${id} 本来就没被禁用`);
      await load();
    } catch (error) {
      D.toast(`启用失败:${error.message}`, "error");
    }
  }

  async function remove(script) {
    const ok = await D.confirmAction(`删除 ${script.file_name}?文件从磁盘删除,不可撤销。`);
    if (!ok) return;
    try {
      await D.api(`/api/dash/scripts/item?id=${encodeURIComponent(script.id)}&${personaQuery()}`, { method: "DELETE" });
      D.toast(`已删除 ${script.id}`);
      D.closeDrawer();
      await load();
    } catch (error) {
      D.toast(`删除失败:${error.message}`, "error");
    }
  }

  D.register({ name: "scripts", root: "dashScriptsRoot", mount, refresh: () => load() });
})();
