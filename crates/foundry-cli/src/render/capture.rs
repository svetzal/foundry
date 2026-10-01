//! Pure rendering of bounded command results.
use std::path::Path;

/// Passing commands emit no log contents. Failures include bounded tails.
pub fn result(program: &str, code: i32, stdout: &Path, stderr: &Path, failures: &str) -> String {
    format!(
        "{program}: exit {code}\nstdout: {}\nstderr: {}\n{failures}",
        stdout.display(),
        stderr.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passing_capture_keeps_log_locations_without_verbose_output() {
        let text = result("cargo", 0, Path::new("out.log"), Path::new("err.log"), "");
        assert_eq!(text, "cargo: exit 0\nstdout: out.log\nstderr: err.log\n");
    }
}
