use eon_cyber::benign_repair::propose;
use serde_json::json;
use tree_sitter::Parser;

// § marks the compiler's location; these snippets are only parsed, never run.
fn proposal(marked: &str, code: &str, message: &str) -> Option<String> {
    let (left, right) = marked.split_once('§').unwrap();
    let source = format!("{left}{right}");
    let diag = json!({"code": code, "offset": left.len(), "message": message});
    let (rule, revision) = propose("kotlin", &source, &diag)?;
    assert_eq!(rule, code);
    Some(revision)
}

fn repair(marked: &str, code: &str, message: &str) -> Option<String> {
    let revision = proposal(marked, code, message)?;
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    assert!(
        !parser
            .parse(&revision, None)
            .unwrap()
            .root_node()
            .has_error()
    );
    Some(revision)
}

const ARG: &str = "ARGUMENT_TYPE_MISMATCH";
const SPREAD: &str = "Argument type mismatch: actual type is 'IntArray', but 'Int' was expected.";
const WIDEN: &str = "Argument type mismatch: actual type is 'Short', but 'Long' was expected.";

#[test]
fn spread_needs_a_bare_call_and_a_unique_single_vararg_parameter() {
    let caller = "fun f(batch: IntArray) = total( /* 🌱 */ §batch )\n";
    let decl = "fun total(\n vararg ns: Int\n) = ns.size\n";
    let src = format!("// fun total(x: Any)\r\n{decl}{caller}");
    assert_eq!(repair(&src, ARG, SPREAD).unwrap(), src.replace('§', "*"));
    for bad in [
        caller.to_owned(),
        format!("// {decl:?}\n{caller}"),
        format!("val example = \"fun total(vararg x: Int)\"\n{caller}"),
        format!("fun total(x: Int) = x\n{caller}"),
        format!("fun Any.total(vararg x: Int) = x.size\n{caller}"),
        format!("fun total(vararg x: Int, z: Long) = z\n{caller}"),
        format!("fun total(z: Int) = z\n{src}"),
        format!("val `total` = 0\n{src}"),
        src.replace("total( /*", "other.total( /*"),
        src.replace("§batch )", "ns = §batch )"),
        src.replace("§batch )", "§batch.size )"),
        src.replace("batch: IntArray", "batch: Array<Int>"),
        src.replace("§batch", "b§atch"),
        src.replace("§batch", "batch /* §batch */"),
    ] {
        assert!(repair(&bad, ARG, SPREAD).is_none(), "{bad}");
    }
    assert!(repair(&src, ARG, WIDEN).is_none());
    assert!(repair(&src, "UNSAFE_CALL", SPREAD).is_none());
}

#[test]
fn widening_requires_agreement_between_both_parameter_types_and_the_message() {
    let src = "fun large(x: Long) = x\nfun send(y: Short) = large(§y)";
    assert_eq!(
        repair(src, ARG, WIDEN).unwrap(),
        src.replace("§y", "y.toLong()")
    );
    assert!(repair(&src.replace("x: Long", "x: Byte"), ARG, WIDEN).is_none());
    assert!(repair(&format!("class Short\n{src}"), ARG, WIDEN).is_none());
    assert!(repair(&src.replace("§y", "§100"), ARG, WIDEN).is_none());
}

const OP: &str = "OPERATOR_MODIFIER_REQUIRED";
const GET: &str = "'operator' modifier is required on 'fun get(p: Int): Int' defined in 'Grid'.";
const SET: &str = "'operator' modifier is required on 'fun set(p: Int, v: Int)' defined in 'Grid'.";

#[test]
fn indexing_matches_the_receiver_owner_and_unique_member_not_declaration_text() {
    let src = "class Grid {\n fun get(p: Int): Int = p\n}\nfun at(g: Grid) = §g[1]";
    assert_eq!(
        repair(src, OP, GET).unwrap(),
        src.replace('§', "").replace("fun get", "operator fun get")
    );
    for bad in [
        src.replace("§g[1]", "g[§1]"),
        src.replace("§g[1]", "§g.get(1)"),
        src.replace("g: Grid", "g: Other"),
        src.replace("fun get", "operator fun get"),
        src.replace("fun get", "fun Other.get"),
        src.replace("fun get", "fun get(p: Long) = p\n fun get"),
        src.replace("fun get", "// fun get"),
    ] {
        assert!(repair(&bad, OP, GET).is_none(), "{bad}");
    }
    assert!(repair(src, OP, SET).is_none());
    let set = "class Grid {\n fun set(p: Int, v: Int) {}\n}\nfun at(g: Grid) { §g[1] = 3 }";
    assert!(repair(set, OP, SET).unwrap().contains("operator fun set"));
    assert!(repair(set, OP, GET).is_none());
    assert!(repair(&set.replace("] =", "] +="), OP, SET).is_none());
}

#[test]
fn safe_call_needs_a_nullable_expression_body_and_the_diagnosed_dot() {
    let src = "fun size(name: String?): Int? = name§.length";
    let msg = "Only safe calls are allowed on a nullable receiver of type 'String?'.";
    let code = "UNSAFE_CALL";
    assert_eq!(repair(src, code, msg).unwrap(), src.replace('§', "?"));
    for bad in [
        src.replace("String?", "String"),
        src.replace("fun size", "fun String?.size")
            .replace(": Int?", ": Int"),
        src.replace("name§.length", "name.§length"),
        src.replace("name§.length", "other§.length"),
    ] {
        assert!(repair(&bad, code, msg).is_none(), "{bad}");
    }
}

#[test]
fn declaration_repairs_do_not_consume_comments_or_other_declarations() {
    let code = "CONST_VAL_WITH_NON_CONST_INITIALIZER";
    let src = "const /* keep */ val limit = §read()\n";
    assert_eq!(
        repair(src, code, "").unwrap(),
        " /* keep */ val limit = read()\n"
    );
    assert!(repair(&src.replace("§read()", "1 + §read()"), code, "").is_none());

    let code = "DATA_CLASS_NOT_PROPERTY_PARAMETER";
    let src = "data class Sample(val tag: String, §count: Int)";
    assert_eq!(repair(src, code, "").unwrap(), src.replace('§', "val "));
    assert!(repair(&src.replace("data class", "class"), code, "").is_none());
    assert!(repair(&src.replace("val tag", "val count"), code, "").is_none());
}

#[test]
fn data_parameter_refuses_conflicting_member_properties_by_normalized_name() {
    let code = "DATA_CLASS_NOT_PROPERTY_PARAMETER";
    for (param, member) in [
        ("edition", "edition"),
        ("edition", "`edition`"),
        ("`edition`", "edition"),
    ] {
        let src =
            format!("data class Volume(§{param}: Int) {{\n private val {member} = {param}\n}}");
        assert!(repair(&src, code, "").is_none(), "{src}");
    }
    let src = "data class Volume(§edition: Int) {\n val first = edition\n}";
    assert_eq!(repair(src, code, "").unwrap(), src.replace('§', "val "));
}

#[test]
fn supertype_requires_a_unique_open_class_with_an_implicit_empty_constructor() {
    let code = "SUPERTYPE_NOT_INITIALIZED";
    let src =
        "open class Shape {\n open fun n() = 0\n}\nclass Dot : §Shape {\n override fun n() = 1\n}";
    assert_eq!(
        repair(src, code, "").unwrap(),
        src.replace("§Shape", "Shape()")
    );
    for bad in [
        src.replace("class Shape", "class Shape(val n: Int)"),
        src.replace("open fun n() = 0", "constructor(i: Int)"),
        src.replace("open class", "interface"),
        format!("class Shape\n{src}"),
        format!("{src}\nfun incomplete("),
    ] {
        assert!(repair(&bad, code, "").is_none(), "{bad}");
    }
}

#[test]
fn compact_supertype_preserves_original_bytes_on_both_sides_of_the_edit() {
    let base = "// 🧭 }\r\nopen class Hull { val label = \"a }\"\t}\r\n";
    let derived = "class Yacht : /* { */ §Hull { fun weigh() { } }\r\n";
    for src in [format!("{base}{derived}"), format!("{derived}{base}")] {
        assert_eq!(
            proposal(&src, "SUPERTYPE_NOT_INITIALIZED", "").unwrap(),
            src.replace("§Hull", "Hull()")
        );
    }
}

#[test]
fn a_compact_brace_is_not_evidence_of_a_superclass_or_a_safe_constructor() {
    let src = "open class Lens { val radius = 3 }\nclass Prime : §Lens { fun d() = 2 }\n";
    let code = "SUPERTYPE_NOT_INITIALIZED";
    for bad in [
        src.replace("§Lens", "L§ens"),
        src.replace("§Lens", "§Lens()"),
        src.replace("§Lens", "§Lens, Other"),
        src.replace("class Prime", "interface Prime"),
        src.replace("val radius = 3", "constructor(i: Int)"),
        src.replace("fun d() = 2", "constructor() : super()"),
        format!("{src}typealias Lens = String\n"),
    ] {
        assert!(proposal(&bad, code, "").is_none(), "{bad}");
    }
    assert!(proposal(src, "UNRESOLVED_REFERENCE", "").is_none());
    // Even a supported rule with a matching target gets no general recovery.
    assert!(
        proposal(
            "class Lens { val radius = 3 }\nconst val LIMIT = §read()\n",
            "CONST_VAL_WITH_NON_CONST_INITIALIZER",
            "",
        )
        .is_none()
    );
}

#[test]
fn virtual_member_terminators_do_not_hide_other_syntax_errors() {
    let src = "open class Tool { val id = 0 }\nclass Saw : §Tool { val n = 1 }\n";
    for bad in [
        src.replace("id = 0 }", "id = 0 // }"),
        src.replace("id = 0 }", "id = \"}"),
        src.replace("id = 0 }", "id = 0 + }"),
        src.replace("n = 1 }", "n = 1 val m = 2 }"),
        format!("{src}fun unfinished("),
        format!("{src}}}"),
    ] {
        assert!(
            proposal(&bad, "SUPERTYPE_NOT_INITIALIZED", "").is_none(),
            "{bad}"
        );
    }
}

#[test]
fn lazy_val_refuses_possible_writes_and_shadowed_delegates() {
    let code = "DELEGATE_SPECIAL_FUNCTION_MISSING";
    let msg = "Type 'Lazy<Int>' has no method 'setValue(Example, KMutableProperty1<*, *>, Int)'.";
    let property = "var score §by lazy { 0 }";
    let src = format!("class Example {{\n {property}\n}}\n// score += 1");
    assert_eq!(
        repair(&src, code, msg).unwrap(),
        src.replace('§', "").replace("var score", "val score")
    );
    for stmt in [
        "this.score = 3",
        "score += 1",
        "this.score++",
        "--score",
        "this::score",
    ] {
        let src = format!("class Example {{\n {property}\n fun change() {{ {stmt} }}\n}}");
        assert!(repair(&src, code, msg).is_none(), "{src}");
    }
    assert!(repair(&format!("fun lazy(x: () -> Int) = x\n{src}"), code, msg).is_none());
    assert!(repair(&src, code, "missing getValue").is_none());
}

#[test]
fn primitive_array_replaces_only_the_terminal_method_name() {
    let code = "RETURN_TYPE_MISMATCH";
    let msg = "Return type mismatch: expected 'ByteArray', actual 'Array<Byte>'.";
    let src = "fun pack(data: List<Byte>): ByteArray = §data.toTypedArray /*keep*/ ()";
    assert_eq!(
        repair(src, code, msg).unwrap(),
        src.replace('§', "").replace("toTypedArray", "toByteArray")
    );
    for bad in [
        src.replace(": ByteArray", ": IntArray"),
        src.replace("§data.to", "data.§to"),
        src.replace("/*keep*/ ()", "().toList()"),
        format!("fun Any.toTypedArray() = 1\n{src}"),
    ] {
        assert!(repair(&bad, code, msg).is_none(), "{bad}");
    }
}

#[test]
fn primitive_array_requires_a_proven_collection_receiver() {
    let code = "RETURN_TYPE_MISMATCH";
    let msg = "Return type mismatch: expected 'IntArray', actual 'Array<Int>'.";
    let direct = "fun pack(xs: List<Int>): IntArray = §xs.toTypedArray()";
    let sorted = direct.replace("xs.toTyped", "xs.sorted().toTyped");
    for bad in [
        direct.replace("List<Int>", "IntArray"),
        direct.replace("List<Int>", "Custom<Int>"),
        direct.replace("List<Int>", "List<Long>"),
        direct.replace("xs: List", "vararg xs: List"),
        direct.replace("xs.toTyped", "xs.unknown().toTyped"),
        format!("typealias List<T> = Custom<T>\n{direct}"),
        format!("fun List<Int>.sorted() = intArrayOf()\n{sorted}"),
    ] {
        assert!(repair(&bad, code, msg).is_none(), "{bad}");
    }
    assert_eq!(
        repair(&sorted, code, msg).unwrap(),
        sorted
            .replace('§', "")
            .replace("toTypedArray", "toIntArray")
    );
}

#[test]
fn illegal_escape_refuses_a_regex_callable_bound_by_a_setter() {
    let code = "ILLEGAL_ESCAPE";
    let src = "var callback: (String) -> Unit = {}\n set(Regex) { Regex(\"§\\d\") }";
    for bad in [src.to_owned(), src.replace("set(Regex)", "set(`Regex`)")] {
        assert!(repair(&bad, code, "").is_none(), "{bad}");
    }
    let other = src.replace("set(Regex)", "set(f)");
    assert_eq!(repair(&other, code, "").unwrap(), other.replace('§', "\\"));
}

#[test]
fn illegal_escape_requires_the_exact_string_chunk_and_regex_argument() {
    let code = "ILLEGAL_ESCAPE";
    let src = r#"val pattern = Regex("🪴 §\s+")"#;
    assert_eq!(repair(src, code, "").unwrap(), src.replace('§', "\\"));
    for bad in [
        src.replace("Regex", "label"),
        src.replace("Regex", "custom.Regex"),
        src.replace("§\\s", "§\\n"),
        src.replace('§', "\\§"),
        format!("class Regex(val s: String)\n{src}"),
        format!("import other.Regex\n{src}"),
    ] {
        assert!(repair(&bad, code, "").is_none(), "{bad}");
    }
    let plain = src.replace('§', "");
    for offset in [plain.find('🪴').unwrap() + 1, plain.len(), usize::MAX] {
        assert!(
            propose(
                "kotlin",
                &plain,
                &json!({"code":code, "message":"", "offset":offset})
            )
            .is_none()
        );
    }
}
