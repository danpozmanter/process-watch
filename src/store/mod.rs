mod bucket;
mod lock;

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;

pub use self::bucket::{Bucket, Program, Units, Usage, decode};
pub use self::lock::RecorderLock;
use crate::span::{HOUR_MS, MINUTE_MS};

const CURRENT_FILE: &str = "minute.tsv";
const LEGACY_CURRENT_FILE: &str = "current.tsv";
const STAGING_SUFFIX: &str = ".tmp";

/// How a recording file holds its buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    /// Text in exact units, as pw 0.5 and earlier wrote it.
    Legacy,
    /// Text in coarse units.
    Plain,
    /// Text in coarse units, gzip-compressed.
    Packed,
}

impl Form {
    fn units(self) -> Units {
        match self {
            Form::Legacy => Units::Exact,
            Form::Plain | Form::Packed => Units::Coarse,
        }
    }
}

/// A file the recorder keeps in its data folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recording {
    /// The buckets of one hour, named by hours since the Unix epoch: plain
    /// while the hour is recorded, packed once it has ended.
    Hour(i64, Form),
    /// The bucket in progress.
    Current(Form),
    /// A replacement being written, before it is renamed over its file.
    Staging,
}

impl Recording {
    fn of_file(name: &str) -> Option<Self> {
        if let Some(replaced) = name.strip_suffix(STAGING_SUFFIX) {
            return Recording::of_file(replaced).map(|_| Recording::Staging);
        }
        match name {
            CURRENT_FILE => return Some(Recording::Current(Form::Plain)),
            LEGACY_CURRENT_FILE => return Some(Recording::Current(Form::Legacy)),
            _ => {}
        }
        let (hour, form) = if let Some(rest) = name.strip_prefix("hour-") {
            match rest.strip_suffix(".tsv.gz") {
                Some(hour) => (hour, Form::Packed),
                None => (rest.strip_suffix(".tsv")?, Form::Plain),
            }
        } else {
            (name.strip_prefix('h')?.strip_suffix(".tsv")?, Form::Legacy)
        };
        Some(Recording::Hour(hour.parse().ok()?, form))
    }
}

/// Wall-clock milliseconds since the Unix epoch, the clock buckets are named by.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn hour_of(minute: i64) -> i64 {
    minute * MINUTE_MS / HOUR_MS
}

fn hour_file(dir: &Path, hour: i64, form: Form) -> PathBuf {
    let name = match form {
        Form::Legacy => format!("h{hour}.tsv"),
        Form::Plain => format!("hour-{hour}.tsv"),
        Form::Packed => format!("hour-{hour}.tsv.gz"),
    };
    dir.join(name)
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

/// The hour files in `dir` of hours that end at or before `end_ms`, with their hour and form.
fn hours_ended_by(dir: &Path, end_ms: i64) -> Result<Vec<(i64, Form, PathBuf)>> {
    let ended = recordings(dir)?
        .into_iter()
        .filter_map(|(kind, path)| match kind {
            Recording::Hour(hour, form) if hour_end_ms(hour) <= end_ms => Some((hour, form, path)),
            _ => None,
        })
        .collect();
    Ok(ended)
}

/// The buckets in the file, or `None` when it does not exist, as the
/// current file briefly does not while the recorder renames a new one over it.
fn read_buckets(path: &Path, form: Form) -> Result<Option<Vec<Bucket>>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let text = match form {
        Form::Packed => {
            let mut text = String::new();
            MultiGzDecoder::new(bytes.as_slice())
                .read_to_string(&mut text)
                .with_context(|| format!("decompressing {}", path.display()))?;
            text
        }
        Form::Legacy | Form::Plain => String::from_utf8_lossy(&bytes).into_owned(),
    };
    Ok(Some(decode(&text, form.units())))
}

/// Replaces the file's contents in one step: a reader sees the old contents
/// or the new, never a mix, and a crash midway leaves the old file in place.
fn replace_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut staging = path.as_os_str().to_owned();
    staging.push(STAGING_SUFFIX);
    let staging = PathBuf::from(staging);
    fs::write(&staging, contents).with_context(|| format!("writing {}", staging.display()))?;
    fs::rename(&staging, path).with_context(|| format!("replacing {}", path.display()))
}

/// Replaces a packed hour file with `buckets`.
fn write_packed(path: &Path, buckets: &[Bucket]) -> Result<()> {
    let mut packer = GzEncoder::new(Vec::new(), Compression::best());
    for bucket in buckets {
        packer.write_all(bucket.encode().as_bytes())?;
    }
    let packed = packer
        .finish()
        .with_context(|| format!("compressing {}", path.display()))?;
    replace_file(path, &packed)
}

/// Adds each bucket not already among `kept`, as the same bucket may be in
/// more than one file while it moves from one to another.
fn keep_new(kept: &mut Vec<Bucket>, seen: &mut HashSet<(i64, i64)>, buckets: Vec<Bucket>) {
    for bucket in buckets {
        if seen.insert((bucket.minute, bucket.opened_ms)) {
            kept.push(bucket);
        }
    }
}

/// Every bucket recorded for one hour, in whichever files hold it.
fn hour_buckets(dir: &Path, hour: i64) -> Result<Vec<Bucket>> {
    let mut kept = Vec::new();
    let mut seen = HashSet::new();
    for form in [Form::Packed, Form::Plain, Form::Legacy] {
        if let Some(buckets) = read_buckets(&hour_file(dir, hour, form), form)? {
            keep_new(&mut kept, &mut seen, buckets);
        }
    }
    Ok(kept)
}

/// Appends a finished bucket to its hour file.
pub fn append_bucket(dir: &Path, bucket: &Bucket) -> Result<()> {
    let target = hour_file(dir, hour_of(bucket.minute), Form::Plain);
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
    replace_file(&dir.join(CURRENT_FILE), bucket.encode().as_bytes())
}

/// Whether the current file is missing, as it is after a clear removed it.
pub fn current_missing(dir: &Path) -> bool {
    !dir.join(CURRENT_FILE).exists()
}

/// Moves a bucket an earlier recorder left in a current file into its hour
/// file, unless that recorder already appended it there.
pub fn adopt_current(dir: &Path) -> Result<()> {
    for form in [Form::Legacy, Form::Plain] {
        let name = match form {
            Form::Legacy => LEGACY_CURRENT_FILE,
            Form::Plain | Form::Packed => CURRENT_FILE,
        };
        let current = dir.join(name);
        let Some(buckets) = read_buckets(&current, form)? else {
            continue;
        };
        for bucket in buckets {
            let archived = hour_buckets(dir, hour_of(bucket.minute))?;
            if !archived.iter().any(|b| b.is_same_as(&bucket)) {
                append_bucket(dir, &bucket)?;
            }
        }
        fs::remove_file(&current).with_context(|| format!("removing {}", current.display()))?;
    }
    Ok(())
}

/// Every recorded bucket that starts at or after `since_ms`, the minute in
/// progress included, oldest first.
pub fn load(dir: &Path, since_ms: i64) -> Result<Vec<Bucket>> {
    let mut found = recordings(dir)?;
    // The minute in progress is also in an hour file once the recorder has
    // finished it, so the hour files are read first.
    found.sort_by_key(|(kind, _)| matches!(kind, Recording::Current(_)));
    let mut kept = Vec::new();
    let mut seen = HashSet::new();
    for (kind, path) in found {
        let form = match kind {
            Recording::Hour(hour, form) if hour_end_ms(hour) > since_ms => form,
            Recording::Current(form) => form,
            Recording::Hour(..) | Recording::Staging => continue,
        };
        let Some(buckets) = read_buckets(&path, form)? else {
            continue;
        };
        let recent = buckets
            .into_iter()
            .filter(|b| b.minute * MINUTE_MS >= since_ms)
            .collect();
        keep_new(&mut kept, &mut seen, recent);
    }
    kept.sort_by_key(|b| (b.minute, b.opened_ms));
    Ok(kept)
}

/// Packs each hour that ended at or before `end_ms` into one compressed
/// file, from whichever files hold it, then removes those. The packed file
/// is in place before any source is removed, and keeps every bucket as it
/// was, so a reader or a crash in between counts none twice.
pub fn pack(dir: &Path, end_ms: i64) -> Result<()> {
    let unpacked: Vec<(i64, PathBuf)> = hours_ended_by(dir, end_ms)?
        .into_iter()
        .filter(|&(_, form, _)| form != Form::Packed)
        .map(|(hour, _, path)| (hour, path))
        .collect();
    let hours: HashSet<i64> = unpacked.iter().map(|&(hour, _)| hour).collect();
    for &hour in &hours {
        let buckets = hour_buckets(dir, hour)?;
        write_packed(&hour_file(dir, hour, Form::Packed), &buckets)?;
    }
    for (_, path) in unpacked {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Rewrites each packed hour that ends at or before `before_ms` as a single
/// bucket for the whole hour, so old recordings keep their totals and peaks
/// in a sixtieth of the space.
pub fn compact(dir: &Path, before_ms: i64) -> Result<()> {
    for (_, form, path) in hours_ended_by(dir, before_ms)? {
        if form != Form::Packed {
            continue;
        }
        let buckets = read_buckets(&path, form)?.unwrap_or_default();
        if buckets.len() < 2 {
            continue;
        }
        if let Some(hour) = Bucket::merge(&buckets) {
            write_packed(&path, &[hour])?;
        }
    }
    Ok(())
}

/// Removes hour files that end at or before `keep_from_ms`.
pub fn prune(dir: &Path, keep_from_ms: i64) -> Result<()> {
    for (_, _, path) in hours_ended_by(dir, keep_from_ms)? {
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
    use crate::accounting::{ProgramTally, Tally};

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

    fn bucket_at(ms: i64, name: &str, cpu_ms: i64) -> Bucket {
        let mut bucket = Bucket::open(ms, 2);
        let total = Tally {
            cpu_ns: cpu_ms * 1_000_000,
            mem_bytes: 10 * 1024,
            procs: 1,
        };
        let program = ProgramTally {
            total,
            runs: 1,
            parts: BTreeMap::from([(name.to_owned(), total)]),
        };
        bucket.add(1000, &BTreeMap::from([(name.to_owned(), program)]));
        bucket
    }

    /// The bucket as pw 0.5 and earlier wrote it, in exact units.
    fn legacy_text(bucket: &Bucket) -> String {
        let mut text = format!(
            "B\t{}\t{}\t{}\t{}\n",
            bucket.minute, bucket.opened_ms, bucket.covered_ms, bucket.cpus
        );
        for (name, p) in &bucket.programs {
            let u = p.usage;
            text.push_str(&format!(
                "G\t{}\t{}\t{}\t{}\t{}\t{name}\nR\t{}\n",
                u.cpu_ns, u.peak_cpu_permille, u.mem_byte_ms, u.peak_mem, u.peak_procs, p.peak_runs
            ));
        }
        text
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn file_names_say_what_they_record() {
        let cases = [
            ("hour-42.tsv", Some(Recording::Hour(42, Form::Plain))),
            ("hour-42.tsv.gz", Some(Recording::Hour(42, Form::Packed))),
            ("h42.tsv", Some(Recording::Hour(42, Form::Legacy))),
            ("minute.tsv", Some(Recording::Current(Form::Plain))),
            ("current.tsv", Some(Recording::Current(Form::Legacy))),
            ("minute.tsv.tmp", Some(Recording::Staging)),
            ("hour-42.tsv.gz.tmp", Some(Recording::Staging)),
            ("h42.tsv.tmp", Some(Recording::Staging)),
            ("recorder.lock", None),
            ("hx.tsv", None),
            ("hour-x.tsv.gz", None),
            ("notes.tmp", None),
        ];
        for (name, kind) in cases {
            assert_eq!(Recording::of_file(name), kind, "{name}");
        }
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
        let names: Vec<&String> = loaded.iter().flat_map(|b| b.programs.keys()).collect();
        assert_eq!(names, ["late"]);
    }

    #[test]
    fn packing_compresses_each_ended_hour_from_every_file_holding_it() {
        let scratch = Scratch::new("pack");
        let old = bucket_at(HOUR_MS, "old", 3);
        let new = bucket_at(HOUR_MS + MINUTE_MS, "new", 4);
        let recording = bucket_at(2 * HOUR_MS, "now", 5);
        let legacy = hour_file(&scratch.0, 1, Form::Legacy);
        fs::write(&legacy, legacy_text(&old) + &legacy_text(&new)).unwrap();
        append_bucket(&scratch.0, &new).unwrap();
        append_bucket(&scratch.0, &recording).unwrap();
        pack(&scratch.0, 2 * HOUR_MS + 1).unwrap();
        assert_eq!(names_in(&scratch.0), ["hour-1.tsv.gz", "hour-2.tsv"]);
        assert_eq!(load(&scratch.0, 0).unwrap(), [old, new, recording]);
    }

    #[test]
    fn a_reader_while_an_hour_is_packed_counts_each_minute_once() {
        let scratch = Scratch::new("pack-midway");
        let minute = bucket_at(HOUR_MS, "a", 3);
        append_bucket(&scratch.0, &minute).unwrap();
        write_packed(
            &hour_file(&scratch.0, 1, Form::Packed),
            std::slice::from_ref(&minute),
        )
        .unwrap();
        assert_eq!(load(&scratch.0, 0).unwrap(), [minute]);
    }

    #[test]
    fn compacting_an_old_hour_keeps_its_totals_and_peaks() {
        let scratch = Scratch::new("compact");
        let first = bucket_at(7 * HOUR_MS, "a", 6);
        let second = bucket_at(7 * HOUR_MS + MINUTE_MS, "a", 30);
        append_bucket(&scratch.0, &first).unwrap();
        append_bucket(&scratch.0, &second).unwrap();
        pack(&scratch.0, 8 * HOUR_MS).unwrap();
        compact(&scratch.0, 8 * HOUR_MS).unwrap();
        let after = load(&scratch.0, 0).unwrap();
        assert_eq!(after, [Bucket::merge(&[first, second]).unwrap()]);
    }

    #[test]
    fn packing_and_compacting_leave_the_hour_still_being_recorded_alone() {
        let scratch = Scratch::new("compact-recent");
        append_bucket(&scratch.0, &bucket_at(HOUR_MS, "a", 1)).unwrap();
        append_bucket(&scratch.0, &bucket_at(HOUR_MS + MINUTE_MS, "a", 1)).unwrap();
        pack(&scratch.0, 2 * HOUR_MS - 1).unwrap();
        compact(&scratch.0, 2 * HOUR_MS - 1).unwrap();
        assert_eq!(names_in(&scratch.0), ["hour-1.tsv"]);
        assert_eq!(load(&scratch.0, 0).unwrap().len(), 2);
    }

    #[test]
    fn pruning_removes_only_hours_that_ended() {
        let scratch = Scratch::new("prune");
        append_bucket(&scratch.0, &bucket_at(HOUR_MS, "gone", 1)).unwrap();
        fs::write(hour_file(&scratch.0, 0, Form::Legacy), "").unwrap();
        pack(&scratch.0, 2 * HOUR_MS).unwrap();
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
        fs::write(scratch.0.join("current.tsv"), "").unwrap();
        fs::write(scratch.0.join("notes.txt"), "mine").unwrap();
        let _lock = RecorderLock::acquire(&scratch.0).unwrap();
        let cleared = clear(&scratch.0).unwrap();
        assert_eq!(cleared.files, 4);
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
        assert_eq!(hour_buckets(&scratch.0, 1).unwrap(), [bucket]);
        assert!(current_missing(&scratch.0));
    }

    #[test]
    fn adopting_reads_what_an_older_recorder_left() {
        let scratch = Scratch::new("adopt-legacy");
        let archived = bucket_at(HOUR_MS, "a", 1);
        let left = bucket_at(HOUR_MS + MINUTE_MS, "b", 2);
        fs::write(
            hour_file(&scratch.0, 1, Form::Legacy),
            legacy_text(&archived),
        )
        .unwrap();
        fs::write(scratch.0.join(LEGACY_CURRENT_FILE), legacy_text(&left)).unwrap();
        adopt_current(&scratch.0).unwrap();
        assert_eq!(names_in(&scratch.0), ["h1.tsv", "hour-1.tsv"]);
        assert_eq!(hour_buckets(&scratch.0, 1).unwrap(), [left, archived]);
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
