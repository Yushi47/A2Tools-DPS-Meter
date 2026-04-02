/**
 * Tauri 2 bridge adapter.
 * Creates window.javaBridge and window.dpsData compatibility objects
 * that translate the old JavaFX bridge calls to Tauri 2 IPC.
 */
(function () {
  "use strict";

  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const { open: shellOpen } = window.__TAURI__.opener;

  // --- Cached state ---
  let settingsCache = {};
  let settingsLoaded = false;
  let cachedDpsJson = null;      // latest DPS snapshot as JSON string
  let cachedPing = null;
  let cachedCaptureStatus = null;
  let cachedDetailsContext = null;

  // Load settings from Rust backend and merge with localStorage.
  // localStorage acts as the synchronous fallback for first reads before invoke resolves.
  invoke("get_settings").then((s) => {
    if (s && typeof s === "object") {
      // Merge backend settings into cache (backend is authoritative)
      settingsCache = s;
      settingsLoaded = true;
      // Also sync to localStorage so future reads before invoke are accurate
      for (const [k, v] of Object.entries(s)) {
        try { localStorage.setItem(k, v); } catch {}
      }
    }
  }).catch(() => { settingsLoaded = true; });

  // --- DPS data polling via events ---
  // The Rust backend emits "dps-update" every 500ms.
  // We cache the latest snapshot so getDpsData() can return it synchronously.
  listen("dps-update", (event) => {
    cachedDpsJson = JSON.stringify(event.payload);
    // Pre-fetch details context so it's ready when user clicks a player
    invoke("get_details_context").then((ctx) => {
      cachedDetailsContext = ctx;
    }).catch(() => {});
  });

  listen("ping-update", (event) => {
    cachedPing = event.payload;
    // Push directly to the app instance for immediate display update
    window._dpsApp?.updatePing?.(event.payload);
  });

  listen("capture-status-changed", (event) => {
    cachedCaptureStatus = event.payload;
  });

  listen("npcap-missing", () => {
    const msg = "Npcap is required for packet capture but is not installed.\n\nWould you like to download it now?";
    if (confirm(msg)) {
      shellOpen("https://npcap.com/#download");
    }
  });

  listen("combat-reset", () => {
    // Clear frontend state without re-invoking backend (already cleared by hotkey)
    cachedDpsJson = null;
    if (window._dpsApp) {
      window._dpsApp.refreshPending = false;
      window._dpsApp.lastJson = null;
      window._dpsApp.lastSnapshot = [];
      window._dpsApp._lastRenderedListSignature = "";
      window._dpsApp._lastRenderedRowsSummary = null;
      window._dpsApp._lastBattleTimeMs = null;
      window._dpsApp._battleTimeVisible = false;
      window._dpsApp.battleTime?.setVisible?.(false);
      window._dpsApp.meterUI?.onResetMeterUi?.();
    }
  });

  // ===== window.dpsData — polled by core.js every 100ms =====
  window.dpsData = {
    getDpsData() {
      return cachedDpsJson;
    },

    getDetailsContext() {
      // Return cached context synchronously; refresh in background
      invoke("get_details_context").then((ctx) => {
        cachedDetailsContext = ctx;
      }).catch(() => {});
      if (cachedDetailsContext) {
        return typeof cachedDetailsContext === "string"
          ? cachedDetailsContext
          : JSON.stringify(cachedDetailsContext);
      }
      return null;
    },

    async getTargetDetails(targetId, actorIdsJson) {
      try {
        const actorIds = actorIdsJson ? JSON.parse(actorIdsJson) : null;
        const result = await invoke("get_skill_details", {
          targetId: Number(targetId),
          actorIds: Array.isArray(actorIds) ? actorIds.map(Number) : null,
        });
        return JSON.stringify(result);
      } catch (e) {
        console.error("[A2Tools] getTargetDetails error:", e);
        return null;
      }
    },

    async getBattleDetail(/* actorId */) {
      try {
        const dps = cachedDpsJson ? JSON.parse(cachedDpsJson) : null;
        const targetId = Number(dps?.targetId) || 0;
        if (targetId <= 0) return null;
        const result = await invoke("get_skill_details", {
          targetId,
          actorIds: null,
        });
        return JSON.stringify(result);
      } catch {
        return null;
      }
    },

    getVersion() {
      return "2.0.1";
    },
  };

  // ===== window.javaBridge — called by various JS modules =====
  window.javaBridge = {
    // --- Settings ---
    getSetting(key) {
      return settingsCache[key] ?? localStorage.getItem(key);
    },
    setSetting(key, value) {
      settingsCache[key] = String(value);
      localStorage.setItem(key, String(value));
      invoke("update_settings", { key, value: String(value) }).catch(() => {});
      // Reload backend i18n data when language changes
      if (key === "dpsMeter.language") {
        invoke("set_language", { language: String(value) }).catch(() => {});
      }
    },
    clearAllSettings() {
      localStorage.clear();
      settingsCache = {};
      invoke("clear_settings").catch(() => {});
    },

    // --- DPS & Combat ---
    resetDps() {
      invoke("reset_combat").catch(() => {});
      cachedDpsJson = null;
      // Clear frontend state and skip the 1s grace period
      if (window._dpsApp) {
        window._dpsApp.refreshPending = false;
        window._dpsApp.lastJson = null;
        window._dpsApp.lastSnapshot = [];
        window._dpsApp._lastRenderedListSignature = "";
        window._dpsApp._lastRenderedRowsSummary = null;
        window._dpsApp._lastBattleTimeMs = null;
        window._dpsApp._battleTimeVisible = false;
        window._dpsApp.battleTime?.setVisible?.(false);
        window._dpsApp.meterUI?.onResetMeterUi?.();
      }
    },
    restartTargetSelection() {
      this.resetDps();
    },
    setTargetSelection(mode) {
      invoke("set_target_mode", { mode }).catch(() => {});
    },
    setCharacterName(name) {
      invoke("set_character_name", { name }).catch(() => {});
    },
    bindLocalActorId(actorId) {
      const id = Number(actorId);
      if (!Number.isFinite(id) || id <= 0) return;
      if (window._boundLocalActorId === id) return;
      window._boundLocalActorId = id;
      invoke("bind_local_actor_id", { actorId: id }).catch(() => {});
      // Also bind nickname if we can find it from any source
      const name =
        window._dpsApp?.USER_NAME ||
        document.querySelector(".characterNameInput")?.value?.trim() ||
        "";
      if (name) {
        this.bindLocalNickname(actorId, name);
      }
      // Force immediate meter refresh so the name shows right away
      invoke("get_dps_snapshot").then((dps) => {
        cachedDpsJson = JSON.stringify(dps);
      }).catch(() => {});
    },
    setLocalPlayerId(actorId) {
      this.bindLocalActorId(actorId);
    },
    bindLocalNickname(actorId, nickname) {
      const id = Number(actorId);
      if (!Number.isFinite(id) || id <= 0 || !nickname) return;
      const key = `${id}:${nickname}`;
      if (window._boundLocalNickname === key) return;
      window._boundLocalNickname = key;
      invoke("bind_local_nickname", { actorId: id, nickname }).catch(() => {});
    },
    setAllTargetsWindowMs() {},
    setTargetSelectionWindowMs() {},
    setTrainSelectionMode() {},

    // --- Window ---
    moveWindow() {
      // No-op — native drag handles window movement via start_drag command.
      // This also effectively disables core.js's bindDragToMoveWindow since
      // it checks `if (!window.javaBridge) return` on mousemove — the function
      // exists but does nothing, so the JS drag system runs but has no effect.
      // The ghost panel logic is tied to hasDragMoved which requires >3px of
      // mouse movement with isDragging=true. We prevent this below.
    },
    exitApp() {
      invoke("quit_app").catch(() => {});
    },

    // --- Browser ---
    openBrowser(url) {
      invoke("open_url", { url }).catch(() => {});
    },

    // --- Ping ---
    getPingMs() {
      return cachedPing;
    },

    // --- Connection Info ---
    getConnectionInfo() {
      return cachedCaptureStatus ? JSON.stringify(cachedCaptureStatus) : null;
    },
    getLastParsedAtMs() {
      return 0;
    },
    getAvailableDevices() {
      // If cache is empty, do a blocking-ish fetch by returning what we have
      // and immediately triggering a refresh. The settings panel re-populates
      // the dropdown on each open, so the second open will have data.
      if (!window._cachedDevices) {
        // Trigger fetch — will be ready next time
        invoke("get_available_devices").then((d) => { window._cachedDevices = d; }).catch(() => {});
        return "[]";
      }
      // Keep refreshing in background
      invoke("get_available_devices").then((d) => { window._cachedDevices = d; }).catch(() => {});
      return JSON.stringify(window._cachedDevices);
    },
    setManualDevice(device) {
      invoke("set_manual_device", { device: device || "" }).catch(() => {});
    },
    resetAutoDetection() {
      invoke("reset_auto_detection").catch(() => {});
    },

    // --- Screenshots ---
    captureScreenshotToClipboard(x, y, w, h) {
      try {
        invoke("capture_screenshot", {
          x: Math.round(x), y: Math.round(y),
          width: Math.round(w), height: Math.round(h),
        }).catch(() => {});
        return true;
      } catch {
        return false;
      }
    },
    captureScreenshotToFile() { return false; },
    chooseScreenshotFolder() { return null; },
    getDefaultScreenshotFolder() { return ""; },

    // --- Hotkeys ---
    getCurrentHotKey() {
      return this.getSetting("dpsMeter.hotkey") || "Ctrl+Alt+Shift+R";
    },
    getCurrentToggleWindowHotKey() {
      return this.getSetting("dpsMeter.toggleWindowHotkey") || "Ctrl+Alt+Up";
    },
    setHotkey(mods, vk) {
      const label = this._buildHotkeyLabel(mods, vk);
      this.setSetting("dpsMeter.hotkey", label);
    },
    setToggleWindowHotkey(mods, vk) {
      const label = this._buildHotkeyLabel(mods, vk);
      this.setSetting("dpsMeter.toggleWindowHotkey", label);
    },
    _buildHotkeyLabel(mods, vk) {
      const parts = [];
      if (mods & 0x02) parts.push("Ctrl");
      if (mods & 0x01) parts.push("Alt");
      if (mods & 0x04) parts.push("Shift");
      // Map common VK codes to names
      const vkNames = {
        0x08: "Backspace", 0x09: "Tab", 0x0D: "Enter", 0x1B: "Esc",
        0x20: "Space", 0x21: "PageUp", 0x22: "PageDown", 0x23: "End",
        0x24: "Home", 0x25: "Left", 0x26: "Up", 0x27: "Right", 0x28: "Down",
        0x2D: "Insert", 0x2E: "Delete",
        0x70: "F1", 0x71: "F2", 0x72: "F3", 0x73: "F4", 0x74: "F5",
        0x75: "F6", 0x76: "F7", 0x77: "F8", 0x78: "F9", 0x79: "F10",
        0x7A: "F11", 0x7B: "F12",
      };
      const keyName = vkNames[vk] || String.fromCharCode(vk);
      parts.push(keyName);
      return parts.join("+");
    },

    // --- Feature flags ---
    isRunningFromIde() { return false; },
    getParsingBacklog() { return 0; },
    isCaptureSuspended() { return false; },
    suspendCapture() {},
    setBossLogsEnabled() {},
    setAutoHideMeter(enabled) {
      invoke("update_settings", { key: "dpsMeter.autoHideMeter", value: String(enabled) }).catch(() => {});
    },
    setSaveRawPackets(enabled) {
      invoke("set_packet_logging", { enabled: !!enabled }).catch(() => {});
    },
    setDebugLoggingEnabled(enabled) {
      invoke("set_debug_logging", { enabled: !!enabled }).catch(() => {});
    },
    getAion2WindowTitle() { return window._cachedAion2Title ?? null; },
    logDebug() {},

    getFightHistory() {
      // Trigger async refresh for next call
      invoke("get_fight_history").then((h) => { window._cachedFightHistory = h; }).catch(() => {});
      if (window._cachedFightHistory) {
        return JSON.stringify(window._cachedFightHistory);
      }
      // First call: block briefly with synchronous fallback
      return "[]";
    },

    getFightDetails(id) {
      // Async — returns a promise
      return invoke("load_fight", { id }).then((r) => JSON.stringify(r)).catch(() => null);
    },

    deleteFight(id) {
      invoke("delete_fight", { id }).catch(() => {});
      return true;
    },

    // --- Resources ---
    readResource(path) {
      // Load resource files synchronously via XMLHttpRequest.
      // Try multiple path prefixes since files may be at root or under /src/data/.
      const candidates = [path, "/src/data" + path, "/src" + path];
      for (const url of candidates) {
        try {
          const xhr = new XMLHttpRequest();
          xhr.open("GET", url, false); // synchronous
          xhr.send();
          if (xhr.status === 200 && xhr.responseText) {
            return xhr.responseText;
          }
        } catch {
          // try next
        }
      }
      return null;
    },
    readCachedIcon(key) {
      // Synchronous read from Rust via cached map
      if (!key) return null;
      if (window._iconCache?.[key] !== undefined) return window._iconCache[key];
      // Trigger async load for next call
      invoke("read_cached_icon", { key }).then((data) => {
        if (!window._iconCache) window._iconCache = {};
        window._iconCache[key] = data ?? null;
      }).catch(() => {});
      return null;
    },
    writeCachedIcon(key, data) {
      if (!key || !data) return;
      if (!window._iconCache) window._iconCache = {};
      window._iconCache[key] = data;
      invoke("write_cached_icon", { key, data }).catch(() => {});
    },

    // --- Fetch ---
    fetchUrlAsync(url, callbackId) {
      // checkRelease.js registers a callback via window._fetchUrlCallback(id, raw)
      fetch(url)
        .then((r) => r.text())
        .then((text) => {
          if (callbackId && typeof window._fetchUrlCallback === "function") {
            window._fetchUrlCallback(callbackId, text);
          }
        })
        .catch(() => {
          if (callbackId && typeof window._fetchUrlCallback === "function") {
            window._fetchUrlCallback(callbackId, JSON.stringify({ error: "fetch failed" }));
          }
        });
    },

    // --- Admin ---
    isAdmin() {
      return invoke("is_admin");
    },
  };


  // Poll AION2 window title and capture status from Rust backend
  const pollStatus = () => {
    invoke("get_aion2_window_title")
      .then((title) => { window._cachedAion2Title = title ?? null; })
      .catch(() => { window._cachedAion2Title = null; });

    invoke("get_capture_status")
      .then((status) => { cachedCaptureStatus = status; })
      .catch(() => {});
  };
  pollStatus();
  setInterval(pollStatus, 3000);

  // ===== Dynamic window resizing =====
  const PANEL_WIDTH = 1540;
  const PANEL_HEIGHT = 820;
  const TOOLTIP_WIDTH = 800;
  let lastSizeKey = "";

  const updateWindowSize = () => {
    if (resizeActive) return; // Don't fight the user while they're resizing
    const fullPanel = !!(
      document.querySelector(".settingsPanel.isOpen") ||
      document.querySelector(".detailsPanel.open") ||
      document.querySelector(".historyPanel.isOpen") ||
      document.querySelector(".historyPanel.open")
    );
    const tooltipOnly = !fullPanel && !!document.querySelector(".hoverDetailsTooltip.isVisible");

    // Measure meter width (may be resized by user via drag handle) and height
    const meter = document.querySelector(".meter");
    let contentW = 396;
    let contentH = 300;
    if (meter) {
      contentW = Math.ceil(meter.offsetWidth) + 16;
      const meterH = Math.max(meter.offsetHeight, meter.scrollHeight);
      const ping = document.querySelector(".pingDisplay");
      const pingH = ping ? ping.offsetHeight + 8 : 0;
      contentH = Math.ceil(meterH + pingH) + 10;
    }

    const w = fullPanel ? PANEL_WIDTH : tooltipOnly ? TOOLTIP_WIDTH : contentW;
    const h = fullPanel ? Math.max(PANEL_HEIGHT, contentH) : contentH;
    const sizeKey = `${w}x${h}`;
    if (sizeKey === lastSizeKey) return;
    lastSizeKey = sizeKey;
    invoke("resize_window", { width: w, height: h }).catch(() => {});
  };

  // Watch all class changes on the container to catch panel open/close instantly
  const containerObserver = new MutationObserver(() => updateWindowSize());
  const startObserving = () => {
    const container = document.querySelector(".container");
    if (container) {
      containerObserver.observe(container, {
        attributes: true,
        attributeFilter: ["class"],
        subtree: true,
      });
    }
  };
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", startObserving);
  } else {
    startObserving();
  }
  // Fallback poll
  setInterval(updateWindowSize, 500);

  // ===== Window dragging =====
  // Core.js's JS-based drag (moveWindow + screenX/Y) is too slow over IPC.
  // Use native Win32 drag via WM_NCLBUTTONDOWN — instant, OS-handled, zero latency.
  // Intercept mousedown on .meter in capture phase before core.js sees it.
  // Intercept mousedown to:
  // 1. Start native drag on header/footer/empty meter areas
  // 2. Block core.js's JS drag system everywhere (it doesn't work in Tauri)
  // 3. Let clicks on interactive elements (.item, buttons, panels) pass through
  document.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    const target = e.target?.nodeType === Node.TEXT_NODE ? e.target.parentElement : e.target;
    // Let interactive elements handle their own clicks normally
    if (target?.closest?.("button, input, select, textarea, a, [data-no-drag]")) return;
    if (target?.closest?.(".headerBtn, .footerBtn, .bossIcon, .resizeHandle")) return;
    // Let panel internals work (close buttons, dropdowns, etc.)
    if (target?.closest?.(".settingsPanel, .historyPanel, .detailsBody, .detailsHeader, .detailsSettingsMenu")) return;
    // Let meter bar item clicks pass through for details/hover
    if (target?.closest?.(".item")) return;
    // Everything else in .meter: native drag
    if (target?.closest?.(".meter")) {
      e.stopImmediatePropagation();
      invoke("start_drag");
    }
  }, { capture: true });

  // Pre-fetch device list and fight history so they're ready when panels open
  invoke("get_available_devices").then((d) => { window._cachedDevices = d; }).catch(() => {});
  invoke("get_fight_history").then((h) => { window._cachedFightHistory = h; }).catch(() => {});
  // Refresh fight history periodically (picks up auto-saved fights)
  setInterval(() => {
    invoke("get_fight_history").then((h) => { window._cachedFightHistory = h; }).catch(() => {});
  }, 10000);

  // ===== Resize handle: expand viewport while dragging =====
  let resizeActive = false;
  const expandViewport = () => {
    resizeActive = true;
    const screenW = window.screen.availWidth || 1920;
    const screenH = window.screen.availHeight || 1080;
    invoke("resize_window", { width: Math.min(screenW, 2000), height: Math.min(screenH, 1200) }).catch(() => {});
  };
  const shrinkViewport = () => {
    if (resizeActive) {
      resizeActive = false;
      lastSizeKey = "";
    }
  };
  // Expand during resize handle drag
  document.addEventListener("mousedown", (e) => {
    if (e.target?.closest?.(".resizeHandle")) expandViewport();
  }, { capture: true });
  document.addEventListener("mouseup", shrinkViewport);

  // Startup diagnostics
  invoke("debug_status").then((s) => {
    console.log("[A2Tools] Debug status:", JSON.stringify(s));
    if (!s.isAdmin) {
      console.warn("[A2Tools] NOT RUNNING AS ADMIN — packet capture will not work!");
    }
  }).catch((e) => console.error("[A2Tools] debug_status failed:", e));

  console.log("[A2Tools] Tauri bridge adapter loaded (javaBridge + dpsData)");
})();
