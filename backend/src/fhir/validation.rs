//! Three-layer validation for inbound Observations ("subscriber validation and rejection"
//! from the course, applied to sensor data):
//!
//! 1. Structural: FHIR R4B schema via typed `fhir-sdk` models (+ unknown-element check).
//! 2. Profile: Vital Signs profile - status binding, category, LOINC code, UCUM units,
//!    subject/effective present, no future timestamps.
//! 3. Plausibility: physiological ranges (sensor-fault detection), systolic > diastolic.
//!
//! Plus a security layer rejecting markup / control characters in any string (stored XSS).
//! Every failure becomes an OperationOutcome issue; nothing here panics on bad input.

use chrono::{DateTime, Duration, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;

use super::builders::{Issue, validate_schema};
use super::codes as c;

pub static FHIR_ID_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9\-.]{1,64}$").unwrap());
static MARKUP_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)<\s*/?\s*[a-z!?]|javascript:|on\w+\s*=").unwrap());
static CONTROL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap());
const MAX_DEPTH: usize = 12;

pub fn is_fhir_id(s: &str) -> bool {
    FHIR_ID_RE.is_match(s)
}

pub fn scan_strings(v: &Value, path: &str, depth: usize, out: &mut Vec<Issue>) {
    if depth > MAX_DEPTH {
        out.push(Issue::error(
            "too-costly",
            "Resource nesting too deep",
            Some(path),
        ));
        return;
    }
    match v {
        Value::String(s) => {
            if MARKUP_RE.is_match(s) {
                out.push(Issue::error(
                    "security",
                    "Markup or script content is not allowed",
                    Some(path),
                ));
            }
            if CONTROL_RE.is_match(s) {
                out.push(Issue::error(
                    "security",
                    "Control characters are not allowed",
                    Some(path),
                ));
            }
        }
        Value::Object(m) => {
            for (k, v) in m {
                scan_strings(v, &format!("{path}.{k}"), depth + 1, out);
                if !out.is_empty() && out.last().map(|i| i.code) == Some("too-costly") {
                    return;
                }
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                scan_strings(v, &format!("{path}[{i}]"), depth + 1, out);
            }
        }
        _ => {}
    }
}

fn codes(concept: &Value, system: &str) -> HashSet<String> {
    concept["coding"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|cd| cd["system"] == system)
                .filter_map(|cd| cd["code"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn check_quantity(
    q: &Value,
    unit: &str,
    (lo, hi): (f64, f64),
    expr: &str,
    label: &str,
    out: &mut Vec<Issue>,
) -> Option<f64> {
    if !q.is_object() {
        out.push(Issue::error(
            "required",
            format!("{label} valueQuantity is required"),
            Some(expr),
        ));
        return None;
    }
    if q["system"] != c::UCUM || q["code"] != unit {
        out.push(Issue::error(
            "code-invalid",
            format!("{label} must use UCUM unit '{unit}'"),
            Some(expr),
        ));
    }
    let Some(v) = q["value"].as_f64() else {
        out.push(Issue::error(
            "value",
            format!("{label} value must be numeric"),
            Some(expr),
        ));
        return None;
    };
    if !(lo..=hi).contains(&v) {
        out.push(Issue::error(
            "value",
            format!("{label} {v} outside plausible range [{lo}, {hi}]"),
            Some(expr),
        ));
    }
    Some(v)
}

/// RFC 3339 instant with explicit offset (FHIR dateTime with time component).
pub fn parse_instant(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Validate an inbound Observation. Empty result == valid.
pub fn validate_observation(data: &Value, known_patients: Option<&HashSet<String>>) -> Vec<Issue> {
    if !data.is_object() {
        return vec![Issue::error(
            "structure",
            "Body must be a JSON object",
            None,
        )];
    }
    if data["resourceType"] != "Observation" {
        return vec![Issue::error(
            "structure",
            "resourceType must be 'Observation'",
            Some("resourceType"),
        )];
    }
    let mut issues = vec![];
    scan_strings(data, "Observation", 0, &mut issues);
    if !issues.is_empty() {
        return issues;
    }

    // --- 1. structural
    let schema = validate_schema(data);
    if !schema.is_empty() {
        return schema
            .into_iter()
            .take(20)
            .map(|(loc, msg)| {
                let expr = (!loc.is_empty()).then(|| format!("Observation.{loc}"));
                Issue::error("structure", truncate(&msg, 200), expr.as_deref())
            })
            .collect();
    }

    // --- 2. profile
    if let Some(id) = data.get("id")
        && !id.as_str().is_some_and(is_fhir_id)
    {
        issues.push(Issue::error(
            "value",
            "id does not match FHIR id pattern",
            Some("Observation.id"),
        ));
    }
    if !data["status"]
        .as_str()
        .is_some_and(|s| c::OBS_STATUS.contains(&s))
    {
        issues.push(Issue::error(
            "code-invalid",
            "status is not in the required value set",
            Some("Observation.status"),
        ));
    }
    let has_vital = data["category"].as_array().is_some_and(|cats| {
        cats.iter()
            .any(|cat| codes(cat, c::OBS_CATEGORY).contains("vital-signs"))
    });
    if !has_vital {
        issues.push(Issue::error(
            "required",
            "category must include 'vital-signs'",
            Some("Observation.category"),
        ));
    }
    let subject = data["subject"]["reference"].as_str().unwrap_or("");
    match subject.strip_prefix("Patient/") {
        None => issues.push(Issue::error(
            "required",
            "subject must reference a Patient",
            Some("Observation.subject"),
        )),
        Some(pid) => {
            if known_patients.is_some_and(|k| !k.contains(pid)) {
                issues.push(Issue::error(
                    "not-found",
                    "subject references an unknown Patient",
                    Some("Observation.subject"),
                ));
            }
        }
    }
    match data["effectiveDateTime"].as_str().and_then(parse_instant) {
        None => issues.push(Issue::error(
            "required",
            "effectiveDateTime with timezone is required",
            Some("Observation.effectiveDateTime"),
        )),
        Some(eff) if eff > Utc::now() + Duration::minutes(5) => issues.push(Issue::error(
            "value",
            "effectiveDateTime is in the future",
            Some("Observation.effectiveDateTime"),
        )),
        _ => {}
    }

    // --- 3. code-specific + plausibility
    let loinc = codes(&data["code"], c::LOINC);
    if loinc.contains(c::CODE_BODY_TEMP) {
        check_quantity(
            &data["valueQuantity"],
            c::UNIT_CEL,
            c::TEMP_RANGE,
            "Observation.valueQuantity",
            "Body temperature",
            &mut issues,
        );
    } else if loinc.contains(c::CODE_BP_PANEL) {
        let comps = data["component"].as_array().cloned().unwrap_or_default();
        let find = |code: &str| {
            comps
                .iter()
                .enumerate()
                .find(|(_, cp)| codes(&cp["code"], c::LOINC).contains(code))
        };
        let mut vals = [None, None];
        for (slot, (code, range, label)) in [
            (c::CODE_SYSTOLIC, c::SYS_RANGE, "Systolic"),
            (c::CODE_DIASTOLIC, c::DIA_RANGE, "Diastolic"),
        ]
        .into_iter()
        .enumerate()
        {
            match find(code) {
                None => issues.push(Issue::error(
                    "required",
                    format!("{label} component ({code}) is required"),
                    Some("Observation.component"),
                )),
                Some((idx, cp)) => {
                    let expr = format!("Observation.component[{idx}].valueQuantity");
                    vals[slot] = check_quantity(
                        &cp["valueQuantity"],
                        c::UNIT_MMHG,
                        range,
                        &expr,
                        label,
                        &mut issues,
                    );
                }
            }
        }
        if let [Some(s), Some(d)] = vals
            && s <= d
        {
            issues.push(Issue::error(
                "value",
                "Systolic must be greater than diastolic",
                Some("Observation.component"),
            ));
        }
    } else {
        issues.push(Issue::error(
            "not-supported",
            format!(
                "Unsupported code; supported LOINC: {}, {}",
                c::CODE_BODY_TEMP,
                c::CODE_BP_PANEL
            ),
            Some("Observation.code"),
        ));
    }
    issues
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
