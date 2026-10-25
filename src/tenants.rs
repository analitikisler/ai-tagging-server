//! Multi-tenant hosting: each tenant owns one or more hostnames and its own
//! GA4 ids, site domains, script ids and origin check. Tenants live in a JSON
//! file the admin API edits and every instance re-reads when it changes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::container::{Container, Settings};
use crate::sandbox::{RunResult, Storage};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tenant {
    /// Lowercase letters, digits and dashes.
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Hostnames the tenant is served on, e.g. `example.com`.
    pub hosts: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// GA4 ids (`G-…`) hits are accepted for.
    pub measurement_ids: Vec<String>,
    /// GTM container and Google tag ids whose scripts are served.
    #[serde(default)]
    pub script_ids: Vec<String>,
    /// Domains whose pages may send hits. Empty means the request's host.
    #[serde(default)]
    pub site_domains: Vec<String>,
    /// `enforce`, `log` or `off`.
    #[serde(default = "log")]
    pub origin_check: String,
    /// The GA4 tag drops plain user-provided data before forwarding.
    #[serde(default)]
    pub strip_user_data: bool,
    /// The GA4 client's optional features.
    #[serde(default)]
    pub features: crate::features::Features,
}

fn yes() -> bool {
    true
}
fn log() -> String {
    "log".into()
}

#[derive(Serialize, Deserialize, Default)]
struct FileFormat {
    #[serde(default)]
    tenants: Vec<Tenant>,
}

impl Tenant {
    /// Lowercases and trims, then checks every field. Multi-tenant servers
    /// never accept `*` as a measurement id: that would make a tenant an
    /// open relay.
    pub fn validate(&mut self) -> Result<(), String> {
        let slug = |s: &str| {
            !s.is_empty()
                && s.len() <= 40
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        self.id = self.id.trim().to_ascii_lowercase();
        if !slug(&self.id) {
            return Err("id must be 1-40 characters: a-z, 0-9 and dashes".into());
        }
        self.name = self.name.trim().to_string();
        self.hosts = clean(&self.hosts, |h| h.to_ascii_lowercase());
        self.measurement_ids = clean(&self.measurement_ids, |h| h.to_ascii_uppercase());
        self.script_ids = clean(&self.script_ids, |h| h.to_ascii_uppercase());
        self.site_domains = clean(&self.site_domains, |h| h.to_ascii_lowercase());
        if self.hosts.is_empty() {
            return Err("at least one host is required".into());
        }
        if self
            .hosts
            .iter()
            .chain(&self.site_domains)
            .any(|h| !hostname(h))
        {
            return Err("hosts and site domains must be plain hostnames".into());
        }
        if self.measurement_ids.is_empty() {
            return Err("at least one GA4 measurement id (G-…) is required".into());
        }
        if self.measurement_ids.iter().any(|m| !tag_id(m, "G-")) {
            return Err("measurement ids look like G-ABC123".into());
        }
        if self
            .script_ids
            .iter()
            .any(|m| !(tag_id(m, "G-") || tag_id(m, "GTM-")))
        {
            return Err("script ids look like GTM-ABC123 or G-ABC123".into());
        }
        if !matches!(self.origin_check.as_str(), "enforce" | "log" | "off") {
            return Err("origin_check must be enforce, log or off".into());
        }
        self.features.validate()?;
        Ok(())
    }

    fn container(&self) -> Container {
        Container::builtin_with(&Settings {
            script_ids: self.script_ids.clone(),
            measurement_ids: self.measurement_ids.clone(),
            site_domains: self.site_domains.clone(),
            origin_check: self.origin_check.clone(),
            strip_user_data: self.strip_user_data,
            features: self.features.clone(),
        })
    }
}

fn clean(v: &[String], f: impl Fn(&str) -> String) -> Vec<String> {
    let mut seen = HashSet::new();
    v.iter()
        .map(|s| f(s.trim()))
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

fn hostname(h: &str) -> bool {
    h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn tag_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|r| {
        !r.is_empty() && r.len() <= 20 && r.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// What a worker needs to run a hit for one tenant.
#[derive(Clone)]
pub struct Resolved {
    pub id: String,
    /// Changes whenever the tenant's settings do, so workers rebuild.
    pub generation: u64,
    pub container: Arc<Container>,
    pub storage: Storage,
    pub stats: Arc<TenantStats>,
}

#[derive(Default)]
pub struct TenantStats {
    pub requests: AtomicU64,
    pub events: AtomicU64,
    pub status_2xx: AtomicU64,
    pub status_4xx: AtomicU64,
    pub status_5xx: AtomicU64,
    pub tag_failures: AtomicU64,
    pub tags_total: AtomicU64,
    pub sends_ok: AtomicU64,
    pub sends_failed: AtomicU64,
    pub tag_skips: AtomicU64,
    pub unclaimed: AtomicU64,
}

impl TenantStats {
    pub fn record(&self, r: &RunResult) {
        self.requests.fetch_add(1, Relaxed);
        self.events.fetch_add(r.events.len() as u64, Relaxed);
        let class = match r.response.status {
            200..=299 => Some(&self.status_2xx),
            400..=499 => Some(&self.status_4xx),
            500..=599 => Some(&self.status_5xx),
            _ => None,
        };
        if let Some(c) = class {
            c.fetch_add(1, Relaxed);
        }
        if r.claimed_by.is_none() {
            self.unclaimed.fetch_add(1, Relaxed);
        }
        let failed = r
            .tags
            .iter()
            .filter(|t| !matches!(t.outcome, "success" | "skipped"))
            .count();
        self.tag_failures.fetch_add(failed as u64, Relaxed);
        self.tags_total.fetch_add(r.tags.len() as u64, Relaxed);
        let ok = r
            .sends
            .iter()
            .filter(|s| s.result.as_ref().is_ok_and(|&status| status < 400))
            .count();
        self.sends_ok.fetch_add(ok as u64, Relaxed);
        self.sends_failed
            .fetch_add((r.sends.len() - ok) as u64, Relaxed);
        let skipped = r.tags.iter().filter(|t| t.outcome == "skipped").count();
        self.tag_skips.fetch_add(skipped as u64, Relaxed);
    }

    fn snapshot(&self) -> Value {
        json!({
            "requests": self.requests.load(Relaxed),
            "events": self.events.load(Relaxed),
            "status_2xx": self.status_2xx.load(Relaxed),
            "status_4xx": self.status_4xx.load(Relaxed),
            "status_5xx": self.status_5xx.load(Relaxed),
            "unclaimed": self.unclaimed.load(Relaxed),
            "tag_failures": self.tag_failures.load(Relaxed),
            "tags_total": self.tags_total.load(Relaxed),
            "sends_ok": self.sends_ok.load(Relaxed),
            "sends_failed": self.sends_failed.load(Relaxed),
            "tag_skips": self.tag_skips.load(Relaxed),
        })
    }
}

struct Live {
    tenant: Tenant,
    resolved: Resolved,
}

#[derive(Default)]
struct State {
    by_id: HashMap<String, Live>,
    by_host: HashMap<String, String>,
    modified: Option<SystemTime>,
}

pub struct Tenants {
    path: PathBuf,
    state: RwLock<State>,
    /// Storage and counters outlive edits of a tenant.
    storage: Mutex<HashMap<String, Storage>>,
    stats: Mutex<HashMap<String, Arc<TenantStats>>>,
    generation: AtomicU64,
    /// Serialises admin writes.
    write: Mutex<()>,
}

impl Tenants {
    pub fn open(path: &Path) -> Result<Arc<Self>, String> {
        let t = Arc::new(Tenants {
            path: path.to_path_buf(),
            state: RwLock::new(State::default()),
            storage: Mutex::default(),
            stats: Mutex::default(),
            generation: AtomicU64::new(1),
            write: Mutex::default(),
        });
        if !path.exists() {
            t.save(&[])?;
        }
        t.reload()?;
        Ok(t)
    }

    fn read_file(&self) -> Result<(Vec<Tenant>, Option<SystemTime>), String> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        let file: FileFormat =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", self.path.display()))?;
        let mut tenants = file.tenants;
        check_all(&mut tenants)?;
        Ok((tenants, modified))
    }

    /// Re-reads the file. A file that doesn't validate is refused and the
    /// tenants already running stay as they are.
    pub fn reload(&self) -> Result<(), String> {
        let (tenants, modified) = self.read_file()?;
        self.install(tenants, modified);
        Ok(())
    }

    /// Reloads when another instance or a person changed the file.
    pub fn reload_if_changed(&self) {
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if modified == self.state.read().expect("tenants").modified {
            return;
        }
        match self.reload() {
            Ok(()) => tracing::info!("tenants reloaded from {}", self.path.display()),
            Err(e) => {
                // Remember the bad version so the warning isn't repeated.
                self.state.write().expect("tenants").modified = modified;
                tracing::warn!("tenants file not reloaded: {e}");
            }
        }
    }

    fn install(&self, tenants: Vec<Tenant>, modified: Option<SystemTime>) {
        let mut state = self.state.write().expect("tenants");
        let mut next = State {
            modified,
            ..Default::default()
        };
        for t in tenants {
            // Unchanged tenants keep their generation, so workers keep their
            // compiled runtimes.
            let generation = match state.by_id.get(&t.id) {
                Some(old) if old.tenant == t => old.resolved.generation,
                _ => self.generation.fetch_add(1, Relaxed),
            };
            let storage = self
                .storage
                .lock()
                .expect("storage")
                .entry(t.id.clone())
                .or_default()
                .clone();
            let stats = self
                .stats
                .lock()
                .expect("stats")
                .entry(t.id.clone())
                .or_default()
                .clone();
            let resolved = match state.by_id.get(&t.id) {
                Some(old) if old.tenant == t => old.resolved.clone(),
                _ => Resolved {
                    id: t.id.clone(),
                    generation,
                    container: Arc::new(t.container()),
                    storage,
                    stats,
                },
            };
            if t.enabled {
                for h in &t.hosts {
                    next.by_host.insert(h.clone(), t.id.clone());
                }
            }
            next.by_id.insert(
                t.id.clone(),
                Live {
                    tenant: t,
                    resolved,
                },
            );
        }
        *state = next;
    }

    /// The tenant serving a request's `Host` header, if it is enabled.
    pub fn resolve(&self, host: &str) -> Option<Resolved> {
        let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
        let state = self.state.read().expect("tenants");
        let id = state.by_host.get(host.trim_end_matches('.'))?;
        state.by_id.get(id).map(|l| l.resolved.clone())
    }

    /// The tenant with this id, enabled or not.
    pub fn get(&self, id: &str) -> Option<Resolved> {
        let state = self.state.read().expect("tenants");
        state.by_id.get(id).map(|l| l.resolved.clone())
    }

    pub fn list(&self) -> Vec<Tenant> {
        let state = self.state.read().expect("tenants");
        let mut v: Vec<Tenant> = state.by_id.values().map(|l| l.tenant.clone()).collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn stats(&self) -> Value {
        let state = self.state.read().expect("tenants");
        let mut out = serde_json::Map::new();
        for (id, live) in &state.by_id {
            out.insert(id.clone(), live.resolved.stats.snapshot());
        }
        Value::Object(out)
    }

    /// Adds a tenant, or replaces the one with the same id (`create` makes
    /// replacing an error).
    pub fn put(&self, mut tenant: Tenant, create: bool) -> Result<Tenant, String> {
        tenant.validate()?;
        let _guard = self.write.lock().expect("write");
        // Pick up edits made to the file by hand before changing it.
        self.reload_if_changed();
        let mut all = self.list();
        let exists = all.iter().any(|t| t.id == tenant.id);
        if create && exists {
            return Err(format!("tenant {} already exists", tenant.id));
        }
        all.retain(|t| t.id != tenant.id);
        all.push(tenant.clone());
        check_all(&mut all)?;
        self.save(&all)?;
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        self.install(all, modified);
        Ok(tenant)
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let _guard = self.write.lock().expect("write");
        self.reload_if_changed();
        let mut all = self.list();
        let before = all.len();
        all.retain(|t| t.id != id);
        if all.len() == before {
            return Ok(false);
        }
        self.save(&all)?;
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        self.install(all, modified);
        Ok(true)
    }

    fn save(&self, tenants: &[Tenant]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&json!({ "tenants": tenants }))
            .map_err(|e| e.to_string())?;
        crate::store::save(&self.path, &text)
    }
}

/// Validates every tenant and makes sure ids and hosts are unique.
fn check_all(tenants: &mut [Tenant]) -> Result<(), String> {
    let mut ids = HashSet::new();
    let mut hosts: HashMap<String, String> = HashMap::new();
    for t in tenants.iter_mut() {
        t.validate().map_err(|e| format!("tenant {}: {e}", t.id))?;
        if !ids.insert(t.id.clone()) {
            return Err(format!("tenant {} appears twice", t.id));
        }
        for h in &t.hosts {
            if let Some(other) = hosts.insert(h.clone(), t.id.clone()) {
                return Err(format!("host {h} belongs to both {other} and {}", t.id));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(id: &str, host: &str) -> Tenant {
        Tenant {
            id: id.into(),
            name: String::new(),
            hosts: vec![host.into()],
            enabled: true,
            measurement_ids: vec!["g-abc123".into()],
            script_ids: vec![],
            site_domains: vec![],
            origin_check: "log".into(),
            strip_user_data: false,
            features: Default::default(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tenants-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("tenants.json")
    }

    #[test]
    fn validates_and_normalises() {
        let mut t = tenant("Shop-1", "Example.COM");
        t.validate().unwrap();
        assert_eq!(t.id, "shop-1");
        assert_eq!(t.hosts, ["example.com"]);
        assert_eq!(t.measurement_ids, ["G-ABC123"]);

        let mut t = tenant("a", "example.com");
        t.measurement_ids = vec!["*".into()];
        assert!(t.validate().is_err());
        let mut t = tenant("a", "exa mple.com");
        assert!(t.validate().is_err());
        let mut t = tenant("a/b", "example.com");
        assert!(t.validate().is_err());
        let mut t = tenant("a", "example.com");
        t.origin_check = "maybe".into();
        assert!(t.validate().is_err());
    }

    #[test]
    fn routes_by_host_and_persists() {
        let path = temp("routes");
        let _ = std::fs::remove_file(&path);
        let tenants = Tenants::open(&path).unwrap();
        assert!(tenants.resolve("example.com").is_none());
        tenants.put(tenant("one", "example.com"), true).unwrap();
        let mut two = tenant("two", "other.org");
        two.enabled = false;
        tenants.put(two, true).unwrap();
        assert_eq!(tenants.resolve("EXAMPLE.com:443").unwrap().id, "one");
        assert!(tenants.resolve("other.org").is_none(), "disabled");
        assert!(tenants.resolve("unknown.net").is_none());

        // A second instance reads the same file.
        let again = Tenants::open(&path).unwrap();
        assert_eq!(again.list().len(), 2);
        assert!(again.resolve("example.com").is_some());
    }

    #[test]
    fn host_belongs_to_one_tenant() {
        let tenants = Tenants::open(&temp("dupes")).unwrap();
        tenants.put(tenant("one", "example.com"), true).unwrap();
        assert!(tenants.put(tenant("two", "example.com"), true).is_err());
        assert!(
            tenants.put(tenant("one", "x.com"), true).is_err(),
            "create refuses replace"
        );
        assert_eq!(tenants.list().len(), 1);
    }

    #[test]
    fn edit_bumps_generation_and_delete_works() {
        let tenants = Tenants::open(&temp("edit")).unwrap();
        tenants.put(tenant("one", "example.com"), true).unwrap();
        let g1 = tenants.resolve("example.com").unwrap().generation;
        tenants.put(tenant("one", "example.com"), false).unwrap();
        assert_eq!(tenants.resolve("example.com").unwrap().generation, g1);
        let mut changed = tenant("one", "example.com");
        changed.origin_check = "enforce".into();
        tenants.put(changed, false).unwrap();
        assert_ne!(tenants.resolve("example.com").unwrap().generation, g1);
        assert!(tenants.delete("one").unwrap());
        assert!(!tenants.delete("one").unwrap());
        assert!(tenants.resolve("example.com").is_none());
    }

    #[test]
    fn bad_file_keeps_running_tenants() {
        let path = temp("bad");
        let _ = std::fs::remove_file(&path);
        let tenants = Tenants::open(&path).unwrap();
        tenants.put(tenant("one", "example.com"), true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "{ not json").unwrap();
        tenants.reload_if_changed();
        assert!(tenants.resolve("example.com").is_some());
    }
}
