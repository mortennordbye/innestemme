//! Keeps the `dump-utterances` folder from growing without end: recordings older than
//! `dump-keep-days` are deleted once an hour. The age comes from the millisecond stamp in the file
//! name (`run-<ms>.wav`, `utterance-<ms>.wav`), not the file time, which a copy or an NFS volume
//! can change. Files with other names are never touched.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

const EVERY: Duration = Duration::from_secs(3600);

/// Deletes the recordings in `dir` made before `now - keep`; returns how many went.
pub fn prune(dir: &Path, keep: Duration, now: SystemTime) -> std::io::Result<usize> {
    let cutoff = now.checked_sub(keep).unwrap_or(UNIX_EPOCH).duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if stamp(&path).is_some_and(|ms| ms < cutoff) {
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) => warn!(%error, path = %path.display(), "could not delete an old recording"),
            }
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
        match tokio::task::spawn_blocking(move || prune(&folder, keep, SystemTime::now())).await {
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

    #[test]
    fn deletes_only_old_recordings() {
        let dir = std::env::temp_dir().join(format!("recordings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(10 * 86_400);
        let day = 86_400_000u128;
        let ms = |days_ago: u128| 10 * day - days_ago * day;
        let names = [
            format!("run-{}.wav", ms(8)),
            format!("utterance-{}.wav", ms(8)),
            format!("run-{}.wav", ms(6)),
            format!("utterance-{}.wav", ms(1)),
            "run-notanumber.wav".to_owned(),
            "speech.wav".to_owned(),
            format!("run-{}.txt", ms(8)),
        ];
        for name in &names {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let removed = prune(&dir, Duration::from_secs(7 * 86_400), now).unwrap();
        let mut left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(removed, 2);
        let mut expected = names[2..].to_vec();
        expected.sort();
        assert_eq!(left, expected);
    }

    #[test]
    fn missing_folder_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join("recordings-test-missing-folder");
        let error = prune(&dir, Duration::from_secs(1), SystemTime::now()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
