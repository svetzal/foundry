//! Pure rendering for sentinel display.

use std::fmt::Write as _;

use comfy_table::{ContentArrangement, Table};
use foundry_sdk::sentinel::{Schedule, SentinelEntry};

/// Render a sentinel's full detail view as a multi-line string.
pub fn sentinel_detail(entry: &SentinelEntry) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Name:      {}", entry.name);
    let _ = writeln!(out, "Schedule:  {}", schedule_summary(&entry.schedule));
    let _ = writeln!(out, "Enabled:   {}", if entry.enabled { "yes" } else { "no" });
    let _ = writeln!(out, "Emits:     {}", entry.emit.event_type.as_str());
    let _ = writeln!(out, "Project:   {}", entry.emit.project);
    let _ = writeln!(out, "Throttle:  {}", entry.emit.throttle);
    let _ = writeln!(out, "Payload:   {}", entry.emit.payload);
    out
}

/// Render the sentinel list as a `comfy_table` string (ends with `\n`).
pub fn sentinel_table(entries: &[SentinelEntry]) -> String {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Name", "Schedule", "Emits", "Project", "Enabled"]);

    for s in entries {
        let schedule = schedule_summary(&s.schedule);
        let event_type = s.emit.event_type.as_str();
        table.add_row(vec![
            s.name.as_str(),
            schedule.as_str(),
            event_type.as_str(),
            s.emit.project.as_str(),
            if s.enabled { "yes" } else { "no" },
        ]);
    }

    let mut out = String::new();
    let _ = writeln!(out, "{table}");
    out
}

fn schedule_summary(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Cron(expr) => format!("cron: {expr}"),
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::sentinel::SentinelStore;

    use super::*;

    // -- schedule_summary (moved from sentinel_commands.rs) --

    #[test]
    fn schedule_summary_renders_cron_prefix() {
        let s = Schedule::Cron("0 2 * * *".to_string());
        assert_eq!(schedule_summary(&s), "cron: 0 2 * * *");
    }

    // -- sentinel_detail content assertions --

    #[test]
    fn sentinel_detail_contains_name_and_schedule() {
        let store = SentinelStore::default_seed();
        let entry = store.find_sentinel("nightly-maintenance").expect("must exist");
        let out = sentinel_detail(entry);
        assert!(out.contains("Name:      nightly-maintenance"), "got: {out}");
        assert!(out.contains("Schedule:  cron:"), "got: {out}");
    }

    #[test]
    fn sentinel_detail_shows_enabled_status() {
        let store = SentinelStore::default_seed();
        let entry = store.find_sentinel("nightly-maintenance").expect("must exist");
        let out = sentinel_detail(entry);
        // Default seed is enabled.
        assert!(out.contains("Enabled:   yes"), "got: {out}");
    }

    #[test]
    fn sentinel_detail_contains_emits_and_project() {
        let store = SentinelStore::default_seed();
        let entry = store.find_sentinel("nightly-maintenance").expect("must exist");
        let out = sentinel_detail(entry);
        assert!(out.contains("Emits:"), "got: {out}");
        assert!(out.contains("Project:"), "got: {out}");
    }

    // -- sentinel_table content assertions --

    #[test]
    fn sentinel_table_contains_all_sentinel_names_from_seed() {
        let store = SentinelStore::default_seed();
        let out = sentinel_table(&store.sentinels);
        assert!(out.contains("nightly-maintenance"), "got: {out}");
    }

    #[test]
    fn sentinel_table_contains_enabled_column() {
        let store = SentinelStore::default_seed();
        let out = sentinel_table(&store.sentinels);
        // All seed sentinels are enabled.
        assert!(out.contains("yes"), "got: {out}");
    }
}
