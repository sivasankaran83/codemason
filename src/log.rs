//! Append-only JSONL event log. Flushed after every write; envelope fields
//! plus sizes and truncation flags only — never file contents or full tool
//! results.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::Error;

pub struct EventLog {
    writer: BufWriter<File>,
    run_id: Uuid,
    seq: u64,
}

impl EventLog {
    pub fn open(path: &Path, run_id: Uuid) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| Error::LogOpen {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|source| Error::LogOpen {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self {
            writer: BufWriter::new(file),
            run_id,
            seq: 0,
        })
    }

    /// Append one event line and flush immediately. `fields` is merged into
    /// the envelope alongside `ts`, `run_id`, `seq` and `type` — it must
    /// never carry file contents or a full tool result, only sizes,
    /// counters and flags.
    pub fn write(&mut self, event_type: &str, fields: Value) {
        self.seq += 1;
        let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

        let mut envelope = json!({
            "ts": ts,
            "run_id": self.run_id.to_string(),
            "seq": self.seq,
            "type": event_type,
        });
        if let (Value::Object(env_map), Value::Object(field_map)) = (&mut envelope, fields) {
            for (k, v) in field_map {
                env_map.insert(k, v);
            }
        }

        let line = envelope.to_string();
        let _ = writeln!(self.writer, "{line}");
        let _ = self.writer.flush();
    }

    pub fn run_id(&self) -> Uuid {
        self.run_id
    }
}

/// Event type name constants — the fixed set SPEC.md's T3.5 declares.
pub mod event_type {
    pub const RUN_STARTED: &str = "run_started";
    pub const INDEX_BUILT: &str = "index_built";
    pub const MODEL_GATED: &str = "model_gated";
    pub const MODEL_UNLISTED: &str = "model_unlisted";
    pub const LLM_CALL: &str = "llm_call";
    pub const TOOL_CALL: &str = "tool_call";
    pub const TOOL_RESULT: &str = "tool_result";
    pub const USAGE_MISSING: &str = "usage_missing";
    pub const BUDGET_EXCEEDED: &str = "budget_exceeded";
    pub const MAX_ITERATIONS_EXCEEDED: &str = "max_iterations_exceeded";
    pub const RUN_COMPLETED: &str = "run_completed";
    pub const RUN_FAILED: &str = "run_failed";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_parse_as_jsonl_with_contiguous_seq() {
        let dir = std::env::temp_dir().join(format!("codemason-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.jsonl");

        let run_id = Uuid::now_v7();
        let mut log = EventLog::open(&path, run_id).expect("open log");
        log.write(event_type::RUN_STARTED, json!({"repo": "x"}));
        log.write(event_type::INDEX_BUILT, json!({"total_chunks": 3}));
        log.write(event_type::RUN_COMPLETED, json!({"iterations": 1}));

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        let mut seqs = Vec::new();
        for line in &lines {
            let parsed: Value = serde_json::from_str(line).expect("valid JSON line");
            assert_eq!(parsed["run_id"], run_id.to_string());
            seqs.push(parsed["seq"].as_u64().unwrap());
        }
        assert_eq!(seqs, vec![1, 2, 3]);
    }
}
