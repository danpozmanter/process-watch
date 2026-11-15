# Changelog

## 0.1.2

- On Linux 6.6+, CPU of orphans too short-lived to sample goes to the program that started them, not to `systemd` or init.

## 0.1.1

- Programs launched by session wrappers (`dbus-run-session`, `xinit`, `pkexec`, ...) and Wayland compositors count as their own programs, not the wrapper's.

## 0.1.0 - Initial release

- `pw record` samples every process at a fixed interval (`-i`, 5 seconds by default) and keeps per-program CPU, memory, and process counts by the minute, folding hours older than a day into one bucket each.
- `pw` reports the top programs by average RAM or CPU time over the past hour, or up to 30 days (`-p`), with each program's peak CPU, peak RAM, and most processes at once.
- A program counts every process it started that did not start a program of its own, judged by process groups, launchers, and pid reuse.
- CPU of children that start and exit between two samples is credited to the program that ran them, on Linux, macOS, and FreeBSD.
- RAM is the memory a process holds alone: resident anonymous memory on Linux, the physical footprint on macOS, the private working set on Windows, and the resident set on FreeBSD.
- The recorder can be stopped at any moment, even with SIGKILL, without losing the minute in progress, and a second recorder on the same folder is refused.
- `pw --clear` deletes every recording, and a running recorder records on from that moment.
- Portable mode: a `data` folder beside `pw` keeps the recordings there.
- Releases for Linux (a static binary, `.deb`, and `.rpm`), macOS (Intel and Apple Silicon), FreeBSD, and Windows (a zip, a portable zip, and an installer).
