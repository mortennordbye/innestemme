//! Keeps the `dump-utterances` folder from growing without end: once an hour, recordings made more
//! than `dump-keep-days` before the newest one are deleted. Counting from the newest recording, not
//! from today, keeps the last days of real use through a week with nobody talking to it, so a
//! replay always has a day to run. The age comes from the millisecond stamp in the file name
//! (`run-<ms>.wav`, `utterance-<ms>.wav`), not the file time, which a copy or an NFS volume can
//! change. Files with other names are never touched.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{info, warn};

const EVERY: Duration = Duration::from_secs(3600);

/// Deletes the recordings in `dir` made more than `keep` before the newest; returns how many went.
pub fn prune(dir: &Path, keep: Duration) -> std::io::Result<usize> {
    let mut recordings = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if let Some(ms) = stamp(&path) {
            recordings.push((ms, path));
        }
    }
    let Some(newest) = recordings.iter().map(|(ms, _)| *ms).max() else { return Ok(0) };
    let cutoff = newest.saturating_sub(keep.as_millis());
    let mut removed = 0;
    for (_, path) in recordings.iter().filter(|(ms, _)| *ms < cutoff) {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(error) => warn!(%error, path = %path.display(), "could not delete an old recording"),
        }
    }
    Ok(removed)
}

/// The millisecond stamp of a recording's file name.
fn stamp(path: &Path) -> Option<u128> {
    let name = path.file_name()?.to_str()?.strip_suffix(".wav")?;
    let ms = name.strip_prefix("run-").or_else(|| name.strip_prefix("utterance-"))?;
    ms.parse().ok()
}

/// Prunes `dir` now and then every hour, for as long as the engine runs.
pub async fn keep(dir: PathBuf, days: u32) {
    let keep = Duration::from_secs(u64::from(days) * 86_400);
    let mut tick = tokio::time::interval(EVERY);
    loop {
        tick.tick().await;
        let folder = dir.clone();
        match tokio::task::spawn_blocking(move || prune(&folder, keep)).await {
            Ok(Ok(0)) => {}
            Ok(Ok(removed)) => info!(removed, days, "old recordings deleted"),
            // The folder appears with the first recording.
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(Err(error)) => warn!(%error, "could not prune the recordings"),
            Err(error) => warn!(%error, "recording pruning stopped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, files: &[String]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("recordings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for file in files {
            std::fs::write(dir.join(file), b"x").unwrap();
        }
        dir
    }

    fn left(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        std::fs::remove_dir_all(dir).unwrap();
        names
    }

    const DAY: u128 = 86_400_000;
    const WEEK: Duration = Duration::from_secs(7 * 86_400);

    #[test]
    fn deletes_only_recordings_a_week_older_than_the_newest() {
        let newest = 100 * DAY;
        let mut names = vec![
            format!("run-{}.wav", newest - 8 * DAY),
            format!("utterance-{}.wav", newest - 8 * DAY),
            format!("run-{}.wav", newest - 6 * DAY),
            format!("utterance-{newest}.wav"),
            "run-notanumber.wav".to_owned(),
            "speech.wav".to_owned(),
            format!("run-{}.txt", newest - 8 * DAY),
        ];
        let dir = folder("cut", &names);
        assert_eq!(prune(&dir, WEEK).unwrap(), 2);
        names.drain(..2);
        names.sort();
        assert_eq!(left(&dir), names);
    }

    #[test]
    fn a_quiet_week_keeps_the_last_days_of_use() {
        // Real clock: the newest recording is a month old, so it and the days before it all stay.
        let old = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() - 30 * DAY;
        let mut names = vec![format!("run-{old}.wav"), format!("run-{}.wav", old - 3 * DAY)];
        let dir = folder("quiet", &names);
        assert_eq!(prune(&dir, WEEK).unwrap(), 0);
        names.sort();
        assert_eq!(left(&dir), names);
    }

    #[test]
    fn missing_folder_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join("recordings-test-missing-folder");
        let error = prune(&dir, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
