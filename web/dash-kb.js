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

  function renderCards() {
    const o = state.overview;
    const embed = o.embedding_enabled ? (o.embedding_model ? `${o.embedding_provider_id} / ${o.embedding_model}` : "未配置模型") : "已关闭";
    const r = o.reindex || {};
    const reindexValue = r.running ? "进行中" : (r.stale_lock ? "锁陈旧" : "空闲");
    const cards = D.statCards([
      { label: "文件", value: o.file_count, hint: `内置 ${o.files.filter((f) => f.builtin).length} · 自有 ${o.files.filter((f) => !f.builtin).length}` },
      { label: "总大小", value: bytes(o.total_size_bytes), hint: `单文件上限 ${o.max_file_size_kb} KB` },
      { label: "语义块", value: o.semantic_chunks, hint: `嵌入:${embed}` },
      { label: "待重建", value: o.stale_files + o.unindexed_files, hint: `陈旧 ${o.stale_files} · 未索引 ${o.unindexed_files}` },
      { label: "重建", value: reindexValue, hint: r.stale_lock ? `锁已 ${Math.round((r.lock_age_secs || 0) / 60)} 分钟` : (r.configured ? "嵌入已配置" : "嵌入未配置,不会重建") }
    ]);
    const last = cards.lastElementChild;
    const actions = D.el("div.dash-card-actions");
    if (r.stale_lock) {
      actions.append(D.el("button.dash-button", { type: "button", text: "清理陈旧锁", onclick: unlockReindex }));
    } else if (!r.running) {
      const button = D.el("button.dash-button", { type: "button", text: "重建语义索引", onclick: startReindex });
      button.disabled = !r.configured;
      actions.append(button);
    }
    last.append(actions);
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
    const button = D.el("button.dash-button", { type: "button", text: task.running ? "更新中…" : "从上游更新", onclick: startUpdate });
    button.disabled = task.running || !d.bundled && !s.shorin_wiki_commit;
    ui.defaultCard.replaceChildren(D.el("div.dash-inline-card", null,
      D.el("span.dash-chip.is-builtin", { text: "内置库" }),
      D.el("span.dash-inline-main", { text: "Shorin ArchLinux Guide(default-kb/)" }),
      D.el("span.dash-cell-muted", { text: `本地 ${short(s.shorin_wiki_commit)} · 远端 ${short(s.remote_commit)} · 上次导入 ${s.last_imported_at ? D.formatTime(s.last_imported_at) : "—"}` }),
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
      ui.tree.append(D.bulkBar({
        count: state.picked.size, noun: "个文件",
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
      try { await D.api("/api/dash/kb/reindex", { method: "POST" }); } catch (_) { /* 未配置嵌入 */ }
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
      D.toast(result.started ? "已开始重建语义索引" : "重建已在进行");
      await loadOverview();
    } catch (error) {
      D.toast(`无法重建:${error.message}`, "error");
    }
  }

  function pollReindex() {
    clearTimeout(state.reindexTimer);
    state.reindexTimer = setTimeout(async () => {
      try {
        const status = await D.api("/api/dash/kb/reindex");
        if (status.running) { pollReindex(); return; }
        D.toast("语义索引重建完成");
        await loadOverview();
      } catch (_) { /* 下次刷新再看 */ }
    }, 5000);
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
