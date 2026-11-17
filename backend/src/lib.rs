//! Vitals Monitor backend: real-time FHIR R4B vitals (body temperature + blood pressure).

pub mod configuration;
pub mod error;
pub mod fhir;
pub mod routes;
pub mod security;
pub mod services;
pub mod startup;
pub mod telemetry;
