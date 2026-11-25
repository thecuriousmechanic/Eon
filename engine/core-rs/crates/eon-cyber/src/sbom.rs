//! Software bill of materials and advisory matching (docs/implementation/03, supply-chain layer).
//!
//! Components are emitted as CycloneDX 1.5 JSON. Advisories are read from OSV-format JSON that is already on disk,
//! so matching works offline. An exact listed version is `Affected`. An unlisted version is `Unverified`
//! if the affected package gives unevaluated ranges or no version information at all.

use eon_primitives::{EonError, EonResult};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Component {
    /// OSV ecosystem name, for example `crates.io`, `PyPI`, or `npm`.
    pub ecosystem: String,
    pub name: String,
    pub version: String,
}

impl Component {
    /// Package URL. The purl type follows the ecosystem name, so `crates.io` becomes `cargo`.
    pub fn purl(&self) -> String {
        format!("pkg:{}/{}@{}", purl_type(&self.ecosystem), self.name, self.version)
    }
}

fn purl_type(ecosystem: &str) -> String {
    match ecosystem {
        "crates.io" => "cargo".to_owned(),
        "PyPI" => "pypi".to_owned(),
        "npm" => "npm".to_owned(),
        "Go" => "golang".to_owned(),
        other => other.to_lowercase(),
    }
}

/// Builds a CycloneDX 1.5 JSON document listing the components.
pub fn to_cyclonedx(components: &[Component]) -> Value {
    let listed: Vec<Value> = components
        .iter()
        .map(|c| {
            json!({
                "type": "library",
                "name": c.name,
                "version": c.version,
                "purl": c.purl(),
            })
        })
        .collect();
    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "components": listed,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Affected {
    pub ecosystem: String,
    pub name: String,
    /// Explicit affected versions, if any.
    pub versions: Vec<String>,
    /// An unlisted version still needs range evaluation (also true for mixed versions and ranges).
    pub ranges_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advisory {
    pub id: String,
    pub summary: String,
    pub affected: Vec<Affected>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The component's exact version is listed as affected.
    Affected,
    /// No exact match, but the advisory gives ranges or lacks version information for this package.
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub purl: String,
    pub advisory: String,
    pub verdict: Verdict,
}

fn invalid(reason: impl std::fmt::Display) -> EonError {
    EonError::ValidationError(format!("invalid OSV advisory: {reason}"))
}

fn text<'a>(value: &'a Value, key: &str) -> EonResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing string field '{key}'")))
}

fn optional_array<'a>(value: &'a Value, key: &str) -> EonResult<&'a [Value]> {
    match value.get(key) {
        None => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(invalid(format!("'{key}' must be an array"))),
    }
}

fn version_string(value: &Value) -> EonResult<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.trim() == *s)
        .ok_or_else(|| invalid("expected a nonempty version string without surrounding whitespace"))
}

fn validate_range(range: &Value) -> EonResult<()> {
    let kind = text(range, "type")?;
    if !matches!(kind, "ECOSYSTEM" | "SEMVER" | "GIT") {
        return Err(invalid("unknown range type"));
    }
    if (kind == "GIT" || range.get("repo").is_some()) && text(range, "repo")?.trim().is_empty() {
        return Err(invalid("empty range repo"));
    }
    let events = range
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("range 'events' must be an array"))?;
    for event in events {
        let fields = event
            .as_object()
            .filter(|f| f.len() == 1)
            .ok_or_else(|| invalid("expected a single range event field"))?;
        let version = ["introduced", "fixed", "last_affected", "limit"]
            .iter()
            .find_map(|key| fields.get(*key))
            .ok_or_else(|| invalid("unknown range event"))?;
        version_string(version)?;
    }
    Ok(())
}

/// Parses an OSV advisory document or an array of them.
pub fn parse_osv(source: &str) -> EonResult<Vec<Advisory>> {
    let root: Value = serde_json::from_str(source).map_err(invalid)?;
    let records = match &root {
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![root.clone()],
        _ => return Err(invalid("expected an object or an array")),
    };
    records.iter().map(parse_record).collect()
}

fn parse_record(record: &Value) -> EonResult<Advisory> {
    let id = text(record, "id")?.to_owned();
    let summary = record.get("summary").and_then(Value::as_str).unwrap_or_default().to_owned();
    let affected = optional_array(record, "affected")?
        .iter()
        .map(|entry| {
            let package = entry.get("package").ok_or_else(|| invalid(format!("{id}: affected entry without package")))?;
            let versions = optional_array(entry, "versions")?
                .iter()
                .map(|v| version_string(v).map(str::to_owned))
                .collect::<EonResult<Vec<_>>>()?;
            let ranges = optional_array(entry, "ranges")?;
            for range in ranges {
                validate_range(range)?;
            }
            Ok(Affected {
                ecosystem: text(package, "ecosystem")?.to_owned(),
                name: text(package, "name")?.to_owned(),
                ranges_only: !ranges.is_empty(),
                versions,
            })
        })
        .collect::<EonResult<Vec<_>>>()?;
    Ok(Advisory { id, summary, affected })
}

/// Matches components against advisories. A matching package entry is skipped only if its nonempty list
/// of versions excludes the component and it has no ranges. An exact match takes precedence.
pub fn match_advisories(components: &[Component], advisories: &[Advisory]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut positions = HashMap::new();
    for component in components {
        for advisory in advisories {
            for affected in &advisory.affected {
                if affected.ecosystem != component.ecosystem || affected.name != component.name {
                    continue;
                }
                let listed = affected.versions.iter().any(|v| v == &component.version);
                let versions_only = !affected.versions.is_empty() && !affected.ranges_only;
                if versions_only && !listed {
                    continue;
                }
                let index = positions.entry((component, advisory.id.as_str())).or_insert_with(|| {
                    let index = findings.len();
                    findings.push(Finding {
                        purl: component.purl(),
                        advisory: advisory.id.clone(),
                        verdict: Verdict::Unverified,
                    });
                    index
                });
                if listed {
                    findings[*index].verdict = Verdict::Affected;
                    break;
                }
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(ecosystem: &str, name: &str, version: &str) -> Component {
        Component {
            ecosystem: ecosystem.into(),
            name: name.into(),
            version: version.into(),
        }
    }

    const ADVISORIES: &str = r#"[
        {"id":"OSV-EXACT","summary":"bad release","affected":[
            {"package":{"ecosystem":"crates.io","name":"serde"},"versions":["1.0.0"]}]},
        {"id":"OSV-RANGE","summary":"range only","affected":[
            {"package":{"ecosystem":"crates.io","name":"tokio"},"ranges":[{"type":"SEMVER","events":[]}]}]}
    ]"#;

    #[test]
    fn cyclonedx_document_lists_purls_in_the_expected_shape() {
        let bom = to_cyclonedx(&[component("crates.io", "serde", "1.0.0")]);
        assert_eq!(bom["bomFormat"], "CycloneDX");
        assert_eq!(bom["specVersion"], "1.5");
        assert_eq!(bom["components"][0]["purl"], "pkg:cargo/serde@1.0.0");
    }

    #[test]
    fn exact_listed_version_is_affected_and_other_versions_are_clean() {
        let advisories = parse_osv(ADVISORIES).unwrap();
        let findings = match_advisories(&[component("crates.io", "serde", "1.0.0")], &advisories);
        assert!(findings.iter().any(|f| f.advisory == "OSV-EXACT" && f.verdict == Verdict::Affected));

        let clean = match_advisories(&[component("crates.io", "serde", "1.0.1")], &advisories);
        assert!(clean.iter().all(|f| f.advisory != "OSV-EXACT"));
    }

    #[test]
    fn range_only_advisories_are_unverified_not_clean() {
        let advisories = parse_osv(ADVISORIES).unwrap();
        let findings = match_advisories(&[component("crates.io", "tokio", "1.40.0")], &advisories);
        assert_eq!(
            findings,
            vec![Finding {
                purl: "pkg:cargo/tokio@1.40.0".into(),
                advisory: "OSV-RANGE".into(),
                verdict: Verdict::Unverified,
            }]
        );
    }

    #[test]
    fn mixed_coverage_keeps_unlisted_versions_unverified_only_for_the_same_package() {
        let advisories = parse_osv(
            r#"{"id":"LOCAL-QUILT","affected":[
                {"package":{"ecosystem":"npm","name":"sample-quilt"},"versions":["1.0.0"],
                 "ranges":[{"type":"SEMVER","events":[{"introduced":"2.0.0"},{"fixed":"2.1.0"}]}]},
                {"package":{"ecosystem":"npm","name":"sample-spool"},"versions":["1.0.0"],"ranges":[]}
            ]}"#,
        ).unwrap();
        let findings = match_advisories(
            &[
                component("npm", "sample-quilt", "1.0.0"),
                component("npm", "sample-quilt", "2.0.3"),
                component("npm", "sample-spool", "2.0.3"),
                component("PyPI", "sample-quilt", "2.0.3"),
            ],
            &advisories,
        );
        assert_eq!(
            findings,
            vec![
                Finding {
                    purl: "pkg:npm/sample-quilt@1.0.0".into(),
                    advisory: "LOCAL-QUILT".into(),
                    verdict: Verdict::Affected,
                },
                Finding {
                    purl: "pkg:npm/sample-quilt@2.0.3".into(),
                    advisory: "LOCAL-QUILT".into(),
                    verdict: Verdict::Unverified,
                },
            ]
        );
    }

    #[test]
    fn repeated_entries_records_and_components_yield_one_strongest_finding() {
        let mut advisories = parse_osv(
            r#"[
                {"id":"LOCAL-LOOM","affected":[
                    {"package":{"ecosystem":"npm","name":"sample-loom"},
                     "ranges":[{"type":"SEMVER","events":[{"introduced":"0"}]}]},
                    {"package":{"ecosystem":"npm","name":"sample-loom"},"versions":["1.0.0"]}]},
                {"id":"LOCAL-LOOM","affected":[
                    {"package":{"ecosystem":"npm","name":"sample-loom"},
                     "ranges":[{"type":"SEMVER","events":[{"introduced":"1.0.0"}]}]}]}
            ]"#,
        ).unwrap();
        let c = component("npm", "sample-loom", "1.0.0");
        let components = [c.clone(), c];
        let expected = vec![Finding {
            purl: "pkg:npm/sample-loom@1.0.0".into(),
            advisory: "LOCAL-LOOM".into(),
            verdict: Verdict::Affected,
        }];
        assert_eq!(match_advisories(&components, &advisories), expected);
        advisories.reverse();
        assert_eq!(match_advisories(&components, &advisories), expected);
    }

    #[test]
    fn missing_or_empty_coverage_is_unverified_only_for_the_matching_ecosystem() {
        let components = [component("npm", "sample-weave", "1.0.0"), component("PyPI", "sample-weave", "1.0.0")];
        for mut entry in [json!({}), json!({"versions":[]}), json!({"ranges":[]}), json!({"versions":[],"ranges":[]})] {
            entry["package"] = json!({"ecosystem":"npm","name":"sample-weave"});
            let record = json!({"id":"LOCAL-WEAVE","affected":[entry]});
            let advisories = parse_osv(&record.to_string()).unwrap();
            assert_eq!(
                match_advisories(&components, &advisories),
                vec![Finding {
                    purl: "pkg:npm/sample-weave@1.0.0".into(),
                    advisory: "LOCAL-WEAVE".into(),
                    verdict: Verdict::Unverified,
                }],
                "{record}"
            );
        }
    }

    #[test]
    fn malformed_coverage_is_rejected_not_discarded() {
        assert!(parse_osv(r#"{"id":"LOCAL-DYE","affected":{}}"#).is_err());
        for (field, value) in [
            ("versions", json!(null)),
            ("versions", json!("1.0.0")),
            ("versions", json!(["1.0.0", 2])),
            ("versions", json!([""])),
            ("versions", json!([" "])),
            ("ranges", json!(null)),
            ("ranges", json!({})),
            ("ranges", json!([false])),
            ("ranges", json!([{}])),
            ("ranges", json!([{"type":"SEMVER","events":{}}])),
            ("ranges", json!([{"type":"SEMVER","events":[{"introduced":1}]}])),
        ] {
            let mut record = json!({"id":"LOCAL-DYE","affected":[{
                "package":{"ecosystem":"npm","name":"sample-dye"}, "versions":["1.0.0"]
            }]});
            record["affected"][0][field] = value;
            assert!(parse_osv(&record.to_string()).is_err(), "accepted {record}");
        }
    }

    #[test]
    fn malformed_advisories_are_refused() {
        assert!(parse_osv("not json").is_err());
        assert!(parse_osv(r#"{"summary":"no id"}"#).is_err());
        assert!(parse_osv(r#"[{"id":"X","affected":[{"versions":["1"]}]}]"#).is_err());
    }
}
