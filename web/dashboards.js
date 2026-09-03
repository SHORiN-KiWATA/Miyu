/*
 * 插件 dashboard 共享层(09-03)。
 *
 * 每个面板是一个独立文件,向这里 register({ name, mount, refresh });控制台
 * rail 切到对应 panel 时由 app.js 调 open(name)。这里只放所有面板都要的
 * 零件:请求封装、DOM 小工具、统计卡、分页条、抽屉、确认框。
 *
 * 加载顺序在 app.js 之前,所以拿不到那边的 createIcon——自带一份 lucide
 * 子集(同 shared.js 的做法)。
 */
window.MiyuDash = (() => {
  const ICONS = {
    "refresh-cw": [["path", { d: "M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" }], ["path", { d: "M21 3v5h-5" }], ["path", { d: "M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" }], ["path", { d: "M8 16H3v5" }]],
    "trash-2": [["path", { d: "M3 6h18" }], ["path", { d: "M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6" }], ["path", { d: "M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2" }], ["line", { x1: "10", x2: "10", y1: "11", y2: "17" }], ["line", { x1: "14", x2: "14", y1: "11", y2: "17" }]],
    search: [["circle", { cx: "11", cy: "11", r: "8" }], ["path", { d: "m21 21-4.3-4.3" }]],
    x: [["path", { d: "M18 6 6 18" }], ["path", { d: "m6 6 12 12" }]],
    "chevron-left": [["path", { d: "m15 18-6-6 6-6" }]],
    "chevron-right": [["path", { d: "m9 18 6-6-6-6" }]],
    brain: [["path", { d: "M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z" }], ["path", { d: "M12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18Z" }], ["path", { d: "M15 13a4.5 4.5 0 0 1-3-4 4.5 4.5 0 0 1-3 4" }]]
  };
  const SVG_NS = "http://www.w3.org/2000/svg";

  function icon(name) {
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    for (const [tag, attrs] of ICONS[name] || []) {
      const node = document.createElementNS(SVG_NS, tag);
      for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
      svg.appendChild(node);
    }
    return svg;
  }

  /* 建元素:el("div.dash-card", { title }, child, "text", ...) */
  function el(spec, attrs, ...children) {
    const [tag, ...classes] = spec.split(".");
    const node = document.createElement(tag || "div");
    if (classes.length) node.className = classes.join(" ");
    for (const [key, value] of Object.entries(attrs || {})) {
      if (value === null || value === undefined || value === false) continue;
      if (key === "text") node.textContent = value;
      else if (key === "html") node.innerHTML = value;
      else if (key.startsWith("on") && typeof value === "function") node.addEventListener(key.slice(2), value);
      else if (key === "dataset") Object.assign(node.dataset, value);
      else node.setAttribute(key, value === true ? "" : value);
    }
    for (const child of children.flat()) {
      if (child === null || child === undefined || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  function iconButton(name, title, onClick, extraClass) {
    const button = el(`button.dash-icon-button${extraClass ? `.${extraClass}` : ""}`, { type: "button", title, "aria-label": title, onclick: onClick });
    button.appendChild(icon(name));
    return button;
  }

  /* 统一的 JSON 请求:非 2xx 抛出带服务端 message 的 Error。 */
  async function api(path, options = {}) {
    const init = { method: options.method || "GET", headers: {} };
    if (options.body !== undefined) {
      init.headers["content-type"] = "application/json";
      init.body = JSON.stringify(options.body);
    }
    const response = await fetch(path, init);
    let payload = null;
    try {
      payload = await response.json();
    } catch (_) { /* 空体 */ }
    if (!response.ok) {
      throw new Error(payload?.error?.message || `HTTP ${response.status}`);
    }
    return payload;
  }

  function statCards(items) {
    const grid = el("div.dash-cards");
    for (const item of items) {
      grid.append(el("div.dash-card", null,
        el("span.dash-card-label", { text: item.label }),
        el("strong.dash-card-value", { text: item.value ?? "—" }),
        item.hint ? el("span.dash-card-hint", { text: item.hint }) : null));
    }
    return grid;
  }

  /* 分页条:offset/limit/total 三个数算出「第 a–b 条 / 共 n」与前后翻页。 */
  function pager({ offset, limit, total, onChange }) {
    const start = total === 0 ? 0 : offset + 1;
    const end = Math.min(offset + limit, total);
    const bar = el("div.dash-pager");
    const prev = iconButton("chevron-left", "上一页", () => onChange(Math.max(0, offset - limit)));
    const next = iconButton("chevron-right", "下一页", () => onChange(offset + limit));
    prev.disabled = offset <= 0;
    next.disabled = end >= total;
    bar.append(el("span.dash-pager-text", { text: total ? `第 ${start}–${end} 条 / 共 ${total}` : "没有条目" }), prev, next);
    return bar;
  }

  /* 右侧抽屉:同一时间只开一个;点遮罩或 × 关闭。 */
  let drawer = null;
  function openDrawer(title, body, actions) {
    closeDrawer();
    drawer = el("div.dash-drawer-overlay", { onclick: (event) => { if (event.target === drawer) closeDrawer(); } });
    const panel = el("aside.dash-drawer", { role: "dialog", "aria-label": title });
    const head = el("header.dash-drawer-head", null, el("strong", { text: title }), iconButton("x", "关闭", closeDrawer));
    const content = el("div.dash-drawer-body", null, body);
    panel.append(head, content);
    if (actions?.length) panel.append(el("footer.dash-drawer-foot", null, actions));
    drawer.append(panel);
    document.body.appendChild(drawer);
    document.addEventListener("keydown", onDrawerKey);
  }
  function onDrawerKey(event) {
    if (event.key === "Escape") closeDrawer();
  }
  function closeDrawer() {
    if (!drawer) return;
    drawer.remove();
    drawer = null;
    document.removeEventListener("keydown", onDrawerKey);
  }

  /* 危险操作确认:原生 dialog,CSP 下不能内联,所以全部程序化生成。 */
  function confirmAction(message, confirmLabel = "删除") {
    return new Promise((resolve) => {
      const dialog = el("dialog.dash-confirm");
      const cancel = el("button.dash-button", { type: "button", text: "取消", onclick: () => { dialog.close(); resolve(false); } });
      const ok = el("button.dash-button.is-danger", { type: "button", text: confirmLabel, onclick: () => { dialog.close(); resolve(true); } });
      dialog.append(el("p", { text: message }), el("div.dash-confirm-actions", null, cancel, ok));
      dialog.addEventListener("close", () => { dialog.remove(); resolve(false); });
      document.body.appendChild(dialog);
      dialog.showModal();
    });
  }

  function formatTime(value) {
    if (!value) return "";
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return String(value);
    const pad = (n) => String(n).padStart(2, "0");
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
  }

  const panels = new Map();
  function register(panel) {
    panels.set(panel.name, panel);
  }
  /* rail 切到某面板:首次挂载,之后只刷新。 */
  function open(name) {
    const panel = panels.get(name);
    if (!panel) return;
    const root = document.getElementById(panel.root);
    if (!root) return;
    if (!panel.mounted) {
      panel.mount(root);
      panel.mounted = true;
    } else if (panel.refresh) {
      panel.refresh();
    }
  }

  return { register, open, api, el, icon, iconButton, statCards, pager, openDrawer, closeDrawer, confirmAction, formatTime };
})();
