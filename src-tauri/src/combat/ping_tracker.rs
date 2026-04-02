use parking_lot::Mutex;

use crate::capture::captured_payload::CapturedPayload;

/// .NET epoch offset: milliseconds between 0001-01-01 and 1970-01-01.
const DOTNET_EPOCH_OFFSET_MS: i64 = 62135596800000;
const MAX_PING_MS: i32 = 9999;
const MIN_PING_RS_BYTES: usize = 12;
const MAX_HISTORY: usize = 10_000;

pub struct PingTracker {
    inner: Mutex<Inner>,
}

struct Inner {
    last_ping: Option<i32>,
    history: Vec<(i64, i32)>,
}

impl PingTracker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                last_ping: None,
                history: Vec::new(),
            }),
        }
    }

    pub fn on_packet(&self, cap: &CapturedPayload) {
        if cap.data.len() >= MIN_PING_RS_BYTES {
            self.try_ping_rs(&cap.data, cap.captured_at_ms);
        }
    }

    fn try_ping_rs(&self, data: &[u8], arrival_ms: i64) {
        let mut i = 0;
        while i + MIN_PING_RS_BYTES <= data.len() {
            if data[i] == 0x03
                && data[i + 1] == 0x36
                && data[i + 2] == 0x00
                && data[i + 3] == 0x00
            {
                let client_sent_raw = read_i64_le(data, i + 4);
                let client_sent_unix_ms = client_sent_raw - DOTNET_EPOCH_OFFSET_MS;
                let rtt_ms = (arrival_ms - client_sent_unix_ms) as i32;

                if (1..=MAX_PING_MS).contains(&rtt_ms) {
                    let mut inner = self.inner.lock();
                    inner.last_ping = Some(rtt_ms);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    inner.history.push((now, rtt_ms));
                    if inner.history.len() > MAX_HISTORY {
                        inner.history.remove(0);
                    }
                }
                i += 12;
            } else {
                i += 1;
            }
        }
    }

    pub fn current_ping_ms(&self) -> Option<i32> {
        self.inner.lock().last_ping
    }

    pub fn get_ping_history(&self, start_ms: i64, end_ms: i64) -> Vec<(i64, i32)> {
        self.inner.lock().history.iter()
            .filter(|(ts, _)| *ts >= start_ms && *ts <= end_ms)
            .cloned()
            .collect()
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock();
        inner.last_ping = None;
        inner.history.clear();
    }
}

fn read_i64_le(data: &[u8], offset: usize) -> i64 {
    let mut v: i64 = 0;
    for j in 0..8 {
        v |= (data[offset + j] as i64) << (j * 8);
    }
    v
}
