# Changelog

## 0.5.0

- `-b`/`--breakdown` lists each program's processes by name under it, and adds RUNS, the most copies of the program started apart running at once.
- Recordings take about a tenth of the space: CPU is kept to the millisecond and memory to the KiB, and each finished hour is compressed. The recorder converts existing recordings when it starts.

## 0.4.0

- `--sort` takes any number column: `peak-ram` (`ram`), `peak-cpu` (`cpu`), `avg-ram`, `avg-cpu`, `cpu-time`, or `procs`.
- `-t`/`--threshold` compares the sorted column in its unit: `'>1gb'`, `'>200%'`, `'>10m'`, or `'>20'`.

## 0.3.0

- `--sort` ranks by peak RAM or peak CPU, not by average RAM or total CPU time.
- `-t`/`--threshold` compares peaks: `'>1gb'` for peak RAM, or `'>200%'` with `--sort cpu`.

## 0.2.0

- Filter by name: `pw firefo` shows every program whose name contains `firefo`, in any case.
- Filter by usage with `-t`/`--threshold`, such as `'>1gb'` for RAM, or `'>50%'` and `'>10m'` with `--sort cpu`.
- A report with no matching programs shows only its heading.

## 0.1.2

- On Linux 6.6+, CPU used by short-lived orphans goes to the program that started them, not to `systemd` or init.

## 0.1.1

- Programs started by session wrappers (`dbus-run-session`, `xinit`, `pkexec`) and Wayland compositors count as programs of their own.

## 0.1.0 - Initial release

- `pw record` samples every process and keeps per-program CPU, RAM, and process counts by the minute.
- `pw` shows the top programs by RAM or CPU over the past hour, or up to 30 days, with their peaks.
- A program includes every process it started, so a browser's helpers count toward the browser.
- CPU of children too short-lived to sample counts toward the program that ran them, on Linux, macOS, and FreeBSD.
- RAM counts only memory a process does not share, so shared libraries are not counted twice.
- The recorder survives being killed at any moment, and only one recorder writes to a folder.
- `pw --clear` deletes every recording.
- Portable mode keeps recordings in a `data` folder beside `pw`.
- Releases for Linux, macOS, FreeBSD, and Windows.
