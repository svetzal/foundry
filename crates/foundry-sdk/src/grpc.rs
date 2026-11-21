//! gRPC transport limits shared by `foundryd` and the `foundry` CLI.
//!
//! tonic refuses to decode a message larger than 4 MiB by default. Whole-ledger
//! reads outgrow that: on mojility-ops-01 a 47 MB `work-items.json` with 3,973
//! items made `ListWorkItems` answer 4.2 MB, and `foundry queue` failed with
//! "decoded message length too large". Both ends apply the same limit, in each
//! direction, so the daemon refuses to send what the CLI would refuse to read.
//!
//! The limit is `FOUNDRY_GRPC_MAX_MESSAGE_MIB` mebibytes (default 64, about
//! fifteen times that ledger's response). An unset, unparseable or `0` value
//! means the default. Set it on both the daemon and the CLI host when you
//! change it.

/// Environment variable that sets the per-message limit, in mebibytes.
pub const MAX_MESSAGE_MIB_ENV: &str = "FOUNDRY_GRPC_MAX_MESSAGE_MIB";

/// Default per-message limit, in mebibytes.
pub const DEFAULT_MAX_MESSAGE_MIB: u32 = 64;

const MIB: usize = 1024 * 1024;

/// The default per-message limit, in bytes.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = DEFAULT_MAX_MESSAGE_MIB as usize * MIB;

/// The per-message limit from `FOUNDRY_GRPC_MAX_MESSAGE_MIB`, in bytes.
#[must_use]
pub fn max_message_bytes() -> usize {
    parse_max_message_bytes(std::env::var(MAX_MESSAGE_MIB_ENV).ok().as_deref())
}

/// The limit a `FOUNDRY_GRPC_MAX_MESSAGE_MIB` value selects, in bytes.
///
/// Unset, unparseable or `0` means [`DEFAULT_MAX_MESSAGE_BYTES`]; a value too
/// large for `usize` saturates.
///
/// ```
/// use foundry_sdk::grpc::{DEFAULT_MAX_MESSAGE_BYTES, parse_max_message_bytes};
///
/// assert_eq!(parse_max_message_bytes(Some("128")), 128 * 1024 * 1024);
/// assert_eq!(parse_max_message_bytes(None), DEFAULT_MAX_MESSAGE_BYTES);
/// ```
#[must_use]
pub fn parse_max_message_bytes(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|mib| *mib > 0)
        .map_or(DEFAULT_MAX_MESSAGE_BYTES, |mib| {
            usize::try_from(mib).map_or(usize::MAX, |mib| mib.saturating_mul(MIB))
        })
}

/// Whether a gRPC status message reports a message over the size limit.
///
/// tonic words both directions as "... message length too large ...".
#[must_use]
pub fn is_message_too_large(message: &str) -> bool {
    message.contains("message length too large")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_64_mib() {
        assert_eq!(DEFAULT_MAX_MESSAGE_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    fn a_value_in_mebibytes_selects_that_many_bytes() {
        assert_eq!(parse_max_message_bytes(Some("16")), 16 * MIB);
        assert_eq!(parse_max_message_bytes(Some(" 256 ")), 256 * MIB);
    }

    #[test]
    fn unset_empty_unparseable_or_zero_means_the_default() {
        for value in [
            None,
            Some(""),
            Some("lots"),
            Some("-1"),
            Some("1.5"),
            Some("0"),
        ] {
            assert_eq!(
                parse_max_message_bytes(value),
                DEFAULT_MAX_MESSAGE_BYTES,
                "value {value:?}"
            );
        }
    }

    #[test]
    fn the_largest_value_does_not_overflow() {
        let bytes = parse_max_message_bytes(Some(&u32::MAX.to_string()));
        assert!(bytes >= u32::MAX as usize);
    }

    #[test]
    fn tonics_size_errors_in_both_directions_are_recognised() {
        assert!(is_message_too_large(
            "Error, decoded message length too large: found 4208032 bytes, the limit is: 4194304 bytes"
        ));
        assert!(is_message_too_large("Error, encoded message length too large: found 9 bytes"));
        assert!(!is_message_too_large("project not found"));
    }
}
