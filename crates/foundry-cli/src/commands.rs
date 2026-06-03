pub(crate) fn parse_throttle(s: &str) -> i32 {
    match s {
        "dry_run" => 1,
        _ => 0,
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
}
