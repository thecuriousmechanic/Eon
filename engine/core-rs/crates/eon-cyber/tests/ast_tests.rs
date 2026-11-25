use eon_cyber::ast::repomap::{CallCounts, Language, SourceFile, SymbolKind, build};
use std::collections::BTreeSet;

fn file<'a>(path: &'a str, language: Language, source: &'a str) -> SourceFile<'a> {
    SourceFile {
        path,
        language,
        source,
    }
}

// A small palette library and its language-specific previews. All source is
// just input data: no loader, interpreter, compiler or test in the fixture.
#[test]
fn ast_tests_polyglot_palette_inventory_and_edges_match_a_handwritten_oracle() {
    use Language::*;
    use SymbolKind::*;
    let mut files = [
        file("lib/rgb.rs", Rust, "pub struct Ink;\npub fn shade() {}\n"),
        file(
            "lib/card.rs",
            Rust,
            "use crate::rgb::shade;\npub fn render() { shade(); }\n",
        ),
        file(
            "paint.kt",
            Kotlin,
            "interface Swatch\nclass Card {\n fun draw() = tint()\n}\nfun tint() = 3\n",
        ),
        file(
            "tones.c",
            C,
            "struct Tone { int red; };\nint base(void) { return 1; }\nint blend(void) { return base(); }",
        ),
        file(
            "paper.cpp",
            Cpp,
            "int base() { return 2; }\nclass Sheet { public: int height() { return base(); } };",
        ),
        file("inks.py", Python, "def pigment():\n    return 'ink'\n"),
        file(
            "palette.py",
            Python,
            "from inks import pigment\nclass Tray:\n    def fill(self):\n        return pigment()\n",
        ),
        file(
            "chip.ts",
            TypeScript,
            "export interface Chip { red: number }\nconst hue = (): number => 4;\nexport function paint() { return hue(); }\n",
        ),
    ];
    let map = build(&files).unwrap();
    let actual: Vec<_> = map
        .symbols
        .iter()
        .map(|s| (s.path, s.kind, s.name, s.line))
        .collect();
    assert_eq!(
        actual,
        [
            ("chip.ts", Interface, "Chip", 1),
            ("chip.ts", Function, "hue", 2),
            ("chip.ts", Function, "paint", 3),
            ("inks.py", Function, "pigment", 1),
            ("lib/card.rs", Function, "render", 2),
            ("lib/rgb.rs", Struct, "Ink", 1),
            ("lib/rgb.rs", Function, "shade", 2),
            ("paint.kt", Interface, "Swatch", 1),
            ("paint.kt", Class, "Card", 2),
            ("paint.kt", Function, "draw", 3),
            ("paint.kt", Function, "tint", 5),
            ("palette.py", Class, "Tray", 2),
            ("palette.py", Function, "fill", 3),
            ("paper.cpp", Function, "base", 1),
            ("paper.cpp", Class, "Sheet", 2),
            ("paper.cpp", Function, "height", 2),
            ("tones.c", Struct, "Tone", 1),
            ("tones.c", Function, "base", 2),
            ("tones.c", Function, "blend", 3),
        ]
    );
    let edges: Vec<_> = map
        .call_edges
        .iter()
        .map(|&(i, j)| {
            (
                map.symbols[i].name,
                map.symbols[j].path,
                map.symbols[j].name,
            )
        })
        .collect();
    assert_eq!(
        edges,
        [
            ("paint", "chip.ts", "hue"),
            ("render", "lib/rgb.rs", "shade"),
            ("draw", "paint.kt", "tint"),
            ("fill", "inks.py", "pigment"),
            ("height", "paper.cpp", "base"),
            ("blend", "tones.c", "base"),
        ]
    );
    assert_eq!(
        map.calls,
        CallCounts {
            matched: 6,
            ..Default::default()
        }
    );
    assert_eq!(
        map.member_edges,
        BTreeSet::from([(9, 8), (12, 11), (15, 14)])
    );
    for symbol in &map.symbols {
        let src = files.iter().find(|f| f.path == symbol.path).unwrap().source;
        assert!(src[symbol.name_byte..].starts_with(symbol.name));
    }
    files.reverse();
    assert_eq!(map, build(&files).unwrap());
}

#[test]
fn ast_tests_pagerank_agrees_with_a_closed_form_and_counts_repeated_edges_once() {
    // a -> b, c -> b; b dangles. Uniform redistribution gives
    // p_a=p_c=x and p_b=(1 + 2d)x, so x=1/(3+2d).
    let map = build(&[file(
        "weights.py",
        Language::Python,
        "def a():\n    b(); b()\ndef b():\n    pass\ndef c():\n    b()\n",
    )])
    .unwrap();
    assert_eq!(map.call_edges.len(), 2);
    assert_eq!(map.calls.matched, 3);
    let x = 1.0 / 4.7;
    for (&(id, score), (name, expected)) in
        map.ranking.iter().zip([("b", 2.7 * x), ("a", x), ("c", x)])
    {
        assert_eq!(map.symbols[id].name, name);
        assert!((score - expected).abs() < 1e-12, "{score} != {expected}");
    }
    let first = "weights.py:3 Function b\n";
    assert_eq!(map.summary(first.len()), first);
    assert_eq!(map.summary(first.len() - 1), "");
    assert!(map.summary(usize::MAX).len() <= 1500);
    assert!(build(&[]).unwrap().ranking.is_empty());
}

#[test]
fn ast_tests_pagerank_splits_votes_in_a_cycle_including_a_self_edge() {
    let m = build(&[file(
        "loop.c",
        Language::C,
        "void b(void);\nvoid a(void) { a(); b(); }\nvoid b(void) { a(); }",
    )])
    .unwrap();
    // a -> {a,b}; b -> a. p_b = (1-d)/2 + d*p_a/2, p_a+p_b=1.
    assert_eq!(m.ranking.len(), 2);
    for (&(id, score), expected) in m.ranking.iter().zip([37.0 / 57.0, 20.0 / 57.0]) {
        assert_eq!(m.symbols[id].name, if expected > 0.5 { "a" } else { "b" });
        assert!((score - expected).abs() < 1e-12);
    }
}

#[test]
fn ast_tests_file_local_c_definitions_do_not_leak_and_kotlin_types_are_named() {
    let m = build(&[
        file(
            "tray.c",
            Language::C,
            "static int tint(void) { return 1; }\nint fill(void) { return tint(); }",
        ),
        file(
            "box.c",
            Language::C,
            "int tint(void);\nint size(void) { return tint(); }",
        ),
        file(
            "card.kt",
            Language::Kotlin,
            "typealias Pigment = String\nenum class Color { RED, BLUE }\nobject Chart\n",
        ),
    ])
    .unwrap();
    assert_eq!(m.calls.matched, 1);
    assert_eq!(m.calls.unmatched, 1);
    let kotlin: Vec<_> = m
        .symbols
        .iter()
        .filter(|s| s.language == Language::Kotlin)
        .map(|s| (s.name, s.kind))
        .collect();
    assert_eq!(
        kotlin,
        [
            ("Pigment", SymbolKind::TypeAlias),
            ("Color", SymbolKind::Enum),
            ("Chart", SymbolKind::Object)
        ]
    );
}

#[test]
fn ast_tests_ambiguous_external_dynamic_and_cross_language_calls_do_not_vote() {
    let map = build(&[
        file(
            "app.ts",
            Language::TypeScript,
            "function draw() { mix(); box.mix(); shade(); const s = 'pretend()'; /* fake() */ }",
        ),
        file("dark.ts", Language::TypeScript, "function mix() {}"),
        file("light.ts", Language::TypeScript, "function mix() {}"),
        file("tones.c", Language::C, "void shade(void) {}"),
    ])
    .unwrap();
    assert!(map.call_edges.is_empty());
    assert_eq!(
        map.calls,
        CallCounts {
            matched: 0,
            ambiguous: 1,
            unmatched: 1,
            unsupported: 1
        }
    );
    assert!(map.ranking.iter().all(|(_, p)| *p == 0.25));
}

#[test]
fn ast_tests_distinct_declarations_and_calls_have_lexical_owners_not_just_names() {
    let map = build(&[file("swatch.py", Language::Python,
        "class A:\n    def label(self):\n        warm()\nclass B:\n    def label(self):\n        cool()\ndef warm():\n    pass\ndef cool():\n    thunk = lambda: warm()\n")]).unwrap();
    assert_eq!(map.call_edges, BTreeSet::from([(1, 4), (3, 5)]));
    assert_eq!(map.member_edges, BTreeSet::from([(1, 0), (3, 2)]));
    assert_eq!(map.symbols[1].name, map.symbols[3].name);
    assert_ne!(map.symbols[1].name_byte, map.symbols[3].name_byte);
}

#[test]
fn ast_tests_invalid_or_over_budget_inputs_never_yield_partial_rankings() {
    use Language::Rust;
    let valid = file("lib.rs", Rust, "fn shade() {}");
    for pair in [
        [valid, valid],
        [valid, file("syntax.rs", Rust, "fn unfinished( {}")],
        [valid, file("nul.rs", Rust, "// \0")],
        [valid, file("bad\nlabel", Rust, "")],
    ] {
        assert!(build(&pair).is_err());
    }
    assert!(build(&vec![valid; 129]).is_err());
    let large = " ".repeat(128 * 1024 + 1);
    assert!(build(&[file("big.rs", Rust, &large)]).is_err());
    let many = "fn shade() {}\n".repeat(4097);
    assert!(build(&[file("many.rs", Rust, &many)]).is_err());
}

#[test]
fn ast_tests_c_conditionals_and_cpp_linkage_do_not_hide_free_definitions() {
    let m = build(&[
        file("tones.c", Language::C, "#ifndef TONES\n#define TONES\n#if WARM\nint red(void) { return 1; }\n#elif COOL\nint blue(void) { return 2; }\n#else\nint grey(void) { return 3; }\n#endif\n#endif\n"),
        file("card.c", Language::C, "int red(void), blue(void), grey(void);\nint sample(void) {\n#if WARM\nreturn red();\n#elif COOL\nreturn blue();\n#else\nreturn grey();\n#endif\n}\n"),
        file("sheet.cpp", Language::Cpp, "extern \"C\" {\n#ifdef PAPER\nint size() { return 1; }\n#endif\n}\nnamespace detail { int depth() { return 2; } }"),
        file("card.cpp", Language::Cpp, "extern \"C\" int size();\nint depth();\nvoid draw() {\n#ifdef PAPER\nsize();\n#endif\ndepth(); }"),
    ]).unwrap();
    let edges: BTreeSet<_> = m
        .call_edges
        .iter()
        .map(|&(i, j)| (m.symbols[i].name, m.symbols[j].name))
        .collect();
    assert_eq!(
        edges,
        BTreeSet::from([
            ("sample", "red"),
            ("sample", "blue"),
            ("sample", "grey"),
            ("draw", "size")
        ])
    );
    assert_eq!(m.calls.unmatched, 1); // namespace member is not a free target
}

#[test]
fn ast_tests_kotlin_expect_does_not_compete_with_actual_definition() {
    let m = build(&[
        file(
            "common.kt",
            Language::Kotlin,
            "expect fun tint(): Int\nexpect fun paper(): Int\nfun draw() = tint() + paper()\n",
        ),
        file(
            "palette.kt",
            Language::Kotlin,
            "actual fun tint(): Int = 1\n",
        ),
    ])
    .unwrap();
    let targets: Vec<_> = m
        .call_edges
        .iter()
        .map(|&(_, i)| (m.symbols[i].path, m.symbols[i].name))
        .collect();
    assert_eq!(targets, [("palette.kt", "tint")]);
    assert_eq!(
        m.calls,
        CallCounts {
            matched: 1,
            unmatched: 1,
            ..Default::default()
        }
    );
}

#[test]
fn ast_tests_unindexed_ts_computed_method_body_is_not_a_factory_call() {
    let m = build(&[file("card.ts", Language::TypeScript,
        "function shared() {} function hidden() {} function factory(key: string) { shared(); return { [key]() { hidden(); }, label() { shared(); } }; }")]).unwrap();
    let edges: BTreeSet<_> = m
        .call_edges
        .iter()
        .map(|&(i, j)| (m.symbols[i].name, m.symbols[j].name))
        .collect();
    assert_eq!(
        edges,
        BTreeSet::from([("factory", "shared"), ("label", "shared")])
    );
    assert_eq!(m.calls.matched, 2);
}

#[test]
fn ast_tests_c_and_cpp_typedefs_capture_each_declarator_not_field_or_parameter_names() {
    let src = "typedef struct { int red; } Tone, *TonePtr;\ntypedef Tone Palette[4];\ntypedef int (*Tint)(int shade);\n";
    for (path, lang) in [("inks.c", Language::C), ("inks.cpp", Language::Cpp)] {
        let m = build(&[file(path, lang, src)]).unwrap();
        let aliases: Vec<_> = m
            .symbols
            .iter()
            .map(|s| (s.name, s.kind, s.name_byte))
            .collect();
        let expected = ["Tone", "TonePtr", "Palette", "Tint"]
            .map(|name| (name, SymbolKind::TypeAlias, src.find(name).unwrap()));
        assert_eq!(aliases, expected, "{path}");
    }
}
