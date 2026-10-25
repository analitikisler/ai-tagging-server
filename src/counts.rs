//! What the hits did, counted per quarter hour, for the alerts: events by
//! name, key values missing from them, tag runs and failures, and sends by
//! vendor host with their errors. Counts only, never hit data.
//!
//! Each instance keeps the current quarter in memory and writes it, whole,
//! to `counts/q<quarter>/<instance>.json` in the shared state about once a
//! minute and when the quarter ends. A check combines the quarters of past
//! days into one file per day, `counts/d<day>.json`, and keeps 35 days.
//! Quarters and days are counted from the Unix epoch, in UTC.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::sandbox::RunResult;
use crate::state::{State, now_ms};

pub const QUARTER_S: u64 = 900;
pub const QUARTERS_PER_DAY: u64 = 96;
/// Event values whose absence is counted.
pub const KEYS: [&str; 4] = ["value", "currency", "transaction_id", "event_id"];
/// Days of counts kept.
const KEEP_DAYS: u64 = 35;
/// Most event names counted per quarter and site; the rest count as `(other)`.
const MAX_NAMES: usize = 300;
/// Most distinct errors kept per host.
const MAX_ERRORS: usize = 10;
/// How often the current quarter is written while hits arrive.
const FLUSH_MS: u64 = 60_000;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Runs {
    pub runs: u64,
    /// Failed, threw or timed out.
    pub failed: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sends {
    pub sends: u64,
    pub failed: u64,
    /// A failed send's status or error, with the start of the vendor's
    /// answer, and how often it came.
    #[serde(default)]
    pub errors: BTreeMap<String, u64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Counts {
    #[serde(default)]
    pub hits: u64,
    #[serde(default)]
    pub unclaimed: u64,
    #[serde(default)]
    pub events: BTreeMap<String, u64>,
    /// Event name to key to how many of its events lacked that key.
    #[serde(default)]
    pub missing: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default)]
    pub tags: BTreeMap<String, Runs>,
    #[serde(default)]
    pub hosts: BTreeMap<String, Sends>,
}

/// Counts by site: the tenant id, or "" outside multi-site mode.
pub type BySite = BTreeMap<String, Counts>;

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

impl Counts {
    pub fn add(&mut self, r: &RunResult) {
        self.hits += 1;
        if r.claimed_by.is_none() {
            self.unclaimed += 1;
        }
        for e in &r.events {
            let mut name = e["event_name"].as_str().unwrap_or("(no name)").to_string();
            if self.events.len() >= MAX_NAMES && !self.events.contains_key(&name) {
                name = "(other)".into();
            }
            *self.events.entry(name.clone()).or_default() += 1;
            for k in KEYS {
                let absent = match e.get(k) {
                    None | Some(Value::Null) => true,
                    Some(Value::String(s)) => s.trim().is_empty(),
                    _ => false,
                };
                if absent {
                    *self
                        .missing
                        .entry(name.clone())
                        .or_default()
                        .entry(k.into())
                        .or_default() += 1;
                }
            }
        }
        for t in &r.tags {
            let c = self.tags.entry(t.tag.clone()).or_default();
            c.runs += 1;
            if t.outcome != "success" {
                c.failed += 1;
            }
        }
        for s in &r.sends {
            let c = self.hosts.entry(host_of(&s.url).to_string()).or_default();
            c.sends += 1;
            let error = match &s.result {
                Ok(status) if *status < 400 => continue,
                Ok(status) => status.to_string(),
                Err(e) => e.chars().take(80).collect(),
            };
            c.failed += 1;
            let key = match &s.detail {
                Some(d) => format!("{error} {}", d.chars().take(80).collect::<String>()),
                None => error,
            };
            if c.errors.len() < MAX_ERRORS || c.errors.contains_key(&key) {
                *c.errors.entry(key).or_default() += 1;
            }
        }
    }

    pub fn merge(&mut self, o: &Counts) {
        self.hits += o.hits;
        self.unclaimed += o.unclaimed;
        for (k, n) in &o.events {
            *self.events.entry(k.clone()).or_default() += n;
        }
        for (e, keys) in &o.missing {
            let m = self.missing.entry(e.clone()).or_default();
            for (k, n) in keys {
                *m.entry(k.clone()).or_default() += n;
            }
        }
        for (t, r) in &o.tags {
            let c = self.tags.entry(t.clone()).or_default();
            c.runs += r.runs;
            c.failed += r.failed;
        }
        for (h, s) in &o.hosts {
            let c = self.hosts.entry(h.clone()).or_default();
            c.sends += s.sends;
            c.failed += s.failed;
            for (e, n) in &s.errors {
                if c.errors.len() < MAX_ERRORS || c.errors.contains_key(e) {
                    *c.errors.entry(e.clone()).or_default() += n;
                }
            }
        }
    }
}

pub fn merge_sites(into: &mut BySite, from: &BySite) {
    for (site, c) in from {
        into.entry(site.clone()).or_default().merge(c);
    }
}

pub fn quarter_now() -> u64 {
    now_ms() / 1000 / QUARTER_S
}

/// Counts this instance's hits and writes them to the shared state.
pub struct Recorder {
    state: Arc<State>,
    current: Mutex<Current>,
}

struct Current {
    quarter: u64,
    sites: BySite,
    written_at: u64,
}

impl Recorder {
    pub fn new(state: Arc<State>) -> Self {
        Recorder {
            state,
            current: Mutex::new(Current {
                quarter: quarter_now(),
                sites: BySite::new(),
                written_at: now_ms(),
            }),
        }
    }

    /// Counts one hit for `site`. Writing happens off the request's thread.
    pub fn record(&self, site: &str, r: &RunResult) {
        let (now_q, now) = (quarter_now(), now_ms());
        let mut writes = Vec::new();
        {
            let mut cur = self.current.lock().expect("counts");
            if cur.quarter != now_q {
                // The quarter that ended is written as it finally was.
                let ended = std::mem::take(&mut cur.sites);
                writes.push((cur.quarter, ended));
                cur.quarter = now_q;
                cur.written_at = now;
            }
            cur.sites.entry(site.to_string()).or_default().add(r);
            if now.saturating_sub(cur.written_at) >= FLUSH_MS {
                cur.written_at = now;
                writes.push((cur.quarter, cur.sites.clone()));
            }
        }
        for (q, sites) in writes {
            let state = self.state.clone();
            let write = move || write_quarter(&state, q, &sites);
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn_blocking(write);
                }
                Err(_) => write(),
            }
        }
    }

    /// Writes the current quarter now, at shutdown or before a check.
    pub fn flush(&self) {
        let (q, sites) = {
            let mut cur = self.current.lock().expect("counts");
            cur.written_at = now_ms();
            (cur.quarter, cur.sites.clone())
        };
        write_quarter(&self.state, q, &sites);
    }
}

fn write_quarter(state: &State, q: u64, sites: &BySite) {
    if sites.is_empty() {
        return;
    }
    let path = format!("counts/q{q}/{}.json", state.instance);
    if let Err(e) = state.write(&path, &json!({ "quarter": q, "sites": sites })) {
        tracing::warn!("counts: not written: {e}");
    }
}

fn parse_sites(v: &Value) -> BySite {
    serde_json::from_value(v["sites"].clone()).unwrap_or_default()
}

/// Reads counts, keeping each day's file once read.
pub struct Reader<'a> {
    state: &'a State,
    days: HashMap<u64, Value>,
}

impl<'a> Reader<'a> {
    pub fn new(state: &'a State) -> Self {
        Reader {
            state,
            days: HashMap::new(),
        }
    }

    /// Every instance's counts for quarter `q`.
    pub fn quarter(&mut self, q: u64) -> BySite {
        let mut out = BySite::new();
        let dir = format!("counts/q{q}");
        let files = self.state.list(&dir);
        if !files.is_empty() {
            for f in files {
                if let Some(v) = self.state.read(&format!("{dir}/{f}")) {
                    merge_sites(&mut out, &parse_sites(&v));
                }
            }
            return out;
        }
        let day = q / QUARTERS_PER_DAY;
        let state = self.state;
        let file = self.days.entry(day).or_insert_with(|| {
            state
                .read(&format!("counts/d{day}.json"))
                .unwrap_or(Value::Null)
        });
        parse_sites(&file["quarters"][(q % QUARTERS_PER_DAY).to_string()])
    }

    /// Several quarters added up.
    pub fn quarters(&mut self, qs: impl IntoIterator<Item = u64>) -> BySite {
        let mut out = BySite::new();
        for q in qs {
            let c = self.quarter(q);
            merge_sites(&mut out, &c);
        }
        out
    }
}

/// Combines the quarters of days before `today` into one file per day and
/// removes them, then drops days older than 35 days.
pub fn roll_up(state: &State, today: u64) {
    let mut by_day: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut days = Vec::new();
    for name in state.list("counts") {
        if let Some(q) = name.strip_prefix('q').and_then(|n| n.parse::<u64>().ok()) {
            if q / QUARTERS_PER_DAY < today {
                by_day.entry(q / QUARTERS_PER_DAY).or_default().push(q);
            }
        } else if let Some(d) = name
            .strip_prefix('d')
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u64>().ok())
        {
            days.push(d);
        }
    }
    for (day, qs) in by_day {
        let path = format!("counts/d{day}.json");
        let mut file = state
            .read(&path)
            .unwrap_or_else(|| json!({ "day": day, "quarters": {} }));
        for q in &qs {
            let mut sites = Reader::new(state).quarter(*q);
            let key = (q % QUARTERS_PER_DAY).to_string();
            merge_sites(&mut sites, &parse_sites(&file["quarters"][&key]));
            file["quarters"][&key] = json!({ "sites": sites });
        }
        if state.write(&path, &file).is_ok() {
            for q in qs {
                state.remove(&format!("counts/q{q}"));
            }
        }
        days.push(day);
    }
    for d in days {
        if d + KEEP_DAYS < today {
            state.remove(&format!("counts/d{d}.json"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{SendRecord, TagRun};

    fn hit(event: Value, tag_ok: bool, send: Result<u16, String>) -> RunResult {
        RunResult {
            claimed_by: Some("GA4".into()),
            events: vec![event],
            tags: vec![TagRun {
                tag: "Meta".into(),
                event: 0,
                outcome: if tag_ok { "success" } else { "failure" },
                error: None,
                skip: None,
            }],
            sends: vec![SendRecord {
                from: "Meta".into(),
                method: "POST".into(),
                url: "https://graph.facebook.com/v21.0/1/events?x=1".into(),
                result: send,
                detail: Some("{\"error\":\"Invalid OAuth access token\"}".into()),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn counts_events_missing_values_tags_and_sends() {
        let mut c = Counts::default();
        c.add(&hit(
            json!({ "event_name": "purchase", "value": 5, "currency": "" }),
            true,
            Ok(200),
        ));
        c.add(&hit(
            json!({ "event_name": "purchase", "currency": "EUR" }),
            false,
            Ok(400),
        ));
        c.add(&RunResult::default());
        assert_eq!(c.hits, 3);
        assert_eq!(c.unclaimed, 1);
        assert_eq!(c.events["purchase"], 2);
        assert_eq!(c.missing["purchase"]["value"], 1);
        assert_eq!(c.missing["purchase"]["currency"], 1);
        assert_eq!(c.missing["purchase"]["event_id"], 2);
        assert_eq!(c.tags["Meta"], Runs { runs: 2, failed: 1 });
        let h = &c.hosts["graph.facebook.com"];
        assert_eq!((h.sends, h.failed), (2, 1));
        assert_eq!(
            h.errors.keys().next().unwrap(),
            "400 {\"error\":\"Invalid OAuth access token\"}"
        );
        let mut twice = c.clone();
        twice.merge(&c);
        assert_eq!(twice.events["purchase"], 4);
        assert_eq!(
            twice.hosts["graph.facebook.com"].errors.values().next(),
            Some(&2)
        );
    }

    #[test]
    fn quarters_are_written_read_and_rolled_up_into_days() {
        let state = State::memory();
        let mut sites = BySite::new();
        sites.entry("".into()).or_default().add(&hit(
            json!({ "event_name": "purchase" }),
            true,
            Ok(200),
        ));
        let day = 20_000u64;
        let q1 = day * QUARTERS_PER_DAY + 40;
        write_quarter(&state, q1, &sites);
        // A second instance's file for the same quarter.
        state
            .write(
                &format!("counts/q{q1}/other.json"),
                &json!({ "quarter": q1, "sites": sites }),
            )
            .unwrap();
        assert_eq!(Reader::new(&state).quarter(q1)[""].events["purchase"], 2);

        roll_up(&state, day + 1);
        assert!(state.list(&format!("counts/q{q1}")).is_empty());
        assert_eq!(state.list("counts"), vec![format!("d{day}.json")]);
        assert_eq!(
            Reader::new(&state).quarter(q1)[""].events["purchase"],
            2,
            "read back from the day file"
        );
        assert_eq!(Reader::new(&state).quarters([q1, q1 + 1])[""].hits, 2);

        roll_up(&state, day + 40);
        assert!(state.list("counts").is_empty(), "older than 35 days");
    }

    #[test]
    fn the_recorder_writes_its_quarter() {
        let state = Arc::new(State::memory());
        let r = Recorder::new(state.clone());
        r.record(
            "site-a",
            &hit(json!({ "event_name": "lead" }), true, Ok(200)),
        );
        r.flush();
        let q = quarter_now();
        assert_eq!(Reader::new(&state).quarter(q)["site-a"].events["lead"], 1);
    }
}
