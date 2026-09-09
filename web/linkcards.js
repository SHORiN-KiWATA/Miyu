"use strict";

/*
 * 正文里的链接卡片。
 *
 * 只升级「独占一整行」的链接——一个段落从头到尾就是一条链接，那它多半是在
 * 推荐这个页面，配张图有用。夹在句子中间的行内链接一律不动：一段话里提三个
 * 网站就弹三张大卡，正文会被撑烂。
 *
 * 几条硬规矩，出问题一律退回普通链接、绝不让正文变样：
 *   · 每个消息最多 3 张卡，名额按「真的做出卡片」算——抓失败的那条不占名额，
 *     后面的链接还有机会；同一地址只升级第一次出现的那个。
 *   · 抓不到元数据 / 超时 / 不是 HTML → 什么都不做。
 *   · 没有 og:image → 画无图小卡（图标 + 标题 + 描述 + 域名）。
 *   · 流式输出期间不抓：地址还没写完就发请求纯属浪费。这里用防抖顶住，最后
 *     一次渲染之后才真正动手。
 *
 * 元数据和缩略图都走本机 `/api/link-preview`：WebUI 的 CSP 是 img-src 'self'
 * / connect-src 'self'，浏览器既取不到第三方页面也显示不了第三方图片，而放宽
 * 这两条等于让模型输出里的任意链接在你的浏览器上留一次带 IP 的请求。
 */
window.MiyuLinkCards = (() => {
  let makeIconSlot = null;

  const MAX_CARDS_PER_MESSAGE = 3;
  /** 最多试这么多条。抓失败的不该白占一个卡片名额——09-09 用户那条 bilibili
      抓挂了,结果排在它后面的维基链接连试都没试。 */
  const MAX_CANDIDATES = 6;
  /** 最后一次渲染之后等这么久才抓。流式重渲染每次都会把它推后。
      700 让每张卡片先白等 0.7s(09-09 用户嫌 B 站卡片慢),250 足够跳过
      流式重渲染的抖动。 */
  const SETTLE_MS = 250;

  /** url → Promise<preview|null>。同一地址整页只问一次。 */
  const lookups = new Map();
  const pending = new WeakMap();

  function previewFor(url) {
    if (lookups.has(url)) return lookups.get(url);
    const request = fetch(`/api/link-preview?url=${encodeURIComponent(url)}`, {
      credentials: "same-origin",
    })
      .then((response) => (response.ok ? response.json() : null))
      .then((payload) => (payload?.ok && payload.preview ? payload.preview : null))
      .catch(() => null);
    lookups.set(url, request);
    return request;
  }

  /** 这个段落是不是「整行只有一条链接」。 */
  function soleLinkOf(paragraph) {
    if (paragraph.dataset.linkCard) return null;
    let link = null;
    for (const node of paragraph.childNodes) {
      if (node.nodeType === Node.TEXT_NODE) {
        // 链接前后允许空白,别的字符都算「夹在句子里」。
        if (node.textContent.trim()) return null;
        continue;
      }
      if (node.nodeType !== Node.ELEMENT_NODE) continue;
      if (node.tagName === "BR") continue;
      if (node.tagName !== "A" || link) return null;
      link = node;
    }
    if (!link) return null;
    const href = link.getAttribute("href") || "";
    if (!/^https?:\/\//i.test(href)) return null;
    return link;
  }

  function media(preview) {
    if (!preview.image) return null;
    const frame = document.createElement("div");
    frame.className = "link-card-media";
    const image = document.createElement("img");
    image.src = `/api/link-preview/image/${encodeURIComponent(preview.image)}`;
    image.alt = "";
    image.loading = "lazy";
    image.decoding = "async";
    // 图挂了就把整块拿掉,别留一个破图框。
    image.addEventListener("error", () => frame.remove());
    frame.appendChild(image);
    return frame;
  }

  function icon(preview) {
    if (preview.icon) {
      const image = document.createElement("img");
      image.className = "link-card-icon";
      image.src = `/api/link-preview/image/${encodeURIComponent(preview.icon)}`;
      image.alt = "";
      image.loading = "lazy";
      image.addEventListener("error", () => image.remove());
      return image;
    }
    // 没有 favicon 就拿域名首字母占位,比一个空洞好看。
    const letter = document.createElement("span");
    letter.className = "link-card-icon is-letter";
    letter.textContent = (preview.site || preview.title || "?").trim().charAt(0).toUpperCase();
    return letter;
  }

  function buildCard(preview, href) {
    const card = document.createElement("a");
    card.className = "link-card";
    card.href = href;
    card.target = "_blank";
    card.rel = "noopener noreferrer";
    card.title = preview.title || href;

    const mediaNode = media(preview);
    if (mediaNode) card.appendChild(mediaNode);

    const body = document.createElement("div");
    body.className = "link-card-body";

    const head = document.createElement("div");
    head.className = "link-card-head";
    head.appendChild(icon(preview));
    const title = document.createElement("strong");
    title.className = "link-card-title";
    title.textContent = preview.title || href;
    head.appendChild(title);
    body.appendChild(head);

    if (preview.description) {
      const description = document.createElement("p");
      description.className = "link-card-desc";
      description.textContent = preview.description;
      body.appendChild(description);
    }

    const foot = document.createElement("div");
    foot.className = "link-card-foot";
    const site = document.createElement("span");
    site.className = "link-card-site";
    site.textContent = preview.site || new URL(href).host;
    foot.append(site);
    if (makeIconSlot) foot.appendChild(makeIconSlot("external-link", "link-card-arrow"));
    body.appendChild(foot);

    card.appendChild(body);
    return card;
  }

  async function upgrade(container) {
    const paragraphs = Array.from(container.querySelectorAll("p"));
    const seen = new Set();
    const targets = [];
    for (const paragraph of paragraphs) {
      if (targets.length >= MAX_CANDIDATES) break;
      const link = soleLinkOf(paragraph);
      if (!link) continue;
      const href = link.href;
      if (seen.has(href)) continue;
      seen.add(href);
      targets.push({ paragraph, href });
    }
    if (!targets.length) return;
    // 先打标记再等结果:这一批的重复渲染不该再排一次队。
    for (const target of targets) target.paragraph.dataset.linkCard = "pending";
    // 逐个落卡,谁先回来谁先画;以前 Promise.all 让一条慢链接拖住整批。
    // 名额按「真的做出卡片」来数,按抵达顺序先到先得。
    let made = 0;
    await Promise.all(
      targets.map((target) =>
        previewFor(target.href).then((preview) => {
          // 段落可能在等待期间被重渲染换掉了,那就别动。
          if (!target.paragraph.isConnected) return;
          if (!preview || made >= MAX_CARDS_PER_MESSAGE) {
            target.paragraph.dataset.linkCard = "none";
            return;
          }
          made += 1;
          target.paragraph.dataset.linkCard = "done";
          target.paragraph.classList.add("has-link-card");
          target.paragraph.replaceChildren(buildCard(preview, target.href));
        }),
      ),
    );
  }

  /**
   * 让这块正文里的独行链接变成卡片。流式期间可以反复调用,只有最后一次算数。
   */
  function scan(container) {
    if (!container) return;
    clearTimeout(pending.get(container));
    pending.set(
      container,
      setTimeout(() => {
        pending.delete(container);
        if (container.isConnected) upgrade(container).catch(() => {});
      }, SETTLE_MS)
    );
  }

  return {
    init(deps) {
      makeIconSlot = deps?.makeIconSlot || null;
    },
    scan,
  };
})();
