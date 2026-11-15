use std::collections::HashMap;

use crate::snapshot::Proc;

/// Programs whose children are programs of their own: init systems, service
/// managers, display and session managers, compositors, desktop shells,
/// sandboxes, command wrappers, and, on Windows, where there are no process
/// groups, the shells and terminals too. Sorted, lowercase.
const LAUNCHERS: &[&str] = &[
    "budgie-panel",
    "bwrap",
    "cinnamon",
    "cinnamon-launcher",
    "cinnamon-session",
    "cmd",
    "conmon",
    "containerd-shim",
    "containerd-shim-runc-v2",
    "dbus-broker",
    "dbus-broker-launch",
    "dbus-daemon",
    "dbus-launch",
    "dbus-run-session",
    "doas",
    "dumb-init",
    "env",
    "explorer",
    "firejail",
    "flatpak-session-helper",
    "gdm",
    "gdm-session-worker",
    "gdm-wayland-session",
    "gdm-x-session",
    "gdm3",
    "gnome-session-binary",
    "gnome-shell",
    "hyprland",
    "i3",
    "init",
    "ionice",
    "kdeinit5",
    "klauncher",
    "krunner",
    "ksmserver",
    "kwin_wayland",
    "kwin_wayland_wrapper",
    "labwc",
    "launchd",
    "lightdm",
    "login",
    "lxpanel",
    "lxqt-session",
    "lxsession",
    "mate-panel",
    "mate-session",
    "nice",
    "niri",
    "nohup",
    "openbox",
    "openconsole",
    "pkexec",
    "plasma_session",
    "plasmashell",
    "powershell",
    "pwsh",
    "runit",
    "runsv",
    "runsvdir",
    "runuser",
    "s6-supervise",
    "s6-svscan",
    "sddm",
    "sddm-helper",
    "services",
    "setsid",
    "smss",
    "startplasma-wayland",
    "startplasma-x11",
    "startx",
    "su",
    "sudo",
    "supervisord",
    "svchost",
    "sway",
    "systemd",
    "timeout",
    "tini",
    "upstart",
    "userinit",
    "wayfire",
    "windowsterminal",
    "wininit",
    "winlogon",
    "xfce4-panel",
    "xfce4-session",
    "xfdesktop",
    "xinit",
];

fn is_launcher(name: &str) -> bool {
    LAUNCHERS
        .binary_search(&name.to_ascii_lowercase().as_str())
        .is_ok()
}

/// Whether `child` starts a program of its own rather than counting toward `parent`.
fn starts_program(child: &Proc, parent: &Proc) -> bool {
    // A parent younger than its child holds a reused pid, not the real parent.
    parent.start_ns > child.start_ns
        || parent.pid == child.pid
        || parent.pid == 1
        // Job control and daemons put a new program in a process group of its
        // own, while a build tool's or browser's children stay in their parent's.
        || (child.pgid != 0 && child.pgid != parent.pgid)
        || is_launcher(&parent.name)
}

/// For each process, the index of the process heading the program it counts
/// toward: its highest ancestor reached without crossing into another program.
pub fn program_roots(procs: &[Proc]) -> Vec<usize> {
    let at: HashMap<i64, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let mut root: Vec<Option<usize>> = vec![None; procs.len()];
    let mut chain = Vec::new();
    for i in 0..procs.len() {
        chain.clear();
        let mut cur = i;
        let top = loop {
            if let Some(known) = root[cur] {
                break known;
            }
            chain.push(cur);
            let Some(&parent) = at.get(&procs[cur].ppid) else {
                break cur;
            };
            // A chain longer than the snapshot has looped through stale parent pids.
            if chain.len() > procs.len() || starts_program(&procs[cur], &procs[parent]) {
                break cur;
            }
            cur = parent;
        };
        for &c in &chain {
            root[c] = Some(top);
        }
    }
    root.into_iter()
        .enumerate()
        .map(|(i, r)| r.unwrap_or(i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i64, ppid: i64, pgid: i64, name: &str) -> Proc {
        Proc {
            pid,
            ppid,
            pgid,
            start_ns: pid,
            name: name.to_owned(),
            ..Proc::default()
        }
    }

    fn programs(procs: &[Proc]) -> Vec<&str> {
        program_roots(procs)
            .into_iter()
            .map(|r| procs[r].name.as_str())
            .collect()
    }

    #[test]
    fn launchers_are_sorted_and_lowercase() {
        assert!(LAUNCHERS.windows(2).all(|w| w[0] < w[1]));
        assert!(LAUNCHERS.iter().all(|l| l.to_ascii_lowercase() == *l));
    }

    #[test]
    fn browser_children_count_toward_the_browser() {
        let procs = [
            p(1, 0, 1, "systemd"),
            p(10, 1, 10, "firefox"),
            p(11, 10, 10, "Isolated Web Co"),
            p(12, 10, 10, "RDD Process"),
        ];
        assert_eq!(
            programs(&procs),
            ["systemd", "firefox", "firefox", "firefox"]
        );
    }

    #[test]
    fn apps_a_session_manager_restores_are_programs_of_their_own() {
        let procs = [
            p(3270, 1, 3270, "systemd"),
            p(3300, 3270, 3300, "ksmserver"),
            p(3301, 3300, 3300, "firefox-bin"),
            p(3302, 3301, 3300, "Isolated Web Co"),
            p(3303, 3300, 3300, "xfce4-terminal"),
        ];
        assert_eq!(
            programs(&procs),
            [
                "systemd",
                "ksmserver",
                "firefox-bin",
                "firefox-bin",
                "xfce4-terminal"
            ]
        );
    }

    #[test]
    fn shell_jobs_are_programs_of_their_own() {
        let procs = [
            p(1, 0, 1, "systemd"),
            p(20, 1, 20, "kitty"),
            p(21, 20, 21, "bash"),
            p(22, 21, 22, "cargo"),
            p(23, 22, 22, "rustc"),
            p(24, 21, 24, "make"),
            p(25, 24, 24, "sh"),
            p(26, 25, 24, "cc1"),
        ];
        assert_eq!(
            programs(&procs),
            [
                "systemd", "kitty", "bash", "cargo", "cargo", "make", "make", "make"
            ]
        );
    }

    #[test]
    fn a_session_wrapper_runs_programs_of_their_own() {
        let procs = [
            p(3270, 1, 3270, "systemd"),
            p(3271, 3270, 3271, "dbus-run-session"),
            p(3272, 3271, 3271, "dbus-daemon"),
            p(3273, 3271, 3271, "kwin_x11"),
        ];
        assert_eq!(
            programs(&procs),
            ["systemd", "dbus-run-session", "dbus-daemon", "kwin_x11"]
        );
    }

    #[test]
    fn a_parent_younger_than_its_child_is_a_reused_pid() {
        let mut stale = p(30, 1, 30, "editor");
        stale.start_ns = 100;
        assert_eq!(
            programs(&[p(31, 30, 30, "helper"), stale]),
            ["helper", "editor"]
        );
    }

    #[test]
    fn launcher_children_without_process_groups_are_programs_of_their_own() {
        let procs = [
            p(4, 0, 0, "explorer"),
            p(8, 4, 0, "chrome"),
            p(12, 8, 0, "chrome"),
            p(16, 4, 0, "cmd"),
            p(20, 16, 0, "msbuild"),
            p(24, 20, 0, "cl"),
        ];
        assert_eq!(
            programs(&procs),
            ["explorer", "chrome", "chrome", "cmd", "msbuild", "msbuild"]
        );
    }
}
