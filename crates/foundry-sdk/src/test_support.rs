//! Helpers for tests that need real files on disk, gated behind the
//! `test-support` feature.

/// Writes `contents` to `path` and makes it executable (mode `0o755`), so a
/// test can then run it as a stand-in tool, hook or wrapper.
///
/// A child `/bin/sh` writes the file. This process never opens `path`, and
/// that is the point. Tests run on parallel threads, and any of them can fork
/// to spawn a process. A forked child gets a copy of every descriptor this
/// process holds and keeps it until it execs (`O_CLOEXEC` closes it only at
/// exec). On Linux, `execve` fails with `ETXTBSY` ("Text file busy") while
/// any process holds a write descriptor on the file's inode. So
/// `std::fs::write` followed by a spawn races any sibling fork that lands
/// while the file is open.
///
/// Writing to a temporary sibling and renaming it into place does not help:
/// rename keeps the inode, so the descriptor a racing child inherited is still
/// a write descriptor on the file that is then executed. Here the only write
/// descriptor lives in the child shell, which has exited (with every process
/// it started) before this function returns. No write descriptor on the inode
/// can exist when the caller execs it, whatever other threads do.
///
/// # Errors
///
/// Returns an error if the shell cannot be spawned or the write or `chmod`
/// fails.
#[cfg(unix)]
pub fn write_executable(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#,
            "sh",
            contents,
        ])
        .arg(path)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "writing executable {} failed ({}): {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::write_executable;

    #[test]
    fn writes_the_contents_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        let contents = "#!/bin/sh\nprintf '%s\\n' \"$1\" 100% `x` -n\n";

        write_executable(&path, contents).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn makes_the_file_executable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");

        write_executable(&path, "#!/bin/sh\nexit 0\n").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, "a much longer original body").unwrap();

        write_executable(&path, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn the_written_script_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool with 'quotes'");

        write_executable(&path, "#!/bin/sh\necho \"ran with $1\"\nexit 3\n").unwrap();

        let output = std::process::Command::new(&path).arg("arg").output().unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "ran with arg\n");
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent/tool");

        let err = write_executable(&path, "#!/bin/sh\n").unwrap_err();

        assert!(err.to_string().contains("absent/tool"), "{err}");
    }
}
