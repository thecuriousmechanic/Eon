use std::io;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use eon_cyber::cpg::FunctionBlock;
use tree_sitter::{Node, ParseOptions, ParseState, Parser};

use super::invalid;

fn declaration(item: Node<'_>) -> Option<Node<'_>> {
    if item.kind() == "export_statement" {
        item.child_by_field_name("declaration")
    } else {
        Some(item)
    }
}

pub(super) fn function(parser: &mut Parser, source: &str, symbol: &str) -> io::Result<FunctionBlock> {
    // JS/TS grammars can spend time resolving ambiguities. In addition to
    // the snapshot byte limit, cancel at the parser's next progress callback.
    let started = Instant::now();
    let mut budget = |_: &ParseState| {
        if started.elapsed() >= Duration::from_secs(1) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser
        .parse_with_options(
            &mut |offset, _| source.as_bytes().get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut budget)),
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "structural parse did not complete"))?;
    let root = tree.root_node();
    if root.has_error() || root.end_byte() != source.len() {
        return Err(invalid("source contains syntax errors"));
    }
    let mut found = None;
    let mut cursor = root.walk();
    for mut node in root.named_children(&mut cursor).filter_map(declaration) {
        let ambient = node.kind() == "ambient_declaration";
        if ambient {
            // Count `declare function` signatures as unsupported, but never
            // descend into a namespace, block or class looking for targets.
            let mut cursor = node.walk();
            let Some(inner) = node.named_children(&mut cursor).find(|child| !child.is_extra()) else {
                continue;
            };
            node = inner;
        }
        if !matches!(
            node.kind(),
            "function_declaration" | "generator_function_declaration" | "function_signature"
        ) {
            continue;
        }
        let name = node
            .child_by_field_name("name")
            .ok_or_else(|| invalid("missing function name"))?;
        let name = &source[name.byte_range()];
        // An escaped identifier could denote the same symbol under a second spelling.
        if name.contains('\\') {
            return Err(invalid("escaped top-level function names are unsupported"));
        }
        if name != symbol {
            continue;
        }
        if ambient || node.kind() == "function_signature" {
            return Err(invalid("ambient or overloaded function is not a unique definition"));
        }
        if found.replace(node).is_some() {
            return Err(invalid("ambiguous top-level function"));
        }
    }
    let node = found.ok_or_else(|| invalid("top-level function not found"))?;
    // Both the declaration and its body may include automatic-semicolon
    // trivia *after* `}`. A trailing comment belongs to the surrounding file.
    let start = node
        .children(&mut node.walk())
        .find(|child| !child.is_extra())
        .filter(|child| matches!(child.kind(), "async" | "function"))
        .ok_or_else(|| invalid("missing function keyword"))?;
    let body = node
        .child_by_field_name("body")
        .ok_or_else(|| invalid("missing function body"))?;
    let end = body
        .children(&mut body.walk())
        .find(|child| child.kind() == "}")
        .ok_or_else(|| invalid("missing closing function brace"))?;
    Ok(FunctionBlock {
        name: symbol.into(),
        start_line: start.start_position().row + 1,
        start_byte: start.start_byte(),
        end_byte: end.end_byte(),
        text: source[start.start_byte()..end.end_byte()].into(),
    })
}
