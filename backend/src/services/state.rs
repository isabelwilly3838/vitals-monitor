//! Application state + the ingest pipeline shared by the simulator and
//! `POST /fhir/Observation`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::anomaly::AlertEngine;
use super::resilience::CircuitBreaker;
use super::simulator::{DEMO_PATIENTS, VitalsSimulator};
use super::store::{PgStore, ResilientStore, Row_};
use super::stream::{Hub, Sample};
use crate::configuration::Settings;
use crate::error::ApiError;
use crate::fhir::builders::{self as b, AuditInput};
use crate::fhir::codes as c;
use crate::fhir::validation::{parse_instant, validate_observation};
use crate::security::auth::{Principal, UserStore};
use crate::security::middleware::RateLimiter;

pub struct AppState {
    pub settings: Settings,
    pub store: ResilientStore,
    pub hub: Hub,
    pub alerts: Mutex<AlertEngine>,
    pub users: UserStore,
    pub login_limiter: RateLimiter,
    pub patient_ids: Vec<String>,
    known: HashSet<String>,
    seq: Mutex<HashMap<String, u32>>,
}

impl AppState {
    pub fn new(settings: Settings, store: Arc<PgStore>) -> anyhow::Result<Self> {
        let r = &settings.resilience;
        let breaker = CircuitBreaker::new(
            "storage",
            r.breaker_failure_threshold,
            Duration::from_millis(r.breaker_reset_timeout_ms),
        );
        let patient_ids: Vec<String> = DEMO_PATIENTS
            .iter()
            .map(|p| p.patient_id.to_string())
            .collect();
        Ok(Self {
            store: ResilientStore::new(
                store,
                breaker,
                r.store_retry_attempts,
                r.fallback_buffer_size,
            ),
            hub: Hub::default(),
            alerts: Mutex::new(AlertEngine::default()),
            users: UserStore::new(&settings.security, settings.environment)?,
            login_limiter: RateLimiter::new(settings.security.login_rate_per_minute),
            known: patient_ids.iter().cloned().collect(),
            patient_ids,
            seq: Mutex::new(HashMap::new()),
            settings,
        })
    }

    pub fn known_patients(&self) -> &HashSet<String> {
        &self.known
    }

    /// Idempotent: (re)write the pseudonymised demo Patients and their Devices.
    pub async fn seed_reference_data(&self) -> anyhow::Result<()> {
        let rows: Vec<Row_> = DEMO_PATIENTS
            .iter()
            .flat_map(|p| {
                [
                    Row_ {
                        resource: b::patient(p.patient_id, p.pseudonym, p.gender, p.birth_year),
                        patient_id: Some(p.patient_id.into()),
                        code: None,
                        effective: None,
                    },
                    Row_ {
                        resource: b::device(
                            p.device_id,
                            p.patient_id,
                            "VitalPatch BT-2 (simulated)",
                        ),
                        patient_id: Some(p.patient_id.into()),
                        code: None,
                        effective: None,
                    },
                ]
            })
            .collect();
        self.store.store.put_many(&rows).await?;
        Ok(())
    }

    /// Build FHIR resources, validate, persist (resiliently) and publish to the stream.
    #[tracing::instrument(name = "ingest_sample", skip(self, temp, sys, dia), fields(seq))]
    pub async fn ingest_sample(
        &self,
        patient: &str,
        ts: DateTime<Utc>,
        temp: Option<f64>,
        sys: Option<f64>,
        dia: Option<f64>,
        device: &str,
    ) -> Result<Vec<Value>, ApiError> {
        let idx = self
            .patient_ids
            .iter()
            .position(|p| p == patient)
            .ok_or_else(|| ApiError::NotFound("Unknown patient".into()))?;
        let ts = b::truncate_ms(ts);
        let seq = {
            let mut m = self.seq.lock().unwrap();
            let s = m.entry(patient.to_string()).or_insert(0);
            *s += 1;
            *s
        };
        tracing::Span::current().record("seq", seq);

        let mut resources = vec![];
        if let Some(t) = temp {
            resources.push(b::body_temperature(
                &format!("{patient}-t-{seq}"),
                patient,
                device,
                ts,
                t,
            ));
        }
        if let (Some(s), Some(d)) = (sys, dia) {
            resources.push(b::blood_pressure(
                &format!("{patient}-bp-{seq}"),
                patient,
                device,
                ts,
                s,
                d,
            ));
        }
        // Every emitted resource is validated, including our own.
        for r in &resources {
            let issues = validate_observation(r, Some(&self.known));
            if !issues.is_empty() {
                tracing::error!(
                    id = r["id"].as_str(),
                    issues = issues.len(),
                    "emitted resource invalid"
                );
                return Err(ApiError::Validation(issues));
            }
        }
        for r in &resources {
            let code = r["code"]["coding"][0]["code"].as_str().map(String::from);
            self.store
                .save(Row_ {
                    resource: r.clone(),
                    patient_id: Some(patient.into()),
                    code,
                    effective: Some(ts),
                })
                .await;
        }

        let alerts = match (temp, sys, dia) {
            (Some(t), Some(s), Some(d)) => self.alerts.lock().unwrap().evaluate(patient, t, s, d),
            _ => 0,
        };
        self.hub.publish(Sample {
            patient_id: patient.into(),
            patient_index: idx as u8,
            seq,
            ts_ms: ts.timestamp_millis() as f64,
            temp_c: temp.map_or(f32::NAN, |v| v as f32),
            systolic: sys.map_or(f32::NAN, |v| v as f32),
            diastolic: dia.map_or(f32::NAN, |v| v as f32),
            alerts,
        });
        Ok(resources)
    }

    /// Validate an inbound FHIR Observation and push it through the same pipeline.
    /// The stored resource is the canonical re-built form (normalised ids/units/interpretation).
    pub async fn ingest_observation(&self, data: &Value) -> Result<Value, ApiError> {
        let issues = validate_observation(data, Some(&self.known));
        if !issues.is_empty() {
            return Err(ApiError::Validation(issues));
        }
        let pid = data["subject"]["reference"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches("Patient/");
        let ts = data["effectiveDateTime"]
            .as_str()
            .and_then(parse_instant)
            .unwrap_or_else(Utc::now);
        let device = data["device"]["reference"]
            .as_str()
            .and_then(|r| r.rsplit('/').next())
            .unwrap_or("unknown");
        let device = if crate::fhir::validation::is_fhir_id(device) {
            device
        } else {
            "unknown"
        };
        let has = |code: &str| {
            data["code"]["coding"].as_array().is_some_and(|a| {
                a.iter()
                    .any(|cd| cd["system"] == c::LOINC && cd["code"] == code)
            })
        };
        let out = if has(c::CODE_BODY_TEMP) {
            self.ingest_sample(
                pid,
                ts,
                data["valueQuantity"]["value"].as_f64(),
                None,
                None,
                device,
            )
            .await?
        } else {
            let comp = |code: &str| {
                data["component"].as_array().and_then(|a| {
                    a.iter()
                        .find(|cp| {
                            cp["code"]["coding"]
                                .as_array()
                                .is_some_and(|cc| cc.iter().any(|x| x["code"] == code))
                        })
                        .and_then(|cp| cp["valueQuantity"]["value"].as_f64())
                })
            };
            self.ingest_sample(
                pid,
                ts,
                None,
                comp(c::CODE_SYSTOLIC),
                comp(c::CODE_DIASTOLIC),
                device,
            )
            .await?
        };
        out.into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("nothing ingested").into())
    }

    /// Simulator loop: one tick per period for every demo patient. Runs under `supervise`.
    pub async fn run_simulator(self: Arc<Self>) -> anyhow::Result<()> {
        let s = &self.settings.simulator;
        let mut sims: Vec<VitalsSimulator> = DEMO_PATIENTS
            .iter()
            .enumerate()
            .map(|(i, p)| VitalsSimulator::new(p.clone(), s.hz, s.seed.map(|x| x + i as u64)))
            .collect();
        let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / s.hz));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::info!(patients = sims.len(), hz = s.hz, "simulator started");
        loop {
            tick.tick().await;
            let now = Utc::now();
            for sim in sims.iter_mut() {
                let (t, sy, d) = sim.step();
                let p = &sim.profile;
                self.ingest_sample(p.patient_id, now, Some(t), Some(sy), Some(d), p.device_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("ingest failed: {e}"))?;
            }
        }
    }

    pub async fn drain_forever(self: Arc<Self>) -> anyhow::Result<()> {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            self.store.drain().await;
        }
    }

    /// Record a FHIR AuditEvent. Audit failure is logged loudly but never fails the request.
    #[allow(clippy::too_many_arguments)] // mirrors the AuditEvent fields 1:1
    pub async fn audit(
        &self,
        action: &str,
        ok: bool,
        who: Option<&Principal>,
        client_ip: &str,
        entity_ref: Option<&str>,
        query: Option<&str>,
        subtype: &str,
    ) {
        let roles = who.map(|p| p.role_names()).unwrap_or_default();
        let user = who.map(|p| p.username.as_str()).unwrap_or("anonymous");
        let ev = b::audit_event(&AuditInput {
            action,
            ok,
            user,
            roles: &roles,
            client_ip,
            entity_ref,
            query,
            subtype,
        });
        if let Err(e) = self.store.store.put_audit(&ev).await {
            tracing::error!(error = %e, "audit write failed");
        }
        tracing::info!(
            action,
            outcome = if ok { "success" } else { "failure" },
            entity = entity_ref,
            subtype,
            "audit"
        );
    }
}
