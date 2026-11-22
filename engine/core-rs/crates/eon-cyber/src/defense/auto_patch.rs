//! Checked source proposals for human review, never live hotpatches.

use std::{fmt, iter::once, ops::Range};

use tree_sitter::Node;

use crate::cpg::{Lang, parse};

use super::{Assessment, Gap, MAX_SOURCE_BYTES};

pub use crate::remediation::patch_isolated_python_token_check;

pub struct BoundsPatch {
    /// Byte range of the original expression in the supplied Rust source.
    pub replaced: Range<usize>,
    source: String,
}

impl BoundsPatch {
    pub fn patched_source(&self) -> &str {
        &self.source
    }
}

impl fmt::Debug for BoundsPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundsPatch")
            .field("replaced", &self.replaced)
            .finish_non_exhaustive()
    }
}

// Compare the entire parsed module's leaf tokens, not text with whitespace
// removed. Only ordinary Rust whitespace can occur in gaps between leaves.
fn matches_form(root: Node<'_>, src: &str, signature: &[&str], tail: &[&str]) -> bool {
    let mut expected = signature.iter().chain(tail).copied().chain(once("}"));
    let whitespace = |s: &str| s.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'));
    let mut end = 0;
    let mut cursor = root.walk();
    loop {
        if cursor.goto_first_child() {
            continue;
        }
        let token = cursor.node().byte_range();
        if !whitespace(&src[end..token.start]) || Some(&src[token.clone()]) != expected.next() {
            return false;
        }
        end = token.end;
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return expected.next().is_none() && whitespace(&src[end..]);
            }
        }
    }
}

fn name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    // The Rust grammar can parse reserved words as identifiers in some
    // contexts. Admit only simple, non-keyword Rust 2024 names.
    const KEYWORDS: &str = "as break const continue crate else enum extern false fn for if impl \
        in let loop match mod move mut pub ref return self static struct super trait true type \
        unsafe use where while async await dyn abstract become box do final macro override priv \
        typeof unsized virtual yield try gen";
    let s = &source[node.byte_range()];
    (node.kind() == "identifier"
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && !KEYWORDS.split_ascii_whitespace().any(|k| k == s))
    .then_some(s)
}

/// One isolated, comment-free Rust module, standard prelude, immutable byte slice, `usize`
/// index and `Option<u8>` return contract. In-bounds reads still yield `Some`;
/// out-of-bounds now yields `None`. Never invent a fallback for other contracts.
pub fn propose_checked_byte_lookup(source: &str) -> Assessment<BoundsPatch> {
    if source.len() > MAX_SOURCE_BYTES {
        return Assessment::Unverified(Gap::LimitExceeded);
    }
    let Ok(tree) = parse(Lang::Rust, source) else {
        return Assessment::Unverified(Gap::InvalidInput);
    };
    let root = tree.root_node();
    if root.has_error() {
        return Assessment::Unverified(Gap::InvalidInput);
    }
    let proposal = || {
        if root.named_child_count() != 1 {
            return None;
        }
        let function = root.named_child(0)?;
        if function.kind() != "function_item" {
            return None;
        }
        let params = function.child_by_field_name("parameters")?;
        let param = |i| params.named_child(i)?.child_by_field_name("pattern");
        let function_name = name(function.child_by_field_name("name")?, source)?;
        let bytes = name(param(0)?, source)?;
        let index = name(param(1)?, source)?;
        #[rustfmt::skip]
        let signature = [
            "fn", function_name, "(", bytes, ":", "&", "[", "u8", "]", ",", index, ":", "usize", ")",
            "->", "Option", "<", "u8", ">", "{",
        ];
        #[rustfmt::skip]
        let original = [
            "Some", "(", "unsafe", "{", "*", bytes, ".", "get_unchecked", "(", index, ")", "}", ")",
        ];
        if bytes == index || !matches_form(root, source, &signature, &original) {
            return None;
        }
        let expr = function.child_by_field_name("body")?.named_child(0)?;
        let replaced = expr.byte_range();
        let mut patched = source.to_owned();
        patched.replace_range(replaced.clone(), &format!("{bytes}.get({index}).copied()"));
        let after = parse(Lang::Rust, &patched).ok()?;
        let expected = [bytes, ".", "get", "(", index, ")", ".", "copied", "(", ")"];
        (!after.root_node().has_error()
            && matches_form(after.root_node(), &patched, &signature, &expected))
        .then_some(BoundsPatch {
            replaced,
            source: patched,
        })
    };
    proposal().map_or(
        Assessment::Unverified(Gap::OutsideRule),
        Assessment::Observed,
    )
}
