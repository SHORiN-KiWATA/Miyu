"use strict";

/*
 * 自己发给她的附件点开后的预览。
 *
 * 以前这些附件芯片就是一个 `<a download>`，点一下直接下载——想确认「我刚才
 * 到底传的是哪份」，只能先存到磁盘再打开。现在点芯片先看内容，下载留在芯片
 * 右边那个箭头上。
 *
 * 能预览什么按 MIME 分：文本类（含 markdown / json / csv）取回正文按等宽排
 * 版；PDF 塞 iframe（后端要带 `?inline=1`，否则 Content-Disposition 会把
 * iframe 变成一次下载）；音视频直接给播放器；图片走 MiyuLightbox，和会话里
 * 的图一个观感；其余二进制没有可看的形态，直接落回下载。
 *
 * 单独成文件的理由同 lightbox.js：app.js 已经将近一万行。
 * 依赖由 app.js 注入：makeIconSlot（图标）、formatFileSize（体积文案）。
 */
window.MiyuPreview = (() => {
  let makeIconSlot = null;
  let formatFileSize = null;
  let root = null;
  let body = null;
  let caption = null;
  let meta = null;
  let externalLink = null;
  let downloadLink = null;
  let lastFocused = null;

  /** 取回正文预览的上限。再大就只看开头，浏览器不该为一份日志卡住。 */
  const MAX_TEXT_BYTES = 512 * 1024;

  const TEXT_MIME_PREFIXES = ["text/"];
  const TEXT_MIME_EXACT = [
    "application/json",
    "application/x-ndjson",
    "application/xml",
    "application/x-yaml",
    "application/yaml",
    "application/toml",
    "application/javascript",
    "application/x-sh",
  ];

  function kindOf(attachment) {
    const mime = String(attachment?.mime || "").toLowerCase();
    if (attachment?.kind === "image" || mime.startsWith("image/")) return "image";
    if (mime.startsWith("video/")) return "video";
    if (mime.startsWith("audio/")) return "audio";
    if (mime === "application/pdf") return "pdf";
    if (TEXT_MIME_PREFIXES.some((prefix) => mime.startsWith(prefix))) return "text";
    if (TEXT_MIME_EXACT.includes(mime)) return "text";
    return "binary";
  }

  /** 有没有可看的形态。没有的话调用方就别把芯片做成可点的。 */
  function canPreview(attachment) {
    return kindOf(attachment) !== "binary";
  }

  function iconButton(icon, label, { tag = "button" } = {}) {
    const node = document.createElement(tag);
    if (tag === "button") node.type = "button";
    node.className = "icon-button preview-action";
    node.title = label;
    node.setAttribute("aria-label", label);
    node.appendChild(makeIconSlot(icon));
    return node;
  }

  function build() {
    root = document.createElement("div");
    root.className = "attachment-preview";
    root.hidden = true;
    root.setAttribute("role", "dialog");
    root.setAttribute("aria-modal", "true");
    root.setAttribute("aria-label", "附件预览");

    const scrim = document.createElement("button");
    scrim.type = "button";
    scrim.className = "attachment-preview-scrim";
    scrim.tabIndex = -1;
    scrim.setAttribute("aria-label", "关闭预览");
    scrim.addEventListener("click", close);

    const frame = document.createElement("div");
    frame.className = "attachment-preview-frame";

    const bar = document.createElement("div");
    bar.className = "attachment-preview-bar";

    const title = document.createElement("span");
    title.className = "attachment-preview-title";
    caption = document.createElement("strong");
    meta = document.createElement("small");
    title.append(caption, meta);

    externalLink = iconButton("external-link", "在新标签页打开", { tag: "a" });
    externalLink.target = "_blank";
    externalLink.rel = "noreferrer noopener";

    downloadLink = iconButton("download", "下载", { tag: "a" });
    downloadLink.setAttribute("download", "");

    const closeButton = iconButton("x", "关闭预览");
    closeButton.classList.add("attachment-preview-close");
    closeButton.addEventListener("click", close);

    bar.append(title, externalLink, downloadLink, closeButton);

    body = document.createElement("div");
    body.className = "attachment-preview-body";

    frame.append(bar, body);
    root.append(scrim, frame);
    document.body.appendChild(root);
  }

  function onKeydown(event) {
    if (event.key === "Escape") {
      event.stopPropagation();
      close();
    }
  }

  function showMessage(text) {
    const note = document.createElement("p");
    note.className = "attachment-preview-note";
    note.textContent = text;
    body.replaceChildren(note);
  }

  async function renderText(url) {
    showMessage("读取中…");
    let response;
    try {
      response = await fetch(url, { credentials: "same-origin" });
    } catch (_) {
      showMessage("读不到这个附件，可能已经被清理了。");
      return;
    }
    if (!response.ok) {
      showMessage(`读不到这个附件（HTTP ${response.status}）。`);
      return;
    }
    let text = await response.text();
    let clipped = false;
    if (text.length > MAX_TEXT_BYTES) {
      text = text.slice(0, MAX_TEXT_BYTES);
      clipped = true;
    }
    const pre = document.createElement("pre");
    pre.className = "attachment-preview-text";
    pre.textContent = text;
    body.replaceChildren(pre);
    if (clipped) {
      const note = document.createElement("p");
      note.className = "attachment-preview-note";
      note.textContent = "文件较大，只显示了开头部分；完整内容请下载。";
      body.appendChild(note);
    }
  }

  /**
   * 音视频后端本来就是内联交付的（`attachment_delivery` 里图片/音频/视频三类
   * 直接给真 MIME），所以给个播放器就行，不用 `?inline=1`。
   */
  function renderMedia(url, kind, mime) {
    const media = document.createElement(kind === "audio" ? "audio" : "video");
    media.className = `attachment-preview-${kind}`;
    media.controls = true;
    media.preload = "metadata";
    media.playsInline = true;
    const source = document.createElement("source");
    source.src = url;
    if (mime) source.type = mime;
    media.appendChild(source);
    // 浏览器不认这个编码时别留一个黑框,直说。
    media.addEventListener("error", () => {
      showMessage("这个格式浏览器放不了，下载下来用本地播放器打开吧。");
    });
    body.replaceChildren(media);
  }

  function renderPdf(url) {
    const frame = document.createElement("iframe");
    frame.className = "attachment-preview-pdf";
    // inline=1 才不会被 Content-Disposition: attachment 变成一次下载。
    frame.src = `${url}${url.includes("?") ? "&" : "?"}inline=1`;
    frame.title = "PDF 预览";
    body.replaceChildren(frame);
  }

  function open(attachment) {
    const url = attachment?.url;
    if (!url || !makeIconSlot) return false;
    const kind = kindOf(attachment);
    if (kind === "binary") return false;
    if (kind === "image") {
      // 图片交给会话内图片同一套放大预览，观感统一。
      window.MiyuLightbox?.open({ url, name: attachment.name || "" });
      return true;
    }
    if (!root) build();
    const name = attachment.name || "附件";
    caption.textContent = name;
    caption.title = name;
    meta.textContent = formatFileSize ? formatFileSize(attachment.size) : "";
    externalLink.href = kind === "pdf" ? `${url}?inline=1` : url;
    downloadLink.href = url;
    lastFocused = document.activeElement;
    root.hidden = false;
    document.body.classList.add("has-attachment-preview");
    document.addEventListener("keydown", onKeydown, true);
    requestAnimationFrame(() => root.classList.add("is-open"));
    if (kind === "pdf") renderPdf(url);
    else if (kind === "video" || kind === "audio") renderMedia(url, kind, attachment.mime);
    else renderText(url);
    return true;
  }

  function close() {
    if (!root || root.hidden) return;
    // 播放器不停就会在背后继续出声。
    for (const media of root.querySelectorAll("video, audio")) media.pause();
    root.classList.remove("is-open");
    root.hidden = true;
    // iframe 留着会一直占着 PDF 渲染器。
    body.replaceChildren();
    document.body.classList.remove("has-attachment-preview");
    document.removeEventListener("keydown", onKeydown, true);
    if (lastFocused && document.contains(lastFocused)) lastFocused.focus();
    lastFocused = null;
  }

  return {
    init(deps) {
      makeIconSlot = deps?.makeIconSlot || null;
      formatFileSize = deps?.formatFileSize || null;
    },
    canPreview,
    open,
    close,
    isOpen: () => Boolean(root) && !root.hidden,
  };
})();
