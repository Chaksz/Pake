(function () {
  const invoke = window.__TAURI__?.core?.invoke;
  const injectStyle = window.__PAKE_INJECT_STYLE__;
  if (typeof invoke !== "function" || typeof injectStyle !== "function") {
    console.warn("[Pake] Adblock cosmetic IPC or style injection is unavailable.");
    return;
  }

  const MAX_TRACKED_TOKENS = 4096;
  const MAX_BATCH_SIZE = 128;
  const MAX_TOKEN_LENGTH = 256;
  const STYLE_ID = "pake-adblock-generic-matched";
  const discoveredClasses = new Set();
  const discoveredIds = new Set();
  const pendingClasses = [];
  const pendingIds = [];
  const appliedSelectors = new Set();
  let totalTokens = 0;
  let flushScheduled = false;
  let flushing = false;
  let dynamicStyle;
  let warnedAboutLimit = false;

  function addToken(value, discovered, pending) {
    if (
      !value ||
      value.length > MAX_TOKEN_LENGTH ||
      /\s/.test(value) ||
      discovered.has(value)
    ) {
      return;
    }
    if (totalTokens >= MAX_TRACKED_TOKENS) {
      if (!warnedAboutLimit) {
        warnedAboutLimit = true;
        console.warn("[Pake] Adblock cosmetic DOM token limit reached.");
      }
      return;
    }

    discovered.add(value);
    pending.push(value);
    totalTokens += 1;
  }

  function collectElement(element) {
    if (!(element instanceof Element)) return;
    for (const className of element.classList) {
      addToken(className, discoveredClasses, pendingClasses);
    }
    addToken(element.id, discoveredIds, pendingIds);
  }

  function scan(node) {
    if (!(node instanceof Element)) return;
    collectElement(node);
    for (const element of node.querySelectorAll("[class], [id]")) {
      collectElement(element);
    }
  }

  function addMatchedSelectors(selectors) {
    let changed = false;
    for (const selector of selectors) {
      if (typeof selector === "string" && !appliedSelectors.has(selector)) {
        appliedSelectors.add(selector);
        changed = true;
      }
    }
    if (!changed) return;

    const css = Array.from(appliedSelectors)
      .map((selector) => `${selector} { display: none !important; }`)
      .join("\n");
    try {
      if (!dynamicStyle) {
        dynamicStyle = injectStyle(css, STYLE_ID);
      } else if (typeof dynamicStyle.replaceSync === "function") {
        dynamicStyle.replaceSync(css);
      } else {
        dynamicStyle.textContent = css;
      }
    } catch (error) {
      console.warn("[Pake] Failed to apply generic adblock selectors:", error);
    }
  }

  async function flush() {
    if (flushing) return;
    flushing = true;
    flushScheduled = false;
    try {
      while (pendingClasses.length || pendingIds.length) {
        const classes = pendingClasses.splice(0, MAX_BATCH_SIZE);
        const ids = pendingIds.splice(0, MAX_BATCH_SIZE - classes.length);
        try {
          const selectors = await invoke(
            "adblock_hidden_class_id_selectors",
            { classes, ids },
          );
          addMatchedSelectors(selectors);
        } catch (error) {
          console.warn("[Pake] Generic adblock rule lookup failed:", error);
        }
      }
    } finally {
      flushing = false;
      scheduleFlush();
    }
  }

  function scheduleFlush() {
    if (flushScheduled || flushing) return;
    if (!pendingClasses.length && !pendingIds.length) return;
    flushScheduled = true;
    setTimeout(() => {
      void flush();
    }, 0);
  }

  function start() {
    const root = document.documentElement;
    if (!root) {
      document.addEventListener("DOMContentLoaded", start, { once: true });
      return;
    }

    const observer = new MutationObserver((records) => {
      for (const record of records) {
        if (record.type === "attributes") {
          collectElement(record.target);
        } else {
          for (const node of record.addedNodes) {
            scan(node);
          }
        }
      }
      scheduleFlush();
    });
    observer.observe(root, {
      attributes: true,
      attributeFilter: ["class", "id"],
      childList: true,
      subtree: true,
    });

    scan(root);
    scheduleFlush();
  }

  start();
})();
