"use strict";

/*
 * 文件分享面板。
 *
 * 与 artifact 预览区是两个独立概念:artifact 是「演示回合产出」,这里是
 * 「把本机文件递给局域网里的人」的持续清单。数据全部来自 /api/shared,
 * 凭 WebUI 登录态访问;视频/音频/图片行内预览,其余只给下载。
 * 单独成文件:app.js 已经九千多行。
 */
window.MiyuShared = (() => {
  const SVG_NS = "http://www.w3.org/2000/svg";
  /*
   * lucide 图标子集。shared.js 先于 app.js 加载,拿不到那边的 createIcon,
   * 所以这里自带一份同风格的小表(path 数据同 lucide,24 viewBox / stroke 2)。
   */
  const ICONS = {
    music: [["path", { d: "M9 18V5l12-2v13" }], ["circle", { cx: "6", cy: "18", r: "3" }], ["circle", { cx: "18", cy: "16", r: "3" }]],
    film: [["rect", { x: "3", y: "3", width: "18", height: "18", rx: "2" }], ["path", { d: "M7 3v18" }], ["path", { d: "M3 7.5h4" }], ["path", { d: "M3 12h18" }], ["path", { d: "M3 16.5h4" }], ["path", { d: "M17 3v18" }], ["path", { d: "M17 7.5h4" }], ["path", { d: "M17 16.5h4" }]],
    image: [["rect", { x: "3", y: "3", width: "18", height: "18", rx: "2", ry: "2" }], ["circle", { cx: "9", cy: "9", r: "2" }], ["path", { d: "m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21" }]],
    file: [["path", { d: "M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" }], ["path", { d: "M14 2v6h6" }]],
    play: [["polygon", { points: "6 3 20 12 6 21 6 3", fill: "currentColor", stroke: "none" }]],
    pause: [["rect", { x: "14", y: "4", width: "4", height: "16", rx: "1", fill: "currentColor", stroke: "none" }], ["rect", { x: "6", y: "4", width: "4", height: "16", rx: "1", fill: "currentColor", stroke: "none" }]],
    download: [["path", { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }], ["polyline", { points: "7 10 12 15 17 10" }], ["line", { x1: "12", x2: "12", y1: "15", y2: "3" }]],
    "volume-2": [["path", { d: "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z" }], ["path", { d: "M16 9a5 5 0 0 1 0 6" }], ["path", { d: "M19.364 18.364a9 9 0 0 0 0-12.728" }]],
    "volume-x": [["path", { d: "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z" }], ["line", { x1: "22", x2: "16", y1: "9", y2: "15" }], ["line", { x1: "16", x2: "22", y1: "9", y2: "15" }]],
    "trash-2": [["path", { d: "M3 6h18" }], ["path", { d: "M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6" }], ["path", { d: "M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2" }], ["line", { x1: "10", x2: "10", y1: "11", y2: "17" }], ["line", { x1: "14", x2: "14", y1: "11", y2: "17" }]],
    "check-square": [["polyline", { points: "9 11 12 14 22 4" }], ["path", { d: "M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11" }]],
    upload: [["path", { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }], ["polyline", { points: "17 8 12 3 7 8" }], ["line", { x1: "12", x2: "12", y1: "3", y2: "15" }]]
  };
  const KIND_ICON = { video: "film", audio: "music", image: "image", other: "file" };
  const MODE_LABEL = { reference: "引用", snapshot: "快照" };
  let panel = null;
  let listBox = null;
  let uploadButton = null;
  let uploadInput = null;
  let uploadStatus = null;
  let uploading = false;
  /* 多选批量操作:选中集以 share_id 为键,render 时会剔除已不存在的条目。 */
  const selectedIds = new Set();
  let currentShares = [];
  let selectAllButton = null;
  let downloadSelectedButton = null;
  let deleteSelectedButton = null;

  function createIcon(name, className = "") {
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
    if (className) svg.setAttribute("class", className);
    const definition = ICONS[name] || ICONS.file;
    for (const [tag, attributes] of definition) {
      const node = document.createElementNS(SVG_NS, tag);
      for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, value);
      svg.appendChild(node);
    }
    return svg;
  }

  function kindIcon(kind) {
    return createIcon(KIND_ICON[kind] || KIND_ICON.other);
  }

  function formatSize(bytes) {
    const value = Number(bytes) || 0;
    if (value >= 1024 * 1024 * 1024) return `${(value / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
    if (value >= 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
    if (value >= 1024) return `${(value / 1024).toFixed(1)} KiB`;
    return `${value} B`;
  }

  function downloadUrl(id) {
    return `${location.origin}/api/shared/${encodeURIComponent(id)}?download=1`;
  }

  function formatTime(seconds) {
    if (!Number.isFinite(seconds) || seconds < 0) return "--:--";
    const total = Math.floor(seconds);
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const s = total % 60;
    const mm = h > 0 ? String(m).padStart(2, "0") : String(m);
    return `${h > 0 ? `${h}:` : ""}${mm}:${String(s).padStart(2, "0")}`;
  }

  /*
   * 自定义音频播放器卡片:隐藏原生 <audio>,用 JS 驱动。
   * 封面音符区 + 文件名/大小/时长 + 播放键 + 可拖进度条 + 静音/音量 + 下载,
   * 配色全部走主题 token,亮暗两套自动协调。
   */
  function buildAudioPlayer(src, fileName, sizeBytes) {
    const card = document.createElement("div");
    card.className = "media-audio-card";

    const audio = document.createElement("audio");
    audio.preload = "metadata";
    audio.src = src;

    /* ── 头部:封面 + 文件名/元信息 ── */
    const head = document.createElement("div");
    head.className = "media-audio-head";
    const cover = document.createElement("div");
    cover.className = "media-audio-cover";
    cover.appendChild(createIcon("music"));
    const info = document.createElement("div");
    info.className = "media-audio-info";
    const name = document.createElement("div");
    name.className = "media-audio-name";
    name.textContent = fileName;
    name.title = fileName;
    const meta = document.createElement("div");
    meta.className = "media-audio-meta";
    const sizeText = formatSize(sizeBytes);
    meta.textContent = `${sizeText} · --:--`;
    info.append(name, meta);
    head.append(cover, info);

    /* ── 控制行:播放 + 时间 + 进度条 + 静音/音量 + 下载 ── */
    const controls = document.createElement("div");
    controls.className = "media-audio-controls";

    const playButton = document.createElement("button");
    playButton.type = "button";
    playButton.className = "media-audio-play";
    playButton.title = "播放";
    playButton.setAttribute("aria-label", "播放");
    playButton.appendChild(createIcon("play"));

    const timeNow = document.createElement("span");
    timeNow.className = "media-audio-time";
    timeNow.textContent = "0:00";
    const timeTotal = document.createElement("span");
    timeTotal.className = "media-audio-time";
    timeTotal.textContent = "--:--";

    const track = document.createElement("div");
    track.className = "media-audio-track";
    track.setAttribute("role", "slider");
    track.setAttribute("tabindex", "0");
    track.setAttribute("aria-label", "播放进度");
    track.setAttribute("aria-valuemin", "0");
    track.setAttribute("aria-valuenow", "0");
    const fill = document.createElement("div");
    fill.className = "media-audio-track-fill";
    track.appendChild(fill);

    const muteButton = document.createElement("button");
    muteButton.type = "button";
    muteButton.className = "media-audio-icon-btn";
    muteButton.title = "静音";
    muteButton.setAttribute("aria-label", "静音");
    muteButton.appendChild(createIcon("volume-2"));

    const volume = document.createElement("input");
    volume.type = "range";
    volume.className = "media-audio-volume";
    volume.min = "0";
    volume.max = "1";
    volume.step = "0.05";
    volume.value = "1";
    volume.title = "音量";
    volume.setAttribute("aria-label", "音量");

    const downloadLink = document.createElement("a");
    downloadLink.className = "media-audio-icon-btn";
    downloadLink.href = `${src}?download=1`;
    downloadLink.setAttribute("download", fileName);
    downloadLink.title = "下载";
    downloadLink.setAttribute("aria-label", "下载");
    downloadLink.appendChild(createIcon("download"));

    controls.append(playButton, timeNow, track, timeTotal, muteButton, volume, downloadLink);
    card.append(head, controls, audio);

    /* ── 状态同步 ── */
    function syncPlayIcon() {
      const playing = !audio.paused && !audio.ended;
      playButton.replaceChildren(createIcon(playing ? "pause" : "play"));
      playButton.title = playing ? "暂停" : "播放";
      playButton.setAttribute("aria-label", playing ? "暂停" : "播放");
      card.classList.toggle("is-playing", playing);
    }

    function syncDuration() {
      const total = audio.duration;
      if (Number.isFinite(total) && total > 0) {
        timeTotal.textContent = formatTime(total);
        meta.textContent = `${sizeText} · ${formatTime(total)}`;
        track.setAttribute("aria-valuemax", String(Math.floor(total)));
        track.classList.remove("is-static");
      } else {
        track.classList.add("is-static");
      }
    }

    function syncProgress() {
      const total = audio.duration;
      const ratio = Number.isFinite(total) && total > 0 ? audio.currentTime / total : 0;
      fill.style.width = `${(Math.min(Math.max(ratio, 0), 1) * 100).toFixed(2)}%`;
      timeNow.textContent = formatTime(audio.currentTime);
      track.setAttribute("aria-valuenow", String(Math.floor(audio.currentTime || 0)));
      track.setAttribute("aria-valuetext", `${formatTime(audio.currentTime)} / ${timeTotal.textContent}`);
    }

    function syncVolume() {
      const muted = audio.muted || audio.volume === 0;
      muteButton.replaceChildren(createIcon(muted ? "volume-x" : "volume-2"));
      muteButton.title = muted ? "取消静音" : "静音";
      muteButton.setAttribute("aria-label", muted ? "取消静音" : "静音");
      volume.value = String(audio.muted ? 0 : audio.volume);
    }

    playButton.addEventListener("click", () => {
      if (audio.paused || audio.ended) audio.play();
      else audio.pause();
    });
    muteButton.addEventListener("click", () => {
      audio.muted = !audio.muted;
    });
    volume.addEventListener("input", () => {
      audio.volume = Number(volume.value);
      audio.muted = audio.volume === 0;
    });

    audio.addEventListener("play", syncPlayIcon);
    audio.addEventListener("pause", syncPlayIcon);
    audio.addEventListener("ended", syncPlayIcon);
    audio.addEventListener("loadedmetadata", () => { syncDuration(); syncProgress(); });
    audio.addEventListener("durationchange", syncDuration);
    audio.addEventListener("timeupdate", syncProgress);
    audio.addEventListener("volumechange", syncVolume);
    audio.addEventListener("error", () => {
      meta.textContent = `${sizeText} · 无法加载`;
      card.classList.add("is-error");
    });

    /* 进度条:点击/拖动跳转,键盘左右 ±5s。 */
    function seekToClientX(clientX) {
      const total = audio.duration;
      if (!Number.isFinite(total) || total <= 0) return;
      const rect = track.getBoundingClientRect();
      if (rect.width <= 0) return;
      const ratio = Math.min(Math.max((clientX - rect.left) / rect.width, 0), 1);
      audio.currentTime = ratio * total;
      syncProgress();
    }
    let dragging = false;
    track.addEventListener("pointerdown", (event) => {
      if (!Number.isFinite(audio.duration) || audio.duration <= 0) return;
      dragging = true;
      track.classList.add("is-scrubbing");
      track.setPointerCapture(event.pointerId);
      seekToClientX(event.clientX);
      event.preventDefault();
    });
    track.addEventListener("pointermove", (event) => {
      if (dragging) seekToClientX(event.clientX);
    });
    const stopDrag = (event) => {
      if (!dragging) return;
      dragging = false;
      track.classList.remove("is-scrubbing");
      if (track.hasPointerCapture(event.pointerId)) track.releasePointerCapture(event.pointerId);
    };
    track.addEventListener("pointerup", stopDrag);
    track.addEventListener("pointercancel", stopDrag);
    track.addEventListener("keydown", (event) => {
      if (!Number.isFinite(audio.duration) || audio.duration <= 0) return;
      if (event.key === "ArrowRight" || event.key === "ArrowUp") {
        audio.currentTime = Math.min(audio.currentTime + 5, audio.duration);
        event.preventDefault();
      } else if (event.key === "ArrowLeft" || event.key === "ArrowDown") {
        audio.currentTime = Math.max(audio.currentTime - 5, 0);
        event.preventDefault();
      } else if (event.key === " " || event.key === "Enter") {
        playButton.click();
        event.preventDefault();
      }
    });

    syncPlayIcon();
    syncVolume();
    return card;
  }

  /* http 局域网源没有 navigator.clipboard,退回 execCommand。 */
  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }
    const scratch = document.createElement("textarea");
    scratch.value = text;
    scratch.style.position = "fixed";
    scratch.style.opacity = "0";
    document.body.appendChild(scratch);
    scratch.select();
    try {
      document.execCommand("copy");
    } finally {
      scratch.remove();
    }
    return Promise.resolve();
  }

  function ensurePanel() {
    if (panel) return panel;
    panel = document.createElement("div");
    panel.className = "shared-files-overlay";
    panel.hidden = true;
    panel.innerHTML = `
      <div class="shared-files-panel" role="dialog" aria-label="分享文件">
        <header class="shared-files-header">
          <strong>分享文件</strong>
          <span class="shared-files-hint">局域网内能打开本 WebUI 的人都可下载</span>
          <button type="button" class="shared-files-refresh" title="刷新">↻</button>
          <button type="button" class="shared-files-close" title="关闭">×</button>
        </header>
        <div class="shared-files-list"></div>
      </div>`;
    panel.addEventListener("click", (event) => {
      if (event.target === panel) hide();
    });
    panel.querySelector(".shared-files-close").addEventListener("click", hide);
    panel.querySelector(".shared-files-refresh").addEventListener("click", refresh);
    listBox = panel.querySelector(".shared-files-list");

    /* 批量操作工具条:全选切换 + 下载所选 + 删除所选,插在 header 与列表之间。 */
    const toolbarBox = document.createElement("div");
    toolbarBox.className = "shared-files-toolbar";
    uploadInput = document.createElement("input");
    uploadInput.type = "file";
    uploadInput.multiple = true;
    uploadInput.hidden = true;
    uploadInput.addEventListener("change", () => {
      const files = Array.from(uploadInput.files || []);
      uploadInput.value = "";
      uploadFiles(files);
    });
    uploadButton = toolButton("upload", "上传", () => uploadInput.click());
    selectAllButton = toolButton("check-square", "全选", toggleSelectAll);
    downloadSelectedButton = toolButton("download", "下载所选", downloadSelected);
    deleteSelectedButton = toolButton("trash-2", "删除所选", deleteSelected, "danger");
    uploadStatus = document.createElement("span");
    uploadStatus.className = "shared-files-upload-status";
    uploadStatus.hidden = true;
    toolbarBox.append(uploadButton, selectAllButton, downloadSelectedButton, deleteSelectedButton, uploadStatus, uploadInput);
    const dialog = panel.querySelector(".shared-files-panel");
    dialog.insertBefore(toolbarBox, listBox);
    /* 整个面板都是投放区:拖文件进来即上传,不用先找按钮。 */
    let dragDepth = 0;
    dialog.addEventListener("dragenter", (event) => {
      if (!hasFiles(event)) return;
      event.preventDefault();
      dragDepth += 1;
      dialog.classList.add("is-dropping");
    });
    dialog.addEventListener("dragover", (event) => {
      if (!hasFiles(event)) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "copy";
    });
    dialog.addEventListener("dragleave", (event) => {
      if (!hasFiles(event)) return;
      dragDepth = Math.max(0, dragDepth - 1);
      if (dragDepth === 0) dialog.classList.remove("is-dropping");
    });
    dialog.addEventListener("drop", (event) => {
      if (!hasFiles(event)) return;
      event.preventDefault();
      dragDepth = 0;
      dialog.classList.remove("is-dropping");
      uploadFiles(Array.from(event.dataTransfer.files || []));
    });
    syncToolbar();

    document.body.appendChild(panel);
    return panel;
  }

  function toolButton(iconName, label, onClick, extraClass) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `shared-files-tool${extraClass ? ` is-${extraClass}` : ""}`;
    button.appendChild(createIcon(iconName));
    const text = document.createElement("span");
    text.textContent = label;
    button.appendChild(text);
    button.addEventListener("click", onClick);
    return button;
  }

  function hasFiles(event) {
    const types = event.dataTransfer?.types;
    return !!types && Array.from(types).includes("Files");
  }

  function setUploadStatus(text) {
    if (!uploadStatus) return;
    uploadStatus.hidden = !text;
    uploadStatus.textContent = text || "";
  }

  /*
   * 逐个 PUT 到 /api/shared:文件名与标题走 URL 编码头,正文原样流上去,
   * 服务端不设大小上限(受 plugins.file_sharing.max_shared_file_bytes 约束)。
   * 串行上传,避免多个大文件同时打满上行。
   */
  async function uploadFiles(files) {
    const list = files.filter((file) => file instanceof File && file.size > 0);
    if (!list.length || uploading) return;
    uploading = true;
    uploadButton.disabled = true;
    const failures = [];
    for (let index = 0; index < list.length; index += 1) {
      const file = list[index];
      setUploadStatus(`上传中 ${index + 1}/${list.length}:${file.name}(${formatSize(file.size)})`);
      try {
        const response = await fetch("/api/shared", {
          method: "POST",
          headers: {
            "content-type": "application/octet-stream",
            "x-miyu-filename": encodeURIComponent(file.name)
          },
          body: file
        });
        if (!response.ok) {
          let detail = `HTTP ${response.status}`;
          try {
            const payload = await response.json();
            if (payload?.error?.message) detail = String(payload.error.message);
          } catch (_) { /* 非 JSON 错误体 */ }
          failures.push(`${file.name}:${detail}`);
        }
      } catch (error) {
        failures.push(`${file.name}:${error.message || error}`);
      }
    }
    uploading = false;
    uploadButton.disabled = false;
    setUploadStatus(failures.length ? `失败 ${failures.length} 个:${failures.join(";")}` : "");
    await refresh();
  }

  function syncToolbar() {
    if (!selectAllButton) return;
    const total = currentShares.length;
    const count = selectedIds.size;
    const allSelected = total > 0 && count >= total;
    selectAllButton.disabled = total === 0;
    selectAllButton.querySelector("span").textContent = allSelected ? "取消全选" : "全选";
    downloadSelectedButton.disabled = count === 0;
    deleteSelectedButton.disabled = count === 0;
  }

  /* 全选/取消全选只改选中集与复选框状态,不重建行,免得收起已展开的预览。 */
  function toggleSelectAll() {
    const allSelected = currentShares.length > 0 && selectedIds.size >= currentShares.length;
    selectedIds.clear();
    if (!allSelected) {
      for (const share of currentShares) selectedIds.add(String(share.share_id));
    }
    for (const box of listBox.querySelectorAll(".shared-files-check")) {
      box.checked = selectedIds.has(box.dataset.shareId);
    }
    syncToolbar();
  }

  /* 删除所选:复用单条删除的 DELETE /api/shared/{id},一次汇总确认。 */
  async function deleteSelected() {
    const ids = [...selectedIds];
    if (!ids.length) return;
    if (!window.confirm(`删除 ${ids.length} 个分享?`)) return;
    for (const id of ids) {
      try {
        await fetch(`/api/shared/${encodeURIComponent(id)}`, { method: "DELETE" });
      } catch (_) {
        /* 单条失败不阻塞其余;refresh 后残留条目自然回到列表。 */
      }
    }
    selectedIds.clear();
    refresh();
  }

  /* 下载所选:隐藏 <a> 依次 click,间隔 300ms 防浏览器拦截连发下载。 */
  async function downloadSelected() {
    const ids = [...selectedIds];
    for (let i = 0; i < ids.length; i += 1) {
      const share = currentShares.find((item) => String(item.share_id) === ids[i]);
      const link = document.createElement("a");
      link.href = downloadUrl(ids[i]);
      link.setAttribute("download", share?.file_name || "");
      link.style.display = "none";
      document.body.appendChild(link);
      link.click();
      link.remove();
      if (i < ids.length - 1) await new Promise((resolve) => setTimeout(resolve, 300));
    }
  }

  function hide() {
    if (panel) panel.hidden = true;
  }

  async function refresh() {
    ensurePanel();
    listBox.textContent = "加载中…";
    let payload;
    try {
      const response = await fetch("/api/shared");
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      payload = await response.json();
    } catch (error) {
      listBox.textContent = `加载失败:${error.message || error}`;
      return;
    }
    render(Array.isArray(payload?.shares) ? payload.shares : []);
  }

  function render(shares) {
    currentShares = shares;
    const alive = new Set(shares.map((share) => String(share.share_id)));
    for (const id of [...selectedIds]) {
      if (!alive.has(id)) selectedIds.delete(id);
    }
    listBox.textContent = "";
    if (!shares.length) {
      const empty = document.createElement("p");
      empty.className = "shared-files-empty";
      empty.textContent = "还没有分享任何文件。点「上传」或把文件拖进来,也可以让 AI 调用 share_file。";
      listBox.appendChild(empty);
      syncToolbar();
      return;
    }
    for (const share of shares) listBox.appendChild(renderRow(share));
    syncToolbar();
  }

  function renderRow(share) {
    const row = document.createElement("div");
    row.className = "shared-files-row";
    const url = downloadUrl(share.share_id);

    const head = document.createElement("div");
    head.className = "shared-files-row-head";
    const shareId = String(share.share_id);
    const check = document.createElement("input");
    check.type = "checkbox";
    check.className = "shared-files-check";
    check.dataset.shareId = shareId;
    check.checked = selectedIds.has(shareId);
    check.setAttribute("aria-label", `选择 ${share.file_name}`);
    check.addEventListener("change", () => {
      if (check.checked) selectedIds.add(shareId);
      else selectedIds.delete(shareId);
      syncToolbar();
    });
    const icon = document.createElement("span");
    icon.className = "shared-files-icon";
    icon.appendChild(kindIcon(share.kind));
    const name = document.createElement("span");
    name.className = "shared-files-name";
    name.textContent = share.title || share.file_name;
    name.title = share.file_name;
    const meta = document.createElement("span");
    meta.className = "shared-files-meta";
    meta.textContent = `${formatSize(share.size_bytes)} · ${MODE_LABEL[share.mode] || share.mode}`;
    head.append(check, icon, name, meta);

    const actions = document.createElement("div");
    actions.className = "shared-files-actions";
    if (share.kind === "video" || share.kind === "audio" || share.kind === "image") {
      actions.appendChild(actionButton("预览", () => togglePreview(row, share)));
    }
    actions.appendChild(actionButton("复制链接", async (button) => {
      await copyText(url);
      const label = button.textContent;
      button.textContent = "已复制";
      setTimeout(() => { button.textContent = label; }, 1200);
    }));
    actions.appendChild(actionButton("下载", () => { window.open(url, "_blank"); }));
    actions.appendChild(actionButton("删除", async () => {
      if (!window.confirm(`删除分享「${share.file_name}」?`)) return;
      await fetch(`/api/shared/${encodeURIComponent(share.share_id)}`, { method: "DELETE" });
      refresh();
    }, "danger"));

    row.append(head, actions);
    return row;
  }

  function actionButton(label, onClick, extraClass) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `shared-files-action${extraClass ? ` is-${extraClass}` : ""}`;
    button.textContent = label;
    button.addEventListener("click", () => onClick(button));
    return button;
  }

  /* 行内预览:再点一次收起。src 不带 download=1,Range 由后端支持,可拖进度条。 */
  function togglePreview(row, share) {
    const existing = row.querySelector(".shared-files-preview");
    if (existing) {
      existing.remove();
      return;
    }
    const box = document.createElement("div");
    box.className = "shared-files-preview";
    const src = `/api/shared/${encodeURIComponent(share.share_id)}`;
    if (share.kind === "audio") {
      box.appendChild(buildAudioPlayer(src, share.file_name, share.size_bytes));
    } else if (share.kind === "video") {
      const shell = document.createElement("div");
      shell.className = "media-video-shell";
      const media = document.createElement("video");
      media.controls = true;
      media.preload = "metadata";
      media.src = src;
      shell.appendChild(media);
      box.appendChild(shell);
    } else {
      const media = document.createElement("img");
      media.alt = share.file_name;
      media.loading = "lazy";
      media.src = src;
      box.appendChild(media);
    }
    row.appendChild(box);
  }

  function toggle() {
    ensurePanel();
    if (panel.hidden) {
      panel.hidden = false;
      refresh();
    } else {
      panel.hidden = true;
    }
  }

  function isShareTool(name) {
    const value = String(name || "").toLowerCase();
    return value === "share_file" || value.startsWith("share_file:");
  }

  /*
   * 气泡内附件卡片:share_file 成功后直接挂在工具签下方(收起态也可见)。
   * 视频/音频/图片在气泡里内联预览;所有类型点击文件行即直接下载,
   * 不需要让 AI 转述链接再复制访问。面板列表只服务批量管理场景。
   */
  function renderCard(output) {
    const text = String(output || "").trim();
    if (!text.startsWith("{")) return null;
    let payload;
    try {
      payload = JSON.parse(text);
    } catch (_) {
      return null;
    }
    if (payload?.status !== "ok" || !payload.share_id || !payload.file_name) return null;
    const card = document.createElement("div");
    card.className = "shared-attachment";
    const kind = String(payload.kind || "other");
    const src = `/api/shared/${encodeURIComponent(payload.share_id)}`;
    if (kind === "audio") {
      /* 音频卡片自带文件名/大小/下载,不再额外挂下载条。 */
      card.appendChild(buildAudioPlayer(src, payload.file_name, payload.size_bytes));
      return card;
    }
    if (kind === "video" || kind === "image") {
      const box = document.createElement("div");
      box.className = "shared-attachment-preview";
      if (kind === "video") {
        const shell = document.createElement("div");
        shell.className = "media-video-shell";
        const media = document.createElement("video");
        media.controls = true;
        media.preload = "metadata";
        media.src = src;
        shell.appendChild(media);
        box.appendChild(shell);
      } else {
        const media = document.createElement("img");
        media.alt = payload.file_name;
        media.loading = "lazy";
        media.src = src;
        box.appendChild(media);
      }
      card.appendChild(box);
    }
    const row = document.createElement("a");
    row.className = "shared-attachment-row";
    row.href = `${src}?download=1`;
    row.setAttribute("download", payload.file_name);
    row.title = "下载";
    const icon = document.createElement("span");
    icon.className = "shared-attachment-icon";
    icon.appendChild(kindIcon(kind));
    const name = document.createElement("span");
    name.className = "shared-attachment-name";
    name.textContent = payload.file_name;
    const meta = document.createElement("span");
    meta.className = "shared-attachment-meta";
    meta.textContent = formatSize(payload.size_bytes);
    const hint = document.createElement("span");
    hint.className = "shared-attachment-download";
    hint.appendChild(createIcon("download"));
    row.append(icon, name, meta, hint);
    card.appendChild(row);
    return card;
  }

  document.addEventListener("DOMContentLoaded", () => {
    const button = document.getElementById("sharedFilesButton");
    if (button) button.addEventListener("click", toggle);
  });

  return { toggle, refresh, isShareTool, renderCard };
})();
