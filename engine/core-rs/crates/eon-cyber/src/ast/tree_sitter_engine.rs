use super::repomap::{
    Inventory, Language, MAX_CALLS, MAX_SYMBOLS, SourceFile, Symbol, SymbolKind, error,
};
use eon_primitives::EonResult;
use std::ops::ControlFlow;
use tree_sitter::{Node, ParseOptions, Parser, TreeCursor};

const MAX_PARSE_CHECKS: usize = 4096;
const MAX_TREE_NODES: usize = 64_000;

impl Language {
    fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        }
    }
}

fn is_identifier(n: Node<'_>) -> bool {
    matches!(
        n.kind(),
        "identifier" | "type_identifier" | "property_identifier" | "field_identifier"
    )
}

/// Peel the declarator, never rummage through the parameter list for a name.
fn c_name(mut n: Node<'_>) -> Option<(Node<'_>, bool)> {
    let mut scoped = false;
    loop {
        n = match n.kind() {
            "identifier" | "field_identifier" | "type_identifier" => return Some((n, scoped)),
            "qualified_identifier" => {
                scoped = true;
                n.child_by_field_name("name")?
            }
            "reference_declarator" | "parenthesized_declarator" => n.named_child(0)?,
            "function_declarator" | "pointer_declarator" | "array_declarator" => {
                n.child_by_field_name("declarator")?
            }
            _ => return None,
        };
    }
}

/// Wrappers that do not introduce a lexical scope around a declaration.
fn at_file_scope(n: Node<'_>) -> bool {
    let mut parent = n.parent();
    while let Some(p) = parent {
        if p.parent().is_none() {
            return true;
        }
        if !matches!(
            p.kind(),
            "decorated_definition"
                | "export_statement"
                | "lexical_declaration"
                | "variable_declaration"
                | "template_declaration"
                | "preproc_if"
                | "preproc_ifdef"
                | "preproc_elif"
                | "preproc_elifdef"
                | "preproc_else"
                | "linkage_specification"
                | "declaration_list"
        ) {
            return false;
        }
        parent = p.parent();
    }
    false
}

fn declaration<'t>(
    lang: Language,
    cursor: &TreeCursor<'t>,
) -> Option<(SymbolKind, Node<'t>, bool)> {
    use Language::*;
    use SymbolKind::*;
    let n = cursor.node();
    // Each declarator of a typedef introduces a separate alias; its type,
    // fields and parameter names are not aliases.
    if matches!(lang, C | Cpp)
        && cursor.field_name() == Some("declarator")
        && n.parent().is_some_and(|p| p.kind() == "type_definition")
    {
        return c_name(n).map(|(name, _)| (TypeAlias, name, false));
    }
    let kind = match (lang, n.kind()) {
        (Rust, "function_item")
        | (Kotlin | TypeScript, "function_declaration")
        | (Python, "function_definition")
        | (TypeScript, "generator_function_declaration" | "method_definition") => Function,
        (TypeScript, "variable_declarator")
            if n.child_by_field_name("value")
                .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression")) =>
        {
            Function
        }
        (C | Cpp, "function_definition") => {
            let (name, scoped) = c_name(n.child_by_field_name("declarator")?)?;
            return Some((Function, name, !scoped && at_file_scope(n)));
        }
        (Rust, "struct_item") => Struct,
        (Rust, "trait_item") => Trait,
        (Rust, "union_item") => Union,
        (Rust, "enum_item") | (TypeScript, "enum_declaration") => Enum,
        (Rust, "type_item")
        | (TypeScript, "type_alias_declaration")
        | (Cpp, "alias_declaration") => TypeAlias,
        (C | Cpp, "struct_specifier") if n.child_by_field_name("body").is_some() => Struct,
        (C | Cpp, "union_specifier") if n.child_by_field_name("body").is_some() => Union,
        (C | Cpp, "enum_specifier") if n.child_by_field_name("body").is_some() => Enum,
        (Cpp, "class_specifier") if n.child_by_field_name("body").is_some() => Class,
        (Python, "class_definition")
        | (TypeScript, "class_declaration" | "abstract_class_declaration") => Class,
        (TypeScript, "interface_declaration") => Interface,
        (Kotlin, "object_declaration" | "companion_object") => Object,
        (Kotlin, "type_alias") => {
            return Some((TypeAlias, n.child_by_field_name("type")?, false));
        }
        // The Kotlin grammar uses class_declaration for interfaces as well.
        (Kotlin, "class_declaration") => {
            let mut cursor = n.walk();
            n.children(&mut cursor)
                .find_map(|child| match child.kind() {
                    "interface" => Some(Interface),
                    "modifiers" => {
                        let mut modifiers = child.walk();
                        child
                            .named_children(&mut modifiers)
                            .any(|m| {
                                m.kind() == "class_modifier"
                                    && m.child(0).is_some_and(|keyword| keyword.kind() == "enum")
                            })
                            .then_some(Enum)
                    }
                    _ => None,
                })
                .unwrap_or(Class)
        }
        _ => return None,
    };
    let name = n
        .child_by_field_name("name")
        .filter(|n| is_identifier(*n))?;
    let free = kind == Function
        && at_file_scope(n)
        && (lang != Kotlin || {
            // Keep the symbol location of an `expect fun`, but it is not an
            // implementation that can receive an edge instead of the `actual`.
            let mut children = n.walk();
            n.named_children(&mut children)
                .any(|c| c.kind() == "function_body")
        });
    Some((kind, name, free))
}

fn call_target(lang: Language, n: Node<'_>) -> Option<Option<Node<'_>>> {
    match (lang, n.kind()) {
        (Language::Kotlin, "call_expression") => {
            let mut cursor = n.walk();
            Some(
                n.named_children(&mut cursor)
                    .find(|c| !c.is_extra())
                    .filter(|c| c.kind() == "identifier"),
            )
        }
        (Language::Python, "call")
        | (
            Language::Rust | Language::C | Language::Cpp | Language::TypeScript,
            "call_expression",
        ) => Some(
            n.child_by_field_name("function")
                .filter(|c| c.kind() == "identifier"),
        ),
        _ => None,
    }
}

/// Anonymous bodies have no symbol to own their calls. A directly bound TS
/// function expression/arrow is indexed under its variable's declaration.
fn anonymous_scope(lang: Language, n: Node<'_>) -> bool {
    match (lang, n.kind()) {
        (Language::Python, "lambda")
        | (Language::Cpp, "lambda_expression")
        | (Language::Rust, "closure_expression")
        | (Language::Kotlin, "lambda_literal" | "anonymous_function")
        // Named methods were handled by `declaration`; even when a computed
        // name is not indexed, the method is a boundary for body calls.
        | (Language::TypeScript, "generator_function" | "method_definition") => true,
        (Language::TypeScript, "arrow_function" | "function_expression") => {
            !n.parent().is_some_and(|p| {
                p.kind() == "variable_declarator"
                    && p.child_by_field_name("name")
                        .is_some_and(|v| v.kind() == "identifier")
                    && p.child_by_field_name("value") == Some(n)
            })
        }
        _ => false,
    }
}

pub(super) fn collect<'a>(file: SourceFile<'a>, inventory: &mut Inventory<'a>) -> EonResult<()> {
    let mut parser = Parser::new();
    parser
        .set_language(&file.language.grammar())
        .map_err(|_| error("incompatible source grammar"))?;
    let mut checks = 0;
    let mut budget = |_: &tree_sitter::ParseState| {
        checks += 1;
        if checks >= MAX_PARSE_CHECKS {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = file.source.as_bytes();
    let tree = parser
        .parse_with_options(
            &mut |at, _| bytes.get(at..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut budget)),
        )
        .ok_or_else(|| error("source parse budget exceeded"))?;
    let root = tree.root_node();
    if root.has_error() {
        return Err(error("source contains syntax errors"));
    }
    if root.descendant_count() > MAX_TREE_NODES {
        return Err(error("syntax tree node limit exceeded"));
    }

    // Keep an owner per depth, not a recursive Rust stack or copies of bodies.
    let mut owners: Vec<Option<usize>> = Vec::new();
    let mut cursor = root.walk();
    loop {
        let n = cursor.node();
        let mut owner = owners.last().copied().flatten();
        if let Some((kind, name, free)) = declaration(file.language, &cursor) {
            if inventory.symbols.len() == MAX_SYMBOLS {
                return Err(error("symbol limit exceeded"));
            }
            let id = inventory.symbols.len();
            let text = &file.source[name.byte_range()];
            if text.len() > 256 || text.chars().any(char::is_control) {
                return Err(error("invalid symbol label"));
            }
            inventory.symbols.push(Symbol {
                path: file.path,
                language: file.language,
                kind,
                name: text,
                line: name.start_position().row + 1,
                name_byte: name.start_byte(),
            });
            if free {
                let file_local = matches!(file.language, Language::C | Language::Cpp) && {
                    let mut cursor = n.walk();
                    n.named_children(&mut cursor).any(|c| {
                        c.kind() == "storage_class_specifier"
                            && &file.source[c.byte_range()] == "static"
                    })
                };
                inventory.free_functions.push((id, file_local));
            }
            if let Some(parent) = owner
                && inventory.symbols[parent].kind != SymbolKind::Function
            {
                inventory.members.insert((id, parent));
            }
            owner = Some(id);
        } else if anonymous_scope(file.language, n) {
            owner = None;
        } else if let Some(id) = owner
            && inventory.symbols[id].kind == SymbolKind::Function
            && let Some(target) = call_target(file.language, n)
        {
            if inventory.calls.len() == MAX_CALLS {
                return Err(error("call site limit exceeded"));
            }
            inventory
                .calls
                .push((id, target.map(|t| &file.source[t.byte_range()])));
        }
        if cursor.goto_first_child() {
            owners.push(owner);
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
            owners.pop();
        }
    }
}
