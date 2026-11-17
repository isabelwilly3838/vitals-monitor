//! Token authentication + role-based access control.
//!
//! Roles
//!   admin      everything, incl. audit log, key rotation, chaos switch
//!   clinician  read all patients, stream, ingest observations
//!   patient    read *own* data only (token carries patient_id)
//!   device     ingest observations only (sensor gateway)

use std::collections::HashMap;
use std::future::{Ready, ready};

use actix_web::{FromRequest, HttpMessage, HttpRequest, dev::Payload, web};
use chrono::Utc;
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::configuration::{Environment, SecuritySettings};
use crate::error::ApiError;
use crate::services::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Clinician,
    Patient,
    Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Perm {
    ReadPatient,
    ReadObs,
    WriteObs,
    ReadAudit,
    AdminKeys,
    Stream,
}

impl Perm {
    pub fn name(&self) -> &'static str {
        match self {
            Perm::ReadPatient => "patient:read",
            Perm::ReadObs => "observation:read",
            Perm::WriteObs => "observation:write",
            Perm::ReadAudit => "audit:read",
            Perm::AdminKeys => "keys:rotate",
            Perm::Stream => "stream:subscribe",
        }
    }
}

impl Role {
    pub fn grants(&self, p: Perm) -> bool {
        use Perm::*;
        match self {
            Role::Admin => true,
            Role::Clinician => matches!(p, ReadPatient | ReadObs | WriteObs | Stream),
            Role::Patient => matches!(p, ReadPatient | ReadObs | Stream),
            Role::Device => matches!(p, WriteObs),
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Clinician => "clinician",
            Role::Patient => "patient",
            Role::Device => "device",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub username: String,
    pub roles: Vec<Role>,
    pub patient_id: Option<String>,
}

impl Principal {
    pub fn has(&self, p: Perm) -> bool {
        self.roles.iter().any(|r| r.grants(p))
    }

    pub fn require(&self, p: Perm) -> Result<(), ApiError> {
        if self.has(p) {
            Ok(())
        } else {
            Err(ApiError::Forbidden(format!(
                "Role lacks permission '{}'",
                p.name()
            )))
        }
    }

    /// Clinicians/admins see everyone; patients only themselves; devices nobody.
    pub fn can_access_patient(&self, patient_id: &str) -> bool {
        if self
            .roles
            .iter()
            .any(|r| matches!(r, Role::Admin | Role::Clinician))
        {
            return true;
        }
        self.roles.contains(&Role::Patient) && self.patient_id.as_deref() == Some(patient_id)
    }

    pub fn sees_all_patients(&self) -> bool {
        self.roles
            .iter()
            .any(|r| matches!(r, Role::Admin | Role::Clinician))
    }

    pub fn role_names(&self) -> Vec<String> {
        self.roles.iter().map(|r| r.as_str().to_string()).collect()
    }
}

// ------------------------------------------------------------------ users

#[derive(Debug, Clone, Deserialize)]
pub struct UserRecord {
    pub username: String,
    pub password_hash: String,
    pub roles: Vec<Role>,
    pub patient_id: Option<String>,
}

/// Development/test accounts. Never loaded in prod (config validation requires users_json).
pub const DEV_USERS: [(&str, &str, Role, Option<&str>); 4] = [
    ("admin", "admin-dev-pass", Role::Admin, None),
    ("clinician", "clinician-dev-pass", Role::Clinician, None),
    ("patient1", "patient-dev-pass", Role::Patient, Some("P001")),
    ("gateway", "device-dev-pass", Role::Device, None),
];

pub struct UserStore {
    users: HashMap<String, UserRecord>,
    dummy_hash: String,
}

impl UserStore {
    pub fn new(sec: &SecuritySettings, env: Environment) -> anyhow::Result<Self> {
        let mut users = HashMap::new();
        let json = sec.users_json.expose_secret();
        if !json.is_empty() {
            let list: Vec<UserRecord> = serde_json::from_str(json)?;
            for u in list {
                users.insert(u.username.clone(), u);
            }
        } else if env != Environment::Prod {
            let cost = if env == Environment::Test { 4 } else { 10 };
            for (name, pw, role, pid) in DEV_USERS {
                users.insert(
                    name.to_string(),
                    UserRecord {
                        username: name.into(),
                        password_hash: bcrypt::hash(pw, cost)?,
                        roles: vec![role],
                        patient_id: pid.map(String::from),
                    },
                );
            }
        }
        Ok(Self {
            users,
            dummy_hash: bcrypt::hash("dummy", 4)?,
        })
    }

    /// Runs bcrypt even for unknown users so response timing does not reveal valid names.
    pub fn authenticate(&self, username: &str, password: &SecretString) -> Option<Principal> {
        let pw = password.expose_secret();
        match self.users.get(username) {
            None => {
                let _ = bcrypt::verify(pw, &self.dummy_hash);
                None
            }
            Some(u) => bcrypt::verify(pw, &u.password_hash)
                .ok()
                .filter(|ok| *ok)
                .map(|_| Principal {
                    username: u.username.clone(),
                    roles: u.roles.clone(),
                    patient_id: u.patient_id.clone(),
                }),
        }
    }
}

// ------------------------------------------------------------------ JWT

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    roles: Vec<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    patient_id: Option<String>,
    iat: i64,
    nbf: i64,
    exp: i64,
    iss: String,
    jti: String,
}

pub fn kid(secret: &str) -> String {
    hex::encode(&Sha256::digest(secret.as_bytes())[..6])
}

pub fn issue_token(p: &Principal, sec: &SecuritySettings) -> Result<(String, i64), ApiError> {
    let secrets = sec.jwt_secret_list();
    let secret = secrets
        .first()
        .ok_or_else(|| anyhow::anyhow!("no JWT secret configured"))?;
    let now = Utc::now().timestamp();
    let ttl = sec.jwt_ttl_minutes * 60;
    let claims = Claims {
        sub: p.username.clone(),
        roles: p.roles.clone(),
        patient_id: p.patient_id.clone(),
        iat: now,
        nbf: now,
        exp: now + ttl,
        iss: sec.jwt_issuer.clone(),
        jti: uuid::Uuid::new_v4().simple().to_string(),
    };
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(kid(secret));
    let token = encode(
        &header,
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| anyhow::anyhow!("token encoding failed: {e}"))?;
    Ok((token, ttl))
}

pub fn decode_token(token: &str, sec: &SecuritySettings) -> Result<Principal, ApiError> {
    let header = decode_header(token).map_err(|_| ApiError::Auth("Invalid token".into()))?;
    let all = sec.jwt_secret_list();
    let matching: Vec<&String> = all.iter().filter(|s| Some(kid(s)) == header.kid).collect();
    let candidates = if matching.is_empty() {
        all.iter().collect()
    } else {
        matching
    };

    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&[&sec.jwt_issuer]);
    validation.set_required_spec_claims(&["exp", "iat", "sub", "iss"]);
    validation.leeway = 5;

    for secret in candidates {
        match decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &validation,
        ) {
            Ok(data) => {
                let c = data.claims;
                return Ok(Principal {
                    username: c.sub,
                    roles: c.roles,
                    patient_id: c.patient_id,
                });
            }
            Err(e) if matches!(e.kind(), jsonwebtoken::errors::ErrorKind::ExpiredSignature) => {
                return Err(ApiError::Auth("Token expired".into()));
            }
            Err(_) => continue,
        }
    }
    Err(ApiError::Auth("Invalid token".into()))
}

/// Extractor: `Authenticated(principal)` - rejects with 401 OperationOutcome if missing/invalid.
/// Also records the user on the request's root tracing span.
pub struct Authenticated(pub Principal);

impl FromRequest for Authenticated {
    type Error = ApiError;
    type Future = Ready<Result<Self, ApiError>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(authenticate_request(req))
    }
}

fn authenticate_request(req: &HttpRequest) -> Result<Authenticated, ApiError> {
    let state = req
        .app_data::<web::Data<AppState>>()
        .ok_or_else(|| anyhow::anyhow!("app state missing"))?;
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::Auth("Missing bearer token".into()))?;
    let p = decode_token(token, &state.settings.security)?;
    if let Some(span) = req.extensions().get::<tracing_actix_web::RootSpan>() {
        span.record("user", tracing::field::display(&p.username));
    }
    Ok(Authenticated(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    fn sec(secrets: &str) -> SecuritySettings {
        SecuritySettings {
            jwt_secrets: SecretString::from(secrets.to_string()),
            jwt_ttl_minutes: 15,
            jwt_issuer: "vitals-monitor".into(),
            encryption_keys: SecretString::from(String::new()),
            users_json: SecretString::from(String::new()),
            login_rate_per_minute: 10,
        }
    }

    fn patient() -> Principal {
        Principal {
            username: "u".into(),
            roles: vec![Role::Patient],
            patient_id: Some("P001".into()),
        }
    }

    #[test]
    fn jwt_roundtrip_keeps_claims() {
        let s = sec(&"a".repeat(40));
        let (tok, ttl) = issue_token(&patient(), &s).unwrap();
        assert_eq!(decode_token(&tok, &s).unwrap(), patient());
        assert_eq!(ttl, 900);
    }

    #[test]
    fn jwt_secret_rotation() {
        let (old, new) = ("o".repeat(40), "n".repeat(40));
        let (tok, _) = issue_token(&patient(), &sec(&old)).unwrap();
        assert!(decode_token(&tok, &sec(&format!("{new},{old}"))).is_ok());
        assert!(decode_token(&tok, &sec(&new)).is_err());
    }

    #[test]
    fn expired_and_forged_tokens_rejected() {
        let s = sec(&"a".repeat(40));
        let claims = Claims {
            sub: "u".into(),
            roles: vec![],
            patient_id: None,
            iat: 0,
            nbf: 0,
            exp: 10,
            iss: "vitals-monitor".into(),
            jti: "x".into(),
        };
        let tok = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&[b'a'; 40]),
        )
        .unwrap();
        assert!(matches!(decode_token(&tok, &s), Err(ApiError::Auth(m)) if m.contains("expired")));
        // alg=none token
        let none = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiJ1IiwiZXhwIjo0MTAyNDQ0ODAwfQ.";
        assert!(decode_token(none, &s).is_err());
        assert!(decode_token("garbage", &s).is_err());
    }

    #[test]
    fn rbac_matrix() {
        let clin = Principal {
            username: "c".into(),
            roles: vec![Role::Clinician],
            patient_id: None,
        };
        let dev = Principal {
            username: "d".into(),
            roles: vec![Role::Device],
            patient_id: None,
        };
        assert!(clin.can_access_patient("P002") && !patient().can_access_patient("P002"));
        assert!(patient().can_access_patient("P001") && !dev.can_access_patient("P001"));
        assert!(dev.has(Perm::WriteObs) && !dev.has(Perm::ReadObs));
        assert!(!clin.has(Perm::ReadAudit));
    }

    #[test]
    fn dev_users_authenticate_and_prod_has_none() {
        let store = UserStore::new(&sec("x"), Environment::Test).unwrap();
        let ok = store.authenticate(
            "clinician",
            &SecretString::from("clinician-dev-pass".to_string()),
        );
        assert_eq!(ok.unwrap().roles, vec![Role::Clinician]);
        assert!(
            store
                .authenticate("clinician", &SecretString::from("nope".to_string()))
                .is_none()
        );
        assert!(
            store
                .authenticate("ghost", &SecretString::from("x".to_string()))
                .is_none()
        );
        let prod = UserStore::new(&sec("x"), Environment::Prod).unwrap();
        assert!(
            prod.authenticate("admin", &SecretString::from("admin-dev-pass".to_string()))
                .is_none()
        );
    }
}
