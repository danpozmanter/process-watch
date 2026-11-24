use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const PW: &str = env!("CARGO_BIN_EXE_pw");

/// A fresh folder for one test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        // Cargo's scratch folder shares the built binary's filesystem, so pw
        // can be hard-linked into it.
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pw-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `pw` with the data-folder variables cleared and a home inside `home`, so
/// nothing a test runs reads or writes the real per-user state folder.
fn pw_in(exe: &Path, home: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .env_remove("PROCESS_WATCH_DIR")
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("LOCALAPPDATA", home.join("local"));
    command
}

fn run(home: &Path, args: &[&str]) -> Output {
    pw_in(Path::new(PW), home).args(args).output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A running `pw record`, stopped when dropped.
struct Recorder {
    child: Child,
    /// The line the recorder prints once it holds the folder's lock.
    started: String,
}

impl Recorder {
    fn start(mut command: Command) -> Self {
        let mut child = command.stderr(Stdio::piped()).spawn().unwrap();
        let mut started = String::new();
        let stderr = child.stderr.take().unwrap();
        BufReader::new(stderr).read_line(&mut started).unwrap();
        Self { child, started }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One minute of recording, opened now, in which each program used a
/// second of CPU and held its bytes of memory throughout.
/// The minute in progress, as the recorder writes it: CPU in ms, memory in
/// KiB, the average first, then the peak.
fn minute_of(programs: &[(&str, i64)]) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap())
        .unwrap();
    let mut text = format!("B\t{}\t{now_ms}\t60000\t4\n", now_ms / 60_000);
    for (name, mem_bytes) in programs {
        let kib = mem_bytes / 1024;
        text.push_str(&format!("G\t1000\t100\t{kib}\t{kib}\t2\t{name}\n"));
    }
    text
}

fn table_rows(report: &str) -> Vec<&str> {
    report
        .lines()
        .skip_while(|line| !line.starts_with("PROGRAM"))
        .skip(1)
        .collect()
}

#[test]
fn version_names_the_package_version() {
    let scratch = Scratch::new("version");
    let output = run(&scratch.0, &["--version"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        format!("pw {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn help_shows_usage() {
    let scratch = Scratch::new("help");
    let output = run(&scratch.0, &["-h"]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("Usage:"));
}

#[test]
fn a_bad_argument_exits_two_and_names_the_problem() {
    let scratch = Scratch::new("bad-arg");
    let output = run(&scratch.0, &["--sort", "disk"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "pw: --sort takes peak-ram, peak-cpu, avg-ram, avg-cpu, cpu-time, procs; got `disk`\n"
    );
}

#[test]
fn a_report_with_nothing_recorded_exits_one() {
    let scratch = Scratch::new("empty");
    let data = scratch.path("data");
    let output = run(&scratch.0, &["-d", data.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("nothing recorded in the past 1h"));
    assert!(stderr(&output).contains("start the recorder with `pw record`"));
}

#[test]
fn a_report_ranks_what_was_recorded() {
    let scratch = Scratch::new("report");
    let data = scratch.path("data");
    fs::create_dir_all(&data).unwrap();
    let text = minute_of(&[("small", 1000 << 10), ("large", 9000 << 10)]);
    fs::write(data.join("minute.tsv"), text).unwrap();
    let dir = data.to_str().unwrap();

    let by_ram = run(&scratch.0, &["-d", dir]);
    assert!(by_ram.status.success(), "{}", stderr(&by_ram));
    let shown = stdout(&by_ram);
    let rows = table_rows(&shown);
    assert_eq!(rows.len(), 2, "{shown}");
    assert!(rows[0].starts_with("large "), "{shown}");

    let lightest = run(&scratch.0, &["-d", dir, "-o", "asc", "-n", "1"]);
    let shown = stdout(&lightest);
    let rows = table_rows(&shown);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].starts_with("small "));
}

/// A data folder holding one minute of three mail and browser programs.
fn three_programs(scratch: &Scratch) -> PathBuf {
    let data = scratch.path("data");
    fs::create_dir_all(&data).unwrap();
    let gib = 1 << 30;
    let text = minute_of(&[
        ("firefox", 2 * gib),
        ("Firefox-esr", gib / 2),
        ("thunderbird", 3 * gib),
    ]);
    fs::write(data.join("minute.tsv"), text).unwrap();
    data
}

/// The report pw prints for `args`, and the programs in its table, in order.
fn report_of(scratch: &Scratch, args: &[&str]) -> (String, Vec<String>) {
    let report = stdout(&run(&scratch.0, args));
    let names = table_rows(&report)
        .iter()
        .filter_map(|row| row.split_whitespace().next())
        .map(str::to_owned)
        .collect();
    (report, names)
}

#[test]
fn a_report_shows_only_programs_matching_its_pattern_and_threshold() {
    let scratch = Scratch::new("filter");
    let data = three_programs(&scratch);
    let dir = data.to_str().unwrap();

    let (_, names) = report_of(&scratch, &["-d", dir, "FIREFO"]);
    assert_eq!(names, ["firefox", "Firefox-esr"]);

    let (heavy, names) = report_of(&scratch, &["-d", dir, "fire", "-t", ">1gb"]);
    assert!(heavy.contains(", matching `fire`, with peak RAM above 1.0 GiB\n"));
    assert_eq!(names, ["firefox"]);

    let (_, names) = report_of(&scratch, &["-d", dir, "--threshold=<1gb"]);
    assert_eq!(names, ["Firefox-esr"]);
}

#[test]
fn a_threshold_compares_the_sorted_column_in_its_unit() {
    let scratch = Scratch::new("sort-column");
    let data = three_programs(&scratch);
    let dir = data.to_str().unwrap();

    let (steady, names) = report_of(&scratch, &["-d", dir, "-s", "avg-ram", "-t", ">1gb"]);
    assert!(steady.contains("by average RAM, highest first, with average RAM above 1.0 GiB\n"));
    assert_eq!(names, ["thunderbird", "firefox"]);

    let (_, names) = report_of(&scratch, &["-d", dir, "-s", "procs", "-t", ">2"]);
    assert!(names.is_empty());

    let busy = run(&scratch.0, &["-d", dir, "-s", "cpu", "-t", ">1gb"]);
    assert_eq!(busy.status.code(), Some(2));
}

#[test]
fn a_breakdown_shows_how_many_builds_ran_and_what_they_ran() {
    let scratch = Scratch::new("breakdown");
    let data = scratch.path("data");
    fs::create_dir_all(&data).unwrap();
    let gib: i64 = 1 << 30;
    let gib_in_kib = gib >> 10;
    let mut text = minute_of(&[("editor", gib)]);
    // Three builds at once: 3 cargo processes and the 12 rustc they ran.
    text.push_str(&format!(
        "G\t1000\t100\t{0}\t{0}\t15\tcargo\n\
         R\t3\n\
         P\t100\t10\t{1}\t{1}\t3\tcargo\n\
         P\t900\t90\t{2}\t{2}\t12\trustc\n",
        33 * gib_in_kib,
        gib_in_kib,
        32 * gib_in_kib,
    ));
    fs::write(data.join("minute.tsv"), text).unwrap();
    let dir = data.to_str().unwrap();

    let (report, names) = report_of(&scratch, &["-d", dir, "-b", "cargo"]);
    assert_eq!(names, ["cargo", "rustc", "cargo"], "{report}");
    let rows = table_rows(&report);
    let first: Vec<&str> = rows[0].split_whitespace().collect();
    assert_eq!(first[..3], ["cargo", "3", "15"], "{report}");
    assert!(rows[1].ends_with("32.0 GiB"), "{report}");

    let (plain, names) = report_of(&scratch, &["-d", dir, "cargo"]);
    assert_eq!(names, ["cargo"]);
    assert!(!plain.contains("RUNS"));
}

#[test]
fn the_current_moment_needs_no_recording_and_lists_pw_itself() {
    let scratch = Scratch::new("current");
    let output = run(&scratch.0, &["-c", "-b", "-n", "100000"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout(&output);
    assert!(report.starts_with("Now: CPU over 1s, "), "{report}");
    assert!(
        table_rows(&report)
            .iter()
            .any(|row| row.trim_start().starts_with("pw")),
        "{report}"
    );

    let period = run(&scratch.0, &["-c", "-p", "2h"]);
    assert_eq!(period.status.code(), Some(2));
    assert!(stderr(&period).contains("takes no period"));
}

#[test]
fn a_report_reads_what_an_older_recorder_kept() {
    let scratch = Scratch::new("legacy");
    let data = scratch.path("data");
    fs::create_dir_all(&data).unwrap();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap())
        .unwrap();
    // CPU in ns, memory-time in byte-ms, peak memory in bytes.
    let text = format!(
        "B\t{}\t{now_ms}\t60000\t4\nG\t30000000000\t900\t{}\t{}\t2\told\n",
        now_ms / 60_000,
        (2i64 << 30) * 60_000,
        3i64 << 30
    );
    fs::write(data.join("current.tsv"), text).unwrap();
    let (report, names) = report_of(&scratch, &["-d", data.to_str().unwrap()]);
    assert_eq!(names, ["old"]);
    let row: Vec<&str> = table_rows(&report)[0].split_whitespace().collect();
    assert_eq!(
        row[2..],
        ["30s", "50.0%", "90.0%", "2.0", "GiB", "3.0", "GiB"]
    );
}

#[test]
fn clearing_removes_what_a_report_would_show() {
    let scratch = Scratch::new("clear");
    let data = scratch.path("data");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("minute.tsv"), minute_of(&[("editor", 100 << 10)])).unwrap();
    let dir = data.to_str().unwrap();

    let cleared = run(&scratch.0, &["--clear", "-d", dir]);
    assert!(cleared.status.success());
    assert!(stdout(&cleared).starts_with("pw: cleared 1 recording file "));
    assert_eq!(run(&scratch.0, &["-d", dir]).status.code(), Some(1));
}

#[test]
fn only_one_recorder_writes_to_a_folder() {
    let scratch = Scratch::new("lock");
    let data = scratch.path("data");
    let dir = data.to_str().unwrap();
    let mut first = pw_in(Path::new(PW), &scratch.0);
    first.args(["record", "-d", dir]);
    let recorder = Recorder::start(first);
    assert!(recorder.started.starts_with("pw: recording every 5s into "));

    let second = run(&scratch.0, &["record", "-d", dir]);
    assert_eq!(second.status.code(), Some(1));
    assert!(stderr(&second).contains("another recorder is already writing to"));

    let report = run(&scratch.0, &["-d", dir]);
    assert!(stderr(&report).contains("the recorder is running"));
    let cleared = run(&scratch.0, &["--clear", "-d", dir]);
    assert!(stdout(&cleared).ends_with("the recorder keeps recording from now\n"));
}

/// pw in a folder of its own, as an unpacked portable archive has it. A
/// hard link, unlike a copy, opens no file for writing that a process other
/// tests fork at the same moment could inherit, which would leave the new
/// executable busy and unable to run.
fn portable_copy(scratch: &Scratch) -> PathBuf {
    let folder = scratch.path("portable");
    fs::create_dir_all(folder.join("data")).unwrap();
    let exe = folder.join(Path::new(PW).file_name().unwrap());
    fs::hard_link(PW, &exe).unwrap();
    exe
}

/// The same folder, however the two paths spell it.
fn same_folder(a: &Path, b: &Path) -> bool {
    fs::canonicalize(a).unwrap() == fs::canonicalize(b).unwrap()
}

fn recording_folder(exe: &Path, home: &Path, env_dir: Option<&Path>) -> String {
    let mut command = pw_in(exe, home);
    command.arg("record");
    if let Some(dir) = env_dir {
        command.env("PROCESS_WATCH_DIR", dir);
    }
    let recorder = Recorder::start(command);
    let line = recorder.started.trim_end();
    line.strip_prefix("pw: recording every 5s into ")
        .unwrap_or_else(|| panic!("unexpected first line: {line}"))
        .to_owned()
}

#[test]
fn a_data_folder_beside_pw_keeps_its_recordings() {
    let scratch = Scratch::new("portable");
    let exe = portable_copy(&scratch);
    let folder = recording_folder(&exe, &scratch.0, None);
    assert!(same_folder(
        Path::new(&folder),
        &exe.parent().unwrap().join("data")
    ));
}

#[test]
fn the_environment_names_the_folder_over_portable_mode() {
    let scratch = Scratch::new("env-over-portable");
    let exe = portable_copy(&scratch);
    let named = scratch.path("named");
    let folder = recording_folder(&exe, &scratch.0, Some(&named));
    assert!(same_folder(Path::new(&folder), &named));
}

#[test]
fn without_a_data_folder_recordings_go_to_the_per_user_state_folder() {
    let scratch = Scratch::new("per-user");
    let folder = recording_folder(Path::new(PW), &scratch.0, None);
    assert!(Path::new(&folder).starts_with(&scratch.0), "{folder}");
    assert!(folder.ends_with("process-watch"), "{folder}");
}
