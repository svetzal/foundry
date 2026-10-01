//! Formation transcript observations, kept separate from terminal accounting.
use super::text;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::BufRead;
use std::path::Path;

/// One formation session, including failed and unfinished invocations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormationSessionReport {
    pub session_id: String,
    pub trace_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub source_log_path: String,
    pub prompt_bytes: Option<u64>,
    pub usage: Option<crate::token_usage::SessionUsage>,
    /// `None` with an explicit error means unavailable, never zero activity.
    pub tool_activity: Option<FormationToolActivity>,
    pub transcript_error: Option<String>,
    pub native_observation: Option<NativeFormationObservation>,
    pub native_observation_error: Option<String>,
}

/// Captured process output, not tokens or text necessarily visible to the model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FormationToolActivity {
    pub completed_commands: u64,
    pub failed_commands: u64,
    pub repeated_commands: u64,
    pub captured_output_bytes: u64,
    pub incomplete_lines: u64,
    pub native_thread_id: Option<String>,
}

/// Last cumulative provider observation, not final or additional billed usage.
/// Input includes cached tokens. In interrupted sessions these counts are a lower bound.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeFormationObservation {
    pub log_path: String,
    pub observed_at: String,
    pub usage_observations: u64,
    pub first_request_input_tokens: u64,
    pub input_tokens_including_cache: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub tool_output_text_bytes: u64,
    pub tool_outputs_with_truncation_markers: u64,
}

/// A durable formation decision. Event IDs link back to full prompts and gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormationDecisionReport {
    pub event_id: String,
    pub trace_id: String,
    pub occurred_at: String,
    pub decision: String,
    pub reason: String,
    pub last_task_run_event_id: Option<String>,
    /// `None` means forced or evidence unavailable; it does not prove either.
    pub prompt_bytes: Option<usize>,
}

pub(super) fn read_tool_activity(path: &Path) -> anyhow::Result<FormationToolActivity> {
    let mut result = FormationToolActivity::default();
    let mut ids = BTreeSet::new();
    let mut commands = BTreeSet::new();
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) if !line.ends_with('\n') => {
                result.incomplete_lines += 1;
                break;
            }
            Err(error) => return Err(error.into()),
        };
        if value["type"] == "thread.started" {
            result.native_thread_id = value["thread_id"].as_str().map(str::to_owned);
        }
        let item = &value["item"];
        if value["type"] == "item.completed" && item["type"] == "command_execution" {
            if let Some(id) = item["id"].as_str()
                && !ids.insert(id.to_owned())
            {
                line.clear();
                continue;
            }
            result.completed_commands += 1;
            if item["exit_code"].as_i64().is_some_and(|code| code != 0) {
                result.failed_commands += 1;
            }
            if let Some(command) = item["command"].as_str()
                && !commands.insert(command.to_owned())
            {
                result.repeated_commands += 1;
            }
            result.captured_output_bytes +=
                item["aggregated_output"].as_str().map_or(0, |s| s.len() as u64);
        }
        line.clear();
    }
    if result.native_thread_id.is_none() {
        anyhow::bail!("unsupported formation transcript: no Codex thread record");
    }
    Ok(result)
}

pub(super) fn find_native_log(root: &Path, id: &str) -> anyhow::Result<std::path::PathBuf> {
    // Only inspect filenames here; never read unrelated session contents.
    let suffix = format!("-{id}.jsonl");
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories.push(entry.path());
            } else if entry.file_type()?.is_file()
                && entry.file_name().to_string_lossy().ends_with(&suffix)
            {
                return Ok(entry.path());
            }
        }
    }
    anyhow::bail!("native transcript unavailable for thread {id}")
}

pub(super) fn read_native_observation(path: &Path) -> anyhow::Result<NativeFormationObservation> {
    let mut report = NativeFormationObservation {
        log_path: path.to_string_lossy().into(),
        observed_at: String::new(),
        usage_observations: 0,
        first_request_input_tokens: 0,
        input_tokens_including_cache: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
        tool_output_text_bytes: 0,
        tool_outputs_with_truncation_markers: 0,
    };
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) if !line.ends_with('\n') => break,
            Err(error) => return Err(error.into()),
        };
        let p = &value["payload"];
        if value["type"] == "event_msg" && p["type"] == "token_count" {
            let total = &p["info"]["total_token_usage"];
            if let Some(input) = total["input_tokens"].as_u64() {
                if report.usage_observations == 0 {
                    report.first_request_input_tokens =
                        p["info"]["last_token_usage"]["input_tokens"].as_u64().unwrap_or(input);
                }
                report.usage_observations += 1;
                report.observed_at = text(&value["timestamp"]);
                report.input_tokens_including_cache = input;
                report.cached_input_tokens = total["cached_input_tokens"].as_u64().unwrap_or(0);
                report.output_tokens = total["output_tokens"].as_u64().unwrap_or(0);
            }
        }
        if value["type"] == "response_item"
            && matches!(
                p["type"].as_str(),
                Some("custom_tool_call_output" | "function_call_output")
            )
        {
            let outputs: Vec<&str> = if let Some(text) = p["output"].as_str() {
                vec![text]
            } else {
                p["output"]
                    .as_array()
                    .map(|parts| parts.iter().filter_map(|v| v["text"].as_str()).collect())
                    .unwrap_or_default()
            };
            for output in outputs {
                report.tool_output_text_bytes += output.len() as u64;
                if output.contains("Warning: truncated output")
                    || output.contains("tokens truncated")
                {
                    report.tool_outputs_with_truncation_markers += 1;
                }
            }
        }
        line.clear();
    }
    if report.usage_observations == 0 {
        anyhow::bail!("native transcript has no usage observations");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fmt::Write;
    fn json_lines(records: &[Value]) -> String {
        let mut body = String::new();
        for record in records {
            writeln!(body, "{record}").unwrap();
        }
        body
    }

    #[test]
    fn completed_commands_are_deduplicated_and_partial_append_is_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.jsonl");
        let item = json!({"type":"item.completed", "item":{"id":"one", "type":"command_execution", "command":"read", "exit_code":0, "aggregated_output":"é"}});
        let repeat = json!({"type":"item.completed", "item":{"id":"two", "type":"command_execution", "command":"read", "exit_code":1, "aggregated_output":"bad"}});
        let records = [
            json!({"type":"thread.started","thread_id":"native"}),
            json!({"type":"item.started","item":{"id":"one","type":"command_execution"}}),
            item.clone(),
            item,
            repeat,
        ];
        let mut body = json_lines(&records);
        body.push_str("{broken");
        std::fs::write(&path, body).unwrap();
        let activity = read_tool_activity(&path).unwrap();
        assert_eq!(activity.completed_commands, 2);
        assert_eq!(activity.repeated_commands, 1);
        assert_eq!(activity.failed_commands, 1);
        assert_eq!(activity.captured_output_bytes, 5);
        assert_eq!(activity.incomplete_lines, 1);
        assert!(read_tool_activity(&dir.path().join("absent")).is_err());
        std::fs::write(&path, "broken\n").unwrap();
        assert!(read_tool_activity(&path).is_err());
    }

    #[test]
    fn native_usage_retains_last_cumulative_observation_without_summing_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-id.jsonl");
        let events = [
            json!({"timestamp":"first","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":3},"last_token_usage":{"input_tokens":100}}}}),
            json!({"timestamp":"last","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":250,"cached_input_tokens":100,"output_tokens":7},"last_token_usage":{"input_tokens":150}}}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"Warning: truncated output"}]}}),
        ];
        std::fs::write(&path, json_lines(&events)).unwrap();
        let observation = read_native_observation(&path).unwrap();
        assert_eq!(observation.input_tokens_including_cache, 250);
        assert_eq!(observation.cached_input_tokens, 100);
        assert_eq!(observation.first_request_input_tokens, 100);
        assert_eq!(observation.output_tokens, 7);
        assert_eq!(observation.observed_at, "last");
        assert_eq!(observation.tool_outputs_with_truncation_markers, 1);
        assert_eq!(find_native_log(dir.path(), "id").unwrap(), path);
        assert!(find_native_log(dir.path(), "absent").is_err());
    }
}
