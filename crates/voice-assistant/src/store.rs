//! Files the engine keeps across restarts (timers, notes, learned corrections) and files it
//! downloads once (models). The rules are the same for each: a write never leaves half a file, and
//! a file that is there but does not read is never saved over.

use std::io::Write;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::warn;

/// The list kept in `path`, entry by entry. `what` names the file in the log ("kept timers").
///
/// - No file: an empty list.
/// - A file that is there but cannot be read (permissions, a disk error): an error, so the caller
///   does not save over what it may keep.
/// - A file that is not a JSON list at all: set aside under a name of its own (`<name>.bad-<unix
///   seconds>`), and an empty list.
/// - An entry that does not read (an older or a damaged file): only that entry is dropped.
pub fn read_list<T: DeserializeOwned>(path: &Path, what: &str) -> std::io::Result<Vec<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            warn!(%error, path = %path.display(), "could not read the {what}; they are not saved this run");
            return Err(error);
        }
    };
    let entries: Vec<serde_json::Value> = match serde_json::from_str(&text) {
        Ok(entries) => entries,
        Err(error) => {
            let aside = path.with_extension(format!("bad-{}", jiff::Timestamp::now().as_second()));
            warn!(%error, path = %path.display(), aside = %aside.display(), "could not read the {what}; set aside");
            let _ = std::fs::rename(path, aside);
            return Ok(Vec::new());
        }
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            serde_json::from_value(entry)
                .inspect_err(|error| warn!(%error, "an entry of the {what} did not read; dropped"))
                .ok()
        })
        .collect())
}

/// `value` as pretty JSON in `path`, through [`write_atomic`].
pub fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    write_atomic(path, &bytes)
}

/// `bytes` in `path`: written to `<name>.part` in the same folder, flushed to the disk and renamed,
/// so a crash leaves either the old file or the new one, never half of one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_aside(path, bytes, true)
}

/// As [`write_atomic`], without the flush to the disk: for caches written while an answer is
/// spoken, where a flush would delay the next sentence and a lost entry is only made again.
pub fn write_cache(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_aside(path, bytes, false)
}

fn write_aside(path: &Path, bytes: &[u8], sync: bool) -> std::io::Result<()> {
    let partial = path.with_extension("part");
    let mut file = std::fs::File::create(&partial)?;
    file.write_all(bytes)?;
    if sync {
        file.sync_all()?;
    }
    drop(file);
    std::fs::rename(&partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
    struct Entry {
        name: String,
        count: u32,
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("innestemme-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, count: u32) -> Entry {
        Entry { name: name.into(), count }
    }

    #[test]
    fn a_list_reads_back_as_written() {
        let dir = dir("round-trip");
        let path = dir.join("list.json");
        let list = vec![entry("one", 1), entry("two", 2)];
        write_json(&path, &list).unwrap();
        assert_eq!(read_list::<Entry>(&path, "entries").unwrap(), list);
        // Nothing is left beside it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        write_json(&path, &list[..1]).unwrap();
        assert_eq!(read_list::<Entry>(&path, "entries").unwrap(), list[..1]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_file_is_an_empty_list() {
        let dir = dir("missing");
        assert!(read_list::<Entry>(&dir.join("list.json"), "entries").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_error_and_stays() {
        let dir = dir("unreadable");
        // A directory where the file should be: there, but not readable as a file.
        let path = dir.join("list.json");
        std::fs::create_dir(&path).unwrap();
        assert!(read_list::<Entry>(&path, "entries").is_err());
        assert!(path.is_dir());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_that_is_not_a_list_is_set_aside() {
        let dir = dir("not-a-list");
        let path = dir.join("list.json");
        std::fs::write(&path, "{ broken").unwrap();
        assert!(read_list::<Entry>(&path, "entries").unwrap().is_empty());
        assert!(!path.exists());
        let aside: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(aside.len(), 1);
        assert!(aside[0].file_name().unwrap().to_string_lossy().starts_with("list.bad-"));
        assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), "{ broken");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_entry_that_does_not_read_loses_only_itself() {
        let dir = dir("bad-entry");
        let path = dir.join("list.json");
        std::fs::write(&path, r#"[{"name": "one", "count": 1}, {"name": 2}, {"name": "three", "count": 3}]"#).unwrap();
        assert_eq!(read_list::<Entry>(&path, "entries").unwrap(), vec![entry("one", 1), entry("three", 3)]);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cache_write_replaces_the_file() {
        let dir = dir("cache");
        let path = dir.join("a.pcm");
        write_cache(&path, b"one").unwrap();
        write_cache(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
