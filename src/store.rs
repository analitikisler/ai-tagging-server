//! Saving the files the admin page edits.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Writes `text` to a temporary file next to `path`, then renames it over
/// `path`, so readers never see half a file. Each save gets its own temporary
/// name: instances sharing a directory (all of them PID 1 in their
/// containers) never write into each other's file. Only the owner can read
/// the file, since it can hold access tokens.
pub fn save(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_file_name(format!(
        "{}.{:016x}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        unique()
    ));
    let written = write_private(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Random per process, different on every call.
fn unique() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(NEXT.fetch_add(1, Relaxed));
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_file_and_leaves_no_temporary_behind() {
        let dir = std::env::temp_dir().join(format!("store-test-{:x}", unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tenants.json");
        std::fs::write(&path, "old").unwrap();
        save(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tenants.json"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
