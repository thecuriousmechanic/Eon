//! Source-only remediation for a complete, isolated CPython module with standard builtins
//! and the unmodified standard-library `hmac` (no injected globals, import hooks or project
//! imports). A source unit cannot attest to the import environment of a deployed application.

use std::{fmt, ops::Range};

use eon_primitives::{EonError, EonResult};
use tree_sitter::Node;

use crate::cpg::{Lang, parse};

/// Byte ranges in the original source: the externally supplied argument, its indexed
/// read, and the condition that returns early. No source text is needed for diagnostics.
#[derive(Debug, PartialEq, Eq)]
pub struct TokenFlow {
    pub parameter: Range<usize>,
    pub indexed_read: Range<usize>,
    pub branch_condition: Range<usize>,
}

/// A checked replacement for this input only. Debug output omits the module text.
pub struct TokenPatch {
    pub flow: TokenFlow,
    pub replaced: Range<usize>,
    source: String,
}

impl TokenPatch {
    pub fn patched_source(&self) -> &str {
        &self.source
    }
}

impl fmt::Debug for TokenPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenPatch")
            .field("flow", &self.flow)
            .field("replaced", &self.replaced)
            .finish_non_exhaustive()
    }
}

struct ManualCheck<'a> {
    provided: &'a str,
    expected: &'a str,
    index: &'a str,
    replaced: Range<usize>,
    flow: TokenFlow,
}

fn token_parameter(name: &str) -> bool {
    matches!(name, "presented" | "expected")
        || (name.ends_with("_token")
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
}

fn inspect<'a>(root: Node<'_>, source: &'a str) -> Option<ManualCheck<'a>> {
    if root.has_error() || root.named_child_count() != 2 {
        return None;
    }
    let text = |n: Node<'_>| &source[n.byte_range()];
    let function = root.named_child(1)?;
    let params = function.child_by_field_name("parameters")?;
    let parameter = params.named_child(0)?.named_child(0)?;
    let provided = text(parameter);
    let expected = text(params.named_child(1)?.named_child(0)?);
    if !token_parameter(provided) || !token_parameter(expected) || provided == expected {
        return None;
    }
    let body = function.child_by_field_name("body")?;
    let early_exit = body.named_child(2)?;
    let index = text(early_exit.child_by_field_name("left")?);
    if !matches!(index, "i" | "idx" | "index") {
        return None;
    }
    let branch = early_exit.child_by_field_name("body")?.named_child(0)?;
    let condition = branch.child_by_field_name("condition")?;
    let read = condition.named_child(0)?;
    if read.kind() != "subscript" || text(read.child_by_field_name("value")?) != provided {
        return None;
    }
    Some(ManualCheck {
        provided,
        expected,
        index,
        replaced: early_exit.start_byte()..body.named_child(3)?.end_byte(),
        flow: TokenFlow {
            parameter: parameter.byte_range(),
            indexed_read: read.byte_range(),
            branch_condition: condition.byte_range(),
        },
    })
}

// Match the *whole module*, including indentation. Tree-sitter alone is permissive
// about Python trivia; whitespace stripping, subtree searches or annotations alone
// would not prove the guards dominate the comparison, or rule out shadowed names.
fn code_lines(source: &str) -> impl Iterator<Item = &str> {
    source
        .lines()
        .map(|line| line.trim_end_matches(' '))
        .filter(|line| !line.is_empty())
}

// Check the emitted callee and both operands independently of the edit renderer.
fn returned_digest_operands<'a>(root: Node<'_>, source: &'a str) -> Option<(&'a str, &'a str)> {
    let body = root.named_child(1)?.child_by_field_name("body")?;
    let ret = body.named_child(2)?;
    if root.has_error() || body.named_child_count() != 3 || ret.kind() != "return_statement" {
        return None;
    }
    let call = ret.named_child(0)?;
    let callee = call.child_by_field_name("function")?;
    let args = call.child_by_field_name("arguments")?;
    if call.kind() != "call" || &source[callee.byte_range()] != "hmac.compare_digest" || args.named_child_count() != 2 {
        return None;
    }
    Some((
        &source[args.named_child(0)?.byte_range()],
        &source[args.named_child(1)?.byte_range()],
    ))
}

/// Propose a single in-memory edit for the isolated CPython profile above. `None`
/// means outside this rule, NOT "secure". No submitted source is imported or run.
/// The admitted family is one complete `verify_auth_token` definition with an
/// existing `import hmac`, exact `bytes` and 32-byte length guards, and an indexed
/// first-mismatch loop. Only names, blank lines, line endings and trailing spaces vary.
pub fn patch_isolated_python_token_check(source: &str) -> EonResult<Option<TokenPatch>> {
    if source.len() > 16 * 1024 {
        return Ok(None);
    }
    let tree = parse(Lang::Python, source)?;
    let Some(ManualCheck {
        provided,
        expected,
        index,
        replaced,
        flow,
    }) = inspect(tree.root_node(), source)
    else {
        return Ok(None);
    };
    let guards = format!(
        "import hmac
def verify_auth_token({provided}: bytes, {expected}: bytes) -> bool:
    if type({provided}) is not bytes or type({expected}) is not bytes:
        return False
    if len({provided}) != 32 or len({expected}) != 32:
        return False"
    );
    let manual = format!(
        "    for {index} in range(32):
        if {provided}[{index}] != {expected}[{index}]:
            return False
    return True"
    );
    if !code_lines(source).eq(code_lines(&guards).chain(code_lines(&manual))) {
        return Ok(None);
    }

    let call = format!("return hmac.compare_digest({provided}, {expected})");
    let mut patched = source.to_owned();
    patched.replace_range(replaced.clone(), &call);
    let tree = parse(Lang::Python, &patched)?;
    if returned_digest_operands(tree.root_node(), &patched) != Some((provided, expected)) {
        return Err(EonError::OperationFailed {
            operation: "remediation.token_check".into(),
            reason: "replacement failed its source postcondition".into(),
        });
    }
    Ok(Some(TokenPatch {
        flow,
        replaced,
        source: patched,
    }))
}
