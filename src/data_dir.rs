use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The environment variable that names the data folder.
const DIR_VARIABLE: &str = "PROCESS_WATCH_DIR";

/// The folder beside the executable that turns on portable mode.
const PORTABLE_FOLDER: &str = "data";

/// Where recordings live: `explicit` when the command line names a folder,
/// else `PROCESS_WATCH_DIR`, else a `data` folder beside the pw executable
/// when one exists (portable mode), else the platform's per-user state folder.
pub fn resolve(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir.to_owned());
    }
    if let Some(dir) = std::env::var_os(DIR_VARIABLE).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = portable_dir() {
        return Ok(dir);
    }
    platform_dir()
        .with_context(|| format!("no home folder to keep recordings in; set {DIR_VARIABLE}"))
}

/// The portable folder beside the running executable, when it exists.
fn portable_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    portable_dir_beside(&exe).filter(|dir| dir.is_dir())
}

/// Where the portable folder of the executable at `exe` would be.
fn portable_dir_beside(exe: &Path) -> Option<PathBuf> {
    exe.parent().map(|dir| dir.join(PORTABLE_FOLDER))
}

/// `%LOCALAPPDATA%\process-watch`.
#[cfg(windows)]
fn platform_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("process-watch"))
}

/// `~/Library/Application Support/process-watch`.
#[cfg(target_os = "macos")]
fn platform_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library/Application Support/process-watch"))
}

/// `$XDG_STATE_HOME/process-watch`, or `~/.local/state/process-watch`.
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_dir() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(state).join("process-watch"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/process-watch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_named_on_the_command_line_wins() {
        let named = Path::new("/somewhere/else");
        assert_eq!(resolve(Some(named)).unwrap(), named);
    }

    #[test]
    fn the_portable_folder_sits_beside_the_executable() {
        let exe = Path::new("/opt/process-watch/pw");
        assert_eq!(
            portable_dir_beside(exe),
            Some(PathBuf::from("/opt/process-watch/data"))
        );
    }

    #[test]
    fn an_executable_with_no_portable_folder_is_not_portable() {
        // Test binaries live in target/<profile>/deps, which holds no `data` folder.
        assert_eq!(portable_dir(), None);
    }
}
