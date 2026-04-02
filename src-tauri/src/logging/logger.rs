use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::capture::captured_payload::CapturedPayload;

pub fn init_logging() {
    // Console layer: respects RUST_LOG env var, defaults to "info"
    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(console_filter);

    // File layer: captures ALL events (debug+) when enabled — no filtering
    // The DebugFileLayer checks DEBUG_ENABLED internally
    let file_filter = EnvFilter::new("debug");

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(DebugFileLayer.with_filter(file_filter))
        .init();
}

// ===== Debug File Logger (tracing layer) =====

static DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);
static DEBUG_WRITER: Mutex<Option<DebugFileWriter>> = Mutex::new(None);

struct DebugFileWriter {
    writer: std::io::BufWriter<std::fs::File>,
    bytes_written: u64,
}

const MAX_DEBUG_LOG_SIZE: u64 = 5 * 1024 * 1024; // 5 MB

/// Custom tracing layer that writes debug+ events to debug.log when enabled.
struct DebugFileLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DebugFileLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !DEBUG_ENABLED.load(Ordering::Relaxed) {
            return;
        }
        // Use try_lock to avoid deadlock if tracing is called while we hold the lock
        let mut guard = match DEBUG_WRITER.try_lock() {
            Some(g) => g,
            None => return,
        };
        let logger = match guard.as_mut() {
            Some(l) => l,
            None => return,
        };
        if logger.bytes_written > MAX_DEBUG_LOG_SIZE {
            return;
        }

        let now = chrono::Local::now().format("%H:%M:%S%.3f");
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARN",
            tracing::Level::INFO => "INFO",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::TRACE => "TRACE",
        };
        let module = event.metadata().module_path().unwrap_or("");
        // Extract short module name (last segment)
        let short_module = module.rsplit("::").next().unwrap_or(module);

        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);

        // Truncate long messages like the Kotlin version (240 chars max)
        let msg = if visitor.0.len() > 240 {
            format!("{}...", &visitor.0[..237])
        } else {
            visitor.0
        };

        let line = format!("{} {} {} - {}\n", now, level, short_module, msg);
        let len = line.len() as u64;
        if logger.writer.write_all(line.as_bytes()).is_ok() {
            logger.bytes_written += len;
            let _ = logger.writer.flush();
        }
    }
}

struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{:?}", value);
        } else if !self.0.is_empty() {
            self.0.push_str(&format!(" {}={:?}", field.name(), value));
        } else {
            self.0 = format!("{}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        } else if !self.0.is_empty() {
            self.0.push_str(&format!(" {}={}", field.name(), value));
        } else {
            self.0 = format!("{}={}", field.name(), value);
        }
    }
}

pub fn set_debug_enabled(enabled: bool, log_dir: &std::path::Path) {
    let prev = DEBUG_ENABLED.swap(enabled, Ordering::SeqCst);
    if enabled && !prev {
        let path = log_dir.join("debug.log");
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true).append(true).open(&path)
        {
            {
                let mut guard = DEBUG_WRITER.lock();
                let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
                *guard = Some(DebugFileWriter {
                    writer: std::io::BufWriter::new(file),
                    bytes_written: bytes,
                });
            } // guard dropped before tracing
            tracing::info!("Debug file logging started: {}", path.display());
        }
    } else if !enabled && prev {
        {
            let mut guard = DEBUG_WRITER.lock();
            *guard = None;
        } // guard dropped before tracing
        tracing::info!("Debug file logging stopped");
    }
}

pub fn is_debug_enabled() -> bool {
    DEBUG_ENABLED.load(Ordering::Relaxed)
}

// ===== Raw Packet Logger =====

static PACKET_LOG_ENABLED: AtomicBool = AtomicBool::new(false);
static PACKET_LOGGER: Mutex<Option<PacketFileLogger>> = Mutex::new(None);

struct PacketFileLogger {
    writer: std::io::BufWriter<std::fs::File>,
    path: PathBuf,
}

pub fn set_packet_log_enabled(enabled: bool, log_dir: &std::path::Path) {
    let prev = PACKET_LOG_ENABLED.swap(enabled, Ordering::SeqCst);
    if enabled && !prev {
        let now = chrono::Local::now();
        let stamp = now.format("%Y%m%d_%H%M%S");
        let path = log_dir.join(format!("packets_{}.txt", stamp));
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true).write(true).open(&path)
        {
            let mut writer = std::io::BufWriter::new(file);
            let header = format!(
                "# Packet capture started at {}\n# Format: TIMESTAMP|STREAMKEY|HEX_DATA\n\n",
                now.format("%+")
            );
            let _ = writer.write_all(header.as_bytes());
            let _ = writer.flush();
            {
                let mut guard = PACKET_LOGGER.lock();
                *guard = Some(PacketFileLogger { writer, path: path.clone() });
            }
            tracing::info!("Raw packet logging started: {}", path.display());
        }
    } else if !enabled && prev {
        let stopped_path = {
            let mut guard = PACKET_LOGGER.lock();
            let p = guard.as_ref().map(|l| l.path.display().to_string());
            *guard = None;
            p
        };
        if let Some(p) = stopped_path {
            tracing::info!("Raw packet logging stopped: {}", p);
        }
    }
}

pub fn is_packet_log_enabled() -> bool {
    PACKET_LOG_ENABLED.load(Ordering::Relaxed)
}

pub fn log_packet(cap: &CapturedPayload) {
    if !PACKET_LOG_ENABLED.load(Ordering::Relaxed) { return; }
    let mut guard = PACKET_LOGGER.lock();
    if let Some(ref mut logger) = *guard {
        let ts = chrono::Local::now().format("%+");
        let key = format!("Client:{}", cap.src_port);
        let hex: String = cap.data.iter().map(|b| format!("{:02X}", b)).collect();
        let line = format!("{}|{}|{}\n", ts, key, hex);
        if logger.writer.write_all(line.as_bytes()).is_ok() {
            let _ = logger.writer.flush();
        }
    }
}
