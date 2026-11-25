//! In-memory, single-candidate edits for benign compiler diagnostics. No file
//! access, process execution, test results, case IDs, or reference patches.

use serde_json::Value;
use std::ops::Range;
use tree_sitter::Parser;

use crate::cpg::{Lang, parse};

mod kotlin;
mod typescript;

type Edit = (Range<usize>, String);

fn apply(source: &str, mut edits: Vec<Edit>) -> Option<String> {
    edits.sort_by_key(|(span, _)| (span.start, span.end));
    if edits.is_empty() || edits.len() > 8 {
        return None;
    }
    let mut result = source.to_owned();
    let mut next = source.len() + 1;
    for (span, text) in edits.into_iter().rev() {
        if source.get(span.clone()).is_none() || span.end > next || span.start == next || text.len() > 128 {
            return None;
        }
        next = span.start;
        result.replace_range(span, &text);
    }
    (result != source && result.len() <= 8192).then_some(result)
}

fn rust(source: &str, diag: &Value) -> Option<(String, String)> {
    if diag["level"] != "error" {
        return None;
    }
    let tree = parse(Lang::Rust, source).ok()?;
    let code = diag["code"]["code"].as_str().unwrap_or("syntax");
    let primary = diag["spans"].as_array()?.iter().find(|s| s["is_primary"] == true)?;
    if primary["file_name"] != "subject.rs" {
        return None;
    }
    let start = usize::try_from(primary["byte_start"].as_u64()?).ok()?;
    let end = usize::try_from(primary["byte_end"].as_u64()?).ok()?;
    source.get(start..end)?;
    let node = tree.root_node().named_descendant_for_byte_range(start, end)?;
    let mut candidates = Vec::new();
    for child in diag["children"].as_array()? {
        if child["level"] != "help" {
            continue;
        }
        let spans = child["spans"].as_array()?;
        let edits = spans
            .iter()
            .map(|s| {
                if s["file_name"] != "subject.rs" || !s["expansion"].is_null() {
                    return None;
                }
                let lo = usize::try_from(s["byte_start"].as_u64()?).ok()?;
                let hi = usize::try_from(s["byte_end"].as_u64()?).ok()?;
                Some((lo..hi, s["suggested_replacement"].as_str()?.to_owned()))
            })
            .collect::<Option<Vec<_>>>();
        let Some(edits) = edits else { continue };
        let fragments: Vec<_> = edits.iter().map(|(_, text)| text.as_str()).collect();
        let machine = spans
            .iter()
            .all(|s| s["suggestion_applicability"] == "MachineApplicable");
        // rustc classifies some context-dependent edits as tentative. Admit
        // only specific operations, never placeholders or arbitrary help text.
        let plausible = match (code, fragments.as_slice()) {
            ("E0382", ["&"]) => child["message"].as_str()?.contains("iterating over"),
            ("E0599", [import]) if import.starts_with("use std::") && import.ends_with(";\n") => true,
            ("E0599", [""]) => child["message"] == "remove the arguments",
            ("E0107", [""]) => child["message"] == "remove the unnecessary generics",
            ("E0308", ["?"]) => child["message"].as_str()?.contains("propagating"),
            ("E0277", ["#[derive(Clone)]\n"]) => true,
            ("E0106", [param, lifetime]) => param.starts_with("<'") && lifetime.starts_with('\''),
            ("E0432", [path]) => node.kind() == "identifier" && path.starts_with("crate::"),
            _ => false,
        };
        if (machine || plausible)
            && let Some(result) = apply(source, edits)
            && !parse(Lang::Rust, &result).ok()?.root_node().has_error()
        {
            candidates.push(result);
        }
    }
    (candidates.len() == 1).then(|| (format!("rustc:{code}"), candidates.remove(0)))
}

/// Propose at most one localized repair for the leading diagnostic. Rust
/// consumes one rustc JSON diagnostic. TS and Kotlin consume a compiler code,
/// UTF-8 byte offset and message. Require a full parse of the revision;
/// Kotlin's checked layout view preserves the original byte offsets.
pub fn propose(dialect: &str, source: &str, diagnostic: &Value) -> Option<(String, String)> {
    if source.is_empty() || source.len() > 8192 {
        return None;
    }
    match dialect {
        "rust" => rust(source, diagnostic),
        "kotlin" => {
            let mut parser = Parser::new();
            parser.set_language(&tree_sitter_kotlin_ng::LANGUAGE.into()).ok()?;
            let code = diagnostic["code"].as_str()?;
            let tree = kotlin::parse(&mut parser, source, code)?;
            let edit = kotlin::edit(tree.root_node(), source, diagnostic)?;
            let result = apply(source, vec![edit])?;
            kotlin::parse(&mut parser, &result, code)?;
            Some((code.to_owned(), result))
        }
        "typescript" => {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
                .ok()?;
            let tree = parser.parse(source, None)?;
            let edits = typescript::edits(tree.root_node(), source, diagnostic)?;
            let result = apply(source, edits)?;
            (!parser.parse(&result, None)?.root_node().has_error())
                .then(|| (format!("tsc:{}", diagnostic["code"].as_u64().unwrap()), result))
        }
        _ => None,
    }
}
