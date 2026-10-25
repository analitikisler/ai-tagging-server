//! Where the server keeps what must outlive one instance: hourly counts,
//! container history, alerts and pinned test hits. With `STATE_DIR` it is a
//! folder every instance shares (on Cloud Run, a mounted Cloud Storage
//! bucket); without it, the same data is kept in this process's memory and
//! is gone when it stops.
//!
//! Paths are relative, `/`-separated and made only of letters, digits, `-`,
//! `_` and `.`, so nothing a caller passes can leave the folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub struct State {
    dir: Option<PathBuf>,
    memory: Mutex<BTreeMap<String, Value>>,
    /// Names this instance's own files, so instances never write the same one.
    pub instance: String,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn valid(rel: &str) -> bool {
    !rel.is_empty()
        && rel.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

impl State {
    /// A shared folder, made when missing.
    pub fn dir(path: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(path)?;
        Ok(State {
            dir: Some(path.to_path_buf()),
            ..Self::memory()
        })
    }

    pub fn memory() -> Self {
        let seed = format!("{}-{}", std::process::id(), now_ms());
        let id: String = Sha256::digest(seed.as_bytes())[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        State {
            dir: None,
            memory: Mutex::default(),
            instance: id,
        }
    }

    pub fn is_shared(&self) -> bool {
        self.dir.is_some()
    }

    pub fn read(&self, rel: &str) -> Option<Value> {
        if !valid(rel) {
            return None;
        }
        match &self.dir {
            Some(d) => {
                let bytes = std::fs::read(d.join(rel)).ok()?;
                serde_json::from_slice(&bytes).ok()
            }
            None => self.memory.lock().expect("state").get(rel).cloned(),
        }
    }

    /// Writes a whole file: a temporary file renamed over the old one, so a
    /// reader never sees half of it.
    pub fn write(&self, rel: &str, v: &Value) -> Result<(), String> {
        if !valid(rel) {
            return Err(format!("bad state path {rel:?}"));
        }
        match &self.dir {
            Some(d) => {
                let path = d.join(rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let tmp = path.with_extension(format!("tmp-{}", self.instance));
                let bytes = serde_json::to_vec(v).map_err(|e| e.to_string())?;
                std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
                std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
            }
            None => {
                self.memory
                    .lock()
                    .expect("state")
                    .insert(rel.to_string(), v.clone());
                Ok(())
            }
        }
    }

    /// Removes a file, or a folder and everything in it.
    pub fn remove(&self, rel: &str) {
        if !valid(rel) {
            return;
        }
        match &self.dir {
            Some(d) => {
                let path = d.join(rel);
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(path);
                } else {
                    let _ = std::fs::remove_file(path);
                }
            }
            None => {
                let prefix = format!("{rel}/");
                self.memory
                    .lock()
                    .expect("state")
                    .retain(|k, _| k != rel && !k.starts_with(&prefix));
            }
        }
    }

    /// The names directly inside a folder, sorted; temporary files left
    /// out.
    pub fn list(&self, rel: &str) -> Vec<String> {
        if !valid(rel) {
            return Vec::new();
        }
        let mut names: Vec<String> = match &self.dir {
            Some(d) => std::fs::read_dir(d.join(rel))
                .map(|it| {
                    it.filter_map(|e| e.ok())
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default(),
            None => {
                let prefix = format!("{rel}/");
                self.memory
                    .lock()
                    .expect("state")
                    .keys()
                    .filter_map(|k| k.strip_prefix(&prefix))
                    .map(|rest| rest.split('/').next().unwrap_or(rest).to_string())
                    .collect()
            }
        };
        names.retain(|n| !n.contains(".tmp-"));
        names.sort();
        names.dedup();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exercise(s: &State) {
        s.write("a/b/one.json", &json!({ "x": 1 })).unwrap();
        s.write("a/b/two.json", &json!(2)).unwrap();
        s.write("a/c.json", &json!(3)).unwrap();
        assert_eq!(s.read("a/b/one.json"), Some(json!({ "x": 1 })));
        assert_eq!(s.list("a"), vec!["b", "c.json"]);
        assert_eq!(s.list("a/b"), vec!["one.json", "two.json"]);
        s.remove("a/b");
        assert_eq!(s.list("a"), vec!["c.json"]);
        assert!(s.read("a/b/one.json").is_none());
        assert!(s.write("../x", &json!(1)).is_err());
        assert!(s.write("a//x", &json!(1)).is_err());
        assert!(s.read("/etc/passwd").is_none());
    }

    #[test]
    fn memory_and_folder_behave_the_same() {
        exercise(&State::memory());
        let dir = std::env::temp_dir().join(format!("state-{}", std::process::id()));
        exercise(&State::dir(&dir).unwrap());
        std::fs::remove_dir_all(dir).ok();
    }
}
