use std::io;
use std::path::Path;

use eon_cyber::cpg::{FunctionBlock, Lang, checked_top_level_function};
use eon_cyber::function_review::{Decision, Review, apply_review, match_blocks};
use tree_sitter::Parser;

use super::{MAX_DOCUMENT, invalid, snapshot};
use crate::workspace::Workspace;

mod ecma;

enum Selector {
    Cpg(Lang),
    Ecma(Parser),
}

impl Selector {
    fn for_path(path: &str) -> io::Result<Self> {
        let grammar = match Path::new(path).extension().and_then(|s| s.to_str()) {
            Some("rs") => return Ok(Self::Cpg(Lang::Rust)),
            Some("py") => return Ok(Self::Cpg(Lang::Python)),
            Some("js" | "jsx" | "mjs" | "cjs") => tree_sitter_javascript::LANGUAGE,
            Some("ts" | "mts" | "cts") => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            Some("tsx") => tree_sitter_typescript::LANGUAGE_TSX,
            _ => return Err(invalid("unsupported structural editing language")),
        };
        let mut parser = Parser::new();
        parser.set_language(&grammar.into()).map_err(io::Error::other)?;
        Ok(Self::Ecma(parser))
    }

    fn find(&mut self, source: &str, symbol: &str) -> io::Result<FunctionBlock> {
        match self {
            Self::Cpg(lang) => checked_top_level_function(*lang, source, symbol).map_err(io::Error::other),
            Self::Ecma(parser) => ecma::function(parser, source, symbol),
        }
    }
}

/// In-memory proposal. The expected digest covers the entire original snapshot
/// and is suitable for `Workspace::edit` after separate operator approval.
#[derive(Debug)]
pub struct ReplacementPreview {
    pub relative_path: String,
    pub expected_sha256: [u8; 32],
    pub before: String,
    pub after: String,
}

/// Preview replacement of one named top-level function declaration in Rust,
/// Python, JavaScript/JSX or TypeScript/TSX. For JS/TS supply the declaration
/// without its `export`/`default` wrapper: those bytes belong to the caller's
/// module and are preserved. Methods, arrows and overload sets are not targets.
pub fn preview_replacement(
    workspace: &Workspace,
    path: &str,
    symbol: &str,
    replacement: &str,
) -> io::Result<ReplacementPreview> {
    let mut selector = Selector::for_path(path)?;
    if replacement.len() > MAX_DOCUMENT || symbol.is_empty() || symbol.len() > 256 {
        return Err(invalid("replacement or symbol exceeds limits"));
    }
    let (before, expected_sha256) = snapshot(workspace, path)?;
    let mut find = |src: &str| selector.find(src, symbol);
    let old = [find(&before)?];
    let new = [find(replacement)?];
    if new[0].text != replacement.trim_end() || before.len() - old[0].text.len() + new[0].text.len() > MAX_DOCUMENT {
        return Err(invalid(
            "replacement must consist of one complete function within budget",
        ));
    }
    let pairings = match_blocks(&old, &new);
    let mut review = Review::new(&pairings);
    if !review.decide(0, Decision::Applied) {
        return Err(invalid("replacement does not change the function"));
    }
    let after = apply_review(&before, &old, &new, &pairings, &review).map_err(|_| invalid("unreviewed function"))?;
    let actual = find(&after)?;
    if actual.start_byte != old[0].start_byte || actual.text != new[0].text {
        return Err(invalid("result did not preserve the selected function boundary"));
    }
    Ok(ReplacementPreview {
        relative_path: path.into(),
        expected_sha256,
        before,
        after,
    })
}
