//! Drafts: the console's edits collect in a draft of the container file,
//! kept in the shared state, and go live together when the draft is
//! published, like a GTM workspace. Until then visitors get the live
//! container, except a browser previewing the draft.
//!
//! A draft remembers the live revision it started from. Publishing checks
//! that live is still that revision, so a hand edit or another publish
//! made meanwhile isn't overwritten without saying so.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

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

pub struct Drafts {
    state: Arc<State>,
    file: Arc<ContainerFile>,
    /// The compiled draft, by revision, so previews don't compile per hit.
    compiled: Mutex<Option<(String, Arc<Container>)>>,
    /// Separate counters for preview hits, so they don't mix with live ones.
    stats: Arc<TenantStats>,
    storage: crate::sandbox::Storage,
    epoch: AtomicU64,
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
        }
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
