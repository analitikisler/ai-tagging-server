//! Drafts: the console's edits collect in a draft of the container file,
//! kept in the shared state, and go live together when the draft is
//! published, like a GTM workspace. Until then visitors get the live
//! container, except a browser previewing the draft.
//!
//! A draft remembers the live revision it started from. Publishing checks
//! that live is still that revision, so a hand edit or another publish
//! made meanwhile isn't overwritten without saying so.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::container::Container;
use crate::container_file::{Applied, Change, ContainerFile, EditError, revision_of};
use crate::state::{State, now_ms};
use crate::tenants::{Resolved, TenantStats};

const KEY: &str = "drafts/default.json";

pub struct Draft {
    pub text: String,
    pub revision: String,
    /// The live revision the draft started from.
    pub base: String,
    pub updated_at: u64,
}

/// A live check as a hit sees it: when it began, whether its browser runs
/// the draft, and whether a preview's sends really go out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Session {
    pub since: u64,
    pub preview: bool,
    pub send: bool,
}

/// The live checks in the shared state, as last read.
#[derive(Default)]
struct Sessions {
    read_at: Option<Instant>,
    by_code: HashMap<String, Session>,
}

/// How long a code no session is known for stays unknown before the shared
/// state is read again. Any hit can carry a code, so a lookup must not cost
/// a read: with this, all of them together cost one a second at most, and a
/// preview started on another instance is found within that second.
const SESSIONS_FOR: Duration = Duration::from_secs(1);

pub struct Drafts {
    state: Arc<State>,
    file: Arc<ContainerFile>,
    /// The compiled draft, by revision, so previews don't compile per hit.
    compiled: Mutex<Option<(String, Arc<Container>)>>,
    /// Separate counters for preview hits, so they don't mix with live ones.
    stats: Arc<TenantStats>,
    storage: crate::sandbox::Storage,
    epoch: AtomicU64,
    /// Held across an edit or a publish, so two at once on this instance
    /// can't both read the same draft and the later one drop the earlier.
    write: Mutex<()>,
    /// The live checks a hit's code is looked up in.
    sessions: Mutex<Sessions>,
}

impl Drafts {
    pub fn new(state: Arc<State>, file: Arc<ContainerFile>) -> Self {
        Drafts {
            state,
            file,
            compiled: Mutex::default(),
            stats: Default::default(),
            storage: Default::default(),
            epoch: AtomicU64::new(1),
            write: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    /// The live check a hit's code names, from memory: a session doesn't
    /// change once started, so one that is known is never read again, and
    /// an unknown code leads to the shared state at most once a second.
    pub fn session(&self, code: &str) -> Option<Session> {
        self.session_at(code, Instant::now())
    }

    fn session_at(&self, code: &str, now: Instant) -> Option<Session> {
        let mut s = self.sessions.lock().expect("sessions");
        if let Some(found) = s.by_code.get(code) {
            return Some(*found);
        }
        let fresh = s
            .read_at
            .is_some_and(|at| now.saturating_duration_since(at) < SESSIONS_FOR);
        if fresh {
            return None;
        }
        s.read_at = Some(now);
        s.by_code = self
            .state
            .list("checkup")
            .iter()
            .filter(|name| name.starts_with("live-"))
            .filter_map(|name| {
                let v = self.state.read(&format!("checkup/{name}"))?;
                let session = Session {
                    since: v["since"].as_u64()?,
                    preview: v["preview"] == json!(true),
                    send: v["send"] == json!(true),
                };
                Some((v["code"].as_str()?.to_string(), session))
            })
            .collect();
        s.by_code.get(code).copied()
    }

    /// Forgets what was read, so the next lookup reads again: after a live
    /// check is started here, its first hit finds it.
    pub fn forget_sessions(&self) {
        *self.sessions.lock().expect("sessions") = Sessions::default();
    }

    pub fn get(&self) -> Option<Draft> {
        let v = self.state.read(KEY)?;
        let text = v["text"].as_str()?.to_string();
        Some(Draft {
            revision: revision_of(text.as_bytes()),
            base: v["base"].as_str().unwrap_or_default().to_string(),
            updated_at: v["updated_at"].as_u64().unwrap_or(0),
            text,
        })
    }

    /// The draft, compiled; None without one.
    pub fn container(&self) -> Option<Arc<Container>> {
        let d = self.get()?;
        let mut c = self.compiled.lock().expect("drafts");
        if let Some((rev, container)) = c.as_ref()
            && *rev == d.revision
        {
            return Some(container.clone());
        }
        let (_, container) = self
            .file
            .compute(&d.text, Change::Restore(d.text.clone()))
            .ok()?;
        let container = Arc::new(container);
        *c = Some((d.revision, container.clone()));
        self.epoch.fetch_add(1, Relaxed);
        Some(container)
    }

    /// What a preview runs: `container` (the draft, with platform tags when
    /// those are added), under its own id, counters and template storage.
    /// `dry` previews send nothing.
    pub fn resolved(&self, container: Arc<Container>, dry: bool) -> Resolved {
        Resolved {
            id: if dry { "preview-dry" } else { "preview" }.into(),
            // Workers rebuild their runtime when this changes.
            generation: self.epoch.load(Relaxed),
            container,
            storage: self.storage.clone(),
            stats: self.stats.clone(),
        }
    }

    /// Applies `change` to the draft, or to a new draft of the live file.
    /// `revision` is the draft's (or live's, without a draft). With `dry`,
    /// nothing is kept. A draft that ends up the same as live is dropped.
    pub fn edit(&self, revision: &str, change: Change, dry: bool) -> Result<Applied, EditError> {
        let _w = self.write.lock().expect("drafts");
        let (live, live_rev) = self.file.text()?;
        let (before, before_rev, base) = match self.get() {
            Some(d) => (d.text, d.revision, d.base),
            None => (live.clone(), live_rev.clone(), live_rev),
        };
        if revision != before_rev {
            return Err(EditError::Conflict(before_rev));
        }
        let (after, container) = self.file.compute(&before, change)?;
        let after_revision = revision_of(after.as_bytes());
        if !dry {
            // Back to what live has (formatting aside): no draft.
            let same = after == live
                || self
                    .file
                    .compute(&live, Change::Restore(live.clone()))
                    .is_ok_and(|(_, l)| crate::history::describe(&l, &container).is_empty());
            if same {
                self.discard();
            } else {
                self.state
                    .write(
                        KEY,
                        &json!({ "text": after, "base": base, "updated_at": now_ms() }),
                    )
                    .map_err(EditError::Io)?;
            }
        }
        Ok(Applied {
            before_revision: before_rev,
            after_revision,
            before,
            after,
            container,
        })
    }

    pub fn discard(&self) {
        self.state.remove(KEY);
    }

    /// Puts the draft live. `revision` is the draft's, so a publish never
    /// takes edits the person didn't see. Live must still be the revision
    /// the draft started from, unless `force`. With `dry`, only works it
    /// out, for the check before publishing.
    pub fn publish(&self, revision: &str, force: bool, dry: bool) -> Result<Applied, EditError> {
        let _w = self.write.lock().expect("drafts");
        let Some(d) = self.get() else {
            return Err(EditError::Invalid("there is no draft to publish".into()));
        };
        if revision != d.revision {
            return Err(EditError::Conflict(d.revision));
        }
        let (_, live_rev) = self.file.text()?;
        if live_rev != d.base && !force {
            return Err(EditError::Invalid(
                "the live container changed since this draft was started; discard the draft, or publish it over the change".into(),
            ));
        }
        let applied = self.file.apply(&live_rev, Change::Restore(d.text), dry)?;
        if !dry {
            self.discard();
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::Tag;
    use serde_json::Value;

    fn setup(name: &str) -> (std::path::PathBuf, Arc<ContainerFile>, Drafts) {
        let dir = std::env::temp_dir().join(format!("drafts-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        std::fs::write(
            &path,
            r#"{ "clients": [ { "name": "GA4", "template": "ga4_client" } ],
  "tags": [ { "name": "GA4", "template": "ga4_tag" } ] }"#,
        )
        .unwrap();
        let f = ContainerFile::open(&path, Default::default(), Default::default()).unwrap();
        let d = Drafts::new(Arc::new(State::memory()), f.clone());
        (path, f, d)
    }

    fn off() -> Tag {
        serde_json::from_value(json!({ "name": "GA4", "template": "ga4_tag", "enabled": false }))
            .unwrap()
    }

    #[test]
    fn a_hits_code_is_looked_up_in_memory() {
        let (_, _, d) = setup("sessions");
        let start = |code: &str, preview: bool, send: bool| {
            d.state
                .write(
                    &format!("checkup/live-{code}.json"),
                    &json!({ "code": code, "since": 1000, "preview": preview, "send": send }),
                )
                .unwrap();
        };
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        // The first lookup reads the shared state; the setup check's own
        // files there are not sessions.
        d.state
            .write("checkup/default.json", &json!({ "site_url": "" }))
            .unwrap();
        start("aaaaaaaa", true, false);
        let a = Session {
            since: 1000,
            preview: true,
            send: false,
        };
        assert_eq!(d.session_at("aaaaaaaa", t0), Some(a));

        // Codes nobody started: answered from memory, however many come.
        // A session that appears meanwhile shows that nothing was read.
        start("bbbbbbbb", false, true);
        for (i, code) in ["cccccccc", "dddddddd", "bbbbbbbb"].iter().enumerate() {
            assert_eq!(d.session_at(code, t0 + ms(100 * i as u64)), None, "{code}");
        }
        // A known session is never read again, even once the state is.
        assert_eq!(d.session_at("aaaaaaaa", t0 + ms(900)), Some(a));

        // A second later the state is read once more, and the session
        // another instance started is found.
        let b = d.session_at("bbbbbbbb", t0 + ms(1000)).unwrap();
        assert_eq!((b.preview, b.send), (false, true));

        // One started on this instance is found at once.
        start("eeeeeeee", true, true);
        assert_eq!(d.session_at("eeeeeeee", t0 + ms(1100)), None);
        d.forget_sessions();
        assert!(d.session_at("eeeeeeee", t0 + ms(1100)).is_some());

        // A file that isn't a session doesn't break the others.
        d.state
            .write("checkup/live-ffffffff.json", &json!({ "code": "ffffffff" }))
            .unwrap();
        d.forget_sessions();
        assert_eq!(d.session_at("ffffffff", t0 + ms(1200)), None);
        assert_eq!(d.session_at("aaaaaaaa", t0 + ms(1200)), Some(a));
    }

    #[test]
    fn edits_collect_in_the_draft_until_published() {
        let (path, f, d) = setup("publish");
        let live = std::fs::read_to_string(&path).unwrap();
        let rev = f.revision().unwrap();
        let a = d
            .edit(&rev, Change::Update("GA4".into(), off()), false)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            live,
            "live is untouched"
        );
        assert!(f.resolved().container.tags[0].enabled);
        let draft = d.get().unwrap();
        assert_eq!(draft.base, rev);
        assert!(!d.container().unwrap().tags[0].enabled);

        // The next edit names the draft's revision.
        let extra: Tag =
            serde_json::from_value(json!({ "name": "Second", "template": "ga4_tag" })).unwrap();
        assert!(matches!(
            d.edit(&rev, Change::Add(extra.clone()), false),
            Err(EditError::Conflict(_))
        ));
        d.edit(&a.after_revision, Change::Add(extra), false)
            .unwrap();

        let draft = d.get().unwrap();
        let published = d.publish(&draft.revision, false, false).unwrap();
        assert_eq!(published.before, live);
        let c = f.resolved().container;
        assert!(!c.tags[0].enabled);
        assert_eq!(c.tags[1].name, "Second");
        assert!(d.get().is_none(), "published drafts are gone");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_draft_back_to_live_is_dropped_and_a_stale_one_is_held() {
        let (path, f, d) = setup("stale");
        let rev = f.revision().unwrap();
        let a = d
            .edit(&rev, Change::Update("GA4".into(), off()), false)
            .unwrap();
        let on: Tag =
            serde_json::from_value(json!({ "name": "GA4", "template": "ga4_tag" })).unwrap();
        d.edit(&a.after_revision, Change::Update("GA4".into(), on), false)
            .unwrap();
        assert!(d.get().is_none(), "no difference from live, no draft");

        let a = d
            .edit(&rev, Change::Update("GA4".into(), off()), false)
            .unwrap();
        // Live changes underneath, by hand.
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"ga4_tag\" }", "\"ga4_tag\", \"x\": 1 }");
        std::fs::write(&path, &text).unwrap();
        assert!(
            matches!(d.publish(&a.after_revision, false, false), Err(EditError::Invalid(m)) if m.contains("changed since"))
        );
        // A dry publish works it out without writing.
        let dry = d.publish(&a.after_revision, true, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(d.get().is_some());
        d.publish(&a.after_revision, true, false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["tags"][0]["enabled"], false);
        assert_eq!(dry.after_revision, f.revision().unwrap());

        d.edit(
            &f.revision().unwrap(),
            Change::Update("GA4".into(), off()),
            false,
        )
        .ok();
        assert!(d.get().is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn previews_get_their_own_runtime_id() {
        let (path, f, d) = setup("preview");
        d.edit(
            &f.revision().unwrap(),
            Change::Update("GA4".into(), off()),
            false,
        )
        .unwrap();
        let c = d.container().unwrap();
        let r = d.resolved(c.clone(), true);
        assert_eq!(r.id, "preview-dry");
        assert!(!r.container.tags[0].enabled);
        assert_eq!(d.resolved(c, false).id, "preview");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
