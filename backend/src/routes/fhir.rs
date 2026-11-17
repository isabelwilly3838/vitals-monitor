//! FHIR R4B RESTful endpoints (subset): metadata, Patient, Device, Observation, AuditEvent.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use futures_util::StreamExt;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use super::client_ip;
use crate::error::{ApiError, FHIR_JSON};
use crate::fhir::builders as b;
use crate::fhir::codes as c;
use crate::fhir::validation::{is_fhir_id, parse_instant, validate_observation};
use crate::security::auth::{Authenticated, Perm};
use crate::services::state::AppState;
use crate::services::store::{ObservationQuery, StoreError};

static DATE_PARAM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(eq|ge|le|gt|lt)?(\d{4}-\d{2}-\d{2}(T[\d:.]+(Z|[+-]\d{2}:\d{2}))?)$").unwrap()
});

fn fhir(body: Value) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(FHIR_JSON)
        .body(body.to_string())
}

fn base_url(req: &HttpRequest) -> String {
    let ci = req.connection_info();
    format!("{}://{}/fhir", ci.scheme(), ci.host())
}

fn self_link(req: &HttpRequest) -> String {
    let ci = req.connection_info();
    format!("{}://{}{}", ci.scheme(), ci.host(), req.uri())
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Chaos | StoreError::Db(_) => {
                tracing::warn!(error = %e, "storage unavailable");
                ApiError::Unavailable("Storage temporarily unavailable, retry shortly".into())
            }
            other => ApiError::Unexpected(anyhow::anyhow!(other)),
        }
    }
}

// ------------------------------------------------------------------ metadata

pub async fn capability_statement() -> HttpResponse {
    let search = json!([
        {"name": "patient", "type": "reference"}, {"name": "code", "type": "token"},
        {"name": "date", "type": "date"}, {"name": "_count", "type": "number"}]);
    fhir(json!({
        "resourceType": "CapabilityStatement",
        "status": "active",
        "date": "2026-11-17",
        "kind": "instance",
        "fhirVersion": "4.3.0",
        "format": ["application/fhir+json"],
        "implementation": {"description": "Vitals Monitor FHIR facade (Rust / Actix Web)"},
        "rest": [{
            "mode": "server",
            "security": {"cors": false, "service": [{"coding": [{
                "system": "http://terminology.hl7.org/CodeSystem/restful-security-service",
                "code": "OAuth", "display": "OAuth"}]}],
                "description": "Bearer JWT from POST /auth/token; RBAC roles admin, clinician, patient, device"},
            "resource": [
                {"type": "Patient", "interaction": [{"code": "read"}, {"code": "search-type"}]},
                {"type": "Device", "interaction": [{"code": "read"}]},
                {"type": "Observation", "supportedProfile": [c::PROFILE_BODYTEMP, c::PROFILE_BP],
                 "interaction": [{"code": "read"}, {"code": "search-type"}, {"code": "create"}],
                 "searchParam": search,
                 "operation": [{"name": "validate", "definition": "http://hl7.org/fhir/OperationDefinition/Resource-validate"}]},
                {"type": "AuditEvent", "interaction": [{"code": "search-type"}]}
            ]
        }]
    }))
}

// ------------------------------------------------------------------ Patient / Device

#[tracing::instrument(skip_all)]
pub async fn search_patients(
    req: HttpRequest,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::ReadPatient)?;
    let rows: Vec<Value> = state
        .store
        .store
        .list_type("Patient")
        .await?
        .into_iter()
        .filter(|r| r["id"].as_str().is_some_and(|id| p.can_access_patient(id)))
        .collect();
    state
        .audit(
            "search",
            true,
            Some(&p),
            &client_ip(&req),
            None,
            Some(req.query_string()),
            "search-type",
        )
        .await;
    let total = rows.len() as i64;
    Ok(fhir(b::bundle_searchset(
        rows,
        &base_url(&req),
        &self_link(&req),
        total,
    )))
}

#[tracing::instrument(skip(req, state, p))]
pub async fn read_patient(
    req: HttpRequest,
    path: web::Path<String>,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::ReadPatient)?;
    let id = path.into_inner();
    if !is_fhir_id(&id) {
        return Err(ApiError::invalid("value", "Invalid id", "Patient.id"));
    }
    let entity = format!("Patient/{id}");
    if !p.can_access_patient(&id) {
        state
            .audit(
                "read",
                false,
                Some(&p),
                &client_ip(&req),
                Some(&entity),
                None,
                "read",
            )
            .await;
        return Err(ApiError::Forbidden(
            "Not permitted to access this patient".into(),
        ));
    }
    let res = state
        .store
        .store
        .get("Patient", &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Patient not found".into()))?;
    state
        .audit(
            "read",
            true,
            Some(&p),
            &client_ip(&req),
            Some(&entity),
            None,
            "read",
        )
        .await;
    Ok(fhir(res))
}

pub async fn read_device(
    path: web::Path<String>,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::ReadPatient)?;
    let id = path.into_inner();
    if !is_fhir_id(&id) {
        return Err(ApiError::invalid("value", "Invalid id", "Device.id"));
    }
    let res = state
        .store
        .store
        .get("Device", &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Device not found".into()))?;
    let owner = res["patient"]["reference"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches("Patient/");
    if !p.can_access_patient(owner) {
        return Err(ApiError::Forbidden(
            "Not permitted to access this device".into(),
        ));
    }
    Ok(fhir(res))
}

// ------------------------------------------------------------------ Observation

/// Parse repeated FHIR `date` params into an inclusive [ge, le] range.
/// Inclusive search window derived from FHIR `date` parameters.
pub type DateRange = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

pub fn parse_dates(values: &[String]) -> Result<DateRange, ApiError> {
    let bad = || {
        ApiError::invalid(
            "value",
            "date must be [eq|ge|le|gt|lt]YYYY-MM-DD[Thh:mm:ss[.sss]Z]",
            "date",
        )
    };
    let (mut ge, mut le) = (None, None);
    for v in values {
        let m = DATE_PARAM.captures(v).ok_or_else(bad)?;
        let prefix = m.get(1).map_or("eq", |p| p.as_str());
        let raw = &m[2];
        let (start, end) = if raw.contains('T') {
            let t = parse_instant(raw).ok_or_else(bad)?;
            (t, t)
        } else {
            let d = NaiveDate::parse_from_str(raw, "%Y-%m-%d").map_err(|_| bad())?;
            let s = d.and_hms_opt(0, 0, 0).unwrap().and_utc();
            (
                s,
                s + chrono::Duration::days(1) - chrono::Duration::milliseconds(1),
            )
        };
        if matches!(prefix, "ge" | "gt" | "eq") {
            ge = Some(start);
        }
        if matches!(prefix, "le" | "lt" | "eq") {
            le = Some(end);
        }
    }
    Ok((ge, le))
}

#[derive(Debug, Deserialize)]
pub struct ObsSearch {
    patient: Option<String>,
    code: Option<String>,
    #[serde(rename = "_count")]
    count: Option<i64>,
}

#[tracing::instrument(skip(req, state, p))]
pub async fn search_observations(
    req: HttpRequest,
    q: web::Query<ObsSearch>,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::ReadObs)?;
    // `date` may repeat (date=ge..&date=le..): read all occurrences from the raw query.
    let dates: Vec<String> = web::Query::<Vec<(String, String)>>::from_query(req.query_string())
        .map(|kv| {
            kv.into_inner()
                .into_iter()
                .filter(|(k, _)| k == "date")
                .map(|(_, v)| v)
                .collect()
        })
        .unwrap_or_default();
    if dates.len() > 4 {
        return Err(ApiError::invalid(
            "too-costly",
            "At most 4 date parameters",
            "date",
        ));
    }
    let count = q.count.unwrap_or(100);
    if !(1..=2000).contains(&count) {
        return Err(ApiError::invalid(
            "value",
            "_count must be between 1 and 2000",
            "_count",
        ));
    }
    let mut pid = match &q.patient {
        Some(raw) => {
            let id = raw.trim_start_matches("Patient/");
            if !is_fhir_id(id) {
                return Err(ApiError::invalid("value", "Invalid patient id", "patient"));
            }
            Some(id.to_string())
        }
        None => None,
    };
    if pid.is_none() && !p.sees_all_patients() {
        pid = p.patient_id.clone(); // patients are implicitly scoped to themselves
    }
    let ip = client_ip(&req);
    if let Some(id) = &pid
        && !p.can_access_patient(id)
    {
        state
            .audit(
                "search",
                false,
                Some(&p),
                &ip,
                None,
                Some(req.query_string()),
                "search-type",
            )
            .await;
        return Err(ApiError::Forbidden(
            "Not permitted to access this patient".into(),
        ));
    }
    let code = match &q.code {
        Some(raw) => {
            let code = raw.rsplit('|').next().unwrap_or("");
            if code != c::CODE_BODY_TEMP && code != c::CODE_BP_PANEL {
                return Err(ApiError::invalid(
                    "not-supported",
                    "Unsupported code",
                    "code",
                ));
            }
            Some(code.to_string())
        }
        None => None,
    };
    let (date_ge, date_le) = parse_dates(&dates)?;
    let query = ObservationQuery {
        patient_id: pid.clone(),
        code,
        date_ge,
        date_le,
        limit: count,
    };
    let (rows, total) = state.store.store.search_observations(&query).await?;
    let entity = pid.map(|id| format!("Patient/{id}"));
    state
        .audit(
            "search",
            true,
            Some(&p),
            &ip,
            entity.as_deref(),
            Some(req.query_string()),
            "search-type",
        )
        .await;
    Ok(fhir(b::bundle_searchset(
        rows,
        &base_url(&req),
        &self_link(&req),
        total,
    )))
}

#[tracing::instrument(skip(req, state, p))]
pub async fn read_observation(
    req: HttpRequest,
    path: web::Path<String>,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::ReadObs)?;
    let id = path.into_inner();
    if !is_fhir_id(&id) {
        return Err(ApiError::invalid("value", "Invalid id", "Observation.id"));
    }
    let res = state
        .store
        .store
        .get("Observation", &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Observation not found".into()))?;
    let pid = res["subject"]["reference"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches("Patient/");
    if !p.can_access_patient(pid) {
        return Err(ApiError::Forbidden(
            "Not permitted to access this observation".into(),
        ));
    }
    state
        .audit(
            "read",
            true,
            Some(&p),
            &client_ip(&req),
            Some(&format!("Observation/{id}")),
            None,
            "read",
        )
        .await;
    Ok(fhir(res))
}

/// Read a JSON body with a hard size limit and content-type check.
async fn json_body(
    req: &HttpRequest,
    mut payload: web::Payload,
    limit: usize,
) -> Result<Value, ApiError> {
    let ctype = req
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let ctype = ctype.split(';').next().unwrap_or("").trim();
    if ctype != FHIR_JSON && ctype != "application/json" {
        return Err(ApiError::invalid(
            "not-supported",
            "Content-Type must be application/fhir+json",
            "header",
        ));
    }
    if let Some(len) = req
        .headers()
        .get("content-length")
        .and_then(|h| h.to_str().ok()?.parse::<usize>().ok())
        && len > limit
    {
        return Err(ApiError::PayloadTooLarge);
    }
    let mut body = web::BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk =
            chunk.map_err(|_| ApiError::BadRequest("Could not read request body".into()))?;
        if body.len() + chunk.len() > limit {
            return Err(ApiError::PayloadTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| ApiError::invalid("structure", "Body is not valid JSON", "body"))
}

#[tracing::instrument(name = "create_observation", skip_all)]
pub async fn create_observation(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::WriteObs)?;
    let data = json_body(&req, payload, state.settings.application.max_body_bytes).await?;
    let ip = client_ip(&req);
    let created = match state.ingest_observation(&data).await {
        Ok(c) => c,
        Err(e) => {
            state
                .audit("create", false, Some(&p), &ip, None, None, "create")
                .await;
            return Err(e);
        }
    };
    let id = created["id"].as_str().unwrap_or_default().to_string();
    state
        .audit(
            "create",
            true,
            Some(&p),
            &ip,
            Some(&format!("Observation/{id}")),
            None,
            "create",
        )
        .await;
    Ok(HttpResponse::Created()
        .content_type(FHIR_JSON)
        .insert_header(("Location", format!("{}/Observation/{id}", base_url(&req))))
        .body(created.to_string()))
}

pub async fn validate_only(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::WriteObs)?;
    let data = json_body(&req, payload, state.settings.application.max_body_bytes).await?;
    let mut issues = validate_observation(&data, Some(state.known_patients()));
    if issues.is_empty() {
        issues.push(b::Issue {
            severity: "information",
            code: "informational",
            diagnostics: "Validation successful".into(),
            expression: None,
        });
    }
    Ok(fhir(b::operation_outcome(&issues)))
}

// ------------------------------------------------------------------ AuditEvent

#[derive(Deserialize)]
pub struct CountOnly {
    #[serde(rename = "_count")]
    count: Option<i64>,
}

pub async fn search_audit(
    req: HttpRequest,
    q: web::Query<CountOnly>,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::ReadAudit)?;
    let n = q.count.unwrap_or(100).clamp(1, 1000);
    let rows = state.store.store.list_audit(n).await?;
    let total = rows.len() as i64;
    Ok(fhir(b::bundle_searchset(
        rows,
        &base_url(&req),
        &self_link(&req),
        total,
    )))
}
