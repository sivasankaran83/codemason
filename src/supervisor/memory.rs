//! The supervisor's evolving memory — the component that turns a sequence of
//! blind retries into a loop that accumulates.
//!
//! Every `codemason` process is stateless by design: it takes a task string,
//! works, commits and exits, remembering nothing. Two consecutive fix
//! attempts against the same failing item therefore begin equally blind. In
//! the session that motivated this module a fix cycle invented a NuGet
//! package that does not exist — `Microsoft.Orleans.Persistence.PostgreSQL`,
//! where the real one is `Microsoft.Orleans.Persistence.AdoNet` — and the
//! next cycle only got it right because a human supplied the correct id. The
//! human was the memory. This module replaces the human.
//!
//! Nothing here reaches inside a job. A job's only input channel is its task
//! string, so what the memory knows reaches the next attempt through
//! [`render_brief`] and nowhere else. That is also why the brief is budgeted:
//! a task string that is mostly memory has crowded out the task.
//!
//! ## Why a trait over one file
//!
//! [`MemoryStore`] exists so a database-backed store can replace the file
//! without touching callers. [`JsonlMemory`] is the only implementation for
//! now — an append-only `.jsonl` file, one JSON object per line, flushed
//! after every write, mirroring the convention in `crate::log`. Append-only
//! matters for the same reason it does there: a supervisor that dies
//! mid-build must not take its accumulated knowledge with it, and a file that
//! is only ever appended to and flushed loses at most the line being written.
//! The reader tolerates that half-written last line rather than treating the
//! whole file as corrupt.
//!
//! Only [`MemoryStore::append`] and [`MemoryStore::all`] are required of an
//! implementation; the filtering and rendering helpers have default bodies
//! written in terms of those two, so a future store implements storage and
//! inherits the rest.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Error};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// What a fact is about. Deliberately a small closed set: these four are what
/// `ORCHESTRATION.md` says the memory must carry, and a memory that will
/// absorb anything is one nobody can render usefully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    /// One dispatch and how it ended — which cycle, the exit code, whether
    /// the acceptance command then passed.
    Attempt,
    /// Something established the hard way: a package id that turned out not
    /// to exist, a build flag that mattered, a convention the target
    /// repository enforces. These are the facts that must reach the next task
    /// text; everything else in the brief is context around them.
    Learned,
    /// A contract or API surface already extracted, so no later job spends a
    /// run re-deriving it.
    Contract,
    /// The error set from a verification. Kept per cycle rather than
    /// overwritten, because the useful question is not "what failed" but
    /// whether the set is shrinking or shifting.
    Errors,
    /// Anything worth recording that is none of the above. Rendered last and
    /// trimmed first.
    Note,
}

impl FactKind {
    /// Descending order of what survives a tight character budget.
    fn priority(self) -> u8 {
        match self {
            FactKind::Learned => 0,
            FactKind::Contract => 1,
            FactKind::Errors => 2,
            FactKind::Attempt => 3,
            FactKind::Note => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            FactKind::Learned => "known",
            FactKind::Contract => "contract",
            FactKind::Errors => "errors",
            FactKind::Attempt => "attempt",
            FactKind::Note => "note",
        }
    }
}

/// One line of memory.
///
/// Flat rather than an enum carrying per-variant payloads: the file is read
/// by anything that can parse JSON, including a human with `grep`, and the
/// envelope convention in `crate::log` is flat for the same reason. Fields
/// that do not apply to a kind are simply absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    /// RFC3339 with milliseconds, matching the event log so the two can be
    /// read side by side.
    pub ts: String,
    /// Which work item this concerns. `None` means build-wide — a fact that
    /// applies to every item, and which therefore appears in every brief.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub kind: FactKind,
    /// The fact itself, as prose the next job can act on.
    pub content: String,
    /// Which fix cycle produced this. Absent for facts that are not tied to
    /// one, such as a contract extracted during planning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle: Option<u32>,
    /// `codemason`'s exit code, on [`FactKind::Attempt`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the acceptance command passed afterwards, on
    /// [`FactKind::Attempt`]. Distinct from the exit code on purpose: exit 2
    /// and 3 commit their work, and only acceptance says whether the work is
    /// done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<bool>,
}

impl Fact {
    fn new(kind: FactKind, item_id: Option<&str>, content: impl Into<String>) -> Self {
        Self {
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            item_id: item_id.map(str::to_string),
            kind,
            content: content.into(),
            cycle: None,
            exit_code: None,
            accepted: None,
        }
    }

    /// A dispatch and its outcome.
    pub fn attempt(
        item_id: &str,
        cycle: u32,
        exit_code: i32,
        accepted: bool,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            cycle: Some(cycle),
            exit_code: Some(exit_code),
            accepted: Some(accepted),
            ..Self::new(FactKind::Attempt, Some(item_id), summary)
        }
    }

    /// Something established the hard way. `item_id` of `None` makes it
    /// build-wide, which is usually right — a package that does not exist
    /// does not exist for any item.
    pub fn learned(item_id: Option<&str>, content: impl Into<String>) -> Self {
        Self::new(FactKind::Learned, item_id, content)
    }

    /// A contract or API surface already extracted.
    pub fn contract(item_id: Option<&str>, content: impl Into<String>) -> Self {
        Self::new(FactKind::Contract, item_id, content)
    }

    /// The error set from one verification.
    pub fn errors(item_id: &str, cycle: u32, content: impl Into<String>) -> Self {
        Self {
            cycle: Some(cycle),
            ..Self::new(FactKind::Errors, Some(item_id), content)
        }
    }

    pub fn note(item_id: Option<&str>, content: impl Into<String>) -> Self {
        Self::new(FactKind::Note, item_id, content)
    }

    /// True when this fact belongs in `item_id`'s brief: either it names that
    /// item, or it is build-wide and so belongs in every brief.
    pub fn concerns(&self, item_id: &str) -> bool {
        match self.item_id.as_deref() {
            Some(id) => id == item_id,
            None => true,
        }
    }
}

/// Storage for facts. Implementations need only [`append`](Self::append) and
/// [`all`](Self::all); the rest is derived.
pub trait MemoryStore {
    /// Record one fact. It must be durable by the time this returns —
    /// a supervisor killed a moment later still has it.
    fn append(&mut self, fact: Fact) -> Result<(), Error>;

    /// Every fact, oldest first. Insertion order is part of the contract:
    /// the brief reports the most recent attempt and the last error set, and
    /// neither means anything without ordering.
    fn all(&self) -> Result<Vec<Fact>, Error>;

    /// Facts concerning one item, oldest first, including build-wide facts.
    fn for_item(&self, item_id: &str) -> Result<Vec<Fact>, Error> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|f| f.concerns(item_id))
            .collect())
    }

    /// Facts of one kind concerning one item, oldest first.
    fn of_kind(&self, item_id: &str, kind: FactKind) -> Result<Vec<Fact>, Error> {
        Ok(self
            .for_item(item_id)?
            .into_iter()
            .filter(|f| f.kind == kind)
            .collect())
    }

    /// How many fix cycles this item has already been through, taken from the
    /// attempts recorded rather than a counter held somewhere else — a
    /// counter and a log disagree eventually, and then nobody knows which is
    /// right.
    fn cycles_attempted(&self, item_id: &str) -> Result<u32, Error> {
        Ok(self.of_kind(item_id, FactKind::Attempt)?.len() as u32)
    }

    /// The text to inject into this item's next task string. Empty when
    /// there is nothing worth saying, so a caller can concatenate it
    /// unconditionally.
    fn brief_for(&self, item_id: &str, opts: BriefOptions) -> Result<String, Error> {
        Ok(render_brief(&self.for_item(item_id)?, opts))
    }
}

/// Budget for the rendered brief. Every number is a ceiling on how much of
/// the next task string memory is allowed to occupy.
#[derive(Debug, Clone, Copy)]
pub struct BriefOptions {
    /// Hard ceiling on the whole brief. Sections are dropped from the least
    /// important end until it fits.
    pub max_chars: usize,
    /// Hard-won facts shown. The most generous limit, because these are the
    /// reason the brief exists.
    pub max_learned: usize,
    pub max_contracts: usize,
    pub max_attempts: usize,
    /// Error sets shown, most recent first. Two by default: one set says what
    /// is broken, two say whether it is shrinking or shifting.
    pub max_error_sets: usize,
    /// Ceiling per rendered line, so one enormous compiler dump cannot
    /// consume the whole budget on its own.
    pub max_chars_per_fact: usize,
}

impl Default for BriefOptions {
    fn default() -> Self {
        Self {
            max_chars: 1600,
            max_learned: 8,
            max_contracts: 3,
            max_attempts: 3,
            max_error_sets: 2,
            max_chars_per_fact: 320,
        }
    }
}

/// Render facts as text for a task string.
///
/// Ordered by what a blind job most needs and trimmed from the other end:
/// hard-won facts, then contracts, then the recent error sets, then the
/// attempt history, then notes. Within each section the most recent facts
/// win, since a later cycle's finding supersedes an earlier one's.
///
/// `facts` is expected to be one item's facts in insertion order — see
/// [`MemoryStore::for_item`].
pub fn render_brief(facts: &[Fact], opts: BriefOptions) -> String {
    let mut sections: Vec<(FactKind, Vec<String>)> = Vec::new();

    for kind in [
        FactKind::Learned,
        FactKind::Contract,
        FactKind::Errors,
        FactKind::Attempt,
        FactKind::Note,
    ] {
        let cap = match kind {
            FactKind::Learned => opts.max_learned,
            FactKind::Contract => opts.max_contracts,
            FactKind::Errors => opts.max_error_sets,
            FactKind::Attempt => opts.max_attempts,
            FactKind::Note => 2,
        };
        if cap == 0 {
            continue;
        }

        // Most recent first, then back into chronological order so an
        // attempt history reads forwards.
        let mut chosen: Vec<&Fact> = facts
            .iter()
            .filter(|f| f.kind == kind)
            .rev()
            .take(cap)
            .collect();
        chosen.reverse();

        if chosen.is_empty() {
            continue;
        }
        let lines = chosen
            .into_iter()
            .map(|f| render_line(f, opts.max_chars_per_fact))
            .collect();
        sections.push((kind, lines));
    }

    if sections.is_empty() {
        return String::new();
    }

    let header = "Memory from earlier cycles (this is what previous attempts \
                  established; it is not part of the task):";
    let mut out = String::from(header);

    sections.sort_by_key(|(kind, _)| kind.priority());
    for (_, lines) in &sections {
        for line in lines {
            let candidate_len = out.len() + 1 + line.len();
            if candidate_len > opts.max_chars {
                // Drop this line and everything below it: sections are
                // already in priority order, so nothing further down is
                // worth more than what has been kept.
                return out;
            }
            out.push('\n');
            out.push_str(line);
        }
    }
    out
}

fn render_line(fact: &Fact, max_chars: usize) -> String {
    let mut prefix = String::from(fact.kind.label());
    if let Some(cycle) = fact.cycle {
        prefix.push_str(&format!(" cycle {cycle}"));
    }
    if let Some(code) = fact.exit_code {
        prefix.push_str(&format!(" exit {code}"));
    }
    if let Some(accepted) = fact.accepted {
        prefix.push_str(if accepted {
            ", acceptance passed"
        } else {
            ", acceptance failed"
        });
    }
    format!("- [{prefix}] {}", clip(&fact.content, max_chars))
}

fn clip(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.replace('\n', " ");
    }
    let kept: String = trimmed.chars().take(max_chars).collect();
    format!("{} […]", kept.trim_end().replace('\n', " "))
}

/// The file-backed store: one JSON object per line, appended and flushed.
pub struct JsonlMemory {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl JsonlMemory {
    /// Open or create the memory file. An existing file is appended to, never
    /// truncated — reopening a build's memory is the normal case, not the
    /// exception.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating memory directory {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening memory file {}", path.display()))?;
        let mut store = Self {
            path: path.to_path_buf(),
            writer: BufWriter::new(file),
        };
        store.terminate_torn_line()?;
        Ok(store)
    }

    /// If the file does not end in a newline, write one before anything else.
    ///
    /// A supervisor killed mid-write leaves a half-written last line with no
    /// terminator. Appending onto it would splice the next fact into the
    /// wreckage and lose them both; closing the line off costs one byte and
    /// confines the damage to the line that was already lost.
    fn terminate_torn_line(&mut self) -> Result<(), Error> {
        let len = fs::metadata(&self.path)
            .with_context(|| format!("stat memory file {}", self.path.display()))?
            .len();
        if len == 0 {
            return Ok(());
        }

        let mut reader = File::open(&self.path)
            .with_context(|| format!("reading memory file {}", self.path.display()))?;
        reader
            .seek(std::io::SeekFrom::Start(len - 1))
            .context("seeking to the last byte of the memory file")?;
        let mut last = [0u8; 1];
        reader
            .read_exact(&mut last)
            .context("reading the last byte of the memory file")?;
        if last[0] != b'\n' {
            self.writer
                .write_all(b"\n")
                .context("terminating a torn memory line")?;
            self.writer.flush().context("flushing memory file")?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Facts in the file, plus the number of lines that could not be parsed.
    ///
    /// A line is skipped rather than fatal. The realistic corruption is a
    /// half-written final line from a supervisor that was killed mid-write,
    /// and losing every earlier fact because of it would defeat the point of
    /// keeping the memory on disk at all.
    pub fn read_with_skipped(path: &Path) -> Result<(Vec<Fact>, usize), Error> {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            // A memory that has never been written to is empty, not broken.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => {
                return Err(Error::new(e))
                    .with_context(|| format!("reading memory file {}", path.display()));
            }
        };

        let mut facts = Vec::new();
        let mut skipped = 0usize;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Fact>(line) {
                Ok(fact) => facts.push(fact),
                Err(_) => skipped += 1,
            }
        }
        Ok((facts, skipped))
    }
}

impl MemoryStore for JsonlMemory {
    fn append(&mut self, fact: Fact) -> Result<(), Error> {
        let line = serde_json::to_string(&fact).context("serialising fact")?;
        writeln!(self.writer, "{line}")
            .with_context(|| format!("writing to memory file {}", self.path.display()))?;
        // Flushed per write for the same reason the event log is: an
        // unflushed fact is a fact the next cycle does not have.
        self.writer
            .flush()
            .with_context(|| format!("flushing memory file {}", self.path.display()))?;
        Ok(())
    }

    fn all(&self) -> Result<Vec<Fact>, Error> {
        Ok(Self::read_with_skipped(&self.path)?.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique path per test — there is no `tempfile` dependency and adding
    /// one is not on offer.
    fn temp_path(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "codemason-memory-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir.join("memory.jsonl")
    }

    #[test]
    fn a_fact_survives_a_round_trip_through_a_real_file() {
        let path = temp_path("roundtrip");
        let mut mem = JsonlMemory::open(&path).expect("open");
        mem.append(Fact::learned(
            None,
            "Microsoft.Orleans.Persistence.PostgreSQL does not exist; use \
             Microsoft.Orleans.Persistence.AdoNet",
        ))
        .expect("append");

        let reopened = JsonlMemory::open(&path).expect("reopen");
        let facts = reopened.all().expect("all");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].kind, FactKind::Learned);
        assert!(facts[0].content.contains("AdoNet"));
        assert!(facts[0].item_id.is_none());
        // The timestamp format is the event log's, so the two read together.
        assert!(facts[0].ts.contains('T') && facts[0].ts.ends_with('Z'));
    }

    #[test]
    fn appending_preserves_order_and_never_rewrites_earlier_lines() {
        let path = temp_path("order");
        let mut mem = JsonlMemory::open(&path).expect("open");
        for i in 0..5u32 {
            mem.append(Fact::note(Some("item-1"), format!("note {i}")))
                .expect("append");
        }
        // A second handle on the same file must extend it, not replace it.
        let mut again = JsonlMemory::open(&path).expect("reopen");
        again
            .append(Fact::note(Some("item-1"), "note 5"))
            .expect("append");

        let contents: Vec<String> = again
            .all()
            .expect("all")
            .into_iter()
            .map(|f| f.content)
            .collect();
        assert_eq!(
            contents,
            vec!["note 0", "note 1", "note 2", "note 3", "note 4", "note 5"]
        );
    }

    #[test]
    fn facts_filter_by_item_and_build_wide_facts_reach_every_item() {
        let path = temp_path("filter");
        let mut mem = JsonlMemory::open(&path).expect("open");
        mem.append(Fact::learned(None, "build-wide")).expect("a");
        mem.append(Fact::contract(Some("item-1"), "one")).expect("b");
        mem.append(Fact::contract(Some("item-2"), "two")).expect("c");

        let one: Vec<String> = mem
            .for_item("item-1")
            .expect("for_item")
            .into_iter()
            .map(|f| f.content)
            .collect();
        assert_eq!(one, vec!["build-wide", "one"]);

        let two: Vec<String> = mem
            .for_item("item-2")
            .expect("for_item")
            .into_iter()
            .map(|f| f.content)
            .collect();
        assert_eq!(two, vec!["build-wide", "two"]);

        assert_eq!(mem.of_kind("item-1", FactKind::Contract).unwrap().len(), 1);
    }

    #[test]
    fn a_half_written_trailing_line_does_not_lose_the_facts_before_it() {
        let path = temp_path("corrupt");
        let mut mem = JsonlMemory::open(&path).expect("open");
        mem.append(Fact::learned(None, "first")).expect("a");
        mem.append(Fact::learned(None, "second")).expect("b");

        // Exactly what a supervisor killed mid-write leaves behind.
        let mut raw = OpenOptions::new().append(true).open(&path).expect("append");
        write!(raw, "{{\"ts\":\"2026-08-24T00:00:00.000Z\",\"kind\":\"lear")
            .expect("partial write");
        raw.flush().expect("flush");

        let (facts, skipped) = JsonlMemory::read_with_skipped(&path).expect("read");
        assert_eq!(skipped, 1);
        let contents: Vec<&str> = facts.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["first", "second"]);

        // And the store keeps working: the next append is still readable.
        let mut mem = JsonlMemory::open(&path).expect("reopen");
        mem.append(Fact::learned(None, "third")).expect("c");
        assert_eq!(mem.all().expect("all").len(), 3);
    }

    #[test]
    fn a_memory_file_that_does_not_exist_yet_reads_as_empty() {
        let path = temp_path("missing");
        let (facts, skipped) = JsonlMemory::read_with_skipped(&path).expect("read");
        assert!(facts.is_empty());
        assert_eq!(skipped, 0);
    }

    #[test]
    fn the_brief_carries_the_hard_won_fact_into_the_next_task() {
        let path = temp_path("brief");
        let mut mem = JsonlMemory::open(&path).expect("open");
        mem.append(Fact::attempt(
            "grains",
            1,
            0,
            false,
            "added persistence provider",
        ))
        .expect("a");
        mem.append(Fact::errors(
            "grains",
            1,
            "NU1101: package Microsoft.Orleans.Persistence.PostgreSQL not found",
        ))
        .expect("b");
        mem.append(Fact::learned(
            None,
            "Microsoft.Orleans.Persistence.PostgreSQL does not exist; the ADO.NET \
             provider is Microsoft.Orleans.Persistence.AdoNet",
        ))
        .expect("c");

        let brief = mem
            .brief_for("grains", BriefOptions::default())
            .expect("brief");
        assert!(brief.contains("AdoNet"), "the hard-won fact must survive: {brief}");
        assert!(brief.contains("NU1101"), "the last error set is shown: {brief}");
        assert!(brief.contains("acceptance failed"), "attempt outcome: {brief}");

        // Hard-won facts come before the attempt history, because that is the
        // order in which the budget drops things.
        let learned_at = brief.find("AdoNet").unwrap();
        let attempt_at = brief.find("acceptance failed").unwrap();
        assert!(learned_at < attempt_at, "learned facts lead: {brief}");
    }

    #[test]
    fn an_empty_memory_renders_nothing_rather_than_an_empty_heading() {
        let path = temp_path("empty");
        let mem = JsonlMemory::open(&path).expect("open");
        assert_eq!(
            mem.brief_for("anything", BriefOptions::default())
                .expect("brief"),
            ""
        );
    }

    #[test]
    fn the_budget_drops_low_priority_facts_before_hard_won_ones() {
        let facts = vec![
            Fact::note(Some("i"), "a note nobody needs"),
            Fact::attempt("i", 1, 1, false, "an attempt summary"),
            Fact::learned(None, "the package id is X"),
        ];
        let opts = BriefOptions {
            max_chars: 140,
            ..Default::default()
        };
        let brief = render_brief(&facts, opts);
        assert!(brief.len() <= 140, "budget respected: {} chars", brief.len());
        assert!(brief.contains("the package id is X"), "{brief}");
        assert!(!brief.contains("a note nobody needs"), "{brief}");
    }

    #[test]
    fn one_enormous_error_dump_cannot_eat_the_whole_brief() {
        let facts = vec![Fact::errors("i", 2, "E".repeat(10_000))];
        let brief = render_brief(&facts, BriefOptions::default());
        assert!(brief.contains("[…]"), "clipped: {brief}");
        assert!(brief.len() < BriefOptions::default().max_chars);
    }

    #[test]
    fn only_the_most_recent_error_sets_are_shown_so_a_planner_sees_the_trend() {
        let facts = vec![
            Fact::errors("i", 1, "three errors"),
            Fact::errors("i", 2, "two errors"),
            Fact::errors("i", 3, "one error"),
        ];
        let brief = render_brief(&facts, BriefOptions::default());
        assert!(!brief.contains("three errors"), "oldest dropped: {brief}");
        assert!(brief.contains("two errors") && brief.contains("one error"), "{brief}");
        assert!(brief.contains("cycle 3"), "the cycle is named: {brief}");
    }

    #[test]
    fn cycles_attempted_is_counted_from_the_attempts_recorded() {
        let path = temp_path("cycles");
        let mut mem = JsonlMemory::open(&path).expect("open");
        assert_eq!(mem.cycles_attempted("i").unwrap(), 0);
        mem.append(Fact::attempt("i", 1, 0, false, "first")).unwrap();
        mem.append(Fact::attempt("i", 2, 0, true, "second")).unwrap();
        // Another item's attempts must not count towards this one.
        mem.append(Fact::attempt("j", 1, 0, true, "other")).unwrap();
        assert_eq!(mem.cycles_attempted("i").unwrap(), 2);
    }

    #[test]
    fn unknown_fields_in_a_stored_fact_are_a_hard_error_not_a_silent_drop() {
        // A fact whose kind this build does not know is skipped, not guessed
        // at: rendering a fact under the wrong heading is worse than omitting
        // it.
        let line = r#"{"ts":"2026-08-24T00:00:00.000Z","kind":"telepathy","content":"x"}"#;
        assert!(serde_json::from_str::<Fact>(line).is_err());
    }
}
