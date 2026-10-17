//! The bounded view of a gate's output that `GateResult::output` carries.
//!
//! A gate's stdout and stderr are tailed separately and share one budget, so a
//! tool that floods one stream (cargo's compile lines on stderr) cannot push
//! the other (the failing test name on stdout) out of the view. Marker lines
//! label the sections: [`STDERR_MARKER`] separates the two streams of one
//! command, and the autofix markers separate the runs of a self-heal attempt.
//! Readers that shorten the view further use [`compact_output`], which keeps a
//! tail of every section instead of a tail of the whole text.

/// Line that separates a command's stdout tail from its stderr tail. Present
/// only when both streams printed something.
pub const STDERR_MARKER: &str = "--- stderr ---";

/// Line that introduces the re-check run's view after an autofix ran but the
/// gate still failed.
pub const AUTOFIX_STILL_FAILING_MARKER: &str = "--- autofix attempted, gate still failing ---";

/// Line that introduces the fix command's view when the fix itself failed.
pub const AUTOFIX_FAILED_MARKER: &str = "--- autofix command failed ---";

const SECTION_MARKERS: [&str; 3] = [
    STDERR_MARKER,
    AUTOFIX_STILL_FAILING_MARKER,
    AUTOFIX_FAILED_MARKER,
];

/// Share `budget` among parts that need `needs[i]` units each.
///
/// Every part is guaranteed `budget / needs.len()` units (or all it needs, if
/// that is less), and the allowance a part does not need goes to the others.
/// The result never exceeds `budget` in total, nor any part's need.
pub(crate) fn share_budget(needs: &[usize], budget: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..needs.len()).collect();
    order.sort_by_key(|&i| needs[i]);
    let mut shares = vec![0; needs.len()];
    let mut remaining = budget;
    for (served, &i) in order.iter().enumerate() {
        let fair = remaining / (needs.len() - served);
        shares[i] = needs[i].min(fair);
        remaining -= shares[i];
    }
    shares
}

/// The bounded view of one command's output: at most `max_lines` lines in
/// total, shared between the stdout tail and the stderr tail by
/// [`share_budget`], with [`STDERR_MARKER`] between them when both have
/// content. The marker line is not counted against `max_lines`.
pub(crate) fn command_view(stdout: &str, stderr: &str, max_lines: usize) -> String {
    let (stdout, stderr) = (stdout.trim(), stderr.trim());
    let lines = |s: &str| if s.is_empty() { 0 } else { s.lines().count() };
    let shares = share_budget(&[lines(stdout), lines(stderr)], max_lines);
    let stdout_tail = tail_lines(stdout, shares[0]);
    let stderr_tail = tail_lines(stderr, shares[1]);
    match (stdout_tail.is_empty(), stderr_tail.is_empty()) {
        (false, false) => format!("{stdout_tail}\n{STDERR_MARKER}\n{stderr_tail}"),
        (false, true) => stdout_tail,
        (true, _) => stderr_tail,
    }
}

/// Keep at most the last `n` lines from `text`, trimming leading/trailing whitespace.
pub(crate) fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        text.trim().to_string()
    } else {
        lines[lines.len() - n..].join("\n").trim().to_string()
    }
}

/// Shorten a gate's `output` to about `max_bytes` while keeping a tail of
/// every section.
///
/// A plain byte tail of the view would keep only its last section (stderr,
/// or the last run of a self-heal attempt) and drop the stdout lines that
/// usually name the failure. Here the marker lines are always kept and the
/// section bodies share `max_bytes` fairly (a section that needs less gives
/// the rest to the others). A body that had to be cut starts with a one-line
/// note saying how many bytes were omitted, so the result may exceed
/// `max_bytes` by those notes and the marker lines.
/// Output that already fits is returned unchanged.
pub fn compact_output(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let sections = split_sections(output);
    let needs: Vec<usize> = sections.iter().map(|(_, body)| body.len()).collect();
    let shares = share_budget(&needs, max_bytes);
    sections
        .iter()
        .zip(shares)
        .flat_map(|((marker, body), share)| {
            let body = tail_bytes(body, share);
            marker.map(str::to_string).into_iter().chain((!body.is_empty()).then_some(body))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split a view into `(marker line, body)` sections. The first section has no
/// marker; each later one starts at a line that is exactly a section marker.
fn split_sections(output: &str) -> Vec<(Option<&str>, String)> {
    let mut sections = vec![(None, Vec::new())];
    for line in output.lines() {
        if SECTION_MARKERS.contains(&line) {
            sections.push((Some(line), Vec::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push(line);
        }
    }
    sections.into_iter().map(|(marker, body)| (marker, body.join("\n"))).collect()
}

/// The last `max_bytes` of `text`, rounded to a char boundary, with a note in
/// front when anything was cut.
fn tail_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let note = format!("[... {start} bytes omitted ...]");
    if start == text.len() {
        note
    } else {
        format!("{note}\n{}", &text[start..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(prefix: &str, count: usize) -> String {
        (0..count).map(|i| format!("{prefix} {i}")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn both_overflowing_streams_get_half_the_budget() {
        assert_eq!(share_budget(&[300, 300], 200), vec![100, 100]);
    }

    #[test]
    fn a_stream_that_needs_less_donates_the_rest() {
        assert_eq!(share_budget(&[30, 500], 200), vec![30, 170]);
        assert_eq!(share_budget(&[500, 30], 200), vec![170, 30]);
        assert_eq!(share_budget(&[10, 20], 200), vec![10, 20]);
    }

    #[test]
    fn many_parts_share_fairly_and_never_exceed_the_budget() {
        let shares = share_budget(&[5, 1_000, 1_000, 1_000], 100);
        assert_eq!(shares[0], 5);
        assert!(shares[1..].iter().all(|&s| s >= 31));
        assert!(shares.iter().sum::<usize>() <= 100);
        assert!(share_budget(&[], 100).is_empty());
    }

    #[test]
    fn the_view_keeps_stdout_failure_lines_when_stderr_floods() {
        let stdout =
            format!("{}\ntest foo::bar ... FAILED\ntest result: FAILED", numbered("ok", 5));
        let stderr = numbered("Compiling crate", 500);

        let view = command_view(&stdout, &stderr, 200);

        assert!(view.contains("test foo::bar ... FAILED"));
        assert!(view.contains("ok 0"), "stdout fits whole, so all of it stays");
        assert!(view.contains(&format!("\n{STDERR_MARKER}\n")));
        assert!(view.ends_with("Compiling crate 499"));
        assert_eq!(view.lines().count(), 201, "200 content lines plus the marker");
    }

    #[test]
    fn the_view_tails_both_streams_when_both_overflow() {
        let view = command_view(&numbered("out", 300), &numbered("err", 300), 200);
        let (out, err) = view.split_once(&format!("\n{STDERR_MARKER}\n")).unwrap();
        assert_eq!(out.lines().count(), 100);
        assert!(out.starts_with("out 200") && out.ends_with("out 299"));
        assert_eq!(err.lines().count(), 100);
        assert!(err.starts_with("err 200") && err.ends_with("err 299"));
    }

    #[test]
    fn the_marker_appears_only_when_both_streams_have_content() {
        assert_eq!(command_view("only out\n", "  \n", 200), "only out");
        assert_eq!(command_view("", "only err", 200), "only err");
        assert_eq!(command_view("", "", 200), "");
    }

    #[test]
    fn tail_lines_over_limit_truncates() {
        let input = numbered("line", 201);
        let result = tail_lines(&input, 200);
        assert_eq!(result.lines().count(), 200);
        assert!(result.starts_with("line 1"));
        assert!(result.ends_with("line 200"));
    }

    #[test]
    fn tail_lines_at_limit_unchanged() {
        let input = numbered("line", 200);
        let result = tail_lines(&input, 200);
        assert_eq!(result.lines().count(), 200);
        assert!(result.starts_with("line 0"));
        assert!(result.ends_with("line 199"));
    }

    #[test]
    fn tail_lines_trims_whitespace() {
        assert_eq!(tail_lines("\n  hello\nworld  \n", 200), "hello\nworld");
    }

    #[test]
    fn compaction_returns_fitting_output_unchanged() {
        assert_eq!(compact_output("short", 512), "short");
    }

    #[test]
    fn compaction_keeps_a_tail_of_every_section() {
        let view = format!(
            "{}\ntest foo::bar ... FAILED\n{STDERR_MARKER}\n{}",
            numbered("ok", 3),
            numbered("Compiling crate", 400)
        );

        let compact = compact_output(&view, 512);

        assert!(compact.contains("test foo::bar ... FAILED"));
        assert!(compact.contains(STDERR_MARKER));
        assert!(compact.ends_with("Compiling crate 399"));
        assert!(compact.contains("bytes omitted"));
        assert!(compact.len() < 512 + 100);
    }

    #[test]
    fn compaction_keeps_every_run_of_a_failed_self_heal() {
        let noise = numbered("noise", 300);
        let view = format!(
            "first failure\n{STDERR_MARKER}\n{noise}\n{AUTOFIX_STILL_FAILING_MARKER}\nrecheck failure\n{STDERR_MARKER}\n{noise}"
        );

        let compact = compact_output(&view, 1_024);

        assert!(compact.contains("first failure"));
        assert!(compact.contains(AUTOFIX_STILL_FAILING_MARKER));
        assert!(compact.contains("recheck failure"));
        assert_eq!(compact.matches(STDERR_MARKER).count(), 2);
    }

    #[test]
    fn compaction_cuts_on_a_char_boundary() {
        let view = format!("{}FINAL ERROR", "é".repeat(2_000));
        let compact = compact_output(&view, 101);
        assert!(compact.ends_with("FINAL ERROR"));
        assert!(compact.starts_with("[... "));
    }
}
