pub(crate) fn parse_throttle(s: &str) -> i32 {
    match s {
        "dry_run" => 1,
        _ => 0,
    }
}

/// Parse a `--not-before` value: an RFC 3339 time, or a duration from `now`
/// such as `30m`, `2h` or `1d` (units `s`, `m`, `h`, `d`).
pub(crate) fn parse_not_before(
    text: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    let text = text.trim();
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(at.with_timezone(&chrono::Utc));
    }
    let duration = parse_duration(text).ok_or_else(|| {
        anyhow::anyhow!(
            "--not-before takes an RFC 3339 time (2026-10-11T09:00:00Z) or a duration such as 2h; got '{text}'"
        )
    })?;
    Ok(now + duration)
}

/// Parse a duration like `90s`, `30m`, `2h` or `1d`.
fn parse_duration(text: &str) -> Option<chrono::Duration> {
    let (digits, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let amount: i64 = digits.parse().ok()?;
    match unit {
        "s" => Some(chrono::Duration::seconds(amount)),
        "m" => Some(chrono::Duration::minutes(amount)),
        "h" => Some(chrono::Duration::hours(amount)),
        "d" => Some(chrono::Duration::days(amount)),
        _ => None,
    }
}

/// Parse a W3C traceparent header value into `(trace_id, parent_span_id)`.
///
/// Format: `00-<trace_id 32 hex>-<span_id 16 hex>-<flags 2 hex>`.
/// Returns `(None, None)` for any malformed input (wrong version, wrong number
/// of parts, or wrong field lengths).
fn parse_traceparent(value: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 4 || parts[0] != "00" || parts[1].len() != 32 || parts[2].len() != 16 {
        return (None, None);
    }
    (Some(parts[1].to_string()), Some(parts[2].to_string()))
}

/// Read the `TRACEPARENT` environment variable and parse it.
///
/// Thin wrapper around [`parse_traceparent`]. Returns `(None, None)` when the
/// env var is absent or malformed.
pub(crate) fn parse_traceparent_from_env() -> (Option<String>, Option<String>) {
    match std::env::var("TRACEPARENT") {
        Ok(v) => parse_traceparent(&v),
        Err(_) => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_traceparent tests --

    #[test]
    fn parse_traceparent_well_formed() {
        let (t, p) = parse_traceparent("00-0123456789abcdef0123456789abcdef-fedcba9876543210-01");
        assert_eq!(t.as_deref(), Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(p.as_deref(), Some("fedcba9876543210"));
    }

    #[test]
    fn parse_traceparent_well_formed_unsampled_flags() {
        let (t, p) = parse_traceparent("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00");
        assert_eq!(t.as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert_eq!(p.as_deref(), Some("bbbbbbbbbbbbbbbb"));
    }

    #[test]
    fn parse_traceparent_empty_returns_none() {
        assert_eq!(parse_traceparent(""), (None, None));
    }

    #[test]
    fn parse_traceparent_unrecognised_string() {
        assert_eq!(parse_traceparent("malformed"), (None, None));
    }

    #[test]
    fn parse_traceparent_wrong_part_count() {
        // Only three parts.
        assert_eq!(
            parse_traceparent("00-0123456789abcdef0123456789abcdef-fedcba9876543210"),
            (None, None)
        );
        // Five parts.
        assert_eq!(
            parse_traceparent("00-0123456789abcdef0123456789abcdef-fedcba9876543210-01-extra"),
            (None, None)
        );
    }

    #[test]
    fn parse_traceparent_wrong_version() {
        assert_eq!(
            parse_traceparent("ff-0123456789abcdef0123456789abcdef-fedcba9876543210-01"),
            (None, None)
        );
    }

    #[test]
    fn parse_traceparent_wrong_trace_id_length() {
        // 31 hex chars instead of 32.
        assert_eq!(
            parse_traceparent("00-0123456789abcdef0123456789abcde-fedcba9876543210-01"),
            (None, None)
        );
    }

    #[test]
    fn parse_traceparent_wrong_span_id_length() {
        // 15 hex chars instead of 16.
        assert_eq!(
            parse_traceparent("00-0123456789abcdef0123456789abcdef-fedcba987654321-01"),
            (None, None)
        );
    }

    #[test]
    fn parse_traceparent_short_fields() {
        assert_eq!(parse_traceparent("00-tooshort-also-01"), (None, None));
    }

    // -- parse_not_before tests --

    fn now() -> chrono::DateTime<chrono::Utc> {
        "2026-10-10T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn not_before_accepts_an_rfc3339_time_in_any_offset() {
        let at = parse_not_before("2026-10-11T09:00:00-04:00", now()).unwrap();
        assert_eq!(at.to_rfc3339(), "2026-10-11T13:00:00+00:00");
    }

    #[test]
    fn not_before_accepts_a_duration_from_now() {
        assert_eq!(
            parse_not_before("2h", now()).unwrap().to_rfc3339(),
            "2026-10-10T14:00:00+00:00"
        );
        assert_eq!(
            parse_not_before("30m", now()).unwrap().to_rfc3339(),
            "2026-10-10T12:30:00+00:00"
        );
        assert_eq!(
            parse_not_before("90s", now()).unwrap().to_rfc3339(),
            "2026-10-10T12:01:30+00:00"
        );
        assert_eq!(
            parse_not_before("1d", now()).unwrap().to_rfc3339(),
            "2026-10-11T12:00:00+00:00"
        );
    }

    #[test]
    fn not_before_names_what_it_rejects() {
        for bad in ["tomorrow", "2h30m", "", "h2", "10"] {
            let err = parse_not_before(bad, now()).unwrap_err();
            assert!(err.to_string().contains("--not-before takes"), "{bad}: {err}");
        }
    }
}
