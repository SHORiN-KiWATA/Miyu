(() => {
  "use strict";

  const MAX_CONTENT_CHARS = 20_000;
  const MAX_CUSTOM_ANSWER_CHARS = 4_000;
  const MAX_TOOL_OUTPUT_CHARS = 200_000;
  const NEAR_BOTTOM_PX = 120;

  const SVG_NS = "http://www.w3.org/2000/svg";
  const ICONS = {
    "arrow-down": [["path", { d: "M12 5v14" }], ["path", { d: "m19 12-7 7-7-7" }]],
    "arrow-up": [["path", { d: "m5 12 7-7 7 7" }], ["path", { d: "M12 19V5" }]],
    atom: [["circle", { cx: "12", cy: "12", r: "1" }], ["path", { d: "M20.2 20.2c2.04-2.03.02-7.37-4.5-11.9-4.52-4.52-9.87-6.54-11.9-4.5-2.04 2.03-.02 7.37 4.5 11.9 4.52 4.52 9.87 6.54 11.9 4.5Z" }], ["path", { d: "M15.7 15.7c4.52-4.52 6.54-9.87 4.5-11.9-2.03-2.04-7.37-.02-11.9 4.5-4.52 4.52-6.54 9.87-4.5 11.9 2.03 2.04 7.37.02 11.9-4.5Z" }]],
    check: [["path", { d: "M20 6 9 17l-5-5" }]],
    "chevron-down": [["path", { d: "m6 9 6 6 6-6" }]],
    "chevron-right": [["path", { d: "m9 18 6-6-6-6" }]],
    "circle-alert": [["circle", { cx: "12", cy: "12", r: "10" }], ["line", { x1: "12", x2: "12", y1: "8", y2: "12" }], ["line", { x1: "12", x2: "12.01", y1: "16", y2: "16" }]],
    "circle-help": [["circle", { cx: "12", cy: "12", r: "10" }], ["path", { d: "M9.09 9a3 3 0 1 1 5.83 1c0 2-3 3-3 3" }], ["path", { d: "M12 17h.01" }]],
    "circle-stop": [["circle", { cx: "12", cy: "12", r: "10" }], ["rect", { width: "6", height: "6", x: "9", y: "9", rx: "1" }]],
    copy: [["rect", { width: "14", height: "14", x: "8", y: "8", rx: "2", ry: "2" }], ["path", { d: "M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" }]],
    download: [["path", { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }], ["polyline", { points: "7 10 12 15 17 10" }], ["line", { x1: "12", x2: "12", y1: "15", y2: "3" }]],
    "external-link": [["path", { d: "M15 3h6v6" }], ["path", { d: "M10 14 21 3" }], ["path", { d: "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" }]],
    fileTerminal: [["path", { d: "M14.5 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7.5L14.5 2z" }], ["polyline", { points: "14 2 14 8 20 8" }], ["path", { d: "m8 13 2 2-2 2" }], ["path", { d: "M12 17h4" }]],
    lightbulb: [["path", { d: "M9 18h6" }], ["path", { d: "M10 22h4" }], ["path", { d: "M15.09 14c.18-.59.59-1.05 1.05-1.52A6 6 0 1 0 7.86 12.5c.45.44.85.9 1.03 1.5" }], ["path", { d: "M9 14h6v1a3 3 0 0 1-6 0v-1Z" }]],
    "list-todo": [["rect", { x: "3", y: "5", width: "6", height: "6", rx: "1" }], ["path", { d: "m3 17 2 2 4-4" }], ["path", { d: "M13 6h8" }], ["path", { d: "M13 12h8" }], ["path", { d: "M13 18h8" }]],
    "loader-circle": [["path", { d: "M21 12a9 9 0 1 1-6.219-8.56" }]],
    "lock-keyhole": [["circle", { cx: "12", cy: "16", r: "1" }], ["rect", { x: "3", y: "10", width: "18", height: "12", rx: "2" }], ["path", { d: "M7 10V7a5 5 0 0 1 10 0v3" }]],
    "log-in": [["path", { d: "M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4" }], ["polyline", { points: "10 17 15 12 10 7" }], ["line", { x1: "15", x2: "3", y1: "12", y2: "12" }]],
    "message-circle": [["path", { d: "M21 15a4 4 0 0 1-4 4H8l-5 3V7a4 4 0 0 1 4-4h10a4 4 0 0 1 4 4z" }]],
    "messages-square": [["path", { d: "M14 9a2 2 0 0 1-2 2H6l-4 4V5a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2z" }], ["path", { d: "M18 9h2a2 2 0 0 1 2 2v10l-4-4h-6a2 2 0 0 1-2-2v-1" }]],
    moon: [["path", { d: "M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z" }]],
    "panel-left": [["rect", { width: "18", height: "18", x: "3", y: "3", rx: "2" }], ["path", { d: "M9 3v18" }]],
    "refresh-cw": [["path", { d: "M21 12a9 9 0 0 0-15.35-6.35L3 8" }], ["path", { d: "M3 3v5h5" }], ["path", { d: "M3 12a9 9 0 0 0 15.35 6.35L21 16" }], ["path", { d: "M16 16h5v5" }]],
    route: [["circle", { cx: "6", cy: "19", r: "3" }], ["path", { d: "M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15" }], ["circle", { cx: "18", cy: "5", r: "3" }]],
    "settings-2": [["path", { d: "M20 7h-9" }], ["path", { d: "M14 17H5" }], ["circle", { cx: "17", cy: "17", r: "3" }], ["circle", { cx: "7", cy: "7", r: "3" }]],
    "sliders-horizontal": [["line", { x1: "21", x2: "14", y1: "4", y2: "4" }], ["line", { x1: "10", x2: "3", y1: "4", y2: "4" }], ["line", { x1: "21", x2: "12", y1: "12", y2: "12" }], ["line", { x1: "8", x2: "3", y1: "12", y2: "12" }], ["line", { x1: "21", x2: "16", y1: "20", y2: "20" }], ["line", { x1: "12", x2: "3", y1: "20", y2: "20" }], ["line", { x1: "14", x2: "14", y1: "2", y2: "6" }], ["line", { x1: "8", x2: "8", y1: "10", y2: "14" }], ["line", { x1: "16", x2: "16", y1: "18", y2: "22" }]],
    sparkles: [["path", { d: "m12 3-1.9 5.8a2 2 0 0 1-1.3 1.3L3 12l5.8 1.9a2 2 0 0 1 1.3 1.3L12 21l1.9-5.8a2 2 0 0 1 1.3-1.3L21 12l-5.8-1.9a2 2 0 0 1-1.3-1.3Z" }], ["path", { d: "M5 3v4" }], ["path", { d: "M19 17v4" }], ["path", { d: "M3 5h4" }], ["path", { d: "M17 19h4" }]],
    "square-pen": [["path", { d: "M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" }], ["path", { d: "M18.37 2.63a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4Z" }]],
    sun: [["circle", { cx: "12", cy: "12", r: "4" }], ["path", { d: "M12 2v2" }], ["path", { d: "M12 20v2" }], ["path", { d: "m4.93 4.93 1.42 1.42" }], ["path", { d: "m17.66 17.66 1.41 1.41" }], ["path", { d: "M2 12h2" }], ["path", { d: "M20 12h2" }], ["path", { d: "m6.34 17.66-1.41 1.41" }], ["path", { d: "m19.07 4.93-1.41 1.41" }]],
    "sun-moon": [["path", { d: "M12 8a2.83 2.83 0 0 0 4 4 4 4 0 1 1-4-4" }], ["path", { d: "M12 2v2" }], ["path", { d: "M12 20v2" }], ["path", { d: "m4.9 4.9 1.4 1.4" }], ["path", { d: "m17.7 17.7 1.4 1.4" }], ["path", { d: "M2 12h2" }], ["path", { d: "M20 12h2" }], ["path", { d: "m6.3 17.7-1.4 1.4" }], ["path", { d: "m19.1 4.9-1.4 1.4" }]],
    "triangle-alert": [["path", { d: "m21.73 18-8-14a2 2 0 0 0-3.46 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3" }], ["path", { d: "M12 9v4" }], ["path", { d: "M12 17h.01" }]],
    wrench: [["path", { d: "M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94z" }]],
    x: [["path", { d: "M18 6 6 18" }], ["path", { d: "m6 6 12 12" }]]
  };

  const EVENT_NAMES = [
    "run.started",
    "turn.started",
    "assistant.delta",
    "reasoning.start",
    "reasoning.reset",
    "reasoning.part_start",
    "reasoning.part_end",
    "reasoning.title",
    "reasoning.delta",
    "tool.started",
    "tool.progress",
    "tool.output",
    "tool.image",
    "tool.finished",
    "question.requested",
    "question.answered",
    "context.compact_start",
    "context.compact_delta",
    "context.compact_end",
    "context.pop_start",
    "context.pop_end",
    "context.error",
    "queue.added",
    "queue.removed",
    "queue.consumed",
    "run.completed",
    "run.cancelled",
    "run.failed",
    "conversation.reset",
    "resync_required"
  ];

  const RUN_EVENTS = new Set(EVENT_NAMES.filter((name) => !["conversation.reset", "resync_required", "queue.added", "queue.removed"].includes(name)));

  const elements = {
    body: document.body,
    sidebar: document.getElementById("sidebar"),
    sidebarScrim: document.getElementById("sidebarScrim"),
    sidebarClose: document.getElementById("sidebarClose"),
    mobileMenuButton: document.getElementById("mobileMenuButton"),
    sidebarStatusDot: document.getElementById("sidebarStatusDot"),
    sidebarConnectionStatus: document.getElementById("sidebarConnectionStatus"),
    newChatButton: document.getElementById("newChatButton"),
    currentConversation: document.getElementById("currentConversation"),
    sidebarConversationTitle: document.getElementById("sidebarConversationTitle"),
    sidebarConversationSnippet: document.getElementById("sidebarConversationSnippet"),
    sidebarConversationTime: document.getElementById("sidebarConversationTime"),
    contextNumbers: document.getElementById("contextNumbers"),
    contextTrack: document.getElementById("contextTrack"),
    contextBar: document.getElementById("contextBar"),
    settingsButton: document.getElementById("settingsButton"),
    sidebarThemeButton: document.getElementById("sidebarThemeButton"),
    conversationTitle: document.getElementById("conversationTitle"),
    conversationMeta: document.getElementById("conversationMeta"),
    modeSwitch: document.getElementById("modeSwitch"),
    modelMenuWrap: document.getElementById("modelMenuWrap"),
    modelButton: document.getElementById("modelButton"),
    modelMark: document.getElementById("modelMark"),
    modelLabel: document.getElementById("modelLabel"),
    modelMenu: document.getElementById("modelMenu"),
    themeButton: document.getElementById("themeButton"),
    topbarSettingsButton: document.getElementById("topbarSettingsButton"),
    errorRegion: document.getElementById("errorRegion"),
    chatScroll: document.getElementById("chatScroll"),
    loadingState: document.getElementById("loadingState"),
    blockedState: document.getElementById("blockedState"),
    blockedTitle: document.getElementById("blockedTitle"),
    blockedMessage: document.getElementById("blockedMessage"),
    loginForm: document.getElementById("loginForm"),
    loginPassword: document.getElementById("loginPassword"),
    loginError: document.getElementById("loginError"),
    loginSubmit: document.getElementById("loginSubmit"),
    loginSubmitLabel: document.getElementById("loginSubmitLabel"),
    retryBootstrapButton: document.getElementById("retryBootstrapButton"),
    timeline: document.getElementById("timeline"),
    emptyState: document.getElementById("emptyState"),
    promptGrid: document.getElementById("promptGrid"),
    jumpBottomButton: document.getElementById("jumpBottomButton"),
    composerDock: document.getElementById("composerDock"),
    questionDock: document.getElementById("questionDock"),
    composerForm: document.getElementById("composerForm"),
    composerInput: document.getElementById("composerInput"),
    queueTray: document.getElementById("queueTray"),
    composerState: document.getElementById("composerState"),
    characterCount: document.getElementById("characterCount"),
    stopButton: document.getElementById("stopButton"),
    sendButton: document.getElementById("sendButton"),
    drawerScrim: document.getElementById("drawerScrim"),
    settingsDrawer: document.getElementById("settingsDrawer"),
    settingsClose: document.getElementById("settingsClose"),
    settingsNav: document.querySelector(".settings-nav"),
    settingsPanels: Array.from(document.querySelectorAll("[data-settings-panel]")),
    settingsModelMark: document.getElementById("settingsModelMark"),
    settingsModelName: document.getElementById("settingsModelName"),
    settingsModelProvider: document.getElementById("settingsModelProvider"),
    capabilityList: document.getElementById("capabilityList"),
    versionLabel: document.getElementById("versionLabel"),
    generalConfigForm: document.getElementById("generalConfigForm"),
    providerEditor: document.getElementById("providerEditor"),
    addProviderButton: document.getElementById("addProviderButton"),
    modelPoolEditor: document.getElementById("modelPoolEditor"),
    pluginEditor: document.getElementById("pluginEditor"),
    promptEditor: document.getElementById("promptEditor"),
    advancedConfigEditor: document.getElementById("advancedConfigEditor"),
    applyAdvancedConfigButton: document.getElementById("applyAdvancedConfigButton"),
    reloadConfigButton: document.getElementById("reloadConfigButton"),
    saveConfigButton: document.getElementById("saveConfigButton"),
    settingsStatus: document.getElementById("settingsStatus"),
    toastRegion: document.getElementById("toastRegion"),
    resetDialog: document.getElementById("resetDialog"),
    resetCancelButton: document.getElementById("resetCancelButton"),
    resetConfirmButton: document.getElementById("resetConfirmButton")
  };

  const state = {
    bootId: null,
    latestEventId: 0,
    lastEventId: 0,
    replayRunId: null,
    replayCutoff: 0,
    turns: [],
    queuedPrompts: [],
    models: [],
    display: {
      reasoning: "summary",
      tool_calls: "summary",
      readable_tool_names: true,
      command_output_lines: 10,
      mixed_model_endpoint_display: "interactive",
      show_mixed_model_endpoint: false
    },
    context: { tokens: 0, window: null },
    usage: {},
    capabilities: {},
    version: null,
    activeRunId: null,
    externalRunningTurnId: null,
    externalQueueAvailable: false,
    live: null,
    eventSource: null,
    connection: "connecting",
    blocked: false,
    adminBusy: false,
    loginSubmitting: false,
    modelSelectionSubmitting: false,
    stagedModelKeys: null,
    modelMenuError: "",
    submitting: false,
    cancellationRequested: false,
    pendingSubmission: null,
    bootstrapPromise: null,
    resyncing: false,
    nearBottom: true,
    followOutput: true,
    programmaticScroll: false,
    settingsOpener: null,
    sidebarOpener: null,
    toastTimer: null,
    healthTimer: null,
    externalSyncTimer: null,
    terminalRunIds: new Set(),
    mode: "normal",
    composing: false,
    settingsView: "interface",
    configLoaded: false,
    configLoading: false,
    configSaving: false,
    configDirty: false,
    configDraft: null,
    configOriginal: null,
    promptDraft: null,
    promptOriginal: null,
    secretStates: {},
    secretChanges: {},
    providerSecretStates: [],
    configMultimodalModels: [],
    invalidConfigFields: new Map()
  };

  class ApiError extends Error {
    constructor(message, status) {
      super(message);
      this.name = "ApiError";
      this.status = status;
    }
  }

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
    const definition = ICONS[name] || ICONS["circle-alert"];
    for (const [tag, attributes] of definition) {
      const node = document.createElementNS(SVG_NS, tag);
      for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, value);
      svg.appendChild(node);
    }
    return svg;
  }

  function renderIconSlots(root = document) {
    const slots = [];
    if (root instanceof Element && root.matches("[data-icon]")) slots.push(root);
    slots.push(...root.querySelectorAll("[data-icon]"));
    for (const slot of slots) {
      slot.replaceChildren(createIcon(slot.dataset.icon));
    }
  }

  function makeIconSlot(name, className = "") {
    const slot = document.createElement("span");
    slot.className = `icon-slot${className ? ` ${className}` : ""}`;
    slot.setAttribute("aria-hidden", "true");
    slot.appendChild(createIcon(name));
    return slot;
  }

  function safeStorageGet(key) {
    try {
      return window.localStorage.getItem(key);
    } catch (_) {
      return null;
    }
  }

  function safeStorageSet(key, value) {
    try {
      window.localStorage.setItem(key, value);
    } catch (_) {
      // Storage can be unavailable in hardened browser profiles.
    }
  }

  function setTheme(theme, persist = true) {
    const selected = theme === "linen" ? "linen" : "graphite";
    elements.body.dataset.theme = selected;
    document.querySelectorAll("[data-theme-choice]").forEach((button) => {
      button.classList.toggle("selected", button.dataset.themeChoice === selected);
      button.setAttribute("aria-pressed", String(button.dataset.themeChoice === selected));
    });
    const nextIcon = selected === "graphite" ? "sun" : "moon";
    for (const button of [elements.themeButton, elements.sidebarThemeButton]) {
      const slot = button.querySelector(".icon-slot");
      slot.replaceChildren(createIcon(nextIcon));
      button.title = selected === "graphite" ? "切换到浅色主题" : "切换到石墨主题";
      button.setAttribute("aria-label", button.title);
    }
    const themeColor = document.querySelector('meta[name="theme-color"]');
    if (themeColor) themeColor.content = selected === "graphite" ? "#111512" : "#f2f5f3";
    if (persist) safeStorageSet("miyu.web.theme", selected);
  }

  function setMode(mode, persist = true) {
    const selected = ["normal", "plan", "chat"].includes(mode) ? mode : "normal";
    state.mode = selected;
    elements.modeSwitch.querySelectorAll("[data-mode]").forEach((button) => {
      const active = button.dataset.mode === selected;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", String(active));
    });
    if (persist) safeStorageSet("miyu.web.mode", selected);
  }

  function closeSidebar() {
    elements.sidebar.classList.remove("open");
    elements.sidebarScrim.classList.remove("visible");
    elements.sidebarScrim.tabIndex = -1;
  }

  function openSidebar(opener = document.activeElement) {
    state.sidebarOpener = opener;
    elements.sidebar.classList.add("open");
    elements.sidebarScrim.classList.add("visible");
    elements.sidebarScrim.tabIndex = 0;
  }

  function getFocusable(container) {
    return Array.from(container.querySelectorAll("button:not(:disabled), input:not(:disabled), textarea:not(:disabled), a[href], [tabindex]:not([tabindex='-1'])"))
      .filter((node) => !node.hidden && node.getClientRects().length > 0);
  }

  function openSettings(opener = document.activeElement) {
    state.settingsOpener = opener;
    closeModelMenu();
    elements.settingsDrawer.classList.add("open");
    elements.settingsDrawer.setAttribute("aria-hidden", "false");
    elements.drawerScrim.classList.add("visible");
    elements.drawerScrim.tabIndex = 0;
    window.requestAnimationFrame(() => elements.settingsClose.focus());
    if (!state.configLoaded && !state.configLoading) loadConfigDraft();
  }

  function closeSettings({ restoreFocus = true } = {}) {
    if (!elements.settingsDrawer.classList.contains("open")) return;
    elements.settingsDrawer.classList.remove("open");
    elements.settingsDrawer.setAttribute("aria-hidden", "true");
    elements.drawerScrim.classList.remove("visible");
    elements.drawerScrim.tabIndex = -1;
    if (restoreFocus && state.settingsOpener instanceof HTMLElement) state.settingsOpener.focus();
    state.settingsOpener = null;
  }

  function openModelMenu() {
    if (elements.modelButton.disabled || state.models.length === 0) return;
    state.stagedModelKeys = new Set(activeModels().map(modelKey));
    state.modelMenuError = "";
    renderModelMenu();
    elements.modelMenu.hidden = false;
    elements.modelButton.setAttribute("aria-expanded", "true");
    const selected = elements.modelMenu.querySelector(".model-menu-item.selected:not(:disabled)");
    const first = elements.modelMenu.querySelector(".model-menu-item:not(:disabled)");
    window.requestAnimationFrame(() => (selected || first)?.focus());
  }

  function closeModelMenu({ restoreFocus = false, discard = true } = {}) {
    if (elements.modelMenu.hidden) return;
    elements.modelMenu.hidden = true;
    elements.modelButton.setAttribute("aria-expanded", "false");
    if (discard) {
      state.stagedModelKeys = null;
      state.modelMenuError = "";
    }
    if (restoreFocus) elements.modelButton.focus();
  }

  function showToast(message, type = "info") {
    const toast = document.createElement("div");
    toast.className = `toast${type === "error" ? " is-error" : ""}`;
    toast.textContent = String(message || "操作未完成");
    elements.toastRegion.replaceChildren(toast);
    if (state.toastTimer) window.clearTimeout(state.toastTimer);
    state.toastTimer = window.setTimeout(() => {
      if (toast.isConnected) toast.remove();
    }, type === "error" ? 6000 : 3000);
  }

  function showInlineError(message) {
    const text = String(message || "操作未完成").trim();
    elements.errorRegion.textContent = text;
    elements.errorRegion.hidden = !text;
  }

  function clearInlineError() {
    elements.errorRegion.textContent = "";
    elements.errorRegion.hidden = true;
  }

  function deepClone(value) {
    if (typeof structuredClone === "function") return structuredClone(value);
    return JSON.parse(JSON.stringify(value));
  }

  function setSettingsView(view) {
    const selected = ["interface", "general", "providers", "models", "plugins", "prompts", "advanced"].includes(view) ? view : "interface";
    state.settingsView = selected;
    elements.settingsNav.querySelectorAll("[data-settings-view]").forEach((button) => {
      const active = button.dataset.settingsView === selected;
      button.classList.toggle("active", active);
      button.setAttribute("aria-current", active ? "page" : "false");
    });
    elements.settingsPanels.forEach((panel) => {
      panel.hidden = panel.dataset.settingsPanel !== selected;
    });
  }

  function configValue(path, fallback = undefined) {
    let value = state.configDraft;
    for (const key of path.split(".")) {
      if (value == null || typeof value !== "object" || !(key in value)) return fallback;
      value = value[key];
    }
    return value;
  }

  function setConfigValue(path, value) {
    if (!state.configDraft) return;
    const keys = path.split(".");
    let target = state.configDraft;
    for (const key of keys.slice(0, -1)) {
      if (!target[key] || typeof target[key] !== "object") target[key] = {};
      target = target[key];
    }
    target[keys[keys.length - 1]] = value;
    markConfigDirty();
  }

  function clearConfigFieldError(input) {
    const message = state.invalidConfigFields.get(input);
    if (message) message.remove();
    state.invalidConfigFields.delete(input);
    input.classList.remove("is-invalid");
  }

  function setConfigFieldError(input, message) {
    clearConfigFieldError(input);
    const error = document.createElement("small");
    error.className = "config-field-error";
    error.textContent = message;
    input.classList.add("is-invalid");
    input.closest(".config-field")?.appendChild(error);
    state.invalidConfigFields.set(input, error);
  }

  function parseConfigInput(input, current) {
    clearConfigFieldError(input);
    if (input.dataset.valueType === "boolean") return input.checked;
    const raw = input.value;
    if (input.dataset.valueType === "number") {
      const number = Number(raw);
      if (!Number.isFinite(number)) throw new Error("请输入有效数字");
      return input.dataset.integer === "true" ? Math.trunc(number) : number;
    }
    if (input.dataset.valueType === "json") {
      if (!raw.trim()) return input.dataset.nullable === "true" ? null : {};
      try {
        return JSON.parse(raw);
      } catch (_) {
        throw new Error("请输入有效 JSON");
      }
    }
    if (input.dataset.valueType === "lines") {
      return raw.split(/\r?\n|,/).map((item) => item.trim()).filter(Boolean);
    }
    return raw;
  }

  function bindConfigInput(input, path, options = {}) {
    input.dataset.configPath = path;
    input.dataset.valueType = options.type || "string";
    if (options.integer) input.dataset.integer = "true";
    if (options.nullable) input.dataset.nullable = "true";
    const eventName = input.tagName === "SELECT" || input.type === "checkbox" ? "change" : "input";
    input.addEventListener(eventName, () => {
      try {
        const value = parseConfigInput(input, configValue(path));
        setConfigValue(path, value);
        updateAdvancedConfigEditor();
        if (options.rerender) renderConfigEditors();
      } catch (error) {
        setConfigFieldError(input, error.message);
        updateSettingsControls();
      }
    });
    return input;
  }

  function configField(labelText, input, description = "") {
    const label = document.createElement("label");
    label.className = "config-field";
    const heading = document.createElement("span");
    heading.className = "config-field-label";
    heading.textContent = labelText;
    label.append(heading, input);
    if (description) {
      const hint = document.createElement("small");
      hint.className = "config-field-hint";
      hint.textContent = description;
      label.appendChild(hint);
    }
    return label;
  }

  function textConfigField(label, path, options = {}) {
    const current = configValue(path, options.defaultValue ?? "");
    const input = options.multiline ? document.createElement("textarea") : document.createElement("input");
    input.className = "config-input";
    if (!options.multiline) input.type = options.inputType || "text";
    if (options.multiline) input.rows = options.rows || 3;
    input.value = options.type === "json"
      ? (current == null ? "" : JSON.stringify(current, null, 2))
      : options.type === "lines"
        ? (Array.isArray(current) ? current.join("\n") : "")
        : String(current ?? "");
    if (options.placeholder) input.placeholder = options.placeholder;
    if (options.min != null) input.min = String(options.min);
    if (options.max != null) input.max = String(options.max);
    if (options.step != null) input.step = String(options.step);
    bindConfigInput(input, path, options);
    return configField(label, input, options.description || "");
  }

  function selectConfigField(label, path, choices, description = "") {
    const select = document.createElement("select");
    select.className = "config-input";
    const current = String(configValue(path, ""));
    for (const choice of choices) {
      const option = document.createElement("option");
      option.value = typeof choice === "string" ? choice : choice.value;
      option.textContent = typeof choice === "string" ? choice : choice.label;
      option.selected = option.value === current;
      select.appendChild(option);
    }
    bindConfigInput(select, path);
    return configField(label, select, description);
  }

  function booleanConfigField(labelText, path, description = "") {
    const label = document.createElement("label");
    label.className = "config-toggle";
    const input = document.createElement("input");
    input.type = "checkbox";
    input.checked = Boolean(configValue(path));
    bindConfigInput(input, path, { type: "boolean" });
    const switchTrack = document.createElement("span");
    switchTrack.className = "toggle-track";
    const copy = document.createElement("span");
    copy.className = "config-toggle-copy";
    const title = document.createElement("strong");
    title.textContent = labelText;
    copy.appendChild(title);
    if (description) {
      const hint = document.createElement("small");
      hint.textContent = description;
      copy.appendChild(hint);
    }
    label.append(input, switchTrack, copy);
    return label;
  }

  function configGroup(titleText, fields = [], description = "") {
    const group = document.createElement("section");
    group.className = "config-group";
    const header = document.createElement("header");
    const title = document.createElement("h3");
    title.textContent = titleText;
    header.appendChild(title);
    if (description) {
      const copy = document.createElement("p");
      copy.textContent = description;
      header.appendChild(copy);
    }
    const body = document.createElement("div");
    body.className = "config-group-body";
    body.append(...fields);
    group.append(header, body);
    return group;
  }

  function actionButton(label, className = "secondary-button") {
    const button = document.createElement("button");
    button.type = "button";
    button.className = className;
    button.textContent = label;
    return button;
  }

  function markConfigDirty() {
    state.configDirty = true;
    updateSettingsControls();
  }

  function clearProviderSecretChanges() {
    for (const key of Object.keys(state.secretChanges)) {
      if (key.startsWith("providers.")) delete state.secretChanges[key];
    }
  }

  function refreshProviderSecretStates() {
    for (const key of Object.keys(state.secretStates)) {
      if (key.startsWith("providers.")) delete state.secretStates[key];
    }
    state.providerSecretStates.forEach((configured, index) => {
      state.secretStates[`providers.${index}.api_key`] = Boolean(configured);
    });
  }

  function updateSettingsControls() {
    const busy = state.configLoading || state.configSaving;
    elements.reloadConfigButton.disabled = busy;
    elements.saveConfigButton.disabled = busy || !state.configLoaded || !state.configDirty || state.invalidConfigFields.size > 0 || conversationRunning();
    elements.addProviderButton.disabled = busy || !state.configLoaded;
    if (state.configLoading) elements.settingsStatus.textContent = "正在载入配置";
    else if (state.configSaving) elements.settingsStatus.textContent = "正在验证并保存";
    else if (!state.configLoaded) elements.settingsStatus.textContent = "尚未载入配置";
    else if (state.invalidConfigFields.size) elements.settingsStatus.textContent = "请修正表单中的错误";
    else if (conversationRunning() && state.configDirty) elements.settingsStatus.textContent = "回复完成后才能保存";
    else elements.settingsStatus.textContent = state.configDirty ? "有未保存的修改" : "配置已同步";
  }

  function updateAdvancedConfigEditor() {
    if (!state.configDraft || document.activeElement === elements.advancedConfigEditor) return;
    elements.advancedConfigEditor.value = JSON.stringify(state.configDraft, null, 2);
  }

  function renderGeneralConfig() {
    elements.generalConfigForm.replaceChildren(
      configGroup("工具", [
        booleanConfigField("启用工具", "tools.enabled"),
        textConfigField("最大工具轮数", "tools.max_rounds", { type: "number", integer: true, inputType: "number", min: 0 }),
        selectConfigField("工具加载模式", "tools.loading_mode", ["full", "hybrid"]),
        booleanConfigField("记住已加载工具", "tools.persist_loaded_tools")
      ]),
      configGroup("Skills", [
        booleanConfigField("启用 Skills", "skills.enabled"),
        booleanConfigField("允许执行命令", "skills.allow_command_execution")
      ]),
      configGroup("显示", [
        selectConfigField("界面语言", "display.language", [{ value: "auto", label: "自动" }, { value: "zh", label: "简体中文" }, { value: "en", label: "English" }]),
        selectConfigField("思考过程", "display.reasoning", [{ value: "summary", label: "摘要" }, { value: "full", label: "完整" }, { value: "hidden", label: "隐藏" }]),
        selectConfigField("工具调用", "display.tool_calls", [{ value: "summary", label: "摘要" }, { value: "full", label: "完整" }, { value: "hidden", label: "隐藏" }]),
        booleanConfigField("显示可读工具名", "display.readable_tool_names"),
        selectConfigField("Mixed 模型端点", "display.mixed_model_endpoint_display", [{ value: "off", label: "关闭" }, { value: "interactive", label: "仅交互模式" }, { value: "all", label: "全部模式" }])
      ]),
      configGroup("上下文", [
        selectConfigField("到达上限后", "context.on_overflow", [{ value: "pop", label: "弹出旧消息" }, { value: "compact", label: "压缩上下文" }]),
        textConfigField("开始裁剪比例", "context.trim_at_ratio", { type: "number", inputType: "number", min: 0.1, max: 1, step: 0.01 }),
        textConfigField("每批裁剪比例", "context.trim_batch_ratio", { type: "number", inputType: "number", min: 0.01, max: 0.9, step: 0.01 })
      ]),
      configGroup("记忆", [
        booleanConfigField("启用记忆", "memory.enabled"),
        booleanConfigField("保留弹出上下文", "memory.evicted_context_enabled"),
        booleanConfigField("启用联想", "memory.association_enabled"),
        booleanConfigField("自动日记", "memory.auto_diary_enabled"),
        booleanConfigField("自动事实记忆", "memory.auto_fact_enabled"),
        textConfigField("联想知识条数", "memory.association_facts", { type: "number", inputType: "number", integer: true, min: 0 }),
        textConfigField("联想事件条数", "memory.association_episodes", { type: "number", inputType: "number", integer: true, min: 0 }),
        textConfigField("联想字符上限", "memory.association_max_chars", { type: "number", inputType: "number", integer: true, min: 0 }),
        textConfigField("片段字符数", "memory.snippet_chars", { type: "number", inputType: "number", integer: true, min: 0 }),
        textConfigField("遗忘期限（天）", "memory.forget_after_days", { type: "number", inputType: "number", integer: true, min: 1 }),
        booleanConfigField("启用遗忘", "memory.forgetting_enabled"),
        textConfigField("遗忘半衰期（天）", "memory.forgetting_half_life_days", { type: "number", inputType: "number", min: 0.1, step: 0.1 }),
        textConfigField("最低遗忘强度", "memory.forgetting_min_strength", { type: "number", inputType: "number", min: 0, max: 1, step: 0.01 }),
        textConfigField("回忆增强强度", "memory.forgetting_review_boost", { type: "number", inputType: "number", min: 0, step: 0.01 }),
        textConfigField("最小任务字数", "memory.learning_min_task_chars", { type: "number", inputType: "number", integer: true, min: 0 }),
        textConfigField("最小方法字数", "memory.learning_min_method_chars", { type: "number", inputType: "number", integer: true, min: 0 })
      ]),
      configGroup("MCP", [
        booleanConfigField("启用 MCP", "mcp.enabled"),
        textConfigField("服务器配置", "mcp.servers", { type: "json", multiline: true, rows: 10, description: "JSON 数组，支持 id、command、args、env、timeout_seconds 和 enabled。" })
      ])
    );
  }

  function secretEditor(labelText, key, { multiline = false } = {}) {
    const wrapper = document.createElement("div");
    wrapper.className = "secret-editor config-field";
    const label = document.createElement("span");
    label.className = "config-field-label";
    label.textContent = labelText;
    const status = document.createElement("small");
    status.className = "secret-status";
    status.textContent = state.secretChanges[key]?.action === "clear"
      ? "将清空"
      : state.secretChanges[key]?.action === "set"
        ? "已输入新值"
        : state.secretStates[key]
          ? "已配置"
          : "未配置";
    const input = multiline ? document.createElement("textarea") : document.createElement("input");
    input.className = "config-input";
    if (!multiline) input.type = "password";
    if (multiline) input.rows = 3;
    input.placeholder = state.secretStates[key] ? "留空保留现有值" : "输入新值";
    input.value = state.secretChanges[key]?.action === "set" ? state.secretChanges[key].value : "";
    input.autocomplete = "new-password";
    const actions = document.createElement("div");
    actions.className = "secret-actions";
    const clear = actionButton("清空", "text-button danger-text");
    const preserve = actionButton("保留", "text-button");
    actions.append(preserve, clear);
    input.addEventListener("input", () => {
      if (input.value) state.secretChanges[key] = { action: "set", value: input.value };
      else delete state.secretChanges[key];
      markConfigDirty();
      status.textContent = input.value ? "已输入新值" : state.secretStates[key] ? "已配置" : "未配置";
    });
    clear.addEventListener("click", () => {
      input.value = "";
      state.secretChanges[key] = { action: "clear" };
      status.textContent = "将清空";
      markConfigDirty();
    });
    preserve.addEventListener("click", () => {
      input.value = "";
      delete state.secretChanges[key];
      status.textContent = state.secretStates[key] ? "已配置" : "未配置";
      markConfigDirty();
    });
    wrapper.append(label, status, input, actions);
    return wrapper;
  }

  function ensureProviderDefaults(provider = {}) {
    return {
      id: "",
      display_name: "",
      base_url: "",
      protocol: "auto",
      api_key: null,
      models: [],
      model_context_window: {},
      model_modalities: {},
      default_model: "",
      timeout_seconds: 60,
      temperature: 0.7,
      anthropic_max_tokens: 4096,
      extra_body: null,
      ...provider
    };
  }

  function replaceProviderReferences(previousId, nextId) {
    if (!previousId || previousId === nextId || !state.configDraft) return;
    if (state.configDraft.active_provider === previousId) state.configDraft.active_provider = nextId;
    for (const poolName of ["active_provider_models", "active_multimodal_provider_models"]) {
      for (const item of state.configDraft[poolName] || []) {
        if (item.provider_id === previousId) item.provider_id = nextId;
      }
    }
    if (state.configDraft.plugins?.vision?.vision_provider_id === previousId) {
      state.configDraft.plugins.vision.vision_provider_id = nextId;
    }
    if (state.configDraft.plugins?.knowledge_base?.embedding_provider_id === previousId) {
      state.configDraft.plugins.knowledge_base.embedding_provider_id = nextId;
    }
  }

  function removeProviderReferences(providerId) {
    if (!state.configDraft) return;
    state.configDraft.active_provider_models = (state.configDraft.active_provider_models || []).filter((item) => item.provider_id !== providerId);
    state.configDraft.active_multimodal_provider_models = (state.configDraft.active_multimodal_provider_models || []).filter((item) => item.provider_id !== providerId);
    if (state.configDraft.plugins?.vision?.vision_provider_id === providerId) {
      state.configDraft.plugins.vision.vision_provider_id = "";
      state.configDraft.plugins.vision.vision_model = "";
    }
    if (state.configDraft.plugins?.knowledge_base?.embedding_provider_id === providerId) {
      state.configDraft.plugins.knowledge_base.embedding_provider_id = "";
      state.configDraft.plugins.knowledge_base.embedding_model = "";
    }
  }

  function renderProviders() {
    elements.providerEditor.replaceChildren();
    const providers = Array.isArray(state.configDraft?.providers) ? state.configDraft.providers : [];
    providers.forEach((provider, index) => {
      const card = document.createElement("details");
      card.className = "provider-card";
      card.open = index === 0;
      const summary = document.createElement("summary");
      const copy = document.createElement("span");
      const name = document.createElement("strong");
      name.textContent = provider.display_name || provider.id || `供应商 ${index + 1}`;
      const id = document.createElement("small");
      id.textContent = provider.id || "尚未命名";
      copy.append(name, id);
      const remove = actionButton("删除", "text-button danger-text");
      remove.addEventListener("click", (event) => {
        event.preventDefault();
        event.stopPropagation();
        if (!window.confirm(`删除供应商“${provider.display_name || provider.id || index + 1}”？`)) return;
        state.configDraft.providers.splice(index, 1);
        state.providerSecretStates.splice(index, 1);
        refreshProviderSecretStates();
        clearProviderSecretChanges();
        removeProviderReferences(provider.id);
        if (state.configDraft.active_provider === provider.id) state.configDraft.active_provider = state.configDraft.providers[0]?.id || "";
        markConfigDirty();
        renderConfigEditors();
      });
      summary.append(copy, remove);
      const body = document.createElement("div");
      body.className = "provider-card-body";
      const fields = [
        ["配置 ID", "id"], ["显示名称", "display_name"], ["Base URL", "base_url"],
        ["默认模型", "default_model"]
      ];
      for (const [label, key] of fields) {
        const input = document.createElement("input");
        input.className = "config-input";
        input.value = String(provider[key] || "");
        input.addEventListener("input", () => {
          const previousId = key === "id" ? String(provider.id || "") : "";
          provider[key] = input.value;
          if (key === "id" && previousId !== provider.id) {
            replaceProviderReferences(previousId, provider.id);
            state.providerSecretStates[index] = false;
            delete state.secretChanges[`providers.${index}.api_key`];
            refreshProviderSecretStates();
            renderModelPools();
          }
          if (key === "default_model") renderModelPools();
          if (key === "display_name" || key === "id") {
            name.textContent = provider.display_name || provider.id || `供应商 ${index + 1}`;
            id.textContent = provider.id || "尚未命名";
          }
          markConfigDirty();
          updateAdvancedConfigEditor();
        });
        if (key === "default_model") {
          input.addEventListener("change", () => {
            provider.models = Array.isArray(provider.models) ? provider.models : [];
            if (provider.default_model && !provider.models.includes(provider.default_model)) {
              provider.models.push(provider.default_model);
            }
            renderModelPools();
            updateAdvancedConfigEditor();
          });
        }
        body.appendChild(configField(label, input));
      }
      const protocol = document.createElement("select");
      protocol.className = "config-input";
      for (const value of ["auto", "openai-chat", "openai-responses", "anthropic"]) {
        const option = document.createElement("option");
        option.value = value;
        option.textContent = value;
        option.selected = provider.protocol === value;
        protocol.appendChild(option);
      }
      protocol.addEventListener("change", () => { provider.protocol = protocol.value; markConfigDirty(); updateAdvancedConfigEditor(); });
      body.appendChild(configField("协议", protocol));
      const secretKey = `providers.${index}.api_key`;
      body.appendChild(secretEditor("API Key", secretKey));

      const numeric = [
        ["超时秒数", "timeout_seconds", 1, 1], ["Temperature", "temperature", 0, 0.1], ["Anthropic 最大 Token", "anthropic_max_tokens", 1, 1]
      ];
      for (const [label, key, min, step] of numeric) {
        const input = document.createElement("input");
        input.className = "config-input";
        input.type = "number";
        input.min = String(min);
        input.step = String(step);
        input.value = String(provider[key] ?? "");
        input.addEventListener("input", () => {
          const value = Number(input.value);
          if (Number.isFinite(value)) {
            provider[key] = key === "temperature" ? value : Math.trunc(value);
            markConfigDirty();
            updateAdvancedConfigEditor();
          }
        });
        body.appendChild(configField(label, input));
      }
      const structured = [
        ["可用模型", "models", "lines", "每行一个模型"],
        ["模型上下文窗口", "model_context_window", "json", "JSON 对象：模型名到 Token 数"],
        ["模型输入模态", "model_modalities", "json", "JSON 对象：模型名到 text/image/audio/video/pdf 数组"],
        ["额外请求体", "extra_body", "json", "JSON 对象，留空表示不设置"]
      ];
      for (const [label, key, type, description] of structured) {
        const input = document.createElement("textarea");
        input.className = "config-input";
        input.rows = key === "models" ? 4 : 5;
        input.value = type === "lines" ? (provider[key] || []).join("\n") : provider[key] == null ? "" : JSON.stringify(provider[key], null, 2);
        input.addEventListener("input", () => {
          clearConfigFieldError(input);
          try {
            provider[key] = type === "lines"
              ? input.value.split(/\r?\n|,/).map((item) => item.trim()).filter(Boolean)
              : input.value.trim() ? JSON.parse(input.value) : key === "extra_body" ? null : {};
            if (key === "models" && provider.default_model && !provider.models.includes(provider.default_model)) {
              provider.models.push(provider.default_model);
            }
            markConfigDirty();
            updateAdvancedConfigEditor();
            if (key === "models" || key === "model_modalities") renderModelPools();
          } catch (_) {
            setConfigFieldError(input, "请输入有效 JSON");
            updateSettingsControls();
          }
        });
        body.appendChild(configField(label, input, description));
      }
      card.append(summary, body);
      elements.providerEditor.appendChild(card);
    });
    if (!providers.length) {
      const empty = document.createElement("p");
      empty.className = "settings-empty";
      empty.textContent = "至少需要添加一个供应商。";
      elements.providerEditor.appendChild(empty);
    }
  }

  function configuredModelChoices() {
    const result = [];
    for (const provider of state.configDraft?.providers || []) {
      const models = Array.isArray(provider.models) && provider.models.length ? provider.models : provider.default_model ? [provider.default_model] : [];
      for (const model of models) {
        if (String(model).trim()) result.push({ provider_id: String(provider.id || ""), provider_name: String(provider.display_name || provider.id || ""), model: String(model) });
      }
    }
    return result;
  }

  function renderModelPoolList(titleText, path, choices) {
    const providers = Array.isArray(state.configDraft?.providers) ? state.configDraft.providers : [];
    const selected = Array.isArray(state.configDraft[path])
      ? state.configDraft[path]
      : path === "active_provider_models"
        ? choices.filter((choice) => choice.provider_id === state.configDraft.active_provider && choice.model === providers.find((provider) => provider.id === state.configDraft.active_provider)?.default_model)
        : [];
    const group = configGroup(titleText);
    const body = group.querySelector(".config-group-body");
    if (!choices.length) {
      const empty = document.createElement("p");
      empty.className = "settings-empty";
      empty.textContent = "请先在供应商中配置模型。";
      body.appendChild(empty);
    }
    for (const model of choices) {
      const label = document.createElement("label");
      label.className = "model-pool-option";
      const input = document.createElement("input");
      input.type = "checkbox";
      input.checked = selected.some((item) => item.provider_id === model.provider_id && item.model === model.model);
      input.addEventListener("change", () => {
        let pool = Array.isArray(state.configDraft[path]) ? state.configDraft[path] : [...selected];
        if (input.checked && !pool.some((item) => item.provider_id === model.provider_id && item.model === model.model)) {
          pool = [...pool, { provider_id: model.provider_id, model: model.model }];
        } else if (!input.checked) {
          pool = pool.filter((item) => item.provider_id !== model.provider_id || item.model !== model.model);
        }
        state.configDraft[path] = pool;
        markConfigDirty();
        updateAdvancedConfigEditor();
      });
      const copy = document.createElement("span");
      const name = document.createElement("strong");
      name.textContent = model.model;
      const provider = document.createElement("small");
      provider.textContent = model.provider_name;
      copy.append(name, provider);
      label.append(input, copy);
      body.appendChild(label);
    }
    return group;
  }

  function renderModelPools() {
    const providers = Array.isArray(state.configDraft?.providers) ? state.configDraft.providers : [];
    const choices = configuredModelChoices();
    const declaredMultimodal = choices.filter((choice) => {
      const provider = providers.find((item) => item.id === choice.provider_id);
      const modalities = provider?.model_modalities?.[choice.model];
      return Array.isArray(modalities) && modalities.some((item) => ["image", "audio", "video", "pdf"].includes(item));
    });
    const multimodalKeys = new Set([
      ...state.configMultimodalModels.map((model) => modelKey(model)),
      ...declaredMultimodal.map((model) => modelKey(model))
    ]);
    const multimodal = choices.filter((choice) => multimodalKeys.has(modelKey(choice)));
    const activeProvider = document.createElement("select");
    activeProvider.className = "config-input";
    for (const provider of providers) {
      const option = document.createElement("option");
      option.value = provider.id || "";
      option.textContent = provider.display_name || provider.id || "未命名供应商";
      option.selected = option.value === state.configDraft.active_provider;
      activeProvider.appendChild(option);
    }
    activeProvider.addEventListener("change", () => {
      state.configDraft.active_provider = activeProvider.value;
      markConfigDirty();
      updateAdvancedConfigEditor();
      renderModelPools();
    });
    elements.modelPoolEditor.replaceChildren(
      configGroup("默认供应商", [configField("未设置文本模型池时使用", activeProvider)]),
      renderModelPoolList("文本模型池", "active_provider_models", choices),
      renderModelPoolList("多模态模型池", "active_multimodal_provider_models", multimodal)
    );
  }

  const PLUGIN_LABELS = {
    weather: "天气", web: "网络搜索", web_images: "图片搜索", deep_research: "深度研究", deep_diagnose: "深度诊断",
    vision: "识图", exchange_rate: "汇率", xuanxue: "玄学", image_generation: "生图", print_image: "打印图片",
    memes: "表情包", knowledge_base: "知识库", archlinux: "Arch Linux", man: "在线手册", moegirl: "萌娘百科",
    hash_codec: "哈希与编解码", calculator: "计算器", package_advisor: "AUR 审查",
    deep_research_linux_game_compatibility: "Linux 游戏兼容", diagnostics: "系统诊断", memory: "记忆"
  };

  const SECRET_PLUGIN_PATHS = new Map([
    ["web.tavily_api_keys", "plugins.web.tavily_api_keys"],
    ["web.firecrawl_api_keys", "plugins.web.firecrawl_api_keys"],
    ["web.anysearch_api_keys", "plugins.web.anysearch_api_keys"],
    ["exchange_rate.api_key", "plugins.exchange_rate.api_key"],
    ["image_generation.api_keys", "plugins.image_generation.api_keys"]
  ]);

  const WEB_HIDDEN_PLUGIN_FIELDS = new Set([
    "vision.preview_with_chafa",
    "image_generation.auto_print",
    "print_image.width_percent",
    "print_image.height_percent",
    "memes.width_percent",
    "memes.height_percent",
    "web_images.auto_preview",
    "web_images.preview_count"
  ]);

  function humanizeConfigKey(key) {
    return String(key).replace(/_/g, " ").replace(/\b\w/g, (character) => character.toUpperCase());
  }

  function pluginValueEditor(pluginKey, fieldKey, value) {
    const path = `plugins.${pluginKey}.${fieldKey}`;
    const secretKey = SECRET_PLUGIN_PATHS.get(`${pluginKey}.${fieldKey}`);
    if (secretKey) return secretEditor(humanizeConfigKey(fieldKey), secretKey, { multiline: Array.isArray(value) });
    if (typeof value === "boolean") return booleanConfigField(humanizeConfigKey(fieldKey), path);
    if (typeof value === "number") return textConfigField(humanizeConfigKey(fieldKey), path, { type: "number", integer: Number.isInteger(value), inputType: "number", step: Number.isInteger(value) ? 1 : 0.01 });
    if (typeof value === "string") return textConfigField(humanizeConfigKey(fieldKey), path, { multiline: value.length > 100, rows: 3 });
    return textConfigField(humanizeConfigKey(fieldKey), path, { type: "json", multiline: true, rows: 5 });
  }

  function renderPlugins() {
    elements.pluginEditor.replaceChildren();
    for (const [pluginKey, plugin] of Object.entries(state.configDraft?.plugins || {})) {
      if (pluginKey === "memory" || pluginKey === "print_image") continue;
      const details = document.createElement("details");
      details.className = "plugin-card";
      const summary = document.createElement("summary");
      const copy = document.createElement("span");
      const title = document.createElement("strong");
      title.textContent = PLUGIN_LABELS[pluginKey] || humanizeConfigKey(pluginKey);
      const technical = document.createElement("small");
      technical.textContent = pluginKey;
      copy.append(title, technical);
      const badge = document.createElement("span");
      badge.className = `plugin-state${plugin?.enabled ? " is-enabled" : ""}`;
      badge.textContent = plugin?.enabled ? "启用" : "禁用";
      summary.append(copy, badge);
      const body = document.createElement("div");
      body.className = "plugin-card-body";
      for (const [fieldKey, value] of Object.entries(plugin || {})) {
        if (WEB_HIDDEN_PLUGIN_FIELDS.has(`${pluginKey}.${fieldKey}`)) continue;
        body.appendChild(pluginValueEditor(pluginKey, fieldKey, value));
      }
      details.append(summary, body);
      elements.pluginEditor.appendChild(details);
    }
  }

  function normalizedDocumentName(name) {
    const trimmed = String(name || "").trim().replace(/[\\/]/g, "-").replace(/\.md$/i, "");
    return trimmed ? `${trimmed}.md` : "";
  }

  function renderPromptCollection(kind, titleText, activePath) {
    const documents = state.promptDraft[kind];
    const group = configGroup(titleText);
    const body = group.querySelector(".config-group-body");
    const active = document.createElement("select");
    active.className = "config-input";
    const defaultOption = document.createElement("option");
    defaultOption.value = "";
    defaultOption.textContent = kind === "personas" ? "Miyu 默认人格" : "不使用用户身份";
    active.appendChild(defaultOption);
    for (const promptDocument of documents) {
      const option = document.createElement("option");
      option.value = promptDocument.name;
      option.textContent = promptDocument.name.replace(/\.md$/i, "");
      active.appendChild(option);
    }
    active.value = String(configValue(activePath, ""));
    active.addEventListener("change", () => { setConfigValue(activePath, active.value); updateAdvancedConfigEditor(); });
    body.appendChild(configField("当前使用", active));
    for (const [index, promptDocument] of documents.entries()) {
      const card = document.createElement("section");
      card.className = "prompt-document";
      const header = document.createElement("header");
      const name = document.createElement("input");
      name.className = "config-input";
      name.value = promptDocument.name.replace(/\.md$/i, "");
      name.setAttribute("aria-label", `${titleText}名称`);
      const remove = actionButton("删除", "text-button danger-text");
      remove.addEventListener("click", () => {
        const wasActive = configValue(activePath, "") === promptDocument.name;
        documents.splice(index, 1);
        if (wasActive) setConfigValue(activePath, "");
        markConfigDirty();
        renderPromptEditor();
        updateAdvancedConfigEditor();
      });
      header.append(name, remove);
      const content = document.createElement("textarea");
      content.className = "config-input prompt-content";
      content.rows = 10;
      content.value = promptDocument.content;
      content.setAttribute("aria-label", `${titleText}内容`);
      name.addEventListener("input", () => {
        const previous = promptDocument.name;
        promptDocument.name = normalizedDocumentName(name.value);
        if (configValue(activePath, "") === previous) setConfigValue(activePath, promptDocument.name);
        markConfigDirty();
        updateAdvancedConfigEditor();
      });
      content.addEventListener("input", () => { promptDocument.content = content.value; markConfigDirty(); });
      card.append(header, content);
      body.appendChild(card);
    }
    const add = actionButton("添加", "secondary-button compact-button");
    add.addEventListener("click", () => {
      const base = kind === "personas" ? "new-persona" : "new-identity";
      let name = `${base}.md`;
      let suffix = 2;
      while (documents.some((document) => document.name === name)) name = `${base}-${suffix++}.md`;
      documents.push({ name, content: "", original_name: null });
      markConfigDirty();
      renderPromptEditor();
    });
    body.appendChild(add);
    return group;
  }

  function renderPromptEditor() {
    elements.promptEditor.replaceChildren(
      renderPromptCollection("personas", "AI 人格", "prompt.active_persona"),
      renderPromptCollection("identities", "用户身份", "prompt.active_identity")
    );
  }

  function renderConfigEditors() {
    if (!state.configLoaded || !state.configDraft) return;
    state.invalidConfigFields.clear();
    renderGeneralConfig();
    renderProviders();
    renderModelPools();
    renderPlugins();
    renderPromptEditor();
    updateAdvancedConfigEditor();
    updateSettingsControls();
  }

  function mapServerSecretStates(payload) {
    const providers = state.configDraft?.providers || [];
    state.providerSecretStates = providers.map((_, index) => Boolean(payload[`providers.${index}.api_key`]));
    const states = { ...payload };
    state.secretStates = states;
    refreshProviderSecretStates();
    return states;
  }

  function applyConfigPayload(payload) {
    state.configDraft = deepClone(payload?.config || {});
    state.configOriginal = deepClone(payload?.config || {});
    state.promptDraft = deepClone(payload?.prompts || { personas: [], identities: [] });
    state.promptOriginal = deepClone(payload?.prompts || { personas: [], identities: [] });
    state.secretChanges = {};
    mapServerSecretStates(payload?.secret_states || {});
    state.configDirty = false;
    state.configLoaded = true;
    state.invalidConfigFields.clear();
    if (Array.isArray(payload?.models)) state.models = payload.models;
    state.configMultimodalModels = Array.isArray(payload?.multimodal_models) ? payload.multimodal_models : [];
    if (payload?.display && typeof payload.display === "object") state.display = payload.display;
    if (payload?.context && typeof payload.context === "object") state.context = payload.context;
    renderConfigEditors();
    renderModelMenu();
    updateContext();
  }

  async function loadConfigDraft() {
    if (state.configLoading || state.configSaving) return;
    if (state.configDirty && !window.confirm("放弃尚未保存的配置修改并重新载入？")) return;
    state.configLoading = true;
    updateSettingsControls();
    try {
      const response = await apiRequest("/api/config");
      applyConfigPayload(await response.json());
    } catch (error) {
      showToast(error.message || "配置载入失败", "error");
      elements.settingsStatus.textContent = error.message || "配置载入失败";
    } finally {
      state.configLoading = false;
      updateSettingsControls();
    }
  }

  function promptStateChanged() {
    if (!state.configOriginal || !state.promptOriginal) return false;
    const promptKeys = ["prompt", "system_prompt_file", "system_prompt"];
    const current = Object.fromEntries(promptKeys.map((key) => [key, state.configDraft?.[key]]));
    const original = Object.fromEntries(promptKeys.map((key) => [key, state.configOriginal?.[key]]));
    return JSON.stringify(current) !== JSON.stringify(original) || JSON.stringify(state.promptDraft) !== JSON.stringify(state.promptOriginal);
  }

  function buildSecretMutations() {
    return { ...state.secretChanges };
  }

  async function saveConfigDraft() {
    if (!state.configLoaded || state.configSaving || state.configLoading || conversationRunning() || state.invalidConfigFields.size) return;
    const resetsConversation = promptStateChanged();
    if (resetsConversation && !window.confirm("人格、身份或提示词文件已修改。保存后会清空当前会话并重建 Agent，继续吗？")) return;
    state.configSaving = true;
    state.adminBusy = true;
    updateSettingsControls();
    updateControlState();
    try {
      const response = await apiRequest("/api/config", {
        method: "PUT",
        body: JSON.stringify({
          config: state.configDraft,
          secrets: buildSecretMutations(),
          prompts: state.promptDraft,
          reset_conversation: resetsConversation
        })
      });
      applyConfigPayload(await response.json());
      if (resetsConversation) await loadBootstrap();
      showToast("配置已保存");
    } catch (error) {
      showToast(error.message || "配置保存失败", "error");
      elements.settingsStatus.textContent = error.message || "配置保存失败";
    } finally {
      state.configSaving = false;
      state.adminBusy = false;
      updateSettingsControls();
      updateControlState();
    }
  }

  function applyAdvancedConfig() {
    try {
      const parsed = JSON.parse(elements.advancedConfigEditor.value);
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("配置必须是 JSON 对象");
      const oldSecretStates = new Map((state.configDraft?.providers || []).map((provider, index) => [String(provider?.id || ""), Boolean(state.providerSecretStates[index])]));
      state.configDraft = parsed;
      state.providerSecretStates = (Array.isArray(parsed.providers) ? parsed.providers : []).map((provider) => oldSecretStates.get(String(provider?.id || "")) || false);
      refreshProviderSecretStates();
      clearProviderSecretChanges();
      markConfigDirty();
      renderConfigEditors();
      showToast("完整配置已应用到草稿");
    } catch (error) {
      showToast(error.message || "JSON 无效", "error");
    }
  }

  async function readErrorMessage(response) {
    try {
      const payload = await response.json();
      const message = payload?.error?.message;
      if (typeof message === "string" && message.trim()) return message.trim();
    } catch (_) {
      // Fall through to an HTTP status message.
    }
    return `请求失败 (${response.status})`;
  }

  async function apiRequest(path, options = {}) {
    const headers = new Headers(options.headers || {});
    headers.set("Accept", "application/json");
    if (options.body != null && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
    let response;
    try {
      response = await fetch(path, { ...options, headers, credentials: "same-origin" });
    } catch (_) {
      throw new ApiError("无法连接 Miyu WebUI", 0);
    }
    if (!response.ok) throw new ApiError(await readErrorMessage(response), response.status);
    return response;
  }

  function asFiniteNumber(value, fallback = 0) {
    const number = Number(value);
    return Number.isFinite(number) ? number : fallback;
  }

  function formatInteger(value) {
    const number = Math.max(0, asFiniteNumber(value));
    try {
      return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 0 }).format(number);
    } catch (_) {
      return String(Math.round(number));
    }
  }

  function formatTokens(value) {
    const number = Math.max(0, asFiniteNumber(value));
    if (number < 1000) return formatInteger(number);
    const useMillions = number >= 1_000_000;
    const amount = number / (useMillions ? 1_000_000 : 1000);
    const digits = amount >= 100 ? 0 : amount >= 10 ? 1 : 1;
    const suffix = useMillions ? "M" : "k";
    try {
      return `${new Intl.NumberFormat("zh-CN", { maximumFractionDigits: digits }).format(amount)}${suffix}`;
    } catch (_) {
      return `${amount.toFixed(digits)}${suffix}`;
    }
  }

  function parseDate(value) {
    if (value == null || value === "") return null;
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? null : date;
  }

  function formatTime(value) {
    const date = parseDate(value);
    if (!date) return "";
    try {
      return new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false }).format(date);
    } catch (_) {
      return date.toLocaleTimeString?.() || "";
    }
  }

  function formatDateTime(value) {
    const date = parseDate(value);
    if (!date) return "";
    try {
      return new Intl.DateTimeFormat("zh-CN", {
        year: "numeric",
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
        hour12: false
      }).format(date);
    } catch (_) {
      return date.toLocaleString?.() || "";
    }
  }

  function formatRelativeTime(value) {
    const date = parseDate(value);
    if (!date) return "";
    const difference = Date.now() - date.getTime();
    if (difference >= 0 && difference < 60_000) return "刚刚";
    if (difference >= 0 && difference < 3_600_000) return `${Math.max(1, Math.floor(difference / 60_000))} 分钟前`;
    const now = new Date();
    if (date.toDateString() === now.toDateString()) return formatTime(date);
    try {
      return new Intl.DateTimeFormat("zh-CN", { month: "numeric", day: "numeric" }).format(date);
    } catch (_) {
      return date.toLocaleDateString?.() || "";
    }
  }

  function dayKey(value) {
    const date = parseDate(value);
    if (!date) return "unknown";
    return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
  }

  function formatDayLabel(value) {
    const date = parseDate(value);
    if (!date) return "较早";
    const today = new Date();
    const yesterday = new Date(today);
    yesterday.setDate(today.getDate() - 1);
    if (date.toDateString() === today.toDateString()) return "今天";
    if (date.toDateString() === yesterday.toDateString()) return "昨天";
    try {
      return new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "long", day: "numeric" }).format(date);
    } catch (_) {
      return date.toLocaleDateString?.() || "较早";
    }
  }

  function firstLine(value) {
    return String(value || "").split(/\r?\n/, 1)[0].trim();
  }

  function modelMark(model) {
    const source = String(model?.provider_name || model?.provider_id || model?.model || "").trim();
    if (!source) return "--";
    const words = source.split(/[\s._/-]+/).filter(Boolean);
    const mark = words.length > 1 ? `${words[0][0] || ""}${words[1][0] || ""}` : source.slice(0, 2);
    return mark.toLocaleUpperCase("en-US");
  }

  function modelKey(model) {
    return JSON.stringify([String(model?.provider_id || ""), String(model?.model || "")]);
  }

  function effectiveUsageTotal(usage) {
    if (!usage || typeof usage !== "object") return 0;
    const explicit = asFiniteNumber(usage.total_tokens, 0);
    return explicit > 0 ? explicit : asFiniteNumber(usage.prompt_tokens, 0) + asFiniteNumber(usage.completion_tokens, 0);
  }

  function setConnectionStatus(status) {
    state.connection = status;
    const definitions = {
      online: { sidebar: "在线", className: "" },
      connecting: { sidebar: "重连中", className: "is-connecting" },
      offline: { sidebar: "离线", className: "is-offline" },
      blocked: { sidebar: "未授权", className: "is-blocked" }
    };
    const selected = definitions[status] || definitions.connecting;
    elements.sidebarConnectionStatus.textContent = selected.sidebar;
    elements.sidebarStatusDot.classList.remove("is-connecting", "is-offline", "is-blocked");
    if (selected.className) elements.sidebarStatusDot.classList.add(selected.className);
  }

  function updateContext() {
    const tokens = Math.max(0, asFiniteNumber(state.context?.tokens));
    const windowSize = state.context?.window == null ? null : Math.max(0, asFiniteNumber(state.context.window));
    elements.contextNumbers.textContent = windowSize ? `${formatTokens(tokens)} / ${formatTokens(windowSize)}` : `${formatTokens(tokens)} / --`;
    const percent = windowSize > 0 ? Math.min(100, Math.max(0, (tokens / windowSize) * 100)) : 0;
    elements.contextBar.style.width = `${percent}%`;
    elements.contextTrack.setAttribute("aria-valuenow", String(Math.round(percent)));
    elements.contextTrack.setAttribute("aria-label", windowSize ? `上下文使用 ${Math.round(percent)}%` : `上下文 ${formatInteger(tokens)} tokens`);
    elements.contextTrack.classList.toggle("is-high", percent >= 75 && percent < 90);
    elements.contextTrack.classList.toggle("is-critical", percent >= 90);
  }

  function updateRuntimeUsage() {}

  function updateCapabilities() {
    const values = [
      ["会话", state.capabilities?.multi_conversation ? "多会话" : "当前单一对话"],
      ["附件", state.capabilities?.attachments ? "可用" : "不可用"],
      ["消息队列", state.capabilities?.queue ? "可用" : "不可用"]
    ];
    elements.capabilityList.replaceChildren();
    for (const [name, value] of values) {
      const row = document.createElement("div");
      const term = document.createElement("dt");
      const description = document.createElement("dd");
      term.textContent = name;
      description.textContent = value;
      row.append(term, description);
      elements.capabilityList.appendChild(row);
    }
  }

  function activeModels() {
    return state.models.filter((model) => model?.active);
  }

  function updateCurrentModelDisplay() {
    const active = activeModels();
    if (active.length === 0) {
      elements.modelMark.textContent = "--";
      elements.modelLabel.textContent = state.models.length ? "未选择模型" : "未配置模型";
      elements.modelLabel.title = elements.modelLabel.textContent;
      elements.settingsModelMark.textContent = "--";
      elements.settingsModelName.textContent = elements.modelLabel.textContent;
      elements.settingsModelProvider.textContent = "--";
      return;
    }
    if (active.length > 1) {
      const title = active.map((model) => `${model.provider_name || model.provider_id || ""} · ${model.model || ""}`).join("\n");
      elements.modelMark.textContent = "MX";
      elements.modelLabel.textContent = `混合模型 · ${active.length}`;
      elements.modelLabel.title = title;
      elements.settingsModelMark.textContent = "MX";
      elements.settingsModelName.textContent = "混合模型";
      elements.settingsModelProvider.textContent = `${active.length} 个活动端点`;
      return;
    }
    const selected = active[0];
    const mark = modelMark(selected);
    elements.modelMark.textContent = mark;
    elements.modelLabel.textContent = String(selected.model || "");
    elements.modelLabel.title = `${selected.provider_name || selected.provider_id || ""} · ${selected.model || ""}`;
    elements.settingsModelMark.textContent = mark;
    elements.settingsModelName.textContent = String(selected.model || "");
    elements.settingsModelProvider.textContent = String(selected.provider_name || selected.provider_id || "");
  }

  function refreshLiveEndpointVisibility() {
    if (!state.live?.endpoint) return;
    const values = [state.live.providerId, state.live.model].map((value) => String(value || "").trim()).filter(Boolean);
    state.live.endpoint.hidden = !state.display?.show_mixed_model_endpoint || values.length === 0;
  }

  function renderModelMenu() {
    elements.modelMenu.replaceChildren();
    const staged = state.stagedModelKeys instanceof Set
      ? state.stagedModelKeys
      : new Set(activeModels().map(modelKey));
    const list = document.createElement("div");
    list.className = "model-menu-list";
    list.setAttribute("role", "group");
    list.setAttribute("aria-label", "可用模型");
    for (const model of state.models) {
      if (!model || typeof model !== "object") continue;
      const button = document.createElement("button");
      button.type = "button";
      button.className = "model-menu-item";
      button.setAttribute("role", "menuitemcheckbox");
      button.dataset.modelKey = modelKey(model);
      const selected = staged.has(button.dataset.modelKey);
      button.setAttribute("aria-checked", String(selected));
      button.classList.toggle("selected", selected);

      const mark = document.createElement("span");
      mark.className = "model-mark";
      mark.textContent = modelMark(model);
      const copy = document.createElement("span");
      copy.className = "model-menu-copy";
      const name = document.createElement("strong");
      name.textContent = String(model.model || "");
      const provider = document.createElement("small");
      provider.textContent = String(model.provider_name || model.provider_id || "");
      copy.append(name, provider);
      const check = document.createElement("span");
      check.className = "icon-slot check-slot";
      check.setAttribute("aria-hidden", "true");
      if (selected) check.appendChild(createIcon("check"));
      button.append(mark, copy, check);
      button.addEventListener("click", () => toggleStagedModel(button.dataset.modelKey));
      list.appendChild(button);
    }

    const footer = document.createElement("footer");
    footer.className = "model-menu-footer";
    footer.setAttribute("role", "none");
    const feedback = document.createElement("span");
    feedback.className = "model-menu-feedback";
    feedback.setAttribute("role", "status");
    feedback.setAttribute("aria-live", "polite");
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "model-cancel";
    cancel.setAttribute("role", "menuitem");
    cancel.textContent = "取消";
    cancel.addEventListener("click", () => closeModelMenu({ restoreFocus: true }));
    const confirm = document.createElement("button");
    confirm.type = "button";
    confirm.className = "model-confirm";
    confirm.setAttribute("role", "menuitem");
    confirm.textContent = "确认";
    confirm.addEventListener("click", confirmModelSelection);
    footer.append(feedback, cancel, confirm);
    elements.modelMenu.append(list, footer);
    updateModelMenuState();
    updateCurrentModelDisplay();
    refreshLiveEndpointVisibility();
    updateControlState();
  }

  function updateModelMenuState() {
    const staged = state.stagedModelKeys instanceof Set
      ? state.stagedModelKeys
      : new Set(activeModels().map(modelKey));
    elements.modelMenu.querySelectorAll(".model-menu-item").forEach((button) => {
      const selected = staged.has(button.dataset.modelKey || "");
      button.classList.toggle("selected", selected);
      button.setAttribute("aria-checked", String(selected));
      const check = button.querySelector(".check-slot");
      if (check) check.replaceChildren(...(selected ? [createIcon("check")] : []));
    });
    const feedback = elements.modelMenu.querySelector(".model-menu-feedback");
    if (feedback) {
      const empty = staged.size === 0;
      feedback.textContent = state.modelMenuError || (empty ? "至少选择一个模型" : `已选择 ${formatInteger(staged.size)} 个模型`);
      feedback.classList.toggle("is-error", Boolean(state.modelMenuError) || empty);
    }
    const confirm = elements.modelMenu.querySelector(".model-confirm");
    if (confirm) {
      confirm.textContent = state.modelSelectionSubmitting ? "正在应用" : "确认";
      confirm.disabled = state.modelSelectionSubmitting || state.adminBusy || state.blocked || conversationRunning() || state.submitting || staged.size === 0;
    }
    const cancel = elements.modelMenu.querySelector(".model-cancel");
    if (cancel) cancel.disabled = state.modelSelectionSubmitting || (state.adminBusy && !state.modelSelectionSubmitting);
  }

  function toggleStagedModel(key) {
    if (!(state.stagedModelKeys instanceof Set) || state.modelSelectionSubmitting) return;
    if (state.stagedModelKeys.has(key)) state.stagedModelKeys.delete(key);
    else state.stagedModelKeys.add(key);
    state.modelMenuError = state.stagedModelKeys.size === 0 ? "至少选择一个模型" : "";
    updateModelMenuState();
  }

  function deriveConversationDetails() {
    if (state.turns.length === 0) {
      const liveUser = state.live?.userText || state.pendingSubmission?.content || "";
      if (!liveUser) return { title: "新对话", snippet: "尚未开始", timestamp: null };
      return { title: firstLine(liveUser) || "新对话", snippet: firstLine(liveUser), timestamp: new Date() };
    }
    const firstTurn = state.turns[0];
    const lastTurn = state.turns[state.turns.length - 1];
    const followups = Array.isArray(lastTurn?.followups) ? lastTurn.followups : [];
    const lastFollowup = followups[followups.length - 1];
    const assistant = String(lastTurn?.assistant_content || "").trim();
    const liveContent = state.activeRunId ? String(state.live?.userText || "").trim() : "";
    const snippet = firstLine(liveContent || assistant || lastFollowup?.content || lastTurn?.user_content || "");
    const timestamp = liveContent ? state.live?.startedAt : lastTurn?.assistant_timestamp || lastFollowup?.submitted_at || lastTurn?.user_timestamp;
    return {
      title: firstLine(firstTurn?.user_content) || "当前对话",
      snippet: snippet || (lastTurn?.status === "running" ? "正在回复" : "对话已开始"),
      timestamp
    };
  }

  function updateConversationChrome() {
    const details = deriveConversationDetails();
    elements.conversationTitle.textContent = details.title;
    elements.conversationTitle.title = details.title;
    elements.sidebarConversationTitle.textContent = details.title;
    elements.sidebarConversationTitle.title = details.title;
    elements.sidebarConversationSnippet.textContent = details.snippet;
    elements.sidebarConversationSnippet.title = details.snippet;
    elements.sidebarConversationTime.textContent = details.timestamp ? formatRelativeTime(details.timestamp) : "";
    if (conversationRunning()) elements.conversationMeta.textContent = state.cancellationRequested ? "正在停止" : "正在回复";
    else elements.conversationMeta.textContent = details.timestamp ? formatRelativeTime(details.timestamp) : "尚未开始";
  }

  function conversationRunning() {
    return Boolean(state.activeRunId || state.externalRunningTurnId);
  }

  function hasPendingQuestion() {
    if (!state.live) return false;
    return Array.from(state.live.questions.values()).some((question) => question.pending);
  }

  function countCharacters(value) {
    return Array.from(String(value || "")).length;
  }

  function resizeComposer() {
    const input = elements.composerInput;
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, window.innerWidth <= 760 ? 120 : 146)}px`;
    const count = countCharacters(input.value);
    elements.characterCount.textContent = `${formatInteger(count)} / 20,000`;
    elements.characterCount.hidden = count < 18_000;
    elements.characterCount.classList.toggle("is-error", count > MAX_CONTENT_CHARS);
    updateControlState();
    window.requestAnimationFrame(updateJumpButtonOffset);
  }

  function updateJumpButtonOffset() {
    elements.jumpBottomButton.style.bottom = `${elements.composerDock.offsetHeight + 10}px`;
  }

  function updateControlState() {
    const running = conversationRunning();
    const cancellable = Boolean(state.activeRunId);
    const queueAvailable = !state.externalRunningTurnId || state.externalQueueAvailable;
    const busy = state.adminBusy || state.submitting;
    const locked = state.blocked || state.adminBusy;
    const inputCount = countCharacters(elements.composerInput.value.trim());

    elements.composerInput.disabled = locked;
    elements.composerForm.classList.toggle("is-disabled", locked);
    elements.newChatButton.disabled = state.blocked || running || busy;
    elements.modelButton.disabled = state.blocked || running || busy || state.models.length === 0;
    elements.modeSwitch.querySelectorAll("button").forEach((button) => {
      button.disabled = state.blocked || running || busy;
    });
    elements.promptGrid.querySelectorAll("button").forEach((button) => {
      button.disabled = state.blocked || running || busy;
    });
    elements.modelMenu.querySelectorAll(".model-menu-item").forEach((button) => {
      button.disabled = state.blocked || running || busy;
    });
    updateModelMenuState();

    elements.sendButton.classList.remove("is-cancel");
    elements.sendButton.querySelector(".icon-slot").replaceChildren(createIcon("arrow-up"));
    elements.sendButton.title = running ? "加入队列" : "发送消息";
    elements.sendButton.setAttribute("aria-label", elements.sendButton.title);
    elements.sendButton.disabled = state.blocked || state.adminBusy || state.submitting || hasPendingQuestion() || (running && !queueAvailable) || inputCount === 0 || inputCount > MAX_CONTENT_CHARS;
    elements.stopButton.hidden = !cancellable;
    elements.stopButton.disabled = !cancellable || state.cancellationRequested || state.adminBusy;
    elements.stopButton.title = state.cancellationRequested ? "正在停止" : "停止回复";
    elements.stopButton.setAttribute("aria-label", elements.stopButton.title);

    if (state.blocked) elements.composerState.textContent = "未授权";
    else if (state.cancellationRequested) elements.composerState.textContent = "正在停止";
    else if (hasPendingQuestion()) elements.composerState.textContent = "等待回答";
    else if (busy) elements.composerState.textContent = state.submitting ? (running ? "正在加入队列" : "正在发送") : "正在处理";
    else if (inputCount > MAX_CONTENT_CHARS) elements.composerState.textContent = "消息不能超过 20,000 个字符";
    else if (running && !queueAvailable) elements.composerState.textContent = "另一会话正在运行";
    else if (running) elements.composerState.textContent = state.queuedPrompts.length
      ? `Miyu 正在回复 · ${state.queuedPrompts.length} 条排队`
      : "Miyu 正在回复";
    else elements.composerState.textContent = "";
    elements.composerState.classList.toggle("is-error", inputCount > MAX_CONTENT_CHARS);
    updateSettingsControls();
  }

  function isNearBottom() {
    const distance = elements.chatScroll.scrollHeight - elements.chatScroll.scrollTop - elements.chatScroll.clientHeight;
    return distance <= NEAR_BOTTOM_PX;
  }

  function scrollToBottom({ force = false, smooth = false } = {}) {
    if (!force && !state.followOutput) {
      elements.jumpBottomButton.hidden = false;
      return;
    }
    if (force) state.followOutput = true;
    window.requestAnimationFrame(() => {
      state.programmaticScroll = true;
      elements.chatScroll.scrollTo({ top: elements.chatScroll.scrollHeight, behavior: smooth ? "smooth" : "auto" });
      state.nearBottom = true;
      elements.jumpBottomButton.hidden = true;
      window.setTimeout(() => {
        state.programmaticScroll = false;
      }, smooth ? 300 : 0);
    });
  }

  function contentAdded() {
    if (state.followOutput) scrollToBottom();
    else elements.jumpBottomButton.hidden = false;
  }

  async function copyText(text) {
    const value = String(text || "");
    if (!value) return false;
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(value);
        showToast("已复制");
        return true;
      }
    } catch (_) {
      // Use the selection fallback below.
    }
    const textarea = document.createElement("textarea");
    textarea.value = value;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.left = "-9999px";
    textarea.style.top = "0";
    document.body.appendChild(textarea);
    textarea.select();
    textarea.setSelectionRange(0, textarea.value.length);
    let copied = false;
    try {
      copied = document.execCommand("copy");
    } catch (_) {
      copied = false;
    }
    textarea.remove();
    showToast(copied ? "已复制" : "复制失败", copied ? "info" : "error");
    return copied;
  }

  function makeCopyButton(textProvider, label = "复制") {
    const button = document.createElement("button");
    button.type = "button";
    button.title = label;
    button.setAttribute("aria-label", label);
    button.appendChild(makeIconSlot("copy"));
    button.addEventListener("click", () => copyText(typeof textProvider === "function" ? textProvider() : textProvider));
    return button;
  }

  function validHttpUrl(value) {
    const raw = String(value || "").trim();
    if (!/^https?:\/\//i.test(raw)) return null;
    try {
      const url = new URL(raw);
      return url.protocol === "http:" || url.protocol === "https:" ? url.href : null;
    } catch (_) {
      return null;
    }
  }

  function appendInline(parent, source, depth = 0) {
    const text = String(source || "");
    if (depth > 8) {
      parent.appendChild(document.createTextNode(text));
      return;
    }
    let index = 0;
    let plainStart = 0;
    const flushPlain = (end) => {
      if (end > plainStart) parent.appendChild(document.createTextNode(text.slice(plainStart, end)));
    };
    while (index < text.length) {
      if (text[index] === "\\" && index + 1 < text.length && "\\`*_[]".includes(text[index + 1])) {
        flushPlain(index);
        parent.appendChild(document.createTextNode(text[index + 1]));
        index += 2;
        plainStart = index;
        continue;
      }
      if (text[index] === "\n") {
        flushPlain(index);
        parent.appendChild(document.createElement("br"));
        index += 1;
        plainStart = index;
        continue;
      }
      if (text[index] === "`") {
        const end = text.indexOf("`", index + 1);
        if (end > index + 1) {
          flushPlain(index);
          const code = document.createElement("code");
          code.textContent = text.slice(index + 1, end);
          parent.appendChild(code);
          index = end + 1;
          plainStart = index;
          continue;
        }
      }
      if (text[index] === "[") {
        const labelEnd = text.indexOf("](", index + 1);
        const urlEnd = labelEnd >= 0 ? text.indexOf(")", labelEnd + 2) : -1;
        if (labelEnd > index + 1 && urlEnd > labelEnd + 2) {
          const href = validHttpUrl(text.slice(labelEnd + 2, urlEnd));
          if (href) {
            flushPlain(index);
            const link = document.createElement("a");
            link.href = href;
            link.target = "_blank";
            link.rel = "noopener noreferrer";
            appendInline(link, text.slice(index + 1, labelEnd), depth + 1);
            parent.appendChild(link);
            index = urlEnd + 1;
            plainStart = index;
            continue;
          }
        }
      }
      const strongMarker = text.startsWith("**", index) ? "**" : text.startsWith("__", index) ? "__" : null;
      if (strongMarker) {
        const end = text.indexOf(strongMarker, index + 2);
        if (end > index + 2 && text.slice(index + 2, end).trim()) {
          flushPlain(index);
          const strong = document.createElement("strong");
          appendInline(strong, text.slice(index + 2, end), depth + 1);
          parent.appendChild(strong);
          index = end + 2;
          plainStart = index;
          continue;
        }
      }
      if (text[index] === "*" || text[index] === "_") {
        const marker = text[index];
        const end = text.indexOf(marker, index + 1);
        if (end > index + 1 && text.slice(index + 1, end).trim()) {
          flushPlain(index);
          const emphasis = document.createElement("em");
          appendInline(emphasis, text.slice(index + 1, end), depth + 1);
          parent.appendChild(emphasis);
          index = end + 1;
          plainStart = index;
          continue;
        }
      }
      index += 1;
    }
    flushPlain(text.length);
  }

  function codeBlock(language, codeText) {
    const wrapper = document.createElement("div");
    wrapper.className = "code-block";
    const toolbar = document.createElement("div");
    toolbar.className = "code-toolbar";
    const label = document.createElement("span");
    label.textContent = language || "代码";
    const copy = makeCopyButton(codeText, "复制代码");
    copy.className = "code-copy-button";
    toolbar.append(label, copy);
    const pre = document.createElement("pre");
    const code = document.createElement("code");
    if (language) code.className = `language-${language}`;
    code.textContent = codeText;
    pre.appendChild(code);
    wrapper.append(toolbar, pre);
    return wrapper;
  }

  function isMarkdownBlockStart(line) {
    return /^\s*```/.test(line) || /^#{1,6}\s+/.test(line) || /^\s*[-*+]\s+/.test(line) || /^\s*\d+[.)]\s+/.test(line) || /^\s*>/.test(line);
  }

  function renderMarkdown(container, source) {
    const lines = String(source || "").replace(/\r\n?/g, "\n").split("\n");
    const fragment = document.createDocumentFragment();
    let index = 0;
    while (index < lines.length) {
      const line = lines[index];
      if (!line.trim()) {
        index += 1;
        continue;
      }
      const fence = line.match(/^\s*```\s*([\w.+-]*)\s*$/);
      if (fence) {
        const codeLines = [];
        index += 1;
        while (index < lines.length && !/^\s*```\s*$/.test(lines[index])) {
          codeLines.push(lines[index]);
          index += 1;
        }
        if (index < lines.length) index += 1;
        const language = /^[\w.+-]{1,40}$/.test(fence[1] || "") ? fence[1] : "";
        fragment.appendChild(codeBlock(language, codeLines.join("\n")));
        continue;
      }
      const heading = line.match(/^(#{1,6})\s+(.+)$/);
      if (heading) {
        const level = Math.min(6, heading[1].length + 1);
        const node = document.createElement(`h${level}`);
        appendInline(node, heading[2]);
        fragment.appendChild(node);
        index += 1;
        continue;
      }
      const unordered = line.match(/^\s*[-*+]\s+(.+)$/);
      if (unordered) {
        const list = document.createElement("ul");
        while (index < lines.length) {
          const itemMatch = lines[index].match(/^\s*[-*+]\s+(.+)$/);
          if (!itemMatch) break;
          const item = document.createElement("li");
          appendInline(item, itemMatch[1]);
          list.appendChild(item);
          index += 1;
        }
        fragment.appendChild(list);
        continue;
      }
      const ordered = line.match(/^\s*\d+[.)]\s+(.+)$/);
      if (ordered) {
        const list = document.createElement("ol");
        while (index < lines.length) {
          const itemMatch = lines[index].match(/^\s*\d+[.)]\s+(.+)$/);
          if (!itemMatch) break;
          const item = document.createElement("li");
          appendInline(item, itemMatch[1]);
          list.appendChild(item);
          index += 1;
        }
        fragment.appendChild(list);
        continue;
      }
      if (/^\s*>/.test(line)) {
        const quoteLines = [];
        while (index < lines.length) {
          const quote = lines[index].match(/^\s*>\s?(.*)$/);
          if (!quote) break;
          quoteLines.push(quote[1]);
          index += 1;
        }
        const blockquote = document.createElement("blockquote");
        appendInline(blockquote, quoteLines.join("\n"));
        fragment.appendChild(blockquote);
        continue;
      }
      const paragraphLines = [line];
      index += 1;
      while (index < lines.length && lines[index].trim() && !isMarkdownBlockStart(lines[index])) {
        paragraphLines.push(lines[index]);
        index += 1;
      }
      const paragraph = document.createElement("p");
      appendInline(paragraph, paragraphLines.join("\n"));
      fragment.appendChild(paragraph);
    }
    container.replaceChildren(fragment);
  }

  function createDayDivider(timestamp) {
    const divider = document.createElement("div");
    divider.className = "day-divider";
    divider.dataset.dayKey = dayKey(timestamp);
    const label = document.createElement("span");
    label.textContent = formatDayLabel(timestamp);
    divider.appendChild(label);
    return divider;
  }

  function appendDayDividerIfNeeded(timestamp) {
    const dividers = elements.timeline.querySelectorAll(".day-divider");
    const lastDivider = dividers[dividers.length - 1];
    if (!lastDivider || lastDivider.dataset.dayKey !== dayKey(timestamp)) elements.timeline.appendChild(createDayDivider(timestamp));
  }

  function createUserMessage(content, timestamp, attributes = {}) {
    const article = document.createElement("article");
    article.className = "message user-message";
    article.dataset.role = "user";
    if (attributes.turnId) article.dataset.turnId = attributes.turnId;
    if (attributes.runId) article.dataset.runId = attributes.runId;
    if (attributes.followupId) article.dataset.followupId = attributes.followupId;
    const bubble = document.createElement("div");
    bubble.className = "user-bubble";
    const paragraph = document.createElement("p");
    paragraph.textContent = String(content || "");
    bubble.appendChild(paragraph);
    const actions = document.createElement("div");
    actions.className = "message-actions";
    const time = document.createElement("span");
    time.textContent = formatTime(timestamp) || "刚刚";
    time.title = formatDateTime(timestamp);
    actions.append(time, makeCopyButton(String(content || ""), "复制消息"));
    article.append(bubble, actions);
    return article;
  }

  function safeAssetUrl(value) {
    const raw = String(value || "").trim();
    if (!raw) return null;
    try {
      const url = new URL(raw, window.location.origin);
      if (url.origin !== window.location.origin || !url.pathname.startsWith("/api/assets/") || url.pathname === "/api/assets/") return null;
      return url.href;
    } catch (_) {
      return null;
    }
  }

  function validAssetDimension(value) {
    const number = Number(value);
    return Number.isInteger(number) && number > 0 && number <= 100_000 ? number : null;
  }

  function createAssetAction(iconName, label, href, download = false) {
    const link = document.createElement("a");
    link.href = href;
    link.title = label;
    link.setAttribute("aria-label", label);
    link.rel = "noopener noreferrer";
    if (download) link.setAttribute("download", "");
    else link.target = "_blank";
    link.appendChild(makeIconSlot(iconName));
    return link;
  }

  function createConversationMedia(asset, { eager = false } = {}) {
    const source = asset && typeof asset === "object" ? asset : {};
    const url = safeAssetUrl(source.url);
    const mime = String(source.mime || "").trim().toLowerCase();
    const imageMime = !mime || mime.startsWith("image/");
    const width = validAssetDimension(source.width);
    const height = validAssetDimension(source.height);
    const alt = String(source.alt || "").trim() || "Miyu 生成的图片";
    const hideCaption = Boolean(source.hide_caption);

    const figure = document.createElement("figure");
    figure.className = "conversation-media";
    if (source.id != null) figure.dataset.assetId = String(source.id);
    const visual = document.createElement("div");
    visual.className = "conversation-media-visual";
    if (width && height) {
      const ratio = width / height;
      if (ratio >= 0.05 && ratio <= 20) {
        visual.classList.add("has-aspect");
        visual.style.aspectRatio = `${width} / ${height}`;
      }
    }
    const fallback = document.createElement("div");
    fallback.className = "conversation-media-fallback";
    fallback.appendChild(makeIconSlot("circle-alert"));
    const fallbackText = document.createElement("span");
    fallbackText.textContent = url && imageMime ? "图片载入失败" : "图片地址不可用";
    fallback.appendChild(fallbackText);

    if (url && imageMime) {
      const image = document.createElement("img");
      image.alt = alt;
      image.loading = eager ? "eager" : "lazy";
      image.decoding = "async";
      if (width) image.width = width;
      if (height) image.height = height;
      fallback.hidden = true;
      image.addEventListener("error", () => {
        image.remove();
        fallback.hidden = false;
        figure.classList.add("is-error");
        contentAdded();
      }, { once: true });
      image.addEventListener("load", contentAdded, { once: true });
      image.src = url;
      visual.append(image, fallback);
    } else {
      visual.appendChild(fallback);
    }

    const caption = document.createElement("figcaption");
    caption.className = "conversation-media-caption";
    if (!hideCaption) {
      const captionText = document.createElement("span");
      captionText.textContent = alt;
      captionText.title = alt;
      caption.appendChild(captionText);
    } else {
      caption.classList.add("is-actions-only");
    }
    if (url) {
      const actions = document.createElement("span");
      actions.className = "conversation-media-actions";
      actions.append(
        createAssetAction("external-link", "在新窗口打开图片", url),
        createAssetAction("download", "下载图片", url, true)
      );
      caption.appendChild(actions);
    }
    figure.appendChild(visual);
    if (caption.childElementCount) figure.appendChild(caption);
    return figure;
  }

  function reasoningDisplayMode() {
    return ["hidden", "summary", "full"].includes(state.display?.reasoning) ? state.display.reasoning : "summary";
  }

  function normalizeReasoningTitle(value) {
    const title = String(value || "").trim().replace(/^[*#\s]+|[*#\s]+$/g, "");
    if (!title || /^正在(?:思考)?(?:\.{3}|…+)?$/u.test(title)) return "";
    return title;
  }

  function splitReasoningText(value) {
    const raw = String(value || "").trim();
    const bold = raw.match(/^\*\*([^\n*]{1,160})\*\*(?:\r?\n){0,2}([\s\S]*)$/);
    if (bold) return { title: normalizeReasoningTitle(bold[1]), body: bold[2].trim() };
    const heading = raw.match(/^#{1,6}\s+([^\n]{1,160})(?:\r?\n)+([\s\S]*)$/);
    if (heading) return { title: normalizeReasoningTitle(heading[1]), body: heading[2].trim() };
    return { title: "", body: raw };
  }

  function createReasoningBlock(text, title = "已思考", live = false, summaryOnly = false) {
    const details = document.createElement("details");
    details.className = "reasoning-block";
    details.classList.toggle("is-summary", summaryOnly);
    details.open = true;
    const summary = document.createElement("summary");
    const atom = makeIconSlot("atom", "reasoning-icon");
    const titleNode = document.createElement("span");
    titleNode.className = "reasoning-title";
    titleNode.textContent = title || (live ? "正在思考" : "已思考");
    const chevron = makeIconSlot("chevron-right", "reasoning-chevron");
    summary.append(atom, titleNode);
    let liveStatus = null;
    let progress = null;
    if (live) {
      liveStatus = document.createElement("span");
      liveStatus.className = "reasoning-live-status";
      liveStatus.textContent = "正在思考 · 0s";
      summary.appendChild(liveStatus);
      progress = document.createElement("div");
      progress.className = "reasoning-progress";
      progress.setAttribute("role", "progressbar");
      progress.setAttribute("aria-label", "思考进度");
      progress.setAttribute("aria-valuetext", "正在思考");
      const progressFill = document.createElement("i");
      progressFill.setAttribute("aria-hidden", "true");
      progress.appendChild(progressFill);
    }
    summary.appendChild(chevron);
    const body = document.createElement("div");
    body.className = "reasoning-text";
    body.textContent = String(text || "");
    details.append(summary);
    if (progress) details.appendChild(progress);
    details.appendChild(body);
    const block = {
      element: details,
      title: titleNode,
      liveStatus,
      progress,
      body,
      raw: String(text || ""),
      pendingTitle: "",
      summaryOnly,
      partOpen: false,
      startedAt: live ? performance.now() : null,
      finished: !live,
      userToggled: false,
      ignoreNextToggle: false
    };
    details.addEventListener("toggle", () => {
      if (block.ignoreNextToggle) {
        block.ignoreNextToggle = false;
        return;
      }
      block.userToggled = true;
    });
    return block;
  }

  function createAssistantMessage({
    content = "",
    reasoning = "",
    reasoningTitle = "已思考",
    assets = [],
    timestamp = null,
    tokenTotal = 0,
    tokenEstimated = false,
    providerId = "",
    model = "",
    activeContext = true,
    turnId = null,
    muted = false
  } = {}) {
    const article = document.createElement("article");
    article.className = `message assistant-message${muted ? " is-muted" : ""}`;
    article.dataset.role = "assistant";
    if (turnId) article.dataset.turnId = turnId;
    const header = document.createElement("header");
    header.className = "assistant-label";
    const avatar = document.createElement("img");
    avatar.src = "/assets/miyu-logo.png";
    avatar.alt = "";
    avatar.setAttribute("aria-hidden", "true");
    const identity = document.createElement("div");
    const name = document.createElement("strong");
    name.textContent = "Miyu";
    const time = document.createElement("span");
    time.textContent = formatTime(timestamp) || "";
    time.title = formatDateTime(timestamp);
    identity.append(name, time);
    header.append(avatar, identity);
    const assistantContent = document.createElement("div");
    assistantContent.className = "assistant-content";
    const blocks = document.createElement("div");
    blocks.className = "assistant-blocks";
    if (String(reasoning || "").trim() && reasoningDisplayMode() !== "hidden") {
      const parsed = splitReasoningText(reasoning);
      const summaryOnly = reasoningDisplayMode() === "summary";
      blocks.appendChild(createReasoningBlock(parsed.body, "已思考", false, summaryOnly).element);
    }
    if (String(content || "").trim()) {
      const markdown = document.createElement("div");
      markdown.className = "markdown-body";
      renderMarkdown(markdown, content);
      blocks.appendChild(markdown);
    }
    for (const asset of Array.isArray(assets) ? assets : []) blocks.appendChild(createConversationMedia(asset));
    assistantContent.appendChild(blocks);
    article.append(header, assistantContent);

    const meta = document.createElement("div");
    meta.className = "assistant-meta";
    if (state.display?.show_mixed_model_endpoint && (String(providerId || "").trim() || String(model || "").trim())) {
      const endpoint = document.createElement("span");
      endpoint.className = "assistant-endpoint";
      endpoint.textContent = [providerId, model].map((value) => String(value || "").trim()).filter(Boolean).join(" / ");
      meta.appendChild(endpoint);
    }
    if (asFiniteNumber(tokenTotal) > 0) {
      const token = document.createElement("span");
      token.textContent = `${tokenEstimated ? "约 " : ""}${formatTokens(tokenTotal)} tokens`;
      meta.appendChild(token);
    }
    if (!activeContext) {
      const contextBadge = document.createElement("span");
      contextBadge.className = "context-state-badge";
      contextBadge.textContent = "已移出当前上下文";
      meta.appendChild(contextBadge);
    }
    const copyValue = String(content || "").trim() || String(reasoning || "");
    if (copyValue) {
      const spacer = document.createElement("span");
      spacer.className = "meta-spacer";
      meta.append(spacer, makeCopyButton(copyValue, "复制回复"));
    }
    if (meta.childNodes.length) article.appendChild(meta);
    return article;
  }

  function createAnsweredQuestionCard(exchange, compact = true) {
    const card = document.createElement("section");
    card.className = "answered-question-card";
    if (compact) card.classList.add("is-compact");
    const header = document.createElement("header");
    const icon = document.createElement("span");
    icon.className = "question-icon";
    icon.appendChild(makeIconSlot("check"));
    const copy = document.createElement("div");
    const status = document.createElement("small");
    status.textContent = "已回答";
    const title = document.createElement("strong");
    const questions = Array.isArray(exchange?.questions) ? exchange.questions : [];
    title.textContent = questions.length === 1 ? String(questions[0]?.header || "补充确认") : `${questions.length} 项补充确认`;
    copy.append(status, title);
    header.append(icon, copy);
    const list = document.createElement("dl");
    list.className = "answered-question-list";
    const answers = Array.isArray(exchange?.answers) ? exchange.answers : [];
    questions.forEach((question, index) => {
      const row = document.createElement("div");
      const term = document.createElement("dt");
      term.textContent = String(question?.question || question?.header || `问题 ${index + 1}`);
      const description = document.createElement("dd");
      const selected = Array.isArray(answers[index]) ? answers[index] : [];
      description.textContent = selected.map(String).join("、") || "未记录";
      row.append(term, description);
      list.appendChild(row);
    });
    card.append(header, list);
    return card;
  }

  function createPersistedQuestion(exchange, turnId) {
    const wrapper = document.createElement("article");
    wrapper.className = "persisted-question-wrap";
    if (turnId) wrapper.dataset.turnId = turnId;
    wrapper.appendChild(createAnsweredQuestionCard(exchange));
    return wrapper;
  }

  function createTurnStatus(turn) {
    const status = document.createElement("div");
    status.className = "turn-status-line";
    status.dataset.turnStatus = String(turn?.id || "");
    const isInterrupted = turn?.status === "interrupted";
    status.classList.toggle("is-interrupted", isInterrupted);
    status.appendChild(makeIconSlot(isInterrupted ? "circle-alert" : "loader-circle"));
    const text = document.createElement("span");
    text.textContent = isInterrupted ? "本轮已中断" : "本轮正在运行";
    status.appendChild(text);
    if (asFiniteNumber(turn?.token_total) > 0) {
      const usage = document.createElement("span");
      usage.textContent = `${turn.token_usage_estimated ? "约 " : ""}${formatTokens(turn.token_total)} tokens`;
      status.appendChild(usage);
    }
    if (turn?.active_context === false) {
      const context = document.createElement("span");
      context.className = "context-state-badge";
      context.textContent = "已移出当前上下文";
      status.appendChild(context);
    }
    return status;
  }

  function renderPersistedTurn(turn) {
    const turnId = String(turn?.id || "");
    elements.timeline.appendChild(createUserMessage(turn?.user_content || "", turn?.user_timestamp, { turnId }));

    const exchanges = Array.isArray(turn?.question_exchanges) ? turn.question_exchanges : [];
    for (const exchange of exchanges) elements.timeline.appendChild(createPersistedQuestion(exchange, turnId));

    const followups = Array.isArray(turn?.followups) ? turn.followups : [];
    for (const followup of followups) {
      const precedingContent = String(followup?.preceding_assistant_content || "");
      const precedingReasoning = String(followup?.preceding_assistant_reasoning || "");
      if (precedingContent.trim() || precedingReasoning.trim()) {
        elements.timeline.appendChild(createAssistantMessage({
          content: precedingContent,
          reasoning: precedingReasoning,
          providerId: followup?.provider_id,
          model: followup?.model,
          timestamp: followup?.submitted_at,
          turnId,
          activeContext: turn?.active_context !== false
        }));
      }
      elements.timeline.appendChild(createUserMessage(followup?.content || "", followup?.submitted_at, {
        turnId,
        followupId: String(followup?.id || "")
      }));
    }

    const assistantContent = String(turn?.assistant_content || "");
    const assistantReasoning = String(turn?.assistant_reasoning || "");
    const assets = turn?.status === "running" ? [] : (Array.isArray(turn?.assets) ? turn.assets : []);
    if (assistantContent.trim() || assistantReasoning.trim() || assets.length) {
      elements.timeline.appendChild(createAssistantMessage({
        content: assistantContent,
        reasoning: assistantReasoning,
        providerId: turn?.provider_id,
        model: turn?.model,
        assets,
        timestamp: turn?.assistant_timestamp,
        tokenTotal: turn?.token_total,
        tokenEstimated: Boolean(turn?.token_usage_estimated),
        activeContext: turn?.active_context !== false,
        turnId,
        muted: turn?.active_context === false
      }));
    }
    if (turn?.status === "running" || turn?.status === "interrupted") elements.timeline.appendChild(createTurnStatus(turn));
    else if (!assistantContent.trim() && !assistantReasoning.trim() && (asFiniteNumber(turn?.token_total) > 0 || turn?.active_context === false)) {
      const metadata = createTurnStatus({ ...turn, status: "completed" });
      metadata.querySelector("span:nth-child(2)").textContent = "本轮已完成";
      metadata.querySelector(".icon-slot").replaceChildren(createIcon("check"));
      elements.timeline.appendChild(metadata);
    }
  }

  function renderConversation() {
    elements.loadingState.hidden = true;
    elements.blockedState.hidden = true;
    clearQuestionDock();
    elements.timeline.replaceChildren();
    const turns = [...state.turns].sort((left, right) => asFiniteNumber(left?.seq) - asFiniteNumber(right?.seq));
    state.turns = turns;
    if (turns.length === 0) {
      elements.timeline.hidden = true;
      elements.emptyState.hidden = false;
    } else {
      elements.emptyState.hidden = true;
      elements.timeline.hidden = false;
      let previousDay = null;
      for (const turn of turns) {
        const currentDay = dayKey(turn?.user_timestamp);
        if (currentDay !== previousDay) {
          elements.timeline.appendChild(createDayDivider(turn?.user_timestamp));
          previousDay = currentDay;
        }
        renderPersistedTurn(turn);
      }
    }
    state.nearBottom = true;
    state.followOutput = true;
    elements.jumpBottomButton.hidden = true;
    updateConversationChrome();
    window.requestAnimationFrame(() => {
      elements.chatScroll.scrollTop = elements.chatScroll.scrollHeight;
    });
  }

  function createLiveState(runId, options = {}) {
    return {
      runId,
      turnId: options.turnId || null,
      userText: options.userText || "",
      startedAt: options.startedAt || new Date(),
      userRendered: Boolean(options.userRendered),
      article: null,
      blocks: null,
      headerStatus: null,
      meta: null,
      endpoint: null,
      copyButton: null,
      currentText: null,
      assistantText: "",
      assistantReasoning: "",
      assets: [],
      reasoning: null,
      reasoningParts: [],
      reasoningStarted: false,
      reasoningTitle: "",
      reasoningTimer: null,
      providerId: "",
      model: "",
      tools: new Map(),
      questions: new Map(),
      contextOperation: null,
      ended: false
    };
  }

  function renderQueueTray() {
    const prompts = Array.isArray(state.queuedPrompts) ? state.queuedPrompts : [];
    elements.queueTray.replaceChildren();
    elements.queueTray.hidden = prompts.length === 0;
    for (const prompt of prompts) {
      const row = document.createElement("div");
      row.className = "queue-item";
      const text = document.createElement("span");
      text.textContent = String(prompt?.content || "");
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "queue-remove";
      remove.title = "移除排队消息";
      remove.setAttribute("aria-label", "移除排队消息");
      remove.appendChild(makeIconSlot("x"));
      remove.addEventListener("click", () => removeQueuedPrompt(prompt.id));
      row.append(text, remove);
      elements.queueTray.appendChild(row);
    }
    updateControlState();
  }

  async function removeQueuedPrompt(promptId) {
    if (!promptId) return;
    try {
      await apiRequest(`/api/queue/${encodeURIComponent(promptId)}`, { method: "DELETE" });
      state.queuedPrompts = state.queuedPrompts.filter((prompt) => String(prompt?.id) !== String(promptId));
      renderQueueTray();
    } catch (error) {
      showToast(error.message || "排队消息移除失败", "error");
      if (error.status === 404) await loadBootstrap();
    }
  }

  function disposeLiveState(live) {
    if (!live) return;
    if (live.reasoningTimer) {
      window.clearInterval(live.reasoningTimer);
      live.reasoningTimer = null;
    }
    if (live.currentText?.renderFrame) {
      window.cancelAnimationFrame(live.currentText.renderFrame);
      live.currentText.renderFrame = null;
    }
    for (const tool of live.tools?.values?.() || []) {
      if (tool.collapseTimer) window.clearTimeout(tool.collapseTimer);
      tool.collapseTimer = null;
    }
  }

  function establishRun(runId) {
    if (!runId) return null;
    if (state.live?.runId === runId) return state.live;
    if (state.activeRunId && state.activeRunId !== runId) return null;
    state.activeRunId = runId;
    state.cancellationRequested = false;
    const runningTurn = [...state.turns].reverse().find((turn) => turn?.status === "running");
    state.live = createLiveState(runId, {
      turnId: runningTurn?.id || null,
      userText: runningTurn?.user_content || "",
      startedAt: runningTurn?.user_timestamp || new Date(),
      userRendered: Boolean(runningTurn)
    });
    updateConversationChrome();
    updateRuntimeUsage();
    updateControlState();
    return state.live;
  }

  function ensureTimelineVisible() {
    elements.loadingState.hidden = true;
    elements.blockedState.hidden = true;
    elements.emptyState.hidden = true;
    elements.timeline.hidden = false;
  }

  function ensureLiveUser(content, runId) {
    const live = establishRun(runId);
    if (!live || live.userRendered) return;
    const text = String(content || live.userText || "");
    if (!text.trim()) return;
    live.userText = text;
    ensureTimelineVisible();
    appendDayDividerIfNeeded(new Date());
    const message = createUserMessage(text, new Date(), { runId });
    if (live.article?.isConnected) elements.timeline.insertBefore(message, live.article);
    else elements.timeline.appendChild(message);
    live.userRendered = true;
    updateConversationChrome();
    contentAdded();
  }

  function removeRunningStatus(turnId) {
    if (!turnId) return;
    const status = Array.from(elements.timeline.querySelectorAll("[data-turn-status]"))
      .find((node) => node.dataset.turnStatus === String(turnId));
    status?.remove();
  }

  function ensureLiveArticle(live) {
    if (live.article) return live.article;
    ensureTimelineVisible();
    ensureLiveUser(live.userText, live.runId);
    removeRunningStatus(live.turnId);
    const article = document.createElement("article");
    article.className = "message assistant-message live-assistant";
    article.dataset.role = "assistant";
    article.dataset.runId = live.runId;
    const header = document.createElement("header");
    header.className = "assistant-label";
    const avatar = document.createElement("img");
    avatar.src = "/assets/miyu-logo.png";
    avatar.alt = "";
    avatar.setAttribute("aria-hidden", "true");
    const identity = document.createElement("div");
    const name = document.createElement("strong");
    name.textContent = "Miyu";
    const status = document.createElement("span");
    status.className = "live-indicator";
    status.textContent = "正在回复";
    identity.append(name, status);
    header.append(avatar, identity);
    const assistantContent = document.createElement("div");
    assistantContent.className = "assistant-content";
    const blocks = document.createElement("div");
    blocks.className = "assistant-blocks";
    assistantContent.appendChild(blocks);
    const meta = document.createElement("div");
    meta.className = "assistant-meta";
    const endpoint = document.createElement("span");
    endpoint.className = "assistant-endpoint";
    endpoint.hidden = true;
    const metaText = document.createElement("span");
    metaText.textContent = "正在生成";
    const spacer = document.createElement("span");
    spacer.className = "meta-spacer";
    const copy = makeCopyButton(() => live.assistantText, "复制回复");
    copy.hidden = true;
    meta.append(endpoint, metaText, spacer, copy);
    article.append(header, assistantContent, meta);
    elements.timeline.appendChild(article);
    live.article = article;
    live.blocks = blocks;
    live.headerStatus = status;
    live.meta = metaText;
    live.endpoint = endpoint;
    live.copyButton = copy;
    contentAdded();
    return article;
  }

  function breakLiveText(live) {
    live.currentText = null;
  }

  function scheduleMarkdownRender(block) {
    if (block.renderFrame) return;
    block.renderFrame = window.requestAnimationFrame(() => {
      block.renderFrame = null;
      renderMarkdown(block.element, block.raw);
      contentAdded();
    });
  }

  function appendAssistantDelta(live, delta) {
    const text = String(delta || "");
    if (!text) return;
    ensureLiveArticle(live);
    if (!live.currentText) {
      finalizeLiveReasoning(live);
      if (live.meta) live.meta.textContent = "正在生成";
      const element = document.createElement("div");
      element.className = "markdown-body live-text-block";
      const block = { element, raw: "", renderFrame: null };
      live.blocks.appendChild(element);
      live.currentText = block;
      live.contextOperation = null;
      if (live.assistantText && !/\s$/.test(live.assistantText)) live.assistantText += "\n\n";
    }
    live.currentText.raw += text;
    live.assistantText += text;
    live.copyButton.hidden = !live.assistantText.trim();
    scheduleMarkdownRender(live.currentText);
    contentAdded();
  }

  function ensureLiveReasoning(live) {
    ensureLiveArticle(live);
    if (live.reasoning) return live.reasoning;
    breakLiveText(live);
    live.contextOperation = null;
    const mode = reasoningDisplayMode();
    const reasoning = createReasoningBlock("", "正在思考", true, mode === "summary");
    reasoning.pendingTitle = normalizeReasoningTitle(live.reasoningTitle);
    if (mode !== "hidden") live.blocks.appendChild(reasoning.element);
    live.reasoning = reasoning;
    live.reasoningParts.push(reasoning);
    if (live.reasoningTimer) window.clearInterval(live.reasoningTimer);
    const updateProgress = () => {
      if (!reasoning.liveStatus || reasoning.startedAt == null) return;
      const elapsed = Math.max(0, Math.floor((performance.now() - reasoning.startedAt) / 1000));
      reasoning.liveStatus.textContent = `正在思考 · ${elapsed}s`;
    };
    updateProgress();
    live.reasoningTimer = window.setInterval(updateProgress, 1000);
    return reasoning;
  }

  function collectLiveReasoning(live) {
    return (live.reasoningParts || [])
      .map((part) => String(part.raw || "").trim())
      .filter(Boolean)
      .join("\n\n");
  }

  function finalizeLiveReasoning(live) {
    const reasoning = live.reasoning;
    if (!reasoning) return;
    if (live.reasoningTimer) {
      window.clearInterval(live.reasoningTimer);
      live.reasoningTimer = null;
    }
    const parsed = splitReasoningText(reasoning.raw);
    const title = "已思考";
    reasoning.raw = parsed.body;
    reasoning.finished = true;
    if (!reasoning.raw.trim() && title === "已思考") {
      reasoning.element.remove();
    } else {
      reasoning.title.textContent = title;
      reasoning.body.textContent = reasoning.raw;
      if (reasoning.progress) reasoning.progress.remove();
      if (reasoning.liveStatus) reasoning.liveStatus.remove();
      if (!reasoning.userToggled) reasoning.element.open = true;
    }
    live.reasoning = null;
    live.reasoningTitle = "";
    live.reasoningStarted = false;
    live.assistantReasoning = collectLiveReasoning(live);
  }

  function handleReasoningEvent(name, live, data) {
    if (name === "reasoning.start") {
      finalizeLiveReasoning(live);
      live.reasoningStarted = true;
      breakLiveText(live);
      ensureLiveReasoning(live);
      if (live.meta) live.meta.textContent = "正在思考";
      return;
    }
    if (name === "reasoning.part_start") {
      finalizeLiveReasoning(live);
      live.reasoningStarted = true;
      breakLiveText(live);
      ensureLiveReasoning(live);
      if (live.meta) live.meta.textContent = "正在思考";
      return;
    }
    if (name === "reasoning.reset") {
      if (live.reasoning) {
        live.reasoning.raw = "";
        live.reasoning.body.textContent = "";
        live.reasoning.pendingTitle = "";
      }
      return;
    }
    if (name === "reasoning.title") {
      live.reasoningTitle = String(data?.title || "").trim();
      const reasoning = ensureLiveReasoning(live);
      reasoning.pendingTitle = normalizeReasoningTitle(live.reasoningTitle);
      return;
    }
    if (name === "reasoning.delta") {
      const delta = String(data?.delta || "");
      if (!delta) return;
      const reasoning = ensureLiveReasoning(live);
      reasoning.raw += delta;
      reasoning.body.textContent = reasoning.raw;
      live.assistantReasoning = collectLiveReasoning(live);
      contentAdded();
      return;
    }
    if (name === "reasoning.part_end") {
      finalizeLiveReasoning(live);
    }
  }

  function prettyArguments(value) {
    if (value == null) return "";
    if (typeof value === "string") {
      const trimmed = value.trim();
      if (!trimmed) return "";
      try {
        return JSON.stringify(JSON.parse(trimmed), null, 2);
      } catch (_) {
        return value;
      }
    }
    try {
      return JSON.stringify(value, null, 2);
    } catch (_) {
      return String(value);
    }
  }

  function parsedToolArguments(value) {
    if (value && typeof value === "object" && !Array.isArray(value)) return value;
    if (typeof value !== "string" || !value.trim()) return {};
    try {
      const parsed = JSON.parse(value);
      return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
    } catch (_) {
      return {};
    }
  }

  function compactLine(value, limit = 92) {
    const line = String(value || "").replace(/\s+/g, " ").trim();
    if (line.length <= limit) return line;
    return `${line.slice(0, Math.max(1, limit - 1))}…`;
  }

  function compactPath(value) {
    const path = String(value || "").trim();
    if (!path) return "";
    return path.split(/[\\/]/).filter(Boolean).pop() || path;
  }

  function toolSubject(name, value) {
    const args = parsedToolArguments(value);
    const toolName = String(name || "");
    if (toolName === "run_command") return compactLine(args.command || args.cmd);
    if (["read", "write", "edit", "apply_patch", "print_image", "vision_analyze"].includes(toolName)) {
      return compactPath(args.filePath || args.file_path || args.path || args.image);
    }
    if (toolName === "grep") {
      const target = compactPath(args.path);
      return compactLine(`${args.pattern || ""}${target ? ` · ${target}` : ""}`);
    }
    if (toolName === "glob") return compactLine(`${args.pattern || ""}${args.path ? ` · ${compactPath(args.path)}` : ""}`);
    if (["webfetch", "web_fetch"].includes(toolName)) return compactLine(args.url);
    if (["web_search", "search_web", "search_web_images"].includes(toolName)) return compactLine(args.query || args.q);
    if (toolName === "generate_image") return compactLine(args.prompt);
    if (toolName === "task") return compactLine(args.description || args.prompt);
    if (toolName === "load_skill") return compactLine(args.name);
    const preferred = ["query", "command", "path", "filePath", "url", "name", "id", "target"];
    for (const key of preferred) {
      if (typeof args[key] === "string" && args[key].trim()) return compactLine(args[key]);
    }
    return "";
  }

  function countToolOutputLines(tool) {
    const streamed = `${tool.stdoutDetail.raw || ""}${tool.stderrDetail.raw || ""}`;
    const output = streamed.trim() ? streamed : tool.resultDetail.raw || "";
    const normalized = output.replace(/\r\n/g, "\n").replace(/\r/g, "\n").replace(/\n+$/, "");
    return normalized ? normalized.split("\n").length : 0;
  }

  function formatToolDuration(milliseconds) {
    if (!Number.isFinite(milliseconds) || milliseconds < 0) return "";
    if (milliseconds < 1_000) return `${Math.max(1, Math.round(milliseconds))} ms`;
    if (milliseconds < 10_000) return `${(milliseconds / 1_000).toFixed(1)} s`;
    return `${Math.round(milliseconds / 1_000)} s`;
  }

  function updateToolSummary(tool) {
    const details = [];
    if (tool.subject) details.push(tool.subject);
    if (tool.imageCount) details.push(`${tool.imageCount} 张图片`);
    const lines = countToolOutputLines(tool);
    if (lines) details.push(`${lines} 行输出`);
    if (tool.finishedAt != null) details.push(formatToolDuration(tool.finishedAt - tool.startedAt));
    tool.summary.textContent = details.filter(Boolean).join(" · ") || (tool.finished ? "无输出" : "等待输出");
  }

  function scrollToolOutputToEnd(tool) {
    for (const detail of [tool.stdoutDetail, tool.stderrDetail, tool.resultDetail]) {
      if (!detail.wrapper.hidden) detail.content.scrollTop = detail.content.scrollHeight;
    }
  }

  function boundedAppend(current, addition) {
    const combined = `${current || ""}${addition || ""}`;
    if (combined.length <= MAX_TOOL_OUTPUT_CHARS) return combined;
    return `[较早输出已省略]\n${combined.slice(combined.length - MAX_TOOL_OUTPUT_CHARS)}`;
  }

  function createToolDetail(labelText, preformatted = false) {
    const wrapper = document.createElement("div");
    wrapper.className = "tool-detail";
    wrapper.hidden = true;
    const label = document.createElement("span");
    label.className = "tool-detail-label";
    label.textContent = labelText;
    const content = document.createElement(preformatted ? "pre" : "p");
    wrapper.append(label, content);
    return { wrapper, content, raw: "" };
  }

  function updateToolStatus(tool, status, iconName, statusClass = "") {
    tool.statusText.textContent = status;
    tool.statusIcon.replaceChildren(createIcon(iconName));
    tool.statusIcon.classList.toggle("is-spinning", iconName === "loader-circle");
    tool.card.classList.remove("is-success", "is-failure");
    if (statusClass) tool.card.classList.add(statusClass);
  }

  function createTool(live, data) {
    ensureLiveArticle(live);
    breakLiveText(live);
    finalizeLiveReasoning(live);
    live.contextOperation = null;
    const toolId = String(data?.tool_id || `${live.runId}_tool_unknown_${live.tools.size + 1}`);
    if (live.tools.has(toolId)) return live.tools.get(toolId);
    const card = document.createElement("section");
    card.className = "tool-card collapsed";
    card.dataset.toolId = toolId;
    const head = document.createElement("button");
    head.className = "tool-head";
    head.type = "button";
    head.setAttribute("aria-expanded", "false");
    const icon = document.createElement("span");
    icon.className = "tool-icon";
    icon.appendChild(makeIconSlot("wrench"));
    const title = document.createElement("span");
    title.className = "tool-title";
    const displayName = document.createElement("strong");
    displayName.textContent = String(data?.display_name || data?.name || "工具");
    const realName = document.createElement("small");
    realName.className = "tool-technical-name";
    realName.textContent = String(data?.name || "");
    const summary = document.createElement("small");
    summary.className = "tool-summary";
    title.append(displayName, realName, summary);
    const status = document.createElement("span");
    status.className = "tool-status";
    const statusIcon = makeIconSlot("loader-circle", "is-spinning");
    const statusText = document.createElement("span");
    statusText.textContent = "运行中";
    status.append(statusIcon, statusText);
    const chevron = makeIconSlot("chevron-down", "tool-chevron");
    head.append(icon, title, status, chevron);
    const body = document.createElement("div");
    body.className = "tool-body";
    const argumentsDetail = createToolDetail("参数", true);
    const progressDetail = createToolDetail("进度");
    const stdoutDetail = createToolDetail("命令输出", true);
    const stderrDetail = createToolDetail("错误输出", true);
    stderrDetail.wrapper.classList.add("is-stderr");
    const resultDetail = createToolDetail("结果", true);
    const argumentText = prettyArguments(data?.arguments);
    if (argumentText) {
      argumentsDetail.raw = argumentText;
      argumentsDetail.content.textContent = argumentText;
      argumentsDetail.wrapper.hidden = false;
    }
    body.append(argumentsDetail.wrapper, progressDetail.wrapper, stdoutDetail.wrapper, stderrDetail.wrapper, resultDetail.wrapper);
    card.append(head, body);
    const tool = {
      id: toolId,
      name: String(data?.name || ""),
      card,
      head,
      body,
      status,
      statusIcon,
      statusText,
      summary,
      argumentsDetail,
      progressDetail,
      stdoutDetail,
      stderrDetail,
      resultDetail,
      subject: toolSubject(data?.name, data?.arguments),
      startedAt: performance.now(),
      finishedAt: null,
      imageCount: 0,
      finished: false,
      collapseTimer: null
    };
    head.addEventListener("click", () => {
      const collapsed = card.classList.toggle("collapsed");
      head.setAttribute("aria-expanded", String(!collapsed));
      if (!collapsed) {
        window.requestAnimationFrame(() => {
          scrollToolOutputToEnd(tool);
          contentAdded();
        });
      }
    });
    updateToolSummary(tool);
    live.tools.set(toolId, tool);
    live.blocks.appendChild(card);
    contentAdded();
    return tool;
  }

  function ensureTool(live, data) {
    const toolId = String(data?.tool_id || "");
    return (toolId && live.tools.get(toolId)) || createTool(live, data);
  }

  function handleToolEvent(name, live, data) {
    if (name === "tool.started") {
      createTool(live, data);
      return;
    }
    const tool = ensureTool(live, data);
    if (name === "tool.image") {
      const asset = data?.asset && typeof data.asset === "object" ? data.asset : null;
      if (asset && safeAssetUrl(asset.url)) {
        const assetId = String(asset.id || asset.url);
        if (!live.assets.some((item) => String(item?.id || item?.url) === assetId)) {
          ensureLiveArticle(live);
          breakLiveText(live);
          finalizeLiveReasoning(live);
          live.contextOperation = null;
          live.assets.push(asset);
          live.blocks.appendChild(createConversationMedia(asset, { eager: true }));
          tool.imageCount += 1;
        }
      } else if (data?.error) {
        const message = String(data.error);
        tool.progressDetail.raw = message;
        tool.progressDetail.content.textContent = message;
        tool.progressDetail.wrapper.hidden = false;
      }
      updateToolSummary(tool);
    } else if (name === "tool.progress") {
      const message = String(data?.message || "");
      tool.progressDetail.raw = message;
      tool.progressDetail.content.textContent = message;
      tool.progressDetail.wrapper.hidden = !message;
      if (!tool.subject && message) tool.subject = compactLine(message);
      updateToolStatus(tool, "运行中", "loader-circle");
      updateToolSummary(tool);
    } else if (name === "tool.output") {
      const detail = data?.stream === "stderr" ? tool.stderrDetail : tool.stdoutDetail;
      detail.raw = boundedAppend(detail.raw, String(data?.output || ""));
      detail.content.textContent = detail.raw;
      detail.wrapper.hidden = !detail.raw;
      if (!tool.card.classList.contains("collapsed")) detail.content.scrollTop = detail.content.scrollHeight;
      updateToolSummary(tool);
    } else if (name === "tool.finished") {
      tool.finished = true;
      tool.finishedAt = performance.now();
      const output = String(data?.output || "");
      tool.resultDetail.raw = output.length > MAX_TOOL_OUTPUT_CHARS ? `[较早输出已省略]\n${output.slice(-MAX_TOOL_OUTPUT_CHARS)}` : output;
      tool.resultDetail.content.textContent = tool.resultDetail.raw;
      tool.resultDetail.wrapper.hidden = !tool.resultDetail.raw;
      const ok = Boolean(data?.ok);
      updateToolStatus(tool, ok ? "完成" : "失败", ok ? "check" : "circle-alert", ok ? "is-success" : "is-failure");
      updateToolSummary(tool);
      if (ok) {
        tool.card.classList.add("collapsed");
        tool.head.setAttribute("aria-expanded", "false");
      } else {
        tool.card.classList.add("collapsed");
        tool.head.setAttribute("aria-expanded", "false");
      }
    }
    contentAdded();
  }

  function updateQuestionOptionClasses(questionState) {
    for (const control of questionState.controls) {
      for (const option of control.options) option.label.classList.toggle("selected", option.input.checked);
      if (control.custom) control.custom.wrapper.classList.toggle("selected", control.custom.toggle.checked);
    }
    questionState.pageTabs?.forEach((tab, index) => {
      const control = questionState.controls[index];
      const answered = control.options.some((option) => option.input.checked)
        || Boolean(control.custom?.toggle.checked && control.custom.textarea.value.trim());
      tab.classList.toggle("is-complete", answered);
    });
  }

  function updateQuestionDock() {
    elements.questionDock.hidden = elements.questionDock.childElementCount === 0;
    window.requestAnimationFrame(updateJumpButtonOffset);
  }

  function clearQuestionDock() {
    elements.questionDock.replaceChildren();
    updateQuestionDock();
  }

  function moveQuestionToTimeline(questionState) {
    if (questionState.card.parentElement !== elements.questionDock) return;
    if (questionState.timelineParent?.isConnected) questionState.timelineParent.appendChild(questionState.card);
    else questionState.card.remove();
    updateQuestionDock();
  }

  function removeQuestionFromDock(questionState) {
    if (questionState.card.parentElement === elements.questionDock) questionState.card.remove();
    updateQuestionDock();
  }

  function setQuestionPage(questionState, index, { focus = false } = {}) {
    if (!questionState?.pages?.length) return;
    const lastIndex = questionState.pages.length - 1;
    const nextIndex = Math.max(0, Math.min(lastIndex, Number(index) || 0));
    questionState.pageIndex = nextIndex;
    questionState.pages.forEach((page, pageIndex) => {
      page.hidden = pageIndex !== nextIndex;
    });
    questionState.pageTabs.forEach((tab, pageIndex) => {
      const active = pageIndex === nextIndex;
      tab.classList.toggle("active", active);
      tab.setAttribute("aria-selected", String(active));
      tab.tabIndex = active ? 0 : -1;
    });
    const multiple = Boolean(questionState.questions[nextIndex]?.multiple);
    questionState.pageLabel.textContent = `第 ${nextIndex + 1} / ${questionState.pages.length} 项`;
    questionState.hint.textContent = multiple ? "可多选" : "请选择一项";
    questionState.previous.hidden = nextIndex === 0;
    questionState.next.hidden = nextIndex === lastIndex;
    questionState.submit.hidden = nextIndex !== lastIndex;
    elements.questionDock.scrollTop = 0;
    window.requestAnimationFrame(() => {
      updateJumpButtonOffset();
      if (focus) questionState.pages[nextIndex].querySelector("input:not(:disabled), textarea:not(:disabled)")?.focus();
    });
  }

  function selectedQuestionAnswers(questionState) {
    const answers = [];
    for (let index = 0; index < questionState.controls.length; index += 1) {
      const control = questionState.controls[index];
      const selected = control.options.filter((option) => option.input.checked).map((option) => option.value);
      if (control.custom?.toggle.checked) {
        const custom = control.custom.textarea.value.trim();
        if (!custom) throw new Error(`请填写第 ${index + 1} 项的自定义回答`);
        if (countCharacters(custom) > MAX_CUSTOM_ANSWER_CHARS) throw new Error(`第 ${index + 1} 项的自定义回答不能超过 4,000 个字符`);
        if (/[\u0000-\u001f\u007f-\u009f]/.test(custom)) throw new Error(`第 ${index + 1} 项的自定义回答不能包含控制字符或换行`);
        if (selected.includes(custom)) throw new Error(`第 ${index + 1} 项包含重复回答`);
        selected.push(custom);
      }
      if (selected.length === 0) throw new Error(`请回答第 ${index + 1} 项`);
      if (!control.multiple && selected.length !== 1) throw new Error(`第 ${index + 1} 项只能选择一个回答`);
      answers.push(selected);
    }
    return answers;
  }

  function setQuestionControlsDisabled(questionState, disabled) {
    questionState.form.querySelectorAll("input, textarea, button").forEach((control) => {
      control.disabled = disabled;
    });
  }

  function renderQuestionAnswerSummary(questionState, answers) {
    questionState.summary.replaceChildren();
    const normalized = Array.isArray(answers) ? answers : [];
    questionState.questions.forEach((question, index) => {
      const row = document.createElement("div");
      const term = document.createElement("dt");
      term.textContent = String(question?.question || question?.header || `问题 ${index + 1}`);
      const value = document.createElement("dd");
      value.textContent = (Array.isArray(normalized[index]) ? normalized[index] : []).map(String).join("、") || "未记录";
      row.append(term, value);
      questionState.summary.appendChild(row);
    });
    questionState.summary.hidden = false;
  }

  function markQuestionAnswered(questionState, answers) {
    if (!questionState || !questionState.pending) return;
    questionState.pending = false;
    questionState.submitting = false;
    questionState.answers = answers;
    questionState.card.classList.remove("is-error");
    questionState.card.classList.add("is-answered");
    questionState.status.textContent = "已回答";
    questionState.icon.replaceChildren(makeIconSlot("check"));
    questionState.error.hidden = true;
    setQuestionControlsDisabled(questionState, true);
    renderQuestionAnswerSummary(questionState, answers);
    moveQuestionToTimeline(questionState);
    updateControlState();
    contentAdded();
  }

  async function submitQuestion(questionState) {
    if (!questionState.pending || questionState.submitting) return;
    let answers;
    try {
      answers = selectedQuestionAnswers(questionState);
    } catch (error) {
      const page = String(error.message || "").match(/第 (\d+) 项/);
      if (page) setQuestionPage(questionState, Number(page[1]) - 1);
      questionState.error.textContent = error.message;
      questionState.error.hidden = false;
      questionState.card.classList.add("is-error");
      return;
    }
    questionState.submitting = true;
    questionState.error.hidden = true;
    questionState.card.classList.remove("is-error");
    questionState.submit.textContent = "提交中";
    setQuestionControlsDisabled(questionState, true);
    try {
      await apiRequest(`/api/questions/${encodeURIComponent(questionState.id)}/answer`, {
        method: "POST",
        body: JSON.stringify({ answers })
      });
      if (questionState.pending) markQuestionAnswered(questionState, answers);
    } catch (error) {
      if (!questionState.pending) return;
      questionState.submitting = false;
      questionState.error.textContent = error.message || "回答提交失败";
      questionState.error.hidden = false;
      questionState.card.classList.add("is-error");
      questionState.submit.textContent = "提交回答";
      setQuestionControlsDisabled(questionState, false);
      showToast(error.message || "回答提交失败", "error");
      if (error.status === 404 || error.status === 409) window.setTimeout(() => loadBootstrap(), 300);
    }
  }

  function createQuestion(live, data) {
    const questionId = String(data?.question_id || "");
    if (!questionId) return null;
    if (live.questions.has(questionId)) return live.questions.get(questionId);
    ensureLiveArticle(live);
    breakLiveText(live);
    finalizeLiveReasoning(live);
    live.contextOperation = null;
    const questions = Array.isArray(data?.questions) ? data.questions : [];
    const card = document.createElement("section");
    card.className = "question-card";
    card.dataset.questionId = questionId;
    const titleId = `live-question-title-${live.questions.size + 1}`;
    card.setAttribute("aria-labelledby", titleId);
    const header = document.createElement("header");
    const icon = document.createElement("span");
    icon.className = "question-icon";
    icon.appendChild(makeIconSlot("circle-help"));
    const headerCopy = document.createElement("div");
    const status = document.createElement("small");
    status.textContent = "等待回答";
    const title = document.createElement("strong");
    title.id = titleId;
    title.textContent = questions.length === 1 ? String(questions[0]?.header || "补充确认") : `${questions.length} 项补充确认`;
    headerCopy.append(status, title);
    header.append(icon, headerCopy);
    const form = document.createElement("form");
    form.className = "question-form";
    const pagination = document.createElement("div");
    pagination.className = "question-pagination";
    const pageLabel = document.createElement("span");
    pageLabel.className = "question-page-label";
    const pageTabsWrap = document.createElement("div");
    pageTabsWrap.className = "question-page-tabs";
    pageTabsWrap.setAttribute("role", "tablist");
    pageTabsWrap.setAttribute("aria-label", "问题页");
    pagination.append(pageLabel, pageTabsWrap);
    form.appendChild(pagination);
    const controls = [];
    const pages = [];
    const pageTabs = [];
    questions.forEach((question, questionIndex) => {
      const fieldset = document.createElement("fieldset");
      fieldset.className = "question-fieldset";
      fieldset.id = `question-${questionId}-page-${questionIndex + 1}`;
      fieldset.setAttribute("role", "tabpanel");
      fieldset.hidden = questionIndex !== 0;
      const pageTab = document.createElement("button");
      pageTab.type = "button";
      pageTab.className = "question-page-tab";
      pageTab.id = `question-${questionId}-tab-${questionIndex + 1}`;
      pageTab.textContent = String(questionIndex + 1);
      pageTab.title = String(question?.header || `问题 ${questionIndex + 1}`);
      pageTab.setAttribute("role", "tab");
      pageTab.setAttribute("aria-controls", fieldset.id);
      pageTab.setAttribute("aria-selected", String(questionIndex === 0));
      fieldset.setAttribute("aria-labelledby", pageTab.id);
      pageTabsWrap.appendChild(pageTab);
      pageTabs.push(pageTab);
      const legend = document.createElement("legend");
      const headerLabel = document.createElement("span");
      headerLabel.className = "question-header-label";
      headerLabel.textContent = String(question?.header || `问题 ${questionIndex + 1}`);
      legend.append(headerLabel, document.createTextNode(String(question?.question || "")));
      fieldset.appendChild(legend);
      const optionList = document.createElement("div");
      optionList.className = "question-options";
      const multiple = Boolean(question?.multiple);
      const inputType = multiple ? "checkbox" : "radio";
      const inputName = `question-${questionId}-${questionIndex}`;
      const options = [];
      for (const option of Array.isArray(question?.options) ? question.options : []) {
        const label = document.createElement("label");
        label.className = "question-option";
        const input = document.createElement("input");
        input.type = inputType;
        input.name = inputName;
        input.value = String(option?.label || "");
        const optionCopy = document.createElement("span");
        optionCopy.className = "question-option-copy";
        const optionLabel = document.createElement("strong");
        optionLabel.textContent = String(option?.label || "");
        optionCopy.appendChild(optionLabel);
        if (String(option?.description || "")) {
          const description = document.createElement("small");
          description.textContent = String(option.description);
          optionCopy.appendChild(description);
        }
        label.append(input, optionCopy);
        optionList.appendChild(label);
        options.push({ input, label, value: String(option?.label || "") });
      }
      fieldset.appendChild(optionList);
      let custom = null;
      if (question?.custom !== false) {
        const wrapper = document.createElement("label");
        wrapper.className = "custom-answer";
        const toggle = document.createElement("input");
        toggle.type = inputType;
        toggle.name = inputName;
        toggle.value = "__custom__";
        const textarea = document.createElement("textarea");
        textarea.rows = 1;
        textarea.placeholder = "自定义回答";
        textarea.setAttribute("aria-label", `${question?.header || `问题 ${questionIndex + 1}`}的自定义回答`);
        textarea.addEventListener("focus", () => {
          toggle.checked = true;
          updateQuestionOptionClasses(questionState);
        });
        textarea.addEventListener("input", () => {
          if (textarea.value) toggle.checked = true;
          updateQuestionOptionClasses(questionState);
        });
        wrapper.append(toggle, textarea);
        fieldset.appendChild(wrapper);
        custom = { wrapper, toggle, textarea };
      }
      form.appendChild(fieldset);
      pages.push(fieldset);
      controls.push({ multiple, options, custom });
    });
    pagination.hidden = questions.length <= 1;
    const error = document.createElement("p");
    error.className = "question-error";
    error.hidden = true;
    const actions = document.createElement("footer");
    actions.className = "question-actions";
    const hint = document.createElement("span");
    const pageActions = document.createElement("div");
    pageActions.className = "question-page-actions";
    const previous = document.createElement("button");
    previous.type = "button";
    previous.className = "question-page-button is-previous";
    previous.title = "上一题";
    previous.setAttribute("aria-label", "上一题");
    previous.appendChild(makeIconSlot("chevron-right"));
    const next = document.createElement("button");
    next.type = "button";
    next.className = "question-page-button";
    next.title = "下一题";
    next.setAttribute("aria-label", "下一题");
    next.appendChild(makeIconSlot("chevron-right"));
    const submit = document.createElement("button");
    submit.className = "question-submit";
    submit.type = "submit";
    submit.textContent = "提交回答";
    pageActions.append(previous, next, submit);
    actions.append(hint, pageActions);
    form.append(error, actions);
    const summary = document.createElement("dl");
    summary.className = "question-answer-summary";
    summary.hidden = true;
    card.append(header, form, summary);
    const questionState = {
      id: questionId,
      runId: live.runId,
      questions,
      card,
      form,
      controls,
      pages,
      pageTabs,
      pageIndex: 0,
      pageLabel,
      hint,
      previous,
      next,
      icon,
      status,
      submit,
      error,
      summary,
      timelineParent: live.blocks,
      pending: true,
      submitting: false,
      answers: null
    };
    form.querySelectorAll("input").forEach((input) => input.addEventListener("change", () => updateQuestionOptionClasses(questionState)));
    pageTabs.forEach((tab, index) => tab.addEventListener("click", () => setQuestionPage(questionState, index, { focus: true })));
    pageTabsWrap.addEventListener("keydown", (event) => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
      event.preventDefault();
      let index = questionState.pageIndex;
      if (event.key === "ArrowLeft") index -= 1;
      else if (event.key === "ArrowRight") index += 1;
      else index = event.key === "Home" ? 0 : pageTabs.length - 1;
      setQuestionPage(questionState, index);
      pageTabs[questionState.pageIndex]?.focus();
    });
    previous.addEventListener("click", () => setQuestionPage(questionState, questionState.pageIndex - 1, { focus: true }));
    next.addEventListener("click", () => setQuestionPage(questionState, questionState.pageIndex + 1, { focus: true }));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      submitQuestion(questionState);
    });
    live.questions.set(questionId, questionState);
    elements.questionDock.replaceChildren(card);
    updateQuestionDock();
    setQuestionPage(questionState, 0);
    updateQuestionOptionClasses(questionState);
    updateControlState();
    contentAdded();
    return questionState;
  }

  function endPendingQuestions(live, message) {
    for (const question of live.questions.values()) {
      if (!question.pending) continue;
      question.pending = false;
      question.submitting = false;
      question.card.classList.add("is-error");
      question.status.textContent = "本轮已结束";
      question.error.textContent = message;
      question.error.hidden = false;
      setQuestionControlsDisabled(question, true);
      removeQuestionFromDock(question);
    }
  }

  function createContextOperation(live, kind) {
    ensureLiveArticle(live);
    breakLiveText(live);
    finalizeLiveReasoning(live);
    const block = document.createElement("section");
    block.className = "context-operation";
    const title = document.createElement("strong");
    title.append(makeIconSlot("refresh-cw"), document.createElement("span"));
    title.lastChild.textContent = kind === "compact" ? "正在整理上下文" : "正在释放旧上下文";
    const output = document.createElement("pre");
    output.hidden = true;
    block.append(title, output);
    const operation = { kind, block, title: title.lastChild, output, raw: "" };
    live.blocks.appendChild(block);
    live.contextOperation = operation;
    contentAdded();
    return operation;
  }

  function handleContextEvent(name, live, data) {
    if (name === "context.compact_start") createContextOperation(live, "compact");
    else if (name === "context.compact_delta") {
      const operation = live.contextOperation?.kind === "compact" ? live.contextOperation : createContextOperation(live, "compact");
      operation.raw = boundedAppend(operation.raw, String(data?.delta || ""));
      operation.output.textContent = operation.raw;
      operation.output.hidden = !operation.raw;
    } else if (name === "context.compact_end") {
      if (live.contextOperation?.kind === "compact") live.contextOperation.title.textContent = "上下文已整理";
      live.contextOperation = null;
    } else if (name === "context.pop_start") createContextOperation(live, "pop");
    else if (name === "context.pop_end") {
      if (live.contextOperation?.kind === "pop") live.contextOperation.title.textContent = "旧上下文已释放";
      live.contextOperation = null;
    } else if (name === "context.error") {
      const operation = live.contextOperation || createContextOperation(live, "compact");
      operation.block.classList.add("is-error");
      operation.title.textContent = "上下文整理未完成";
      operation.raw = String(data?.message || "上下文维护失败");
      operation.output.textContent = operation.raw;
      operation.output.hidden = false;
      live.contextOperation = null;
    }
    contentAdded();
  }

  function appendRunNotice(live, message, error = false) {
    ensureLiveArticle(live);
    breakLiveText(live);
    const notice = document.createElement("div");
    notice.className = `run-notice${error ? " is-error" : ""}`;
    notice.append(makeIconSlot(error ? "circle-alert" : "circle-stop"));
    const text = document.createElement("span");
    text.textContent = String(message || "");
    notice.appendChild(text);
    live.blocks.appendChild(notice);
  }

  function markUnfinishedTools(live) {
    for (const tool of live.tools.values()) {
      if (tool.finished) continue;
      tool.finished = true;
      tool.finishedAt = performance.now();
      updateToolStatus(tool, "已中断", "circle-alert", "is-failure");
      updateToolSummary(tool);
      tool.card.classList.add("collapsed");
      tool.head.setAttribute("aria-expanded", "false");
    }
  }

  function setLiveEndpoint(live, providerId, model) {
    const values = [providerId, model].map((value) => String(value || "").trim()).filter(Boolean);
    live.providerId = String(providerId || "");
    live.model = String(model || "");
    if (!live.endpoint) return;
    live.endpoint.textContent = values.join(" / ");
    live.endpoint.hidden = !state.display?.show_mixed_model_endpoint || values.length === 0;
  }

  function consumeLiveQueue(live, data) {
    finalizeLiveReasoning(live);
    setLiveEndpoint(live, data?.provider_id, data?.model);
    if (live.headerStatus) live.headerStatus.textContent = "刚刚";
    if (live.meta) live.meta.textContent = "已完成";

    const ids = new Set((Array.isArray(data?.prompt_ids) ? data.prompt_ids : []).map(String));
    const consumed = state.queuedPrompts.filter((prompt) => ids.has(String(prompt?.id)));
    state.queuedPrompts = state.queuedPrompts.filter((prompt) => !ids.has(String(prompt?.id)));
    for (const prompt of consumed) {
      elements.timeline.appendChild(createUserMessage(prompt?.content || "", prompt?.submitted_at || new Date(), {
        turnId: live.turnId,
        runId: live.runId,
        followupId: prompt?.id
      }));
    }
    renderQueueTray();

    live.article = null;
    live.blocks = null;
    live.headerStatus = null;
    live.meta = null;
    live.endpoint = null;
    live.copyButton = null;
    live.currentText = null;
    live.assistantText = "";
    live.assistantReasoning = "";
    live.reasoning = null;
    live.reasoningParts = [];
    live.reasoningStarted = false;
    live.reasoningTitle = "";
    live.tools = new Map();
    live.questions = new Map();
    live.contextOperation = null;
    if (["normal", "plan", "chat"].includes(data?.mode)) setMode(data.mode, false);
    contentAdded();
  }

  function updateLocalTurnFromLive(live, terminalStatus, data) {
    const status = terminalStatus === "completed" ? "completed" : "interrupted";
    let turn = live.turnId ? state.turns.find((item) => String(item?.id) === String(live.turnId)) : null;
    if (!turn && live.userText) {
      turn = {
        id: live.turnId || `local-${live.runId}`,
        seq: state.turns.length ? Math.max(...state.turns.map((item) => asFiniteNumber(item?.seq))) + 1 : 1,
        status,
        active_context: true,
        user_content: live.userText,
        assistant_content: live.assistantText,
        assistant_reasoning: live.assistantReasoning || null,
        provider_id: data?.provider_id || live.providerId || null,
        model: data?.model || live.model || null,
        user_timestamp: new Date().toISOString(),
        assistant_timestamp: new Date().toISOString(),
        token_total: effectiveUsageTotal(data?.usage),
        token_usage_estimated: Boolean(data?.usage_estimated),
        question_exchanges: [],
        followups: [],
        assets: [...live.assets]
      };
      state.turns.push(turn);
    } else if (turn) {
      turn.status = status;
      if (live.assistantText.trim()) turn.assistant_content = live.assistantText;
      if (live.assistantReasoning.trim()) turn.assistant_reasoning = live.assistantReasoning;
      if (data?.provider_id || live.providerId) turn.provider_id = data?.provider_id || live.providerId;
      if (data?.model || live.model) turn.model = data?.model || live.model;
      if (live.assets.length) turn.assets = [...live.assets];
      turn.assistant_timestamp = new Date().toISOString();
      if (terminalStatus === "completed") {
        turn.token_total = effectiveUsageTotal(data?.usage);
        turn.token_usage_estimated = Boolean(data?.usage_estimated);
      }
    }
  }

  function finishLiveRun(kind, data) {
    const runId = String(data?.run_id || "");
    const live = state.live?.runId === runId ? state.live : establishRun(runId);
    if (!live || live.ended) return;
    live.ended = true;
    finalizeLiveReasoning(live);
    setLiveEndpoint(live, data?.provider_id, data?.model);
    state.terminalRunIds.add(runId);
    if (state.terminalRunIds.size > 30) state.terminalRunIds.delete(state.terminalRunIds.values().next().value);

    if (kind === "completed") {
      if (live.headerStatus) live.headerStatus.textContent = "刚刚";
      if (live.meta) {
        const total = effectiveUsageTotal(data?.usage);
        live.meta.textContent = total > 0 ? `${data?.usage_estimated ? "约 " : ""}${formatTokens(total)} tokens` : "已完成";
      }
    } else if (kind === "cancelled") {
      markUnfinishedTools(live);
      endPendingQuestions(live, "本轮已停止，无法再提交回答");
      appendRunNotice(live, "回复已停止");
      if (live.headerStatus) live.headerStatus.textContent = "已停止";
      if (live.meta) live.meta.textContent = "已停止";
    } else {
      markUnfinishedTools(live);
      endPendingQuestions(live, "本轮已结束，无法再提交回答");
      appendRunNotice(live, String(data?.message || "本轮运行失败"), true);
      if (live.headerStatus) live.headerStatus.textContent = "运行失败";
      if (live.meta) live.meta.textContent = "运行失败";
    }

    updateLocalTurnFromLive(live, kind, data);
    if (kind === "completed") {
      if (data?.context_tokens != null) state.context.tokens = Math.max(0, asFiniteNumber(data.context_tokens));
      state.context.window = data?.context_window == null ? state.context.window : Math.max(0, asFiniteNumber(data.context_window));
      const usage = data?.usage && typeof data.usage === "object" ? data.usage : null;
      if (usage) {
        state.usage.last_usage = usage;
        state.usage.last_conversation_usage = usage;
        state.usage.requests = asFiniteNumber(state.usage.requests) + 1;
        state.usage.prompt_tokens = asFiniteNumber(state.usage.prompt_tokens) + asFiniteNumber(usage.prompt_tokens);
        state.usage.completion_tokens = asFiniteNumber(state.usage.completion_tokens) + asFiniteNumber(usage.completion_tokens);
        state.usage.total_tokens = asFiniteNumber(state.usage.total_tokens) + effectiveUsageTotal(usage);
      }
    }
    state.activeRunId = null;
    state.replayRunId = null;
    state.replayCutoff = 0;
    state.cancellationRequested = false;
    state.pendingSubmission = null;
    state.live = null;
    updateContext();
    updateRuntimeUsage(data?.usage || null, Boolean(data?.usage_estimated));
    updateConversationChrome();
    updateControlState();
    contentAdded();
    window.requestAnimationFrame(() => {
      if (!state.blocked && !elements.settingsDrawer.classList.contains("open")) elements.composerInput.focus();
    });
    window.setTimeout(syncBootstrapSnapshot, 120);
  }

  function clearExternalSyncTimer() {
    if (!state.externalSyncTimer) return;
    window.clearTimeout(state.externalSyncTimer);
    state.externalSyncTimer = null;
  }

  function scheduleExternalSync() {
    clearExternalSyncTimer();
    if (!state.externalRunningTurnId || state.blocked) return;
    state.externalSyncTimer = window.setTimeout(() => {
      state.externalSyncTimer = null;
      syncBootstrapSnapshot();
    }, 1_000);
  }

  async function syncBootstrapSnapshot() {
    if (state.blocked) return;
    if (state.resyncing) {
      scheduleExternalSync();
      return;
    }
    try {
      const response = await apiRequest("/api/bootstrap");
      const snapshot = await response.json();
      if (state.bootId && snapshot?.boot_id && snapshot.boot_id !== state.bootId) {
        await loadBootstrap();
        return;
      }
      const snapshotActiveRunId = typeof snapshot?.active_run_id === "string" && snapshot.active_run_id ? snapshot.active_run_id : null;
      if (snapshotActiveRunId && snapshotActiveRunId !== state.activeRunId) {
        await loadBootstrap();
        return;
      }
      const previousExternalTurnId = state.externalRunningTurnId;
      const nextExternalTurnId = snapshotActiveRunId
        ? null
        : typeof snapshot?.running_turn_id === "string" && snapshot.running_turn_id
          ? snapshot.running_turn_id
          : null;
      const nextTurns = Array.isArray(snapshot?.turns)
        ? snapshot.turns.sort((a, b) => asFiniteNumber(a?.seq) - asFiniteNumber(b?.seq))
        : state.turns;
      const turnsChanged = JSON.stringify(nextTurns) !== JSON.stringify(state.turns);
      state.turns = nextTurns;
      state.queuedPrompts = Array.isArray(snapshot?.queued_prompts) ? snapshot.queued_prompts : state.queuedPrompts;
      state.models = Array.isArray(snapshot?.models) ? snapshot.models : state.models;
      state.display = snapshot?.display && typeof snapshot.display === "object" ? snapshot.display : state.display;
      state.context = snapshot?.context && typeof snapshot.context === "object" ? snapshot.context : state.context;
      state.usage = snapshot?.usage && typeof snapshot.usage === "object" ? snapshot.usage : state.usage;
      state.capabilities = snapshot?.capabilities && typeof snapshot.capabilities === "object" ? snapshot.capabilities : state.capabilities;
      state.version = snapshot?.version ?? state.version;
      state.externalRunningTurnId = nextExternalTurnId;
      state.externalQueueAvailable = Boolean(nextExternalTurnId && snapshot?.external_queue_available);
      elements.versionLabel.textContent = state.version ? `v${state.version}` : "--";
      if ((previousExternalTurnId || nextExternalTurnId) && (turnsChanged || previousExternalTurnId !== nextExternalTurnId)) {
        renderConversation();
      }
      renderModelMenu();
      renderQueueTray();
      updateCapabilities();
      updateContext();
      updateRuntimeUsage();
      updateConversationChrome();
      updateControlState();
    } catch (error) {
      if (error.status === 401) showBlockedState(true);
    } finally {
      scheduleExternalSync();
    }
  }

  async function ensureActiveTurnUser(turnId) {
    const live = state.live;
    if (!live || live.userRendered || !turnId) return;
    const existing = state.turns.find((turn) => String(turn?.id) === String(turnId));
    if (existing) {
      live.userText = String(existing.user_content || "");
      ensureLiveUser(live.userText, live.runId);
      return;
    }
    try {
      const response = await apiRequest("/api/bootstrap");
      const snapshot = await response.json();
      const turn = Array.isArray(snapshot?.turns) ? snapshot.turns.find((item) => String(item?.id) === String(turnId)) : null;
      if (!turn || state.live !== live) return;
      state.turns = snapshot.turns.sort((a, b) => asFiniteNumber(a?.seq) - asFiniteNumber(b?.seq));
      live.userText = String(turn.user_content || "");
      ensureLiveUser(live.userText, live.runId);
    } catch (_) {
      // The stream can continue; a later bootstrap will recover the user turn.
    }
  }

  function handleRunEvent(name, data) {
    const runId = String(data?.run_id || "");
    if (!runId) return;
    if (state.activeRunId && state.activeRunId !== runId) return;
    if (!state.activeRunId && name !== "run.started" && state.live?.runId !== runId) return;
    const live = establishRun(runId);
    if (!live) return;

    if (name === "run.started") {
      if (["normal", "plan", "chat"].includes(data?.mode)) setMode(data.mode, false);
      updateControlState();
    } else if (name === "turn.started") {
      live.turnId = String(data?.turn_id || "");
      removeRunningStatus(live.turnId);
      ensureActiveTurnUser(live.turnId);
    } else if (name === "assistant.delta") appendAssistantDelta(live, data?.delta);
    else if (name.startsWith("reasoning.")) handleReasoningEvent(name, live, data);
    else if (name === "queue.consumed") consumeLiveQueue(live, data);
    else if (name.startsWith("tool.")) handleToolEvent(name, live, data);
    else if (name === "question.requested") createQuestion(live, data);
    else if (name === "question.answered") {
      const question = live.questions.get(String(data?.question_id || ""));
      if (question) markQuestionAnswered(question, data?.answers);
    } else if (name.startsWith("context.")) handleContextEvent(name, live, data);
    else if (name === "run.completed") finishLiveRun("completed", data);
    else if (name === "run.cancelled") finishLiveRun("cancelled", data);
    else if (name === "run.failed") finishLiveRun("failed", data);
  }

  function eventShouldBeHandled(name, data, eventId) {
    if (name === "resync_required") {
      if (eventId > 0) state.lastEventId = eventId;
      return true;
    }
    if (eventId > 0 && eventId <= state.lastEventId) return false;
    if (eventId > 0) state.lastEventId = eventId;
    if (state.replayRunId && eventId > 0 && eventId <= state.replayCutoff) {
      if (!RUN_EVENTS.has(name)) return false;
      return String(data?.run_id || "") === state.replayRunId;
    }
    if (RUN_EVENTS.has(name) && state.activeRunId && String(data?.run_id || "") !== state.activeRunId) return false;
    return true;
  }

  function handleSseEvent(name, event) {
    let data;
    try {
      data = event.data ? JSON.parse(event.data) : {};
    } catch (_) {
      showToast("收到无法解析的事件，正在重新同步", "error");
      loadBootstrap();
      return;
    }
    const eventId = Math.max(0, asFiniteNumber(event.lastEventId));
    if (!eventShouldBeHandled(name, data, eventId)) return;
    if (name === "resync_required") {
      if (!state.resyncing) {
        state.resyncing = true;
        loadBootstrap().finally(() => {
          state.resyncing = false;
        });
      }
      return;
    }
    if (name === "queue.added") {
      const prompt = data?.prompt;
      if (prompt && !state.queuedPrompts.some((item) => String(item?.id) === String(prompt?.id))) {
        state.queuedPrompts.push(prompt);
        renderQueueTray();
      }
      return;
    }
    if (name === "queue.removed") {
      state.queuedPrompts = state.queuedPrompts.filter((prompt) => String(prompt?.id) !== String(data?.prompt_id));
      renderQueueTray();
      return;
    }
    if (name === "conversation.reset") {
      loadBootstrap();
      return;
    }
    handleRunEvent(name, data);
  }

  function closeEventSource() {
    if (state.eventSource) {
      state.eventSource.close();
      state.eventSource = null;
    }
    if (state.healthTimer) {
      window.clearTimeout(state.healthTimer);
      state.healthTimer = null;
    }
  }

  async function refineConnectionHealth(source) {
    if (state.eventSource !== source || source.readyState === EventSource.OPEN) return;
    try {
      const response = await fetch("/api/health", { cache: "no-store", credentials: "same-origin" });
      if (!response.ok) throw new Error("health check failed");
      if (state.eventSource === source && source.readyState !== EventSource.OPEN) setConnectionStatus("connecting");
    } catch (_) {
      if (state.eventSource === source && source.readyState !== EventSource.OPEN) setConnectionStatus("offline");
    }
  }

  function connectEventSource(after) {
    closeEventSource();
    if (state.blocked) return;
    const source = new EventSource(`/api/events?after=${encodeURIComponent(Math.max(0, asFiniteNumber(after)))}`);
    state.eventSource = source;
    source.onopen = () => {
      if (state.eventSource !== source) return;
      setConnectionStatus("online");
      if (state.healthTimer) window.clearTimeout(state.healthTimer);
      state.healthTimer = null;
    };
    source.onerror = () => {
      if (state.eventSource !== source) return;
      setConnectionStatus("connecting");
      if (state.healthTimer) window.clearTimeout(state.healthTimer);
      state.healthTimer = window.setTimeout(() => refineConnectionHealth(source), 1200);
    };
    for (const name of EVENT_NAMES) source.addEventListener(name, (event) => handleSseEvent(name, event));
  }

  function showBlockedState(unauthorized, message = "") {
    state.blocked = true;
    state.activeRunId = null;
    state.externalRunningTurnId = null;
    state.externalQueueAvailable = false;
    clearExternalSyncTimer();
    disposeLiveState(state.live);
    state.live = null;
    clearQuestionDock();
    closeEventSource();
    elements.loadingState.hidden = true;
    elements.timeline.hidden = true;
    elements.emptyState.hidden = true;
    elements.blockedState.hidden = false;
    elements.blockedTitle.textContent = unauthorized ? "登录 Miyu" : "无法载入 Miyu WebUI";
    elements.blockedMessage.textContent = unauthorized ? "输入访问密码以继续。" : message || "本地服务暂时无法访问";
    elements.loginForm.hidden = !unauthorized;
    elements.retryBootstrapButton.hidden = unauthorized;
    elements.loginError.textContent = "";
    elements.loginError.hidden = true;
    setLoginSubmitting(false);
    setConnectionStatus(unauthorized ? "blocked" : "offline");
    updateControlState();
    if (unauthorized) window.requestAnimationFrame(() => elements.loginPassword.focus());
  }

  function applyBootstrap(snapshot) {
    state.blocked = false;
    clearExternalSyncTimer();
    disposeLiveState(state.live);
    state.bootId = String(snapshot?.boot_id || "");
    state.latestEventId = Math.max(0, asFiniteNumber(snapshot?.latest_event_id));
    state.turns = Array.isArray(snapshot?.turns) ? snapshot.turns.sort((a, b) => asFiniteNumber(a?.seq) - asFiniteNumber(b?.seq)) : [];
    state.queuedPrompts = Array.isArray(snapshot?.queued_prompts) ? snapshot.queued_prompts : [];
    state.models = Array.isArray(snapshot?.models) ? snapshot.models : [];
    state.display = snapshot?.display && typeof snapshot.display === "object" ? snapshot.display : state.display;
    state.context = snapshot?.context && typeof snapshot.context === "object" ? snapshot.context : { tokens: 0, window: null };
    state.usage = snapshot?.usage && typeof snapshot.usage === "object" ? snapshot.usage : {};
    state.capabilities = snapshot?.capabilities && typeof snapshot.capabilities === "object" ? snapshot.capabilities : {};
    state.version = snapshot?.version ?? null;
    state.activeRunId = typeof snapshot?.active_run_id === "string" && snapshot.active_run_id ? snapshot.active_run_id : null;
    state.externalRunningTurnId = !state.activeRunId && typeof snapshot?.running_turn_id === "string" && snapshot.running_turn_id
      ? snapshot.running_turn_id
      : null;
    state.externalQueueAvailable = Boolean(state.externalRunningTurnId && snapshot?.external_queue_available);
    state.cancellationRequested = false;
    state.pendingSubmission = null;
    state.live = null;
    elements.loginForm.hidden = true;
    elements.retryBootstrapButton.hidden = false;
    elements.loginPassword.value = "";
    elements.loginError.textContent = "";
    elements.loginError.hidden = true;
    setLoginSubmitting(false);
    elements.versionLabel.textContent = state.version ? `v${state.version}` : "--";
    clearInlineError();
    renderConversation();
    renderModelMenu();
    renderQueueTray();
    updateCapabilities();
    updateContext();
    if (state.activeRunId) {
      const runningTurn = [...state.turns].reverse().find((turn) => turn?.status === "running");
      state.live = createLiveState(state.activeRunId, {
        turnId: runningTurn?.id || null,
        userText: runningTurn?.user_content || "",
        startedAt: runningTurn?.user_timestamp || new Date(),
        userRendered: Boolean(runningTurn)
      });
      state.replayRunId = state.activeRunId;
      state.replayCutoff = state.latestEventId;
      state.lastEventId = 0;
      connectEventSource(0);
    } else {
      state.replayRunId = null;
      state.replayCutoff = 0;
      state.lastEventId = state.latestEventId;
      connectEventSource(state.latestEventId);
    }
    setConnectionStatus("connecting");
    updateRuntimeUsage();
    updateConversationChrome();
    updateControlState();
    scheduleExternalSync();
  }

  async function loadBootstrap() {
    if (state.bootstrapPromise) return state.bootstrapPromise;
    state.bootstrapPromise = (async () => {
      clearExternalSyncTimer();
      closeEventSource();
      state.adminBusy = false;
      state.submitting = false;
      if (!state.turns.length && !state.live) {
        elements.loadingState.hidden = false;
        elements.blockedState.hidden = true;
        elements.emptyState.hidden = true;
        elements.timeline.hidden = true;
      }
      setConnectionStatus("connecting");
      updateControlState();
      try {
        const response = await apiRequest("/api/bootstrap");
        const snapshot = await response.json();
        applyBootstrap(snapshot);
      } catch (error) {
        showBlockedState(error.status === 401, error.message);
      }
    })();
    try {
      await state.bootstrapPromise;
    } finally {
      state.bootstrapPromise = null;
    }
  }

  function setLoginSubmitting(submitting) {
    state.loginSubmitting = Boolean(submitting);
    elements.loginPassword.disabled = state.loginSubmitting;
    elements.loginSubmit.disabled = state.loginSubmitting;
    elements.loginSubmit.classList.toggle("is-loading", state.loginSubmitting);
    elements.loginSubmitLabel.textContent = state.loginSubmitting ? "正在登录" : "登录";
    const icon = elements.loginSubmit.querySelector(".icon-slot");
    if (icon) icon.replaceChildren(createIcon(state.loginSubmitting ? "loader-circle" : "log-in"));
  }

  async function submitLogin() {
    if (state.loginSubmitting) return;
    const password = elements.loginPassword.value;
    if (!password) {
      elements.loginError.textContent = "请输入访问密码";
      elements.loginError.hidden = false;
      elements.loginPassword.focus();
      return;
    }
    elements.loginError.textContent = "";
    elements.loginError.hidden = true;
    setLoginSubmitting(true);
    try {
      await apiRequest("/api/auth/login", {
        method: "POST",
        body: JSON.stringify({ password })
      });
      elements.loginPassword.value = "";
      await loadBootstrap();
    } catch (error) {
      elements.loginError.textContent = error.status === 401 ? "密码不正确，请重试" : error.message || "登录失败";
      elements.loginError.hidden = false;
      window.requestAnimationFrame(() => {
        elements.loginPassword.focus();
        elements.loginPassword.select();
      });
    } finally {
      setLoginSubmitting(false);
    }
  }

  async function confirmModelSelection() {
    if (!(state.stagedModelKeys instanceof Set) || conversationRunning() || state.adminBusy || state.submitting) return;
    const selected = state.models.filter((model) => state.stagedModelKeys.has(modelKey(model)));
    if (selected.length === 0) {
      state.modelMenuError = "至少选择一个模型";
      updateModelMenuState();
      return;
    }
    state.modelSelectionSubmitting = true;
    state.adminBusy = true;
    state.modelMenuError = "";
    clearInlineError();
    updateControlState();
    let applied = false;
    try {
      const response = await apiRequest("/api/models/active", {
        method: "PUT",
        body: JSON.stringify({
          models: selected.map((model) => ({
            provider_id: String(model.provider_id || ""),
            model: String(model.model || "")
          }))
        })
      });
      const payload = await response.json();
      state.models = Array.isArray(payload?.models) ? payload.models : state.models;
      if (payload?.display && typeof payload.display === "object") state.display = payload.display;
      state.context = payload?.context && typeof payload.context === "object" ? payload.context : state.context;
      applied = true;
    } catch (error) {
      state.modelMenuError = error.message || "模型设置未保存";
      showInlineError(error.message);
      showToast(error.message, "error");
    } finally {
      state.adminBusy = false;
      state.modelSelectionSubmitting = false;
      if (applied) {
        closeModelMenu();
        renderModelMenu();
        updateContext();
        showToast("模型设置已更新");
      }
      updateControlState();
      if (applied) window.requestAnimationFrame(() => elements.modelButton.focus());
      else {
        updateModelMenuState();
        window.requestAnimationFrame(() => elements.modelMenu.querySelector(".model-confirm")?.focus());
      }
    }
  }

  async function submitTurn() {
    if (state.adminBusy || state.submitting || state.blocked) return;
    if (hasPendingQuestion() || (state.externalRunningTurnId && !state.externalQueueAvailable)) return;
    const queueing = conversationRunning();
    const content = elements.composerInput.value.trim();
    const count = countCharacters(content);
    if (!content) {
      elements.composerState.textContent = "消息不能为空";
      elements.composerState.classList.add("is-error");
      return;
    }
    if (count > MAX_CONTENT_CHARS) {
      elements.composerState.textContent = "消息不能超过 20,000 个字符";
      elements.composerState.classList.add("is-error");
      return;
    }
    state.submitting = true;
    if (!queueing) state.pendingSubmission = { content, mode: state.mode };
    clearInlineError();
    updateControlState();
    try {
      const response = await apiRequest(queueing ? "/api/queue" : "/api/turns", {
        method: "POST",
        body: JSON.stringify(queueing ? { content } : { content, mode: state.mode })
      });
      const payload = await response.json();
      const queuedPrompt = queueing ? payload : payload?.queued ? payload.prompt : null;
      if (queuedPrompt) {
        if (!state.queuedPrompts.some((prompt) => String(prompt?.id) === String(queuedPrompt?.id))) {
          state.queuedPrompts.push(queuedPrompt);
        }
        state.pendingSubmission = null;
        if (!queueing) {
          const fallbackRunId = String(payload?.run_id || "");
          state.externalRunningTurnId = fallbackRunId ? null : String(payload?.running_turn_id || "") || null;
          state.externalQueueAvailable = Boolean(state.externalRunningTurnId);
          if (fallbackRunId) state.activeRunId = fallbackRunId;
        }
        elements.composerInput.value = "";
        resizeComposer();
        renderQueueTray();
        if (!queueing && state.activeRunId) await loadBootstrap();
        else scheduleExternalSync();
        return;
      }
      const runId = String(payload?.run_id || "");
      if (!runId) throw new ApiError("服务未返回运行标识", response.status);
      if (state.terminalRunIds.has(runId)) {
        await loadBootstrap();
      } else {
        state.activeRunId = runId;
        const live = state.live?.runId === runId ? state.live : createLiveState(runId, { userText: content });
        live.userText = content;
        state.live = live;
        ensureLiveUser(content, runId);
        elements.composerInput.value = "";
        resizeComposer();
        updateRuntimeUsage();
        updateConversationChrome();
      }
    } catch (error) {
      if (!queueing) state.pendingSubmission = null;
      showInlineError(error.status === 409
        ? queueing ? "回复状态刚刚发生变化，正在同步" : "已有回复正在运行，正在同步当前状态"
        : error.message);
      showToast(error.status === 409 ? "回复状态已同步，请重新发送" : error.message, "error");
      if (error.status === 409) await loadBootstrap();
    } finally {
      state.submitting = false;
      updateControlState();
    }
  }

  async function cancelActiveRun() {
    const runId = state.activeRunId;
    if (!runId || state.cancellationRequested) return;
    state.cancellationRequested = true;
    updateConversationChrome();
    updateControlState();
    try {
      await apiRequest(`/api/runs/${encodeURIComponent(runId)}/cancel`, { method: "POST" });
    } catch (error) {
      state.cancellationRequested = false;
      showInlineError(error.message);
      showToast(error.message, "error");
      updateConversationChrome();
      updateControlState();
      if (error.status === 404 || error.status === 409) await loadBootstrap();
    }
  }

  function hasHistory() {
    return state.turns.length > 0 || Boolean(state.live?.userRendered) || Boolean(elements.timeline.querySelector(".user-message"));
  }

  function requestNewConversation() {
    closeSidebar();
    if (!hasHistory()) {
      elements.composerInput.focus();
      return;
    }
    if (conversationRunning() || state.adminBusy || state.submitting) return;
    if (typeof elements.resetDialog.showModal === "function") elements.resetDialog.showModal();
    else elements.resetDialog.setAttribute("open", "");
    window.requestAnimationFrame(() => elements.resetCancelButton.focus());
  }

  async function resetConversation() {
    if (conversationRunning() || state.adminBusy || state.submitting) return;
    state.adminBusy = true;
    elements.resetConfirmButton.disabled = true;
    elements.resetCancelButton.disabled = true;
    elements.resetConfirmButton.textContent = "正在清除";
    updateControlState();
    try {
      await apiRequest("/api/conversation/reset", { method: "POST" });
      if (elements.resetDialog.open) elements.resetDialog.close("confirmed");
      await loadBootstrap();
      elements.composerInput.focus();
    } catch (error) {
      showInlineError(error.message);
      showToast(error.message, "error");
      if (error.status === 409) await loadBootstrap();
    } finally {
      state.adminBusy = false;
      elements.resetConfirmButton.disabled = false;
      elements.resetCancelButton.disabled = false;
      elements.resetConfirmButton.textContent = "清除并新建";
      updateControlState();
    }
  }

  function handleGlobalKeydown(event) {
    if (elements.settingsDrawer.classList.contains("open") && event.key === "Tab") {
      const focusable = getFocusable(elements.settingsDrawer);
      if (!focusable.length) {
        event.preventDefault();
        elements.settingsDrawer.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    }
    if (event.key === "Escape") {
      if (elements.resetDialog.open) return;
      if (!elements.modelMenu.hidden) {
        event.preventDefault();
        closeModelMenu({ restoreFocus: true });
        return;
      }
      if (elements.settingsDrawer.classList.contains("open")) {
        event.preventDefault();
        closeSettings();
        return;
      }
      if (elements.sidebar.classList.contains("open")) {
        event.preventDefault();
        closeSidebar();
        state.sidebarOpener?.focus?.();
      }
    }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k" && !event.shiftKey && !event.altKey) {
      event.preventDefault();
      requestNewConversation();
    }
  }

  function bindEvents() {
    elements.mobileMenuButton.addEventListener("click", (event) => openSidebar(event.currentTarget));
    elements.sidebarClose.addEventListener("click", closeSidebar);
    elements.sidebarScrim.addEventListener("click", closeSidebar);
    elements.currentConversation.addEventListener("click", () => {
      closeSidebar();
      scrollToBottom({ force: true, smooth: true });
    });
    elements.settingsButton.addEventListener("click", (event) => openSettings(event.currentTarget));
    elements.topbarSettingsButton.addEventListener("click", (event) => openSettings(event.currentTarget));
    elements.settingsClose.addEventListener("click", () => closeSettings());
    elements.drawerScrim.addEventListener("click", () => closeSettings());
    elements.settingsNav.querySelectorAll("[data-settings-view]").forEach((button) => {
      button.addEventListener("click", () => setSettingsView(button.dataset.settingsView));
    });
    elements.addProviderButton.addEventListener("click", () => {
      if (!state.configDraft) return;
      state.configDraft.providers = Array.isArray(state.configDraft.providers) ? state.configDraft.providers : [];
      state.configDraft.providers.push(ensureProviderDefaults());
      state.providerSecretStates.push(false);
      refreshProviderSecretStates();
      markConfigDirty();
      renderConfigEditors();
      setSettingsView("providers");
      const cards = elements.providerEditor.querySelectorAll(".provider-card");
      const card = cards[cards.length - 1];
      if (card) {
        card.open = true;
        card.scrollIntoView({ block: "nearest" });
      }
    });
    elements.reloadConfigButton.addEventListener("click", loadConfigDraft);
    elements.saveConfigButton.addEventListener("click", saveConfigDraft);
    elements.applyAdvancedConfigButton.addEventListener("click", applyAdvancedConfig);
    elements.themeButton.addEventListener("click", () => setTheme(elements.body.dataset.theme === "graphite" ? "linen" : "graphite"));
    elements.sidebarThemeButton.addEventListener("click", () => setTheme(elements.body.dataset.theme === "graphite" ? "linen" : "graphite"));
    document.querySelectorAll("[data-theme-choice]").forEach((button) => button.addEventListener("click", () => setTheme(button.dataset.themeChoice)));
    elements.modeSwitch.querySelectorAll("[data-mode]").forEach((button) => button.addEventListener("click", () => setMode(button.dataset.mode)));
    elements.modelButton.addEventListener("click", (event) => {
      event.stopPropagation();
      if (elements.modelMenu.hidden) openModelMenu();
      else closeModelMenu({ restoreFocus: true });
    });
    elements.modelMenu.addEventListener("keydown", (event) => {
      const items = Array.from(elements.modelMenu.querySelectorAll("button:not(:disabled)"));
      const index = items.indexOf(document.activeElement);
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const direction = event.key === "ArrowDown" ? 1 : -1;
        items[(index + direction + items.length) % items.length]?.focus();
      } else if (event.key === "Home" || event.key === "End") {
        event.preventDefault();
        items[event.key === "Home" ? 0 : items.length - 1]?.focus();
      } else if (event.key === "Escape") {
        event.preventDefault();
        closeModelMenu({ restoreFocus: true });
      }
    });
    document.addEventListener("click", (event) => {
      if (!elements.modelMenu.hidden && !event.target.closest("#modelMenuWrap")) closeModelMenu();
    });
    elements.promptGrid.querySelectorAll("[data-prompt]").forEach((button) => {
      button.addEventListener("click", () => {
        if (elements.composerInput.disabled) return;
        elements.composerInput.value = button.dataset.prompt || "";
        resizeComposer();
        elements.composerInput.focus();
      });
    });
    elements.composerInput.addEventListener("input", resizeComposer);
    elements.composerInput.addEventListener("compositionstart", () => {
      state.composing = true;
    });
    elements.composerInput.addEventListener("compositionend", () => {
      state.composing = false;
    });
    elements.composerInput.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !event.shiftKey && !event.isComposing && !state.composing && event.keyCode !== 229) {
        event.preventDefault();
        if (!elements.sendButton.disabled) elements.composerForm.requestSubmit();
      }
    });
    elements.composerForm.addEventListener("submit", (event) => {
      event.preventDefault();
      submitTurn();
    });
    elements.stopButton.addEventListener("click", cancelActiveRun);
    elements.loginForm.addEventListener("submit", (event) => {
      event.preventDefault();
      submitLogin();
    });
    elements.newChatButton.addEventListener("click", requestNewConversation);
    elements.retryBootstrapButton.addEventListener("click", loadBootstrap);
    elements.resetConfirmButton.addEventListener("click", resetConversation);
    elements.chatScroll.addEventListener("scroll", () => {
      state.nearBottom = isNearBottom();
      if (state.nearBottom) {
        state.followOutput = true;
        elements.jumpBottomButton.hidden = true;
      } else if (!state.programmaticScroll) {
        state.followOutput = false;
        elements.jumpBottomButton.hidden = false;
      }
    }, { passive: true });
    elements.chatScroll.addEventListener("wheel", (event) => {
      if (event.deltaY < 0) state.followOutput = false;
    }, { passive: true });
    elements.chatScroll.addEventListener("touchmove", () => {
      if (!isNearBottom()) state.followOutput = false;
    }, { passive: true });
    elements.jumpBottomButton.addEventListener("click", () => scrollToBottom({ force: true, smooth: true }));
    window.addEventListener("resize", updateJumpButtonOffset, { passive: true });
    document.addEventListener("keydown", handleGlobalKeydown);
  }

  function initialize() {
    renderIconSlots();
    setTheme(safeStorageGet("miyu.web.theme") || "graphite", false);
    setMode(safeStorageGet("miyu.web.mode") || "normal", false);
    setSettingsView("interface");
    bindEvents();
    resizeComposer();
    updateSettingsControls();
    loadBootstrap();
  }

  initialize();
})();
