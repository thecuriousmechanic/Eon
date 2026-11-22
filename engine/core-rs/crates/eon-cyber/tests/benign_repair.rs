use eon_cyber::benign_repair::propose;
use serde_json::json;

#[test]
fn situated_ts_diagnostic_not_a_function_name_lookup() {
    let src = "function days(later: Date, earlier: Date) { return later - earlier; }";
    let d = json!({"code": 2362, "message": "arithmetic operand", "offset": src.find("later -").unwrap()});
    let (rule, edit) = propose("typescript", src, &d).unwrap();
    assert_eq!(rule, "tsc:2362");
    assert!(edit.contains("later.getTime() - earlier"));
    // Same location, same syntax, but now the receiver is a number.
    let not_date = src.replace("later: Date", "later: number");
    let d = json!({"offset": not_date.find("later -").unwrap(), "code": 2362, "message": "arithmetic operand"});
    assert!(propose("typescript", &not_date, &d).is_none());
}

#[test]
fn arrow_body_requires_both_compiler_and_ast_evidence() {
    let src = "function scores(a: number[]): {n:number}[] { return a.map(x => { n: x * 2 }); }";
    let d = json!({"code": 2322, "offset": src.find("return").unwrap(), "message": "Type 'void[]' is not assignable"});
    let (_, edit) = propose("typescript", src, &d).unwrap();
    assert!(edit.contains("=> ({ n: x * 2 })"));
    let not_object = src.replace("n: x * 2", "console.log(x)");
    assert!(propose("typescript", &not_object, &d).is_none());
    assert!(
        propose(
            "typescript",
            src,
            &json!({"code": 2322, "offset": 0, "message": "other"})
        )
        .is_none()
    );
}

#[test]
fn competing_or_foreign_rustc_help_is_not_applied() {
    let src = "fn update() { let n = 1; n = 2; }";
    let at = src.find("n = 1").unwrap();
    let span = json!({"byte_start": at, "byte_end": at, "file_name": "subject.rs", "is_primary": true,
        "suggested_replacement": "mut ", "suggestion_applicability": "MachineApplicable"});
    let help = json!({"level":"help", "spans":[span]});
    let mut error = json!({"level":"error","code":{"code":"E0384"},"spans":[span],"children":[help]});
    assert!(propose("rust", src, &error).unwrap().1.contains("let mut n"));
    error["children"] = json!([help, help]);
    assert!(propose("rust", src, &error).is_none());
    error["children"] = json!([help]);
    error["children"][0]["spans"][0]["file_name"] = json!("foreign.rs");
    assert!(propose("rust", src, &error).is_none());
}

#[test]
fn rejects_unknown_dialects_offsets_and_oversized_input() {
    let d = json!({"code": 2551, "offset": usize::MAX, "message": "Did you mean 'x'?"});
    assert!(propose("typescript", "const n = 1;", &d).is_none());
    assert!(propose("typescript", &"x".repeat(8193), &d).is_none());
    assert!(propose("tsx", "const n = 1;", &d).is_none());
    assert!(propose("kotlin", "val n = 1", &d).is_none());
}
