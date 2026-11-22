use serde_json::Value;
use tree_sitter::Node;

use super::Edit;

fn ancestor<'t>(mut node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    loop {
        if node.kind() == kind {
            return Some(node);
        }
        node = node.parent()?;
    }
}

// Require a unique matching declaration/construct, not the first occurrence
// of the spelling in the file. The caller chooses the enclosing scope.
fn unique<'t>(root: Node<'t>, pred: impl Fn(Node<'t>) -> bool) -> Option<Node<'t>> {
    let mut stack = vec![root];
    let mut found = None;
    while let Some(node) = stack.pop() {
        if pred(node) && found.replace(node).is_some() {
            return None;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    found
}

pub(super) fn edits(root: Node<'_>, src: &str, diag: &Value) -> Option<Vec<Edit>> {
    let pos = usize::try_from(diag["offset"].as_u64()?).ok()?;
    src.get(pos..pos.checked_add(1)?)?;
    let leaf = root.descendant_for_byte_range(pos, pos + 1)?;
    let text = |n: Node<'_>| &src[n.byte_range()];
    let insert = |pos, s: &str| (pos..pos, s.to_owned());
    let replace = |n: Node<'_>, s: &str| (n.byte_range(), s.to_owned());
    let msg = diag["message"].as_str()?;
    let edits = match diag["code"].as_u64()? {
        2339 if msg.contains("on type 'string'") => {
            let member = ancestor(leaf, "member_expression")?;
            let loop_ = ancestor(member, "for_in_statement")?;
            let op = loop_.child_by_field_name("operator")?;
            let item = loop_.child_by_field_name("left")?;
            let seq = loop_.child_by_field_name("right")?;
            let function = ancestor(loop_, "function_declaration")?;
            let param = unique(function.child_by_field_name("parameters")?, |n| {
                n.kind() == "required_parameter" && n.child_by_field_name("pattern").map(text) == Some(text(seq))
            })?;
            if text(op) != "in"
                || !text(param.child_by_field_name("type")?).ends_with("[]")
                || text(member.child_by_field_name("object")?) != text(item)
            {
                return None;
            }
            vec![replace(op, "of")]
        }
        2551 if leaf.kind() == "property_identifier" => {
            let name = msg.split_once("Did you mean '")?.1.strip_suffix("'?")?;
            if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return None;
            }
            vec![replace(leaf, name)]
        }
        2362 | 2363 if leaf.kind() == "identifier" => {
            let bin = leaf.parent()?;
            if bin.kind() != "binary_expression" || text(bin.child_by_field_name("operator")?) != "-" {
                return None;
            }
            let params = ancestor(bin, "function_declaration")?.child_by_field_name("parameters")?;
            unique(params, |n| {
                n.kind() == "required_parameter"
                    && n.child_by_field_name("pattern").map(text) == Some(text(leaf))
                    && n.child_by_field_name("type")
                        .is_some_and(|ty| text(ty).trim_start_matches(':').trim() == "Date")
            })?;
            vec![insert(leaf.end_byte(), ".getTime()")]
        }
        2322 if msg.contains("'void[]'") => {
            let ret = ancestor(leaf, "return_statement")?;
            let arrow = unique(ret, |n| n.kind() == "arrow_function")?;
            let block = arrow.child_by_field_name("body")?;
            // In this ambiguous arrow position tree-sitter can represent the
            // token sequence as an object/pair, while tsc inferred void[].
            if block.named_child_count() != 1
                || !matches!(
                    (block.kind(), block.named_child(0)?.kind()),
                    ("statement_block", "labeled_statement") | ("object", "pair")
                )
            {
                return None;
            }
            vec![insert(block.start_byte(), "("), insert(block.end_byte(), ")")]
        }
        2740 if msg.contains("Type 'Set<") && msg.contains("[]'") => {
            let expr = ancestor(leaf, "return_statement")?.named_child(0)?;
            if expr.kind() != "new_expression" || text(expr.child_by_field_name("constructor")?) != "Set" {
                return None;
            }
            vec![insert(expr.start_byte(), "Array.from("), insert(expr.end_byte(), ")")]
        }
        2794 if msg.contains("'void'") => {
            let ctor = ancestor(leaf, "new_expression")?.child_by_field_name("constructor")?;
            if text(ctor) != "Promise" {
                return None;
            }
            vec![insert(ctor.end_byte(), "<void>")]
        }
        18010 => {
            let field = ancestor(leaf, "public_field_definition")?;
            if field.child_by_field_name("name")?.kind() != "private_property_identifier" {
                return None;
            }
            vec![replace(ancestor(leaf, "accessibility_modifier")?, "")]
        }
        4114 => {
            let method = ancestor(leaf, "method_definition")?;
            if leaf != method.child_by_field_name("name")? || method.start_byte() != leaf.start_byte() {
                return None;
            }
            vec![insert(method.start_byte(), "override ")]
        }
        2869 => {
            let compare = ancestor(leaf, "binary_expression")?;
            let coalesce = compare.parent()?;
            if !matches!(text(compare.child_by_field_name("operator")?), "<" | ">" | "<=" | ">=")
                || text(coalesce.child_by_field_name("operator")?) != "??"
                || coalesce.child_by_field_name("left")? != compare
            {
                return None;
            }
            vec![
                insert(compare.child_by_field_name("right")?.start_byte(), "("),
                insert(coalesce.end_byte(), ")"),
            ]
        }
        1354 => {
            let ty = ancestor(leaf, "readonly_type")?;
            let inner = ty.named_child(0)?;
            let name = inner.child_by_field_name("name")?;
            if inner.kind() != "generic_type" || text(name) != "Array" {
                return None;
            }
            vec![(ty.start_byte()..name.end_byte(), "ReadonlyArray".to_owned())]
        }
        2556 => {
            let spread = ancestor(leaf, "spread_element")?;
            let name = spread.named_child(0)?;
            if name.kind() != "identifier" {
                return None;
            }
            let decl = unique(ancestor(spread, "function_declaration")?, |n| {
                n.kind() == "variable_declarator" && n.child_by_field_name("name").map(text) == Some(text(name))
            })?;
            let value = decl.child_by_field_name("value")?;
            if value.kind() != "array" || decl.parent()?.child(0)?.kind() != "const" {
                return None;
            }
            vec![insert(value.end_byte(), " as const")]
        }
        _ => return None,
    };
    Some(edits)
}
