/*
 * 表情包面板(09-04)。
 *
 * 库作用域(默认当前人格映射的库)→ 统计卡 → 过滤条(状态 / 动图 / 来源 / 标签 /
 * 搜索,搜索用后端同款打分在前端复算)→ 画廊 → 抽屉(大图、编辑、来源、平台引用、
 * 启停、重分类、删除)。上传抽屉支持 AI 分类与手填两种模式,逐张排队。
 * 数据来自 /api/dash/memes/*。
 */
(() => {
  const D = window.MiyuDash;
  if (!D) return;

  const state = {
    library: D.recall("memes.library"),
    libraries: null,
    listing: null,
    refs: new Map(),
    filter: { state: "all", animated: "all", origin: "all", tag: "" },
    q: "",
    loadSeq: 0
  };
  const ui = {};

  const STATE_LABEL = { builtin: "内置", user: "自有", shadowed: "已覆盖", disabled: "已禁用" };

  function itemState(item) {
    if (item.disabled) return "disabled";
    if (item.shadowed) return "shadowed";
    return item.source;
  }
  const stateChip = (item) => {
    const s = itemState(item);
    const cls = { builtin: "is-builtin", user: "is-active", shadowed: "is-warn", disabled: "is-muted" }[s];
    return D.el(`span.dash-chip.${cls}`, { text: STATE_LABEL[s] });
  };
  const imageUrl = (item) => `/api/dash/memes/image?${new URLSearchParams({ library: state.library, id: item.id })}`;
  const libQuery = () => `library=${encodeURIComponent(state.library)}`;

  /* 与后端 score_meme 同一套打分,前端复算给"模型会看到的前三"提示。 */
  function normalize(value) {
    return value.toLowerCase().replace(/[!-\/:-@\[-`{-~，。！？、；：（）“”]/g, " ");
  }
  function terms(query) {
    const out = new Set();
    for (const token of query.split(/\s+/).filter(Boolean)) {
      if ([...token].length > 1) out.add(token);
      if (/[^\x00-\x7f]/.test(token)) {
        const chars = [...token];
        for (let i = 0; i + 1 < chars.length; i += 1) out.add(chars[i] + chars[i + 1]);
      }
    }
    return [...out];
  }
  function score(item, query) {
    const q = normalize(query);
    const ts = terms(q);
    if (!ts.length) return 0.1;
    const name = normalize(`${item.name.zh} ${item.name.en}`);
    const desc = normalize(item.description);
    const usage = normalize(item.usage);
    const tags = normalize(item.tags.join(" "));
    let s = 0;
    for (const t of ts) {
      if (tags.includes(t)) s += 3;
      if (name.includes(t)) s += 2.5;
      if (usage.includes(t)) s += 2;
      if (desc.includes(t)) s += 1.2;
    }
    if (q && `${name} ${desc} ${usage} ${tags}`.includes(q)) s += 2;
    return s;
  }

  /* ── 挂载 ─────────────────────────────────────────────── */
  function mount(root) {
    root.textContent = "";
    ui.stamp = D.el("small", { text: "" });
    ui.library = D.select([], state.library, (value) => { state.library = value; D.remember("memes.library", value); loadItems(); }, "表情库");
    ui.mapping = D.el("small.dash-scope-hint", { text: "" });
    const head = D.el("div.con-head", null,
      D.el("h2", { text: "表情包" }),
      D.iconButton("refresh-cw", "刷新", () => reloadAll()),
      ui.stamp,
      D.el("span.dash-scope", null, D.el("span.dash-scope-label", { text: "库" }), ui.library, ui.mapping,
        D.el("button.dash-button.is-primary", { type: "button", onclick: openUpload }, D.icon("plus"), "上传")));

    ui.cards = D.el("div");
    ui.tags = D.el("div.dash-tag-cloud");
    ui.search = D.el("input.dash-search", { type: "search", placeholder: "按名称、描述、用法、标签搜索…", oninput: () => {
      clearTimeout(ui.searchTimer);
      ui.searchTimer = setTimeout(() => { state.q = ui.search.value.trim(); renderGallery(); }, 200);
    } });
    const filterSelect = (key, options) => D.select(options, state.filter[key], (value) => { state.filter[key] = value; renderGallery(); });
    const toolbar = D.el("div.dash-toolbar", null,
      filterSelect("state", [{ value: "all", label: "全部状态" }, { value: "builtin", label: "内置" }, { value: "user", label: "自有" }, { value: "shadowed", label: "已覆盖" }, { value: "disabled", label: "已禁用" }]),
      filterSelect("animated", [{ value: "all", label: "静图 + 动图" }, { value: "yes", label: "仅动图" }, { value: "no", label: "仅静图" }]),
      filterSelect("origin", [{ value: "all", label: "全部来源" }, { value: "collected", label: "QQ 收集" }, { value: "manual", label: "手工添加" }]),
      D.el("label.dash-search-box", null, D.icon("search"), ui.search));
    ui.hint = D.el("p.dash-search-hint", { hidden: true });
    ui.gallery = D.el("div.dash-gallery");
    root.append(head, ui.cards, toolbar, ui.tags, ui.hint, ui.gallery);
    reloadAll();
  }

  async function reloadAll() {
    await loadLibraries();
    await loadItems();
  }

  async function loadLibraries() {
    try {
      const payload = await D.api("/api/dash/memes/libraries");
      state.libraries = payload;
      const names = payload.libraries.map((entry) => entry.name);
      if (!state.library || !names.includes(state.library)) state.library = payload.active;
      ui.library.textContent = "";
      for (const entry of payload.libraries) {
        const marks = [entry.builtin ? "内置" : null, entry.user ? "自有" : null].filter(Boolean).join("+");
        ui.library.append(D.el("option", { value: entry.name, text: `${entry.name}${entry.name === payload.active ? "(当前人格)" : ""}${marks ? ` · ${marks}` : ""}` }));
      }
      ui.library.value = state.library;
      const persona = payload.active_persona || "默认人格";
      ui.mapping.textContent = `人格 ${persona} → ${payload.active}`;
    } catch (error) {
      ui.stamp.textContent = `库清单加载失败:${error.message}`;
    }
  }

  async function loadItems() {
    const seq = ++state.loadSeq;
    ui.stamp.textContent = "载入中…";
    try {
      const listing = await D.api(`/api/dash/memes/items?${libQuery()}`);
      if (seq !== state.loadSeq) return;
      state.listing = listing;
      state.refs = new Map((listing.refs || []).map((r) => [r.meme_id, r]));
      renderCards();
      renderTags();
      renderGallery();
      ui.stamp.textContent = `${listing.library} · ${listing.stats.total} 张`;
    } catch (error) {
      if (seq !== state.loadSeq) return;
      ui.gallery.replaceChildren(D.el("p.dash-empty", { text: `加载失败:${error.message}` }));
      ui.stamp.textContent = "";
    }
  }

  function renderCards() {
    const s = state.listing.stats;
    const mtime = state.listing.index_mtime ? D.formatTime(state.listing.index_mtime * 1000) : "—";
    ui.cards.replaceChildren(D.statCards([
      { label: "总数", value: s.total, hint: `内置 ${s.builtin} · 自有 ${s.user}` },
      { label: "已覆盖", value: s.shadowed, hint: "自有条目盖住同图内置项" },
      { label: "已禁用", value: s.disabled, hint: "模型看不到,面板仍列出" },
      { label: "QQ 收集", value: s.collected, hint: `近 7 天 ${s.collected_7d}` },
      { label: "索引更新", value: mtime.slice(5), hint: state.listing.user_dir }
    ]));
  }

  function renderTags() {
    const counts = new Map();
    for (const item of state.listing.items) for (const tag of item.tags) counts.set(tag, (counts.get(tag) || 0) + 1);
    const top = [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 24);
    ui.tags.textContent = "";
    if (!top.length) return;
    for (const [tag, count] of top) {
      ui.tags.append(D.el(`button.dash-chip.is-clickable${state.filter.tag === tag ? ".is-active" : ""}`, { type: "button", text: `${tag} ${count}`, onclick: () => { state.filter.tag = state.filter.tag === tag ? "" : tag; renderTags(); renderGallery(); } }));
    }
  }

  function visibleItems() {
    const f = state.filter;
    let items = state.listing.items.filter((item) => {
      if (f.state !== "all" && itemState(item) !== f.state) return false;
      if (f.animated === "yes" && !item.animated) return false;
      if (f.animated === "no" && item.animated) return false;
      if (f.origin === "collected" && !item.origin) return false;
      if (f.origin === "manual" && item.origin) return false;
      if (f.tag && !item.tags.includes(f.tag)) return false;
      return true;
    });
    if (state.q) {
      items = items.map((item) => ({ item, s: score(item, state.q) })).filter((e) => e.s > 0).sort((a, b) => b.s - a.s).map((e) => e.item);
    }
    return items;
  }

  function renderGallery() {
    const items = visibleItems();
    ui.gallery.textContent = "";
    if (state.q) {
      const enabled = items.filter((item) => !item.disabled).slice(0, 3).map((item) => item.name.zh);
      ui.hint.hidden = false;
      ui.hint.textContent = enabled.length ? `模型搜索“${state.q}”会拿到前 3 张:${enabled.join(" · ")}` : `模型搜索“${state.q}”拿不到任何表情`;
    } else {
      ui.hint.hidden = true;
    }
    if (!items.length) {
      ui.gallery.append(D.el("p.dash-empty", { text: state.listing.items.length ? "没有匹配的表情。" : "这个库还是空的,上传几张吧。" }));
      return;
    }
    for (const item of items) {
      const refs = state.refs.get(item.id);
      const card = D.el("figure.dash-meme", { tabindex: "0", onclick: () => openDetail(item), onkeydown: (event) => { if (event.key === "Enter") openDetail(item); } },
        D.el("div.dash-meme-thumb", null,
          D.el("img", { src: imageUrl(item), alt: item.name.zh, loading: "lazy", decoding: "async" }),
          item.animated ? D.el("span.dash-meme-badge", { text: "GIF" }) : null,
          refs?.outbound ? D.el("span.dash-meme-badge.is-count", { text: `↑${refs.outbound}` }) : null),
        D.el("figcaption.dash-meme-cap", null, D.el("span.dash-meme-name", { text: item.name.zh }), stateChip(item)));
      card.classList.toggle("is-disabled", item.disabled);
      ui.gallery.append(card);
    }
  }

  /* ── 详情抽屉 ────────────────────────────────────────── */
  function openDetail(item) {
    const refs = state.refs.get(item.id);
    const form = {
      name_zh: D.el("input.dash-select.dash-wide", { type: "text", value: item.name.zh, maxlength: "80" }),
      name_en: D.el("input.dash-select.dash-wide", { type: "text", value: item.name.en, maxlength: "80", placeholder: "可选" }),
      description: D.el("textarea.dash-textarea", { rows: "3", maxlength: "500" }),
      usage: D.el("textarea.dash-textarea", { rows: "3", maxlength: "500" }),
      tags: D.el("input.dash-select.dash-wide", { type: "text", value: item.tags.join(", "), placeholder: "逗号分隔,最多 16 个" })
    };
    form.description.value = item.description;
    form.usage.value = item.usage;
    const image = D.el("img.dash-meme-full", { src: imageUrl(item), alt: item.name.zh });
    const meta = [
      ["ID", item.short_id], ["完整", item.id.replace("sha256:", "")], ["文件", item.file], ["类型", `${item.mime_type}${item.animated ? " · 动图" : ""}`],
      ["状态", STATE_LABEL[itemState(item)]]
    ];
    if (item.origin) {
      const o = item.origin;
      meta.push(["来源", `${o.platform} ${o.conversation_kind} ${o.conversation_id}`], ["发送者", `${o.sender_name || "?"}(${o.sender_id})`]);
      if (o.sent_at) meta.push(["发送于", D.formatTime(o.sent_at)]);
      if (o.collected_at) meta.push(["收集于", D.formatTime(o.collected_at)]);
    } else {
      meta.push(["来源", item.source === "builtin" ? "内置库" : "手工添加"]);
    }
    if (refs) meta.push(["平台引用", `收到 ${refs.inbound} · 发出 ${refs.outbound} · 最近 ${D.formatTime(refs.last_seen_at)}`]);

    const body = D.el("div", null,
      D.el("div.dash-meme-hero", null, image),
      D.el("div.dash-field-row", null, D.field("中文名", form.name_zh), D.field("英文名", form.name_en)),
      D.field("描述(图上是什么)", form.description),
      D.field("用法(什么时候发)", form.usage),
      D.field("标签", form.tags),
      item.source === "builtin" ? D.el("p.dash-banner", { text: "这是内置库条目:保存会把图片复制到自有库并生成覆盖项;删除只会禁用。" }) : null,
      D.el("h4.dash-section", { text: "元数据" }),
      D.el("dl.dash-meta", null, meta.flatMap(([key, value]) => [D.el("dt", { text: key }), D.el("dd", { text: String(value) })])));

    const classify = D.el("button.dash-button", { type: "button", text: "让模型重看", title: "调用视觉模型重新生成描述、用法、标签,填进表单不直接保存", onclick: async () => {
      classify.disabled = true; classify.textContent = "看图中…";
      try {
        const result = await D.api(`/api/dash/memes/items/${encodeURIComponent(item.id)}/classify?${libQuery()}`, { method: "POST" });
        form.name_zh.value = result.name?.zh || form.name_zh.value;
        form.name_en.value = result.name?.en || form.name_en.value;
        form.description.value = result.description || form.description.value;
        form.usage.value = result.usage || form.usage.value;
        form.tags.value = (result.tags || []).join(", ");
        D.toast(`模型建议已填入(置信 ${result.confidence})`);
      } catch (error) {
        D.toast(`重看失败:${error.message}`, "error");
      } finally {
        classify.disabled = false; classify.textContent = "让模型重看";
      }
    } });
    const toggle = D.el("button.dash-button", { type: "button", text: item.disabled ? "启用" : "禁用", onclick: () => patch(item, { enabled: item.disabled }) });
    const remove = D.el("button.dash-button.is-danger", { type: "button", text: item.source === "builtin" ? "禁用(内置)" : "删除", onclick: () => removeItem(item) });
    const save = D.el("button.dash-button.is-primary", { type: "button", text: "保存", onclick: () => patch(item, {
      name_zh: form.name_zh.value, name_en: form.name_en.value, description: form.description.value, usage: form.usage.value,
      tags: form.tags.value.split(/[,,、]/).map((t) => t.trim()).filter(Boolean)
    }) });
    D.openDrawer(item.name.zh, body, [classify, toggle, remove, save]);
  }

  async function patch(item, body) {
    try {
      await D.api(`/api/dash/memes/items/${encodeURIComponent(item.id)}?${libQuery()}`, { method: "PATCH", body });
      D.toast("已保存");
      D.closeDrawer();
      await loadItems();
    } catch (error) {
      D.toast(`保存失败:${error.message}`, "error");
    }
  }

  async function removeItem(item) {
    if (item.source === "builtin") {
      const ok = await D.confirmAction(`禁用内置表情「${item.name.zh}」?内置库文件不会删除,模型将看不到它;之后可在“已禁用”里重新启用。`, "禁用");
      if (!ok) return;
      return patch(item, { enabled: false });
    }
    const hard = await D.confirmAction(`删除「${item.name.zh}」?\n\n图片文件会进回收站,索引条目移除;平台引用记录保留。`, "删除");
    if (!hard) return;
    try {
      await D.api(`/api/dash/memes/items/${encodeURIComponent(item.id)}?${libQuery()}&hard=false`, { method: "DELETE" });
      D.toast("已删除");
      D.closeDrawer();
      await loadItems();
    } catch (error) {
      D.toast(`删除失败:${error.message}`, "error");
    }
  }

  /* ── 上传抽屉 ────────────────────────────────────────── */
  function openUpload() {
    const files = D.el("input", { type: "file", multiple: true, accept: "image/png,image/jpeg,image/gif,image/webp" });
    let mode = "ai";
    const modeSeg = D.segmented([{ value: "ai", label: "模型分类" }, { value: "manual", label: "手填元数据" }], mode, (value) => { mode = value; manualBox.hidden = value !== "manual"; });
    const form = {
      name_zh: D.el("input.dash-select.dash-wide", { type: "text", maxlength: "80", placeholder: "必填" }),
      name_en: D.el("input.dash-select.dash-wide", { type: "text", maxlength: "80", placeholder: "可选" }),
      description: D.el("textarea.dash-textarea", { rows: "2", maxlength: "500", placeholder: "图上是什么(必填)" }),
      usage: D.el("textarea.dash-textarea", { rows: "2", maxlength: "500", placeholder: "什么时候发(必填)" }),
      tags: D.el("input.dash-select.dash-wide", { type: "text", placeholder: "逗号分隔" })
    };
    const manualBox = D.el("div", { hidden: true }, D.field("中文名", form.name_zh), D.field("英文名", form.name_en), D.field("描述", form.description), D.field("用法", form.usage), D.field("标签", form.tags),
      D.el("p.dash-field-hint", { text: "手填模式下多张图共用同一套元数据,适合一次传一张。" }));
    const log = D.el("ul.dash-upload-list");
    const body = D.el("div", null,
      D.field("图片", files, "PNG / JPEG / GIF / WebP;每边 32–4096 px;GIF ≤120 帧 15 秒;单张 ≤ 配置上限"),
      D.field("入库方式", modeSeg.el, "模型分类会用视觉模型看图并严格把关,不合格会拒绝;拒绝后可切手填强制入库"),
      manualBox,
      D.el("h4.dash-section", { text: "结果" }), log);
    const submit = D.el("button.dash-button.is-primary", { type: "button", text: "开始上传", onclick: async () => {
      const list = Array.from(files.files || []);
      if (!list.length) { D.toast("先选图片", "error"); return; }
      if (mode === "manual" && (!form.name_zh.value.trim() || !form.description.value.trim() || !form.usage.value.trim())) { D.toast("手填模式要填中文名、描述、用法", "error"); return; }
      submit.disabled = true;
      let added = 0;
      for (const file of list) {
        const row = D.el("li", null, D.el("span.dash-cell-mono", { text: file.name }), D.el("span.dash-chip", { text: "上传中…" }));
        log.append(row);
        const chip = row.lastChild;
        try {
          const params = new URLSearchParams({ library: state.library, mode });
          if (mode === "manual") {
            params.set("name_zh", form.name_zh.value); params.set("name_en", form.name_en.value);
            params.set("description", form.description.value); params.set("usage", form.usage.value); params.set("tags", form.tags.value);
          }
          const response = await fetch(`/api/dash/memes/items?${params}`, { method: "POST", body: await file.arrayBuffer(), headers: { "content-type": "application/octet-stream" } });
          const payload = await response.json().catch(() => null);
          if (!response.ok) throw new Error(payload?.error?.message || `HTTP ${response.status}`);
          if (payload.already_exists) { chip.textContent = `已存在:${payload.name?.zh || ""}`; chip.className = "dash-chip is-warn"; }
          else if (payload.rejected) { chip.textContent = `模型拒绝:${payload.error || ""}`; chip.className = "dash-chip is-danger"; chip.title = payload.error || ""; }
          else if (payload.needs_user_info) { chip.textContent = `模型看不出来,切手填重传`; chip.className = "dash-chip is-danger"; chip.title = payload.error || ""; }
          else if (payload.success) { chip.textContent = `已入库:${payload.name?.zh || ""}`; chip.className = "dash-chip is-active"; added += 1; }
          else { chip.textContent = payload.message || "未知结果"; chip.className = "dash-chip is-warn"; }
        } catch (error) {
          chip.textContent = `失败:${error.message}`; chip.className = "dash-chip is-danger";
        }
      }
      submit.disabled = false;
      if (added) { D.toast(`入库 ${added} 张`); await loadItems(); }
    } });
    D.openDrawer(`上传到 ${state.library}`, body, [submit]);
  }

  D.register({ name: "memes", root: "dashMemesRoot", mount, refresh: () => reloadAll() });
})();
