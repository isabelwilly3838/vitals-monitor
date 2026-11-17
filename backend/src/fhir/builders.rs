//! Builders for the FHIR R4B resources emitted by the backend.
//!
//! Each builder returns a `serde_json::Value`; `validate_schema` round-trips any of them
//! through the typed `fhir-sdk` R4B models, so every emitted resource is schema-valid
//! (checked in tests for every resource type).

use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use super::codes as c;

pub fn iso(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Truncate to millisecond precision (what FHIR instants / the stream carry).
pub fn truncate_ms(ts: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ts.timestamp_millis()).unwrap_or(ts)
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn coding(system: &str, code: &str, display: &str) -> Value {
    json!({"system": system, "code": code, "display": display})
}

fn quantity(value: f64, unit: &str) -> Value {
    json!({"value": round2(value), "unit": unit, "system": c::UCUM, "code": unit})
}

fn interp(code: &str) -> Value {
    let text = match code {
        "H" => "High",
        "L" => "Low",
        _ => "Normal",
    };
    json!([{"coding": [coding(c::INTERPRETATION, code, text)]}])
}

pub fn interpret_temp(t: f64) -> &'static str {
    if t >= c::TEMP_HIGH {
        "H"
    } else if t < c::TEMP_LOW {
        "L"
    } else {
        "N"
    }
}
pub fn interpret_sys(s: f64) -> &'static str {
    if s >= c::SYS_HIGH {
        "H"
    } else if s < c::SYS_LOW {
        "L"
    } else {
        "N"
    }
}
pub fn interpret_dia(d: f64) -> &'static str {
    if d >= c::DIA_HIGH {
        "H"
    } else if d < c::DIA_LOW {
        "L"
    } else {
        "N"
    }
}

fn vital_category() -> Value {
    json!([{"coding": [coding(c::OBS_CATEGORY, "vital-signs", "Vital Signs")], "text": "Vital Signs"}])
}

fn base_obs(
    id: &str,
    patient: &str,
    device: &str,
    effective: DateTime<Utc>,
    profile: &str,
    code: &str,
) -> Value {
    let now = iso(Utc::now());
    json!({
        "resourceType": "Observation",
        "id": id,
        "meta": {"profile": [c::PROFILE_VITALSIGNS, profile], "lastUpdated": now},
        "status": "final",
        "category": vital_category(),
        "code": {"coding": [coding(c::LOINC, code, c::display(code))], "text": c::display(code)},
        "subject": {"reference": format!("Patient/{patient}")},
        "device": {"reference": format!("Device/{device}")},
        "effectiveDateTime": iso(effective),
        "issued": now,
    })
}

pub fn body_temperature(
    id: &str,
    patient: &str,
    device: &str,
    effective: DateTime<Utc>,
    temp_c: f64,
) -> Value {
    let mut o = base_obs(
        id,
        patient,
        device,
        effective,
        c::PROFILE_BODYTEMP,
        c::CODE_BODY_TEMP,
    );
    o["valueQuantity"] = quantity(temp_c, c::UNIT_CEL);
    o["interpretation"] = interp(interpret_temp(temp_c));
    o
}

pub fn blood_pressure(
    id: &str,
    patient: &str,
    device: &str,
    effective: DateTime<Utc>,
    sys: f64,
    dia: f64,
) -> Value {
    let mut o = base_obs(
        id,
        patient,
        device,
        effective,
        c::PROFILE_BP,
        c::CODE_BP_PANEL,
    );
    let (is, id_) = (interpret_sys(sys), interpret_dia(dia));
    o["component"] = json!([
        {"code": {"coding": [coding(c::LOINC, c::CODE_SYSTOLIC, c::display(c::CODE_SYSTOLIC))]},
         "valueQuantity": quantity(sys, c::UNIT_MMHG), "interpretation": interp(is)},
        {"code": {"coding": [coding(c::LOINC, c::CODE_DIASTOLIC, c::display(c::CODE_DIASTOLIC))]},
         "valueQuantity": quantity(dia, c::UNIT_MMHG), "interpretation": interp(id_)},
    ]);
    let worst = if is == "H" || id_ == "H" {
        "H"
    } else if is == "L" || id_ == "L" {
        "L"
    } else {
        "N"
    };
    o["interpretation"] = interp(worst);
    o
}

/// Pseudonymised demo patient: no real names; identifier is a study code.
pub fn patient(id: &str, pseudonym: &str, gender: &str, birth_year: i32) -> Value {
    json!({
        "resourceType": "Patient",
        "id": id,
        "meta": {"profile": ["http://hl7.org/fhir/StructureDefinition/Patient"]},
        "identifier": [{"system": "urn:vitals-monitor:study-id", "value": pseudonym}],
        "active": true,
        "gender": gender,
        "birthDate": birth_year.to_string(),
    })
}

pub fn device(id: &str, patient: &str, model: &str) -> Value {
    json!({
        "resourceType": "Device",
        "id": id,
        "status": "active",
        "deviceName": [{"name": model, "type": "model-name"}],
        "patient": {"reference": format!("Patient/{patient}")},
    })
}

pub fn bundle_searchset(entries: Vec<Value>, base_url: &str, self_link: &str, total: i64) -> Value {
    let entry: Vec<Value> = entries
        .into_iter()
        .map(|r| {
            let full = format!(
                "{base_url}/{}/{}",
                r["resourceType"].as_str().unwrap_or(""),
                r["id"].as_str().unwrap_or("")
            );
            json!({"fullUrl": full, "resource": r, "search": {"mode": "match"}})
        })
        .collect();
    json!({
        "resourceType": "Bundle",
        "id": uuid::Uuid::new_v4().to_string(),
        "type": "searchset",
        "timestamp": iso(Utc::now()),
        "total": total,
        "link": [{"relation": "self", "url": self_link}],
        "entry": entry,
    })
}

/// One OperationOutcome issue. `code` is a FHIR issue-type code.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Issue {
    pub severity: &'static str,
    pub code: &'static str,
    pub diagnostics: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
}

impl Issue {
    pub fn error(
        code: &'static str,
        diagnostics: impl Into<String>,
        expression: Option<&str>,
    ) -> Self {
        Self {
            severity: "error",
            code,
            diagnostics: diagnostics.into(),
            expression: expression.map(String::from),
        }
    }
}

pub fn operation_outcome(issues: &[Issue]) -> Value {
    let issue: Vec<Value> = issues
        .iter()
        .map(|i| {
            let mut v =
                json!({"severity": i.severity, "code": i.code, "diagnostics": i.diagnostics});
            if let Some(e) = &i.expression {
                v["expression"] = json!([e]);
            }
            v
        })
        .collect();
    json!({"resourceType": "OperationOutcome", "issue": issue})
}

pub struct AuditInput<'a> {
    pub action: &'a str,
    pub ok: bool,
    pub user: &'a str,
    pub roles: &'a [String],
    pub client_ip: &'a str,
    pub entity_ref: Option<&'a str>,
    pub query: Option<&'a str>,
    pub subtype: &'a str,
}

/// FHIR R4B AuditEvent (RESTful interaction or user authentication).
pub fn audit_event(a: &AuditInput) -> Value {
    let action = match a.action {
        "create" => "C",
        "read" => "R",
        "update" => "U",
        "delete" => "D",
        _ => "E",
    };
    let type_ = if a.action == "login" {
        coding(
            "http://dicom.nema.org/resources/ontology/DCM",
            "110114",
            "User Authentication",
        )
    } else {
        coding(
            "http://terminology.hl7.org/CodeSystem/audit-event-type",
            "rest",
            "RESTful Operation",
        )
    };
    let mut agent = json!({
        "type": {"coding": [coding("http://terminology.hl7.org/CodeSystem/v3-ParticipationType", "IRCP", "information recipient")]},
        "who": {"identifier": {"system": "urn:vitals-monitor:user", "value": a.user}},
        "requestor": true,
        "network": {"address": a.client_ip, "type": "2"},
    });
    if !a.roles.is_empty() {
        agent["role"] = Value::Array(a.roles.iter().map(|r| json!({"text": r})).collect());
    }
    let mut ev = json!({
        "resourceType": "AuditEvent",
        "id": uuid::Uuid::new_v4().to_string(),
        "type": type_,
        "subtype": [{"system": "http://hl7.org/fhir/restful-interaction", "code": a.subtype}],
        "action": action,
        "recorded": iso(Utc::now()),
        "outcome": if a.ok { "0" } else { "4" },
        "agent": [agent],
        "source": {"observer": {"display": "vitals-monitor-backend"},
                   "type": [coding("http://terminology.hl7.org/CodeSystem/security-source-type", "4", "Application Server")]},
    });
    if a.entity_ref.is_some() || a.query.is_some() {
        let mut entity = json!({});
        if let Some(r) = a.entity_ref {
            entity["what"] = json!({"reference": r});
        }
        if let Some(q) = a.query {
            entity["query"] = json!(base64::engine::general_purpose::STANDARD.encode(q));
        }
        ev["entity"] = json!([entity]);
    }
    ev
}

/// Structural FHIR R4B check: parse into the typed model, then re-serialise and make sure
/// no input element was silently dropped (fhir-sdk ignores unknown elements, FHIR forbids
/// them). Returns a list of human-readable problems.
pub fn validate_schema(resource: &Value) -> Vec<(String, String)> {
    use fhir_sdk::r4b::resources::{
        AuditEvent, Bundle, CapabilityStatement, Device, Observation, OperationOutcome, Patient,
    };
    fn check<T: serde::de::DeserializeOwned + serde::Serialize>(
        v: &Value,
    ) -> Vec<(String, String)> {
        match serde_json::from_value::<T>(v.clone()) {
            Err(e) => vec![(String::new(), e.to_string())],
            Ok(model) => {
                let back = serde_json::to_value(&model).unwrap_or(Value::Null);
                let mut out = vec![];
                unknown_elements(v, &back, "", &mut out);
                out
            }
        }
    }
    match resource["resourceType"].as_str() {
        Some("Observation") => check::<Observation>(resource),
        Some("Patient") => check::<Patient>(resource),
        Some("Device") => check::<Device>(resource),
        Some("Bundle") => check::<Bundle>(resource),
        Some("OperationOutcome") => check::<OperationOutcome>(resource),
        Some("AuditEvent") => check::<AuditEvent>(resource),
        Some("CapabilityStatement") => check::<CapabilityStatement>(resource),
        _ => vec![(
            "resourceType".into(),
            "unsupported or missing resourceType".into(),
        )],
    }
}

fn unknown_elements(input: &Value, parsed: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match (input, parsed) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in a {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                match b.get(k) {
                    None => out.push((p, format!("Unknown or invalid element '{k}'"))),
                    Some(w) => unknown_elements(v, w, &p, out),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for (i, v) in a.iter().enumerate() {
                if let Some(w) = b.get(i) {
                    unknown_elements(v, w, &format!("{path}[{i}]"), out);
                }
            }
        }
        _ => {}
    }
}
