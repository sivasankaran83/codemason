//! Cohesion-aware partitioning of the dependency graph — stage 2 of
//! `ORCHESTRATION.md`.
//!
//! Produces disjoint groups of files so that two concurrent `codemason` jobs
//! can never be assigned the same file. Deliberately arithmetic and not a
//! model's judgement: partitioning must be deterministic and reproducible,
//! and a model would be both worse at it and unable to answer the same way
//! twice.
//!
//! The algorithm is the MVP subset of Co-Coder's: isolate high in-degree
//! files as single-file partitions, then group the remainder by connected
//! components. Community detection is deliberately left out until
//! measurement says these partitions are too coarse.
//!
//! This lives in the binary rather than in a sidecar script because the
//! deploy target is a container carrying `git` and nothing else — a script
//! in another language would not run there, and "single self-contained
//! binary" is a constraint this project actually holds itself to.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Serialize;

use crate::engine::DependencyGraph;
use crate::text::normalize_slashes;

/// A file every partitioner consumer needs to treat as exclusive: enough
/// other files depend on it that two concurrent jobs editing it is the
/// expected outcome rather than bad luck.
const DEFAULT_HUB_RATIO: f64 = 0.10;
const DEFAULT_MIN_HUB_DEPENDENTS: usize = 3;

#[derive(Debug, Clone, Copy)]
pub struct PartitionOptions {
    /// Fraction of the repository that must depend on a file before it is
    /// treated as a hub.
    pub hub_ratio: f64,
    /// Floor on absolute dependents, so a tiny repository does not declare
    /// every file a hub.
    pub min_hub_dependents: usize,
}

impl Default for PartitionOptions {
    fn default() -> Self {
        Self {
            hub_ratio: DEFAULT_HUB_RATIO,
            min_hub_dependents: DEFAULT_MIN_HUB_DEPENDENTS,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Partition {
    pub id: String,
    /// `"component"` — a cohesive group safe to hand to one job.
    /// `"hub"` — a single high in-degree file, exclusive to one job.
    pub kind: &'static str,
    pub files: Vec<String>,
    pub file_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependent_count: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct PartitionStats {
    pub files: usize,
    pub edges: usize,
    pub partitions: usize,
    pub component_partitions: usize,
    pub hub_partitions: usize,
    pub largest_partition: usize,
    /// How many jobs can actually run at once — the only number that decides
    /// whether parallelising is worth anything.
    pub usable_parallelism: usize,
    /// True when the repository is too densely coupled to split. A correct
    /// outcome, not a failure: naive parallelism on coupled code measures
    /// worse than running sequentially.
    pub degrades_to_sequential: bool,
}

#[derive(Debug, Serialize)]
pub struct PartitionResult {
    pub partitions: Vec<Partition>,
    pub hubs: Vec<String>,
    pub stats: PartitionStats,
}

pub fn partition(graph: &DependencyGraph, opts: PartitionOptions) -> PartitionResult {
    let names: Vec<String> = {
        let mut n: Vec<String> = graph.all_files().into_iter().collect();
        n.sort();
        n
    };
    let total = names.len();

    if total == 0 {
        return PartitionResult {
            partitions: Vec::new(),
            hubs: Vec::new(),
            stats: PartitionStats {
                files: 0,
                edges: 0,
                partitions: 0,
                component_partitions: 0,
                hub_partitions: 0,
                largest_partition: 0,
                usable_parallelism: 0,
                degrades_to_sequential: true,
            },
        };
    }

    let known: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    let denom = (total.saturating_sub(1)).max(1) as f64;

    let dependents_of: HashMap<&str, usize> = names
        .iter()
        .map(|n| (n.as_str(), graph.dependents(n).len()))
        .collect();

    // 1. Hubs. These are where concurrent edits collide, so they are never
    //    co-scheduled with anything else.
    let hubs: Vec<String> = names
        .iter()
        .filter(|n| {
            let d = dependents_of.get(n.as_str()).copied().unwrap_or(0);
            d >= opts.min_hub_dependents && (d as f64 / denom) > opts.hub_ratio
        })
        .cloned()
        .collect();
    let hub_set: HashSet<&str> = hubs.iter().map(|s| s.as_str()).collect();

    // 2. Undirected adjacency over non-hub files. Edges *through* a hub are
    //    dropped on purpose: two files that are related only because both
    //    import a shared types module are not coupled to each other, and
    //    treating them as coupled collapses the whole repository into one
    //    partition.
    let mut adjacency: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for name in &names {
        let n = name.as_str();
        if hub_set.contains(n) {
            continue;
        }
        let Some(node) = graph.deps(name) else { continue };
        for dep in &node.depends_on {
            let d = dep.as_str();
            if d == n || hub_set.contains(d) || !known.contains(d) {
                continue;
            }
            adjacency.entry(n).or_default().insert(d);
            adjacency.entry(d).or_default().insert(n);
        }
    }

    // 3. Connected components over what remains.
    let mut unseen: BTreeSet<&str> = names
        .iter()
        .map(|s| s.as_str())
        .filter(|n| !hub_set.contains(n))
        .collect();
    let mut groups: Vec<Vec<String>> = Vec::new();

    while let Some(&start) = unseen.iter().next() {
        unseen.remove(start);
        let mut stack = vec![start];
        let mut component = vec![start];
        while let Some(node) = stack.pop() {
            if let Some(neighbours) = adjacency.get(node) {
                for &nb in neighbours {
                    if unseen.remove(nb) {
                        component.push(nb);
                        stack.push(nb);
                    }
                }
            }
        }
        let mut files: Vec<String> = component.into_iter().map(normalize_slashes).collect();
        files.sort();
        groups.push(files);
    }

    // Largest first, so the caller reads the coupling that matters at the top.
    groups.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));

    let mut partitions: Vec<Partition> = groups
        .into_iter()
        .enumerate()
        .map(|(i, files)| Partition {
            id: format!("p{i}"),
            kind: "component",
            file_count: files.len(),
            files,
            dependent_count: None,
        })
        .collect();

    let component_partitions = partitions.len();

    let mut hubs_ranked = hubs.clone();
    hubs_ranked.sort_by_key(|h| std::cmp::Reverse(dependents_of.get(h.as_str()).copied().unwrap_or(0)));
    for (i, hub) in hubs_ranked.iter().enumerate() {
        partitions.push(Partition {
            id: format!("h{i}"),
            kind: "hub",
            files: vec![normalize_slashes(hub)],
            file_count: 1,
            dependent_count: Some(dependents_of.get(hub.as_str()).copied().unwrap_or(0)),
        });
    }

    let largest_partition = partitions.iter().map(|p| p.file_count).max().unwrap_or(0);

    PartitionResult {
        stats: PartitionStats {
            files: total,
            edges: graph.edge_count(),
            partitions: partitions.len(),
            component_partitions,
            hub_partitions: hubs_ranked.len(),
            largest_partition,
            usable_parallelism: component_partitions,
            degrades_to_sequential: component_partitions <= 1,
        },
        hubs: hubs_ranked.into_iter().map(|h| normalize_slashes(&h)).collect(),
        partitions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Index;

    fn graph_from(files: &[(&str, &str)]) -> (tempdir::Dir, Index) {
        let dir = tempdir::Dir::new();
        for (name, body) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
        let index = Index::build(dir.path()).expect("index builds");
        (dir, index)
    }

    /// Minimal temp-dir helper; the crate has no dev-dependency on tempfile
    /// and the pinned dependency set is deliberately closed.
    mod tempdir {
        use std::path::{Path, PathBuf};
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p = std::env::temp_dir()
                    .join(format!("codemason-part-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn unrelated_files_partition_separately() {
        let (_d, index) = graph_from(&[
            ("a.rs", "pub fn a() {}\n"),
            ("b.rs", "pub fn b() {}\n"),
            ("c.rs", "pub fn c() {}\n"),
        ]);
        let r = partition(index.graph(), PartitionOptions::default());
        assert_eq!(r.stats.files, 3);
        assert_eq!(r.stats.component_partitions, 3, "no edges means no coupling");
        assert!(!r.stats.degrades_to_sequential);
    }

    /// An empty graph must report "nothing to parallelise" rather than
    /// panicking on the empty-set arithmetic. Constructed directly:
    /// `Index::build` refuses a directory containing no supported source
    /// files, so this state is unreachable through it.
    #[test]
    fn an_empty_graph_degrades_to_sequential_rather_than_panicking() {
        let empty = DependencyGraph::new();
        let r = partition(&empty, PartitionOptions::default());
        assert!(r.stats.degrades_to_sequential);
        assert_eq!(r.stats.usable_parallelism, 0);
        assert!(r.partitions.is_empty());
    }

    /// A single cohesive cluster is one partition, which is the
    /// degrade-to-sequential signal the orchestrator stops on.
    #[test]
    fn one_cohesive_cluster_degrades_to_sequential() {
        let (_d, index) = graph_from(&[
            ("a.rs", "mod b;\npub fn a() { b::b(); }\n"),
            ("b.rs", "pub fn b() {}\n"),
        ]);
        let r = partition(
            index.graph(),
            // Thresholds that cannot fire, so nothing is split off as a hub
            // and the coupling itself decides the outcome.
            PartitionOptions {
                hub_ratio: 1.0,
                min_hub_dependents: 9999,
            },
        );
        assert!(
            r.stats.component_partitions <= 1,
            "coupled files must not be split across partitions, got {:?}",
            r.stats
        );
    }

    #[test]
    fn every_file_lands_in_exactly_one_partition() {
        let (_d, index) = graph_from(&[
            ("a.rs", "pub fn a() {}\n"),
            ("b.rs", "pub fn b() {}\n"),
            ("nested/c.rs", "pub fn c() {}\n"),
        ]);
        let r = partition(index.graph(), PartitionOptions::default());

        let mut seen: Vec<String> = r
            .partitions
            .iter()
            .flat_map(|p| p.files.iter().cloned())
            .collect();
        let before = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(before, seen.len(), "a file appeared in two partitions");
        assert_eq!(seen.len(), r.stats.files, "a file was dropped entirely");
    }

    #[test]
    fn hub_thresholds_are_honoured() {
        let (_d, index) = graph_from(&[
            ("a.rs", "pub fn a() {}\n"),
            ("b.rs", "pub fn b() {}\n"),
        ]);
        // A threshold nothing can satisfy must produce no hubs at all,
        // rather than silently falling back to some default.
        let r = partition(
            index.graph(),
            PartitionOptions {
                hub_ratio: 1.0,
                min_hub_dependents: 9999,
            },
        );
        assert!(r.hubs.is_empty());
        assert_eq!(r.stats.hub_partitions, 0);
    }
}
