mod bucket;
mod lock;

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub use self::bucket::{Bucket, Usage, decode};
pub use self::lock::RecorderLock;
use crate::span::{HOUR_MS, MINUTE_MS};

const CURRENT_FILE: &str = "current.tsv";
const STAGING_SUFFIX: &str = ".tmp";

/// A file the recorder keeps in its data folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recording {
    /// The finished buckets of one hour, named by hours since the Unix epoch.
    Hour(i64),
    /// The bucket in progress.
    Current,
    /// A replacement being written, before it is renamed over its file.
    Staging,
}

impl Recording {
    fn of_file(name: &str) -> Option<Self> {
        if name == CURRENT_FILE {
            return Some(Recording::Current);
        }
        if let Some(replaced) = name.strip_suffix(STAGING_SUFFIX) {
            return Recording::of_file(replaced).map(|_| Recording::Staging);
        }
        let hour = name.strip_prefix('h')?.strip_suffix(".tsv")?.parse().ok()?;
        Some(Recording::Hour(hour))
    }
}

/// Wall-clock milliseconds since the Unix epoch, the clock buckets are named by.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn hour_file(dir: &Path, minute: i64) -> PathBuf {
    dir.join(format!("h{}.tsv", minute * MINUTE_MS / HOUR_MS))
}

fn hour_end_ms(hour: i64) -> i64 {
    (hour + 1) * HOUR_MS
}

/// Every recording in `dir` with its path; none when the folder does not exist.
fn recordings(dir: &Path) -> Result<Vec<(Recording, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        if let Some(kind) = entry.file_name().to_str().and_then(Recording::of_file) {
            found.push((kind, entry.path()));
        }
    }
    Ok(found)
}

/// The hour files in `dir` that end at or before `end_ms`.
fn hours_ended_by(dir: &Path, end_ms: i64) -> Result<Vec<PathBuf>> {
    let ended = recordings(dir)?
        .into_iter()
        .filter_map(|(kind, path)| match kind {
            Recording::Hour(hour) if hour_end_ms(hour) <= end_ms => Some(path),
            _ => None,
        })
        .collect();
    Ok(ended)
}

/// The file's text, or `None` when it does not exist, as the current file
/// briefly does not while the recorder renames a new one over it.
fn read_if_present(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replaces the file's contents in one step: a reader sees the old text or
/// the new, never a mix, and a crash midway leaves the old file in place.
fn replace_file(path: &Path, contents: &str) -> Result<()> {
    let mut staging = path.as_os_str().to_owned();
    staging.push(STAGING_SUFFIX);
    let staging = PathBuf::from(staging);
    fs::write(&staging, contents).with_context(|| format!("writing {}", staging.display()))?;
    fs::rename(&staging, path).with_context(|| format!("replacing {}", path.display()))
}

/// `archived` and every bucket of `current` it does not already hold.
fn with_unarchived(mut archived: Vec<Bucket>, current: Vec<Bucket>) -> Vec<Bucket> {
    for bucket in current {
        if !archived.iter().any(|b| b.is_same_as(&bucket)) {
            archived.push(bucket);
        }
    }
    archived
}

/// Appends a finished bucket to its hour file.
pub fn append_bucket(dir: &Path, bucket: &Bucket) -> Result<()> {
    let target = hour_file(dir, bucket.minute);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)
        .with_context(|| format!("opening {}", target.display()))?;
    file.write_all(bucket.encode().as_bytes())
        .with_context(|| format!("writing {}", target.display()))
}

/// Replaces the current file with the bucket still being filled, so reports
/// see the latest minute and a restarted recorder can adopt it.
pub fn write_current(dir: &Path, bucket: &Bucket) -> Result<()> {
    replace_file(&dir.join(CURRENT_FILE), &bucket.encode())
}

/// Whether the current file is missing, as it is after a clear removed it.
pub fn current_missing(dir: &Path) -> bool {
    !dir.join(CURRENT_FILE).exists()
}

/// Moves a bucket an earlier recorder left in the current file into its hour
/// file, unless that recorder already appended it there.
pub fn adopt_current(dir: &Path) -> Result<()> {
    let current = dir.join(CURRENT_FILE);
    let Some(text) = read_if_present(&current)? else {
        return Ok(());
    };
    for bucket in decode(&text) {
        let archived = read_if_present(&hour_file(dir, bucket.minute))?
            .map(|hour| decode(&hour))
            .unwrap_or_default();
        if !archived.iter().any(|b| b.is_same_as(&bucket)) {
            append_bucket(dir, &bucket)?;
        }
    }
    fs::remove_file(&current).with_context(|| format!("removing {}", current.display()))
}

/// Every recorded bucket that starts at or after `since_ms`, the minute in
/// progress included.
pub fn load(dir: &Path, since_ms: i64) -> Result<Vec<Bucket>> {
    let mut archived = Vec::new();
    let mut current = Vec::new();
    for (kind, path) in recordings(dir)? {
        let into = match kind {
            Recording::Hour(hour) if hour_end_ms(hour) > since_ms => &mut archived,
            Recording::Current => &mut current,
            Recording::Hour(_) | Recording::Staging => continue,
        };
        let Some(text) = read_if_present(&path)? else {
            continue;
        };
        into.extend(
            decode(&text)
                .into_iter()
                .filter(|b| b.minute * MINUTE_MS >= since_ms),
        );
    }
    // The minute in progress is also in an hour file once the recorder has finished it.
    Ok(with_unarchived(archived, current))
}

/// Rewrites each hour file that ends at or before `before_ms` as a single
/// bucket for the whole hour, so old recordings keep their totals and peaks
/// in a sixtieth of the space.
pub fn compact(dir: &Path, before_ms: i64) -> Result<()> {
    for path in hours_ended_by(dir, before_ms)? {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let buckets = decode(&text);
        if buckets.len() < 2 {
            continue;
        }
        if let Some(hour) = Bucket::merge(&buckets) {
            replace_file(&path, &hour.encode())?;
        }
    }
    Ok(())
}

/// Removes hour files that end at or before `keep_from_ms`.
pub fn prune(dir: &Path, keep_from_ms: i64) -> Result<()> {
    for path in hours_ended_by(dir, keep_from_ms)? {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// What a clear removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cleared {
    pub files: usize,
    pub bytes: u64,
}

/// Removes every recording in the folder, leaving the lock and any other
/// file alone. A running recorder keeps its lock and goes on recording from now.
pub fn clear(dir: &Path) -> Result<Cleared> {
    let mut cleared = Cleared::default();
    for (_, path) in recordings(dir)? {
        let bytes = fs::metadata(&path).map_or(0, |m| m.len());
        match fs::remove_file(&path) {
            Ok(()) => {
                cleared.files += 1;
                cleared.bytes += bytes;
            }
            // The recorder replaces the current file by renaming over it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::accounting::Tally;

    /// A fresh, empty folder for one test, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("pw-{name}-{}", std::process::id()));
            if let Err(e) = fs::remove_dir_all(&dir) {
                assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
            }
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn bucket_at(ms: i64, name: &str, cpu_ns: i64) -> Bucket {
        let mut bucket = Bucket::open(ms, 2);
        let tally = Tally {
            cpu_ns,
            mem_bytes: 10,
            procs: 1,
        };
        bucket.add(1000, &BTreeMap::from([(name.to_owned(), tally)]));
        bucket
    }

    #[test]
    fn file_names_say_what_they_record() {
        assert_eq!(Recording::of_file("h42.tsv"), Some(Recording::Hour(42)));
        assert_eq!(Recording::of_file("current.tsv"), Some(Recording::Current));
        assert_eq!(
            Recording::of_file("current.tsv.tmp"),
            Some(Recording::Staging)
        );
        assert_eq!(Recording::of_file("h42.tsv.tmp"), Some(Recording::Staging));
        assert_eq!(Recording::of_file("recorder.lock"), None);
        assert_eq!(Recording::of_file("hx.tsv"), None);
        assert_eq!(Recording::of_file("notes.tmp"), None);
    }

    #[test]
    fn loading_a_missing_folder_finds_nothing() {
        let dir = std::env::temp_dir().join("pw-never-created-folder");
        assert!(load(&dir, 0).unwrap().is_empty());
        assert_eq!(clear(&dir).unwrap(), Cleared::default());
    }

    #[test]
    fn loading_counts_the_minute_in_progress_once() {
        let scratch = Scratch::new("load-current");
        let finished = bucket_at(HOUR_MS, "a", 5);
        let in_progress = bucket_at(HOUR_MS + MINUTE_MS, "a", 7);
        append_bucket(&scratch.0, &finished).unwrap();
        write_current(&scratch.0, &finished).unwrap();
        assert_eq!(
            load(&scratch.0, 0).unwrap(),
            std::slice::from_ref(&finished)
        );
        write_current(&scratch.0, &in_progress).unwrap();
        assert_eq!(load(&scratch.0, 0).unwrap(), [finished, in_progress]);
    }

    #[test]
    fn loading_skips_buckets_before_the_period() {
        let scratch = Scratch::new("load-since");
        append_bucket(&scratch.0, &bucket_at(HOUR_MS, "old", 1)).unwrap();
        append_bucket(&scratch.0, &bucket_at(2 * HOUR_MS, "early", 1)).unwrap();
        append_bucket(
            &scratch.0,
            &bucket_at(2 * HOUR_MS + 30 * MINUTE_MS, "late", 1),
        )
        .unwrap();
        let loaded = load(&scratch.0, 2 * HOUR_MS + MINUTE_MS).unwrap();
        let names: Vec<&String> = loaded.iter().flat_map(|b| b.usage.keys()).collect();
        assert_eq!(names, ["late"]);
    }

    #[test]
    fn compacting_an_old_hour_keeps_its_totals_and_peaks() {
        let scratch = Scratch::new("compact");
        let first = bucket_at(7 * HOUR_MS, "a", 6);
        let second = bucket_at(7 * HOUR_MS + MINUTE_MS, "a", 30);
        append_bucket(&scratch.0, &first).unwrap();
        append_bucket(&scratch.0, &second).unwrap();
        compact(&scratch.0, 8 * HOUR_MS).unwrap();
        let after = load(&scratch.0, 0).unwrap();
        assert_eq!(after, [Bucket::merge(&[first, second]).unwrap()]);
    }

    #[test]
    fn compacting_leaves_the_hour_still_being_recorded_alone() {
        let scratch = Scratch::new("compact-recent");
        append_bucket(&scratch.0, &bucket_at(HOUR_MS, "a", 1)).unwrap();
        append_bucket(&scratch.0, &bucket_at(HOUR_MS + MINUTE_MS, "a", 1)).unwrap();
        compact(&scratch.0, 2 * HOUR_MS - 1).unwrap();
        assert_eq!(load(&scratch.0, 0).unwrap().len(), 2);
    }

    #[test]
    fn pruning_removes_only_hours_that_ended() {
        let scratch = Scratch::new("prune");
        append_bucket(&scratch.0, &bucket_at(HOUR_MS, "gone", 1)).unwrap();
        append_bucket(&scratch.0, &bucket_at(2 * HOUR_MS, "kept", 1)).unwrap();
        prune(&scratch.0, 2 * HOUR_MS + 1).unwrap();
        let loaded = load(&scratch.0, 0).unwrap();
        assert_eq!(loaded, [bucket_at(2 * HOUR_MS, "kept", 1)]);
    }

    #[test]
    fn clearing_removes_recordings_but_not_the_lock() {
        let scratch = Scratch::new("clear");
        let bucket = bucket_at(HOUR_MS, "a", 1);
        append_bucket(&scratch.0, &bucket).unwrap();
        write_current(&scratch.0, &bucket).unwrap();
        fs::write(scratch.0.join("h3.tsv.tmp"), "half").unwrap();
        fs::write(scratch.0.join("notes.txt"), "mine").unwrap();
        let _lock = RecorderLock::acquire(&scratch.0).unwrap();
        let cleared = clear(&scratch.0).unwrap();
        assert_eq!(cleared.files, 3);
        assert!(cleared.bytes > 0);
        assert!(load(&scratch.0, 0).unwrap().is_empty());
        assert!(scratch.0.join("recorder.lock").exists());
        assert!(scratch.0.join("notes.txt").exists());
        assert!(current_missing(&scratch.0));
    }

    #[test]
    fn adopting_moves_an_unarchived_bucket_once() {
        let scratch = Scratch::new("adopt");
        let bucket = bucket_at(HOUR_MS, "a", 1);
        write_current(&scratch.0, &bucket).unwrap();
        adopt_current(&scratch.0).unwrap();
        write_current(&scratch.0, &bucket).unwrap();
        adopt_current(&scratch.0).unwrap();
        let archived = decode(&fs::read_to_string(hour_file(&scratch.0, bucket.minute)).unwrap());
        assert_eq!(archived, [bucket]);
        assert!(current_missing(&scratch.0));
    }

    #[test]
    fn adopting_with_no_current_file_changes_nothing() {
        let scratch = Scratch::new("adopt-none");
        adopt_current(&scratch.0).unwrap();
        assert!(load(&scratch.0, 0).unwrap().is_empty());
    }

    #[test]
    fn a_recorder_lock_is_seen_by_others_only_while_held() {
        let scratch = Scratch::new("lock");
        assert!(!RecorderLock::is_held(&scratch.0));
        let lock = RecorderLock::acquire(&scratch.0).unwrap();
        drop(lock);
        assert!(!RecorderLock::is_held(&scratch.0));
    }
}
