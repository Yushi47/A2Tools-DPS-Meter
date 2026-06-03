const createHistoryUI = ({ onOpenFight } = {}) => {
  const panel = document.querySelector(".historyPanel");
  if (!panel) return null;

  const listEl = panel.querySelector(".historyList");
  const closeBtn = panel.querySelector(".historyClose");
  const emptyEl = panel.querySelector(".historyEmpty");
  const trainToggleBtn = panel.querySelector(".historyTrainToggle");
  const deleteToggleBtn = panel.querySelector(".historyDeleteToggle");
  const viewToggleEl = panel.querySelector(".historyViewToggle");
  const viewBtns = viewToggleEl ? [...viewToggleEl.querySelectorAll(".historyViewBtn")] : [];
  const filterBossEl = panel.querySelector(".historyFilterBoss");
  const filterPlayerEl = panel.querySelector(".historyFilterPlayer");
  const filterPlayerTrigger = filterPlayerEl?.querySelector(".historyClassDropdownTrigger");
  const filterPlayerLabel = filterPlayerEl?.querySelector(".historyClassDropdownLabel");
  const filterPlayerMenu = filterPlayerEl?.querySelector(".historyClassDropdownMenu");
  const filterDateEl = panel.querySelector(".historyFilterDate");

  // Map from the Korean class name stored in fight records → stable enum key used for i18n
  const JOB_KEY_MAP = {
    "검성": "GLADIATOR",
    "수호성": "TEMPLAR",
    "궁성": "RANGER",
    "살성": "ASSASSIN",
    "마도성": "SORCERER",
    "치유성": "CLERIC",
    "정령성": "ELEMENTALIST",
    "호법성": "CHANTER",
  };

  let showDeleteMode = false;
  let filterBoss = "";
  let filterPlayer = "";
  let filterDate = "";
  let classDropdownOpen = false;

  // View mode: "grouped" (each boss its own collapsible section) or "list" (flat chronological).
  const VIEW_KEY = "historyViewMode";
  let viewMode = (() => {
    try { return localStorage.getItem(VIEW_KEY) === "list" ? "list" : "grouped"; } catch { return "grouped"; }
  })();
  const expandedGroups = new Set();

  const syncDeleteToggle = () => {
    if (!deleteToggleBtn) return;
    deleteToggleBtn.classList.toggle("active", showDeleteMode);
    panel.classList.toggle("deleteMode", showDeleteMode);
  };

  const syncViewToggle = () => {
    viewBtns.forEach((b) => {
      b.classList.toggle("active", b.dataset.view === viewMode);
      b.title = b.dataset.view === "list"
        ? t("history.viewList", "List view")
        : t("history.viewGrouped", "Group by boss");
    });
    panel.classList.toggle("groupedView", viewMode === "grouped");
  };

  const i18n = window.i18n;
  const t = (key, fallback) => i18n?.t?.(key, fallback) ?? fallback;

  const STORAGE_KEY = "historyShowTraining";
  let showTraining = (() => {
    try { return localStorage.getItem(STORAGE_KEY) !== "0"; } catch { return true; }
  })();

  const syncTrainToggle = () => {
    if (!trainToggleBtn) return;
    trainToggleBtn.classList.toggle("active", showTraining);
    trainToggleBtn.title = t(
      showTraining ? "history.hideTrainingBattles" : "history.showTrainingBattles",
      showTraining ? "Hide Training Battles" : "Show Training Battles"
    );
  };

  const formatTime = (ms) => {
    const totalMs = Number(ms);
    if (!Number.isFinite(totalMs) || totalMs <= 0) return "00:00";
    const totalSeconds = Math.floor(totalMs / 1000);
    const minutes = Math.floor(totalSeconds / 60);
    const seconds = totalSeconds % 60;
    return `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
  };

  const formatDate = (ms) => {
    const d = new Date(Number(ms));
    if (isNaN(d.getTime())) return "-";
    const pad = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  };

  const formatDamage = (v) => {
    const n = Number(v);
    if (!Number.isFinite(n)) return "-";
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}m`;
    if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
    return `${Math.round(n)}`;
  };

  const getJobLabel = (job) => {
    const key = JOB_KEY_MAP[job];
    if (!key) return job;
    return t(`classes.${key}`, key);
  };

  const setClassDropdownOpen = (open) => {
    classDropdownOpen = open;
    filterPlayerEl?.classList.toggle("open", open);
  };

  const setClassFilter = (job) => {
    filterPlayer = job;
    if (filterPlayerLabel) {
      if (job) {
        const img = `<img src="./assets/${job}.png" alt="" class="historyClassDropdownIcon" onerror="this.style.display='none'">`;
        filterPlayerLabel.innerHTML = `${img}${getJobLabel(job)}`;
      } else {
        filterPlayerLabel.textContent = t("history.filterPlayer", "All classes");
      }
    }
    filterPlayerMenu?.querySelectorAll(".historyClassOption").forEach((opt) => {
      opt.classList.toggle("selected", opt.dataset.job === job);
    });
    setClassDropdownOpen(false);
    renderList(allFights);
  };

  let allFights = [];
  const PAGE_SIZE = 30;
  let renderedCount = 0;
  let lastVisible = [];
  let loadingMore = false;

  const populateDropdowns = (fights) => {
    if (!filterBossEl || !filterPlayerEl || !filterDateEl) return;
    const allOption = (label) => `<option value="">${label}</option>`;

    const bossNames = [...new Set(fights.map((f) => f.bossName || "").filter(Boolean))].sort();
    filterBossEl.innerHTML = allOption(t("history.filterBoss", "All bosses"));
    bossNames.forEach((name) => {
      const opt = document.createElement("option");
      opt.value = name;
      opt.textContent = name;
      if (name === filterBoss) opt.selected = true;
      filterBossEl.appendChild(opt);
    });

    // Custom class dropdown
    if (filterPlayerMenu) {
      filterPlayerMenu.innerHTML = "";
      const allOpt = document.createElement("div");
      allOpt.className = "historyClassOption" + (filterPlayer === "" ? " selected" : "");
      allOpt.dataset.job = "";
      allOpt.textContent = t("history.filterPlayer", "All classes");
      allOpt.addEventListener("click", () => setClassFilter(""));
      filterPlayerMenu.appendChild(allOpt);

      const jobs = [...new Set(fights.flatMap((f) => Array.isArray(f.jobs) ? f.jobs : []).filter(Boolean))].sort(
        (a, b) => getJobLabel(a).localeCompare(getJobLabel(b))
      );
      jobs.forEach((job) => {
        const opt = document.createElement("div");
        opt.className = "historyClassOption" + (job === filterPlayer ? " selected" : "");
        opt.dataset.job = job;
        const img = document.createElement("img");
        img.src = `./assets/${job}.png`;
        img.alt = "";
        img.className = "historyClassDropdownIcon";
        img.onerror = () => { img.style.display = "none"; };
        opt.appendChild(img);
        opt.appendChild(document.createTextNode(getJobLabel(job)));
        opt.addEventListener("click", () => setClassFilter(job));
        filterPlayerMenu.appendChild(opt);
      });

      // Sync label
      if (!filterPlayer) {
        if (filterPlayerLabel) filterPlayerLabel.textContent = t("history.filterPlayer", "All classes");
      }
    }

    const dates = [...new Set(fights.map((f) => formatDate(f.startTimeMs).slice(0, 10)).filter((d) => d !== "-"))].sort().reverse();
    filterDateEl.innerHTML = allOption(t("history.filterDate", "All dates"));
    dates.forEach((date) => {
      const opt = document.createElement("option");
      opt.value = date;
      opt.textContent = date;
      if (date === filterDate) opt.selected = true;
      filterDateEl.appendChild(opt);
    });
  };

  const applyFilters = (fights) => {
    return fights.filter((f) => {
      if (f.isTrain && !showTraining) return false;
      if (filterBoss && (f.bossName || "") !== filterBoss) return false;
      if (filterPlayer) {
        const jobs = Array.isArray(f.jobs) ? f.jobs : [];
        if (!jobs.includes(filterPlayer)) return false;
      }
      if (filterDate && formatDate(f.startTimeMs).slice(0, 10) !== filterDate) return false;
      return true;
    });
  };

  const buildRow = (fight, { grouped = false } = {}) => {
    const row = document.createElement("div");
    row.className = grouped ? "historyRow historyRowChild" : "historyRow";
    row.dataset.fightId = fight.id;

    const infoEl = document.createElement("div");
    infoEl.className = "historyRowInfo";

    const nameEl = document.createElement("div");
    nameEl.className = "historyRowName";
    // In grouped view the boss name is the section header, so the row leads with its date instead.
    nameEl.textContent = grouped
      ? formatDate(fight.startTimeMs)
      : (fight.bossName || `Boss #${fight.targetId}`);
    if (fight.isLive) {
      const lastActivityMs = Number(fight.startTimeMs) + Number(fight.durationMs);
      if (Date.now() - lastActivityMs < 60_000) {
        const badge = document.createElement("span");
        badge.className = "historyLiveBadge";
        badge.textContent = t("history.liveBadge", "Live");
        nameEl.appendChild(badge);
      }
    }
    if (fight.isTrain) {
      const badge = document.createElement("span");
      badge.className = "historyTrainBadge";
      badge.textContent = t("history.trainBadge", "Training");
      nameEl.appendChild(badge);
    }

    const metaEl = document.createElement("div");
    metaEl.className = "historyRowMeta";

    const timeEl = document.createElement("span");
    timeEl.className = "historyRowTime";
    timeEl.textContent = formatDate(fight.startTimeMs);

    const durEl = document.createElement("span");
    durEl.className = "historyRowDuration";
    durEl.textContent = formatTime(fight.durationMs);

    const dmgEl = document.createElement("span");
    dmgEl.className = "historyRowDamage";
    dmgEl.textContent = formatDamage(fight.totalDamage);

    if (!grouped) metaEl.appendChild(timeEl);
    metaEl.appendChild(durEl);
    metaEl.appendChild(dmgEl);

    const iconsEl = document.createElement("div");
    iconsEl.className = "historyRowIcons";
    const allJobs = (Array.isArray(fight.jobs) ? fight.jobs : []).slice(0, 12);
    allJobs.forEach((job) => {
      if (!job) return;
      const wrap = document.createElement("span");
      wrap.className = "historyIconWrap";
      wrap.setAttribute("data-tip", getJobLabel(job));
      const img = document.createElement("img");
      img.src = `./assets/${job}.png`;
      img.alt = job;
      img.className = "historyRowClassIcon";
      img.onerror = () => { wrap.style.display = "none"; };
      wrap.appendChild(img);
      iconsEl.appendChild(wrap);
    });

    infoEl.appendChild(nameEl);
    infoEl.appendChild(metaEl);

    const actionsEl = document.createElement("div");
    actionsEl.className = "historyRowActions";

    if (!fight.isLive) {
      const deleteBtn = document.createElement("button");
      deleteBtn.className = "historyDeleteBtn";
      deleteBtn.type = "button";
      deleteBtn.setAttribute("aria-label", t("history.delete", "Delete"));
      deleteBtn.innerHTML = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" width="15" height="15"><polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><path d="M10 11v6M14 11v6"/><path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/></svg>`;
      deleteBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        if (window.javaBridge?.deleteFight?.(fight.id)) {
          allFights = allFights.filter((x) => x.id !== fight.id);
          if (viewMode === "grouped") {
            // Re-render so the section's fight count updates and empty sections drop out.
            renderList(allFights);
          } else {
            row.remove();
            if (!listEl.querySelector(".historyRow")) {
              if (emptyEl) emptyEl.style.display = "";
            }
          }
        }
      });
      actionsEl.appendChild(deleteBtn);
    }

    row.appendChild(infoEl);
    row.appendChild(iconsEl);
    row.appendChild(actionsEl);

    row.addEventListener("click", async () => {
      const rawRecord = await window.javaBridge?.getFightDetails?.(fight.id);
      if (!rawRecord) return;
      let record;
      try {
        record = typeof rawRecord === "string" ? JSON.parse(rawRecord) : rawRecord;
      } catch {
        return;
      }
      onOpenFight?.(record);
    });

    return row;
  };

  const appendPage = () => {
    if (!listEl || renderedCount >= lastVisible.length) return;
    const end = Math.min(renderedCount + PAGE_SIZE, lastVisible.length);
    const frag = document.createDocumentFragment();
    for (let i = renderedCount; i < end; i++) {
      frag.appendChild(buildRow(lastVisible[i]));
    }
    listEl.appendChild(frag);
    renderedCount = end;
  };

  const fightCountLabel = (n) =>
    n === 1
      ? t("history.fightCountOne", "1 fight")
      : t("history.fightCount", "{n} fights").replace("{n}", n);

  const startMs = (f) => Number(f.startTimeMs) || 0;

  const buildGroup = (group) => {
    // Most recent run first within a section.
    group.fights.sort((a, b) => startMs(b) - startMs(a));

    const wrap = document.createElement("div");
    wrap.className = "historyGroup";

    const header = document.createElement("div");
    header.className = "historyGroupHeader";

    const chevron = document.createElement("span");
    chevron.className = "historyGroupChevron";
    chevron.innerHTML = `<svg width="10" height="6" viewBox="0 0 10 6" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="M0 0l5 6 5-6z" fill="currentColor"/></svg>`;

    const nameEl = document.createElement("div");
    nameEl.className = "historyGroupName";
    nameEl.textContent = group.name;

    const countEl = document.createElement("span");
    countEl.className = "historyGroupCount";
    countEl.textContent = fightCountLabel(group.fights.length);

    header.appendChild(chevron);
    header.appendChild(nameEl);
    header.appendChild(countEl);

    const childWrap = document.createElement("div");
    childWrap.className = "historyGroupChildren";

    const renderChildren = () => {
      if (childWrap.childElementCount) return;
      const frag = document.createDocumentFragment();
      group.fights.forEach((f) => frag.appendChild(buildRow(f, { grouped: true })));
      childWrap.appendChild(frag);
    };

    const expanded = expandedGroups.has(group.name);
    header.classList.toggle("expanded", expanded);
    childWrap.style.display = expanded ? "" : "none";
    if (expanded) renderChildren();

    header.addEventListener("click", () => {
      const nowExpanded = !expandedGroups.has(group.name);
      if (nowExpanded) {
        expandedGroups.add(group.name);
        renderChildren();
      } else {
        expandedGroups.delete(group.name);
      }
      header.classList.toggle("expanded", nowExpanded);
      childWrap.style.display = nowExpanded ? "" : "none";
    });

    wrap.appendChild(header);
    wrap.appendChild(childWrap);
    return wrap;
  };

  const renderGrouped = (visible) => {
    const groups = new Map();
    visible.forEach((f) => {
      const key = f.bossName || `Boss #${f.targetId}`;
      let g = groups.get(key);
      if (!g) { g = { name: key, fights: [] }; groups.set(key, g); }
      g.fights.push(f);
    });
    // Sections ordered by their most recent run.
    const ordered = [...groups.values()].sort(
      (a, b) => Math.max(...b.fights.map(startMs)) - Math.max(...a.fights.map(startMs))
    );
    const frag = document.createDocumentFragment();
    ordered.forEach((g) => frag.appendChild(buildGroup(g)));
    listEl.appendChild(frag);
  };

  const renderList = (fights) => {
    if (!listEl) return;
    listEl.innerHTML = "";
    renderedCount = 0;

    lastVisible = applyFilters(fights);

    if (!lastVisible || lastVisible.length === 0) {
      if (emptyEl) emptyEl.style.display = "";
      return;
    }
    if (emptyEl) emptyEl.style.display = "none";

    if (viewMode === "grouped") {
      renderGrouped(lastVisible);
    } else {
      appendPage();
    }
  };

  const open = () => {
    panel.classList.add("open");
    syncTrainToggle();
    syncViewToggle();
    const raw = window.javaBridge?.getFightHistory?.();
    try {
      allFights = typeof raw === "string" ? JSON.parse(raw) : (Array.isArray(raw) ? raw : []);
    } catch {
      allFights = [];
    }
    populateDropdowns(allFights);
    renderList(allFights);
  };

  // Load more rows when scrolled near the bottom (flat list only; grouped renders sections eagerly)
  listEl?.addEventListener("scroll", () => {
    if (viewMode !== "list") return;
    if (loadingMore || renderedCount >= lastVisible.length) return;
    const threshold = 80;
    if (listEl.scrollTop + listEl.clientHeight >= listEl.scrollHeight - threshold) {
      loadingMore = true;
      appendPage();
      loadingMore = false;
    }
  });

  filterBossEl?.addEventListener("change", () => {
    filterBoss = filterBossEl.value;
    renderList(allFights);
  });
  filterPlayerTrigger?.addEventListener("click", () => {
    setClassDropdownOpen(!classDropdownOpen);
  });
  document.addEventListener("click", (e) => {
    if (classDropdownOpen && filterPlayerEl && !filterPlayerEl.contains(e.target)) {
      setClassDropdownOpen(false);
    }
  });
  filterDateEl?.addEventListener("change", () => {
    filterDate = filterDateEl.value;
    renderList(allFights);
  });

  trainToggleBtn?.addEventListener("click", () => {
    showTraining = !showTraining;
    try { localStorage.setItem(STORAGE_KEY, showTraining ? "1" : "0"); } catch {}
    syncTrainToggle();
    renderList(allFights);
  });

  deleteToggleBtn?.addEventListener("click", () => {
    showDeleteMode = !showDeleteMode;
    syncDeleteToggle();
  });

  viewBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      const mode = btn.dataset.view === "list" ? "list" : "grouped";
      if (mode === viewMode) return;
      viewMode = mode;
      try { localStorage.setItem(VIEW_KEY, viewMode); } catch {}
      syncViewToggle();
      expandedGroups.clear();
      renderList(allFights);
    });
  });

  const close = () => {
    panel.classList.remove("open");
    showDeleteMode = false;
    syncDeleteToggle();
    filterBoss = "";
    filterPlayer = "";
    filterDate = "";
    expandedGroups.clear();
    if (filterBossEl) filterBossEl.selectedIndex = 0;
    if (filterPlayerLabel) filterPlayerLabel.textContent = t("history.filterPlayer", "All classes");
    setClassDropdownOpen(false);
    if (filterDateEl) filterDateEl.selectedIndex = 0;
  };

  const isOpen = () => panel.classList.contains("open");

  closeBtn?.addEventListener("click", close);

  return { open, close, isOpen };
};
