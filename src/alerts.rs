//! Alerts when tracking breaks. A check compares the last hour of counts
//! with the same hour on each of the 7 days before: key events that drop or
//! stop, vendors refusing sends, tags failing, key values going missing,
//! hits no client claims, and an event name that changes spelling. Every
//! check needs a minimum volume, so a quiet hour doesn't raise an alarm.
//!
//! An alert opens when a check finds a problem, is posted to Slack (when a
//! webhook is set) and shown in the console, and resolves at the first
//! check that no longer finds it. It names the setup changes made in the 3
//! hours before it started, and with the AI on, carries a short diagnosis.
//!
//! Checks run from `POST /admin/api/alerts/check` (a Cloud Scheduler job on
//! Cloud Run) or a timer in the server. A lock in the shared state keeps two
//! instances from checking at once.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::counts::{BySite, Counts, QUARTERS_PER_DAY, Reader, Recorder, quarter_now, roll_up};
use crate::history::{History, sentence};
use crate::state::{State, now_ms};

/// Days a site needs counts for before volume checks run.
const LEARNING_DAYS: usize = 3;
const BASELINE_DAYS: u64 = 7;
/// Quarters in the checked window: one hour.
const WINDOW: u64 = 4;
/// How long a check may hold the lock.
const LOCK_MS: u64 = 120_000;
/// Most Slack messages one check sends; the rest are summed up in one.
const MAX_MESSAGES: usize = 5;
/// A worse alert is posted again at most this often.
const REPEAT_MS: u64 = 3_600_000;
/// Setup changes this long before an alert starts are named in it.
const RELATED_MS: u64 = 3 * 3_600_000;
/// Resolved alerts kept for the console.
const KEEP_RESOLVED: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// The events whose drop or stop raises an alert.
    pub key_events: Vec<String>,
    /// Language of Slack messages: en or tr.
    pub language: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            key_events: ["purchase", "generate_lead", "sign_up", "add_to_cart"]
                .map(String::from)
                .to_vec(),
            language: "en".into(),
        }
    }
}

/// A problem one check found.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub kind: &'static str,
    pub site: String,
    pub subject: String,
    /// "problem" or "warning".
    pub severity: &'static str,
    pub numbers: Value,
}

impl Finding {
    pub fn id(&self) -> String {
        format!("{}|{}|{}", self.kind, self.site, self.subject)
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

fn round(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// A name with case and separators taken out, to spot a changed spelling.
fn simplified(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The problems in one site's last hour, `now`, against the same hour on
/// earlier days that had hits, `base`.
pub fn evaluate(site: &str, now: &Counts, base: &[Counts], cfg: &Config) -> Vec<Finding> {
    let mut out = Vec::new();
    let learning = base.len() < LEARNING_DAYS;
    let f = |kind, subject: String, severity, numbers| Finding {
        kind,
        site: site.to_string(),
        subject,
        severity,
        numbers,
    };
    let usual =
        |pick: &dyn Fn(&Counts) -> u64| median(base.iter().map(|c| pick(c) as f64).collect());

    if !learning {
        for e in &cfg.key_events {
            let n = now.events.get(e).copied().unwrap_or(0) as f64;
            let u = usual(&|c| c.events.get(e).copied().unwrap_or(0));
            if u < 10.0 {
                continue;
            }
            if n == 0.0 {
                out.push(f(
                    "event_stopped",
                    e.clone(),
                    "problem",
                    json!({ "now": 0, "usual": round(u) }),
                ));
            } else if n <= 0.3 * u || (u >= 20.0 && n <= 0.5 * u) {
                let severity = if n <= 0.3 * u { "problem" } else { "warning" };
                let drop = round((1.0 - n / u) * 100.0);
                out.push(f(
                    "event_drop",
                    e.clone(),
                    severity,
                    json!({ "now": n, "usual": round(u), "drop_pct": drop }),
                ));
            }
        }
    }

    // Rates: against all earlier days pooled, or nothing while learning.
    let pooled = |pick: &dyn Fn(&Counts) -> (u64, u64)| {
        let (a, b) = base
            .iter()
            .map(pick)
            .fold((0, 0), |(x, y), (a, b)| (x + a, y + b));
        ratio(a, b)
    };
    let raised = |rate: f64, usual: f64, floor: f64| rate >= 0.2 && rate >= 2.0 * usual.max(floor);

    for (host, s) in &now.hosts {
        if s.sends < 20 {
            continue;
        }
        let rate = ratio(s.failed, s.sends);
        let u = pooled(&|c| c.hosts.get(host).map_or((0, 0), |h| (h.failed, h.sends)));
        if raised(rate, u, 0.02) {
            let top = s
                .errors
                .iter()
                .max_by_key(|(_, n)| **n)
                .map(|(e, _)| e.clone());
            let severity = if rate >= 0.5 { "problem" } else { "warning" };
            out.push(f(
                "vendor_failing",
                host.clone(),
                severity,
                json!({
                    "failed": s.failed, "sends": s.sends, "rate_pct": round(rate * 100.0),
                    "usual_pct": round(u * 100.0), "top_error": top,
                }),
            ));
        }
    }
    for (tag, r) in &now.tags {
        if r.runs < 20 {
            continue;
        }
        let rate = ratio(r.failed, r.runs);
        let u = pooled(&|c| c.tags.get(tag).map_or((0, 0), |t| (t.failed, t.runs)));
        if raised(rate, u, 0.02) {
            let severity = if rate >= 0.5 { "problem" } else { "warning" };
            out.push(f("tag_failing", tag.clone(), severity, json!({
                "failed": r.failed, "runs": r.runs, "rate_pct": round(rate * 100.0), "usual_pct": round(u * 100.0),
            })));
        }
    }

    // Values platforms need: on purchases, and an event id where Meta or
    // TikTok are sent to, which they use to match server and browser events.
    let capi = now
        .hosts
        .keys()
        .any(|h| h.ends_with("facebook.com") || h.contains("tiktok"));
    for e in &cfg.key_events {
        let n = now.events.get(e).copied().unwrap_or(0);
        if n < 10 {
            continue;
        }
        let mut keys: Vec<&str> = Vec::new();
        if e == "purchase" {
            keys.extend(["value", "currency"]);
        }
        if capi {
            keys.push("event_id");
        }
        for k in keys {
            let miss = |c: &Counts| {
                c.missing
                    .get(e)
                    .and_then(|m| m.get(k))
                    .copied()
                    .unwrap_or(0)
            };
            let rate = ratio(miss(now), n);
            let u = pooled(&|c| (miss(c), c.events.get(e).copied().unwrap_or(0)));
            if raised(rate, u, 0.05) {
                out.push(f("missing_value", format!("{e}.{k}"), "warning", json!({
                    "missing": miss(now), "events": n, "rate_pct": round(rate * 100.0), "usual_pct": round(u * 100.0),
                })));
            }
        }
    }

    if now.hits >= 20 {
        let rate = ratio(now.unclaimed, now.hits);
        let u = pooled(&|c| (c.unclaimed, c.hits));
        if raised(rate, u, 0.05) {
            let severity = if rate >= 0.5 { "problem" } else { "warning" };
            out.push(f("unclaimed", String::new(), severity, json!({
                "unclaimed": now.unclaimed, "hits": now.hits, "rate_pct": round(rate * 100.0), "usual_pct": round(u * 100.0),
            })));
        }
    }

    if !learning {
        let seen_before: BTreeSet<&String> = base.iter().flat_map(|c| c.events.keys()).collect();
        for (new, &n) in &now.events {
            if n < 10 || seen_before.contains(new) {
                continue;
            }
            for old in &seen_before {
                let u = usual(&|c| c.events.get(*old).copied().unwrap_or(0));
                let still = now.events.get(*old).copied().unwrap_or(0) as f64;
                if *old != new
                    && u >= 10.0
                    && simplified(old) == simplified(new)
                    && still <= 0.5 * u
                {
                    out.push(f("name_changed", format!("{old}→{new}"), "warning", json!({
                        "old": old, "new": new, "old_now": still, "old_usual": round(u), "new_now": n,
                    })));
                }
            }
        }
    }
    out
}

/// An alert's title and text, in English or Turkish.
pub fn words(kind: &str, subject: &str, n: &Value, lang: &str) -> (String, String) {
    let num = |k: &str| {
        let v = n[k].as_f64().unwrap_or(0.0);
        if v.fract() == 0.0 {
            format!("{}", v as i64)
        } else {
            format!("{v:.1}")
        }
    };
    let tr = lang == "tr";
    match kind {
        "event_stopped" if tr => (
            format!("{subject} durdu"),
            format!(
                "Son bir saatte hiç {subject} yok; bu saatte genellikle {} civarında.",
                num("usual")
            ),
        ),
        "event_stopped" => (
            format!("{subject} stopped"),
            format!(
                "No {subject} in the last hour; usually about {} at this time.",
                num("usual")
            ),
        ),
        "event_drop" if tr => (
            format!("{subject} %{} düştü", num("drop_pct")),
            format!(
                "Son bir saatte {} {subject}; bu saatte genellikle {} civarında.",
                num("now"),
                num("usual")
            ),
        ),
        "event_drop" => (
            format!("{subject} down {}%", num("drop_pct")),
            format!(
                "{} {subject} in the last hour; usually about {} at this time.",
                num("now"),
                num("usual")
            ),
        ),
        "vendor_failing" => {
            let err = n["top_error"]
                .as_str()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            if tr {
                (
                    format!("{subject} gönderimleri reddediyor"),
                    format!(
                        "Son bir saatte {} gönderimden {} tanesi başarısız (%{}; genellikle %{}){err}.",
                        num("sends"),
                        num("failed"),
                        num("rate_pct"),
                        num("usual_pct")
                    ),
                )
            } else {
                (
                    format!("{subject} is refusing sends"),
                    format!(
                        "{} of {} sends failed in the last hour ({}%; usually {}%){err}.",
                        num("failed"),
                        num("sends"),
                        num("rate_pct"),
                        num("usual_pct")
                    ),
                )
            }
        }
        "tag_failing" if tr => (
            format!("\"{subject}\" tag'i başarısız oluyor"),
            format!(
                "Son bir saatte {} çalışmadan {} tanesi başarısız, hata verdi veya zaman aşımına uğradı (%{}; genellikle %{}).",
                num("runs"),
                num("failed"),
                num("rate_pct"),
                num("usual_pct")
            ),
        ),
        "tag_failing" => (
            format!("Tag \"{subject}\" is failing"),
            format!(
                "{} of {} runs failed, threw or timed out in the last hour ({}%; usually {}%).",
                num("failed"),
                num("runs"),
                num("rate_pct"),
                num("usual_pct")
            ),
        ),
        "missing_value" => {
            let (event, key) = subject.split_once('.').unwrap_or((subject, ""));
            if tr {
                (
                    format!("{event} event'lerinde {key} eksik"),
                    format!(
                        "Son bir saatteki {} {event} event'inin {} tanesinde {key} yok (%{}; genellikle %{}).",
                        num("events"),
                        num("missing"),
                        num("rate_pct"),
                        num("usual_pct")
                    ),
                )
            } else {
                (
                    format!("{key} missing on {event}"),
                    format!(
                        "{} of {} {event} events in the last hour had no {key} ({}%; usually {}%).",
                        num("missing"),
                        num("events"),
                        num("rate_pct"),
                        num("usual_pct")
                    ),
                )
            }
        }
        "unclaimed" if tr => (
            "Sahipsiz hit'ler arttı".into(),
            format!(
                "Son bir saatte {} hit'ten {} tanesini hiçbir client almadı (%{}; genellikle %{}). Sık nedeni: sitede değişen bir measurement id.",
                num("hits"),
                num("unclaimed"),
                num("rate_pct"),
                num("usual_pct")
            ),
        ),
        "unclaimed" => (
            "Unclaimed hits are up".into(),
            format!(
                "No client claimed {} of {} hits in the last hour ({}%; usually {}%). A common cause is a changed measurement id on the site.",
                num("unclaimed"),
                num("hits"),
                num("rate_pct"),
                num("usual_pct")
            ),
        ),
        "name_changed" => {
            let (old, new) = (
                n["old"].as_str().unwrap_or(""),
                n["new"].as_str().unwrap_or(""),
            );
            if tr {
                (
                    format!("{old} artık {new} olarak geliyor"),
                    format!(
                        "Son bir saatte {} {new} geldi; önceki günlerde hiç yoktu. {old} ise {} oldu (genellikle {}). Bu adı bekleyen tag'ler çalışmaz.",
                        num("new_now"),
                        num("old_now"),
                        num("old_usual")
                    ),
                )
            } else {
                (
                    format!("{old} now arrives as {new}"),
                    format!(
                        "{} {new} in the last hour, never seen on earlier days, while {old} fell to {} (usually {}). Tags waiting for the old name won't fire.",
                        num("new_now"),
                        num("old_now"),
                        num("old_usual")
                    ),
                )
            }
        }
        _ => (subject.to_string(), String::new()),
    }
}

/// What a check needs from the console besides counts: the containers by
/// site, for the AI, and a way to explain an alert.
pub struct Context<'a> {
    pub explain: Option<
        &'a (
                dyn Fn(Value) -> futures_util::future::BoxFuture<'static, Option<Value>>
                    + Send
                    + Sync
            ),
    >,
}

pub struct Alerts {
    state: Arc<State>,
    history: Arc<History>,
    counts: Option<Arc<Recorder>>,
    slack: Option<String>,
    /// The console's address, for links in Slack.
    admin_url: Option<String>,
    http: reqwest::Client,
}

impl Alerts {
    pub fn new(
        state: Arc<State>,
        history: Arc<History>,
        counts: Option<Arc<Recorder>>,
        slack: Option<String>,
        admin_url: Option<String>,
    ) -> Self {
        Alerts {
            state,
            history,
            counts,
            slack,
            admin_url: admin_url.map(|u| u.trim_end_matches(['/', '#']).to_string()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
        }
    }

    /// Writes this instance's counts so far, before they are read.
    pub async fn flush_counts(&self) {
        if let Some(c) = &self.counts {
            let c = c.clone();
            let _ = tokio::task::spawn_blocking(move || c.flush()).await;
        }
    }

    pub fn slack_on(&self) -> bool {
        self.slack.is_some()
    }

    pub fn config(&self) -> Config {
        self.state
            .read("alerts/config.json")
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    pub fn set_config(&self, mut c: Config) -> Result<Config, String> {
        c.key_events = c
            .key_events
            .iter()
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if c.key_events.len() > 30 || c.key_events.iter().any(|e| e.chars().count() > 100) {
            return Err("at most 30 key events of at most 100 characters".into());
        }
        if !matches!(c.language.as_str(), "en" | "tr") {
            return Err("language must be en or tr".into());
        }
        self.state.write("alerts/config.json", &json!(c))?;
        Ok(c)
    }

    /// Open and recent alerts and how the last check went.
    pub fn view(&self) -> Value {
        let s = self
            .state
            .read("alerts/state.json")
            .unwrap_or_else(|| json!({}));
        let mut open: Vec<Value> = s["open"]
            .as_object()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        open.sort_by_key(|a| std::cmp::Reverse(a["started_at"].as_u64().unwrap_or(0)));
        json!({
            "open": open,
            "recent": s["recent"].as_array().cloned().unwrap_or_default(),
            "last_check": s["last_check"],
            "config": self.config(),
            "slack": self.slack_on(),
            "shared": self.state.is_shared(),
        })
    }

    fn lock(&self) -> bool {
        let me = &self.state.instance;
        let now = now_ms();
        if let Some(l) = self.state.read("alerts/lock.json")
            && l["until"].as_u64().unwrap_or(0) > now
            && l["instance"].as_str() != Some(me)
        {
            return false;
        }
        if self
            .state
            .write(
                "alerts/lock.json",
                &json!({ "instance": me, "until": now + LOCK_MS }),
            )
            .is_err()
        {
            return false;
        }
        // Whoever wrote last holds it.
        self.state
            .read("alerts/lock.json")
            .is_some_and(|l| l["instance"].as_str() == Some(me))
    }

    /// The findings for the last hour, and each site's days of history.
    fn find(state: &State, cfg: &Config, now_q: u64) -> (Vec<Finding>, BTreeMap<String, usize>) {
        roll_up(state, now_q / QUARTERS_PER_DAY);
        let mut r = Reader::new(state);
        let window: Vec<u64> = (now_q.saturating_sub(WINDOW)..now_q).collect();
        let now = r.quarters(window.iter().copied());
        let days: Vec<BySite> = (1..=BASELINE_DAYS)
            .map(|d| {
                r.quarters(
                    window
                        .iter()
                        .map(|q| q.saturating_sub(d * QUARTERS_PER_DAY)),
                )
            })
            .collect();
        let sites: BTreeSet<&String> = now
            .keys()
            .chain(days.iter().flat_map(|d| d.keys()))
            .collect();
        let mut findings = Vec::new();
        let mut history = BTreeMap::new();
        for site in sites {
            let base: Vec<Counts> = days
                .iter()
                .filter_map(|d| d.get(site))
                .filter(|c| c.hits > 0)
                .cloned()
                .collect();
            history.insert(site.clone(), base.len());
            let empty = Counts::default();
            findings.extend(evaluate(site, now.get(site).unwrap_or(&empty), &base, cfg));
        }
        (findings, history)
    }

    /// Runs the checks, opens and resolves alerts, and posts to Slack.
    pub async fn check(&self, ctx: Context<'_>) -> Value {
        if !self.lock() {
            return json!({ "skipped": "another check is running" });
        }
        let cfg = self.config();
        if let Some(c) = &self.counts {
            let c = c.clone();
            let _ = tokio::task::spawn_blocking(move || c.flush()).await;
        }
        let state = self.state.clone();
        let cfg2 = cfg.clone();
        let now_q = quarter_now();
        let (findings, days) =
            tokio::task::spawn_blocking(move || Self::find(&state, &cfg2, now_q))
                .await
                .unwrap_or_default();
        let result = self.reconcile(findings, &days, &cfg, ctx).await;
        self.state.remove("alerts/lock.json");
        result
    }

    async fn reconcile(
        &self,
        findings: Vec<Finding>,
        days: &BTreeMap<String, usize>,
        cfg: &Config,
        ctx: Context<'_>,
    ) -> Value {
        let now = now_ms();
        let mut s = self
            .state
            .read("alerts/state.json")
            .unwrap_or_else(|| json!({}));
        let mut open: BTreeMap<String, Value> = s["open"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let mut recent: Vec<Value> = s["recent"].as_array().cloned().unwrap_or_default();
        let mut posts: Vec<String> = Vec::new();
        let (mut opened, mut resolved, mut explained) = (0, 0, 0);
        let found: BTreeSet<String> = findings.iter().map(Finding::id).collect();

        for f in &findings {
            let id = f.id();
            let (en, tr) = (
                words(f.kind, &f.subject, &f.numbers, "en"),
                words(f.kind, &f.subject, &f.numbers, "tr"),
            );
            let texts = json!({ "en": { "title": en.0, "text": en.1 }, "tr": { "title": tr.0, "text": tr.1 } });
            match open.get_mut(&id) {
                Some(a) => {
                    let worse = a["severity"] == "warning" && f.severity == "problem";
                    a["severity"] = json!(f.severity);
                    a["numbers"] = f.numbers.clone();
                    a["texts"] = texts;
                    a["updated_at"] = json!(now);
                    if worse
                        && now.saturating_sub(a["notified_at"].as_u64().unwrap_or(0)) >= REPEAT_MS
                    {
                        a["notified_at"] = json!(now);
                        posts.push(self.post_text(a, cfg, "worse"));
                    }
                }
                None => {
                    let related: Vec<Value> = self
                        .history
                        .since(now.saturating_sub(RELATED_MS))
                        .into_iter()
                        .map(|e| {
                            let text: Vec<String> = e["edits"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(sentence)
                                .collect();
                            json!({ "at": e["at"], "source": e["source"], "text": text, "edits": e["edits"] })
                        })
                        .collect();
                    let mut a = json!({
                        "id": id, "kind": f.kind, "site": f.site, "subject": f.subject,
                        "severity": f.severity, "numbers": f.numbers, "texts": texts,
                        "started_at": now, "updated_at": now, "notified_at": now,
                        "related": related, "diagnosis": null,
                    });
                    if let Some(explain) = ctx.explain.filter(|_| explained < 3) {
                        explained += 1;
                        a["diagnosis"] = explain(a.clone()).await.unwrap_or(Value::Null);
                    }
                    posts.push(self.post_text(&a, cfg, "open"));
                    opened += 1;
                    open.insert(id, a);
                }
            }
        }
        let gone: Vec<String> = open
            .keys()
            .filter(|k| !found.contains(*k))
            .cloned()
            .collect();
        for id in gone {
            if let Some(mut a) = open.remove(&id) {
                a["resolved_at"] = json!(now);
                posts.push(self.post_text(&a, cfg, "resolved"));
                recent.insert(0, a);
                resolved += 1;
            }
        }
        recent.truncate(KEEP_RESOLVED);

        let (sent, slack_error) = self.post_all(posts, cfg).await;
        let learning: Vec<&String> = days
            .iter()
            .filter(|(_, n)| **n < LEARNING_DAYS)
            .map(|(s, _)| s)
            .collect();
        let last = json!({
            "at": now, "opened": opened, "resolved": resolved, "open": open.len(),
            "posted": sent, "slack_error": slack_error, "learning": learning, "days": days,
        });
        s["open"] = json!(open);
        s["recent"] = json!(recent);
        s["last_check"] = last.clone();
        if let Err(e) = self.state.write("alerts/state.json", &s) {
            tracing::warn!("alerts: state not saved: {e}");
        }
        last
    }

    /// The Slack text for an alert that opened, got worse or resolved.
    fn post_text(&self, a: &Value, cfg: &Config, what: &str) -> String {
        let lang = cfg.language.as_str();
        let t = &a["texts"][lang];
        let title = t["title"].as_str().unwrap_or_default();
        let site = a["site"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| format!(" · {s}"))
            .unwrap_or_default();
        let tr = lang == "tr";
        if what == "resolved" {
            let head = if tr { "Çözüldü" } else { "Resolved" };
            return format!("✅ *{head}: {title}*{site}");
        }
        let icon = if a["severity"] == "problem" {
            "🔴"
        } else {
            "🟠"
        };
        let mut out = format!(
            "{icon} *{title}*{site}\n{}",
            t["text"].as_str().unwrap_or_default()
        );
        if what == "worse" {
            out.insert_str(0, if tr { "Kötüleşti: " } else { "Worse: " });
        }
        for r in a["related"].as_array().into_iter().flatten().take(3) {
            let when = r["at"].as_u64().map(clock).unwrap_or_default();
            let text: Vec<&str> = r["text"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let lead = if tr {
                "Değişiklik"
            } else {
                "Possibly related"
            };
            out.push_str(&format!("\n{lead} ({when} UTC): {}", text.join("; ")));
        }
        let d = &a["diagnosis"];
        if let Some(cause) = d["likely_cause"].as_str().filter(|c| !c.is_empty()) {
            let lead = if tr {
                "Olası neden (AI)"
            } else {
                "Likely cause (AI)"
            };
            out.push_str(&format!("\n_{lead}:_ {cause}"));
            if let Some(first) = d["check_first"].as_str().filter(|c| !c.is_empty()) {
                let lead = if tr {
                    "Önce kontrol edin"
                } else {
                    "Check first"
                };
                out.push_str(&format!("\n_{lead}:_ {first}"));
            }
        }
        if let Some(url) = &self.admin_url {
            let link = if tr {
                "Uyarılar sayfasını aç"
            } else {
                "Open the Alerts page"
            };
            out.push_str(&format!("\n<{url}#alerts|{link}>"));
        }
        out
    }

    async fn post_all(&self, mut posts: Vec<String>, cfg: &Config) -> (usize, Option<String>) {
        if self.slack.is_none() || posts.is_empty() {
            return (0, None);
        }
        if posts.len() > MAX_MESSAGES {
            let more = posts.len() - (MAX_MESSAGES - 1);
            posts.truncate(MAX_MESSAGES - 1);
            posts.push(if cfg.language == "tr" {
                format!("…ve {more} uyarı daha; hepsi konsolun Uyarılar sayfasında.")
            } else {
                format!("…and {more} more alerts; see them all on the console's Alerts page.")
            });
        }
        let mut sent = 0;
        for p in posts {
            match self.post(&p).await {
                Ok(()) => sent += 1,
                Err(e) => {
                    tracing::warn!("alerts: Slack: {e}");
                    return (sent, Some(e));
                }
            }
        }
        (sent, None)
    }

    /// Posts one message to the Slack webhook.
    pub async fn post(&self, text: &str) -> Result<(), String> {
        let Some(url) = &self.slack else {
            return Err("no Slack webhook is set (SLACK_WEBHOOK_URL)".into());
        };
        let body = serde_json::to_vec(&json!({ "text": text })).unwrap_or_default();
        let r = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if r.status().is_success() {
            Ok(())
        } else {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            Err(format!(
                "Slack answered {status}: {}",
                text.chars().take(200).collect::<String>()
            ))
        }
    }
}

/// HH:MM in UTC.
fn clock(ms: u64) -> String {
    let s = ms / 1000 % 86_400;
    format!("{:02}:{:02}", s / 3600, s / 60 % 60)
}

#[cfg(test)]
mod tests;
