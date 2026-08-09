pub mod capture;
pub mod combat;
pub mod config;
pub mod entity;
pub mod history;
pub mod i18n;
pub mod logging;
pub mod platform;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tauri::{Emitter, Manager};
use tokio::sync::mpsc;

use capture::captured_payload::CapturedPayload;
use capture::combat_port_detector::CombatPortDetector;
use capture::pcap_capturer::PcapCapturer;
use combat::capture_dispatcher::CaptureDispatcher;
use combat::data_storage::DataStorage;
use combat::dps_calculator::DpsCalculator;
use combat::ping_tracker::PingTracker;
use config::settings::Settings;
use entity::dps_data::DpsData;
use entity::fight_record::{FightRecord, FightSummary};
use entity::details_context::{DetailsContext, TargetDetailsResponse};
use history::fight_history::FightHistoryManager;
use i18n::lookup::{NpcLookup, SkillLookup};

/// Monitor the Details window was last placed on. Recorded here rather than
/// passed back from JS so the confirmation cannot disagree with the placement.
static DETAILS_MONITOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

/// Shared application state.
pub struct AppState {
    pub data_storage: Arc<DataStorage>,
    pub dps_calculator: Mutex<DpsCalculator>,
    pub ping_tracker: Arc<PingTracker>,
    pub port_detector: Arc<CombatPortDetector>,
    pub fight_history: FightHistoryManager,
    pub settings: Settings,
    pub skill_lookup: Arc<SkillLookup>,
    pub npc_lookup: Arc<NpcLookup>,
    pub app_data_dir: std::path::PathBuf,
    pub i18n_data_dir: Option<std::path::PathBuf>,
}

// ===== TAURI COMMANDS =====

#[tauri::command]
fn get_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
fn get_dps_snapshot(state: tauri::State<'_, AppState>) -> DpsData {
    state.dps_calculator.lock().get_dps()
}

#[tauri::command]
fn get_skill_details(state: tauri::State<'_, AppState>, target_id: i32, actor_ids: Option<Vec<i32>>) -> TargetDetailsResponse {
    state.dps_calculator.lock().get_target_details(target_id, actor_ids.as_deref())
}

#[tauri::command]
fn get_details_context(state: tauri::State<'_, AppState>) -> DetailsContext {
    state.dps_calculator.lock().get_details_context()
}

#[tauri::command]
fn get_fight_history(state: tauri::State<'_, AppState>) -> Vec<FightSummary> {
    state.fight_history.list_fights()
}

#[tauri::command]
fn save_fight(state: tauri::State<'_, AppState>, record: FightRecord) -> Result<(), String> {
    state.fight_history.save_fight(&record)
}

#[tauri::command]
fn load_fight(state: tauri::State<'_, AppState>, id: String) -> Result<FightRecord, String> {
    state.fight_history.load_fight(&id)
}

#[tauri::command]
fn delete_fight(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    state.fight_history.delete_fight(&id)
}

#[tauri::command]
fn export_fight_json(state: tauri::State<'_, AppState>, record: FightRecord) -> Result<String, String> {
    state.fight_history.export_fight_json(&record)
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> std::collections::HashMap<String, String> {
    state.settings.get_all()
}

#[tauri::command]
fn update_settings(state: tauri::State<'_, AppState>, key: String, value: String) {
    state.settings.set(&key, &value);
}

#[tauri::command]
fn clear_settings(state: tauri::State<'_, AppState>) {
    state.settings.clear();
}

#[tauri::command]
fn get_ping(state: tauri::State<'_, AppState>) -> Option<i32> {
    state.ping_tracker.current_ping_ms()
}

#[tauri::command]
fn get_capture_status(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let port = state.port_detector.current_port();
    let device = state.port_detector.current_device();
    let local_id = state.data_storage.local_player_id();
    let char_name = state.data_storage.local_character_name();
    serde_json::json!({
        "locked": port.is_some(),
        "port": port,
        "device": device.clone().unwrap_or_default(),
        "ip": device.unwrap_or_else(|| "127.0.0.1".to_string()),
        "localPlayerId": local_id,
        "characterName": char_name,
    })
}

#[tauri::command]
fn set_target_mode(state: tauri::State<'_, AppState>, mode: String) {
    state.dps_calculator.lock().set_target_selection_mode(&mode);
}

#[tauri::command]
fn set_character_name(state: tauri::State<'_, AppState>, name: String) {
    let trimmed = name.trim().to_string();
    state.data_storage.set_local_character_name(Some(name));
    // If an actor ID was already bound, propagate the new character name
    // into nickname_storage immediately so the main meter window updates.
    if !trimmed.is_empty() {
        if let Some(id) = state.data_storage.local_player_id() {
            state.data_storage.set_permanent_nickname(id as i32, &trimmed);
        }
    }
}

#[tauri::command]
fn bind_local_actor_id(state: tauri::State<'_, AppState>, actor_id: i64) {
    if actor_id <= 0 {
        // Clear manual binding — auto-detection will take over
        tracing::info!("bind_local_actor_id: cleared");
        state.data_storage.set_local_player_id(None);
        return;
    }
    let already_bound = state.data_storage.local_player_id() == Some(actor_id);
    if !already_bound {
        tracing::info!("bind_local_actor_id: {}", actor_id);
        state.data_storage.set_local_player_id(Some(actor_id));
    }
    // Always (re)apply the permanent nickname if we have a character name,
    // even when the actor_id was already bound — this handles the case where
    // the character name was set AFTER the actor_id binding.
    if let Some(name) = state.data_storage.local_character_name() {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            let current = state.data_storage.get_nickname(actor_id as i32);
            if current.as_deref() != Some(trimmed) {
                state.data_storage.set_permanent_nickname(actor_id as i32, trimmed);
            }
        }
    }
}

#[tauri::command]
fn bind_local_nickname(state: tauri::State<'_, AppState>, actor_id: i64, nickname: String) {
    // Always update if the stored nickname differs from the requested one.
    // Previously we skipped if the actor had ANY nickname, which left stale
    // false-positive scan results stuck in place.
    let current = state.data_storage.get_nickname(actor_id as i32);
    if state.data_storage.local_player_id() == Some(actor_id)
        && current.as_deref() == Some(nickname.as_str())
    {
        return;
    }
    tracing::info!("bind_local_nickname: {} -> '{}' (was {:?})", actor_id, nickname, current);
    state.data_storage.set_local_player_id(Some(actor_id));
    // Use set_permanent_nickname so it survives reset_nicknames() calls
    state.data_storage.set_permanent_nickname(actor_id as i32, &nickname);
}

#[tauri::command]
fn reset_combat(state: tauri::State<'_, AppState>) {
    state.dps_calculator.lock().restart_target_selection(true);
    // Don't reset port detector or ping — keep the network connection alive
    // Only clear combat data and re-learn nicknames from future packets
    state.data_storage.reset_nicknames();
}

#[tauri::command]
fn is_admin() -> bool {
    platform::admin::is_admin()
}

#[tauri::command]
fn set_language(state: tauri::State<'_, AppState>, language: String) {
    tracing::info!("Language change requested: {}", language);
    if let Some(ref data_dir) = state.i18n_data_dir {
        i18n::lookup::load_language(&state.skill_lookup, &state.npc_lookup, data_dir, &language);
    } else {
        tracing::warn!("No i18n data dir available for language reload");
    }
    state.settings.set("dpsMeter.language", &language);
}

#[tauri::command]
fn set_debug_logging(state: tauri::State<'_, AppState>, enabled: bool) {
    logging::logger::set_debug_enabled(enabled, &state.app_data_dir);
    state.settings.set("dpsMeter.debugLoggingEnabled", if enabled { "true" } else { "false" });
}

#[tauri::command]
fn set_packet_logging(state: tauri::State<'_, AppState>, enabled: bool) {
    logging::logger::set_packet_log_enabled(enabled, &state.app_data_dir);
    state.settings.set("dpsMeter.saveRawPackets", if enabled { "true" } else { "false" });
}

#[tauri::command]
fn reset_auto_detection(state: tauri::State<'_, AppState>) {
    state.port_detector.reset();
    state.ping_tracker.reset();
}

#[tauri::command]
fn get_available_devices() -> Vec<String> {
    // Load wpcap.dll and enumerate devices
    match crate::capture::pcap_capturer::list_device_labels() {
        Ok(labels) => labels,
        Err(_) => Vec::new(),
    }
}

#[tauri::command]
fn set_manual_device(state: tauri::State<'_, AppState>, device: String) {
    let dev = if device.trim().is_empty() { None } else { Some(device) };
    state.port_detector.set_preferred_device(dev);
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn read_cached_icon(state: tauri::State<'_, AppState>, key: String) -> Option<String> {
    let path = state.app_data_dir.join("icon_cache").join(&key);
    std::fs::read_to_string(&path).ok()
}

#[tauri::command]
fn write_cached_icon(state: tauri::State<'_, AppState>, key: String, data: String) {
    let cache_dir = state.app_data_dir.join("icon_cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    let path = cache_dir.join(&key);
    let _ = std::fs::write(&path, &data);
}


#[tauri::command]
async fn show_update_window(app: tauri::AppHandle, current: String, latest: String, msi_url: String) -> Result<bool, String> {
    let msg = format!("A new update is available!\n\nCurrent: {}\nLatest: {}\n\nDownload and install now?", current, latest);

    let accepted = tokio::task::spawn_blocking(move || {
        #[cfg(windows)]
        {
            use windows::Win32::UI::WindowsAndMessaging::*;
            use windows::core::PCWSTR;
            let msg_w: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
            let title: Vec<u16> = "A2Tools - Update Available".encode_utf16().chain(std::iter::once(0)).collect();
            let result = unsafe {
                MessageBoxW(None, PCWSTR(msg_w.as_ptr()), PCWSTR(title.as_ptr()), MB_YESNO | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND)
            };
            result == IDYES
        }
        #[cfg(not(windows))]
        { false }
    }).await.unwrap_or(false);

    if accepted && !msi_url.is_empty() {
        // Download and install in background
        let app2 = app.clone();
        let url = msi_url.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = download_and_install_msi_inner(&app2, &url).await {
                tracing::error!("Update download failed: {}", e);
                // Show error dialog
                let _ = tokio::task::spawn_blocking(move || {
                    #[cfg(windows)]
                    {
                        use windows::Win32::UI::WindowsAndMessaging::*;
                        use windows::core::PCWSTR;
                        let msg: Vec<u16> = format!("Download failed: {}\n\nPlease download manually.", e)
                            .encode_utf16().chain(std::iter::once(0)).collect();
                        let title: Vec<u16> = "A2Tools - Update Error".encode_utf16().chain(std::iter::once(0)).collect();
                        unsafe { MessageBoxW(None, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR | MB_TOPMOST); }
                    }
                }).await;
            }
        });
    } else if accepted {
        // No MSI URL, open releases page
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", "https://github.com/taengu/A2Tools-DPS-Meter/releases"]).spawn();
    }

    Ok(accepted)
}

async fn download_and_install_msi_inner(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use futures_util::StreamExt;

    // Show progress dialog on a blocking thread
    let app_clone = app.clone();
    let url_owned = url.to_string();

    let response = reqwest::get(&url_owned).await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let total_size = response.content_length().unwrap_or(0);
    let file_name = url_owned.rsplit('/').next().unwrap_or("update.msi");
    let msi_path = std::env::temp_dir().join(file_name);

    let mut file = tokio::fs::File::create(&msi_path).await.map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();
    let mut last_pct: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if total_size > 0 {
            let pct = (downloaded * 100 / total_size).min(100);
            if pct != last_pct {
                last_pct = pct;
                let _ = app_clone.emit("download-progress", pct);
                tracing::info!("Download: {}%", pct);
            }
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);

    tracing::info!("Download complete, launching installer: {}", msi_path.display());

    // Detect current install directory from the running executable's location
    let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let install_dir = current_exe.parent()
        .ok_or("Could not determine install directory")?
        .to_string_lossy()
        .into_owned();
    // Strip a trailing backslash so msiexec doesn't interpret \" as an escape
    let install_dir = install_dir.trim_end_matches('\\').to_string();

    // Launch the MSI installer. msiexec.exe uses its own non-standard command line
    // parser, so PROPERTY="value" pairs with spaces require literal embedded quotes
    // — not what std::process::Command's normal arg quoting produces. We use raw_arg
    // (Windows-only) to control the exact command line.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("msiexec");
        cmd.raw_arg("/i")
            .raw_arg(format!("\"{}\"", msi_path.display()))
            .raw_arg("/passive")
            .raw_arg(format!("INSTALLDIR=\"{}\"", install_dir))
            .raw_arg("AUTOLAUNCHAPP=1");
        tracing::info!("msiexec args: /i \"{}\" /passive INSTALLDIR=\"{}\" AUTOLAUNCHAPP=1",
            msi_path.display(), install_dir);
        cmd.spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        return Err("MSI install only supported on Windows".to_string());
    }

    // Give installer time to start, then exit
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    app_clone.exit(0);
    Ok(())
}

#[tauri::command]
async fn fetch_url(url: String) -> Result<String, String> {
    reqwest::get(&url).await.map_err(|e| e.to_string())?
        .text().await.map_err(|e| e.to_string())
}

#[tauri::command]
fn open_url(url: String) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
}

#[tauri::command]
fn resize_window(app: tauri::AppHandle, width: f64, height: f64) {
    // Only the overlay auto-sizes itself. The details window is sized to a
    // whole monitor by open_details_window and must never be resized from JS.
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }));
    }
}

/// Displays as reported by the OS, for the "Show Details on Monitor" picker.
/// Positions and sizes are physical pixels, which is what set_position and
/// set_size want for exact monitor placement.
#[tauri::command]
fn list_monitors(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let primary = app.primary_monitor().ok().flatten();
    let primary_name = primary.as_ref().and_then(|m| m.name().cloned());
    let primary_rect = primary.as_ref().map(|m| {
        let p = *m.position();
        let s = *m.size();
        (p.x, p.y, s.width as i32, s.height as i32)
    });

    let monitors = match app.available_monitors() {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };

    let mut entries: Vec<(usize, serde_json::Value)> = monitors
        .into_iter()
        .enumerate()
        .map(|(index, m)| {
            let pos = m.position();
            let size = m.size();
            let name = m.name().cloned().unwrap_or_else(|| format!("Display {}", index + 1));
            let is_primary = primary_name.as_ref() == Some(&name);
            // Where this screen sits relative to the primary, so the picker can
            // say "right" / "above" instead of only a resolution — a resolution
            // alone does not tell you which physical monitor you just chose.
            let side = match primary_rect {
                _ if is_primary => "",
                Some((px, py, pw, ph)) => {
                    let (x, y, w, h) = (pos.x, pos.y, size.width as i32, size.height as i32);
                    if x >= px + pw { "right" }
                    else if x + w <= px { "left" }
                    else if y >= py + ph { "below" }
                    else if y + h <= py { "above" }
                    else { "" }
                }
                None => "",
            };
            (
                index,
                serde_json::json!({
                    // Index into available_monitors — this is what gets saved and
                    // passed back to open_details_window, so it must stay stable
                    // regardless of the display order below.
                    "index": index,
                    "name": name,
                    "x": pos.x,
                    "y": pos.y,
                    "width": size.width,
                    "height": size.height,
                    "scaleFactor": m.scale_factor(),
                    "isPrimary": is_primary,
                    "side": side,
                }),
            )
        })
        .collect();

    // Present the primary first so the picker's "1" is the screen the game is
    // on and "2" is the other one. The OS order is not dependable: on a
    // two-screen setup here it reported the secondary display first, which made
    // "Monitor 2" select the primary.
    entries.sort_by_key(|(index, v)| {
        let primary = v.get("isPrimary").and_then(|p| p.as_bool()).unwrap_or(false);
        (!primary, *index)
    });
    entries.into_iter().map(|(_, v)| v).collect()
}

/// Open (or move) the always-on Details window, filling the chosen monitor.
/// Frameless to match the overlay; the in-page header carries the close button.
#[tauri::command]
fn open_details_window(app: tauri::AppHandle, monitor_index: usize) -> Result<(), String> {
    open_details_on_monitor_inner(&app, monitor_index, true)
}

fn open_details_on_monitor(app: &tauri::AppHandle, monitor_index: usize) -> Result<(), String> {
    open_details_on_monitor_inner(app, monitor_index, false)
}

/// `force_place` = the user just picked this monitor, so ignore any remembered
/// position and fill that screen.
fn open_details_on_monitor_inner(
    app: &tauri::AppHandle,
    monitor_index: usize,
    force_place: bool,
) -> Result<(), String> {
    let monitors = app.available_monitors().map_err(|e| e.to_string())?;
    if monitors.is_empty() {
        return Err("no monitors reported".into());
    }
    let monitor = monitors
        .get(monitor_index)
        .ok_or_else(|| format!("monitor {} is not connected", monitor_index))?;
    DETAILS_MONITOR.store(monitor_index, std::sync::atomic::Ordering::Relaxed);

    // available_monitors reports PHYSICAL pixels, but WebviewWindowBuilder's
    // position()/inner_size() take LOGICAL pixels. Convert, or on a scaled
    // display the window lands in the wrong place at the wrong size.
    let scale = monitor.scale_factor();
    let pos = *monitor.position();
    let size = *monitor.size();
    let lx = pos.x as f64 / scale;
    let ly = pos.y as f64 / scale;
    let lw = size.width as f64 / scale;
    let lh = size.height as f64 / scale;

    tracing::info!(
        "details window -> monitor {} '{}' physical {}x{} at {},{} (scale {}) => logical {}x{} at {},{}",
        monitor_index,
        monitor.name().cloned().unwrap_or_default(),
        size.width, size.height, pos.x, pos.y, scale, lw, lh, lx, ly
    );

    if let Some(existing) = app.get_webview_window("details") {
        // Already open — move it only when the user explicitly picked a monitor.
        if force_place {
            let _ = existing.unmaximize();
            let _ = existing.set_position(tauri::Position::Physical(pos));
            let _ = existing.set_size(tauri::Size::Physical(size));
        }
        let _ = existing.show();
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        announce_details_placement(app, monitor_index);
        return Ok(());
    }

    // Born at the target coordinates rather than created-then-moved. Moving a
    // hidden window and calling maximize() put it on whichever monitor Windows
    // still considered current, which is how Details kept opening on the same
    // screen as the overlay.
    let window = tauri::WebviewWindowBuilder::new(
        app,
        "details",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("A2Tools DPS Meter — Details")
    .decorations(false)
    .transparent(false)
    .always_on_top(false)
    .resizable(true)
    .skip_taskbar(false)
    .position(lx, ly)
    .inner_size(lw, lh)
    // Built hidden: a visible webview paints white until the bundle loads and
    // applies the dark theme, which read as "a blank white window filled the
    // screen". details_window_ready shows it once the page has rendered.
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;

    // An explicit monitor pick always wins; otherwise fall back to wherever the
    // user last dragged the window.
    if force_place || !restore_window_geometry(app, &window, "details") {
        // Re-assert in physical units: the builder's logical values round on
        // fractional-scale displays.
        let _ = window.set_position(tauri::Position::Physical(pos));
        let _ = window.set_size(tauri::Size::Physical(size));
    }

    // Announced once the window reports ready (see details_window_ready); a
    // freshly built webview has no listener attached yet.
    // Safety net. Unconditional: is_visible() does not reliably reflect whether
    // the window was actually mapped, so guarding on it left the window created
    // but never revealed. show() on an already-visible window is a no-op, so the
    // worst case here is a redundant call.
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if let Some(w) = handle.get_webview_window("details") {
            let _ = w.show();
            let _ = w.set_focus();
            tracing::info!("details window revealed (safety net)");
        }
    });
    Ok(())
}



/// Restore a tool window's remembered geometry. Returns true if anything was
/// applied, so callers know whether they still need to place it themselves.
fn restore_window_geometry(app: &tauri::AppHandle, window: &tauri::WebviewWindow, label: &str) -> bool {
    let Some(state) = app.try_state::<AppState>() else { return false };
    let get = |k: &str| state.settings.get(&format!("window.{}.{}", label, k))
        .and_then(|v| v.trim().parse::<i32>().ok());
    let (Some(x), Some(y)) = (get("x"), get("y")) else { return false };
    if x <= -10000 || y <= -10000 {
        return false;
    }
    // Only restore onto a screen that still exists — an unplugged monitor would
    // otherwise strand the window off-desktop.
    let on_screen = app.available_monitors().map(|ms| {
        ms.iter().any(|m| {
            let p = *m.position();
            let s = *m.size();
            x >= p.x - 64 && x < p.x + s.width as i32 && y >= p.y - 64 && y < p.y + s.height as i32
        })
    }).unwrap_or(false);
    if !on_screen {
        tracing::info!("{} window: saved position {},{} is off-desktop; ignoring", label, x, y);
        return false;
    }
    let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y }));
    if let (Some(w), Some(h)) = (get("w"), get("h")) {
        if w > 200 && h > 150 {
            let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize {
                width: w as u32,
                height: h as u32,
            }));
        }
    }
    true
}

/// The Settings window. Free-floating like Details — it used to be a panel that
/// forced the overlay to resize itself to ~820px tall.
#[tauri::command]
fn open_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window("settings") {
        let _ = existing.show();
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        return Ok(());
    }
    let window = tauri::WebviewWindowBuilder::new(
        &app,
        "settings",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("A2Tools DPS Meter — Settings")
    .decorations(false)
    .transparent(false)
    .always_on_top(false)
    .resizable(true)
    .skip_taskbar(false)
    .inner_size(760.0, 820.0)
    .min_inner_size(520.0, 420.0)
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;

    if !restore_window_geometry(&app, &window, "settings") {
        let _ = window.center();
    }
    // Same hidden-until-painted treatment as Details: a visible webview paints
    // white until the bundle loads.
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if let Some(w) = handle.get_webview_window("settings") {
            let _ = w.show();
            let _ = w.set_focus();
        }
    });
    Ok(())
}

#[tauri::command]
fn close_settings_window(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.close();
    }
}

/// Shown when the frontend of a tool window has painted.
#[tauri::command]
fn tool_window_ready(app: tauri::AppHandle, label: String) {
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Tell the Details window which screen it just landed on, so it can confirm
/// visually. A dropdown label alone does not prove the right monitor was picked.
fn announce_details_placement(app: &tauri::AppHandle, monitor_index: usize) {
    let monitors = match app.available_monitors() {
        Ok(m) => m,
        Err(_) => return,
    };
    let Some(monitor) = monitors.get(monitor_index) else { return };
    let primary = app.primary_monitor().ok().flatten();
    let primary_name = primary.as_ref().and_then(|m| m.name().cloned());
    let name = monitor.name().cloned().unwrap_or_default();
    let is_primary = primary_name.as_ref() == Some(&name);

    // Position in the primary-first ordering the picker shows.
    let mut ordered: Vec<(bool, usize)> = monitors
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name().cloned() == primary_name, i))
        .collect();
    ordered.sort_by_key(|(is_p, i)| (!*is_p, *i));
    let position = ordered
        .iter()
        .position(|(_, i)| *i == monitor_index)
        .unwrap_or(monitor_index);

    let size = *monitor.size();
    let _ = app.emit_to(
        "details",
        "details-placed",
        serde_json::json!({
            "number": position + 1,
            "width": size.width,
            "height": size.height,
            "isPrimary": is_primary,
        }),
    );
}

/// Called by the Details window once its panel has painted.
#[tauri::command]
fn details_window_ready(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("details") {
        let _ = window.show();
        tracing::info!("details window revealed (frontend ready)");
    }
    let index = DETAILS_MONITOR.load(std::sync::atomic::Ordering::Relaxed);
    if index != usize::MAX {
        announce_details_placement(&app, index);
    }
}

#[tauri::command]
fn close_details_window(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("details") {
        let _ = window.close();
    }
}

#[tauri::command]
fn capture_screenshot(app: tauri::AppHandle, x: i32, y: i32, width: i32, height: i32) {
    #[cfg(windows)]
    {
        if let Some(window) = app.get_webview_window("main") {
            if let Ok(raw) = window.hwnd() {
                let hwnd_val = raw.0 as isize;
                std::thread::spawn(move || {
                    platform::screenshot::capture_to_clipboard(hwnd_val, x, y, width, height);
                });
            }
        }
    }
}

#[tauri::command]
fn start_drag(app: tauri::AppHandle) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{HWND, WPARAM, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_NCLBUTTONDOWN};
        use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;

        if let Some(window) = app.get_webview_window("main") {
            if let Ok(raw) = window.hwnd() {
                unsafe {
                    let _ = ReleaseCapture();
                    const HTCAPTION: usize = 2;
                    let hwnd = HWND(raw.0);
                    let _ = PostMessageW(Some(hwnd), WM_NCLBUTTONDOWN, WPARAM(HTCAPTION), LPARAM(0));
                }
            }
        }
    }
}

#[tauri::command]
fn get_aion2_window_title() -> Option<String> {
    platform::window_detector::find_aion2_window_title()
}

#[tauri::command]
fn test_auto_hide() -> serde_json::Value {
    let aion_fg = platform::window_detector::is_aion2_foreground();
    let aion_title = platform::window_detector::find_aion2_window_title();
    serde_json::json!({
        "aion2_foreground": aion_fg,
        "aion2_title": aion_title,
    })
}

#[tauri::command]
fn debug_status(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let port = state.port_detector.current_port();
    let device = state.port_detector.current_device();
    let ping = state.ping_tracker.current_ping_ms();
    let dmg_gen = state.data_storage.damage_generation();
    let window = platform::window_detector::find_aion2_window_title();
    let admin = platform::admin::is_admin();
    serde_json::json!({
        "port": port,
        "device": device,
        "ping": ping,
        "damageGeneration": dmg_gen,
        "aion2Window": window,
        "isAdmin": admin,
    })
}

#[tauri::command]
async fn replay_file(state: tauri::State<'_, AppState>, file_path: String) -> Result<String, String> {
    // Reset existing data before replay
    state.dps_calculator.lock().restart_target_selection(true);
    state.data_storage.reset_nicknames();

    // Feed packets directly to StreamProcessor, bypassing CaptureDispatcher
    // (no AION2 window check, no port detection needed for replay)
    let data_storage = state.data_storage.clone();
    let skill_lookup = state.skill_lookup.clone();
    let npc_lookup = state.npc_lookup.clone();
    let i18n_dir = state.i18n_data_dir.clone();

    let count = tokio::task::spawn_blocking(move || {
        use crate::capture::stream_processor::StreamProcessor;

        let mut processor = StreamProcessor::new(data_storage.clone(), skill_lookup, npc_lookup);
        // Load DOT IDs
        if let Some(ref data_dir) = i18n_dir {
            let mut dot_ids = std::collections::HashSet::new();
            if let Ok(text) = std::fs::read_to_string(data_dir.join("dot_skill_ids.json")) {
                if let Ok(ids) = serde_json::from_str::<Vec<i32>>(&text) {
                    for id in ids { dot_ids.insert(id); }
                }
            }
            processor.set_dot_skill_ids(dot_ids);
        }

        // Each line in the replay file is a complete game payload — process directly
        // without TCP reassembly (the assembler would incorrectly concatenate payloads)
        let text = match std::fs::read_to_string(&file_path) {
            Ok(t) => t.trim_start_matches('\u{feff}').to_string(), // Strip BOM
            Err(e) => return Err(format!("Failed to read file: {}", e)),
        };

        let mut packet_count = 0;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() != 3 { continue; }
            // Use capture-time timestamp from the row, not wall clock
            if let Some(ts) = parse_replay_timestamp(parts[0].trim()) {
                processor.set_override_timestamp(Some(ts));
            }
            let hex = parts[2];
            let data = match decode_replay_hex(hex) {
                Some(d) => d,
                None => continue,
            };
            packet_count += 1;
            processor.consume_stream(&data);
        }

        let dmg = data_storage.damage_generation();
        Ok(format!("Replay complete. {} packets, {} damage events.", packet_count, dmg))
    }).await.map_err(|e| format!("Replay task failed: {}", e))?;

    // Force snapshot boss fights from the replay
    {
        let mut calc = state.dps_calculator.lock();
        let records = calc.snapshot_boss_fights_force();
        let mut sorted = records;
        sorted.sort_by(|a, b| b.total_damage.cmp(&a.total_damage));
        for record in sorted.iter().take(10) {
            if let Err(e) = state.fight_history.save_fight(record) {
                tracing::warn!("Failed to save replay fight: {}", e);
            } else {
                tracing::info!("Saved replay fight: {} ({})", record.boss_name, record.id);
            }
        }
        // Mark all targets as saved so the periodic auto-save loop doesn't re-process them
        calc.mark_all_targets_saved();
    }

    count
}

/// Parse an ISO 8601 timestamp (or plain epoch millis) into epoch milliseconds.
fn parse_replay_timestamp(s: &str) -> Option<i64> {
    // Try plain integer first (epoch millis)
    if let Ok(ms) = s.parse::<i64>() {
        return Some(ms);
    }
    // Parse ISO 8601: "2026-04-01T14:08:18.447814200-03:00"
    // Manual parse to avoid adding a chrono dependency
    // Format: YYYY-MM-DDTHH:MM:SS.fractional[+-]HH:MM
    let t_pos = s.find('T')?;
    let date_part = &s[..t_pos];
    let time_and_tz = &s[t_pos + 1..];

    let date_parts: Vec<&str> = date_part.split('-').collect();
    if date_parts.len() != 3 { return None; }
    let year: i64 = date_parts[0].parse().ok()?;
    let month: i64 = date_parts[1].parse().ok()?;
    let day: i64 = date_parts[2].parse().ok()?;

    // Split time from timezone offset (look for + or - after the seconds)
    let (time_part, tz_offset_mins) = if let Some(plus_pos) = time_and_tz.rfind('+') {
        if plus_pos > 6 { // Must be after HH:MM:SS
            let tz = &time_and_tz[plus_pos + 1..];
            let tz_parts: Vec<&str> = tz.split(':').collect();
            let h: i64 = tz_parts.first()?.parse().ok()?;
            let m: i64 = tz_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            (&time_and_tz[..plus_pos], h * 60 + m)
        } else {
            (time_and_tz, 0i64)
        }
    } else if let Some(minus_pos) = time_and_tz.rfind('-') {
        if minus_pos > 6 {
            let tz = &time_and_tz[minus_pos + 1..];
            let tz_parts: Vec<&str> = tz.split(':').collect();
            let h: i64 = tz_parts.first()?.parse().ok()?;
            let m: i64 = tz_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            (&time_and_tz[..minus_pos], -(h * 60 + m))
        } else {
            (time_and_tz, 0i64)
        }
    } else {
        // No timezone, treat as UTC
        let tp = time_and_tz.trim_end_matches('Z');
        (tp, 0i64)
    };

    // Parse time: HH:MM:SS.fractional
    let colon_parts: Vec<&str> = time_part.split(':').collect();
    if colon_parts.len() < 3 { return None; }
    let hour: i64 = colon_parts[0].parse().ok()?;
    let minute: i64 = colon_parts[1].parse().ok()?;
    let sec_parts: Vec<&str> = colon_parts[2].split('.').collect();
    let second: i64 = sec_parts[0].parse().ok()?;
    let millis: i64 = if sec_parts.len() > 1 {
        let frac = sec_parts[1];
        // Take first 3 digits for milliseconds
        let padded = if frac.len() >= 3 { &frac[..3] } else { frac };
        let mut ms: i64 = padded.parse().ok()?;
        if frac.len() < 3 {
            for _ in 0..(3 - frac.len()) { ms *= 10; }
        }
        ms
    } else {
        0
    };

    // Convert to Unix epoch using a simplified algorithm
    // Days from epoch (1970-01-01)
    let days = days_from_civil(year, month, day);
    let total_secs = days * 86400 + hour * 3600 + minute * 60 + second - tz_offset_mins * 60;
    Some(total_secs * 1000 + millis)
}

/// Days from 1970-01-01 for a given civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn decode_replay_hex(hex: &str) -> Option<Vec<u8>> {
    let clean: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() % 2 != 0 { return None; }
    let mut bytes = Vec::with_capacity(clean.len() / 2);
    for chunk in clean.as_bytes().chunks(2) {
        let h = match chunk[0] {
            b'0'..=b'9' => chunk[0] - b'0',
            b'a'..=b'f' => chunk[0] - b'a' + 10,
            b'A'..=b'F' => chunk[0] - b'A' + 10,
            _ => return None,
        };
        let l = match chunk[1] {
            b'0'..=b'9' => chunk[1] - b'0',
            b'a'..=b'f' => chunk[1] - b'a' + 10,
            b'A'..=b'F' => chunk[1] - b'A' + 10,
            _ => return None,
        };
        bytes.push((h << 4) | l);
    }
    Some(bytes)
}

// ===== APP SETUP =====

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::logger::init_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // Resolve data directory
            let app_data_dir = app.path().app_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let _ = std::fs::create_dir_all(&app_data_dir);

            // Load resources — try multiple paths (dev vs production)
            let skill_lookup = SkillLookup::new();
            let npc_lookup = NpcLookup::new();
            let mut dot_ids: HashSet<i32> = HashSet::new();

            let resource_dir = app.path().resource_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let candidate_dirs = [
                resource_dir.join("data"),                        // production: resources/data
                resource_dir.join("_up_").join("src").join("data"), // production: resources/_up_/src/data (from ../src/data)
                resource_dir.join("..").join("src").join("data"), // dev: src-tauri/../src/data
                std::path::PathBuf::from("src/data"),             // dev: cwd fallback
                std::path::PathBuf::from("../src/data"),          // dev: from src-tauri/
            ];

            // Find the data directory
            let mut found_data_dir: Option<std::path::PathBuf> = None;
            for data_dir in &candidate_dirs {
                if data_dir.exists() && data_dir.join("i18n").join("skills").exists() {
                    found_data_dir = Some(data_dir.clone());
                    break;
                }
            }

            if let Some(ref data_dir) = found_data_dir {
                // Load DOT skill IDs (language-independent)
                if let Ok(text) = std::fs::read_to_string(data_dir.join("dot_skill_ids.json")) {
                    if let Ok(ids) = serde_json::from_str::<Vec<i32>>(&text) {
                        for id in ids { dot_ids.insert(id); }
                        tracing::info!("Loaded {} DOT skill IDs", dot_ids.len());
                    }
                }

                // Load skill/NPC data in the user's language
                let language = Settings::new(app_data_dir.clone())
                    .get("dpsMeter.language")
                    .unwrap_or_else(|| "en".to_string());
                i18n::lookup::load_language(&skill_lookup, &npc_lookup, data_dir, &language);
            } else {
                tracing::warn!("Failed to find data directory!");
            }

            let skill_lookup = Arc::new(skill_lookup);
            let npc_lookup = Arc::new(npc_lookup);

            let data_storage = Arc::new(DataStorage::new());
            let ping_tracker = Arc::new(PingTracker::new());
            let port_detector = Arc::new(CombatPortDetector::new());

            let dps_calculator = DpsCalculator::new(
                data_storage.clone(),
                skill_lookup.clone(),
                npc_lookup.clone(),
                ping_tracker.clone(),
            );

            let settings = Settings::new(app_data_dir.clone());

            // Load logging settings from saved state
            if settings.get("dpsMeter.debugLoggingEnabled").as_deref() == Some("true") {
                logging::logger::set_debug_enabled(true, &app_data_dir);
            }
            if settings.get("dpsMeter.saveRawPackets").as_deref() == Some("true") {
                logging::logger::set_packet_log_enabled(true, &app_data_dir);
            }

            let state = AppState {
                data_storage: data_storage.clone(),
                dps_calculator: Mutex::new(dps_calculator),
                ping_tracker: ping_tracker.clone(),
                port_detector: port_detector.clone(),
                fight_history: FightHistoryManager::new(app_data_dir.clone()),
                settings,
                skill_lookup: skill_lookup.clone(),
                npc_lookup: npc_lookup.clone(),
                app_data_dir: app_data_dir.clone(),
                i18n_data_dir: found_data_dir.clone(),
            };

            app.manage(state);

            // Reopen the Details window if it was left enabled. Done here rather
            // than from JS because the backend already has settings loaded — the
            // frontend reads them asynchronously and would race the first paint.
            {
                let saved = app.state::<AppState>().settings.get("dpsMeter.detailsMonitor");
                if let Some(value) = saved {
                    let value = value.trim().to_string();
                    if !value.is_empty() && value != "off" {
                        if let Ok(index) = value.parse::<usize>() {
                            let handle = app.handle().clone();
                            // Deferred: available_monitors is unreliable until the
                            // main window exists and the event loop has run once.
                            tauri::async_runtime::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(600)).await;
                                if let Err(e) = open_details_on_monitor(&handle, index) {
                                    tracing::warn!("details window reopen failed: {}", e);
                                }
                            });
                        }
                    }
                }
            }

            // Restore saved window position and ensure always-on-top
            if let Some(window) = app.get_webview_window("main") {
                let state_ref = app.state::<AppState>();
                if let (Some(x), Some(y)) = (state_ref.settings.get("window.x"), state_ref.settings.get("window.y")) {
                    if let (Ok(x), Ok(y)) = (x.parse::<i32>(), y.parse::<i32>()) {
                        // Don't restore minimized positions (Windows uses -32000,-32000)
                        if x > -10000 && y > -10000 {
                            let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y }));
                        }
                    }
                }
                let _ = window.set_always_on_top(true);
            }

            // Check if Npcap is available before starting capture
            let npcap_available = unsafe { libloading::Library::new("wpcap.dll").is_ok() };
            if !npcap_available {
                tracing::error!("Npcap is not installed — packet capture disabled");
                // Notify frontend to show install prompt
                let handle_npcap = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    // Small delay so frontend has time to initialize
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = handle_npcap.emit("npcap-missing", ());
                });
            }

            // Start capture pipeline
            let (tx, rx) = mpsc::channel::<CapturedPayload>(4096);

            let capturer = PcapCapturer::new(tx);
            if npcap_available {
                capturer.start();
            }

            let mut dispatcher = CaptureDispatcher::new(
                data_storage.clone(),
                skill_lookup.clone(),
                npc_lookup.clone(),
                port_detector.clone(),
                ping_tracker.clone(),
            );
            dispatcher.set_dot_skill_ids(dot_ids);

            // Run dispatcher in background
            tauri::async_runtime::spawn(async move {
                dispatcher.run(rx).await;
            });

            // Register global hotkeys from saved settings (or defaults)
            let hotkey_handle = app.handle().clone();
            let hotkey_manager = platform::hotkeys::HotkeyManager::new();

            let reload_label = app.state::<AppState>().settings
                .get("dpsMeter.hotkey").unwrap_or_default();
            let toggle_label = app.state::<AppState>().settings
                .get("dpsMeter.toggleWindowHotkey").unwrap_or_default();

            let (reload_mods, reload_vk) = platform::hotkeys::parse_hotkey_label(&reload_label)
                .unwrap_or((0x0002 | 0x0001, 0x52)); // Default: Ctrl+Alt+R
            let (toggle_mods, toggle_vk) = platform::hotkeys::parse_hotkey_label(&toggle_label)
                .unwrap_or((0x0002 | 0x0001, 0x26)); // Default: Ctrl+Alt+Up

            hotkey_manager.start(
                reload_mods, reload_vk,
                toggle_mods, toggle_vk,
                {
                    let h = hotkey_handle.clone();
                    move || {
                        tracing::info!("Hotkey: reload triggered");
                        if let Some(state) = h.try_state::<AppState>() {
                            state.dps_calculator.lock().restart_target_selection(true);
                            state.data_storage.reset_nicknames();
                        }
                        // Notify frontend to clear UI
                        let _ = h.emit("combat-reset", ());
                        let _ = h.emit("dps-update", &entity::dps_data::DpsData::new());
                    }
                },
                {
                    let h = hotkey_handle;
                    move || {
                        // Toggle window visibility
                        if let Some(window) = h.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.set_always_on_top(true);
                                let _ = window.set_focus();
                            }
                        }
                    }
                },
            );

            // Periodic DPS update emission (every 500ms)
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(500));
                let mut tick_count: u64 = 0;
                let mut hide_delay: u64 = 0; // ticks to wait before hiding
                loop {
                    interval.tick().await;
                    tick_count += 1;

                    if let Some(state) = handle.try_state::<AppState>() {
                        let t0 = std::time::Instant::now();
                        let lock_guard = state.dps_calculator.lock();
                        let lock_ms = t0.elapsed().as_millis();
                        let dps = {
                            let mut calc = lock_guard;
                            calc.get_dps()
                        };
                        let calc_ms = t0.elapsed().as_millis();
                        let _ = handle.emit("dps-update", &dps);
                        let total_ms = t0.elapsed().as_millis();
                        if total_ms > 200 {
                            tracing::warn!("Slow: lock={}ms calc={}ms emit={}ms total={}ms gen={}",
                                lock_ms, calc_ms - lock_ms, total_ms - calc_ms, total_ms,
                                state.data_storage.damage_generation());
                        }

                        if let Some(ping) = state.ping_tracker.current_ping_ms() {
                            let _ = handle.emit("ping-update", ping);
                        }

                        // --- Auto-hide when AION2 loses focus (every tick) ---
                        let auto_hide = tick_count > 20
                            && state.settings.get("dpsMeter.autoHideMeter")
                                .unwrap_or_default() == "true";
                        if auto_hide {
                            if let Some(window) = handle.get_webview_window("main") {
                                let aion_fg = platform::window_detector::is_aion2_foreground();
                                let is_self_fg = window.is_focused().unwrap_or(false);
                                let is_visible = window.is_visible().unwrap_or(true);
                                let is_minimized = window.is_minimized().unwrap_or(false);
                                if tick_count % 4 == 0 {
                                    tracing::trace!("auto-hide: aion_fg={} self_fg={} visible={} minimized={} hide_delay={}",
                                        aion_fg, is_self_fg, is_visible, is_minimized, hide_delay);
                                }
                                #[cfg(windows)]
                                {
                                    use windows::Win32::Foundation::HWND;
                                    use windows::Win32::UI::WindowsAndMessaging::*;
                                    if let Ok(raw) = window.hwnd() {
                                        let hwnd = HWND(raw.0);
                                        if aion_fg || is_self_fg {
                                            hide_delay = 0;
                                            if !is_visible || is_minimized {
                                                unsafe {
                                                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                                                    let _ = SetWindowPos(
                                                        hwnd, Some(HWND_TOPMOST),
                                                        0, 0, 0, 0,
                                                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                                                    );
                                                }
                                                // Notify frontend to recalculate window size
                                                // (content may have changed while minimized)
                                                let _ = window.emit("force-resize", ());
                                            }
                                        } else if is_visible && !is_minimized {
                                            // Wait 3 ticks (1.5s) before hiding to avoid
                                            // flickering during alt-tab transitions
                                            hide_delay += 1;
                                            if hide_delay >= 3 {
                                                unsafe {
                                                    let _ = SetWindowPos(
                                                        hwnd, Some(HWND_NOTOPMOST),
                                                        0, 0, 0, 0,
                                                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                                                    );
                                                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // --- Save window position every ~5 seconds (every 10 ticks) ---
                        if tick_count % 10 == 0 {
                            if let Some(window) = handle.get_webview_window("main") {
                                if let Ok(pos) = window.outer_position() {
                                    // Don't save minimized/hidden positions
                                    if pos.x > -10000 && pos.y > -10000 {
                                        state.settings.set("window.x", &pos.x.to_string());
                                        state.settings.set("window.y", &pos.y.to_string());
                                    }
                                }
                            }
                            // Details and Settings float independently of the
                            // overlay, so each remembers where it was left.
                            for label in ["details", "settings"] {
                                if let Some(w) = handle.get_webview_window(label) {
                                    if !w.is_visible().unwrap_or(false) {
                                        continue;
                                    }
                                    if let Ok(pos) = w.outer_position() {
                                        if pos.x > -10000 && pos.y > -10000 {
                                            state.settings.set(&format!("window.{}.x", label), &pos.x.to_string());
                                            state.settings.set(&format!("window.{}.y", label), &pos.y.to_string());
                                        }
                                    }
                                    if let Ok(size) = w.outer_size() {
                                        if size.width > 100 && size.height > 100 {
                                            state.settings.set(&format!("window.{}.w", label), &size.width.to_string());
                                            state.settings.set(&format!("window.{}.h", label), &size.height.to_string());
                                        }
                                    }
                                }
                            }
                        }

                    }
                }
            });

            // Separate task for boss fight auto-save (every 30s, on blocking thread)
            let handle_save = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if let Some(state) = handle_save.try_state::<AppState>() {
                        if state.data_storage.damage_generation() > 0 {
                            // Run on blocking thread to avoid starving the async runtime
                            // snapshot_boss_fights acquires the dps_calculator lock
                            // Run synchronously but only if lock is available
                            if let Some(mut calc) = state.dps_calculator.try_lock() {
                                let records = calc.snapshot_boss_fights();
                                drop(calc);
                                for record in &records {
                                    let _ = state.fight_history.save_fight(record);
                                }
                            }
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_version,
            get_dps_snapshot,
            get_skill_details,
            get_details_context,
            get_fight_history,
            save_fight,
            load_fight,
            delete_fight,
            export_fight_json,
            get_settings,
            update_settings,
            get_ping,
            get_capture_status,
            set_target_mode,
            set_character_name,
            bind_local_actor_id,
            bind_local_nickname,
            clear_settings,
            reset_combat,
            is_admin,
            set_language,
            set_debug_logging,
            set_packet_logging,
            get_aion2_window_title,
            debug_status,
            quit_app,
            open_url,
            read_cached_icon,
            write_cached_icon,
            resize_window,
            list_monitors,
            open_details_window,
            close_details_window,
            open_settings_window,
            close_settings_window,
            tool_window_ready,
            details_window_ready,
            capture_screenshot,
            start_drag,
            reset_auto_detection,
            get_available_devices,
            set_manual_device,
            replay_file,
            test_auto_hide,
            fetch_url,
            show_update_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
