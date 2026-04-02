(() => {
  const CDN_MANIFEST = "https://cdn.a2tools.app/latest-v2.json";
  const RELEASES_URL = "https://github.com/taengu/A2Tools-DPS-Meter/releases";
  const FETCH_TIMEOUT = 6000;
  const START_DELAY = 800,
    RETRY = 500,
    LIMIT = 5;

  const parseVersion = (v) => {
    const cleaned = String(v || "").trim().replace(/^v/i, "");
    const [base, prerelease] = cleaned.split("-", 2);
    const [a = 0, b = 0, c = 0] = String(base || "")
      .split(".")
      .map(Number);
    return {
      base,
      prerelease: Boolean(prerelease),
      value: a * 1e6 + b * 1e3 + c,
    };
  };

  let modal, textEl, actionsEl, progressSection, progressBar, progressText, statusText;
  let once = false;
  let msiUrl = null;
  let releaseUrl = null;

  const $ = (sel) => document.querySelector(sel);

  const showActions = () => {
    actionsEl.style.display = "flex";
    progressSection.style.display = "none";
    statusText.style.display = "none";
  };

  const showProgress = () => {
    actionsEl.style.display = "none";
    progressSection.style.display = "block";
    statusText.style.display = "none";
    progressBar.style.width = "0%";
    progressText.textContent = "0%";
  };

  const showStatus = (msg, isError) => {
    actionsEl.style.display = "none";
    progressSection.style.display = "none";
    statusText.style.display = "block";
    statusText.textContent = msg;
    statusText.style.color = isError ? "#ff5252" : "#4caf50";
  };

  const startDownload = () => {
    if (!msiUrl || !window.javaBridge?.startUpdate) return;
    showProgress();
    window.javaBridge.startUpdate(msiUrl);
  };

  // Callbacks invoked from Kotlin via executeScript
  window.onDownloadProgress = (percent) => {
    if (progressBar && progressText) {
      progressBar.style.width = percent + "%";
      progressText.textContent = percent + "%";
    }
  };

  window.onDownloadComplete = () => {
    const msg = window.i18n?.t?.("update.installing") || "Installing update...";
    showStatus(msg, false);
  };

  window.onDownloadError = () => {
    const msg = window.i18n?.t?.("update.downloadError") || "Download failed. Please try again or install manually.";
    showStatus(msg, true);
    // Show actions again after a moment so user can retry or go manual
    setTimeout(showActions, 2000);
  };

  window.onDownloadCancelled = () => {
    showActions();
  };

  const start = () =>
    setTimeout(async () => {
      if (once) return;
      once = true;

      modal = $("#updateModal");
      textEl = $("#updateModalText");
      actionsEl = $("#updateModalActions");
      progressSection = $("#updateProgress");
      progressBar = $("#updateProgressBar");
      progressText = $("#updateProgressText");
      statusText = $("#updateStatusText");

      // Install Now — download and install MSI directly
      $(".updateInstallBtn").onclick = startDownload;

      // Release Notes — open release page in browser
      document.querySelectorAll(".updateNotesBtn").forEach((btn) => {
        btn.onclick = () => {
          if (releaseUrl) window.javaBridge?.openBrowser?.(releaseUrl);
        };
      });

      // Cancel — abort in-progress download
      $(".updateCancelBtn").onclick = () => {
        window.javaBridge?.cancelUpdate?.();
      };

      // Manual Install — open MSI URL or releases page in browser
      $(".updateManualBtn").onclick = () => {
        window.javaBridge?.openBrowser?.(msiUrl || RELEASES_URL);
      };

      // Update Later — dismiss
      $(".updateLaterBtn").onclick = () => {
        modal.classList.remove("isOpen");
      };

      // Wait for bridges
      for (
        let i = 0;
        i < LIMIT && !(window.dpsData?.getVersion && window.javaBridge?.openBrowser);
        i++
      ) {
        await new Promise((r) => setTimeout(r, RETRY));
      }
      if (!(window.dpsData?.getVersion && window.javaBridge?.openBrowser)) {
        return;
      }
      if (window.javaBridge?.isRunningViaGradle?.()) return;

      const rawCurrent = String(window.dpsData.getVersion() || "").trim();
      const current = rawCurrent.startsWith("v") ? rawCurrent : "v" + rawCurrent;

      let _cbId = 0;
      const _cbMap = new Map();
      window._fetchUrlCallback = (id, raw) => {
        const resolve = _cbMap.get(id);
        if (resolve) { _cbMap.delete(id); resolve(raw); }
      };
      const bridgeFetch = (url) => {
        if (!window.javaBridge?.fetchUrlAsync) throw new Error("fetchUrlAsync bridge not available");
        return new Promise((resolve, reject) => {
          const id = "cb" + (++_cbId);
          _cbMap.set(id, resolve);
          window.javaBridge.fetchUrlAsync(url, id);
          setTimeout(() => { if (_cbMap.has(id)) { _cbMap.delete(id); reject(new Error("timeout")); } }, FETCH_TIMEOUT + 1000);
        }).then((raw) => {
          const obj = JSON.parse(raw);
          if (obj.error) throw new Error(obj.error);
          return obj;
        });
      };

      let result;
      try {
        const m = await bridgeFetch(CDN_MANIFEST);
        const v = m.version?.startsWith("v") ? m.version : "v" + m.version;
        result = { latest: v, msi: m.msiUrl || null, notes: m.releaseNotesUrl || RELEASES_URL };
      } catch {
        return;
      }

      const latest = result.latest;
      const latestInfo = parseVersion(latest);

      const currentInfo = parseVersion(current);
      const hasUpdate =
        latestInfo.value > currentInfo.value ||
        (latestInfo.value === currentInfo.value &&
          currentInfo.prerelease &&
          !latestInfo.prerelease);
      if (!hasUpdate) return;

      msiUrl = result.msi;
      releaseUrl = RELEASES_URL;

      // Show/hide install button based on whether MSI is available
      const installBtn = $(".updateInstallBtn");
      if (msiUrl && window.javaBridge?.startUpdate) {
        installBtn.style.display = "block";
      } else {
        installBtn.style.display = "none";
      }

      const fallback = `A new update is available!\n\nCurrent version: ${current}\nLatest version: ${latest}`;
      textEl.textContent =
        window.i18n?.format?.("update.text", { current, latest }, fallback) || fallback;
      showActions();
      modal.classList.add("isOpen");
    }, START_DELAY);

  window.ReleaseChecker = { start };
})();
