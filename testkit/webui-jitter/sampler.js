// 页面内逐帧采样:每个动画帧记一条
//   [t, scrollTop, scrollHeight, clientHeight, streaming, following, bubbleGen]
// following = 「回到底部」按钮是否隐藏(即 followOutput);bubbleGen = 流式气泡
// 元素换过几次身份(时间线被整体重建时会换)。另外记下 scroll 事件与程序
// 调用 scrollTo 的时刻,取证「视口是谁挪的」。
// 两个探针(WebKitGTK / Chromium)注入同一段,量出来的东西才可比。
(() => {
  const scroller = document.getElementById("chatScroll");
  const jump = document.getElementById("jumpBottomButton");
  window.__jit = { samples: [], events: [], done: false };
  let generation = 0;
  let lastBubble = null;
  const originalScrollTo = Element.prototype.scrollTo;
  Element.prototype.scrollTo = function (...args) {
    if (this === scroller) {
      const target = typeof args[0] === "object" ? args[0].top : args[1];
      window.__jit.events.push(["scrollTo", Math.round(performance.now()), Math.round(target ?? -1),
        Math.round(this.scrollTop), this.scrollHeight]);
    }
    return originalScrollTo.apply(this, args);
  };
  // 直接赋值 scrollTop 的调用也要看见(时间线整体重建那条路走的是它)。
  const topDescriptor = Object.getOwnPropertyDescriptor(Element.prototype, "scrollTop");
  Object.defineProperty(Element.prototype, "scrollTop", {
    configurable: true,
    get() { return topDescriptor.get.call(this); },
    set(value) {
      if (this === scroller) {
        window.__jit.events.push(["setTop", Math.round(performance.now()), Math.round(value),
          Math.round(topDescriptor.get.call(this)), this.scrollHeight]);
      }
      return topDescriptor.set.call(this, value);
    },
  });
  // DOM 变动摘要:视口无故归零时,看那一帧前后动了什么。
  const observer = new MutationObserver((records) => {
    const now = Math.round(performance.now());
    for (const record of records) {
      const target = record.target;
      const label = target.nodeType === 1
        ? `${target.tagName.toLowerCase()}.${String(target.className || "").split(" ")[0]}`
        : target.nodeName;
      window.__jit.events.push(["mut", now, record.type, label, record.addedNodes.length,
        record.removedNodes.length, record.attributeName || ""]);
    }
  });
  observer.observe(scroller, { childList: true, subtree: true, attributes: true,
    attributeFilter: ["class", "style", "hidden"] });
  scroller.addEventListener("scroll", () => {
    window.__jit.events.push(["scroll", Math.round(performance.now()), Math.round(scroller.scrollTop),
      scroller.scrollHeight, jump && !jump.hidden ? "jump-shown" : ""]);
  });
  function tick(now) {
    const bubble = document.querySelector(".assistant-message.is-streaming");
    if (bubble && bubble !== lastBubble) {
      generation += 1;
      lastBubble = bubble;
    }
    window.__jit.samples.push([
      Math.round(now),
      Math.round(scroller.scrollTop),
      scroller.scrollHeight,
      scroller.clientHeight,
      bubble ? 1 : 0,
      jump && jump.hidden ? 1 : 0,
      generation,
    ]);
    if (!window.__jit.done) window.requestAnimationFrame(tick);
  }
  window.requestAnimationFrame(tick);
  return "armed";
})();
