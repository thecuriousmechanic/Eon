//! Read-only architectural analysis of source text supplied by the caller.
//! The map covers named function definitions (including selected method and
//! TypeScript arrow forms) and named type declarations, not every identifier
//! or type reference. No filesystem discovery, language servers, submitted
//! code execution, or debugger integration is involved.
//!
//! ```
//! use eon_cyber::ast::repomap::{build, Language, SourceFile};
//! let files = [SourceFile {
//!     path: "palette.py",
//!     language: Language::Python,
//!     source: "def tone():\n    return 3\ndef preview():\n    return tone()\n",
//! }];
//! let map = build(&files).unwrap();
//! assert!(map.summary(200).starts_with("palette.py:1 Function tone\n"));
//! ```

pub mod repomap;
mod tree_sitter_engine;
