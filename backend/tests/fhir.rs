//! Unit tests: FHIR builders (schema validity, golden-file regression) and the 3-layer
//! validator (core + edge cases).

use std::collections::HashSet;

use chrono::{Duration, TimeZone, Utc};
use serde_json::{Value, json};
use vitals_monitor::fhir::builders::{self as b, AuditInput, validate_schema};
use vitals_monitor::fhir::validation::validate_observation;

fn known() -> HashSet<String> {
    ["P001", "P002", "P003"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn codes(v: &Value) -> HashSet<&'static str> {
    validate_observation(v, Some(&known()))
        .into_iter()
        .map(|i| i.code)
        .collect()
}

fn temp(v: f64) -> Value {
    b::body_temperature("t1", "P001", "D001", Utc::now(), v)
}

fn bp(s: f64, d: f64) -> Value {
    b::blood_pressure("bp1", "P001", "D001", Utc::now(), s, d)
}

// ------------------------------------------------------------------ builders

#[test]
fn every_built_resource_is_schema_valid() {
    let roles = vec!["admin".to_string()];
    let resources = [
        temp(37.2),
        bp(130.0, 85.0),
        b::patient("P001", "STUDY-1", "female", 1970),
        b::device("D001", "P001", "x"),
        b::operation_outcome(&[b::Issue::error("invalid", "x", Some("Observation.id"))]),
        b::audit_event(&AuditInput {
            action: "read",
            ok: true,
            user: "u",
            roles: &roles,
            client_ip: "::1",
            entity_ref: Some("Patient/P001"),
            query: Some("a=b"),
            subtype: "read",
        }),
        b::bundle_searchset(
            vec![temp(37.0)],
            "http://h/fhir",
            "http://h/fhir/Observation",
            1,
        ),
    ];
    for r in resources {
        assert_eq!(
            validate_schema(&r),
            vec![],
            "{} failed schema validation",
            r["resourceType"]
        );
    }
}

#[test]
fn bundle_wraps_entries_with_full_urls() {
    let bundle = b::bundle_searchset(
        vec![temp(37.0)],
        "http://h/fhir",
        "http://h/fhir/Observation",
        7,
    );
    assert_eq!(bundle["total"], 7);
    assert_eq!(
        bundle["entry"][0]["fullUrl"],
        "http://h/fhir/Observation/t1"
    );
}

#[test]
fn temperature_interpretation_boundaries() {
    for (t, want) in [(37.0, "N"), (38.0, "H"), (34.9, "L"), (35.0, "N")] {
        assert_eq!(
            temp(t)["interpretation"][0]["coding"][0]["code"],
            want,
            "t={t}"
        );
    }
}

#[test]
fn bp_interpretation_takes_worst_component() {
    let o = bp(120.0, 95.0);
    assert_eq!(o["interpretation"][0]["coding"][0]["code"], "H");
    assert_eq!(
        o["component"][1]["interpretation"][0]["coding"][0]["code"],
        "H"
    );
}

#[test]
fn login_audit_uses_dicom_type_and_omits_empty_roles() {
    let ev = b::audit_event(&AuditInput {
        action: "login",
        ok: false,
        user: "x",
        roles: &[],
        client_ip: "1",
        entity_ref: None,
        query: None,
        subtype: "login",
    });
    assert_eq!(ev["type"]["code"], "110114");
    assert_eq!(ev["outcome"], "4");
    assert!(ev["agent"][0].get("role").is_none());
}

// ------------------------------------------------------------------ golden-file regression

fn normalise(mut v: Value) -> Value {
    v["meta"].as_object_mut().unwrap().remove("lastUpdated");
    v.as_object_mut().unwrap().remove("issued");
    v
}

/// The golden files were produced by the original Python backend: this proves the Rust
/// port emits *identical* FHIR. Regenerate deliberately with `UPDATE_GOLDEN=1 cargo test`.
#[test]
fn output_matches_golden_files() {
    let ts = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    for (name, got) in [
        (
            "bodytemp",
            normalise(b::body_temperature("P001-t-1", "P001", "D001", ts, 38.4)),
        ),
        (
            "bp",
            normalise(b::blood_pressure(
                "P001-bp-1",
                "P001",
                "D001",
                ts,
                142.5,
                91.0,
            )),
        ),
    ] {
        let path = dir.join(format!("{name}.json"));
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(&path, serde_json::to_string_pretty(&got).unwrap() + "\n").unwrap();
        }
        let want: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(got, want, "{name} drifted from golden file");
    }
}

// ------------------------------------------------------------------ validator: happy paths

#[test]
fn valid_resources_pass() {
    assert_eq!(codes(&temp(37.0)), HashSet::new());
    assert_eq!(codes(&bp(120.0, 80.0)), HashSet::new());
    let mut o = temp(37.0);
    o["effectiveDateTime"] = json!("2026-01-01T10:00:00+02:00");
    assert_eq!(codes(&o), HashSet::new());
}

#[test]
fn plausible_range_is_inclusive() {
    assert!(codes(&temp(25.0)).is_empty() && codes(&temp(45.0)).is_empty());
}

// ------------------------------------------------------------------ validator: edge cases

#[test]
fn non_observation_bodies_rejected() {
    for body in [
        Value::Null,
        json!([]),
        json!("x"),
        json!(1),
        json!({"resourceType": "Patient"}),
    ] {
        assert_eq!(codes(&body), HashSet::from(["structure"]), "{body}");
    }
}

#[test]
fn unknown_element_rejected() {
    let mut o = temp(37.0);
    o["evil"] = json!(1);
    assert!(codes(&o).contains("structure"));
}

#[test]
fn wrong_type_rejected() {
    let mut o = temp(37.0);
    o["valueQuantity"]["value"] = json!("hot");
    assert!(codes(&o).contains("structure"));
}

#[test]
fn invalid_status_rejected() {
    let mut o = temp(37.0);
    o["status"] = json!("bogus");
    assert!(!codes(&o).is_empty());
}

#[test]
fn missing_vital_signs_category() {
    let mut o = temp(37.0);
    o["category"][0]["coding"][0]["code"] = json!("laboratory");
    assert!(codes(&o).contains("required"));
}

#[test]
fn unknown_patient_and_non_patient_subject() {
    let o = b::body_temperature("t", "P999", "D", Utc::now(), 37.0);
    assert!(codes(&o).contains("not-found"));
    let mut o = temp(37.0);
    o["subject"]["reference"] = json!("Group/1");
    assert!(codes(&o).contains("required"));
}

#[test]
fn timestamps() {
    let mut o = temp(37.0);
    o["effectiveDateTime"] = json!(b::iso(Utc::now() + Duration::hours(1)));
    assert!(codes(&o).contains("value"), "future");
    o["effectiveDateTime"] = json!("2026-01-01");
    assert!(!codes(&o).is_empty(), "no time/zone");
    o["effectiveDateTime"] = json!("notadate");
    assert!(!codes(&o).is_empty(), "garbage");
}

#[test]
fn implausible_temperatures_rejected() {
    for v in [24.9, 45.1, -1.0, 1e9] {
        assert!(codes(&temp(v)).contains("value"), "{v}");
    }
}

#[test]
fn wrong_unit_and_missing_value() {
    let mut o = temp(37.0);
    o["valueQuantity"]["code"] = json!("[degF]");
    o["valueQuantity"]["unit"] = json!("[degF]");
    assert!(codes(&o).contains("code-invalid"));
    let mut o = temp(37.0);
    o.as_object_mut().unwrap().remove("valueQuantity");
    assert!(codes(&o).contains("required"));
}

#[test]
fn blood_pressure_rules() {
    assert!(
        codes(&bp(80.0, 90.0)).contains("value"),
        "sys must exceed dia"
    );
    let mut o = bp(120.0, 80.0);
    o["component"].as_array_mut().unwrap().truncate(1);
    assert!(codes(&o).contains("required"));
}

#[test]
fn unsupported_loinc_code() {
    let mut o = temp(37.0);
    o["code"]["coding"][0]["code"] = json!("8867-4"); // heart rate
    assert!(codes(&o).contains("not-supported"));
}

#[test]
fn xss_and_control_characters_rejected() {
    for payload in [
        "<script>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "javascript:alert(1)",
        "x\u{0}y",
        "<svg/onload=alert(1)>",
    ] {
        let mut o = temp(37.0);
        o["code"]["text"] = json!(payload);
        assert!(codes(&o).contains("security"), "{payload:?}");
    }
}

#[test]
fn sql_injection_string_is_just_data() {
    let mut o = temp(37.0);
    o["subject"]["reference"] = json!("Patient/P001' OR '1'='1");
    assert!(codes(&o).contains("not-found"));
}

#[test]
fn deeply_nested_input_does_not_crash() {
    let mut deep = json!({});
    for _ in 0..50 {
        deep = json!({"x": deep});
    }
    let mut o = temp(37.0);
    o["code"]["extension"] = json!([deep]);
    assert!(!codes(&o).is_empty());
}
