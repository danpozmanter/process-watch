# process-watch

Top programs by peak RAM or peak CPU over the past hours (the last hour by
default, up to a month). A program counts all of its
processes together: Firefox's dozen content processes are one row, and
`cargo build` includes every `rustc` it ran.

Runs on Linux, macOS, Windows, and FreeBSD. Written in Rust, as one binary:

- `pw record` samples every process and records per-program usage. It is
  required: no operating system records peak CPU, or anything about programs
  that have already exited, so something has to sample.
- `pw` reports on what the recorder saved.
- `pw -c` shows what each program uses right now, recorder or not.

```
$ pw
Past 1h: 59m 55s recorded, 24 CPUs (100% CPU = one core)
Top 10 of 131 programs by peak RAM, highest first

PROGRAM            PROCS  CPU TIME  AVG CPU  PEAK CPU    AVG RAM   PEAK RAM
firefox-bin           22    6m 41s    11.1%    164.0%    5.8 GiB    6.4 GiB
cargo                 14   23m 10s    38.6%   2280.5%    1.2 GiB    3.9 GiB
...
```

## Usage

```
pw [-n N] [-s COLUMN] [-o desc|asc] [-p SPAN] [-t LIMIT] [-b] [-d DIR] [PATTERN]
pw -c [-n N] [-s COLUMN] [-o desc|asc] [-t LIMIT] [-b] [PATTERN]
pw --clear [-d DIR]
pw record [-i SPAN] [-r SPAN] [-d DIR]
```

| Flag | Meaning | Default |
|---|---|---|
| `-n`, `--number` | programs to show | 10 |
| `-s`, `--sort` | the column to rank by; see below | `peak-ram` |
| `-o`, `--order` | `desc` shows the heaviest first, `asc` the lightest | `desc` |
| `-p`, `--period` | how far back to look, from `1h` up to `30d` | `1h` |
| `-t`, `--threshold` | show only programs above (`>`) or below (`<`) a limit on the `--sort` column; see below | |
| `-b`, `--breakdown` | under each program, its processes by name; see below | |
| `-c`, `--current` | what each program uses now, without the recorder; see below | |
| `PATTERN` | show only programs whose name contains it, ignoring case | |
| `--clear` | delete every recording | |
| `-i`, `--interval` | (record) time between samples | `5s` |
| `-r`, `--retain` | (record) how long recordings are kept, up to `30d` | `12h` |
| `-d`, `--dir` | data folder | see below |

Flags take `-n 5`, `-n5`, `--number 5`, or `--number=5`. As with other
Unix tools, switches combine in any order and may end with one flag that
takes a value: `-cb` is `-c -b`, and `-cbn 5` is `-c -b -n 5`. A pattern that
starts with `-`, or is the word `record`, goes after `--`: `pw -- -bin`.

`pw firefo` shows `firefox`, `Firefox-esr`, and anything else whose name
contains `firefo` in any case.

`--sort` names a column, and `--threshold` compares that column, in its unit:

| Sort | Column | Threshold |
|---|---|---|
| `peak-ram`, or `ram` (default) | PEAK RAM | `>1gb`, `<500mb`, `>1.5tb` |
| `peak-cpu`, or `cpu` | PEAK CPU | `>200%`, `<0.5%` |
| `avg-ram` | AVG RAM | `>1gb` |
| `avg-cpu` | AVG CPU | `>50%` |
| `cpu-time` | CPU TIME | `>10m`, `<1h30m` |
| `procs` | PROCS | `>20` |

Sizes are binary, as the report shows them: `1gb` is 1024 MiB, the `1.0 GiB`
of the RAM columns; `mib`, `gib`, and `tib` mean the same. A CPU percentage
is of one core. Quote the threshold, since a shell takes a bare `>` or `<` as
a redirection:

```
pw -t '>1gb'
pw firefox -s cpu --threshold '>200%'
pw -s avg-cpu -t '>50%' -p 1d
```

`-b` answers what a program's numbers are made of: whether 33 GiB of `cargo`
was one build or three at once, and how much of it was `rustc`. It adds a
RUNS column, the most copies of the program started apart (as from different
terminals) that ran at once, and lists the program's processes under it by
name, ranked by the `--sort` column:

```
$ pw -b cargo
Past 1h: 30m recorded, 24 CPUs (100% CPU = one core)
Top 1 of 1 programs by peak RAM, highest first, matching `cargo`

PROGRAM  RUNS  PROCS  CPU TIME  AVG CPU  PEAK CPU    AVG RAM   PEAK RAM
cargo       3     15    3h 20m   666.7%   2200.0%   20.0 GiB   33.0 GiB
  rustc           12    3h 16m   655.6%   2180.0%   19.5 GiB   32.2 GiB
  cargo            3    3m 20s    11.1%     30.0%  512.0 MiB  819.2 MiB
```

CPU time and averages of the processes add up to the program's. Each keeps
its own peaks, which may come at different moments. Time recorded by a `pw`
older than 0.5.0 has no breakdown, and shows as `(no breakdown)`.

`-c` samples every process itself and shows what each program uses at this
moment: CPU over the next second, and RAM and processes at its end. It needs
no recorder, so it takes no `--period`. `--sort` and `--threshold` work as
above, with `ram` and `cpu` meaning the moment's amounts (`cpu-time` is
refused), and `-b` breaks each program down by process name, with RUNS the
copies running now:

```
$ pw -c -b -n 2
Now: CPU over 1s, 24 CPUs (100% CPU = one core)
Top 2 of 123 programs by RAM, highest first

PROGRAM            RUNS  PROCS   CPU        RAM
firefox-bin           1     31  6.0%    8.9 GiB
  Isolated Web Co           20  2.0%    7.1 GiB
  firefox-bin                1  4.0%    1.4 GiB
  WebExtensions              1  0.0%  159.3 MiB
  ...
claude                3      3  3.0%  910.1 MiB
```

Run the recorder as a background service (below). A report covers only time
the recorder was running, and its header says how much of the period that was.

### Columns

- **PROCS**: the most processes the program had at once.
- **CPU TIME**: CPU consumed over the period, all processes summed.
- **AVG CPU**: CPU time divided by the recorded time; 100% is one core busy.
- **PEAK CPU**: the busiest single sampling interval.
- **AVG RAM**: memory averaged over the recorded time.
- **PEAK RAM**: the largest total at any one sample.

## What counts as one program

Each process is attributed to its highest ancestor that did not start a
program of its own, and rows are merged by that ancestor's name. A process
starts a program of its own when:

- it is in a different process group than its parent. Shells give each
  command line its own group and daemons start new sessions, while browsers,
  build tools, and `sh -c` recipes keep their children in their group;
- its parent is init (pid 1) or a launcher: a service manager (`systemd`,
  `launchd`), a display or session manager (`lightdm`, `ksmserver`,
  `gnome-session-binary`), a desktop shell (`gnome-shell`, `plasmashell`), a
  sandbox (`bwrap`), a wrapper (`sudo`, `env`, `nohup`), and on Windows, which
  has no process groups, `explorer`, `services`, `svchost`, `cmd`, and
  `powershell`;
- its recorded parent started after it did, meaning the parent's pid was reused.

On macOS, apps that run helpers as XPC services, such as Safari and other
WebKit apps, have those helpers started by `launchd` rather than by the app,
so they appear as programs of their own (`com.apple.WebKit.WebContent`).

A program that starts and finishes entirely between two samples is never
seen by name; its CPU counts toward the program that started it.

Short-lived children are counted too. When a parent reaps a child, the kernel
adds the child's CPU to the parent's "children" counter. The recorder credits
the counter's growth, minus what it already counted while the children were
alive, to the program those children belonged to. When programs that exited
together share one reaper (`sudo make`), the growth is split in proportion to
the CPU each used in its last sampled interval. Windows keeps no such
counter, so there, a child that starts and exits between two samples is not
seen.

A process whose parent exited first is reaped by an ancestor further up (a
subreaper such as `systemd --user`, or init). On Linux 6.6 and later the
recorder follows fork and exit events, so such an orphan's CPU still goes to
the program that started it, split by run time when orphans of several
programs land in one interval. Older kernels, macOS, and FreeBSD report no
such events to an unprivileged process, so there, an orphan no sample saw
counts toward its reaper; on macOS that reaper is `launchd`, whose counters
only root can read, so a non-root recorder misses that CPU entirely.

## How Process Watch Measures RAM

Summing resident memory across a program's processes would count shared
libraries once per process. Each platform reports what a process holds alone:

| Platform | Measure |
|---|---|
| Linux | resident anonymous memory (`statm` resident minus shared) |
| macOS | physical footprint, as Activity Monitor shows |
| Windows | private working set, as Task Manager shows |
| FreeBSD | resident set size (FreeBSD has no cheap per-process private measure) |

## Installing

Each [release](https://github.com/danpozmanter/process-watch/releases) carries:

| Platform | Files |
|---|---|
| Linux x86_64 | `process-watch-VERSION-linux-x86_64.tar.gz` (a static binary that runs on any distribution), `.deb`, `.rpm` |
| macOS | `process-watch-VERSION-macos-x86_64.tar.gz` (Intel), `-macos-aarch64.tar.gz` (Apple Silicon); macOS 12 or later |
| FreeBSD x86_64 | `process-watch-VERSION-freebsd-x86_64.tar.gz`; FreeBSD 14 or later |
| Windows x86_64 | `process-watch-VERSION-x86_64-setup.exe`, `-windows-x86_64.zip`, `-windows-x86_64-portable.zip` |

Each file has a `.sha256` beside it.

**From a tarball** (Linux, macOS, FreeBSD), unpack it and run its installer:

```
./install.sh              # ~/.local/bin, and starts the recorder
./install.sh --system     # /usr/local/bin (sudo)
./install.sh --prefix DIR # DIR/bin
./install.sh --no-service # the binary only
```

It starts the recorder as a systemd user service (Linux) or a launchd agent
(macOS); on FreeBSD it prints the crontab line to start it. Running it again
upgrades in place and restarts the recorder.

**From the .deb or .rpm**, install the package, then start the recorder as
each user who wants one:

```
systemctl --user enable --now process-watch
```

**On Windows**, the setup program installs for the current user without
administrator rights (or for everyone, if chosen), puts `pw` on the PATH, and
starts the recorder now and at every sign-in. Uninstalling stops the recorder
and removes everything but the recordings. The zip holds `pw.exe` to run from
anywhere.

**From source**, with Rust 1.85 or later, run `./install.sh` in a checkout; it
builds a release binary first.

### Portable mode

A folder named `data` beside `pw` keeps the recordings, so pw and everything it
records stay inside one folder that runs from anywhere and leaves nothing in
the user's profile. The Windows portable zip comes with that folder, as does
the setup program's portable mode, which extracts the files without
installing anything. Run `pw record` and `pw` from that folder as usual.

## Running the recorder as a service

`install.sh` sets this up on Linux and macOS; by hand:

**Linux (systemd):**

```
cp contrib/systemd/process-watch.service ~/.config/systemd/user/
systemctl --user enable --now process-watch
```

**macOS (launchd):**

```
cp contrib/launchd/local.process-watch.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/local.process-watch.plist
```

The plist runs `/usr/local/bin/pw record`; edit the path if you installed
elsewhere. macOS shows CPU and memory of other users' processes only to root,
so run it as a LaunchDaemon (in `/Library/LaunchDaemons`) to see those too.

**Windows**: the setup program sets this up. For the zip, use Task
Scheduler, from a command prompt:

```
schtasks /Create /TN process-watch /SC ONLOGON /RL LIMITED /TR "conhost.exe --headless \"%LOCALAPPDATA%\Programs\process-watch\pw.exe\" record"
```

`conhost.exe --headless` keeps the recorder from opening a console window.

**FreeBSD, or Linux without systemd**: add to `crontab -e`:

```
@reboot $HOME/.local/bin/pw record
```

Only one recorder writes to a data folder at a time; a second one exits
with an error. The recorder can be stopped at any moment, even with
`SIGKILL`: every sample also leaves the minute in progress on disk, and the
next recorder picks it up. It exits on an I/O error, such as a full disk, so
run it under a supervisor that restarts it, as the systemd unit and launchd
agent above do.

## Data

Recordings live in the folder `-d` names, else in `PROCESS_WATCH_DIR` if set,
else in a `data` folder beside `pw` if there is one (portable mode), and
otherwise in:

| Platform | Folder |
|---|---|
| Linux, FreeBSD | `$XDG_STATE_HOME/process-watch`, or `~/.local/state/process-watch` |
| macOS | `~/Library/Application Support/process-watch` |
| Windows | `%LOCALAPPDATA%\process-watch` |

The recorder keeps one line per program per minute, plus one per process name
for a program that runs more than one, in one text file per hour
(`hour-<unix hour>.tsv`), and keeps the minute in progress in `minute.tsv`,
so a report includes it. CPU is kept to the millisecond and memory to the KiB,
the precision a report shows. Once an hour ends, its file is gzip-compressed
(`hour-<unix hour>.tsv.gz`). The recorder deletes hours older than its
retention (`-r`, 12 hours by default, up to 30 days) and folds each hour older
than a day into a single bucket: totals and peaks stay exact, and a report
reaching further back than a day starts at an hour boundary.

A desktop with about 140 programs keeps about 85 KB per finished hour of
minute detail, 5 KB per folded hour, and up to 1.3 MB for the hour in
progress: about 2 MB at the default 12 hours, and about 7 MB for a full 30
days. A recorder from pw 0.4 or earlier wrote `h<unix hour>.tsv` and
`current.tsv`; the first recorder of a newer pw converts them.

`pw --clear` deletes every recording. A running recorder keeps going and
records from that moment on.

## Cost

Measured on Linux with about 650 processes and 3,000 threads, at the default
5-second interval: 0.12% of one core, one thread, and under 3 MB of memory.
A sample takes about 6 ms, nearly all of it the kernel producing
`/proc/<pid>/stat` (which sums CPU over each process's threads) and
`/proc/<pid>/statm`; between samples the recorder sleeps and uses nothing.
Halving the interval doubles the CPU, and so does doubling the process count.

## Development

```
cargo test                     # unit and command-line tests
scripts/package.sh dist        # build and package for this platform
tests/smoke.sh path/to/pw      # end to end, on a built binary
```

Unit tests sit beside the code they test. `tests/cli.rs` runs the built `pw`
as a user would: argument errors, reports over recorded data, clearing, the
one-recorder lock, and portable mode. The smoke test records for a few
seconds and checks the recorder lock, the report and its periods, and
argument errors; on Linux, macOS, and FreeBSD it also runs a workload of
short-lived subprocesses and checks the CPU recorded for it against the CPU
the shell measured.

`scripts/package.sh` builds the archives for the platform it runs on, and with
`--installers` the `.deb` and `.rpm` (Linux; needs `dpkg-deb` and `rpmbuild`)
or the setup program (Windows; needs Inno Setup 6).

Clippy caps each function's cognitive complexity at 10 and its length at 50
lines (`clippy.toml`).

CI (`.github/workflows/test.yml`) runs the tests, packages pw, and smoke tests
the package on Linux, macOS, and Windows, and in a FreeBSD VM; on Linux it
also checks formatting, lints every release target, and tests on the minimum
Rust. Pushing a `v*` tag runs `.github/workflows/release.yml`, which builds
every package, installs and runs each one as a user would, and publishes them
with that version's notes from `CHANGELOG.md`. Running it by hand builds and
checks everything without publishing.

## License

Apache License 2.0; see [LICENSE](LICENSE).
