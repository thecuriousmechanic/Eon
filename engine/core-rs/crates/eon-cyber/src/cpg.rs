//! Polyglot code graph from tree-sitter syntax trees (docs/implementation/03, code property graph).
//!
//! Functions are the nodes. An edge goes from a function to every function it calls, when the callee name matches a
//! function defined in the same file. Calls to names defined elsewhere are kept as unresolved, so the caller sees
//! them instead of losing them. Only Rust and Python are supported for now, and each language's call and definition
//! node kinds are listed explicitly, so an unsupported construct is never silently treated as a call.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use eon_primitives::{EonError, EonResult};
use tree_sitter::{Language, Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
}

impl Lang {
    fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
        }
    }

    fn language(self) -> Language {
        match self {
            Lang::Rust => Language::new(tree_sitter_rust::LANGUAGE),
            Lang::Python => Language::new(tree_sitter_python::LANGUAGE),
        }
    }

    /// Node kind for a function definition. Both grammars name its identifier field `name`.
    fn function_kind(self) -> &'static str {
        match self {
            Lang::Rust => "function_item",
            Lang::Python => "function_definition",
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CodeGraph {
    /// Function names defined in the file, in source order.
    pub functions: Vec<String>,
    /// Resolved edges: caller name to callee name, both defined in this file.
    pub edges: BTreeSet<(String, String)>,
    /// Calls whose callee is not defined in this file, keyed by caller name.
    pub unresolved: BTreeMap<String, BTreeSet<String>>,
}

impl CodeGraph {
    /// True when `to` can be reached from `from` through resolved edges.
    pub fn reaches(&self, from: &str, to: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([from.to_owned()]);
        while let Some(current) = queue.pop_front() {
            if current == to {
                return true;
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            for (caller, callee) in &self.edges {
                if *caller == current {
                    queue.push_back(callee.clone());
                }
            }
        }
        false
    }
}

fn parse_error(reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "cpg.parse".into(),
        reason: reason.to_string(),
    }
}

/// Last identifier of a callee expression, with the module or type qualifier when the call names one. `helper()` gives
/// (`helper`, None), `store::save()` gives (`save`, `store`), `Thing::new()` gives (`new`, `Thing`), and `self.run()` gives
/// (`run`, None) because the receiver type is unknown.
fn callee_name(node: Node, source: &[u8]) -> Option<(String, Option<String>)> {
    let text = |n: Node| n.utf8_text(source).ok().map(str::to_owned);
    let last_segment = |n: Node, separator: &str| {
        text(n).map(|t| t.rsplit(separator).next().unwrap_or_default().to_owned())
    };
    match node.kind() {
        "identifier" => text(node).map(|name| (name, None)),
        "scoped_identifier" => {
            let name = node.child_by_field_name("name").and_then(text)?;
            let qualifier = node.child_by_field_name("path").and_then(|p| last_segment(p, "::"));
            Some((name, qualifier))
        }
        "field_expression" => node.child_by_field_name("field").and_then(text).map(|name| (name, None)),
        "attribute" => {
            let name = node.child_by_field_name("attribute").and_then(text)?;
            let qualifier = node
                .child_by_field_name("object")
                .filter(|o| o.kind() == "identifier")
                .and_then(text);
            Some((name, qualifier))
        }
        _ => None,
    }
}

fn call_callee(lang: Lang, call: Node) -> Option<Node> {
    match lang {
        Lang::Rust if call.kind() == "call_expression" => call.child_by_field_name("function"),
        Lang::Python if call.kind() == "call" => call.child_by_field_name("function"),
        _ => None,
    }
}

struct Walk<'a> {
    lang: Lang,
    source: &'a [u8],
    calls: Vec<RawCall>,
    defined: Vec<String>,
}

impl Walk<'_> {
    fn visit(&mut self, node: Node, current: Option<&str>) {
        let definition = (node.kind() == self.lang.function_kind())
            .then(|| node.child_by_field_name("name").and_then(|n| n.utf8_text(self.source).ok()).map(str::to_owned))
            .flatten();
        if let Some(name) = definition {
            self.defined.push(name.clone());
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                self.visit(child, Some(&name));
            }
            return;
        }
        if let (Some(caller), Some((callee, qualifier))) =
            (current, call_callee(self.lang, node).and_then(|c| callee_name(c, self.source)))
        {
            self.calls.push((caller.to_owned(), callee, qualifier));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child, current);
        }
    }
}

/// Function definitions and calls of one file, in source order. Each call carries the qualifier it was written with, if any.
/// A call as written: caller, callee name, and the module or type qualifier if one was written.
type RawCall = (String, String, Option<String>);

/// A Rust `use` import: the name it binds locally, and the module and original name it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Import {
    local: String,
    module: String,
    name: String,
}

/// The import a `use` declaration makes, if it is a plain path or an `as` alias of one. Lists and globs are skipped.
fn import_of(argument: Node, source: &[u8]) -> Option<Import> {
    let text = |n: Node| n.utf8_text(source).ok().map(str::to_owned);
    let last = |n: Node| text(n).map(|t| t.rsplit("::").next().unwrap_or_default().to_owned());
    match argument.kind() {
        "scoped_identifier" => {
            let module = last(argument.child_by_field_name("path")?)?;
            let name = argument.child_by_field_name("name").and_then(text)?;
            Some(Import { local: name.clone(), module, name })
        }
        "use_as_clause" => {
            let path = argument.child_by_field_name("path")?;
            if path.kind() != "scoped_identifier" {
                return None;
            }
            let module = last(path.child_by_field_name("path")?)?;
            let name = path.child_by_field_name("name").and_then(text)?;
            let local = argument.child_by_field_name("alias").and_then(text)?;
            Some(Import { local, module, name })
        }
        _ => None,
    }
}

/// The imports in a grouped `use module::{a, b as c};`. Items that are nested paths or globs are skipped.
fn grouped_imports(argument: Node, source: &[u8]) -> Vec<Import> {
    let text = |n: Node| n.utf8_text(source).ok().map(str::to_owned);
    let module = argument
        .child_by_field_name("path")
        .and_then(text)
        .and_then(|t| t.rsplit("::").next().map(str::to_owned));
    let (Some(module), Some(list)) = (module, argument.child_by_field_name("list")) else {
        return Vec::new();
    };
    let mut imports = Vec::new();
    let mut cursor = list.walk();
    for item in list.named_children(&mut cursor) {
        let import = match item.kind() {
            "identifier" => text(item).map(|name| Import {
                local: name.clone(),
                module: module.clone(),
                name,
            }),
            "use_as_clause" => match (
                item.child_by_field_name("path").and_then(text),
                item.child_by_field_name("alias").and_then(text),
            ) {
                (Some(name), Some(local)) => Some(Import {
                    local,
                    module: module.clone(),
                    name,
                }),
                _ => None,
            },
            _ => None,
        };
        imports.extend(import);
    }
    imports
}

fn collect_imports(node: Node, source: &[u8], imports: &mut Vec<Import>) {
    if node.kind() == "use_declaration" {
        if let Some(argument) = node.child_by_field_name("argument") {
            if argument.kind() == "scoped_use_list" {
                imports.extend(grouped_imports(argument, source));
            } else if let Some(import) = import_of(argument, source) {
                imports.push(import);
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_imports(child, source, imports);
    }
}

/// Definitions, calls, and (for Rust) `use` imports of one file.
struct Analysis {
    defined: Vec<String>,
    calls: Vec<RawCall>,
    imports: Vec<Import>,
}

pub(crate) fn parse(lang: Lang, source: &str) -> EonResult<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&lang.language()).map_err(parse_error)?;
    parser.parse(source, None).ok_or_else(|| parse_error("parser produced no tree"))
}

fn analyze(lang: Lang, source: &str) -> EonResult<Analysis> {
    let tree = parse(lang, source)?;
    let mut walk = Walk {
        lang,
        source: source.as_bytes(),
        calls: Vec::new(),
        defined: Vec::new(),
    };
    walk.visit(tree.root_node(), None);
    let mut imports = Vec::new();
    if lang == Lang::Rust {
        collect_imports(tree.root_node(), source.as_bytes(), &mut imports);
    }
    Ok(Analysis {
        defined: walk.defined,
        calls: walk.calls,
        imports,
    })
}

/// Builds the code graph for one source file.
pub fn build(lang: Lang, source: &str) -> EonResult<CodeGraph> {
    let Analysis {
        defined: defined_names,
        calls,
        ..
    } = analyze(lang, source)?;
    let defined: BTreeSet<&str> = defined_names.iter().map(String::as_str).collect();
    let mut graph = CodeGraph {
        functions: defined_names.clone(),
        ..CodeGraph::default()
    };
    for (caller, callee, _qualifier) in &calls {
        if defined.contains(callee.as_str()) {
            graph.edges.insert((caller.clone(), callee.clone()));
        } else {
            graph.unresolved.entry(caller.clone()).or_default().insert(callee.clone());
        }
    }
    Ok(graph)
}

/// One function definition with its source text, for structural diffs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionBlock {
    pub name: String,
    /// One-based line where the definition starts.
    pub start_line: usize,
    /// Byte offsets of the definition in the source it was parsed from.
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
}

/// Lists every function definition in `source` in source order. Nested functions are included.
pub fn function_blocks(lang: Lang, source: &str) -> EonResult<Vec<FunctionBlock>> {
    let tree = parse(lang, source)?;
    let mut blocks = Vec::new();
    collect_blocks(lang, tree.root_node(), source, &mut blocks);
    Ok(blocks)
}

/// Exact, unique top-level function in a syntactically complete source. Unlike the
/// inspection API this never selects a method, a nested function or a recovered tree.
/// In Python, uniqueness is checked among function and class declarations directly in the module,
/// including decorated ones. Only undecorated functions are editable; assignments are not analyzed.
pub fn checked_top_level_function(lang: Lang, source: &str, symbol: &str) -> EonResult<FunctionBlock> {
    let tree = parse(lang, source)?;
    let root = tree.root_node();
    if root.has_error() {
        return Err(parse_error("source contains syntax errors"));
    }
    let mut cursor = root.walk();
    let mut found = root.named_children(&mut cursor).filter(|node| {
        let definition = match (lang, node.kind()) {
            (Lang::Python, "decorated_definition") => node.child_by_field_name("definition"),
            (Lang::Python, "class_definition") => Some(*node),
            (_, kind) if kind == lang.function_kind() => Some(*node),
            _ => None,
        };
        definition
            .and_then(|def| def.child_by_field_name("name"))
            .is_some_and(|name| &source[name.byte_range()] == symbol)
    });
    let first = found.next();
    if found.next().is_some() {
        return Err(parse_error("ambiguous top-level function"));
    }
    let node = first
        .filter(|node| node.kind() == lang.function_kind())
        .ok_or_else(|| parse_error("top-level function not found"))?;
    Ok(FunctionBlock {
        name: symbol.into(),
        start_line: node.start_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        text: source[node.byte_range()].into(),
    })
}

fn collect_blocks(lang: Lang, node: Node, source: &str, blocks: &mut Vec<FunctionBlock>) {
    if node.kind() == lang.function_kind()
        && let Some(name) = node.child_by_field_name("name")
    {
        blocks.push(FunctionBlock {
            name: source[name.byte_range()].to_owned(),
            start_line: node.start_position().row + 1,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            text: source[node.byte_range()].to_owned(),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_blocks(lang, child, source, blocks);
    }
}

/// One source file of a project, identified by its path.
#[derive(Debug, Clone, Copy)]
pub struct SourceFile<'a> {
    pub path: &'a str,
    pub lang: Lang,
    pub source: &'a str,
}

/// Code graph across several files. Nodes are `path::function`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProjectGraph {
    pub functions: Vec<String>,
    /// Resolved edges, same-file or cross-file.
    pub edges: BTreeSet<(String, String)>,
    /// Calls with no definition anywhere in the project, keyed by qualified caller.
    pub unresolved: BTreeMap<String, BTreeSet<String>>,
    /// Calls whose name has more than one definition in the project. They are not resolved, because the right callee is
    /// unknown.
    pub ambiguous: BTreeMap<String, BTreeSet<String>>,
}

impl ProjectGraph {
    /// True when `to` can be reached from `from` through resolved edges.
    pub fn reaches(&self, from: &str, to: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([from.to_owned()]);
        while let Some(current) = queue.pop_front() {
            if current == to {
                return true;
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            for (caller, callee) in &self.edges {
                if *caller == current {
                    queue.push_back(callee.clone());
                }
            }
        }
        false
    }
}

/// The module a file defines: its stem, or its directory name for `mod.rs`, `lib.rs`, and `main.rs`.
fn module_name(path: &str) -> &str {
    let file = std::path::Path::new(path);
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    match stem {
        "mod" | "lib" | "main" | "__init__" => file.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()).unwrap_or(stem),
        _ => stem,
    }
}

/// Builds one code graph for several files. A call resolves into another file only when the project has exactly one
/// matching definition, in the same language. A qualified call (`store::save`) matches the module the qualifier names;
/// an unqualified call prefers a definition in its own file. Calls that match several definitions are reported as
/// ambiguous. Calls that match none stay unresolved.
pub fn build_project(files: &[SourceFile]) -> EonResult<ProjectGraph> {
    let mut graph = ProjectGraph::default();
    let mut definitions: BTreeMap<(&'static str, String), Vec<(&str, String)>> = BTreeMap::new();
    let mut analyzed = Vec::with_capacity(files.len());
    for file in files {
        let Analysis {
            defined,
            calls,
            imports,
        } = analyze(file.lang, file.source)?;
        for name in &defined {
            let qualified = format!("{}::{name}", file.path);
            graph.functions.push(qualified.clone());
            definitions
                .entry((file.lang.name(), name.clone()))
                .or_default()
                .push((file.path, qualified));
        }
        analyzed.push((file, calls, imports));
    }

    for (file, calls, imports) in analyzed {
        for (caller, callee, qualifier) in calls {
            let caller = format!("{}::{caller}", file.path);
            // A call through a `use` import is qualified by the module it was imported from.
            let (lookup, qualifier) = match (&qualifier, imports.iter().find(|i| i.local == callee)) {
                (None, Some(import)) => (import.name.clone(), Some(import.module.clone())),
                _ => (callee.clone(), qualifier),
            };
            let candidates: &[(&str, String)] = definitions
                .get(&(file.lang.name(), lookup))
                .map_or(&[], Vec::as_slice);
            let matches: BTreeSet<&str> = match qualifier.as_deref() {
                Some(module) => {
                    let by_module: BTreeSet<&str> = candidates
                        .iter()
                        .filter(|(path, _)| module_name(path) == module)
                        .map(|(_, qualified)| qualified.as_str())
                        .collect();
                    if by_module.is_empty() {
                        candidates
                            .iter()
                            .filter(|(path, _)| *path == file.path)
                            .map(|(_, qualified)| qualified.as_str())
                            .collect()
                    } else {
                        by_module
                    }
                }
                None => {
                    let same_file: BTreeSet<&str> = candidates
                        .iter()
                        .filter(|(path, _)| *path == file.path)
                        .map(|(_, qualified)| qualified.as_str())
                        .collect();
                    if same_file.is_empty() {
                        candidates.iter().map(|(_, qualified)| qualified.as_str()).collect()
                    } else {
                        same_file
                    }
                }
            };
            match matches.len() {
                0 => {
                    graph.unresolved.entry(caller).or_default().insert(callee);
                }
                1 => {
                    let target = matches.into_iter().next().unwrap_or_default();
                    graph.edges.insert((caller, target.to_owned()));
                }
                _ => {
                    graph.ambiguous.entry(caller).or_default().insert(callee);
                }
            }
        }
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = r#"
fn handle_request(input: &str) -> String {
    let cleaned = sanitize(input);
    store(&cleaned);
    format!("{cleaned}")
}
fn sanitize(s: &str) -> String { s.trim().to_string() }
fn store(s: &str) { write_to_disk(s); }
fn unrelated() { println_helper(); }
"#;

    const PYTHON: &str = r#"
def handler(payload):
    cleaned = sanitize(payload)
    return run_query(cleaned)

def sanitize(text):
    return text.strip()

def run_query(q):
    return db.execute(q)
"#;

    #[test]
    fn rust_calls_resolve_within_the_file_and_external_calls_are_kept() {
        let graph = build(Lang::Rust, RUST).unwrap();
        assert!(graph.functions.contains(&"handle_request".to_owned()));
        assert!(graph.edges.contains(&("handle_request".into(), "sanitize".into())));
        assert!(graph.edges.contains(&("handle_request".into(), "store".into())));
        assert!(graph.unresolved["store"].contains("write_to_disk"));
        assert!(graph.unresolved["unrelated"].contains("println_helper"));
    }

    #[test]
    fn reachability_follows_resolved_edges_only() {
        let graph = build(Lang::Rust, RUST).unwrap();
        assert!(graph.reaches("handle_request", "store"));
        assert!(!graph.reaches("store", "handle_request"));
        assert!(!graph.reaches("unrelated", "store"));
    }

    #[test]
    fn python_calls_and_attribute_calls_are_extracted() {
        let graph = build(Lang::Python, PYTHON).unwrap();
        assert!(graph.reaches("handler", "run_query"));
        assert!(graph.reaches("handler", "sanitize"));
        assert!(graph.unresolved["run_query"].contains("execute"), "{:?}", graph.unresolved);
    }
}
