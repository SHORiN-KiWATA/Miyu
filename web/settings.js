"use strict";

/*
 * 设置页(09-04 重做)。
 *
 * 旧版把配置当 JSON 树按类型自动画表单,对象和数组一律扔 textarea 让人手写
 * JSON,QQ 平台整块没有页面。这里改成三层:页面只放卡片/行(概览),点开进
 * 右侧抽屉(详情),细项走弹出框/弹出菜单(微调)。所有 JSON 换成结构化编辑器,
 * 字段的中文标签、范围、枚举来自 settings-schema.js。
 *
 * 与 app.js 的分工:配置草稿(state.configDraft / promptDraft / secretChanges)、
 * 载入/保存/「高级」JSON 仍在 app.js;本文件只负责渲染与就地修改草稿,通过
 * init(ctx) 拿到那边的 state 与几个回调。加载顺序在 app.js 之前。
 *
 * CSP 禁内联 style 属性与内联事件,所以全部程序化生成,尺寸走 element.style。
 */
window.MiyuSettings = (() => {
  const SVG_NS = "http://www.w3.org/2000/svg";
  const ICONS = {
    x: [["path", { d: "M18 6 6 18" }], ["path", { d: "m6 6 12 12" }]],
    plus: [["path", { d: "M5 12h14" }], ["path", { d: "M12 5v14" }]],
    check: [["path", { d: "M20 6 9 17l-5-5" }]],
    ellipsis: [["circle", { cx: "12", cy: "12", r: "1" }], ["circle", { cx: "19", cy: "12", r: "1" }], ["circle", { cx: "5", cy: "12", r: "1" }]],
    "trash-2": [["path", { d: "M3 6h18" }], ["path", { d: "M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6" }], ["path", { d: "M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2" }], ["line", { x1: "10", x2: "10", y1: "11", y2: "17" }], ["line", { x1: "14", x2: "14", y1: "11", y2: "17" }]],
    "refresh-cw": [["path", { d: "M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" }], ["path", { d: "M21 3v5h-5" }], ["path", { d: "M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" }], ["path", { d: "M8 16H3v5" }]],
    "chevron-down": [["path", { d: "m6 9 6 6 6-6" }]],
    "chevron-right": [["path", { d: "m9 18 6-6-6-6" }]],
    star: [["path", { d: "M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z" }]],
    sparkles: [["path", { d: "M9.937 15.5A2 2 0 0 0 8.5 14.063l-6.135-1.582a.5.5 0 0 1 0-.962L8.5 9.936A2 2 0 0 0 9.937 8.5l1.582-6.135a.5.5 0 0 1 .963 0L14.063 8.5A2 2 0 0 0 15.5 9.937l6.135 1.581a.5.5 0 0 1 0 .964L15.5 14.063a2 2 0 0 0-1.437 1.437l-1.582 6.135a.5.5 0 0 1-.963 0z" }], ["path", { d: "M20 3v4" }], ["path", { d: "M22 5h-4" }]],
    "file-text": [["path", { d: "M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" }], ["path", { d: "M14 2v4a2 2 0 0 0 2 2h4" }], ["path", { d: "M10 9H8" }], ["path", { d: "M16 13H8" }], ["path", { d: "M16 17H8" }]],
    image: [["rect", { x: "3", y: "3", width: "18", height: "18", rx: "2", ry: "2" }], ["circle", { cx: "9", cy: "9", r: "2" }], ["path", { d: "m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21" }]],
    mic: [["path", { d: "M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" }], ["path", { d: "M19 10v2a7 7 0 0 1-14 0v-2" }], ["line", { x1: "12", x2: "12", y1: "19", y2: "22" }]],
    film: [["rect", { x: "3", y: "3", width: "18", height: "18", rx: "2" }], ["path", { d: "M7 3v18" }], ["path", { d: "M3 12h18" }], ["path", { d: "M17 3v18" }]],
    "file-type": [["path", { d: "M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" }], ["path", { d: "M14 2v4a2 2 0 0 0 2 2h4" }], ["path", { d: "M9 13v-1h6v1" }], ["path", { d: "M12 12v6" }], ["path", { d: "M11 18h2" }]],
    folder: [["path", { d: "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" }]],
    "message-circle": [["path", { d: "M7.9 20A9 9 0 1 0 4 16.1L2 22Z" }]],
    users: [["path", { d: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" }], ["circle", { cx: "9", cy: "7", r: "4" }], ["path", { d: "M22 21v-2a4 4 0 0 0-3-3.87" }], ["path", { d: "M16 3.13a4 4 0 0 1 0 7.75" }]],
    server: [["rect", { width: "20", height: "8", x: "2", y: "2", rx: "2", ry: "2" }], ["rect", { width: "20", height: "8", x: "2", y: "14", rx: "2", ry: "2" }], ["line", { x1: "6", x2: "6.01", y1: "6", y2: "6" }], ["line", { x1: "6", x2: "6.01", y1: "18", y2: "18" }]],
    "circle-alert": [["circle", { cx: "12", cy: "12", r: "10" }], ["line", { x1: "12", x2: "12", y1: "8", y2: "12" }], ["line", { x1: "12", x2: "12.01", y1: "16", y2: "16" }]],
    pencil: [["path", { d: "M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" }], ["path", { d: "m15 5 4 4" }]],
    "loader-circle": [["path", { d: "M21 12a9 9 0 1 1-6.219-8.56" }]],
    "key-round": [["path", { d: "M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z" }], ["circle", { cx: "16.5", cy: "7.5", r: ".5", fill: "currentColor" }]],
    search: [["circle", { cx: "11", cy: "11", r: "8" }], ["path", { d: "m21 21-4.3-4.3" }]],
    "arrow-up": [["path", { d: "m5 12 7-7 7 7" }], ["path", { d: "M12 19V5" }]],
    "arrow-down": [["path", { d: "M12 5v14" }], ["path", { d: "m19 12-7 7-7-7" }]]
  };

  let ctx = null;
  const S = () => ctx.state;
  const schema = () => window.MiyuSettingsSchema || {};
  const reducedMotion = () => {
    try { return window.matchMedia("(prefers-reduced-motion: reduce)").matches; } catch (_) { return false; }
  };

  /* ───────────────────────── DOM 小工具 ───────────────────────── */

  function icon(name, className = "") {
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("class", `st-icon${className ? ` ${className}` : ""}`);
    for (const [tag, attrs] of ICONS[name] || ICONS["circle-alert"]) {
      const node = document.createElementNS(SVG_NS, tag);
      for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
      svg.appendChild(node);
    }
    return svg;
  }

  /* el("div.a.b", { text, html, dataset, onclick... }, ...children) */
  function el(spec, attrs, ...children) {
    const [tag, ...classes] = spec.split(".");
    const node = document.createElement(tag || "div");
    if (classes.length) node.className = classes.join(" ");
    for (const [key, value] of Object.entries(attrs || {})) {
      if (value === null || value === undefined || value === false) continue;
      if (key === "text") node.textContent = value;
      else if (key === "dataset") Object.assign(node.dataset, value);
      else if (key.startsWith("on") && typeof value === "function") node.addEventListener(key.slice(2), value);
      else if (key === "value") node.value = value;
      else if (key === "checked" || key === "disabled" || key === "hidden" || key === "open") node[key] = Boolean(value);
      else node.setAttribute(key, value === true ? "" : value);
    }
    for (const child of children.flat(Infinity)) {
      if (child === null || child === undefined || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  function button(label, { kind = "secondary", iconName = null, onClick = null, title = null, small = false, danger = false } = {}) {
    const node = el(`button.st-btn.is-${kind}${small ? ".is-small" : ""}${danger ? ".is-danger" : ""}`, { type: "button", title, onclick: onClick });
    if (iconName) node.append(icon(iconName));
    if (label) node.append(el("span", { text: label }));
    if (!label && title) node.setAttribute("aria-label", title);
    return node;
  }

  function iconButton(name, title, onClick, extra = "") {
    const node = el(`button.st-icon-btn${extra ? `.${extra}` : ""}`, { type: "button", title, "aria-label": title, onclick: onClick });
    node.append(icon(name));
    return node;
  }

  function chip(text, cls = "") {
    return el(`span.st-chip${cls ? `.${cls}` : ""}`, { text });
  }

  function getPath(object, path, fallback) {
    let value = object;
    for (const key of String(path).split(".")) {
      if (value == null || typeof value !== "object" || !(key in value)) return fallback;
      value = value[key];
    }
    return value;
  }

  function setPath(object, path, value) {
    const keys = String(path).split(".");
    let target = object;
    for (const key of keys.slice(0, -1)) {
      if (!target[key] || typeof target[key] !== "object") target[key] = {};
      target = target[key];
    }
    target[keys[keys.length - 1]] = value;
  }

  function deletePath(object, path) {
    const keys = String(path).split(".");
    let target = object;
    for (const key of keys.slice(0, -1)) {
      if (!target?.[key] || typeof target[key] !== "object") return;
      target = target[key];
    }
    delete target[keys[keys.length - 1]];
  }

  function cfg(path, fallback) { return getPath(S().configDraft, path, fallback); }
  function setCfg(path, value) { setPath(S().configDraft, path, value); dirty(); }
  function dirty() { ctx.markConfigDirty(); ctx.updateAdvancedConfigEditor(); }
  function toast(message, type) { ctx.showToast(message, type); }
  function clone(value) { return value === undefined ? undefined : JSON.parse(JSON.stringify(value)); }

  function formatTokens(value) {
    const number = Number(value);
    if (!Number.isFinite(number) || number <= 0) return "";
    if (number >= 1_000_000) return `${(number / 1_000_000).toFixed(number % 1_000_000 ? 1 : 0)}M`;
    if (number >= 1000) return `${Math.round(number / 1000)}K`;
    return String(number);
  }

  function hashHue(text) {
    let hash = 0;
    for (const char of String(text)) hash = (hash * 31 + char.charCodeAt(0)) >>> 0;
    return hash % 360;
  }

  function initials(text) {
    const trimmed = String(text || "").trim();
    if (!trimmed) return "?";
    const words = trimmed.split(/[\s_-]+/).filter(Boolean);
    if (words.length >= 2) return (words[0][0] + words[1][0]).toUpperCase();
    return trimmed.slice(0, 2).toUpperCase();
  }

  function mark(text, cls = "") {
    const node = el(`span.st-mark${cls ? `.${cls}` : ""}`, { text: initials(text) });
    node.style.setProperty("--mark-hue", String(hashHue(text)));
    return node;
  }

  /* 局部刷新:每个页面挂到自己的根节点,改动后只重画自己;抽屉内部亦然。 */
  const pages = new Map();
  function rerender(name) {
    const page = pages.get(name);
    if (!page || !page.root) return;
    const scrollTop = page.root.closest(".settings-content")?.scrollTop || 0;
    // 首次画才放入场动画;之后的局部重画(关抽屉、改名同步卡片)不再重放,
    // 否则整页卡片又从头飘一遍,看起来像刷新。切换导航时 onShow 再放开。
    page.root.classList.toggle("is-settled", Boolean(page.rendered));
    page.rendered = true;
    page.root.replaceChildren();
    page.render(page.root);
    const content = page.root.closest(".settings-content");
    if (content) content.scrollTop = scrollTop;
  }

  /* ───────────────────────── 校验状态 ───────────────────────── */

  function setInvalid(input, message) {
    clearInvalid(input);
    const error = el("small.st-error", { text: message });
    input.classList.add("is-invalid");
    const host = input.closest(".st-row, .st-field") || input.parentElement;
    host?.appendChild(error);
    if (!reducedMotion()) {
      input.classList.remove("st-shake");
      void input.offsetWidth;
      input.classList.add("st-shake");
    }
    S().invalidConfigFields.set(input, error);
    ctx.updateSettingsControls();
  }

  function clearInvalid(input) {
    const error = S().invalidConfigFields.get(input);
    if (error) error.remove();
    S().invalidConfigFields.delete(input);
    input.classList.remove("is-invalid");
  }

  /* 抽屉关掉时,里面还挂着的错误要一并撤销,否则保存按钮永远灰着。 */
  function dropInvalidWithin(root) {
    for (const input of Array.from(S().invalidConfigFields.keys())) {
      if (root.contains(input)) clearInvalid(input);
    }
    ctx.updateSettingsControls();
  }

  /* ───────────────────────── 浮层:抽屉 / 菜单 / 弹出框 / 对话框 ───────────────────────── */

  let drawerState = null;

  /* openDrawer({ title, subtitle, tabs: [{id,label,render(body)}] | body, footer: [nodes], width, onClose }) */
  function openDrawer(options) {
    closeDrawer();
    const overlay = el("div.st-drawer-overlay", {
      onclick: (event) => { if (event.target === overlay) closeDrawer(); }
    });
    const panel = el("aside.st-drawer", { role: "dialog", "aria-label": options.title });
    if (options.width) panel.style.setProperty("--st-drawer-width", options.width);
    const heading = el("div.st-drawer-title", null,
      el("strong", { text: options.title }),
      options.subtitle ? el("small", { text: options.subtitle }) : null);
    const head = el("header.st-drawer-head", null, heading, iconButton("x", "关闭", () => closeDrawer()));
    const body = el("div.st-drawer-body");
    panel.append(head);
    const state = { overlay, panel, body, tab: null, options, tabsBar: null };
    if (Array.isArray(options.tabs) && options.tabs.length) {
      const bar = el("nav.st-tabs", { role: "tablist" });
      const indicator = el("i.st-tabs-indicator");
      bar.append(indicator);
      for (const tab of options.tabs) {
        const item = el("button.st-tab", { type: "button", role: "tab", dataset: { tab: tab.id }, text: tab.label, onclick: () => setDrawerTab(tab.id) });
        bar.append(item);
      }
      state.tabsBar = bar;
      panel.append(bar);
    }
    panel.append(body);
    if (options.footer?.length) panel.append(el("footer.st-drawer-foot", null, options.footer));
    overlay.append(panel);
    document.body.appendChild(overlay);
    document.addEventListener("keydown", onDrawerKey, true);
    drawerState = state;
    if (state.tabsBar) setDrawerTab(options.initialTab || options.tabs[0].id, true);
    else if (typeof options.body === "function") options.body(body);
    else if (options.body) body.append(options.body);
    return {
      close: closeDrawer,
      body,
      setTab: setDrawerTab,
      refresh: () => setDrawerTab(state.tab, true),
      setTitle: (text) => { heading.querySelector("strong").textContent = text; }
    };
  }

  function setDrawerTab(id, immediate = false) {
    const state = drawerState;
    if (!state?.tabsBar) return;
    const tab = state.options.tabs.find((item) => item.id === id) || state.options.tabs[0];
    const previous = state.tab;
    state.tab = tab.id;
    const buttons = Array.from(state.tabsBar.querySelectorAll(".st-tab"));
    let active = null;
    for (const item of buttons) {
      const on = item.dataset.tab === tab.id;
      item.classList.toggle("is-active", on);
      item.setAttribute("aria-selected", on ? "true" : "false");
      if (on) active = item;
    }
    const indicator = state.tabsBar.querySelector(".st-tabs-indicator");
    if (indicator && active) {
      indicator.style.setProperty("--x", `${active.offsetLeft}px`);
      indicator.style.setProperty("--w", `${active.offsetWidth}px`);
    }
    dropInvalidWithin(state.body);
    const paint = () => {
      const scrollTop = state.body.scrollTop;
      state.body.replaceChildren();
      state.body.classList.toggle("is-settled", immediate && previous === tab.id);
      tab.render(state.body);
      state.body.scrollTop = immediate && previous === tab.id ? scrollTop : 0;
      state.body.classList.remove("st-tab-enter");
      if (!reducedMotion() && !immediate) {
        void state.body.offsetWidth;
        state.body.classList.add("st-tab-enter");
      }
    };
    if (previous === tab.id || immediate) paint();
    else paint();
  }

  function onDrawerKey(event) {
    if (event.key !== "Escape") return;
    if (menuState || popoverState) return;
    event.stopPropagation();
    event.preventDefault();
    closeDrawer();
  }

  function closeDrawer() {
    const state = drawerState;
    if (!state) return;
    drawerState = null;
    closeMenu();
    closePopover();
    dropInvalidWithin(state.body);
    document.removeEventListener("keydown", onDrawerKey, true);
    const done = () => { state.overlay.remove(); state.options.onClose?.(); };
    if (reducedMotion()) return done();
    state.overlay.classList.add("is-closing");
    state.panel.addEventListener("animationend", done, { once: true });
    setTimeout(done, 260);
  }

  function drawerIsOpen() { return Boolean(drawerState); }

  /* 锚定浮层的通用定位:优先锚点下方,放不下就翻到上方;水平贴锚点左沿,溢出则贴右沿。 */
  function positionFloating(node, anchor, { align = "start", gap = 6 } = {}) {
    const rect = anchor.getBoundingClientRect();
    const scale = Number(getComputedStyle(document.documentElement).getPropertyValue("--ui-scale")) || 1;
    node.style.setProperty("visibility", "hidden");
    const width = node.offsetWidth;
    const height = node.offsetHeight;
    const viewportWidth = window.innerWidth / scale;
    const viewportHeight = window.innerHeight / scale;
    const r = { left: rect.left / scale, right: rect.right / scale, top: rect.top / scale, bottom: rect.bottom / scale };
    let left = align === "end" ? r.right - width : r.left;
    if (left + width > viewportWidth - 8) left = viewportWidth - width - 8;
    if (left < 8) left = 8;
    let top = r.bottom + gap;
    let flipped = false;
    if (top + height > viewportHeight - 8) {
      const above = r.top - gap - height;
      if (above >= 8) { top = above; flipped = true; }
      else top = Math.max(8, viewportHeight - height - 8);
    }
    node.style.setProperty("left", `${left}px`);
    node.style.setProperty("top", `${top}px`);
    node.style.removeProperty("visibility");
    node.classList.toggle("is-flipped", flipped);
  }

  let menuState = null;
  /* openMenu(anchor, [{ label, hint, icon, checked, danger, disabled, onSelect }], { align }) */
  function openMenu(anchor, items, options = {}) {
    closeMenu();
    const menu = el("div.st-menu", { role: "menu" });
    if (options.width) menu.style.setProperty("width", options.width);
    let first = null;
    for (const item of items) {
      if (item === "-" || item?.separator) { menu.append(el("hr.st-menu-sep")); continue; }
      if (item.heading) { menu.append(el("div.st-menu-heading", { text: item.heading })); continue; }
      const node = el(`button.st-menu-item${item.danger ? ".is-danger" : ""}${item.checked ? ".is-checked" : ""}`, {
        type: "button", role: "menuitem", disabled: item.disabled,
        onclick: () => { closeMenu(); item.onSelect?.(); }
      });
      node.append(el("span.st-menu-check", null, item.checked ? icon("check") : null));
      if (item.icon) node.append(icon(item.icon, "st-menu-icon"));
      node.append(el("span.st-menu-copy", null, el("strong", { text: item.label }), item.hint ? el("small", { text: item.hint }) : null));
      if (!first && !item.disabled) first = node;
      menu.append(node);
    }
    document.body.appendChild(menu);
    positionFloating(menu, anchor, { align: options.align || "start" });
    menuState = { menu, anchor };
    const onDoc = (event) => { if (!menu.contains(event.target) && event.target !== anchor && !anchor.contains(event.target)) closeMenu(); };
    const onKey = (event) => {
      if (event.key === "Escape") { event.stopPropagation(); event.preventDefault(); closeMenu(); anchor.focus?.(); }
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const nodes = Array.from(menu.querySelectorAll(".st-menu-item:not(:disabled)"));
        const index = nodes.indexOf(document.activeElement);
        const next = event.key === "ArrowDown" ? nodes[(index + 1) % nodes.length] : nodes[(index - 1 + nodes.length) % nodes.length];
        next?.focus();
      }
    };
    menuState.cleanup = () => {
      document.removeEventListener("pointerdown", onDoc, true);
      document.removeEventListener("keydown", onKey, true);
      window.removeEventListener("resize", closeMenu);
    };
    setTimeout(() => {
      if (menuState?.menu !== menu) return;
      document.addEventListener("pointerdown", onDoc, true);
      document.addEventListener("keydown", onKey, true);
      window.addEventListener("resize", closeMenu);
    }, 0);
    anchor.setAttribute("aria-expanded", "true");
    (first || menu).focus?.();
    return menu;
  }

  function closeMenu() {
    if (!menuState) return;
    const { menu, anchor, cleanup } = menuState;
    menuState = null;
    cleanup?.();
    anchor?.setAttribute("aria-expanded", "false");
    const done = () => menu.remove();
    if (reducedMotion()) return done();
    menu.classList.add("is-closing");
    menu.addEventListener("animationend", done, { once: true });
    setTimeout(done, 200);
  }

  let popoverState = null;
  /* openPopover(anchor, (body, close) => void, { width, title, align }) —— 单模型微调、限流这类小表单 */
  function openPopover(anchor, build, options = {}) {
    closePopover();
    closeMenu();
    const pop = el("div.st-popover", { role: "dialog", "aria-label": options.title || "" });
    pop.style.setProperty("width", options.width || "320px");
    const body = el("div.st-popover-body");
    if (options.title) pop.append(el("header.st-popover-head", null, el("strong", { text: options.title }), iconButton("x", "关闭", () => closePopover())));
    pop.append(body);
    document.body.appendChild(pop);
    build(body, () => closePopover());
    positionFloating(pop, anchor, { align: options.align || "end" });
    popoverState = { pop, anchor };
    const onDoc = (event) => {
      if (pop.contains(event.target) || anchor.contains(event.target)) return;
      if (event.target.closest?.(".st-menu")) return;
      closePopover();
    };
    const onKey = (event) => { if (event.key === "Escape") { event.stopPropagation(); event.preventDefault(); closePopover(); } };
    popoverState.cleanup = () => {
      document.removeEventListener("pointerdown", onDoc, true);
      document.removeEventListener("keydown", onKey, true);
      window.removeEventListener("resize", closePopover);
    };
    setTimeout(() => {
      if (popoverState?.pop !== pop) return;
      document.addEventListener("pointerdown", onDoc, true);
      document.addEventListener("keydown", onKey, true);
      window.addEventListener("resize", closePopover);
    }, 0);
    const focusable = body.querySelector("input, select, textarea, button");
    focusable?.focus();
    return { close: closePopover, body, reposition: () => positionFloating(pop, anchor, { align: options.align || "end" }) };
  }

  function closePopover() {
    if (!popoverState) return;
    const { pop, cleanup } = popoverState;
    popoverState = null;
    cleanup?.();
    dropInvalidWithin(pop);
    popoverState = null;
    const done = () => pop.remove();
    if (reducedMotion()) return done();
    pop.classList.add("is-closing");
    pop.addEventListener("animationend", done, { once: true });
    setTimeout(done, 200);
  }

  /* 居中对话框(原生 dialog):MCP 服务器、拉取结果这类需要专注的表单。 */
  function openDialog({ title, subtitle, body, actions, width, onClose }) {
    const dialog = el("dialog.st-dialog");
    if (width) dialog.style.setProperty("--st-dialog-width", width);
    const content = el("div.st-dialog-body");
    if (typeof body === "function") body(content); else if (body) content.append(body);
    const close = () => { if (dialog.open) dialog.close(); };
    const head = el("header.st-dialog-head", null,
      el("div.st-drawer-title", null, el("strong", { text: title }), subtitle ? el("small", { text: subtitle }) : null),
      iconButton("x", "关闭", close));
    dialog.append(head, content);
    if (actions?.length) dialog.append(el("footer.st-dialog-foot", null, actions));
    dialog.addEventListener("keydown", (event) => { if (event.key === "Escape") event.stopPropagation(); });
    dialog.addEventListener("cancel", (event) => { event.preventDefault(); close(); });
    dialog.addEventListener("click", (event) => { if (event.target === dialog) close(); });
    dialog.addEventListener("close", () => { dropInvalidWithin(dialog); dialog.remove(); onClose?.(); });
    document.body.appendChild(dialog);
    dialog.showModal();
    return { dialog, close, body: content };
  }

  function confirmAction(message, label = "删除") {
    return new Promise((resolve) => {
      let settled = false;
      const finish = (value) => { if (settled) return; settled = true; resolve(value); handle.close(); };
      const handle = openDialog({
        title: label,
        body: el("p.st-dialog-text", { text: message }),
        actions: [
          button("取消", { onClick: () => finish(false) }),
          button(label, { kind: "primary", danger: true, onClick: () => finish(true) })
        ],
        width: "380px",
        onClose: () => finish(false)
      });
    });
  }

  /* ───────────────────────── 基础控件 ───────────────────────── */

  function toggle(checked, onChange, label = "") {
    const node = el("button.toggle", { type: "button", role: "switch", "aria-checked": checked ? "true" : "false", "aria-label": label || undefined });
    node.addEventListener("click", () => {
      const next = node.getAttribute("aria-checked") !== "true";
      node.setAttribute("aria-checked", next ? "true" : "false");
      onChange(next);
    });
    return node;
  }

  function textInput(value, onInput, { type = "text", placeholder = "", mono = false, width = null, ariaLabel = null } = {}) {
    const node = el(`input.st-input${mono ? ".is-mono" : ""}`, { type, placeholder, "aria-label": ariaLabel, value: value ?? "" });
    if (width) node.style.setProperty("width", width);
    if (onInput) node.addEventListener("input", () => onInput(node.value, node));
    return node;
  }

  /* f32 经 JSON 变成 0.8999999761 这种尾数,显示时收成 7 位有效数字;写回仍是用户输入的值。 */
  function tidyNumber(value) {
    if (typeof value !== "number" || !Number.isFinite(value) || Number.isInteger(value)) return value ?? "";
    return Number.parseFloat(value.toPrecision(7));
  }

  function numberInput(field, value, onCommit) {
    const node = el("input.st-input.is-number", { type: "number", value: tidyNumber(value), "aria-label": field.label });
    if (field.min != null) node.min = String(field.min);
    if (field.max != null) node.max = String(field.max);
    node.step = field.step != null ? String(field.step) : field.integer ? "1" : "any";
    node.addEventListener("input", () => {
      clearInvalid(node);
      const raw = node.value.trim();
      if (!raw) {
        if (field.nullable) { onCommit(null); return; }
        setInvalid(node, "不能为空");
        return;
      }
      const number = Number(raw);
      if (!Number.isFinite(number)) return setInvalid(node, "请输入有效数字");
      if (field.integer && !Number.isInteger(number)) return setInvalid(node, "必须是整数");
      if (field.min != null && number < field.min) return setInvalid(node, `不能小于 ${field.min}`);
      if (field.max != null && number > field.max) return setInvalid(node, `不能大于 ${field.max}`);
      onCommit(number);
      ctx.updateSettingsControls();
    });
    return node;
  }

  function selectInput(choices, value, onChange, ariaLabel) {
    const wrap = el("span.st-select-wrap");
    const node = el("select.st-select", { "aria-label": ariaLabel });
    for (const choice of choices) {
      const item = typeof choice === "string" ? { value: choice, label: choice } : choice;
      node.append(el("option", { value: item.value, text: item.label }));
    }
    node.value = String(value ?? "");
    if (![...node.options].some((option) => option.value === node.value) && node.options.length) node.selectedIndex = 0;
    node.addEventListener("change", () => onChange(node.value));
    wrap.append(node, icon("chevron-down", "st-select-caret"));
    return wrap;
  }

  function textarea(value, onInput, { rows = 4, placeholder = "", mono = false, ariaLabel = null } = {}) {
    const node = el(`textarea.st-textarea${mono ? ".is-mono" : ""}`, { rows: String(rows), placeholder, "aria-label": ariaLabel });
    node.value = value ?? "";
    if (onInput) node.addEventListener("input", () => onInput(node.value, node));
    return node;
  }

  /* 分段:options [{value,label}] */
  function segmented(options, value, onChange) {
    const wrap = el("div.segmented", { role: "group" });
    const set = (next) => { for (const item of wrap.querySelectorAll("button")) item.classList.toggle("active", item.dataset.value === String(next)); };
    for (const option of options) {
      wrap.append(el("button", { type: "button", text: option.label, dataset: { value: String(option.value) }, onclick: () => { set(option.value); onChange(option.value); } }));
    }
    set(value);
    return wrap;
  }

  /* 设置行:左标题+说明,右控件。block=true 时控件占整行(列表、文本域)。 */
  function row(label, control, { hint = "", block = false, cls = "" } = {}) {
    const copy = el("div.lbl", null, el("strong", { text: label }), hint ? el("small", { text: hint }) : null);
    const node = el(`div.st-row${block ? ".is-block" : ""}${cls ? `.${cls}` : ""}`, null, copy, control ? el("div.st-row-control", null, control) : null);
    return node;
  }

  function card(rows, { title = "", description = "", actions = null, cls = "" } = {}) {
    const node = el(`section.st-card${cls ? `.${cls}` : ""}`);
    if (title || actions) {
      node.append(el("header.st-card-head", null,
        el("div", null, el("h3", { text: title }), description ? el("p", { text: description }) : null),
        actions ? el("div.st-card-actions", null, actions) : null));
    }
    node.append(el("div.st-card-body", null, rows));
    return node;
  }

  /* 可折叠区:disclosure(title, buildBody, { open, count }) */
  function disclosure(title, build, { open = false, hint = "" } = {}) {
    const node = el(`div.st-disclosure${open ? ".is-open" : ""}`);
    const body = el("div.st-disclosure-body");
    const inner = el("div.st-disclosure-inner");
    body.append(inner);
    let built = open;
    if (open) build(inner);
    const head = el("button.st-disclosure-head", { type: "button", "aria-expanded": open ? "true" : "false", onclick: () => {
      const willOpen = !node.classList.contains("is-open");
      if (willOpen && !built) { build(inner); built = true; }
      node.classList.toggle("is-open", willOpen);
      head.setAttribute("aria-expanded", willOpen ? "true" : "false");
    } }, icon("chevron-right", "st-disclosure-caret"), el("strong", { text: title }), hint ? el("small", { text: hint }) : null);
    node.append(head, body);
    return node;
  }

  function empty(text, actionNode = null) {
    return el("div.st-empty", null, el("p", { text }), actionNode);
  }

  /* 芯片列表编辑器:值是数组,回车/逗号/粘贴多个自动拆分;parse 负责校验单项。 */
  function chipList(values, onChange, { placeholder = "", parse = (text) => text, mono = false, ariaLabel = "" } = {}) {
    const list = Array.isArray(values) ? [...values] : [];
    const wrap = el(`div.st-chips${mono ? ".is-mono" : ""}`);
    const input = el("input.st-chips-input", { type: "text", placeholder, "aria-label": ariaLabel });
    const paint = () => {
      for (const node of wrap.querySelectorAll(".st-chip-item")) node.remove();
      list.forEach((value, index) => {
        const item = el("span.st-chip-item", null, el("span", { text: String(value) }), iconButton("x", "移除", () => {
          list.splice(index, 1);
          onChange([...list]);
          paint();
        }));
        wrap.insertBefore(item, input);
      });
    };
    const commit = (raw) => {
      const parts = String(raw).split(/[\s,;，；、]+/).map((item) => item.trim()).filter(Boolean);
      if (!parts.length) return;
      let changed = false;
      for (const part of parts) {
        let value;
        try { value = parse(part); } catch (error) { setInvalid(input, error.message); return; }
        if (value === null || value === undefined) continue;
        if (!list.some((existing) => String(existing) === String(value))) { list.push(value); changed = true; }
      }
      clearInvalid(input);
      input.value = "";
      if (changed) { onChange([...list]); paint(); }
    };
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === "," || event.key === "，") { event.preventDefault(); commit(input.value); }
      else if (event.key === "Backspace" && !input.value && list.length) { list.pop(); onChange([...list]); paint(); }
    });
    input.addEventListener("blur", () => { if (input.value.trim()) commit(input.value); });
    input.addEventListener("paste", (event) => {
      const text = event.clipboardData?.getData("text") || "";
      if (/[\s,;，；、]/.test(text.trim())) { event.preventDefault(); commit(text); }
    });
    wrap.addEventListener("click", (event) => { if (event.target === wrap) input.focus(); });
    wrap.append(input);
    paint();
    return wrap;
  }

  const parseQqId = (text) => {
    if (!/^\d{5,12}$/.test(text)) throw new Error(`无效号码：${text}`);
    return Number(text);
  };

  /* 键值表编辑器:值是对象;valueKind = "text" | "json"(值按 JSON 字面量解析,不合法退回字符串) */
  function kvTable(object, onChange, { keyPlaceholder = "键", valuePlaceholder = "值", valueKind = "text", secretValues = false } = {}) {
    const entries = Object.entries(object && typeof object === "object" ? object : {});
    const wrap = el("div.st-kv");
    const emit = () => {
      const next = {};
      for (const [key, value] of entries) if (key.trim()) next[key.trim()] = value;
      onChange(next);
    };
    const format = (value) => valueKind === "json" ? (typeof value === "string" ? value : JSON.stringify(value)) : String(value ?? "");
    const parse = (raw) => {
      if (valueKind !== "json") return raw;
      const trimmed = raw.trim();
      if (!trimmed) return "";
      try { return JSON.parse(trimmed); } catch (_) { return raw; }
    };
    const paint = () => {
      wrap.replaceChildren();
      entries.forEach((entry, index) => {
        const keyInput = textInput(entry[0], (value) => { entry[0] = value; emit(); }, { placeholder: keyPlaceholder, mono: true, ariaLabel: keyPlaceholder });
        const valueInput = textInput(format(entry[1]), (value) => { entry[1] = parse(value); emit(); }, { placeholder: valuePlaceholder, mono: true, ariaLabel: valuePlaceholder, type: secretValues ? "password" : "text" });
        wrap.append(el("div.st-kv-row", null, keyInput, el("span.st-kv-eq", { text: "=" }), valueInput, iconButton("trash-2", "删除", () => { entries.splice(index, 1); emit(); paint(); }, "is-danger")));
      });
      wrap.append(button("添加一项", { iconName: "plus", small: true, onClick: () => { entries.push(["", ""]); paint(); wrap.querySelector(".st-kv-row:last-of-type input")?.focus(); } }));
    };
    paint();
    return wrap;
  }

  /* 字符串列表(命令参数、关键词、允许扩展名):每行一个的可排序编辑器。 */
  function stringList(values, onChange, { placeholder = "", mono = false } = {}) {
    const list = Array.isArray(values) ? [...values] : [];
    const wrap = el("div.st-list");
    const emit = () => onChange(list.map((item) => String(item)));
    const paint = () => {
      wrap.replaceChildren();
      list.forEach((value, index) => {
        const input = textInput(value, (next) => { list[index] = next; emit(); }, { placeholder, mono, ariaLabel: `${placeholder || "项目"} ${index + 1}` });
        input.addEventListener("keydown", (event) => {
          if (event.key === "Enter") { event.preventDefault(); list.splice(index + 1, 0, ""); emit(); paint(); wrap.querySelectorAll("input")[index + 1]?.focus(); }
          if (event.key === "Backspace" && !input.value && list.length > 0) { event.preventDefault(); list.splice(index, 1); emit(); paint(); wrap.querySelectorAll("input")[Math.max(0, index - 1)]?.focus(); }
        });
        const rowNode = el("div.st-list-row", null,
          input,
          iconButton("arrow-up", "上移", () => { if (index === 0) return; [list[index - 1], list[index]] = [list[index], list[index - 1]]; emit(); paint(); }),
          iconButton("trash-2", "删除", () => { list.splice(index, 1); emit(); paint(); }, "is-danger"));
        wrap.append(rowNode);
      });
      wrap.append(button("添加一项", { iconName: "plus", small: true, onClick: () => { list.push(""); emit(); paint(); wrap.querySelector(".st-list-row:last-of-type input")?.focus(); } }));
    };
    paint();
    return wrap;
  }

  /* ───────────────────────── 密钥 ───────────────────────── */

  function secretStatus(key) {
    const change = S().secretChanges[key];
    if (change?.action === "clear") return { text: "将清空", cls: "is-warn" };
    if (change?.action === "set") return { text: "已输入新值", cls: "is-ok" };
    return S().secretStates[key] ? { text: "已配置", cls: "is-ok" } : { text: "未配置", cls: "" };
  }

  /* 单个密钥:密码框 + 状态签 + 清空/保留;留空即保留服务器上的现有值。 */
  function secretControl(key, { list = false, placeholder = "" } = {}) {
    const status = el("span.st-secret-status");
    const paintStatus = () => { const info = secretStatus(key); status.textContent = info.text; status.className = `st-secret-status ${info.cls}`; };
    const current = S().secretChanges[key]?.action === "set" ? S().secretChanges[key].value : "";
    const input = list
      ? textarea(current, null, { rows: 3, placeholder: placeholder || (S().secretStates[key] ? "留空保留现有值；每行一个" : "每行一个密钥"), mono: true, ariaLabel: key })
      : el("input.st-input.is-mono", { type: "password", autocomplete: "new-password", "aria-label": key, placeholder: placeholder || (S().secretStates[key] ? "留空保留现有值" : "输入新值"), value: current });
    input.addEventListener("input", () => {
      if (input.value) S().secretChanges[key] = { action: "set", value: input.value };
      else delete S().secretChanges[key];
      ctx.markConfigDirty();
      paintStatus();
    });
    const clear = button("清空", { kind: "text", small: true, danger: true, onClick: () => {
      input.value = "";
      S().secretChanges[key] = { action: "clear" };
      ctx.markConfigDirty();
      paintStatus();
    } });
    const keep = button("保留", { kind: "text", small: true, onClick: () => {
      input.value = "";
      delete S().secretChanges[key];
      ctx.markConfigDirty();
      paintStatus();
    } });
    paintStatus();
    return el("div.st-secret", null, el("div.st-secret-line", null, input, status), el("div.st-secret-actions", null, keep, clear));
  }

  /* ───────────────────────── 模型选择 ───────────────────────── */

  function providers() { return Array.isArray(S().configDraft?.providers) ? S().configDraft.providers : []; }
  function providerById(id) { return providers().find((provider) => String(provider?.id || "") === String(id || "")); }

  function providerModels(provider) {
    const models = Array.isArray(provider?.models) && provider.models.length ? provider.models : provider?.default_model ? [provider.default_model] : [];
    return models.map((model) => String(model)).filter((model) => model.trim());
  }

  function modelChoices() {
    const result = [];
    for (const provider of providers()) {
      for (const model of providerModels(provider)) {
        result.push({ provider_id: String(provider.id || ""), provider_name: String(provider.display_name || provider.id || ""), model, provider });
      }
    }
    return result;
  }

  function modelModalities(provider, model) {
    const declared = provider?.model_modalities;
    if (declared && typeof declared === "object" && Object.prototype.hasOwnProperty.call(declared, model)) {
      return Array.isArray(declared[model]) ? declared[model] : [];
    }
    const inferred = S().configInferredImageModels.some((item) => item?.provider_id === provider?.id && item?.model === model);
    return inferred ? ["text", "image"] : ["text"];
  }

  function supportsMedia(provider, model) { return modelModalities(provider, model).includes("image"); }
  function supportsVideo(provider, model) { return modelModalities(provider, model).includes("video"); }

  function sameRef(a, b) { return a?.provider_id === b?.provider_id && a?.model === b?.model; }

  /* 级联多选(供应商 → 模型):弹出框里按供应商分组勾选。
     options: { capability: "image"|"video"|null, inherit: {label, hint} | null } */
  function modelPoolControl(getValue, setValue, options = {}) {
    const anchor = el("button.st-picker", { type: "button", "aria-haspopup": "dialog" });
    const paint = () => {
      const value = getValue();
      anchor.replaceChildren();
      if (value === null || value === undefined) {
        anchor.append(el("span.st-picker-text.is-muted", { text: options.inherit?.label || "未设置" }));
      } else if (!value.length) {
        anchor.append(el("span.st-picker-text.is-muted", { text: options.emptyLabel || "未选择模型" }));
      } else {
        const visible = value.slice(0, 3);
        for (const item of visible) anchor.append(chip(item.model, "is-model"));
        if (value.length > visible.length) anchor.append(chip(`+${value.length - visible.length}`));
      }
      anchor.append(icon("chevron-down", "st-picker-caret"));
    };
    anchor.addEventListener("click", () => {
      openPopover(anchor, (body) => {
        const value = getValue();
        let selected = Array.isArray(value) ? value.map((item) => ({ provider_id: item.provider_id, model: item.model })) : [];
        let inherit = value === null || value === undefined;
        const commit = () => { setValue(inherit ? null : selected); paint(); };
        if (options.inherit) {
          const inheritRow = el("label.st-check-row.is-strong", null,
            el("input", { type: "checkbox", checked: inherit }),
            el("span", null, el("strong", { text: options.inherit.label }), options.inherit.hint ? el("small", { text: options.inherit.hint }) : null));
          inheritRow.querySelector("input").addEventListener("change", (event) => {
            inherit = event.target.checked;
            body.querySelectorAll(".st-pick-group input").forEach((input) => { input.disabled = inherit; });
            commit();
          });
          body.append(inheritRow);
        }
        const groups = new Map();
        for (const choice of modelChoices()) {
          if (options.capability === "image" && !supportsMedia(choice.provider, choice.model)) continue;
          if (options.capability === "video" && !supportsVideo(choice.provider, choice.model)) continue;
          if (!groups.has(choice.provider_id)) groups.set(choice.provider_id, { name: choice.provider_name, items: [] });
          groups.get(choice.provider_id).items.push(choice);
        }
        if (!groups.size) body.append(el("p.st-hint", { text: options.capability ? "没有具备该能力的模型，先去供应商里标注模态。" : "请先在供应商中配置模型。" }));
        for (const [providerId, group] of groups) {
          const groupNode = el("div.st-pick-group", null, el("div.st-pick-group-title", { text: group.name }));
          for (const choice of group.items) {
            const ref = { provider_id: providerId, model: choice.model };
            const input = el("input", { type: "checkbox", checked: selected.some((item) => sameRef(item, ref)), disabled: inherit });
            input.addEventListener("change", () => {
              if (input.checked) { if (!selected.some((item) => sameRef(item, ref))) selected = [...selected, ref]; }
              else selected = selected.filter((item) => !sameRef(item, ref));
              commit();
            });
            groupNode.append(el("label.st-check-row", null, input, el("span", null, el("strong", { text: choice.model }))));
          }
          body.append(groupNode);
        }
      }, { title: options.title || "选择模型", width: "340px" });
    });
    paint();
    return anchor;
  }

  /* 单选「供应商/模型」(识图、嵌入):下拉菜单,可清空。 */
  function modelRefControl(getValue, setValue, options = {}) {
    const anchor = el("button.st-picker", { type: "button", "aria-haspopup": "menu" });
    const paint = () => {
      const value = getValue();
      anchor.replaceChildren();
      if (!value?.provider_id) anchor.append(el("span.st-picker-text.is-muted", { text: options.emptyLabel || "未设置（自动）" }));
      else anchor.append(el("span.st-picker-text", null, el("strong", { text: value.model || providerById(value.provider_id)?.default_model || "" }), el("small", { text: providerById(value.provider_id)?.display_name || value.provider_id })));
      anchor.append(icon("chevron-down", "st-picker-caret"));
    };
    anchor.addEventListener("click", () => {
      const value = getValue();
      const items = [{ label: options.emptyLabel || "未设置（自动）", checked: !value?.provider_id, onSelect: () => { setValue(null); paint(); } }, "-"];
      let lastProvider = null;
      for (const choice of modelChoices()) {
        if (options.capability === "image" && !supportsMedia(choice.provider, choice.model)) continue;
        if (options.capability === "video" && !supportsVideo(choice.provider, choice.model)) continue;
        if (lastProvider !== choice.provider_id) { items.push({ heading: choice.provider_name }); lastProvider = choice.provider_id; }
        items.push({ label: choice.model, checked: value?.provider_id === choice.provider_id && value?.model === choice.model, onSelect: () => { setValue({ provider_id: choice.provider_id, model: choice.model }); paint(); } });
      }
      if (items.length === 2) items.push({ label: "没有可选模型", disabled: true });
      openMenu(anchor, items, { width: "300px" });
    });
    paint();
    return anchor;
  }

  /* ───────────────────────── schema 驱动的字段 ───────────────────────── */

  /* binding: { get(), set(value) };field 见 settings-schema.js 顶部注释。 */
  function fieldControl(field, binding) {
    const value = binding.get();
    const current = value === undefined ? field.default : value;
    switch (field.kind) {
      case "toggle":
        return toggle(Boolean(current), (next) => binding.set(next), field.label);
      case "number": {
        const input = numberInput(field, current ?? "", (next) => binding.set(next));
        if (field.unit) return el("span.st-unit-wrap", null, input, el("span.st-unit", { text: field.unit }));
        return input;
      }
      case "select":
        return selectInput(field.choices || [], current ?? "", (next) => binding.set(next), field.label);
      case "text":
        return textInput(current ?? "", (next) => binding.set(next), { placeholder: field.placeholder || "", mono: Boolean(field.mono), ariaLabel: field.label });
      case "textarea":
        return textarea(current ?? "", (next) => binding.set(next), { rows: field.rows || 4, placeholder: field.placeholder || "", mono: Boolean(field.mono), ariaLabel: field.label });
      case "secret":
        return secretControl(field.secretKey || binding.secretKey, {});
      case "secret-list":
        return secretControl(field.secretKey || binding.secretKey, { list: true });
      case "id-list":
        return chipList(Array.isArray(current) ? current : [], (next) => binding.set(next), { placeholder: field.placeholder || "输入号码后回车", parse: parseQqId, mono: true, ariaLabel: field.label });
      case "string-list":
        if (Array.isArray(field.choices) && field.choices.length) return choiceChips(field.choices, Array.isArray(current) ? current : [], (next) => binding.set(next));
        return field.inline
          ? chipList(Array.isArray(current) ? current : [], (next) => binding.set(next), { placeholder: field.placeholder || "输入后回车", ariaLabel: field.label })
          : stringList(Array.isArray(current) ? current : [], (next) => binding.set(next), { placeholder: field.placeholder || "", mono: Boolean(field.mono) });
      case "u32-list":
        return chipList(Array.isArray(current) ? current : [], (next) => binding.set(next), { placeholder: field.placeholder || "输入数字后回车", parse: (text) => { if (!/^\d{1,9}$/.test(text)) throw new Error(`无效数字：${text}`); return Number(text); }, mono: true, ariaLabel: field.label });
      case "kv":
        return kvTable(current, (next) => binding.set(next), { keyPlaceholder: field.keyPlaceholder || "键", valuePlaceholder: field.valuePlaceholder || "值", valueKind: field.valueKind || "text" });
      case "model-pool":
        return modelPoolControl(() => binding.get(), (next) => binding.set(next), {
          capability: field.capability || null,
          inherit: field.optional ? { label: field.inheritLabel || "继承", hint: field.inheritHint || "" } : null,
          title: field.label
        });
      case "model-ref":
        return modelRefControl(() => binding.get(), (next) => binding.set(next), { capability: field.capability || null, emptyLabel: field.emptyLabel });
      case "rate-limit":
        return rateLimitControl(() => binding.get(), (next) => binding.set(next), field);
      case "session-limits":
        return sessionLimitsControl(() => binding.get(), (next) => binding.set(next), field);
      case "identity-mappings":
        return identityMappingsControl(Array.isArray(current) ? current : [], (next) => binding.set(next));
      case "json":
      default: {
        const input = textarea(current == null ? "" : JSON.stringify(current, null, 2), null, { rows: 5, mono: true, ariaLabel: field.label });
        input.addEventListener("input", () => {
          clearInvalid(input);
          try { binding.set(input.value.trim() ? JSON.parse(input.value) : null); ctx.updateSettingsControls(); }
          catch (_) { setInvalid(input, "请输入有效 JSON"); }
        });
        return input;
      }
    }
  }

  const BLOCK_KINDS = new Set(["textarea", "secret", "secret-list", "id-list", "string-list", "u32-list", "kv", "identity-mappings", "json"]);

  function fieldRow(field, binding) {
    const control = fieldControl(field, binding);
    const block = BLOCK_KINDS.has(field.kind) && !(field.kind === "string-list" && field.inline);
    return row(field.label, control, { hint: field.hint || "", block, cls: `is-${field.kind}` });
  }

  /* 多选芯片(固定选项):星期这类。 */
  function choiceChips(choices, values, onChange) {
    const selected = new Set(values.map(String));
    const wrap = el("div.st-choice-chips", { role: "group" });
    for (const choice of choices) {
      const node = el("button.st-choice-chip", { type: "button", text: choice.label, "aria-pressed": selected.has(String(choice.value)) ? "true" : "false", onclick: () => {
        if (selected.has(String(choice.value))) selected.delete(String(choice.value)); else selected.add(String(choice.value));
        node.setAttribute("aria-pressed", selected.has(String(choice.value)) ? "true" : "false");
        onChange(choices.map((item) => item.value).filter((value) => selected.has(String(value))));
      } });
      wrap.append(node);
    }
    return wrap;
  }

  /* 把一组 schema 字段渲染成行;bindingFor(field, keyOverride) 给出读写。
     model-ref 由 providerKey/modelKey 两个键合成;showWhen 在每次改动后重算。 */
  function fieldRows(fields, bindingFor) {
    const visible = (fields || []).filter((field) => !field.hidden);
    const conditional = [];
    const nodes = [];
    const refresh = () => {
      for (const { node, field } of conditional) {
        const source = visible.find((item) => (item.key || item.path) === field.showWhen.key);
        const value = source ? bindingFor(source).get() : bindingFor(field, field.showWhen.key).get();
        const shown = Array.isArray(field.showWhen.value) ? field.showWhen.value.includes(value ?? source?.default) : (value ?? source?.default) === field.showWhen.value;
        node.hidden = !shown;
      }
    };
    for (const field of visible) {
      let binding;
      if (field.kind === "model-ref") {
        const providerBinding = bindingFor(field, field.providerKey);
        const modelBinding = bindingFor(field, field.modelKey);
        binding = {
          get: () => ({ provider_id: providerBinding.get() || "", model: modelBinding.get() || "" }),
          set: (value) => { providerBinding.set(value?.provider_id || ""); modelBinding.set(value?.model || ""); }
        };
      } else {
        binding = bindingFor(field);
      }
      const wrapped = { ...binding, set: (value) => { binding.set(value); refresh(); } };
      const node = fieldRow(field, wrapped);
      if (field.showWhen) conditional.push({ node, field });
      nodes.push(node);
    }
    refresh();
    return nodes;
  }

  /* 绑定到 configDraft 上的完整路径。 */
  function pathBinding(path, { nullable = false } = {}) {
    return {
      get: () => cfg(path),
      set: (value) => {
        if (value === null && nullable) { deletePath(S().configDraft, path); dirty(); return; }
        setCfg(path, value);
      }
    };
  }

  /* 绑定到某个对象的键(插件 settings、路由项等)。 */
  function objectBinding(object, key, { onSet = null } = {}) {
    return {
      get: () => object?.[key],
      set: (value) => {
        if (value === null || value === undefined) delete object[key];
        else object[key] = value;
        dirty();
        onSet?.(value);
      }
    };
  }

  /* 「N 条 / M 秒」限流小控件:按钮显示摘要,点开弹出框改。 */
  function rateLimitControl(getValue, setValue, field) {
    const anchor = el("button.st-picker.is-compact", { type: "button" });
    const paint = () => {
      const value = getValue() || field.default || { max_messages: 0, window_seconds: 0 };
      anchor.replaceChildren(el("span.st-picker-text", null, el("strong", { text: `${value.max_messages} 条` }), el("small", { text: `/ ${value.window_seconds} 秒` })), icon("pencil", "st-picker-caret"));
    };
    anchor.addEventListener("click", () => {
      openPopover(anchor, (body) => {
        const value = { ...(field.default || { max_messages: 2, window_seconds: 600 }), ...(getValue() || {}) };
        body.append(
          row("窗口内最多条数", numberInput({ label: "条数", min: 1, max: 100000, integer: true }, value.max_messages, (next) => { value.max_messages = next; setValue({ ...value }); paint(); })),
          row("窗口秒数", numberInput({ label: "秒数", min: 1, max: 86400, integer: true }, value.window_seconds, (next) => { value.window_seconds = next; setValue({ ...value }); paint(); }))
        );
      }, { title: field.label, width: "300px" });
    });
    paint();
    return anchor;
  }

  function sessionLimitsControl(getValue, setValue, field) {
    const anchor = el("button.st-picker.is-compact", { type: "button" });
    const paint = () => {
      const value = getValue();
      anchor.replaceChildren();
      if (!value && field.optional) anchor.append(el("span.st-picker-text.is-muted", { text: field.inheritLabel || "继承" }));
      else {
        const limits = value || field.default || { running: 1, queued: 8 };
        anchor.append(el("span.st-picker-text", null, el("strong", { text: `并行 ${limits.running}` }), el("small", { text: `/ 排队 ${limits.queued}` })));
      }
      anchor.append(icon("pencil", "st-picker-caret"));
    };
    anchor.addEventListener("click", () => {
      openPopover(anchor, (body) => {
        let value = getValue() ? { ...getValue() } : null;
        const form = el("div");
        const paintForm = () => {
          form.replaceChildren();
          if (!value) return;
          form.append(
            row("并行运行数量", numberInput({ label: "并行", min: 1, max: 16, integer: true }, value.running, (next) => { value.running = next; setValue({ ...value }); paint(); })),
            row("等待队列数量", numberInput({ label: "排队", min: 0, max: 64, integer: true }, value.queued, (next) => { value.queued = next; setValue({ ...value }); paint(); }))
          );
        };
        if (field.optional) {
          const check = el("input", { type: "checkbox", checked: Boolean(value) });
          check.addEventListener("change", () => {
            value = check.checked ? { ...(field.default || { running: 1, queued: 8 }) } : null;
            setValue(value ? { ...value } : null);
            paint();
            paintForm();
          });
          body.append(el("label.st-check-row.is-strong", null, check, el("span", null, el("strong", { text: "覆盖并发配置" }), el("small", { text: field.inheritHint || "不勾选则继承上层设置" }))));
        }
        body.append(form);
        paintForm();
      }, { title: field.label, width: "300px" });
    });
    paint();
    return anchor;
  }

  /* 识人映射:昵称 ↔ QQ 号 的行表 */
  function identityMappingsControl(values, onChange) {
    const list = values.map((item) => ({ nickname: String(item?.nickname || ""), user_id: item?.user_id ?? "" }));
    const wrap = el("div.st-kv");
    const emit = () => onChange(list.filter((item) => item.nickname.trim() && Number.isSafeInteger(Number(item.user_id))).map((item) => ({ nickname: item.nickname.trim(), user_id: Number(item.user_id) })));
    const paint = () => {
      wrap.replaceChildren();
      list.forEach((item, index) => {
        const nick = textInput(item.nickname, (next) => { item.nickname = next; emit(); }, { placeholder: "昵称", ariaLabel: "昵称" });
        const id = textInput(item.user_id, (next, node) => {
          clearInvalid(node);
          if (next && !/^\d{5,12}$/.test(next)) return setInvalid(node, "QQ 号应为 5–12 位数字");
          item.user_id = next; emit();
        }, { placeholder: "QQ 号", mono: true, ariaLabel: "QQ 号" });
        wrap.append(el("div.st-kv-row", null, nick, el("span.st-kv-eq", { text: "→" }), id, iconButton("trash-2", "删除", () => { list.splice(index, 1); emit(); paint(); }, "is-danger")));
      });
      wrap.append(button("添加映射", { iconName: "plus", small: true, onClick: () => { list.push({ nickname: "", user_id: "" }); paint(); wrap.querySelector(".st-kv-row:last-of-type input")?.focus(); } }));
    };
    paint();
    return wrap;
  }

  /* ───────────────────────── 供应商引用维护(从 app.js 搬来) ───────────────────────── */

  const PLATFORM_POOL_NAMES = ["text_models", "multimodal_models", "non_whitelist_text_models"];

  function forEachPlatformPool(callback) {
    const qq = S().configDraft?.platforms?.qq;
    if (!qq || typeof qq !== "object") return;
    for (const poolName of PLATFORM_POOL_NAMES) {
      if (Array.isArray(qq[poolName])) callback(qq, poolName, qq[poolName]);
    }
    for (const [pluginId, instance] of Object.entries(qq.plugins || {})) {
      const settings = instance?.settings;
      if (Array.isArray(settings?.text_models)) callback(settings, "text_models", settings.text_models);
      if (pluginId === "real_context" || pluginId === "qq_group_join_approval") continue;
    }
    for (const route of Array.isArray(qq.conversations) ? qq.conversations : []) {
      if (!route || typeof route !== "object") continue;
      for (const poolName of PLATFORM_POOL_NAMES) {
        if (Array.isArray(route[poolName])) callback(route, poolName, route[poolName]);
      }
    }
  }

  function forEachTierPool(callback) {
    const tiers = S().configDraft?.subagent_tiers;
    if (!tiers || typeof tiers !== "object") return;
    for (const [tierName, pool] of Object.entries(tiers)) if (Array.isArray(pool)) callback(tiers, tierName, pool);
  }

  function pruneOptionalPool(owner, key, predicate) {
    if (!owner || !Array.isArray(owner[key])) return;
    const pool = owner[key].filter(predicate);
    if (pool.length) owner[key] = pool; else delete owner[key];
  }

  function providerHasModel(provider, model) {
    const normalized = String(model || "").trim();
    return Boolean(normalized) && (String(provider?.default_model || "") === normalized || (Array.isArray(provider?.models) && provider.models.includes(normalized)));
  }

  function refTarget(item) {
    const provider = providerById(String(item?.provider_id || "").trim());
    const model = String(item?.model || "").trim();
    return provider && providerHasModel(provider, model) ? { provider, model } : null;
  }

  function pruneModelReferences() {
    const draft = S().configDraft;
    if (!draft) return;
    pruneOptionalPool(draft, "active_provider_models", (item) => Boolean(refTarget(item)));
    pruneOptionalPool(draft, "active_multimodal_provider_models", (item) => { const target = refTarget(item); return Boolean(target) && supportsMedia(target.provider, target.model); });
    forEachTierPool((tiers, tierName, pool) => { tiers[tierName] = pool.filter((item) => Boolean(refTarget(item))); });
    forEachPlatformPool((owner, poolName, pool) => {
      owner[poolName] = pool.filter((item) => { const target = refTarget(item); return Boolean(target) && (poolName !== "multimodal_models" || supportsMedia(target.provider, target.model)); });
      if (!owner[poolName].length) delete owner[poolName];
    });
    const vision = draft.plugins?.vision;
    if (vision?.vision_provider_id) {
      const provider = providerById(vision.vision_provider_id);
      const model = String(vision.vision_model || "").trim() || String(provider?.default_model || "").trim();
      if (!provider || !providerHasModel(provider, model) || !supportsMedia(provider, model)) { vision.vision_provider_id = ""; vision.vision_model = ""; }
    }
    if (vision?.video_provider_id) {
      const provider = providerById(vision.video_provider_id);
      const model = String(vision.video_model || "").trim();
      if (!provider || !providerHasModel(provider, model)) { vision.video_provider_id = ""; vision.video_model = ""; }
    }
    const kb = draft.plugins?.knowledge_base;
    if (kb?.embedding_provider_id) {
      const provider = providerById(kb.embedding_provider_id);
      const model = String(kb.embedding_model || "").trim() || String(provider?.default_model || "").trim();
      if (!provider || !providerHasModel(provider, model)) { kb.embedding_provider_id = ""; kb.embedding_model = ""; }
    }
    if (draft.embedding?.provider_id) {
      const provider = providerById(draft.embedding.provider_id);
      const model = String(draft.embedding.model || "").trim();
      if (!provider || !providerHasModel(provider, model)) { draft.embedding.provider_id = ""; draft.embedding.model = ""; }
    }
  }

  function replaceProviderReferences(previousId, nextId) {
    const draft = S().configDraft;
    if (!previousId || previousId === nextId || !draft) return;
    if (draft.active_provider === previousId) draft.active_provider = nextId;
    for (const poolName of ["active_provider_models", "active_multimodal_provider_models"]) {
      for (const item of draft[poolName] || []) if (item.provider_id === previousId) item.provider_id = nextId;
    }
    for (const owner of [draft.plugins?.vision, draft.plugins?.knowledge_base, draft.embedding]) {
      if (!owner) continue;
      for (const key of ["vision_provider_id", "video_provider_id", "embedding_provider_id", "provider_id"]) if (owner[key] === previousId) owner[key] = nextId;
    }
    forEachTierPool((_tiers, _name, pool) => { for (const item of pool) if (item?.provider_id === previousId) item.provider_id = nextId; });
    forEachPlatformPool((_owner, _name, pool) => { for (const item of pool) if (item?.provider_id === previousId) item.provider_id = nextId; });
    for (const models of [S().configMultimodalModels, S().configInferredImageModels]) {
      for (const model of models) if (model?.provider_id === previousId) model.provider_id = nextId;
    }
  }

  function removeProviderReferences(providerId) {
    const draft = S().configDraft;
    if (!draft) return;
    pruneOptionalPool(draft, "active_provider_models", (item) => item?.provider_id !== providerId);
    pruneOptionalPool(draft, "active_multimodal_provider_models", (item) => item?.provider_id !== providerId);
    forEachTierPool((tiers, tierName, pool) => { tiers[tierName] = pool.filter((item) => item?.provider_id !== providerId); });
    forEachPlatformPool((owner, poolName, pool) => { owner[poolName] = pool.filter((item) => item?.provider_id !== providerId); if (!owner[poolName].length) delete owner[poolName]; });
    for (const [owner, keys] of [[draft.plugins?.vision, [["vision_provider_id", "vision_model"], ["video_provider_id", "video_model"]]], [draft.plugins?.knowledge_base, [["embedding_provider_id", "embedding_model"]]], [draft.embedding, [["provider_id", "model"]]]]) {
      if (!owner) continue;
      for (const [providerKey, modelKey] of keys) if (owner[providerKey] === providerId) { owner[providerKey] = ""; owner[modelKey] = ""; }
    }
    S().configMultimodalModels = S().configMultimodalModels.filter((item) => item?.provider_id !== providerId);
    S().configInferredImageModels = S().configInferredImageModels.filter((item) => item?.provider_id !== providerId);
  }

  /* 引用数:该模型出现在多少个池里(概览卡上显示)。 */
  function poolMembership(providerId, model) {
    const draft = S().configDraft;
    const ref = { provider_id: providerId, model };
    const result = [];
    if ((draft.active_provider_models || []).some((item) => sameRef(item, ref))) result.push("文本");
    if ((draft.active_multimodal_provider_models || []).some((item) => sameRef(item, ref))) result.push("多模态");
    forEachTierPool((_tiers, name, pool) => { if (pool.some((item) => sameRef(item, ref))) result.push(name); });
    return result;
  }

  /* ───────────────────────── 供应商页 ───────────────────────── */

  const PROTOCOLS = [
    { value: "auto", label: "自动识别" },
    { value: "openai-chat", label: "OpenAI Chat Completions" },
    { value: "openai-responses", label: "OpenAI Responses" },
    { value: "anthropic", label: "Anthropic Messages" }
  ];
  const BUILTIN_PROTOCOLS = { "claude-code": "Claude Code", antigravity: "Antigravity", codex: "Codex" };
  const MODALITY_ICONS = { text: "file-text", image: "image", audio: "mic", video: "film", pdf: "file-type" };
  const MODALITY_LABELS = { text: "文本", image: "图片", audio: "音频", video: "视频", pdf: "PDF" };
  function isBuiltinProvider(provider) { return Boolean(BUILTIN_PROTOCOLS[String(provider?.protocol || "").trim()]); }

  function providerDefaults(provider = {}) {
    return { id: "", display_name: "", base_url: "", protocol: "auto", api_key: null, models: [], model_context_window: {}, model_costs: {}, model_modalities: {}, default_model: "", timeout_seconds: 60, temperature: 1.0, anthropic_max_tokens: 4096, extra_body: null, ...provider };
  }

  function providerCard(provider, index) {
    const models = providerModels(provider);
    const secretKey = `providers.${index}.api_key`;
    const active = models.filter((model) => poolMembership(provider.id, model).length).length;
    const node = el("button.st-provider-card", { type: "button", onclick: () => openProviderDrawer(index) });
    node.style.setProperty("--i", String(index));
    const status = secretStatus(secretKey);
    const chips = [chip(BUILTIN_PROTOCOLS[provider.protocol] || PROTOCOLS.find((item) => item.value === provider.protocol)?.label || provider.protocol || "auto", "is-soft")];
    chips.push(chip(`${models.length} 个模型`, "is-soft"));
    if (active) chips.push(chip(`${active} 在池中`, "is-accent"));
    if (!isBuiltinProvider(provider)) chips.push(chip(status.text === "未配置" ? "无密钥" : "密钥 ✓", status.text === "未配置" ? "is-warn" : "is-ok"));
    if (provider.enabled === false) chips.push(chip("已停用", "is-warn"));
    node.append(
      mark(provider.display_name || provider.id),
      el("span.st-provider-copy", null,
        el("strong", { text: provider.display_name || provider.id || `供应商 ${index + 1}` }),
        el("small", { text: provider.id || "尚未命名" }),
        el("span.st-provider-chips", null, chips)),
      icon("chevron-right", "st-card-caret")
    );
    return node;
  }

  function renderProvidersPage(root) {
    const list = providers();
    const add = button("添加供应商", { kind: "primary", iconName: "plus", onClick: () => {
      const draft = S().configDraft;
      draft.providers = Array.isArray(draft.providers) ? draft.providers : [];
      draft.providers.push(providerDefaults({ protocol: "auto" }));
      S().providerSecretStates.push(false);
      ctx.refreshProviderSecretStates();
      dirty();
      rerender("providers");
      openProviderDrawer(draft.providers.length - 1, { isNew: true });
    } });
    root.append(el("div.st-page-head", null, el("div", null, el("h2", { text: "供应商" }), el("p.st-page-desc", { text: "每张卡是一个 API 端点。点开配置连接、拉取模型、标注能力与价格。" })), add));
    if (!list.length) { root.append(empty("还没有供应商。至少需要添加一个。")); return; }
    root.append(el("div.st-grid", null, list.map((provider, index) => providerCard(provider, index))));
  }

  function openProviderDrawer(index, { isNew = false } = {}) {
    const provider = providers()[index];
    if (!provider) return;
    let referencedId = String(provider.id || "");
    const builtin = isBuiltinProvider(provider);
    const secretKey = `providers.${index}.api_key`;
    let syncTimer = null;
    const syncCard = () => { clearTimeout(syncTimer); syncTimer = setTimeout(() => rerender("providers"), 320); };

    const connectionTab = (body) => {
      const rows = [];
      if (builtin) rows.push(row("类型", chip(BUILTIN_PROTOCOLS[provider.protocol], "is-accent"), { hint: "内置本机 CLI 中转，没有 URL 与密钥。" }));
      rows.push(row("配置 ID", textInput(provider.id, (value) => {
        const previous = String(provider.id || "");
        provider.id = value.trim();
        const next = String(provider.id || "");
        if (referencedId && next && referencedId !== next) replaceProviderReferences(referencedId, next);
        if (next) referencedId = next;
        S().providerSecretStates[index] = false;
        delete S().secretChanges[secretKey];
        ctx.refreshProviderSecretStates();
        dirty();
        if (previous !== provider.id) syncCard();
      }, { mono: true, placeholder: "如 deepseek" }), { hint: "配置里的唯一标识，改名会同步所有引用。" }));
      rows.push(row("显示名称", textInput(provider.display_name, (value) => { provider.display_name = value; dirty(); syncCard(); }, { placeholder: "界面上显示的名字" })));
      if (!builtin) {
        rows.push(row("Base URL", textInput(provider.base_url, (value) => { provider.base_url = value.trim(); dirty(); }, { mono: true, placeholder: "https://api.example.com/v1" }), { hint: "到 /v1 为止；拉取模型时自动补 /models。", block: true }));
        rows.push(row("协议", selectInput(PROTOCOLS, provider.protocol || "auto", (value) => { provider.protocol = value; dirty(); syncCard(); }, "协议"), { hint: "自动识别按 URL 判断；Anthropic 端点须显式选。" }));
        rows.push(row("API Key", secretControl(secretKey), { hint: "支持 $env:NAME 读取环境变量；多个密钥用逗号分隔轮询。", block: true }));
      } else {
        rows.push(row("启用", toggle(provider.enabled !== false, (value) => { provider.enabled = value; dirty(); syncCard(); }), { hint: "关闭后该供应商的模型不会出现在任何池里。" }));
      }
      rows.push(row("超时秒数", numberInput({ label: "超时", min: 1, integer: true }, provider.timeout_seconds ?? 60, (value) => { provider.timeout_seconds = value; dirty(); })));
      body.append(card(rows));
    };

    const modelsTab = (body) => {
      const fetchButton = button("拉取模型列表", { kind: "primary", iconName: "refresh-cw", onClick: () => fetchProviderModels(index, fetchButton) });
      const addButton = button("手动添加", { iconName: "plus", onClick: () => {
        openPopover(addButton, (popBody, close) => {
          const input = textInput("", null, { mono: true, placeholder: "模型名，如 deepseek-chat", ariaLabel: "模型名" });
          const commit = () => {
            const name = input.value.trim();
            if (!name) return;
            provider.models = Array.isArray(provider.models) ? provider.models : [];
            if (!provider.models.includes(name)) provider.models.push(name);
            if (!provider.default_model) provider.default_model = name;
            dirty();
            close();
            drawer.refresh();
            syncCard();
            enrichModels(index, [name], { silent: true }).then((changed) => { if (changed) drawer.refresh(); });
          };
          input.addEventListener("keydown", (event) => { if (event.key === "Enter") { event.preventDefault(); commit(); } });
          popBody.append(el("div.st-inline-form", null, input, button("添加", { kind: "primary", small: true, onClick: commit })));
        }, { title: "添加模型", width: "320px" });
      } });
      body.append(el("div.st-toolbar", null, fetchButton, addButton, el("span.st-toolbar-hint", { text: builtin ? "内置 CLI 供应商的目录由 CLI 提供。" : "从 /models 端点拉取，并用 models.dev 目录补全能力与价格。" })));
      const models = Array.isArray(provider.models) ? provider.models : [];
      if (!models.length) { body.append(empty("还没有模型。拉取列表或手动添加一个。")); return; }
      const list = el("div.st-model-list");
      models.forEach((model, modelIndex) => list.append(modelRow(index, model, modelIndex)));
      body.append(list);
    };

    const advancedTab = (body) => {
      const rows = [
        row("Temperature", numberInput({ label: "Temperature", min: 0, max: 2, step: 0.1 }, provider.temperature ?? 1, (value) => { provider.temperature = value; dirty(); }), { hint: "供应商默认温度；单个模型可在模型列表里覆盖。" }),
        row("Anthropic 最大 Token", numberInput({ label: "最大 Token", min: 1, integer: true }, provider.anthropic_max_tokens ?? 4096, (value) => { provider.anthropic_max_tokens = value; dirty(); }), { hint: "仅 Anthropic 协议用；其它协议忽略。" }),
        row("工具结果带媒体", selectInput([{ value: "", label: "自动判断" }, { value: "true", label: "可以" }, { value: "false", label: "不可以" }], provider.tool_result_media == null ? "" : String(provider.tool_result_media), (value) => { if (value === "") delete provider.tool_result_media; else provider.tool_result_media = value === "true"; dirty(); }, "工具结果带媒体"), { hint: "工具输出能否直接携带图片；不能则改为追加一条带图用户消息。" }),
        row("额外请求体", kvTable(provider.extra_body || {}, (next) => { provider.extra_body = Object.keys(next).length ? next : null; dirty(); }, { keyPlaceholder: "字段名", valuePlaceholder: "值（支持 JSON 字面量）", valueKind: "json" }), { hint: "合并进每次请求的 JSON 顶层，例如 {\"top_k\": 40}。", block: true })
      ];
      body.append(card(rows));
    };

    const footer = [];
    if (!builtin) {
      footer.push(button("删除供应商", { kind: "text", danger: true, iconName: "trash-2", onClick: async () => {
        if (!(await confirmAction(`删除供应商“${provider.display_name || provider.id || index + 1}”？引用它的模型池会一并清理。`, "删除"))) return;
        const draft = S().configDraft;
        draft.providers.splice(index, 1);
        S().providerSecretStates.splice(index, 1);
        ctx.refreshProviderSecretStates();
        ctx.clearProviderSecretChanges();
        const removedId = referencedId || provider.id;
        removeProviderReferences(removedId);
        if (draft.active_provider === removedId || draft.active_provider === provider.id) draft.active_provider = draft.providers[0]?.id || "";
        dirty();
        closeDrawer();
        rerender("providers");
        rerender("models");
      } }));
    }
    footer.push(el("span.st-foot-spacer"), button("完成", { kind: "primary", onClick: () => closeDrawer() }));

    const drawer = openDrawer({
      title: provider.display_name || provider.id || "新供应商",
      subtitle: provider.id || "",
      width: "560px",
      tabs: [
        { id: "connection", label: "连接", render: connectionTab },
        { id: "models", label: "模型", render: modelsTab },
        { id: "advanced", label: "高级", render: advancedTab }
      ],
      initialTab: provider.id ? "models" : "connection",
      footer,
      onClose: () => {
        // 新建但没填 ID 的空供应商会让整份配置保存失败,关抽屉时直接丢弃。
        if (isNew && !String(provider.id || "").trim()) {
          const draft = S().configDraft;
          const position = draft.providers.indexOf(provider);
          if (position >= 0) { draft.providers.splice(position, 1); S().providerSecretStates.splice(position, 1); ctx.refreshProviderSecretStates(); }
          toast("没有填写 ID 的供应商已丢弃");
        }
        pruneModelReferences(); rerender("providers"); rerender("models"); ctx.renderModelMenu?.();
      }
    });
  }

  function modelRow(providerIndex, model, modelIndex) {
    const provider = providers()[providerIndex];
    const isDefault = provider.default_model === model;
    const modalities = modelModalities(provider, model);
    const window = provider.model_context_window?.[model];
    const cost = provider.model_costs?.[model];
    const temperature = provider.model_temperature?.[model];
    const loading = provider.model_tools_loading_mode?.[model];
    const pools = poolMembership(provider.id, model);
    const node = el("div.st-model-row");
    node.style.setProperty("--i", String(modelIndex));
    const badges = el("span.st-model-badges");
    for (const modality of modalities) badges.append(el("span.st-modality", { title: MODALITY_LABELS[modality] || modality }, icon(MODALITY_ICONS[modality] || "circle-alert")));
    if (window) badges.append(chip(formatTokens(window), "is-soft"));
    if (cost) badges.append(chip(`${cost.currency === "CNY" ? "¥" : "$"}${cost.input}/${cost.output}`, "is-soft"));
    if (temperature != null) badges.append(chip(`T ${temperature}`, "is-soft"));
    if (loading) badges.append(chip(loading, "is-soft"));
    for (const pool of pools) badges.append(chip(pool, "is-accent"));
    const star = iconButton("star", isDefault ? "默认模型" : "设为默认", () => {
      provider.default_model = model;
      dirty();
      for (const item of node.parentElement.querySelectorAll(".st-model-row")) item.classList.remove("is-default");
      node.classList.add("is-default");
      rerender("providers");
    }, `st-star${isDefault ? " is-on" : ""}`);
    const more = iconButton("ellipsis", "更多", () => openMenu(more, [
      { label: "微调…", hint: "上下文窗口 / 模态 / 温度 / 价格", icon: "pencil", onSelect: () => openModelTuner(providerIndex, model, more) },
      { label: "从目录补全", hint: "用 models.dev 数据填上下文窗口、模态、价格", icon: "sparkles", onSelect: () => enrichModels(providerIndex, [model]).then((changed) => { if (changed) refreshDrawerTab(); }) },
      { label: isDefault ? "已是默认模型" : "设为默认", icon: "star", disabled: isDefault, onSelect: () => { provider.default_model = model; dirty(); refreshDrawerTab(); rerender("providers"); } },
      "-",
      { label: "移除", icon: "trash-2", danger: true, onSelect: () => {
        provider.models = (provider.models || []).filter((item) => item !== model);
        for (const key of ["model_context_window", "model_costs", "model_modalities", "model_temperature", "model_tools_loading_mode"]) if (provider[key] && typeof provider[key] === "object") delete provider[key][model];
        if (provider.default_model === model) provider.default_model = provider.models[0] || "";
        pruneModelReferences();
        dirty();
        refreshDrawerTab();
        rerender("providers");
        rerender("models");
      } }
    ], { align: "end", width: "260px" }));
    node.classList.toggle("is-default", isDefault);
    node.append(star, el("span.st-model-name", null, el("strong", { text: model })), badges, more);
    return node;
  }

  function refreshDrawerTab() { if (drawerState) setDrawerTab(drawerState.tab, true); }

  /* 单模型微调弹出框:所有按模型的覆盖项都在这里,不再手写 JSON 对象。 */
  function openModelTuner(providerIndex, model, anchor) {
    const provider = providers()[providerIndex];
    const ensure = (key) => { if (!provider[key] || typeof provider[key] !== "object") provider[key] = {}; return provider[key]; };
    const setOrDelete = (key, value) => { const map = ensure(key); if (value === null || value === undefined || value === "") delete map[model]; else map[model] = value; dirty(); };
    openPopover(anchor, (body) => {
      body.append(row("上下文窗口", el("span.st-unit-wrap", null, numberInput({ label: "上下文窗口", min: 1, integer: true, nullable: true }, provider.model_context_window?.[model] ?? "", (value) => setOrDelete("model_context_window", value)), el("span.st-unit", { text: "tokens" })), { hint: "留空则用 models.dev 目录或供应商 /models 报的数。" }));
      const modalityWrap = el("div.st-choice-chips");
      const current = new Set(modelModalities(provider, model));
      const declared = provider.model_modalities && Object.prototype.hasOwnProperty.call(provider.model_modalities, model);
      for (const modality of ["text", "image", "audio", "video", "pdf"]) {
        const chipNode = el("button.st-choice-chip", { type: "button", "aria-pressed": current.has(modality) ? "true" : "false", onclick: () => {
          if (current.has(modality)) current.delete(modality); else current.add(modality);
          chipNode.setAttribute("aria-pressed", current.has(modality) ? "true" : "false");
          ensure("model_modalities")[model] = ["text", "image", "audio", "video", "pdf"].filter((item) => current.has(item));
          dirty();
          pruneModelReferences();
        } }, icon(MODALITY_ICONS[modality]), el("span", { text: MODALITY_LABELS[modality] }));
        modalityWrap.append(chipNode);
      }
      body.append(row("输入模态", modalityWrap, { hint: declared ? "已手动标注。" : "未标注时按目录推断；勾选即写入配置。", block: true }));
      body.append(row("温度覆盖", numberInput({ label: "温度", min: 0, max: 2, step: 0.1, nullable: true }, provider.model_temperature?.[model] ?? "", (value) => setOrDelete("model_temperature", value)), { hint: "留空继承供应商温度。" }));
      body.append(row("工具加载模式", selectInput([{ value: "", label: "继承全局" }, { value: "full", label: "full（完整声明）" }, { value: "stub", label: "stub（按需加载）" }], provider.model_tools_loading_mode?.[model] || "", (value) => setOrDelete("model_tools_loading_mode", value), "工具加载模式"), { hint: "约束解码型模型（如 glm-5.3-flash）需要 full。" }));
      const cost = { currency: "USD", input: "", output: "", cache_read: "", ...(provider.model_costs?.[model] || {}) };
      const commitCost = () => {
        if (cost.input === "" && cost.output === "") { setOrDelete("model_costs", null); return; }
        const next = { currency: cost.currency || "USD", input: Number(cost.input) || 0, output: Number(cost.output) || 0 };
        if (cost.cache_read !== "" && cost.cache_read != null) next.cache_read = Number(cost.cache_read) || 0;
        setOrDelete("model_costs", next);
      };
      const costGrid = el("div.st-cost-grid", null,
        selectInput([{ value: "USD", label: "USD" }, { value: "CNY", label: "CNY" }], cost.currency, (value) => { cost.currency = value; commitCost(); }, "币种"),
        el("label.st-cost-cell", null, el("small", { text: "输入" }), numberInput({ label: "输入价", min: 0, step: 0.001, nullable: true }, cost.input, (value) => { cost.input = value ?? ""; commitCost(); })),
        el("label.st-cost-cell", null, el("small", { text: "输出" }), numberInput({ label: "输出价", min: 0, step: 0.001, nullable: true }, cost.output, (value) => { cost.output = value ?? ""; commitCost(); })),
        el("label.st-cost-cell", null, el("small", { text: "缓存读" }), numberInput({ label: "缓存读价", min: 0, step: 0.001, nullable: true }, cost.cache_read ?? "", (value) => { cost.cache_read = value ?? ""; commitCost(); })));
      body.append(row("价格 / 1M tokens", costGrid, { hint: "留空用 models.dev 目录价；中转/赠送端点在这里手填。", block: true }));
    }, { title: model, width: "380px" });
  }

  /* 调后端 /api/providers/models:fetch=true 拉目录,false 只补元数据。 */
  async function requestProviderModels(provider, { fetch, models }) {
    const payload = { ...provider };
    delete payload.api_key;
    const response = await ctx.apiRequest("/api/providers/models", { method: "POST", body: JSON.stringify({ provider: payload, fetch, models: models || [] }) });
    return response.json();
  }

  /* 把目录元数据写进供应商表(只补空缺,不覆盖手填)。返回是否有改动。 */
  function applyCatalog(provider, entries) {
    let changed = false;
    for (const entry of entries || []) {
      const model = entry.id;
      if (!model) continue;
      if (entry.context_window && !(provider.model_context_window?.[model])) {
        if (!provider.model_context_window || typeof provider.model_context_window !== "object") provider.model_context_window = {};
        provider.model_context_window[model] = entry.context_window;
        changed = true;
      }
      if (Array.isArray(entry.modalities) && entry.modalities.length && !(provider.model_modalities && Object.prototype.hasOwnProperty.call(provider.model_modalities, model))) {
        if (!provider.model_modalities || typeof provider.model_modalities !== "object") provider.model_modalities = {};
        provider.model_modalities[model] = entry.modalities;
        changed = true;
      }
      if (entry.cost && !(provider.model_costs?.[model])) {
        if (!provider.model_costs || typeof provider.model_costs !== "object") provider.model_costs = {};
        const cost = { currency: "USD", input: entry.cost.input ?? 0, output: entry.cost.output ?? 0 };
        if (entry.cost.cache_read != null) cost.cache_read = entry.cost.cache_read;
        provider.model_costs[model] = cost;
        changed = true;
      }
    }
    if (changed) dirty();
    return changed;
  }

  async function enrichModels(providerIndex, models, { silent = false } = {}) {
    const provider = providers()[providerIndex];
    try {
      const result = await requestProviderModels(provider, { fetch: false, models });
      const changed = applyCatalog(provider, result.models);
      if (!silent) toast(changed ? "已从目录补全元数据" : "目录里没有更多信息", changed ? "info" : "error");
      if (changed) { rerender("providers"); rerender("models"); }
      return changed;
    } catch (error) {
      if (!silent) toast(error.message || "目录查询失败", "error");
      return false;
    }
  }

  async function fetchProviderModels(providerIndex, trigger) {
    const provider = providers()[providerIndex];
    trigger.disabled = true;
    trigger.classList.add("is-loading");
    try {
      const result = await requestProviderModels(provider, { fetch: true });
      openFetchDialog(providerIndex, result);
    } catch (error) {
      toast(error.message || "拉取失败", "error");
    } finally {
      trigger.disabled = false;
      trigger.classList.remove("is-loading");
    }
  }

  function openFetchDialog(providerIndex, result) {
    const provider = providers()[providerIndex];
    const existing = new Set(providerModels(provider));
    const entries = result.models || [];
    const selected = new Set();
    let filterText = "";
    const list = el("div.st-fetch-list");
    const summary = el("span.st-toolbar-hint");
    const paint = () => {
      list.replaceChildren();
      const visible = entries.filter((entry) => !filterText || entry.id.toLowerCase().includes(filterText));
      if (!visible.length) list.append(empty(entries.length ? "没有匹配的模型。" : "供应商没有返回任何模型。"));
      visible.forEach((entry, index) => {
        const already = existing.has(entry.id);
        const input = el("input", { type: "checkbox", checked: already || selected.has(entry.id), disabled: already });
        input.addEventListener("change", () => { if (input.checked) selected.add(entry.id); else selected.delete(entry.id); paintSummary(); });
        const badges = el("span.st-model-badges");
        for (const modality of entry.modalities || []) badges.append(el("span.st-modality", { title: MODALITY_LABELS[modality] || modality }, icon(MODALITY_ICONS[modality] || "circle-alert")));
        if (entry.context_window) badges.append(chip(formatTokens(entry.context_window), "is-soft"));
        if (entry.cost) badges.append(chip(`$${entry.cost.input}/${entry.cost.output}`, "is-soft"));
        if (already) badges.append(chip("已添加", "is-ok"));
        const rowNode = el("label.st-fetch-row", null, input, el("strong", { text: entry.id }), badges);
        rowNode.style.setProperty("--i", String(Math.min(index, 20)));
        list.append(rowNode);
      });
    };
    const paintSummary = () => { summary.textContent = `${entries.length} 个模型 · 已选 ${selected.size}`; };
    const search = textInput("", (value) => { filterText = value.trim().toLowerCase(); paint(); }, { placeholder: "筛选模型名", ariaLabel: "筛选模型名" });
    const selectAll = button("全选可见", { small: true, onClick: () => {
      for (const entry of entries) if (!existing.has(entry.id) && (!filterText || entry.id.toLowerCase().includes(filterText))) selected.add(entry.id);
      paint(); paintSummary();
    } });
    const handle = openDialog({
      title: "拉取到的模型",
      subtitle: `${provider.display_name || provider.id} · 来源：${{ http: "/models 端点", cli: "本机 CLI", catalog: "目录" }[result.source] || result.source}`,
      width: "560px",
      body: (body) => { body.append(el("div.st-toolbar", null, search, selectAll, summary), list); paint(); paintSummary(); },
      actions: [
        button("取消", { onClick: () => handle.close() }),
        button("加入所选", { kind: "primary", onClick: () => {
          if (!selected.size) { toast("没有勾选任何模型", "error"); return; }
          provider.models = Array.isArray(provider.models) ? provider.models : [];
          for (const id of selected) if (!provider.models.includes(id)) provider.models.push(id);
          if (!provider.default_model) provider.default_model = provider.models[0];
          applyCatalog(provider, entries.filter((entry) => selected.has(entry.id)));
          dirty();
          handle.close();
          refreshDrawerTab();
          rerender("providers");
          rerender("models");
          toast(`已加入 ${selected.size} 个模型`);
        } })
      ]
    });
  }

  /* ───────────────────────── 模型池矩阵 ───────────────────────── */

  const POOL_COLUMNS = [
    { id: "text", label: "文本", hint: "主对话与辅助任务", path: "active_provider_models" },
    { id: "multimodal", label: "多模态", hint: "看图/看视频时用", path: "active_multimodal_provider_models", capability: "image" },
    { id: "cheap", label: "cheap", hint: "子代理 · 简单任务", tier: "cheap" },
    { id: "balanced", label: "balanced", hint: "子代理 · 普通任务", tier: "balanced" },
    { id: "strong", label: "strong", hint: "子代理 · 复杂任务", tier: "strong" }
  ];

  function poolArray(column) {
    const draft = S().configDraft;
    if (column.tier) {
      if (!draft.subagent_tiers || typeof draft.subagent_tiers !== "object") draft.subagent_tiers = {};
      if (!Array.isArray(draft.subagent_tiers[column.tier])) draft.subagent_tiers[column.tier] = [];
      return draft.subagent_tiers[column.tier];
    }
    if (!Array.isArray(draft[column.path])) {
      // 文本池缺省 = 当前供应商的默认模型;第一次改动前先把这层隐含语义落成显式数组。
      if (column.path === "active_provider_models") {
        const provider = providerById(draft.active_provider);
        draft[column.path] = provider?.default_model ? [{ provider_id: provider.id, model: provider.default_model }] : [];
      } else draft[column.path] = [];
    }
    return draft[column.path];
  }

  function poolHas(column, ref) {
    const draft = S().configDraft;
    const pool = column.tier ? draft.subagent_tiers?.[column.tier] : draft[column.path];
    if (!Array.isArray(pool)) {
      if (column.path === "active_provider_models") {
        const provider = providerById(draft.active_provider);
        return Boolean(provider) && provider.id === ref.provider_id && provider.default_model === ref.model;
      }
      return false;
    }
    return pool.some((item) => sameRef(item, ref));
  }

  function poolToggle(column, ref, on) {
    const pool = poolArray(column);
    const index = pool.findIndex((item) => sameRef(item, ref));
    if (on && index < 0) pool.push({ provider_id: ref.provider_id, model: ref.model });
    if (!on && index >= 0) pool.splice(index, 1);
    dirty();
    ctx.renderModelMenu?.();
  }

  /* 池里现在有谁:文本池没显式设置时 = 当前供应商的默认模型(隐含成员)。 */
  function poolMembers(column) {
    const draft = S().configDraft;
    const pool = column.tier ? draft.subagent_tiers?.[column.tier] : draft[column.path];
    if (Array.isArray(pool)) return { items: pool.map((item) => ({ provider_id: item.provider_id, model: item.model })), implicit: false };
    if (column.path === "active_provider_models") {
      const provider = providerById(draft.active_provider);
      if (provider?.default_model) return { items: [{ provider_id: provider.id, model: provider.default_model }], implicit: true };
    }
    return { items: [], implicit: false };
  }

  function renderModelsPage(root) {
    root.append(el("div.st-page-head", null, el("div", null, el("h2", { text: "模型池" }), el("p.st-page-desc", { text: "每个池是一组候选模型，请求时在池里轮询。文本池和多模态池给主对话；三个档位池给子代理按任务难度挑。" }))));
    if (!modelChoices().length) { root.append(empty("请先在供应商中配置模型。", button("去供应商", { onClick: () => ctx.setSettingsView("providers") }))); return; }
    const grid = el("div.st-pool-grid");
    POOL_COLUMNS.forEach((column, index) => grid.append(poolCard(column, index)));
    root.append(grid);
  }

  function poolCard(column, index) {
    const node = el("section.st-pool-card");
    node.style.setProperty("--i", String(index));
    const count = el("span.st-chip.is-accent");
    const list = el("div.st-pool-list");
    const paint = () => {
      const { items, implicit } = poolMembers(column);
      count.textContent = items.length ? `${items.length} 个模型` : "空";
      count.className = `st-chip ${items.length ? "is-accent" : "is-soft"}`;
      list.replaceChildren();
      if (!items.length) list.append(el("p.st-pool-empty", { text: column.emptyHint || "还没有模型，点下面添加。" }));
      items.forEach((item, position) => {
        const provider = providerById(item.provider_id);
        const rowNode = el("div.st-pool-member");
        rowNode.style.setProperty("--i", String(position));
        rowNode.append(...[
          mark(provider?.display_name || item.provider_id, "is-small"),
          el("span.st-pool-member-copy", null, el("strong", { text: item.model }), el("small", { text: provider?.display_name || item.provider_id })),
          implicit ? chip("默认模型", "is-soft") : null,
          iconButton("x", "移出池", () => { poolToggle(column, item, false); paint(); }, "is-danger")
        ].filter(Boolean));
        list.append(rowNode);
      });
    };
    const add = button("添加模型", { iconName: "plus", small: true, onClick: () => {
      openPopover(add, (body) => {
        const groups = new Map();
        for (const choice of modelChoices()) {
          if (column.capability === "image" && !supportsMedia(choice.provider, choice.model)) continue;
          if (!groups.has(choice.provider_id)) groups.set(choice.provider_id, { name: choice.provider_name, items: [] });
          groups.get(choice.provider_id).items.push(choice);
        }
        if (!groups.size) body.append(el("p.st-hint", { text: "没有可加入的模型：多模态池需要模型标注了图片输入能力。" }));
        for (const [providerId, group] of groups) {
          const groupNode = el("div.st-pick-group", null, el("div.st-pick-group-title", { text: group.name }));
          for (const choice of group.items) {
            const ref = { provider_id: providerId, model: choice.model };
            const input = el("input", { type: "checkbox", checked: poolHas(column, ref) });
            input.addEventListener("change", () => { poolToggle(column, ref, input.checked); paint(); });
            groupNode.append(el("label.st-check-row", null, input, el("span", null, el("strong", { text: choice.model }))));
          }
          body.append(groupNode);
        }
      }, { title: `加入${column.label}池`, width: "340px", align: "start" });
    } });
    node.append(
      el("header.st-pool-head", null, el("div", null, el("h3", { text: `${column.label}池` }), el("p", { text: column.hint })), count),
      list,
      el("footer.st-pool-foot", null, add));
    paint();
    return node;
  }

  /* ───────────────────────── 人格与身份 ───────────────────────── */

  function documentName(name) {
    const trimmed = String(name || "").trim().replace(/[\\/]/g, "-").replace(/\.md$/i, "");
    return trimmed ? `${trimmed}.md` : "";
  }

  function displayName(doc) { return String(doc?.name || "").replace(/\.md$/i, ""); }

  function personaAvatarUrl(doc) {
    return doc?.avatar_path ? `/api/persona/avatar?path=${encodeURIComponent(doc.avatar_path)}` : "";
  }

  function personaCard(kind, doc, index, activePath) {
    const active = cfg(activePath, "") === doc.name;
    const node = el(`button.st-persona-card${active ? ".is-active" : ""}`, { type: "button", onclick: () => openPersonaDrawer(kind, index) });
    node.style.setProperty("--i", String(index + 1));
    const avatar = el("span.st-avatar");
    const url = personaAvatarUrl(doc);
    if (url) {
      const image = el("img", { src: url, alt: "" });
      image.addEventListener("error", () => { image.remove(); avatar.append(mark(displayName(doc))); });
      avatar.append(image);
    } else avatar.append(mark(displayName(doc)));
    node.append(...[avatar, el("span.st-persona-copy", null, el("strong", { text: displayName(doc) || "未命名" }), el("small", { text: `${String(doc.content || "").length} 字` })), active ? chip("使用中", "is-accent") : null, icon("chevron-right", "st-card-caret")].filter(Boolean));
    return node;
  }

  function renderPromptsPage(root) {
    const drafts = S().promptDraft || { personas: [], identities: [] };
    const section = (kind, title, description, activePath, defaultLabel) => {
      const documents = Array.isArray(drafts[kind]) ? drafts[kind] : (drafts[kind] = []);
      const add = button(kind === "personas" ? "新建人格" : "新建身份", { iconName: "plus", onClick: () => {
        const base = kind === "personas" ? "新建人格" : "新建身份";
        let name = `${base}.md`;
        let suffix = 2;
        while (documents.some((doc) => doc.name === name)) name = `${base} ${suffix++}.md`;
        documents.push({ name, content: "", avatar_path: null, original_name: null });
        setCfg(activePath, name);
        rerender("prompts");
        openPersonaDrawer(kind, documents.length - 1);
      } });
      root.append(el("div.st-page-head", null, el("div", null, el("h2", { text: title }), el("p.st-page-desc", { text: description })), add));
      const grid = el("div.st-grid.is-personas");
      const active = cfg(activePath, "");
      const defaultCard = el(`button.st-persona-card${!active ? ".is-active" : ""}`, { type: "button", onclick: () => { setCfg(activePath, ""); rerender("prompts"); } });
      defaultCard.style.setProperty("--i", "0");
      defaultCard.append(...[el("span.st-avatar.is-default", null, icon("sparkles")), el("span.st-persona-copy", null, el("strong", { text: defaultLabel }), el("small", { text: kind === "personas" ? "内置人格，不可编辑" : "不附加用户身份说明" })), !active ? chip("使用中", "is-accent") : null].filter(Boolean));
      grid.append(defaultCard);
      documents.forEach((doc, index) => grid.append(personaCard(kind, doc, index, activePath)));
      root.append(grid);
    };
    section("personas", "AI 人格", "点卡片编辑内容、看板与预设问题；「使用中」的人格决定她怎么说话。", "prompt.active_persona", "Miyu 默认人格");
    section("identities", "用户身份", "告诉她你是谁。同样点卡片编辑。", "prompt.active_identity", "不使用用户身份");
  }

  function imageField(doc, key, label, previewClass) {
    const preview = el(`img.${previewClass}`, { alt: "" });
    const wrap = el("div.st-image-field");
    const pathInput = textInput(doc[key] || "", (value) => { doc[key] = value.trim() || null; paint(); ctx.markConfigDirty(); }, { mono: true, placeholder: "留空使用默认", ariaLabel: label });
    const picker = el("input", { type: "file", accept: "image/png,image/jpeg,image/webp,image/gif,image/bmp", hidden: true });
    const pick = button("选择图片", { iconName: "folder", small: true, onClick: () => picker.click() });
    const clear = button("清除", { kind: "text", small: true, danger: true, onClick: () => { doc[key] = null; pathInput.value = ""; paint(); ctx.markConfigDirty(); } });
    const paint = () => {
      const url = doc[key] ? `/api/persona/avatar?path=${encodeURIComponent(doc[key])}` : "";
      preview.classList.toggle("is-missing", !url);
      if (url) preview.src = url; else preview.removeAttribute("src");
    };
    preview.addEventListener("error", () => { preview.removeAttribute("src"); preview.classList.add("is-missing"); });
    picker.addEventListener("change", async () => {
      const file = picker.files?.[0];
      if (!file) return;
      if (file.size > 8 * 1024 * 1024) return toast("图片不能超过 8 MiB", "error");
      pick.disabled = true;
      try {
        const response = await ctx.apiRequest("/api/persona/assets", { method: "POST", headers: { "Content-Type": file.type || "application/octet-stream" }, body: file });
        const result = await response.json();
        doc[key] = result.path;
        pathInput.value = result.path;
        preview.classList.remove("is-missing");
        preview.src = result.preview_url;
        ctx.markConfigDirty();
        rerender("prompts");
      } catch (error) {
        toast(error.message || "图片上传失败", "error");
      } finally {
        pick.disabled = false;
        picker.value = "";
      }
    });
    paint();
    wrap.append(preview, el("div.st-image-controls", null, pathInput, el("div.st-inline-actions", null, pick, clear), picker));
    return row(label, wrap, { block: true });
  }

  function openPersonaDrawer(kind, index) {
    const documents = S().promptDraft[kind];
    const doc = documents[index];
    if (!doc) return;
    const activePath = kind === "personas" ? "prompt.active_persona" : "prompt.active_identity";
    const isPersona = kind === "personas";
    const contentTab = (body) => {
      const nameInput = textInput(displayName(doc), (value) => {
        const previous = doc.name;
        doc.name = documentName(value);
        if (cfg(activePath, "") === previous) setCfg(activePath, doc.name);
        ctx.markConfigDirty();
        drawer.setTitle(displayName(doc) || "未命名");
        rerender("prompts");
      }, { placeholder: "名称", ariaLabel: "名称" });
      const active = cfg(activePath, "") === doc.name;
      const useToggle = toggle(active, (value) => { setCfg(activePath, value ? doc.name : ""); rerender("prompts"); });
      body.append(card([
        row("名称", nameInput, { hint: "存成 prompts 目录下同名 .md 文件。" }),
        row("设为当前使用", useToggle, { hint: isPersona ? "切换人格会重排系统提示词。" : "开启后每次对话都附带这段身份说明。" })
      ]));
      body.append(card([row("内容", textarea(doc.content, (value) => { doc.content = value; ctx.markConfigDirty(); }, { rows: 16, placeholder: isPersona ? "她是谁、怎么说话、有什么习惯……" : "你是谁、希望她怎么称呼你……", ariaLabel: "内容" }), { block: true, hint: "Markdown；越具体越稳定。" })]));
    };
    const boardTab = (body) => {
      body.append(card([
        imageField(doc, "avatar_path", "头像", "st-avatar-preview"),
        imageField(doc, "board_image_path", "看板图片", "st-board-preview"),
        row("看板大字", textInput(doc.board_title || "", (value) => { doc.board_title = value.trim() || null; ctx.markConfigDirty(); }, { placeholder: ctx.DEFAULT_BOARD_TITLE })),
        row("看板小字", textInput(doc.board_subtitle || "", (value) => { doc.board_subtitle = value.trim() || null; ctx.markConfigDirty(); }, { placeholder: ctx.DEFAULT_BOARD_SUBTITLE }))
      ], { title: "空白页看板", description: "新会话第一屏显示的头像、大图与文案。" }));
    };
    const starterTab = (body) => {
      const values = Array.isArray(doc.starter_prompts) ? ctx.DEFAULT_STARTER_PROMPTS.map((_, i) => String(doc.starter_prompts[i] || "")) : ctx.DEFAULT_STARTER_PROMPTS.map(() => "");
      const rows = values.map((value, i) => row(`预设问题 ${i + 1}`, textInput(value, (next) => {
        values[i] = next;
        doc.starter_prompts = values.some((item) => item.trim()) ? [...values] : null;
        ctx.markConfigDirty();
      }, { placeholder: ctx.DEFAULT_STARTER_PROMPTS[i] })));
      body.append(card(rows, { title: "预设问题", description: "空白页上的四个快捷入口；留空用默认。" }));
    };
    const tabs = [{ id: "content", label: "内容", render: contentTab }];
    if (isPersona) tabs.push({ id: "board", label: "看板", render: boardTab }, { id: "starter", label: "预设问题", render: starterTab });
    const drawer = openDrawer({
      title: displayName(doc) || "未命名",
      subtitle: isPersona ? "AI 人格" : "用户身份",
      width: "600px",
      tabs,
      footer: [
        button("删除", { kind: "text", danger: true, iconName: "trash-2", onClick: async () => {
          if (!(await confirmAction(`删除“${displayName(doc)}”？文件会一并删除。`, "删除"))) return;
          const wasActive = cfg(activePath, "") === doc.name;
          documents.splice(index, 1);
          if (wasActive) setCfg(activePath, "");
          ctx.markConfigDirty();
          closeDrawer();
          rerender("prompts");
        } }),
        el("span.st-foot-spacer"),
        button("完成", { kind: "primary", onClick: () => closeDrawer() })
      ],
      onClose: () => rerender("prompts")
    });
  }

  /* ───────────────────────── 全局 ───────────────────────── */

  function generalBinding(field) {
    const binding = pathBinding(field.path, { nullable: Boolean(field.nullable) });
    binding.secretKey = field.path;
    return binding;
  }

  function generalBindingFor(field, keyOverride) {
    if (keyOverride) return pathBinding(keyOverride);
    return generalBinding(field);
  }

  function renderGeneralPage(root) {
    root.append(el("div.st-page-head", null, el("div", null, el("h2", { text: "全局" }), el("p.st-page-desc", { text: "工具、上下文、记忆这些跟供应商无关的行为。数字类参数收在每张卡的「高级参数」里。" }))));
    const sections = (schema().general || []).filter((section) => section.id !== "mcp");
    sections.forEach((section, index) => {
      const rows = fieldRows(section.fields, generalBindingFor);
      if (Array.isArray(section.advanced) && section.advanced.length) {
        rows.push(disclosure("高级参数", (inner) => inner.append(...fieldRows(section.advanced, generalBindingFor)), { hint: `${section.advanced.length} 项` }));
      }
      const node = card(rows, { title: section.title, description: section.description || "" });
      node.style.setProperty("--i", String(index));
      root.append(node);
    });
  }

  /* ───────────────────────── MCP ───────────────────────── */

  function mcpServers() {
    const draft = S().configDraft;
    if (!draft.mcp || typeof draft.mcp !== "object") draft.mcp = { enabled: false, servers: [] };
    if (!Array.isArray(draft.mcp.servers)) draft.mcp.servers = [];
    return draft.mcp.servers;
  }

  function renderMcpPage(root) {
    const servers = mcpServers();
    const add = button("添加服务器", { kind: "primary", iconName: "plus", onClick: () => openMcpDialog(null) });
    root.append(el("div.st-page-head", null, el("div", null, el("h2", { text: "MCP" }), el("p.st-page-desc", { text: "Model Context Protocol 服务器：每个是一条本机命令，启动后把它的工具挂进工具面。" })), add));
    root.append(card([row("启用 MCP", toggle(Boolean(cfg("mcp.enabled")), (value) => setCfg("mcp.enabled", value)), { hint: "总开关；关闭后所有服务器都不启动。" })]));
    if (!servers.length) { root.append(empty("还没有 MCP 服务器。")); return; }
    const list = el("div.st-card");
    const body = el("div.st-card-body.is-list");
    servers.forEach((server, index) => {
      const item = el("div.st-server-row");
      item.style.setProperty("--i", String(index));
      const command = [server.command, ...(server.args || [])].join(" ");
      item.append(
        el("span.st-server-mark", null, icon("server")),
        el("button.st-server-copy", { type: "button", onclick: () => openMcpDialog(index) }, el("strong", { text: server.display_name || server.id || `服务器 ${index + 1}` }), el("small.is-mono", { text: command })),
        chip(`${server.timeout_seconds ?? 30}s`, "is-soft"),
        toggle(server.enabled !== false, (value) => { server.enabled = value; dirty(); }, "启用"),
        iconButton("pencil", "编辑", () => openMcpDialog(index)),
        iconButton("trash-2", "删除", async () => {
          if (!(await confirmAction(`删除 MCP 服务器“${server.display_name || server.id}”？`, "删除"))) return;
          servers.splice(index, 1);
          dirty();
          rerender("mcp");
        }, "is-danger")
      );
      body.append(item);
    });
    list.append(body);
    root.append(list);
  }

  function openMcpDialog(index) {
    const servers = mcpServers();
    const editing = index != null;
    const draft = editing ? clone(servers[index]) : { id: "", display_name: "", command: "", args: [], env: {}, timeout_seconds: 30, enabled: true };
    draft.args = Array.isArray(draft.args) ? draft.args : [];
    draft.env = draft.env && typeof draft.env === "object" ? draft.env : {};
    const idInput = textInput(draft.id, (value) => { draft.id = value.trim(); }, { mono: true, placeholder: "如 filesystem", ariaLabel: "ID" });
    const handle = openDialog({
      title: editing ? "编辑 MCP 服务器" : "添加 MCP 服务器",
      width: "560px",
      body: (body) => {
        body.append(card([
          row("ID", idInput, { hint: "工具名前缀；只用字母、数字、连字符。" }),
          row("显示名称", textInput(draft.display_name, (value) => { draft.display_name = value; }, { placeholder: "可选" })),
          row("命令", textInput(draft.command, (value) => { draft.command = value.trim(); }, { mono: true, placeholder: "npx / uvx / 绝对路径" }), { hint: "可执行文件本身；参数放下面。" }),
          row("参数", stringList(draft.args, (value) => { draft.args = value; }, { placeholder: "一个参数", mono: true }), { block: true, hint: "每行一个，按启动顺序。" }),
          row("环境变量", kvTable(draft.env, (value) => { draft.env = value; }, { keyPlaceholder: "NAME", valuePlaceholder: "value" }), { block: true }),
          row("超时秒数", numberInput({ label: "超时", min: 1, max: 600, integer: true }, draft.timeout_seconds ?? 30, (value) => { draft.timeout_seconds = value; })),
          row("启用", toggle(draft.enabled !== false, (value) => { draft.enabled = value; }))
        ]));
      },
      actions: [
        button("取消", { onClick: () => handle.close() }),
        button(editing ? "保存" : "添加", { kind: "primary", onClick: () => {
          if (!draft.id) return setInvalid(idInput, "ID 不能为空");
          if (!/^[A-Za-z0-9_-]+$/.test(draft.id)) return setInvalid(idInput, "只能用字母、数字、下划线和连字符");
          if (servers.some((server, i) => server.id === draft.id && i !== index)) return setInvalid(idInput, "ID 已存在");
          if (!draft.command) { toast("命令不能为空", "error"); return; }
          if (editing) servers[index] = draft; else servers.push(draft);
          dirty();
          handle.close();
          rerender("mcp");
        } })
      ]
    });
  }

  /* ───────────────────────── 插件 ───────────────────────── */

  const PLUGIN_GROUP_ORDER = ["联网", "视觉与生图", "知识与记忆", "系统工具", "CLI 中转"];

  function pluginObject(key) {
    const draft = S().configDraft;
    if (!draft.plugins || typeof draft.plugins !== "object") draft.plugins = {};
    if (!draft.plugins[key] || typeof draft.plugins[key] !== "object") draft.plugins[key] = {};
    return draft.plugins[key];
  }

  function nestedBinding(object, key, { secretKey = null } = {}) {
    const binding = {
      get: () => getPath(object, key),
      set: (value) => {
        if (value === null || value === undefined) deletePath(object, key); else setPath(object, key, value);
        dirty();
      }
    };
    if (secretKey) binding.secretKey = secretKey;
    return binding;
  }

  function pluginBindingFor(pluginKey) {
    const object = pluginObject(pluginKey);
    return (field, keyOverride) => nestedBinding(object, keyOverride || field.key, { secretKey: `plugins.${pluginKey}.${keyOverride || field.key}` });
  }

  function pluginHasSwitch(pluginKey, definition) {
    return (definition.fields || []).some((field) => field.key === "enabled") || typeof pluginObject(pluginKey).enabled === "boolean";
  }

  function renderPluginsPage(root) {
    root.append(el("div.st-page-head", null, el("div", null, el("h2", { text: "插件" }), el("p.st-page-desc", { text: "卡片上直接开关；点卡片调参数。QQ 群管理记录在控制台「群管」面板。" }))));
    const definitions = schema().toolPlugins || {};
    const groups = new Map(PLUGIN_GROUP_ORDER.map((name) => [name, []]));
    for (const [key, definition] of Object.entries(definitions)) {
      if (key === "memory" || key === "print_image") continue;
      const group = groups.has(definition.group) ? definition.group : "系统工具";
      groups.get(group).push({ key, definition });
    }
    let cardIndex = 0;
    for (const [group, items] of groups) {
      if (!items.length) continue;
      root.append(el("h3.st-group-title", { text: group }));
      const grid = el("div.st-grid.is-plugins");
      for (const { key, definition } of items) {
        const object = pluginObject(key);
        const hasSwitch = pluginHasSwitch(key, definition);
        const node = el("div.st-plugin-card");
        node.style.setProperty("--i", String(cardIndex++));
        node.classList.toggle("is-off", hasSwitch && object.enabled === false);
        const open = el("button.st-plugin-open", { type: "button", onclick: () => openPluginDrawer(key) },
          mark(definition.title || key),
          el("span.st-plugin-copy", null, el("strong", { text: definition.title || key }), el("small", { text: definition.description || key })));
        node.append(open);
        if (hasSwitch) {
          node.append(toggle(object.enabled !== false && object.enabled !== undefined ? Boolean(object.enabled) : object.enabled === undefined ? Boolean((definition.fields || []).find((field) => field.key === "enabled")?.default) : false, (value) => {
            object.enabled = value;
            node.classList.toggle("is-off", !value);
            dirty();
          }, `${definition.title} 启用`));
        } else node.append(chip("CLI", "is-soft"));
        grid.append(node);
      }
      root.append(grid);
    }
  }

  function openPluginDrawer(pluginKey) {
    const definition = (schema().toolPlugins || {})[pluginKey];
    if (!definition) return;
    const bindingFor = pluginBindingFor(pluginKey);
    const fields = (definition.fields || []).filter((field) => field.key !== "enabled");
    const render = (body) => {
      const object = pluginObject(pluginKey);
      const top = [];
      if (pluginHasSwitch(pluginKey, definition)) {
        const enabledField = (definition.fields || []).find((field) => field.key === "enabled");
        top.push(row("启用插件", toggle(object.enabled === undefined ? Boolean(enabledField?.default) : Boolean(object.enabled), (value) => { object.enabled = value; dirty(); rerender("plugins"); }), { hint: enabledField?.hint || "" }));
      }
      if (top.length) body.append(card(top));
      if (fields.length) body.append(card(fieldRows(fields, bindingFor)));
      if (definition.custom === "api_quota_accounts") body.append(apiQuotaAccountsCard("deepseek"), apiQuotaAccountsCard("openrouter"));
      if (!fields.length && !top.length && !definition.custom) body.append(empty("这个插件没有可调参数。"));
    };
    openDrawer({ title: definition.title || pluginKey, subtitle: `plugins.${pluginKey}`, width: "560px", body: render, footer: [el("span.st-foot-spacer"), button("完成", { kind: "primary", onClick: () => closeDrawer() })], onClose: () => { pruneModelReferences(); rerender("plugins"); } });
  }

  /* 额度查询插件的多账号密钥:索引会随增删移动,密钥状态跟着账号 id 走。 */
  function apiQuotaAccountsCard(providerKey) {
    const plugin = pluginObject("api_quota");
    if (!plugin[providerKey] || typeof plugin[providerKey] !== "object") plugin[providerKey] = { accounts: [] };
    const provider = plugin[providerKey];
    provider.accounts = Array.isArray(provider.accounts) && provider.accounts.length ? provider.accounts : [{ id: "account-1", name: "默认账号", api_key: "" }];
    const prefix = `plugins.api_quota.${providerKey}.accounts.`;
    const reindex = (previous) => {
      const saved = new Map(previous.map((account, index) => [account.id || account.name, { configured: Boolean(S().secretStates[`${prefix}${index}.api_key`]), change: S().secretChanges[`${prefix}${index}.api_key`] }]));
      for (const key of Object.keys(S().secretChanges)) if (key.startsWith(prefix)) delete S().secretChanges[key];
      for (const key of Object.keys(S().secretStates)) if (key.startsWith(prefix)) delete S().secretStates[key];
      provider.accounts.forEach((account, index) => {
        const prior = saved.get(account.id || account.name);
        S().secretStates[`${prefix}${index}.api_key`] = Boolean(prior?.configured);
        if (prior?.change) S().secretChanges[`${prefix}${index}.api_key`] = prior.change;
      });
    };
    const body = el("div.st-accounts");
    const paint = () => {
      body.replaceChildren();
      provider.accounts.forEach((account, index) => {
        const item = el("div.st-account");
        item.append(
          el("div.st-account-head", null,
            textInput(account.name || `账号 ${index + 1}`, (value) => { account.name = value; dirty(); }, { placeholder: "账号名称", ariaLabel: "账号名称" }),
            iconButton("trash-2", "删除账号", async () => {
              if (!(await confirmAction(`删除账号“${account.name || index + 1}”？`, "删除"))) return;
              const previous = provider.accounts.map((entry) => ({ ...entry }));
              if (provider.accounts.length === 1) provider.accounts[0] = { id: provider.accounts[0].id || "account-1", name: "默认账号", api_key: "" };
              else provider.accounts.splice(index, 1);
              reindex(previous);
              if (previous.length === 1) { S().secretStates[`${prefix}0.api_key`] = false; S().secretChanges[`${prefix}0.api_key`] = { action: "clear" }; }
              dirty();
              paint();
            }, "is-danger")),
          secretControl(`${prefix}${index}.api_key`));
        body.append(item);
      });
    };
    paint();
    const add = button("新建账号", { iconName: "plus", small: true, onClick: () => {
      if (provider.accounts.length >= 32) return toast("每个平台最多配置 32 个账号", "error");
      const previous = provider.accounts.map((entry) => ({ ...entry }));
      let number = 2;
      while (provider.accounts.some((account) => account.name === `账号 ${number}`)) number += 1;
      provider.accounts.push({ id: `account-${globalThis.crypto?.randomUUID?.() || `${Date.now()}-${Math.random().toString(36).slice(2)}`}`, name: `账号 ${number}`, api_key: "" });
      reindex(previous);
      dirty();
      paint();
    } });
    return card([body, add], { title: providerKey === "deepseek" ? "DeepSeek 账号" : "OpenRouter 账号", description: providerKey === "deepseek" ? "余额按 CNY 与 USD 分成两个池，分别显示。" : "每个账号对应一个 OpenRouter API Key。" });
  }

  function remapApiQuotaSecrets(previousConfig, nextConfig) {
    for (const providerKey of ["deepseek", "openrouter"]) {
      const prefix = `plugins.api_quota.${providerKey}.accounts.`;
      const previousAccounts = previousConfig?.plugins?.api_quota?.[providerKey]?.accounts || [];
      const saved = new Map(previousAccounts.map((account, index) => [account.id, { configured: Boolean(S().secretStates[`${prefix}${index}.api_key`]), change: S().secretChanges[`${prefix}${index}.api_key`] }]).filter(([id]) => id));
      for (const key of Object.keys(S().secretStates)) if (key.startsWith(prefix)) delete S().secretStates[key];
      for (const key of Object.keys(S().secretChanges)) if (key.startsWith(prefix)) delete S().secretChanges[key];
      (nextConfig?.plugins?.api_quota?.[providerKey]?.accounts || []).forEach((account, index) => {
        const prior = saved.get(account.id);
        S().secretStates[`${prefix}${index}.api_key`] = Boolean(prior?.configured);
        if (prior?.change) S().secretChanges[`${prefix}${index}.api_key`] = prior.change;
      });
    }
  }

  /* ───────────────────────── QQ 平台 ───────────────────────── */

  function qqConfig() {
    const draft = S().configDraft;
    if (!draft.platforms || typeof draft.platforms !== "object") draft.platforms = {};
    if (!draft.platforms.qq || typeof draft.platforms.qq !== "object") draft.platforms.qq = {};
    return draft.platforms.qq;
  }

  function qqBindingFor(field, keyOverride) {
    const key = keyOverride || field.path || field.key;
    const binding = pathBinding(`platforms.qq.${key}`, { nullable: Boolean(field.nullable || field.optional) });
    binding.secretKey = `platforms.qq.${key}`;
    return binding;
  }

  const QQ_SECTIONS = [
    { id: "connection", title: "连接", description: "NapCat / OneBot 反向 WebSocket 接入与基础行为。" },
    { id: "access", title: "权限与白名单", description: "谁能找她说话、谁是管理员。" },
    { id: "limits", title: "限流与并发", description: "非白名单的节流，以及同时能跑几轮。" },
    { id: "models", title: "模型", description: "QQ 里用哪些模型；不设则继承全局池。" }
  ];

  function renderQqPage(root) {
    const qq = qqConfig();
    const switchLabel = el("span", { text: qq.enabled ? "已启用" : "未启用" });
    const enabled = toggle(Boolean(qq.enabled), (value) => { qq.enabled = value; dirty(); switchLabel.textContent = value ? "已启用" : "未启用"; root.classList.toggle("is-platform-off", !value); });
    root.append(el("div.st-page-head", null, el("div", null, el("h2", { text: "QQ 平台" }), el("p.st-page-desc", { text: "腾讯 QQ 接入。改动保存后会重启监听。" })), el("label.st-head-switch", null, switchLabel, enabled)));
    root.classList.toggle("is-platform-off", !qq.enabled);
    const qqSchema = schema().qq || {};
    QQ_SECTIONS.forEach((section, index) => {
      const fields = (qqSchema[section.id] || []).filter((field) => field.path !== "enabled");
      const node = card(fieldRows(fields, qqBindingFor), { title: section.title, description: section.description });
      node.style.setProperty("--i", String(index));
      root.append(node);
    });
    root.append(routesCard());
    root.append(qqPluginsCard());
  }

  /* 会话专属配置(私聊/群聊路由) */
  function routes() {
    const qq = qqConfig();
    if (!Array.isArray(qq.conversations)) qq.conversations = [];
    return qq.conversations;
  }

  function routeSummary(route) {
    const chips = [];
    const persona = route.persona?.mode;
    if (persona === "miyu") chips.push(chip("人格：Miyu", "is-soft"));
    else if (persona === "custom") chips.push(chip(`人格：${String(route.persona?.name || "").replace(/\.md$/i, "")}`, "is-soft"));
    if (Array.isArray(route.text_models) && route.text_models.length) chips.push(chip(`文本 ${route.text_models.length}`, "is-accent"));
    else if (route.text_models_inheritance === "global") chips.push(chip("文本：继承全局", "is-soft"));
    if (Array.isArray(route.multimodal_models) && route.multimodal_models.length) chips.push(chip(`多模态 ${route.multimodal_models.length}`, "is-accent"));
    if (route.extra_prompt) chips.push(chip("额外提示词", "is-soft"));
    if (route.session_limits) chips.push(chip(`并行 ${route.session_limits.running}`, "is-soft"));
    return chips;
  }

  function routesCard() {
    const list = routes();
    const add = button("新增会话配置", { iconName: "plus", small: true, onClick: () => {
      list.push({ conversation: { kind: "group", id: "" } });
      dirty();
      openRouteDrawer(list.length - 1);
    } });
    const body = el("div.st-card-body.is-list");
    if (!list.length) body.append(empty("没有专属配置；所有会话按上面的平台设置走。"));
    list.forEach((route, index) => {
      const item = el("button.st-route-row", { type: "button", onclick: () => openRouteDrawer(index) });
      item.style.setProperty("--i", String(index));
      item.append(
        el("span.st-server-mark", null, icon(route.conversation?.kind === "private" ? "message-circle" : "users")),
        el("span.st-route-copy", null, el("strong", { text: `${route.conversation?.kind === "private" ? "私聊" : "群聊"} ${route.conversation?.id || "（未填号码）"}` }), el("span.st-provider-chips", null, routeSummary(route).length ? routeSummary(route) : chip("未覆盖任何项", "is-soft"))),
        icon("chevron-right", "st-card-caret"));
      body.append(item);
    });
    const node = el("section.st-card", null,
      el("header.st-card-head", null, el("div", null, el("h3", { text: "会话专属配置" }), el("p", { text: "某个群或私聊单独用另一套人格、模型或提示词。" })), el("div.st-card-actions", null, add)),
      body);
    node.style.setProperty("--i", "4");
    return node;
  }

  function openRouteDrawer(index) {
    const list = routes();
    const route = list[index];
    if (!route) return;
    if (!route.conversation) route.conversation = { kind: "group", id: "" };
    if (!route.persona) route.persona = { mode: "inherit" };
    const personaChoices = (S().promptDraft?.personas || []).map((doc) => ({ value: doc.name, label: displayName(doc) }));
    const fields = (schema().qq?.routes?.fields || []).map((field) => field.key === "persona.name"
      ? { ...field, kind: "select", hint: personaChoices.length ? "从「人格」页里已有的人格中选" : "还没有自定义人格，先去「人格」页新建", choices: [{ value: "", label: "请选择" }, ...personaChoices] }
      : field);
    const bindingFor = (field, keyOverride) => {
      const key = keyOverride || field.key;
      const base = nestedBinding(route, key);
      return { ...base, set: (value) => {
        if (key === "conversation.id") value = String(value ?? "").trim();
        base.set(value);
        if (key.startsWith("persona.")) normalizeRoutePersona(route);
        drawer.setTitle(routeTitle(route));
      } };
    };
    const routeTitle = (item) => `${item.conversation?.kind === "private" ? "私聊" : "群聊"} ${item.conversation?.id || ""}`.trim();
    const drawer = openDrawer({
      title: routeTitle(route),
      subtitle: "会话专属配置",
      width: "560px",
      body: (body) => body.append(card(fieldRows(fields, bindingFor))),
      footer: [
        button("删除", { kind: "text", danger: true, iconName: "trash-2", onClick: async () => {
          if (!(await confirmAction("删除这条会话专属配置？", "删除"))) return;
          list.splice(index, 1);
          dirty();
          closeDrawer();
        } }),
        el("span.st-foot-spacer"),
        button("完成", { kind: "primary", onClick: () => closeDrawer() })
      ],
      onClose: () => {
        normalizeRoutePersona(route);
        if (!String(route.conversation?.id || "").trim()) { list.splice(list.indexOf(route), 1); dirty(); toast("没有填号码的会话配置已丢弃", "error"); }
        rerender("qq");
      }
    });
  }

  function normalizeRoutePersona(route) {
    const persona = route.persona;
    if (!persona || persona.mode === "inherit" || !persona.mode) { delete route.persona; return; }
    if (persona.mode === "miyu") { route.persona = { mode: "miyu" }; return; }
    if (persona.mode === "custom") {
      const name = String(persona.name || "").trim();
      route.persona = { mode: "custom", name: name ? documentName(name) : "" };
    }
  }

  /* QQ 插件卡片墙 */
  function qqPluginInstance(id) {
    const qq = qqConfig();
    if (!qq.plugins || typeof qq.plugins !== "object") qq.plugins = {};
    if (!qq.plugins[id] || typeof qq.plugins[id] !== "object") qq.plugins[id] = {};
    if (!qq.plugins[id].settings || typeof qq.plugins[id].settings !== "object") qq.plugins[id].settings = {};
    return qq.plugins[id];
  }

  function qqPluginEnabled(id, definition) {
    const instance = qqConfig().plugins?.[id];
    return typeof instance?.enabled === "boolean" ? instance.enabled : Boolean(definition.enabledDefault);
  }

  function qqPluginsCard() {
    const definitions = schema().qqPlugins || {};
    const grid = el("div.st-grid.is-plugins");
    Object.entries(definitions).forEach(([id, definition], index) => {
      const node = el("div.st-plugin-card");
      node.style.setProperty("--i", String(index));
      const enabled = qqPluginEnabled(id, definition);
      node.classList.toggle("is-off", !enabled);
      node.append(
        el("button.st-plugin-open", { type: "button", onclick: () => openQqPluginDrawer(id) }, mark(definition.title || id), el("span.st-plugin-copy", null, el("strong", { text: definition.title || id }), el("small", { text: definition.description || id }))),
        toggle(enabled, (value) => { qqPluginInstance(id).enabled = value; node.classList.toggle("is-off", !value); dirty(); }, `${definition.title} 启用`));
      grid.append(node);
    });
    const node = el("section.st-card.is-plain", null, el("header.st-card-head", null, el("div", null, el("h3", { text: "QQ 插件" }), el("p", { text: "群聊真实上下文、回复处理、入群审批、定时消息等。卡片上开关，点开调参数。" }))), grid);
    node.style.setProperty("--i", "5");
    return node;
  }

  function openQqPluginDrawer(id) {
    const definition = (schema().qqPlugins || {})[id];
    if (!definition) return;
    const instance = qqPluginInstance(id);
    const settings = instance.settings;
    const bindingFor = (field, keyOverride) => nestedBinding(settings, keyOverride || field.key);
    const statusCard = () => card([row("插件状态", toggle(qqPluginEnabled(id, definition), (value) => { instance.enabled = value; dirty(); rerender("qq"); }), { hint: definition.description || "" })]);
    let tabs = null;
    let body = null;
    if (Array.isArray(definition.groups) && definition.groups.length) {
      tabs = definition.groups.map((group, index) => ({
        id: group.id, label: group.title,
        render: (host) => {
          if (index === 0) host.append(statusCard());
          host.append(card(fieldRows(group.fields, bindingFor)));
        }
      }));
    } else {
      body = (host) => {
        host.append(statusCard());
        if (definition.fields?.length) host.append(card(fieldRows(definition.fields, bindingFor)));
        if (definition.custom === "join_approval_groups") host.append(joinApprovalGroupsCard(settings, definition));
        if (definition.custom === "scheduled_tasks") host.append(scheduledTasksCard(settings, definition));
      };
    }
    openDrawer({ title: definition.title || id, subtitle: `platforms.qq.plugins.${id}`, width: tabs ? "760px" : "560px", tabs: tabs || undefined, body: body || undefined, footer: [el("span.st-foot-spacer"), button("完成", { kind: "primary", onClick: () => closeDrawer() })], onClose: () => rerender("qq") });
  }

  function joinApprovalGroupsCard(settings, definition) {
    if (!Array.isArray(settings.groups)) settings.groups = [];
    const list = settings.groups;
    const body = el("div.st-accounts");
    const paint = () => {
      body.replaceChildren();
      if (!list.length) body.append(empty("没有分群条件；未列出的群按插件默认逻辑处理。"));
      list.forEach((group, index) => {
        const item = el("div.st-account");
        item.append(
          el("div.st-account-head", null,
            el("span.st-unit-wrap", null, numberInput({ label: "群号", min: 1, integer: true }, group.group_id ?? "", (value) => { group.group_id = value; dirty(); }), el("span.st-unit", { text: "群号" })),
            iconButton("trash-2", "删除", async () => { if (!(await confirmAction("删除这条审批条件？", "删除"))) return; list.splice(index, 1); dirty(); paint(); }, "is-danger")),
          textarea(group.approve_condition || "", (value) => { group.approve_condition = value; dirty(); }, { rows: 3, placeholder: "通过条件，自然语言描述", ariaLabel: "通过条件" }));
        body.append(item);
      });
    };
    paint();
    return card([body, button("新增一项", { iconName: "plus", small: true, onClick: () => { list.push({ group_id: null, approve_condition: "" }); dirty(); paint(); body.querySelector(".st-account:last-of-type input")?.focus(); } })], { title: "分群审批条件", description: "每个群一条自然语言条件，模型据此判断入群申请。" });
  }

  function parseConversation(text) {
    const match = /^(group|private):(\d+)$/.exec(String(text || "").trim());
    return match ? { kind: match[1], id: match[2] } : { kind: "group", id: "" };
  }

  function scheduledTasksCard(settings, definition) {
    if (!Array.isArray(settings.tasks)) settings.tasks = [];
    const list = settings.tasks;
    const body = el("div.st-card-body.is-list");
    const paint = () => {
      body.replaceChildren();
      if (!list.length) body.append(empty("没有定时任务。"));
      list.forEach((task, index) => {
        const conversation = parseConversation(task.conversation);
        const item = el("button.st-route-row", { type: "button", onclick: () => openTaskDialog(index) });
        item.append(
          el("span.st-server-mark", null, icon("alarm-clock" in ICONS ? "alarm-clock" : "message-circle")),
          el("span.st-route-copy", null,
            el("strong", { text: `${conversation.kind === "private" ? "私聊" : "群聊"} ${conversation.id} · ${(task.times || []).join(" / ") || "未设时间"}` }),
            el("small", { text: `${Array.isArray(task.days) && task.days.length ? task.days.join(",") + " · " : "每天 · "}${String(task.message || "").slice(0, 60)}` })),
          icon("chevron-right", "st-card-caret"));
        body.append(item);
      });
    };
    const openTaskDialog = (index) => {
      const editing = index != null;
      const source = editing ? list[index] : { conversation: "group:", times: [], message: "", days: [], account: null };
      const conversation = parseConversation(source.conversation);
      const draft = { conversation, times: [...(source.times || [])], message: source.message || "", days: [...(source.days || [])], account: source.account ?? null };
      const bindingFor = (field, keyOverride) => nestedBinding(draft, keyOverride || field.key);
      const handle = openDialog({
        title: editing ? "编辑定时任务" : "新增定时任务",
        width: "560px",
        body: (host) => host.append(card(fieldRows(definition.taskFields || [], bindingFor))),
        actions: [
          button("取消", { onClick: () => handle.close() }),
          button(editing ? "保存" : "添加", { kind: "primary", onClick: () => {
            if (!/^\d+$/.test(String(draft.conversation.id || ""))) return toast("号码必须是纯数字", "error");
            const times = (draft.times || []).map((item) => String(item).trim()).filter(Boolean);
            if (!times.length || times.some((item) => !/^([01]\d|2[0-3]):[0-5]\d$/.test(item))) return toast("时间点格式为 HH:MM，至少一个", "error");
            if (!String(draft.message || "").trim()) return toast("发送内容不能为空", "error");
            const next = { conversation: `${draft.conversation.kind}:${draft.conversation.id}`, times, message: draft.message };
            if (Array.isArray(draft.days) && draft.days.length) next.days = draft.days;
            if (draft.account != null) next.account = draft.account;
            if (editing) list[index] = next; else list.push(next);
            dirty();
            handle.close();
            paint();
          } }),
          editing ? button("删除", { kind: "text", danger: true, onClick: async () => { if (!(await confirmAction("删除这个定时任务？", "删除"))) return; list.splice(index, 1); dirty(); handle.close(); paint(); } }) : null
        ].filter(Boolean)
      });
    };
    paint();
    return card([body, button("新增任务", { iconName: "plus", small: true, onClick: () => openTaskDialog(null) })], { title: "任务", description: "到点向指定群或私聊发送固定内容。" });
  }

  /* ───────────────────────── 装配 ───────────────────────── */

  const PAGE_RENDERERS = {
    prompts: renderPromptsPage,
    providers: renderProvidersPage,
    models: renderModelsPage,
    general: renderGeneralPage,
    mcp: renderMcpPage,
    plugins: renderPluginsPage,
    qq: renderQqPage
  };

  function init(context) {
    ctx = context;
    for (const [name, render] of Object.entries(PAGE_RENDERERS)) {
      const root = document.getElementById(`settings-${name}`);
      if (root) pages.set(name, { root, render });
    }
  }

  function render() {
    if (!ctx || !S().configLoaded || !S().configDraft) return;
    closeDrawer();
    closeMenu();
    closePopover();
    for (const name of pages.keys()) rerender(name);
  }

  function renderPage(name) { rerender(name); }

  /* 导航切到某页:放开入场动画(hidden→显示会让 CSS 动画重新开始)。 */
  function onShow(name) { pages.get(name)?.root.classList.remove("is-settled"); }

  return { init, render, renderPage, onShow, remapApiQuotaSecrets, closeOverlays: () => { closeDrawer(); closeMenu(); closePopover(); } };
})();
