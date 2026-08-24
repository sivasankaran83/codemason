//! Stage 3, PLAN — AgentFlow's **planner** module.
//!
//! Turns a goal plus a set of partitions into work items sorted into levels.
//! This is the one stage of the loop that genuinely needs a model: everything
//! else here is arithmetic or process control, and doing those with a model
//! would make them slower, dearer and non-reproducible.
//!
//! The planner reuses this crate's existing client, gating and configuration
//! rather than reimplementing them. A supervisor that had to grow its own
//! provider handling would be a second `codemason`, which is exactly what
//! this design avoids.
//!
//! ## What the planner is really for
//!
//! A `codemason` job sees its task string and its own repository. Nothing
//! else. It cannot ask a question, cannot see a sibling job, and cannot read a
//! file another job wrote in a different worktree. So every fact the planner
//! knows and does not write into the task text is a fact the job does not
//! have — and the observed failure mode is a job that spends its whole budget
//! rediscovering what the planner already knew. One run made 31 `read_file`
//! calls, wrote nothing, and exhausted its budget; the same work, with the
//! contracts pasted into the task text, committed 14 files.

use serde::Deserialize;

use crate::llm::{ChatMessage, Client};
use crate::supervisor::{Plan, WorkItem};

/// What the model is asked to return. Kept deliberately close to `Plan` so
/// the translation is obvious, but separate so a malformed response fails at
/// the edge rather than halfway through a build.
#[derive(Debug, Deserialize)]
struct PlanResponse {
    items: Vec<PlanItem>,
    #[serde(default)]
    rationale: String,
    #[serde(default)]
    not_parallelized: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlanItem {
    id: String,
    #[serde(default)]
    partition_id: Option<String>,
    level: u32,
    #[serde(default)]
    repo: Option<String>,
    task: String,
    #[serde(default)]
    acceptance: Option<String>,
}

/// Everything the planner needs to produce a plan.
pub struct PlanRequest<'a> {
    pub goal: &'a str,
    pub repo: &'a str,
    /// `codemason index --partition --json`, verbatim. Passed as text because
    /// the planner reasons over it rather than computing on it.
    pub partitions_json: &'a str,
    /// Facts carried forward from earlier cycles — the evolving memory. Empty
    /// on the first pass through the loop.
    pub memory: &'a str,
    /// Contract surfaces already extracted, to be pasted into task text so no
    /// job pays to rediscover them.
    pub contracts: &'a str,
    pub model: &'a str,
}

const SYSTEM: &str = "\
You plan work for independent coding agents that run concurrently and cannot \
communicate. Return ONLY a JSON object, no prose and no code fences.

Shape:
{\"items\":[{\"id\":\"...\",\"partition_id\":\"p0\",\"level\":1,\"repo\":\"...\",\
\"task\":\"...\",\"acceptance\":\"...\"}],\"rationale\":\"...\",\"not_parallelized\":\"...\"}

Rules, in order of importance:

1. One item maps to exactly ONE partition. Two items in the same level must \
never share a partition_id — they would edit the same files concurrently, \
which is the failure partitioning exists to prevent.

2. Each item's `task` must be COMPLETELY self-contained. The job sees only \
that string and its own repository. Write out verbatim every interface, \
signature, constant and convention it must match. Never write \"match the \
interface in X\" — the job cannot see X.

3. Name external dependencies exactly: the published package id and a real \
version. A previous run invented a package that does not exist and two fix \
cycles were spent discovering it. Instruct the job that a package which fails \
to restore must be removed and reported, not swapped for another guess.

4. Levels encode dependencies. If item B needs what item A defines, B goes in \
a later level. Contracts complete before implementations start; two jobs \
inventing the same interface in parallel will not agree, and nothing catches \
it until integration.

5. Size each item to finish in roughly 5-10 tool-using iterations. Too many \
items costs coordination; too few costs tokens quadratically, because \
conversation history is re-sent on every call.

6. Tell each job to write code early and to read at most two or three named \
files. The dominant observed failure is a job that reads exhaustively and \
never writes.

7. `acceptance` is the command that proves the item worked — a build or test \
command. It, not the job's own summary, decides whether the work is done.

8. If the goal touches one cohesive area, emit a SINGLE item and say why in \
not_parallelized. That is a correct plan. Do not manufacture parallelism: \
naive splitting measures worse than one sequential agent.";

fn user_prompt(req: &PlanRequest<'_>) -> String {
    let mut s = format!(
        "Goal:\n{}\n\nRepository: {}\n\nPartitions (file ownership is disjoint between them):\n{}\n",
        req.goal, req.repo, req.partitions_json
    );
    if !req.contracts.trim().is_empty() {
        s.push_str(&format!(
            "\nExisting contract surface. Paste whatever an item needs into that item's task \
             text verbatim, so no job has to rediscover it:\n{}\n",
            req.contracts
        ));
    }
    if !req.memory.trim().is_empty() {
        // Deliberately last and clearly labelled: this is what earlier cycles
        // learned, and it is the difference between a loop that accumulates
        // and a sequence of blind retries.
        s.push_str(&format!(
            "\nWhat earlier cycles established. Treat these as known facts and carry the \
             relevant ones into the task text of the items they affect:\n{}\n",
            req.memory
        ));
    }
    s
}

/// Strip a fenced code block if the model wrapped its JSON in one.
///
/// Asking for bare JSON mostly works; models still fence it often enough that
/// failing the whole build over three backticks would be a poor trade.
fn unfence(raw: &str) -> &str {
    let t = raw.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start_matches('\n')
        .rsplit_once("```")
        .map(|(body, _)| body.trim())
        .unwrap_or(rest.trim())
}

/// Parse a planner response into a `Plan`, rejecting a plan that would put
/// two concurrent items on the same partition.
pub fn parse_plan(raw: &str, default_repo: &str) -> Result<Plan, anyhow::Error> {
    let body = unfence(raw);
    let response: PlanResponse = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("planner did not return usable JSON: {e}"))?;

    if response.items.is_empty() {
        anyhow::bail!("planner returned no work items");
    }

    let plan = Plan {
        items: response
            .items
            .into_iter()
            .map(|i| WorkItem {
                id: i.id,
                partition_id: i.partition_id,
                level: i.level.max(1),
                repo: i.repo.unwrap_or_else(|| default_repo.to_string()),
                task: i.task,
                acceptance: i.acceptance,
            })
            .collect(),
        rationale: response.rationale,
        not_parallelized: response.not_parallelized,
    };

    // Refuse rather than dispatch. Two concurrent items on one partition is
    // the precise failure partitioning exists to prevent, and it is far
    // cheaper to reject the plan than to discover it after N runs have
    // trampled each other.
    let clashes = plan.conflicting_partitions();
    if !clashes.is_empty() {
        anyhow::bail!(
            "plan puts two concurrent items on the same partition ({}); \
             split the item or merge those partitions",
            clashes.join(", ")
        );
    }

    Ok(plan)
}

/// Ask the model for a plan. The planner uses no tools — it is given
/// everything it needs and returns one object.
pub fn plan(client: &Client, req: &PlanRequest<'_>) -> Result<Plan, anyhow::Error> {
    let messages = vec![
        ChatMessage::system(SYSTEM),
        ChatMessage::user(user_prompt(req)),
    ];

    let completion = client
        .complete(req.model, &messages, &[])
        .map_err(|e| anyhow::anyhow!("planner call failed: {e}"))?;

    let raw = completion
        .message
        .content
        .as_deref()
        .unwrap_or_default()
        .to_string();

    if raw.trim().is_empty() {
        anyhow::bail!("planner returned an empty message");
    }

    parse_plan(&raw, req.repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{
      "items": [
        {"id":"a","partition_id":"p0","level":1,"task":"do a","acceptance":"cargo test"},
        {"id":"b","partition_id":"p1","level":1,"task":"do b"}
      ],
      "rationale": "two independent partitions"
    }"#;

    #[test]
    fn a_well_formed_plan_parses_and_defaults_the_repo() {
        let plan = parse_plan(GOOD, "/repo").expect("parses");
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.items[0].repo, "/repo");
        assert_eq!(plan.items[0].acceptance.as_deref(), Some("cargo test"));
        assert_eq!(plan.levels(), vec![1]);
    }

    #[test]
    fn a_fenced_response_is_still_accepted() {
        let fenced = format!("```json\n{GOOD}\n```");
        assert!(parse_plan(&fenced, "/repo").is_ok());
    }

    #[test]
    fn two_concurrent_items_on_one_partition_are_refused_before_dispatch() {
        let bad = r#"{"items":[
            {"id":"a","partition_id":"p0","level":1,"task":"a"},
            {"id":"b","partition_id":"p0","level":1,"task":"b"}]}"#;
        let err = parse_plan(bad, "/repo").expect_err("must refuse");
        assert!(err.to_string().contains("p0"), "names the partition: {err}");
    }

    #[test]
    fn the_same_partition_in_different_levels_is_allowed() {
        let ok = r#"{"items":[
            {"id":"a","partition_id":"p0","level":1,"task":"a"},
            {"id":"b","partition_id":"p0","level":2,"task":"b"}]}"#;
        assert!(parse_plan(ok, "/repo").is_ok(), "sequential access is safe");
    }

    #[test]
    fn an_empty_item_list_is_an_error_not_an_empty_build() {
        let err = parse_plan(r#"{"items":[]}"#, "/repo").expect_err("must fail");
        assert!(err.to_string().contains("no work items"), "{err}");
    }

    #[test]
    fn unusable_output_names_the_problem_rather_than_panicking() {
        let err = parse_plan("I think we should start by...", "/repo").expect_err("must fail");
        assert!(err.to_string().contains("usable JSON"), "{err}");
    }

    #[test]
    fn memory_and_contracts_reach_the_prompt_when_present() {
        let req = PlanRequest {
            goal: "g",
            repo: "/r",
            partitions_json: "{}",
            memory: "package Foo does not exist; use Bar",
            contracts: "pub trait Thing {}",
            model: "m",
        };
        let p = user_prompt(&req);
        assert!(p.contains("package Foo does not exist"), "memory must be carried");
        assert!(p.contains("pub trait Thing"), "contracts must be carried");
    }

    #[test]
    fn an_empty_memory_adds_no_section() {
        let req = PlanRequest {
            goal: "g",
            repo: "/r",
            partitions_json: "{}",
            memory: "   ",
            contracts: "",
            model: "m",
        };
        let p = user_prompt(&req);
        assert!(!p.contains("earlier cycles established"));
    }

    #[test]
    fn a_level_of_zero_is_normalised_to_one() {
        // Levels are 1-based everywhere else; a model that emits 0 should not
        // silently create a phantom level before the first.
        let z = r#"{"items":[{"id":"a","level":0,"task":"a"}]}"#;
        let plan = parse_plan(z, "/repo").expect("parses");
        assert_eq!(plan.items[0].level, 1);
    }
}
