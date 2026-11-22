use super::{Adapter, AllocationCounters, Error, Result, Visit, Wire};
use serde_json::{Value, json};

const LOCALS: usize = 3; // visit, allocations, frees

fn handle(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .filter(|n| (1..=i32::MAX as u64).contains(n))
        .ok_or(Error::UnexpectedFixture)
}

fn local_scope(scopes: &Value, adapter: Adapter) -> Result<u64> {
    let names: &[&str] = match adapter {
        Adapter::LldbDap => &["Locals", "Globals", "Registers"],
        Adapter::CodeLldb => &["Local", "Static", "Global", "Registers"],
    };
    let scopes = scopes
        .as_array()
        .filter(|s| s.len() == names.len())
        .ok_or(Error::UnexpectedFixture)?;
    let mut ids = [0; 4];
    for (i, (scope, name)) in scopes.iter().zip(names).enumerate() {
        let id = handle(&scope["variablesReference"])?;
        if scope["name"] != *name || ids[..i].contains(&id) {
            return Err(Error::UnexpectedFixture);
        }
        ids[i] = id;
    }
    let local = &scopes[0];
    let expected = match adapter {
        Adapter::LldbDap => {
            ids[0] == 1
                && local["presentationHint"] == "locals"
                && local["namedVariables"] == LOCALS
        }
        Adapter::CodeLldb => {
            local.get("presentationHint").is_none_or(|h| h == "locals")
                && local.get("namedVariables").is_none_or(|n| n == LOCALS)
        }
    };
    if !expected
        || local["expensive"] != false
        || local.get("indexedVariables").is_some_and(|n| n != 0)
    {
        return Err(Error::UnexpectedFixture);
    }
    Ok(ids[0])
}

fn parse_counters(vars: &Value, visit: Visit) -> Result<AllocationCounters> {
    let rows = vars
        .as_array()
        .filter(|rows| rows.len() == LOCALS)
        .ok_or(Error::UnexpectedFixture)?;
    // Exactly three distinct names must be present. Neither adapter metadata
    // nor even a scalar's textual representation is returned to the caller.
    let scalar = |name: &str, ty: &str| {
        let row = rows
            .iter()
            .find(|r| r["name"] == name)
            .ok_or(Error::UnexpectedFixture)?;
        if row["type"] != ty
            || row["variablesReference"] != 0
            || row.get("namedVariables").is_some_and(|n| n != 0)
            || row.get("indexedVariables").is_some_and(|n| n != 0)
        {
            return Err(Error::UnexpectedFixture);
        }
        let raw = row["value"].as_str().ok_or(Error::UnexpectedFixture)?;
        match raw.as_bytes() {
            [n @ b'0'..=b'3'] => Ok(n - b'0'),
            _ => Err(Error::UnexpectedFixture),
        }
    };
    let expected = match visit {
        Visit::Second => 2,
        Visit::Third => 3,
    };
    let allocations = scalar("allocations", "unsigned int")?;
    let frees = scalar("frees", "unsigned int")?;
    if scalar("visit", "int")? != expected || allocations != expected || frees != expected - 1 {
        return Err(Error::UnexpectedFixture);
    }
    Ok(AllocationCounters { allocations, frees })
}

/// Called only after all three source frames and the hit ID have matched.
/// One local scope of the top frame, one page, no child expansion or evaluation.
pub(super) fn observe(
    wire: &mut Wire,
    adapter: Adapter,
    visit: Visit,
    frames: &Value,
) -> Result<AllocationCounters> {
    let top = handle(&frames[0]["id"])?;
    let second = handle(&frames[1]["id"])?;
    let third = handle(&frames[2]["id"])?;
    if top == second || top == third || second == third {
        return Err(Error::UnexpectedFixture);
    }
    let scopes = wire.call("scopes", json!({"frameId":top}))?;
    let locals = local_scope(&scopes["scopes"], adapter)?;
    let vars = wire.call(
        "variables",
        json!({"variablesReference":locals, "filter":"named", "start":0, "count":LOCALS}),
    )?;
    parse_counters(&vars["variables"], visit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(adapter: Adapter) -> Value {
        let names: &[&str] = match adapter {
            Adapter::LldbDap => &["Locals", "Globals", "Registers"],
            Adapter::CodeLldb => &["Local", "Static", "Global", "Registers"],
        };
        let mut rows: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, name)| json!({"name": name, "expensive":false, "variablesReference":i+1}))
            .collect();
        if adapter == Adapter::LldbDap {
            rows[0]["presentationHint"] = json!("locals");
            rows[0]["namedVariables"] = json!(3);
        }
        json!(rows)
    }

    #[test]
    fn only_a_unique_bounded_local_scope_can_be_requested() {
        for adapter in [Adapter::LldbDap, Adapter::CodeLldb] {
            let valid = scopes(adapter);
            assert_eq!(local_scope(&valid, adapter), Ok(1));
            for (field, bad) in [
                ("name", json!("Registers")),
                ("expensive", json!(true)),
                ("variablesReference", json!(2)),
                ("variablesReference", json!(0)),
                ("variablesReference", json!(2147483648u64)),
                ("variablesReference", json!("1")),
                ("namedVariables", json!(4)),
                ("indexedVariables", json!(1)),
                ("presentationHint", json!("globals")),
            ] {
                let mut wrong = valid.clone();
                wrong[0][field] = bad;
                assert_eq!(local_scope(&wrong, adapter), Err(Error::UnexpectedFixture));
            }
            let mut extra = valid.clone();
            extra.as_array_mut().unwrap().push(valid[0].clone());
            assert_eq!(local_scope(&extra, adapter), Err(Error::UnexpectedFixture));
        }
    }

    fn variables() -> Value {
        json!([
            {"name":"visit", "type":"int", "value":"2", "variablesReference":0},
            {"name":"allocations", "type":"unsigned int", "value":"2", "variablesReference":0,
             "evaluateName":"discarded", "memoryReference":"discarded"},
            {"name":"frees", "type":"unsigned int", "value":"1", "variablesReference":0}
        ])
    }

    #[test]
    fn counters_reject_wrong_visits_noncanonical_values_and_nonleaves() {
        let valid = variables();
        assert_eq!(
            parse_counters(&valid, Visit::Second),
            Ok(AllocationCounters {
                allocations: 2,
                frees: 1
            })
        );
        assert_eq!(
            parse_counters(&valid, Visit::Third),
            Err(Error::UnexpectedFixture)
        );
        for (field, bad) in [
            ("name", json!("frees")),
            ("name", json!("other")),
            ("type", json!("unsigned int *")),
            ("value", json!(2)),
            ("value", json!("+2")),
            ("value", json!("02")),
            ("value", json!("2 ")),
            ("value", json!("0x2")),
            ("value", json!("4")),
            ("value", json!("1")),
            ("value", json!("2".repeat(4096))),
            ("variablesReference", json!(1)),
            ("indexedVariables", json!(1)),
            ("namedVariables", json!("0")),
        ] {
            let mut wrong = valid.clone();
            wrong[1][field] = bad;
            assert_eq!(
                parse_counters(&wrong, Visit::Second),
                Err(Error::UnexpectedFixture)
            );
        }
        let mut rows = valid.as_array().unwrap().clone();
        rows.pop();
        assert_eq!(
            parse_counters(&json!(rows), Visit::Second),
            Err(Error::UnexpectedFixture)
        );
        rows.extend([valid[2].clone(), valid[2].clone()]);
        assert_eq!(
            parse_counters(&json!(rows), Visit::Second),
            Err(Error::UnexpectedFixture)
        );
    }

    #[test]
    fn malformed_wire_replies_never_trigger_a_follow_up_lookup() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};

        for bad_scope in [true, false] {
            let (mut peer, socket) = UnixStream::pair().unwrap();
            let mut local = scopes(Adapter::CodeLldb);
            let mut vars = variables();
            if bad_scope {
                local[0]["variablesReference"] = local[2]["variablesReference"].clone();
            } else {
                vars[1]["variablesReference"] = json!(10);
            }
            for (seq, cmd, body) in [
                (1, "scopes", json!({"scopes":local})),
                (2, "variables", json!({"variables":vars})),
            ] {
                let response = json!({"type":"response", "request_seq":seq, "command":cmd,
                    "success":true, "body":body})
                .to_string();
                write!(peer, "Content-Length: {}\r\n\r\n{response}", response.len()).unwrap();
                if bad_scope {
                    break;
                }
            }
            let mut wire = Wire::new(socket, Instant::now() + Duration::from_secs(1));
            assert_eq!(
                observe(
                    &mut wire,
                    Adapter::CodeLldb,
                    Visit::Second,
                    &json!([{"id":11},{"id":12},{"id":13}])
                ),
                Err(Error::UnexpectedFixture)
            );
            drop(wire);
            let mut requests = String::new();
            peer.read_to_string(&mut requests).unwrap();
            assert_eq!(
                requests.matches("Content-Length: ").count(),
                if bad_scope { 1 } else { 2 }
            );
            assert!(requests.contains(r#""command":"scopes""#));
            assert_eq!(requests.contains(r#""command":"variables""#), !bad_scope);
        }
    }
}
