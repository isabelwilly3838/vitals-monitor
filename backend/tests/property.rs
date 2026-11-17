//! Property-based & fuzz tests (proptest).

use std::collections::HashSet;

use chrono::Utc;
use proptest::prelude::*;
use serde_json::{Value, json};
use vitals_monitor::fhir::builders as b;
use vitals_monitor::fhir::validation::validate_observation;
use vitals_monitor::services::anomaly::threshold_flags;
use vitals_monitor::services::simulator::{DEMO_PATIENTS, VitalsSimulator};
use vitals_monitor::services::stream::{Sample, decode, encode};

fn known() -> HashSet<String> {
    HashSet::from(["P001".to_string()])
}

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        (-1e6f64..1e6).prop_map(|f| json!(f)),
        ".{0,20}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::hash_map("[a-zA-Z]{1,12}", inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

proptest! {
    #[test]
    fn any_plausible_temperature_is_valid_fhir(t in 25.0f64..=45.0) {
        let o = b::body_temperature("o", "P001", "D", Utc::now(), t);
        prop_assert!(validate_observation(&o, Some(&known())).is_empty());
    }

    #[test]
    fn bp_valid_iff_systolic_gt_diastolic(s in 40.0f64..=300.0, d in 20.0f64..=200.0) {
        let o = b::blood_pressure("o", "P001", "D", Utc::now(), s, d);
        let r2 = |v: f64| (v * 100.0).round() / 100.0;
        prop_assert_eq!(validate_observation(&o, Some(&known())).is_empty(), r2(s) > r2(d));
    }

    #[test]
    fn binary_frame_roundtrip(idx in 0u8..3, seq: u32, ts in 0f64..4e12, t: f32, s: f32, d: f32, alerts in 0u8..32) {
        let ids: Vec<String> = ["P001", "P002", "P003"].iter().map(|s| s.to_string()).collect();
        let sample = Sample { patient_id: ids[idx as usize].clone(), patient_index: idx, seq, ts_ms: ts,
                              temp_c: t, systolic: s, diastolic: d, alerts };
        let out = decode(&encode(&sample), &ids).unwrap();
        prop_assert_eq!(encode(&out), encode(&sample)); // bit-exact, incl. NaN payloads
    }

    #[test]
    fn decoder_never_panics_on_garbage(buf in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = decode(&buf, &["P001".to_string()]);
    }

    #[test]
    fn validator_fuzz_never_panics(
        mut obj in prop::collection::hash_map(
            prop::sample::select(vec!["resourceType", "status", "code", "subject", "valueQuantity",
                                      "component", "category", "effectiveDateTime", "id", "meta", "foo"]),
            arb_json(), 0..8)
    ) {
        obj.entry("resourceType").or_insert(json!("Observation"));
        let v = Value::Object(obj.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        for issue in validate_observation(&v, Some(&known())) {
            prop_assert!(!issue.code.is_empty() && !issue.severity.is_empty());
        }
    }

    #[test]
    fn simulator_always_emits_valid_observations(seed: u64) {
        let mut sim = VitalsSimulator::new(DEMO_PATIENTS[0].clone(), 1.0, Some(seed));
        let ts = Utc::now();
        for _ in 0..20 {
            let (t, s, d) = sim.step();
            prop_assert!(validate_observation(&b::body_temperature("o", "P001", "D", ts, t), Some(&known())).is_empty());
            prop_assert!(validate_observation(&b::blood_pressure("o", "P001", "D", ts, s, d), Some(&known())).is_empty());
        }
    }

    #[test]
    fn alert_flags_agree_with_fhir_interpretation(t in 25.0f64..45.0, s in 40.0f64..300.0, d in 20.0f64..200.0) {
        let f = threshold_flags(t, s, d);
        prop_assert_eq!(f & 1 != 0, b::interpret_temp(t) == "H");
        prop_assert_eq!(f & 2 != 0, b::interpret_temp(t) == "L");
    }
}
