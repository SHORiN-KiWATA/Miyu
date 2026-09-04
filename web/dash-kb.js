/*
 * 知识库面板(09-04)。
 *
 * 统计卡 + 内置库卡 → 搜索条(内容 / 文件名)→ 左目录树 / 右预览或搜索结果。
 * 上传(文件或整个文件夹)在前端按扩展名与大小预检,逐个 POST;删除、语义重建、
 * 内置库更新走各自接口,重建与更新都有轮询状态。数据来自 /api/dash/kb/*。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = {
    overview: null,
    defaultKb: null,
    mode: "browse",      // browse | search
    searchBy: "content",
    q: "",
    selected: "",
    collapsed: new Set(),
    uploadLog: [],
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
    ui.fileInput = D.el("input", { type: "file", multiple: true, hidden: true, onchange: () => queueUploads(Array.from(ui.fileInput.files), false) });
    ui.dirInput = D.el("input", { type: "file", hidden: true, onchange: () => queueUploads(Array.from(ui.dirInput.files), true) });
    ui.dirInput.setAttribute("webkitdirectory", "");
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "知识库" }),
      D.iconButton("refresh-cw", "刷新", () => reloadAll()),
      ui.stamp,
      D.el("span.dash-scope", null,
        D.el("button.dash-button.is-primary", { type: "button", onclick: () => ui.fileInput.click() }, D.icon("plus"), "上传文件"),
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
    reloadAll();
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
      ui.tree.append(D.el("p.dash-empty", { text: "库里还没有文件。上传文本、Markdown 或配置文件开始。" }));
      return;
    }
    const tree = buildTree(o.files);
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
    const row = D.el("div.dash-tree-row.is-file", { title: file.name, onclick: () => { state.selected = file.name; state.mode = "browse"; ui.search.value = ""; state.q = ""; renderTree(); renderMain(); } },
      D.el("span.dash-tree-indent", { text: "" }),
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
  function allowed(file) {
    const o = state.overview;
    if (!o) return { ok: true };
    const name = file.name.toLowerCase();
    const exts = (o.allowed_extensions || "").split(",").map((s) => s.trim()).filter(Boolean);
    const names = (o.allowed_filenames || "").split(",").map((s) => s.trim()).filter(Boolean);
    const dot = name.lastIndexOf(".");
    const ext = dot >= 0 ? name.slice(dot) : "";
    if (!(exts.includes(ext) || names.includes(name))) return { ok: false, reason: "类型不允许" };
    if (file.size > o.max_file_size_kb * 1024) return { ok: false, reason: `超过 ${o.max_file_size_kb} KB` };
    if (file.size === 0) return { ok: false, reason: "空文件" };
    return { ok: true };
  }

  async function queueUploads(files, keepPaths) {
    if (!files.length) return;
    ui.fileInput.value = "";
    ui.dirInput.value = "";
    state.uploadLog = [];
    ui.uploadLog.hidden = false;
    let done = 0, skipped = 0, failed = 0;
    for (const file of files) {
      const name = keepPaths && file.webkitRelativePath ? file.webkitRelativePath : file.name;
      const check = allowed(file);
      if (!check.ok) { skipped += 1; logUpload(name, `跳过:${check.reason}`, "is-muted"); continue; }
      try {
        const buffer = await file.arrayBuffer();
        const response = await fetch(`/api/dash/kb/files?name=${encodeURIComponent(name)}`, { method: "POST", body: buffer, headers: { "content-type": "application/octet-stream" } });
        const payload = await response.json().catch(() => null);
        if (!response.ok) throw new Error(payload?.error?.message || `HTTP ${response.status}`);
        done += 1;
        logUpload(name, "已入库", "is-active");
      } catch (error) {
        failed += 1;
        logUpload(name, `失败:${error.message}`, "is-danger");
      }
    }
    D.toast(`上传完成:${done} 成功 · ${skipped} 跳过 · ${failed} 失败`, failed ? "error" : undefined);
    if (done) {
      // 逐文件导入不触发重建;整批完了起一次。失败(嵌入未配置)不算错。
      try { await D.api("/api/dash/kb/reindex", { method: "POST" }); } catch (_) { /* 未配置嵌入 */ }
    }
    await loadOverview();
  }

  function logUpload(name, text, cls) {
    state.uploadLog.push({ name, text, cls });
    ui.uploadLog.replaceChildren(
      D.el("div.dash-upload-head", null, D.el("strong", { text: `上传记录(${state.uploadLog.length})` }), D.el("span.dash-actions-gap"), D.iconButton("x", "收起", () => { ui.uploadLog.hidden = true; })),
      D.el("ul.dash-upload-list", null, state.uploadLog.slice(-40).map((entry) => D.el("li", null, D.el("span.dash-cell-mono", { text: entry.name }), D.el(`span.dash-chip.${entry.cls}`, { text: entry.text })))));
  }

  /* ── 删除 / 重建 / 更新 ─────────────────────────────── */
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
