//! A bounded symbol index and PageRank summary, not a compiler reference graph.
//! Inputs are in-memory UTF-8 source; path labels are never opened or followed.
//! Unlike `cpg::build_project`, distinct declarations with the same spelling
//! keep distinct IDs. Only bare call syntax is matched. An edge means a name
//! match, not a verified binding (imports, variable shadowing, receiver types,
//! overload selection, macros and linking are not analyzed).
//! A unique same-file candidate is preferred to a unique same-language
//! project candidate. Ambiguous names and qualified calls never cast votes.

use super::tree_sitter_engine;
use eon_primitives::{EonError, EonResult};
use std::collections::{BTreeMap, BTreeSet};

const MAX_FILES: usize = 128;
const MAX_FILE_BYTES: usize = 128 * 1024;
const MAX_PROJECT_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_SYMBOLS: usize = 4096;
pub(super) const MAX_CALLS: usize = 32_768;
const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Language {
    Rust,
    Kotlin,
    C,
    Cpp,
    Python,
    TypeScript,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceFile<'a> {
    pub path: &'a str,
    pub language: Language,
    pub source: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Class,
    Interface,
    Enum,
    Trait,
    Object,
    TypeAlias,
    Union,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol<'a> {
    pub path: &'a str,
    pub language: Language,
    pub kind: SymbolKind,
    pub name: &'a str,
    /// One-based line and zero-based UTF-8 byte offset of the name.
    pub line: usize,
    pub name_byte: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallCounts {
    /// Call sites (not distinct edges) within indexed functions.
    pub matched: usize,
    pub ambiguous: usize,
    pub unmatched: usize,
    /// Qualified, indirect, or otherwise unsupported callee expressions.
    pub unsupported: usize,
}

#[derive(Debug, PartialEq)]
pub struct RepoMap<'a> {
    /// IDs are indices into `symbols`, ordered by path and traversal order.
    pub symbols: Vec<Symbol<'a>>,
    /// Caller -> candidate top-level function; repeated calls are one edge.
    pub call_edges: BTreeSet<(usize, usize)>,
    /// Member -> lexically enclosing named type, not an inferred runtime type.
    pub member_edges: BTreeSet<(usize, usize)>,
    pub calls: CallCounts,
    /// (symbol ID, probability), descending by score, then ascending by ID.
    pub ranking: Vec<(usize, f64)>,
}

impl RepoMap<'_> {
    /// A prefix of whole ranked location lines, never source bodies. A byte
    /// cap is reproducible without assuming a tokenizer or a model context.
    pub fn summary(&self, byte_budget: usize) -> String {
        let mut out = String::new();
        let budget = byte_budget.min(1500);
        for &(id, _) in &self.ranking {
            let s = &self.symbols[id];
            let line = format!("{}:{} {:?} {}\n", s.path, s.line, s.kind, s.name);
            if line.len() > budget - out.len() {
                break;
            }
            out.push_str(&line);
        }
        out
    }
}

pub(super) fn error(reason: &'static str) -> EonError {
    EonError::OperationFailed {
        operation: "ast.repomap".into(),
        reason: reason.into(),
    }
}

#[derive(Default)]
pub(super) struct Inventory<'a> {
    pub symbols: Vec<Symbol<'a>>,
    pub members: BTreeSet<(usize, usize)>,
    /// Only top-level named functions may be targets of a bare call.
    /// The flag distinguishes a file-local `static` in C/C++.
    pub free_functions: Vec<(usize, bool)>,
    pub calls: Vec<(usize, Option<&'a str>)>,
}

fn index<K: Ord>(map: &mut BTreeMap<K, Option<usize>>, key: K, id: usize) {
    map.entry(key)
        .and_modify(|slot| *slot = None)
        .or_insert(Some(id));
}

/// Analyze up to 128 distinct path labels (256 bytes each), 128 KiB per
/// file and 2 MiB total. Invalid/recovered syntax, NULs, or exceeded work
/// limits reject the request; no partial map is silently ranked.
pub fn build<'a>(files: &[SourceFile<'a>]) -> EonResult<RepoMap<'a>> {
    if files.len() > MAX_FILES {
        return Err(error("too many source files"));
    }
    let mut sorted = BTreeMap::new();
    let mut bytes = 0;
    for file in files {
        if file.path.is_empty() || file.path.len() > 256 || file.path.chars().any(char::is_control)
        {
            return Err(error("invalid path label"));
        }
        if file.source.len() > MAX_FILE_BYTES || bytes + file.source.len() > MAX_PROJECT_BYTES {
            return Err(error("source byte limit exceeded"));
        }
        if file.source.contains('\0') {
            return Err(error("NUL in source"));
        }
        bytes += file.source.len();
        if sorted.insert(file.path, file).is_some() {
            return Err(error("duplicate path label"));
        }
    }

    let mut inventory = Inventory::default();
    for file in sorted.into_values() {
        tree_sitter_engine::collect(*file, &mut inventory)?;
    }
    let Inventory {
        symbols,
        members: member_edges,
        free_functions,
        calls: sites,
    } = inventory;
    let (mut local, mut project) = (BTreeMap::new(), BTreeMap::new());
    for (id, file_local) in free_functions {
        let s = &symbols[id];
        index(&mut local, (s.path, s.name), id);
        if !file_local {
            index(&mut project, (s.language, s.name), id);
        }
    }
    let mut call_edges = BTreeSet::new();
    let mut calls = CallCounts::default();
    for (from, target) in sites {
        let Some(name) = target else {
            calls.unsupported += 1;
            continue;
        };
        let s = &symbols[from];
        match local
            .get(&(s.path, name))
            .or_else(|| project.get(&(s.language, name)))
        {
            Some(Some(to)) => {
                calls.matched += 1;
                call_edges.insert((from, *to));
            }
            Some(None) => calls.ambiguous += 1,
            None => calls.unmatched += 1,
        }
    }
    let ranking = rank(symbols.len(), &call_edges, &member_edges);
    Ok(RepoMap {
        symbols,
        call_edges,
        member_edges,
        calls,
        ranking,
    })
}

/// Uniform teleportation and uniform redistribution of dangling mass.
/// Sorted IDs/edges fix floating-point accumulation order; a fixed iteration
/// count bounds work independently of the shape or convergence of the graph.
fn rank(
    n: usize,
    calls: &BTreeSet<(usize, usize)>,
    members: &BTreeSet<(usize, usize)>,
) -> Vec<(usize, f64)> {
    if n == 0 {
        return vec![];
    }
    let mut outgoing = vec![vec![]; n];
    for &(from, to) in calls.union(members) {
        outgoing[from].push(to);
    }
    let mut scores = vec![1.0 / n as f64; n];
    let mut next = vec![0.0; n];
    for _ in 0..ITERATIONS {
        let dangling: f64 = outgoing
            .iter()
            .zip(&scores)
            .filter(|(edges, _)| edges.is_empty())
            .map(|(_, p)| p)
            .sum();
        next.fill((1.0 - DAMPING + DAMPING * dangling) / n as f64);
        for (edges, score) in outgoing.iter().zip(&scores) {
            if !edges.is_empty() {
                let share = DAMPING * score / edges.len() as f64;
                for &to in edges {
                    next[to] += share;
                }
            }
        }
        std::mem::swap(&mut scores, &mut next);
    }
    let mut ranking: Vec<_> = scores.into_iter().enumerate().collect();
    ranking.sort_by(|(i, a), (j, b)| b.total_cmp(a).then(i.cmp(j)));
    ranking
}
