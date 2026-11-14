use std::path::Path;

use anyhow::Result;

use crate::store::{self, Cleared, RecorderLock};
use crate::{data_dir, size};

/// Deletes every recording in the data folder and says what went.
pub fn clear(dir: Option<&Path>) -> Result<String> {
    let dir = data_dir::resolve(dir)?;
    let cleared = store::clear(&dir)?;
    Ok(describe(cleared, &dir, RecorderLock::is_held(&dir)))
}

/// The line a clear prints.
fn describe(cleared: Cleared, dir: &Path, recorder_running: bool) -> String {
    let files = if cleared.files == 1 { "file" } else { "files" };
    let still = if recorder_running {
        "; the recorder keeps recording from now"
    } else {
        ""
    };
    format!(
        "pw: cleared {} recording {files} ({}) from {}{still}\n",
        cleared.files,
        size::format(i64::try_from(cleared.bytes).unwrap_or(i64::MAX)),
        dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clear_names_what_it_removed() {
        let cleared = Cleared {
            files: 3,
            bytes: 2048,
        };
        assert_eq!(
            describe(cleared, Path::new("d"), false),
            "pw: cleared 3 recording files (2.0 KiB) from d\n"
        );
    }

    #[test]
    fn a_clear_says_a_running_recorder_goes_on() {
        let cleared = Cleared { files: 1, bytes: 9 };
        assert_eq!(
            describe(cleared, Path::new("d"), true),
            "pw: cleared 1 recording file (9 B) from d; the recorder keeps recording from now\n"
        );
    }
}
