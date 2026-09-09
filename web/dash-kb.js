/*
 * 知识库面板(09-04)。
 *
 * 统计卡 + 内置库卡 → 搜索条(内容 / 文件名)→ 左目录树 / 右预览或搜索结果。
 * 上传(拖放、选文件、选文件夹三个入口共用一条流水线)在前端按扩展名、大小、
 * UTF-8 预检,逐个 POST;删除、语义重建、内置库更新走各自接口,重建与更新都有
 * 轮询状态。数据来自 /api/dash/kb/*。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = {
    picked: new Set(),
    overview: null,
    defaultKb: null,
    mode: "browse",      // browse | search
    searchBy: "content",
    q: "",
    selected: "",
    collapsed: new Set(),
    uploading: false,
    reindexTimer: null,
    reindexMisses: 0,
    updateTimer: null,
    loadSeq: 0
  };
  const ui = {};

  const INDEX_LABEL = { fresh: "已索引", stale: "陈旧", unindexed: "未索引" };
  const INDEX_CLASS = { fresh: "is-fresh", stale: "is-stale", unindexed: "is-none" };

  function bytes(n) {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(2)} MB`;
  }

  /* ── 挂载 ─────────────────────────────────────────────── */
  function mount(root) {
    root.textContent = "";
    ui.stamp = D.el("small", { text: "" });
    // 三个入口(选文件 / 选文件夹 / 拖放)都归一成 { file, name },后面只有一条流水线。
    ui.fileInput = D.el("input", { type: "file", multiple: true, hidden: true, onchange: () => queueUploads(Array.from(ui.fileInput.files).map((file) => ({ file, name: file.name }))) });
    ui.dirInput = D.el("input", { type: "file", multiple: true, hidden: true, onchange: () => queueUploads(Array.from(ui.dirInput.files).map((file) => ({ file, name: file.webkitRelativePath || file.name }))) });
    ui.dirInput.setAttribute("webkitdirectory", "");
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "知识库" }),
      D.iconButton("refresh-cw", "刷新", () => reloadAll()),
      ui.stamp,
      D.el("span.dash-scope", null,
        D.el("button.dash-button.is-primary", { type: "button", title: "也可以直接把文件拖到这个页面上", onclick: () => ui.fileInput.click() }, D.icon("plus"), "上传文件"),
        D.el("button.dash-button", { type: "button", onclick: () => ui.dirInput.click() }, D.icon("archive"), "上传文件夹"),
        ui.fileInput, ui.dirInput));

    ui.cards = D.el("div");
    ui.defaultCard = D.el("div");
    ui.search = D.el("input.dash-search", { type: "search", placeholder: "搜索知识库…", oninput: () => {
      clearTimeout(ui.searchTimer);
      ui.searchTimer = setTimeout(() => { state.q = ui.search.value.trim(); state.mode = state.q ? "search" : "browse"; renderMain(); }, 280);
    } });
    ui.by = D.segmented([{ value: "content", label: "按内容" }, { value: "name", label: "按文件名" }], state.searchBy, (value) => { state.searchBy = value; if (state.q) renderMain(); });
    const toolbar = D.el("div.dash-toolbar", null, ui.by.el, D.el("label.dash-search-box", null, D.icon("search"), ui.search));

    ui.tree = D.el("div.dash-tree-pane");
    ui.main = D.el("div.dash-main-pane");
    ui.uploadLog = D.el("div.dash-upload-log", { hidden: true });
    root.append(head, ui.cards, ui.defaultCard, toolbar, ui.uploadLog, D.el("div.dash-split", null, ui.tree, ui.main));
    wireDrop(root);
    reloadAll();
  }

  /* 投放区取整个知识库面板(root 的 .con-panel 外壳),不是右边的文件树:库空时
     文件树只剩一行「还没有文件」,而那正是最想把文件拖进来的时刻——把最小的
     目标留给最常见的动作说不过去。面板外壳还铺满整个正文区,拖到哪都算数。
     共享文件面板(shared.js)也是「整面板可投放」,这里沿用同一套语言。 */
  function wireDrop(root) {
    const host = root.parentElement || root;
    host.classList.add("dash-drop-host");
    ui.veil = D.el("div.dash-drop-veil", { hidden: true },
      D.el("div.dash-drop-label", null, D.icon("plus"), "松开即上传到知识库"));
    host.append(ui.veil);
    // 拖的是文字/链接时既不显示指示层也不 preventDefault,事件照常冒泡出去。
    const hasFiles = (event) => Array.from(event.dataTransfer?.types || []).includes("Files");
    // 计数而不是布尔:进入子元素会先给父元素发 dragleave,只看布尔会一路闪。
    let depth = 0;
    const show = (on) => { ui.veil.hidden = !on; host.classList.toggle("is-dropping", on); };
    host.addEventListener("dragenter", (event) => {
      if (!hasFiles(event)) return;
      event.preventDefault();
      depth += 1;
      show(true);
    });
    host.addEventListener("dragover", (event) => {
      if (!hasFiles(event)) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "copy";
    });
    host.addEventListener("dragleave", (event) => {
      if (!hasFiles(event)) return;
      depth = Math.max(0, depth - 1);
      if (!depth) show(false);
    });
    host.addEventListener("drop", async (event) => {
      if (!hasFiles(event)) return;
      event.preventDefault();
      depth = 0;
      show(false);
      const dropped = await collectDropped(event.dataTransfer);
      await queueUploads(dropped.items, dropped.notes);
    });
  }

  async function reloadAll() {
    await Promise.all([loadOverview(), loadDefault()]);
  }

  /* ── 概览 ─────────────────────────────────────────────── */
  async function loadOverview() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    try {
      const o = await D.api("/api/dash/kb/overview");
      if (seq !== state.loadSeq) return;
      state.overview = o;
      renderCards();
      renderTree();
      if (state.mode === "browse" && state.selected && !o.files.some((f) => f.name === state.selected)) state.selected = "";
      renderMain();
      ui.stamp.textContent = o.exists ? `${o.file_count} 个文件 · ${bytes(o.total_size_bytes)}` : "库尚未建立";
      if (o.reindex?.running) pollReindex();
    } catch (error) {
      ui.stamp.textContent = `加载失败:${error.message}`;
    }
  }

  /* 重建卡的四态:陈旧锁 / 正在跑 / 上次失败 / 空闲。
     以前只有前两态加一个「空闲」,失败和「跑完但一个都没嵌上」都长成空闲的样子。 */
  function reindexCard(r) {
    if (r.stale_lock) {
      return { label: "重建", value: "锁陈旧", hint: `锁已 ${Math.round((r.lock_age_secs || 0) / 60)} 分钟` };
    }
    if (r.running) {
      const value = r.total > 0 ? `${reindexPercent(r.done, r.total)}%` : "进行中";
      const where = r.current ? ` · ${r.current.split("/").pop()}` : "";
      const hint = r.total > 0 ? `${r.done}/${r.total} 个文件${where}`
        : (r.phase === "starting" ? "正在启动…" : "统计文件中…");
      return { label: "重建", value, hint };
    }
    if (r.last_error) return { label: "重建", value: "上次失败", hint: `上次重建失败:${firstLine(r.last_error)}` };
    if (r.failed > 0) return { label: "重建", value: "空闲", hint: `上次有 ${r.failed} 个文件没能嵌入:${firstLine(r.last_file_error || "")}` };
    return { label: "重建", value: "空闲", hint: r.configured ? "可以重建" : "嵌入未配置,不会重建" };
  }

  /** 进行中的百分比。
   *
   * 向下取整,而且在真跑完之前封在 99——`Math.round` 会把 6497/6507 这种
   * 「还差十个」四舍五入成 100%,于是卡片显示 100% 却还在跑(09-09 用户实拍)。
   * 100% 只能表示「完了」,不能表示「快完了」。
   */
  function reindexPercent(done, total) {
    return Math.min(99, Math.floor((done / total) * 100));
  }

  function firstLine(text) {
    const line = String(text || "").split("\n").find((part) => part.trim()) || "";
    return line.length > 90 ? `${line.slice(0, 90)}…` : line;
  }

  function renderCards() {
    const o = state.overview;
    // 和重建卡用同一个判断(后端给的 embedding_configured),否则同一屏会自相矛盾。
    const embed = !o.embedding_enabled ? "已关闭"
      : o.embedding_configured ? `嵌入:${o.embedding_model_id}` : "嵌入:未配置模型";
    const r = o.reindex || {};
    const cards = D.statCards([
      { label: "文件", value: o.file_count, hint: `内置 ${o.files.filter((f) => f.builtin).length} · 自有 ${o.files.filter((f) => !f.builtin).length}` },
      { label: "总大小", value: bytes(o.total_size_bytes), hint: `单文件上限 ${o.max_file_size_kb} KB` },
      { label: "语义块", value: o.semantic_chunks, hint: embed },
      { label: "待重建", value: o.stale_files + o.unindexed_files, hint: `陈旧 ${o.stale_files} · 未索引 ${o.unindexed_files}` },
      reindexCard(r)
    ]);
    const last = cards.lastElementChild;
    // 进度条只在真跑着、且知道总数时出现;不知道总数就别画一根假的。
    if (r.running && r.total > 0) {
      const fill = D.el("i");
      fill.style.width = `${reindexPercent(r.done, r.total)}%`;
      last.append(D.el("div.dash-card-progress", null, fill));
    }
    const actions = D.el("div.dash-card-actions");
    if (r.stale_lock) {
      actions.append(D.el("button.dash-button", { type: "button", text: "清理陈旧锁", onclick: unlockReindex }));
    } else if (!r.running) {
      const button = D.el("button.dash-button", { type: "button", text: "重建语义索引", onclick: startReindex });
      button.disabled = !r.configured;
      actions.append(button);
    }
    last.append(actions);
    // 报错在卡片里必然被截断,完整原文挂 title(日志路径也在里面)。
    const hint = last.querySelector(".dash-card-hint");
    if (hint && (r.last_error || r.last_file_error)) {
      hint.title = [r.last_error, r.last_file_error, r.log_path && `日志:${r.log_path}`].filter(Boolean).join("\n");
    }
    ui.cards.replaceChildren(cards);
    if (!o.enabled) ui.cards.prepend(D.el("p.dash-banner", { text: "知识库插件在配置里是关闭的:模型用不到它,面板仍可查看与整理文件。" }));
  }

  async function loadDefault() {
    try {
      state.defaultKb = await D.api("/api/dash/kb/default");
      renderDefault();
      if (state.defaultKb.task?.running) pollUpdate();
    } catch (error) {
      ui.defaultCard.replaceChildren(D.el("p.dash-empty", { text: `内置库状态加载失败:${error.message}` }));
    }
  }

  function renderDefault() {
    const d = state.defaultKb;
    const s = d.state || {};
    const task = d.task || {};
    const short = (hash) => (hash || "").slice(0, 10) || "—";
    const status = task.running ? task.stage || "进行中…"
      : task.error ? `上次失败:${task.error}`
        : s.update_available ? "有可用更新" : "已是最新";
    const button = D.el("button.dash-button.is-slim", { type: "button", text: task.running ? "更新中…" : "更新", onclick: startUpdate });
    button.disabled = task.running || !d.bundled && !s.shorin_wiki_commit;
    // 版本号一致时只写一个:同一串哈希写两遍撑满一行,再把按钮挤到第二行,整条
    // 就长得像一根进度条了(09-09 用户反馈)。完整信息挂在 title 上。
    const local = short(s.shorin_wiki_commit);
    const remote = short(s.remote_commit);
    const version = local === remote ? local : `${local} → ${remote}`;
    const imported = s.last_imported_at ? D.formatTime(s.last_imported_at) : "—";
    const detail = D.el("span.dash-cell-muted", { text: `${version} · ${imported}` });
    detail.title = `本地 ${short(s.shorin_wiki_commit)} · 远端 ${short(s.remote_commit)} · 上次导入 ${imported}`;
    ui.defaultCard.replaceChildren(D.el("div.dash-inline-card.is-tight", null,
      D.el("span.dash-chip.is-builtin", { text: "内置库" }),
      D.el("span.dash-inline-main", { text: "Shorin ArchLinux Guide" }),
      detail,
      D.el(`span.dash-chip${s.update_available && !task.running ? ".is-warn" : ""}`, { text: status }),
      button));
  }

  /* ── 目录树 ───────────────────────────────────────────── */
  function buildTree(files) {
    const root = { dirs: new Map(), files: [] };
    for (const file of files) {
      const parts = file.name.split("/");
      let node = root;
      for (const part of parts.slice(0, -1)) {
        if (!node.dirs.has(part)) node.dirs.set(part, { dirs: new Map(), files: [] });
        node = node.dirs.get(part);
      }
      node.files.push(file);
    }
    return root;
  }

  function renderTree() {
    const o = state.overview;
    ui.tree.textContent = "";
    if (!o.files.length) {
      ui.tree.append(D.el("p.dash-empty", { text: "库里还没有文件。把文本、Markdown 或配置文件拖到这个页面上,或者用右上角的按钮选。" }));
      return;
    }
    const tree = buildTree(o.files);
    const names = new Set(o.files.map((file) => file.name));
    for (const name of [...state.picked]) if (!names.has(name)) state.picked.delete(name);
    if (state.picked.size) {
      const all = o.files.map((file) => file.name);
      ui.tree.append(D.bulkBar({
        count: state.picked.size, total: all.length, noun: "个文件", compact: true,
        onAll: () => { for (const name of all) state.picked.add(name); renderTree(); },
        onNone: () => { state.picked.clear(); renderTree(); },
        actions: [{ label: "删除所选", icon: "trash-2", danger: true, onClick: bulkRemove }]
      }));
    }
    const list = D.el("ul.dash-tree");
    // 内置库先折叠、放最后;自有内容在前。
    const dirs = [...tree.dirs.entries()].sort(([a], [b]) => (a === "default-kb") - (b === "default-kb") || a.localeCompare(b));
    for (const file of tree.files) list.append(fileNode(file, 0));
    for (const [name, node] of dirs) list.append(dirNode(name, node, name, 0));
    ui.tree.append(list);
  }

  function dirNode(name, node, path, depth) {
    const builtin = path === "default-kb";
    if (builtin && !state.collapsed.has("__init")) { state.collapsed.add("__init"); state.collapsed.add(path); }
    const collapsed = state.collapsed.has(path);
    const count = countFiles(node);
    const li = D.el("li.dash-tree-dir");
    const row = D.el("div.dash-tree-row.is-dir", { onclick: () => { if (collapsed) state.collapsed.delete(path); else state.collapsed.add(path); renderTree(); } },
      D.el("span.dash-tree-indent", { text: "" }),
      D.icon(collapsed ? "chevron-right" : "chevron-down"),
      dirCheck(node, name),
      D.el("span.dash-tree-name", { text: name }),
      builtin ? D.el("span.dash-chip.is-builtin", { text: "内置" }) : null,
      D.el("span.dash-tree-count", { text: String(count) }));
    row.firstChild.style.width = `${depth * 14}px`;
    li.append(row);
    if (!collapsed) {
      const children = D.el("ul.dash-tree");
      for (const [childName, child] of [...node.dirs.entries()].sort(([a], [b]) => a.localeCompare(b))) children.append(dirNode(childName, child, `${path}/${childName}`, depth + 1));
      for (const file of node.files) children.append(fileNode(file, depth + 1));
      li.append(children);
    }
    return li;
  }

  function countFiles(node) {
    let total = node.files.length;
    for (const child of node.dirs.values()) total += countFiles(child);
    return total;
  }

  /** 这个目录底下所有文件的完整名字,递归。整目录勾选靠它。 */
  function filesUnder(node, out = []) {
    for (const file of node.files) out.push(file.name);
    for (const child of node.dirs.values()) filesUnder(child, out);
    return out;
  }

  /**
   * 目录行的三态勾选框。
   *
   * 没有它,想删掉一个几百个文件的库只能展开后一个一个点——这正是用户报的
   * 「没法批量删除」。半选(indeterminate)必须有:否则一个已勾了部分文件的目录
   * 看上去和完全没勾一样,再点一下会把已选的也一起清掉。
   */
  function dirCheck(node, label) {
    const names = filesUnder(node);
    const picked = names.filter((name) => state.picked.has(name)).length;
    const box = D.el("input.dash-row-check.dash-tree-check", {
      type: "checkbox", "aria-label": `选择 ${label} 下的全部文件`,
    });
    box.checked = picked > 0 && picked === names.length;
    box.indeterminate = picked > 0 && picked < names.length;
    box.addEventListener("click", (event) => event.stopPropagation());
    box.addEventListener("change", () => {
      // 半选时点一下是「全选」,而不是「清空」——那更符合正在挑选的意图。
      const selectAll = box.checked || picked < names.length;
      for (const name of names) {
        if (selectAll) state.picked.add(name);
        else state.picked.delete(name);
      }
      renderTree();
    });
    return box;
  }

  function fileNode(file, depth) {
    const short = file.name.split("/").pop();
    const box = D.el("input.dash-row-check.dash-tree-check", { type: "checkbox", "aria-label": `选择 ${short}` });
    box.checked = state.picked.has(file.name);
    box.addEventListener("click", (event) => event.stopPropagation());
    box.addEventListener("change", () => { if (box.checked) state.picked.add(file.name); else state.picked.delete(file.name); renderTree(); });
    const row = D.el("div.dash-tree-row.is-file", { title: file.name, onclick: () => { state.selected = file.name; state.mode = "browse"; ui.search.value = ""; state.q = ""; renderTree(); renderMain(); } },
      D.el("span.dash-tree-indent", { text: "" }),
      box,
      D.el(`span.dash-index-dot.${INDEX_CLASS[file.index] || "is-none"}`, { title: INDEX_LABEL[file.index] || file.index }),
      D.el("span.dash-tree-name", { text: short }),
      D.el("span.dash-tree-size", { text: bytes(file.size_bytes) }),
      D.iconButton("trash-2", "删除", (event) => { event.stopPropagation(); removeFile(file); }, "is-danger"));
    row.firstChild.style.width = `${depth * 14 + 16}px`;
    row.classList.toggle("is-selected", state.selected === file.name);
    return D.el("li", null, row);
  }

  /* ── 右侧:预览 / 搜索 ───────────────────────────────── */
  function renderMain() {
    if (state.mode === "search" && state.q) return renderSearch();
    if (!state.selected) {
      ui.main.replaceChildren(D.el("p.dash-empty", { text: "点左侧文件预览,或在上方搜索。" }));
      return;
    }
    renderPreview(state.selected, 1, true);
  }

  async function renderPreview(name, start, reset) {
    const file = state.overview?.files.find((f) => f.name === name);
    try {
      const page = await D.api(`/api/dash/kb/file?${new URLSearchParams({ name, start: String(start), lines: "400" })}`);
      if (state.selected !== name) return;
      if (reset || !ui.code) {
        ui.code = D.el("pre.dash-code");
        ui.codeMore = D.el("div.dash-code-more");
        const head = D.el("div.dash-preview-head", null,
          D.el("strong.dash-preview-name", { text: name }),
          file ? D.el("span.dash-cell-muted", { text: `${bytes(file.size_bytes)} · ${page.total_lines} 行 · ${INDEX_LABEL[file.index] || ""}${file.chunks ? ` ${file.chunks} 块` : ""}` }) : null,
          file?.builtin ? D.el("span.dash-chip.is-builtin", { text: "内置(更新时会被覆盖)" }) : null,
          D.el("span.dash-actions-gap"),
          D.iconButton("trash-2", "删除此文件", () => file && removeFile(file), "is-danger"));
        ui.main.replaceChildren(head, ui.code, ui.codeMore);
      }
      appendLines(ui.code, page.text, page.start);
      ui.codeMore.textContent = "";
      if (page.has_more) {
        ui.codeMore.append(D.el("button.dash-button", { type: "button", text: `继续加载(还有 ${page.total_lines - page.end} 行)`, onclick: () => renderPreview(name, page.end + 1, false) }));
      }
    } catch (error) {
      ui.main.replaceChildren(D.el("p.dash-empty", { text: `读取失败:${error.message}` }));
    }
  }

  function appendLines(pre, text, startLine) {
    const lines = text.split("\n");
    lines.forEach((line, index) => {
      pre.append(D.el("span.dash-code-line", null, D.el("span.dash-code-no", { text: String(startLine + index) }), D.el("span.dash-code-text", { text: line || " " })));
    });
  }

  async function renderSearch() {
    const seq = ++state.loadSeq;
    ui.main.replaceChildren(D.el("p.dash-empty", { text: "搜索中…" }));
    try {
      const result = await D.api(`/api/dash/kb/search?${new URLSearchParams({ q: state.q, by: state.searchBy, limit: "20" })}`);
      if (seq !== state.loadSeq) return;
      const list = D.el("div.dash-results");
      const head = D.el("div.dash-preview-head", null,
        D.el("strong", { text: `“${state.q}” 命中 ${result.total_matches} 个文件` }),
        state.searchBy === "content" ? D.el("span.dash-cell-muted", { text: result.semantic_used ? "关键词 + 语义" : "仅关键词" }) : null);
      list.append(head);
      if (!result.results.length) list.append(D.el("p.dash-empty", { text: "没有匹配。关键词搜索是逐文件扫描,试试换个词或按文件名找。" }));
      for (const hit of result.results) {
        const card = D.el("div.dash-result", { onclick: () => { state.selected = hit.path; state.mode = "browse"; ui.search.value = ""; state.q = ""; renderTree(); renderMain(); } },
          D.el("div.dash-result-head", null,
            D.el("strong", { text: hit.name }),
            D.el("span.dash-cell-muted", { text: hit.directory || "" }),
            D.el("span.dash-actions-gap"),
            hit.source ? D.el(`span.dash-chip${hit.source === "semantic" ? ".is-builtin" : ""}`, { text: hit.source === "semantic" ? "语义" : "关键词" }) : null,
            hit.match_reason ? D.el("span.dash-chip", { text: hit.match_reason }) : null,
            D.el("span.dash-cell-mono", { text: Number(hit.score).toFixed(0) })));
        for (const snippet of (hit.snippets || []).slice(0, 3)) {
          card.append(D.el("p.dash-snippet", { text: typeof snippet === "string" ? snippet : (snippet.text || JSON.stringify(snippet)) }));
        }
        list.append(card);
      }
      ui.main.replaceChildren(list);
    } catch (error) {
      ui.main.replaceChildren(D.el("p.dash-empty", { text: `搜索失败:${error.message}` }));
    }
  }

  /* ── 上传 ─────────────────────────────────────────────── */

  // 一眼就不是文本的扩展名单独给一句人话,别让用户对着「类型不允许」猜。
  const BINARY_EXTS = new Set([".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".ico", ".avif", ".tiff", ".heic",
    ".pdf", ".zip", ".gz", ".xz", ".bz2", ".zst", ".tar", ".7z", ".rar", ".mp3", ".wav", ".flac", ".ogg", ".opus",
    ".m4a", ".mp4", ".mkv", ".mov", ".webm", ".woff", ".woff2", ".ttf", ".otf", ".exe", ".dll", ".so", ".dylib",
    ".bin", ".img", ".iso", ".o", ".a", ".class", ".jar", ".wasm", ".db", ".sqlite", ".sqlite3", ".pyc",
    ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".psd", ".blend"]);
  const MAX_DROP_FILES = 200;   // 再多基本是误拖了整个家目录
  const MAX_DROP_DEPTH = 3;     // 目录递归层数

  function csvList(value) {
    return (value || "").split(",").map((s) => s.trim().toLowerCase()).filter(Boolean);
  }

  function extOf(base) {
    const dot = base.lastIndexOf(".");
    return dot > 0 ? base.slice(dot) : "";   // 点在开头是 .env 这类整名,不算扩展名
  }

  /* 前端预检,判据抄的是 Rust 侧 validate_file(大小 / 扩展名或整名 / UTF-8),
     只为省掉一趟明知会被 400 回来的网络。overview 没载入时全放行,由服务端说了算。 */
  function precheck(name, file) {
    if (file.size === 0) return "空文件";
    const o = state.overview;
    if (!o) return "";
    if (file.size > o.max_file_size_kb * 1024) return `超过 ${o.max_file_size_kb} KB`;
    const base = name.split("/").pop().toLowerCase();
    const ext = extOf(base);
    if (csvList(o.allowed_extensions).includes(ext) || csvList(o.allowed_filenames).includes(base)) return "";
    if (BINARY_EXTS.has(ext) || /^(image|video|audio)\//.test(file.type)) return "不是文本文件";
    return `类型不允许(${ext || "无扩展名"})`;
  }

  function isUtf8Text(buffer) {
    try { new TextDecoder("utf-8", { fatal: true }).decode(buffer); return true; } catch (_) { return false; }
  }

  /* 拖进来的可能是目录。DataTransfer 在事件回调返回后就作废,所以 entry 必须在
     drop 的同步段里全部取出来,异步遍历只能吃这份快照。 */
  function transferSnapshot(transfer) {
    const items = Array.from(transfer?.items || []).filter((item) => item.kind === "file");
    return { entries: items.map((item) => (item.webkitGetAsEntry ? item.webkitGetAsEntry() : null)), files: Array.from(transfer?.files || []) };
  }

  async function collectDropped(transfer) {
    const { entries, files } = transferSnapshot(transfer);
    const items = [];
    const notes = [];
    if (!entries.some(Boolean)) {
      // 拿不到 entry(老内核)时只能要平铺的 files,目录在这条路上本就看不见。
      for (const file of files.slice(0, MAX_DROP_FILES)) items.push({ file, name: file.name });
      if (files.length > MAX_DROP_FILES) notes.push(`一次最多 ${MAX_DROP_FILES} 个文件,其余忽略`);
      return { items, notes };
    }
    let tooDeep = 0;
    const walk = async (entry, prefix, depth) => {
      if (!entry || items.length >= MAX_DROP_FILES) return;
      if (entry.isFile) {
        const file = await new Promise((resolve) => entry.file(resolve, () => resolve(null)));
        if (file) items.push({ file, name: prefix ? `${prefix}/${file.name}` : file.name });
        return;
      }
      if (!entry.isDirectory) return;
      if (depth >= MAX_DROP_DEPTH) { tooDeep += 1; return; }
      const reader = entry.createReader();
      const next = prefix ? `${prefix}/${entry.name}` : entry.name;
      // readEntries 每次最多给 100 条,要一直读到空数组才算读完一层。
      for (;;) {
        const batch = await new Promise((resolve) => reader.readEntries(resolve, () => resolve([])));
        if (!batch.length) break;
        for (const child of batch) await walk(child, next, depth + 1);
        if (items.length >= MAX_DROP_FILES) break;
      }
    };
    for (const entry of entries) await walk(entry, "", 0);
    if (tooDeep) notes.push(`目录只展开 ${MAX_DROP_DEPTH} 层,更深的 ${tooDeep} 个目录没有进来`);
    if (items.length >= MAX_DROP_FILES) notes.push(`一次最多 ${MAX_DROP_FILES} 个文件,其余忽略`);
    return { items, notes };
  }

  function resetLog() {
    ui.uploadHead = D.el("strong", { text: "上传记录" });
    ui.uploadList = D.el("ul.dash-upload-list");
    ui.uploadLog.replaceChildren(
      D.el("div.dash-upload-head", null, ui.uploadHead, D.el("span.dash-actions-gap"), D.iconButton("x", "收起", () => { ui.uploadLog.hidden = true; })),
      ui.uploadList);
    ui.uploadLog.hidden = false;
  }

  /* 一个文件一行,返回就地改这行结果的函数——等待中 / 上传中 / 结果共用一个 chip。 */
  function logRow(name, text, cls) {
    const chip = D.el(`span.dash-chip${cls ? `.${cls}` : ""}`, { text });
    ui.uploadList.append(D.el("li", null, D.el("span.dash-cell-mono", { text: name, title: name }), chip));
    ui.uploadList.scrollTop = ui.uploadList.scrollHeight;
    return (nextText, nextCls) => {
      chip.textContent = nextText;
      chip.title = nextText;
      chip.className = `dash-chip${nextCls ? ` ${nextCls}` : ""}`;
      ui.uploadList.scrollTop = ui.uploadList.scrollHeight;
    };
  }

  /* items 是 [{ file, name }];串行上传,一个失败不影响后面的。 */
  async function queueUploads(items, notes = []) {
    ui.fileInput.value = "";
    ui.dirInput.value = "";
    if (!items.length && !notes.length) return;   // 空投放:什么也不做,也不报错
    if (state.uploading) { D.toast("上一批还在传,等它完", "error"); return; }
    state.uploading = true;
    resetLog();
    for (const note of notes) logRow("—", note, "is-muted");
    // 同名文件服务端是覆盖写,所以「已存在」按上传前的库快照 + 本批已传过的名字判。
    const known = new Set((state.overview?.files || []).map((file) => file.name));
    let stored = 0, replaced = 0, skipped = 0, rejected = 0, failed = 0;
    try {
      const rows = items.map((item) => ({ item, update: logRow(item.name, "等待中", "is-muted") }));
      for (const [index, row] of rows.entries()) {
        const { file, name } = row.item;
        ui.uploadHead.textContent = `上传记录(${index + 1}/${rows.length})`;
        const reason = precheck(name, file);
        if (reason) { skipped += 1; row.update(`跳过:${reason}`, "is-muted"); continue; }
        row.update("上传中…", "");
        try {
          const buffer = await file.arrayBuffer();
          if (!isUtf8Text(buffer)) { skipped += 1; row.update("跳过:不是 UTF-8 文本", "is-muted"); continue; }
          const response = await fetch(`/api/dash/kb/files?name=${encodeURIComponent(name)}`, { method: "POST", body: buffer, headers: { "content-type": "application/octet-stream" } });
          const payload = await response.json().catch(() => null);
          const message = payload?.error?.message || "";
          // 400 是服务端那三道闸(路径 / 类型 / 「这是 Miyu 自己的东西」)判的,理由原样给人看。
          if (response.status === 400) { rejected += 1; row.update(`被拒:${message || "服务端不收这个文件"}`, "is-danger"); continue; }
          if (!response.ok) { failed += 1; row.update(`失败:HTTP ${response.status}${message ? ` ${message}` : ""}`, "is-danger"); continue; }
          const saved = payload?.name || name;
          if (known.has(saved)) { replaced += 1; row.update("已存在:已覆盖", "is-warn"); } else { stored += 1; row.update("成功", "is-active"); }
          known.add(saved);
        } catch (error) {
          failed += 1;
          row.update(`失败:${error.message}`, "is-danger");
        }
      }
      ui.uploadHead.textContent = `上传记录(${rows.length})`;
    } finally {
      state.uploading = false;
    }
    const parts = [];
    if (stored) parts.push(`入库 ${stored} 个`);
    if (replaced) parts.push(`覆盖 ${replaced} 个`);
    if (skipped) parts.push(`跳过 ${skipped} 个`);
    if (rejected) parts.push(`被拒 ${rejected} 个`);
    if (failed) parts.push(`失败 ${failed} 个`);
    D.toast(parts.join(" · ") || "没有文件入库", rejected || failed ? "error" : undefined);
    if (stored || replaced) {
      // 逐文件导入不触发重建;整批完了起一次。失败(嵌入未配置)不算错。
      // 已经有一趟在跑时后端会排队(它的清单里没有这一批),所以这里照样接轮询。
      try {
        await D.api("/api/dash/kb/reindex", { method: "POST" });
        pollReindex(700);
      } catch (_) { /* 未配置嵌入 */ }
    }
    await loadOverview();
  }

  /* ── 删除 / 重建 / 更新 ─────────────────────────────── */
  async function bulkRemove() {
    const files = state.overview.files.filter((file) => state.picked.has(file.name));
    if (!files.length) return;
    const builtin = files.filter((file) => file.builtin).length;
    const ok = await D.confirmAction(`删除选中的 ${files.length} 个文件?${builtin ? `其中 ${builtin} 个是内置库文件,下次更新内置库时会回来。` : ""}\n\n文件和它们的语义块一起删除,不可撤销。`);
    if (!ok) return;
    await D.runBatch(files, (file) => D.api(`/api/dash/kb/files?name=${encodeURIComponent(file.name)}`, { method: "DELETE" }), "删除");
    state.picked.clear();
    if (files.some((file) => file.name === state.selected)) state.selected = null;
    await loadOverview();
  }

  async function removeFile(file) {
    const ok = await D.confirmAction(`删除 ${file.name}?${file.builtin ? "\n\n这是内置库文件,下次更新内置库时会回来。" : "\n\n文件和它的语义块一起删除,不可撤销。"}`);
    if (!ok) return;
    try {
      await D.api(`/api/dash/kb/files?name=${encodeURIComponent(file.name)}`, { method: "DELETE" });
      if (state.selected === file.name) state.selected = "";
      D.toast("已删除");
      await loadOverview();
    } catch (error) {
      D.toast(`删除失败:${error.message}`, "error");
    }
  }

  async function startReindex() {
    try {
      const result = await D.api("/api/dash/kb/reindex", { method: "POST" });
      D.toast(result.started ? "已开始重建语义索引" : "已排队:当前这趟跑完接着建");
      // 子进程从起到建锁有几百毫秒,后端会先写一帧「正在启动」占住这个窗口;
      // 这里再主动接上轮询,不指望概览那一帧恰好读到。
      pollReindex(700);
      await loadOverview();
    } catch (error) {
      D.toast(`无法重建:${error.message}`, "error");
    }
  }

  /* 轮询只刷卡片,不整份重拉概览:几千个文件的库里,概览一次要带回整棵文件树。
     状态接口本身就带回了语义块 / 陈旧 / 未索引三个数,够卡片用了。

     而且是原地改数字,不重建卡片——statCards 每次重建都会跑一遍数字滚动动画,
     两秒一次的话整排数字会一直在那儿跳。只有形态变了(开跑 / 收工 / 冒出陈旧锁)
     才重建,那时候按钮本来就得换一个。 */
  function reindexShape(r) {
    return [Boolean(r.running), Boolean(r.stale_lock), Boolean(r.configured), r.total > 0].join("|");
  }

  function applyReindexStatus(status) {
    const o = state.overview;
    if (!o) return;
    const changed = reindexShape(o.reindex || {}) !== reindexShape(status);
    o.reindex = status;
    for (const key of ["semantic_chunks", "stale_files", "unindexed_files"]) {
      if (typeof status[key] === "number") o[key] = status[key];
    }
    const grid = ui.cards.querySelector(".dash-cards");
    if (!grid || changed) { renderCards(); return; }
    const set = (label, value, hint) => {
      const card = [...grid.children].find((node) => node.querySelector(".dash-card-label")?.textContent === label);
      if (!card) return;
      card.querySelector(".dash-card-value").textContent = String(value);
      const hintNode = card.querySelector(".dash-card-hint");
      if (hintNode && hint != null) hintNode.textContent = hint;
    };
    set("语义块", o.semantic_chunks);
    set("待重建", o.stale_files + o.unindexed_files, `陈旧 ${o.stale_files} · 未索引 ${o.unindexed_files}`);
    const info = reindexCard(status);
    set(info.label, info.value, info.hint);
    const fill = grid.lastElementChild.querySelector(".dash-card-progress > i");
    if (fill && status.total > 0) {
      fill.style.width = `${reindexPercent(status.done, status.total)}%`;
    }
  }

  function pollReindex(delay = 2000) {
    clearTimeout(state.reindexTimer);
    state.reindexTimer = setTimeout(async () => {
      let status;
      try {
        status = await D.api("/api/dash/kb/reindex");
        state.reindexMisses = 0;
      } catch (_) {
        // 一次抖动不该让进度条就此停住;连续几次拿不到再放弃。
        if (++state.reindexMisses <= 5) pollReindex(3000);
        return;
      }
      applyReindexStatus(status);
      if (status.running) { pollReindex(); return; }
      D.toast(status.last_error ? `重建失败:${firstLine(status.last_error)}` : "语义索引重建完成",
        status.last_error ? "error" : undefined);
      await loadOverview();
    }, delay);
  }

  async function unlockReindex() {
    try {
      const result = await D.api("/api/dash/kb/reindex/lock", { method: "DELETE" });
      D.toast(result.cleared ? "已清理陈旧锁" : "锁不陈旧,未动");
      await loadOverview();
    } catch (error) {
      D.toast(`失败:${error.message}`, "error");
    }
  }

  async function startUpdate() {
    const ok = await D.confirmAction("从上游仓库拉取内置库并重新导入 default-kb/ 下全部文件?需要 git 与网络,通常几十秒。", "更新");
    if (!ok) return;
    try {
      await D.api("/api/dash/kb/default/update", { method: "POST" });
      await loadDefault();
    } catch (error) {
      D.toast(`无法开始更新:${error.message}`, "error");
    }
  }

  function pollUpdate() {
    clearTimeout(state.updateTimer);
    state.updateTimer = setTimeout(async () => {
      try {
        state.defaultKb = await D.api("/api/dash/kb/default");
        renderDefault();
        if (state.defaultKb.task?.running) { pollUpdate(); return; }
        D.toast(state.defaultKb.task?.error ? "内置库更新失败" : "内置库已更新", state.defaultKb.task?.error ? "error" : undefined);
        await loadOverview();
      } catch (_) { pollUpdate(); }
    }, 2500);
  }

  D.register({ name: "kb", root: "dashKbRoot", mount, refresh: () => reloadAll() });
})();
