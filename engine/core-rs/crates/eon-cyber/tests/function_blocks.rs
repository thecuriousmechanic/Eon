//! Function blocks carry each definition's name, start line, and exact source text.

use eon_cyber::cpg::{Lang, checked_top_level_function, function_blocks};

const BEFORE: &str = r#"
fn load(path: &str) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn save(path: &str, body: &str) {
    std::fs::write(path, body).unwrap();
}
"#;

#[test]
fn every_function_is_listed_with_its_name_line_and_exact_text() {
    let blocks = function_blocks(Lang::Rust, BEFORE).unwrap();
    let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, vec!["load", "save"]);
    assert_eq!(blocks[0].start_line, 2);
    assert_eq!(blocks[1].start_line, 6);
    assert!(blocks[1].text.starts_with("fn save("));
    assert!(blocks[1].text.trim_end().ends_with('}'));
}

#[test]
fn nested_functions_are_included() {
    let source = "fn outer() {\n    fn inner() {}\n}\n";
    let names: Vec<String> = function_blocks(Lang::Rust, source).unwrap().into_iter().map(|b| b.name).collect();
    assert_eq!(names, vec!["outer", "inner"]);
}

#[test]
fn python_classes_and_decorated_functions_count_toward_uniqueness_but_are_not_editable() {
    let ordinary = "def f(): pass\n";
    for conflict in [
        "@wrap\ndef f(): pass\n",
        "@wrap\n@other\nasync def f(): pass\n",
        "class f: pass\n",
        "@wrap\nclass f: pass\n",
    ] {
        for source in [conflict.to_owned(), format!("{ordinary}{conflict}"), format!("{conflict}{ordinary}")] {
            assert!(checked_top_level_function(Lang::Python, &source, "f").is_err(), "{source}");
        }
    }
}
