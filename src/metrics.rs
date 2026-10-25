//! Process-wide counters behind `/system`. Plain atomics, so counting a hit
//! never takes a lock or allocates.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use serde_json::{Value, json};

use crate::sandbox::RunResult;

/// Upper bounds, in milliseconds, of the latency buckets; a last bucket
/// takes everything slower.
const BUCKETS_MS: [u64; 9] = [1, 5, 10, 25, 50, 100, 250, 1000, 5000];

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub status_2xx: AtomicU64,
    pub status_3xx: AtomicU64,
    pub status_4xx: AtomicU64,
    pub status_5xx: AtomicU64,
    /// Turned away with 503 because every worker was busy and the queue full.
    pub rejected_busy: AtomicU64,
    pub in_flight: AtomicU64,
    /// Hits for a host no tenant serves.
    pub unknown_host: AtomicU64,
    pub unclaimed: AtomicU64,
    pub client_errors: AtomicU64,
    pub events: AtomicU64,
    pub tags_success: AtomicU64,
    pub tags_failure: AtomicU64,
    /// Tags that chose not to send (unmapped event, no ad consent).
    pub tags_skipped: AtomicU64,
    pub tags_timeout: AtomicU64,
    pub sends_ok: AtomicU64,
    pub sends_failed: AtomicU64,
    pub send_retries: AtomicU64,
    latency_sum_us: AtomicU64,
    latency: [AtomicU64; BUCKETS_MS.len() + 1],
}

pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::default);
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Starts the uptime clock; call once at startup.
pub fn init() {
    LazyLock::force(&STARTED);
}

pub fn uptime() -> std::time::Duration {
    STARTED.elapsed()
}

impl Metrics {
    /// Counts a finished hit: its response status, outcome and total latency.
    pub fn record(&self, r: &RunResult, elapsed: std::time::Duration) {
        self.requests.fetch_add(1, Relaxed);
        match r.response.status {
            200..=299 => &self.status_2xx,
            300..=399 => &self.status_3xx,
            400..=499 => &self.status_4xx,
            _ => &self.status_5xx,
        }
        .fetch_add(1, Relaxed);
        if r.claimed_by.is_none() {
            self.unclaimed.fetch_add(1, Relaxed);
        }
        if r.client_error.is_some() {
            self.client_errors.fetch_add(1, Relaxed);
        }
        self.events.fetch_add(r.events.len() as u64, Relaxed);
        for t in &r.tags {
            match t.outcome {
                "success" => &self.tags_success,
                "failure" => &self.tags_failure,
                "skipped" => &self.tags_skipped,
                _ => &self.tags_timeout,
            }
            .fetch_add(1, Relaxed);
        }
        for s in &r.sends {
            match &s.result {
                Ok(status) if *status < 400 => &self.sends_ok,
                _ => &self.sends_failed,
            }
            .fetch_add(1, Relaxed);
        }
        let ms = elapsed.as_millis() as u64;
        let slot = BUCKETS_MS
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(BUCKETS_MS.len());
        self.latency[slot].fetch_add(1, Relaxed);
        self.latency_sum_us
            .fetch_add(elapsed.as_micros() as u64, Relaxed);
    }

    pub fn snapshot(&self) -> Value {
        let n = |c: &AtomicU64| c.load(Relaxed);
        let counts: Vec<u64> = self.latency.iter().map(n).collect();
        let timed: u64 = counts.iter().sum();
        // Upper bound of the bucket holding the given share of hits.
        let quantile = |q: f64| -> Value {
            if timed == 0 {
                return Value::Null;
            }
            let target = (timed as f64 * q).ceil() as u64;
            let mut seen = 0;
            for (i, c) in counts.iter().enumerate() {
                seen += c;
                if seen >= target {
                    return BUCKETS_MS.get(i).map_or(Value::Null, |b| json!(b));
                }
            }
            Value::Null
        };
        let mut buckets = serde_json::Map::new();
        for (i, c) in counts.iter().enumerate() {
            let label = BUCKETS_MS
                .get(i)
                .map_or_else(|| "over_5000".to_string(), |b| format!("le_{b}"));
            buckets.insert(label, json!(c));
        }
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": STARTED.elapsed().as_secs(),
            "requests": {
                "total": n(&self.requests),
                "in_flight": n(&self.in_flight),
                "rejected_busy": n(&self.rejected_busy),
                "unknown_host": n(&self.unknown_host),
                "unclaimed": n(&self.unclaimed),
                "client_errors": n(&self.client_errors),
                "events": n(&self.events),
                "by_status": {
                    "2xx": n(&self.status_2xx),
                    "3xx": n(&self.status_3xx),
                    "4xx": n(&self.status_4xx),
                    "5xx": n(&self.status_5xx),
                },
            },
            "tags": {
                "success": n(&self.tags_success),
                "failure": n(&self.tags_failure),
                "skipped": n(&self.tags_skipped),
                "timeout": n(&self.tags_timeout),
            },
            "sends": {
                "ok": n(&self.sends_ok),
                "failed": n(&self.sends_failed),
                "retries": n(&self.send_retries),
            },
            "latency_ms": {
                "mean": if timed == 0 { Value::Null } else { json!(n(&self.latency_sum_us) as f64 / timed as f64 / 1000.0) },
                // Bucket upper bounds, not exact values; null means over 5000.
                "p50_le": quantile(0.5),
                "p95_le": quantile(0.95),
                "p99_le": quantile(0.99),
                "buckets": buckets,
            },
        })
    }
}
