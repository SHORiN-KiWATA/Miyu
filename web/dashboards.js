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
    brain: [["path", { d: "M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z" }], ["path", { d: "M12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18Z" }], ["path", { d: "M15 13a4.5 4.5 0 0 1-3-4 4.5 4.5 0 0 1-3 4" }]],
    plus: [["path", { d: "M5 12h14" }], ["path", { d: "M12 5v14" }]],
    pencil: [["path", { d: "M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" }], ["path", { d: "m15 5 4 4" }]],
    history: [["path", { d: "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" }], ["path", { d: "M3 3v5h5" }], ["path", { d: "M12 7v5l4 2" }]],
    archive: [["rect", { width: "20", height: "5", x: "2", y: "3", rx: "1" }], ["path", { d: "M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8" }], ["path", { d: "M10 12h4" }]],
    eraser: [["path", { d: "m7 21-4.3-4.3c-1-1-1-2.5 0-3.4l9.6-9.6c1-1 2.5-1 3.4 0l5.6 5.6c1 1 1 2.5 0 3.4L13 21" }], ["path", { d: "M22 21H7" }], ["path", { d: "m5 11 9 9" }]],
    "rotate-ccw": [["path", { d: "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" }], ["path", { d: "M3 3v5h5" }]],
    check: [["path", { d: "M20 6 9 17l-5-5" }]],
    "external-link": [["path", { d: "M15 3h6v6" }], ["path", { d: "M10 14 21 3" }], ["path", { d: "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" }]]
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

  const reducedMotion = () => {
    try { return window.matchMedia("(prefers-reduced-motion: reduce)").matches; } catch (_) { return false; }
  };

  /* 纯整数的统计值从 0 数到位;带单位/小数/文字的原样显示。 */
  function countUp(node, value) {
    const text = String(value ?? "—");
    if (reducedMotion() || !/^\d{1,9}$/.test(text)) { node.textContent = text; return; }
    const target = Number(text);
    if (target < 8) { node.textContent = text; return; }
    const start = performance.now();
    const duration = 520;
    const tick = (now) => {
      const p = Math.min(1, (now - start) / duration);
      const eased = 1 - Math.pow(1 - p, 3);
      node.textContent = String(Math.round(target * eased));
      if (p < 1) requestAnimationFrame(tick); else node.textContent = text;
    };
    requestAnimationFrame(tick);
  }

  function statCards(items) {
    const grid = el("div.dash-cards");
    items.forEach((item, index) => {
      const value = el("strong.dash-card-value");
      countUp(value, item.value);
      const card = el("div.dash-card", null,
        el("span.dash-card-label", { text: item.label }),
        value,
        item.hint ? el("span.dash-card-hint", { text: item.hint }) : null);
      card.style.setProperty("--i", String(index));
      grid.append(card);
    });
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
    // 捕获阶段拦 Escape:app.js 在 document 上也听 Escape 并会把整个控制台关掉,
    // 抽屉开着时 Escape 只该关抽屉。
    document.addEventListener("keydown", onDrawerKey, true);
  }
  function onDrawerKey(event) {
    if (event.key !== "Escape") return;
    event.stopPropagation();
    event.preventDefault();
    closeDrawer();
  }
  function closeDrawer() {
    if (!drawer) return;
    drawer.remove();
    drawer = null;
    document.removeEventListener("keydown", onDrawerKey, true);
  }

  /* 危险操作确认:原生 dialog,CSP 下不能内联,所以全部程序化生成。 */
  function confirmAction(message, confirmLabel = "删除") {
    return new Promise((resolve) => {
      const dialog = el("dialog.dash-confirm");
      const cancel = el("button.dash-button", { type: "button", text: "取消", onclick: () => { dialog.close(); resolve(false); } });
      const ok = el("button.dash-button.is-danger", { type: "button", text: confirmLabel, onclick: () => { dialog.close(); resolve(true); } });
      dialog.append(el("p", { text: message }), el("div.dash-confirm-actions", null, cancel, ok));
      // 同上:Escape 关的是确认框,不能顺带把控制台关了。
      dialog.addEventListener("keydown", (event) => { if (event.key === "Escape") event.stopPropagation(); });
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

  /* 分段切换(标签页):options [{value,label}],返回 {el, set}。 */
  function segmented(options, value, onChange) {
    const wrap = el("div.con-segmented");
    const set = (next) => {
      for (const button of wrap.querySelectorAll("button")) button.classList.toggle("on", button.dataset.value === next);
    };
    for (const option of options) {
      wrap.append(el("button", { type: "button", text: option.label, dataset: { value: option.value }, onclick: () => { set(option.value); onChange(option.value); } }));
    }
    set(value);
    return { el: wrap, set };
  }

  /* 下拉:options [{value,label}] 或 ["a","b"]。 */
  function select(options, value, onChange, title) {
    const node = el("select.dash-select", { title, onchange: () => onChange(node.value) });
    for (const option of options) {
      const item = typeof option === "string" ? { value: option, label: option } : option;
      node.append(el("option", { value: item.value, text: item.label }));
    }
    if (value !== undefined) node.value = value;
    return node;
  }

  /* 表格:columns [{label, width}] 决定列模板,rows 由调用方渲染成 dash-row。
     列宽走 CSSOM 自定义属性——CSP 禁 style 属性,不禁 element.style。 */
  /* 可排序列:column.sort 给键名,options.sort = {key, dir: "asc"|"desc", onChange(key, dir)};
     点当前列翻转方向,点别的列切到那列并按默认方向(desc)。 */
  function table(columns, options = {}) {
    const grid = el("div.dash-table", { role: "table" });
    grid.style.setProperty("--dash-cols", columns.map((column) => column.width || "1fr").join(" "));
    grid.style.setProperty("--dash-min", columns.length > 5 ? "820px" : "0");
    const head = el("div.dash-row.is-head", { role: "row" });
    const sort = options.sort;
    for (const column of columns) {
      if (column.sort && sort) {
        const active = sort.key === column.sort;
        const cell = el(`span.is-sortable${active ? ".is-active" : ""}`, {
          role: "columnheader", tabindex: "0", title: "点击排序",
          "aria-sort": active ? (sort.dir === "asc" ? "ascending" : "descending") : "none",
          onclick: () => sort.onChange(column.sort, active && sort.dir === "desc" ? "asc" : "desc"),
          onkeydown: (event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); cell.click(); } }
        }, column.label || "", el("i.dash-sort-arrow", { text: active ? (sort.dir === "asc" ? "▲" : "▼") : "▾" }));
        head.append(cell);
      } else {
        head.append(el("span", { text: column.label || "" }));
      }
    }
    grid.append(head);
    return grid;
  }

  /* 表单行:label + 控件。 */
  function field(label, control, hint) {
    return el("label.dash-field", null, el("span.dash-field-label", { text: label }), control, hint ? el("small.dash-field-hint", { text: hint }) : null);
  }

  /* 垂直时间线:entries [{time, title, body, chip, chipClass}]。 */
  function timeline(entries, emptyText) {
    if (!entries.length) return el("p.dash-empty", { text: emptyText || "暂无记录" });
    const list = el("ol.dash-timeline");
    for (const entry of entries) {
      const item = el("li.dash-timeline-item", null,
        el("time.dash-timeline-time", { text: entry.time || "" }),
        el("div.dash-timeline-card", null,
          entry.chip ? el(`span.dash-chip${entry.chipClass ? `.${entry.chipClass}` : ""}`, { text: entry.chip }) : null,
          entry.title ? el("strong.dash-timeline-title", { text: entry.title }) : null,
          entry.body instanceof Node ? entry.body : (entry.body ? el("p.dash-timeline-body", { text: entry.body }) : null)));
      list.append(item);
    }
    return list;
  }

  /* 内联 SVG 折线(属性画,不写 style)。points [{x, y}],y 越大越高。 */
  function sparkline(points, options = {}) {
    const width = options.width || 260;
    const height = options.height || 60;
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
    svg.setAttribute("class", "dash-sparkline");
    svg.setAttribute("preserveAspectRatio", "none");
    if (points.length < 2) return svg;
    const xs = points.map((p) => p.x);
    const ys = points.map((p) => p.y);
    const minX = Math.min(...xs), maxX = Math.max(...xs);
    let minY = options.min ?? Math.min(...ys), maxY = options.max ?? Math.max(...ys);
    if (options.min === undefined && options.max === undefined) {
      if (options.baseline !== undefined) { minY = Math.min(minY, options.baseline); maxY = Math.max(maxY, options.baseline); }
      const pad = Math.max((maxY - minY) * 0.15, 0.5);
      minY -= pad; maxY += pad;
    }
    const sx = (x) => maxX === minX ? 0 : ((x - minX) / (maxX - minX)) * (width - 4) + 2;
    const sy = (y) => maxY === minY ? height / 2 : height - 2 - ((y - minY) / (maxY - minY)) * (height - 4);
    if (options.baseline !== undefined && options.baseline >= minY && options.baseline <= maxY) {
      const base = document.createElementNS(SVG_NS, "line");
      base.setAttribute("x1", "0"); base.setAttribute("x2", String(width));
      base.setAttribute("y1", String(sy(options.baseline))); base.setAttribute("y2", String(sy(options.baseline)));
      base.setAttribute("class", "dash-sparkline-base");
      svg.append(base);
    }
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("d", points.map((p, i) => `${i ? "L" : "M"}${sx(p.x).toFixed(1)} ${sy(p.y).toFixed(1)}`).join(" "));
    path.setAttribute("class", "dash-sparkline-line");
    // pathLength=1 让 CSS 里 stroke-dasharray:1 能做"画线"动画,不用量真实长度。
    path.setAttribute("pathLength", "1");
    svg.append(path);
    return svg;
  }

  /* 底部提示:成功/失败都走这里,3 秒消失。 */
  let toastNode = null;
  function toast(message, kind) {
    if (toastNode) toastNode.remove();
    toastNode = el(`div.dash-toast${kind ? `.is-${kind}` : ""}`, { text: message, role: "status" });
    document.body.appendChild(toastNode);
    setTimeout(() => { toastNode?.remove(); toastNode = null; }, 3200);
  }

  /* 本地记住作用域选择(人格/账号/库),存取都包 try——隐私模式会抛。 */
  function remember(key, value) {
    try { localStorage.setItem(`miyu.dash.${key}`, value); } catch (_) { /* 忽略 */ }
  }
  function recall(key) {
    try { return localStorage.getItem(`miyu.dash.${key}`) || ""; } catch (_) { return ""; }
  }

  const panels = new Map();
  function register(panel) {
    panels.set(panel.name, panel);
  }
  function has(name) {
    return panels.has(name);
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

  return { register, has, open, api, countUp, el, icon, iconButton, statCards, pager, openDrawer, closeDrawer, confirmAction, formatTime,
    segmented, select, table, field, timeline, sparkline, toast, remember, recall };
})();
