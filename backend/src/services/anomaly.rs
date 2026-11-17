//! Real-time clinical alerting: threshold rules + rate-of-change detection (rolling z-score).
//! Produces the bit flags carried in every stream frame.

use std::collections::{HashMap, VecDeque};

use super::stream::*;
use crate::fhir::codes as c;

pub fn threshold_flags(temp: f64, sys: f64, dia: f64) -> u8 {
    let mut f = 0;
    if temp >= c::TEMP_HIGH {
        f |= ALERT_FEVER;
    }
    if temp < c::TEMP_LOW {
        f |= ALERT_HYPOTHERMIA;
    }
    if sys >= c::SYS_HIGH || dia >= c::DIA_HIGH {
        f |= ALERT_HYPERTENSION;
    }
    if sys < c::SYS_LOW || dia < c::DIA_LOW {
        f |= ALERT_HYPOTENSION;
    }
    f
}

/// Flags a systolic reading that deviates > `z_limit` std-devs from the recent window.
pub struct RateOfChange {
    window: VecDeque<f64>,
    size: usize,
    z_limit: f64,
    min_std: f64,
}

impl Default for RateOfChange {
    fn default() -> Self {
        Self {
            window: VecDeque::new(),
            size: 30,
            z_limit: 4.0,
            min_std: 1.5,
        }
    }
}

impl RateOfChange {
    pub fn check(&mut self, v: f64) -> bool {
        let mut flag = false;
        if self.window.len() >= 10 {
            let n = self.window.len() as f64;
            let mean = self.window.iter().sum::<f64>() / n;
            let var = self.window.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
            flag = (v - mean).abs() / var.sqrt().max(self.min_std) > self.z_limit;
        }
        self.window.push_back(v);
        if self.window.len() > self.size {
            self.window.pop_front();
        }
        flag
    }
}

#[derive(Default)]
pub struct AlertEngine {
    roc: HashMap<String, RateOfChange>,
    last: HashMap<String, u8>,
}

impl AlertEngine {
    pub fn evaluate(&mut self, patient: &str, temp: f64, sys: f64, dia: f64) -> u8 {
        let mut flags = threshold_flags(temp, sys, dia);
        if self.roc.entry(patient.into()).or_default().check(sys) {
            flags |= ALERT_RAPID_CHANGE;
        }
        // log transitions only, so a sustained fever does not flood the log pipeline
        let prev = self.last.insert(patient.into(), flags).unwrap_or(0);
        if flags != prev {
            if flags != 0 {
                tracing::warn!(patient, flags, "vital alert raised");
            } else {
                tracing::info!(patient, "vital alert cleared");
            }
        }
        flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds() {
        assert_eq!(threshold_flags(37.0, 120.0, 80.0), 0);
        assert_eq!(
            threshold_flags(38.2, 150.0, 80.0),
            ALERT_FEVER | ALERT_HYPERTENSION
        );
        assert_eq!(
            threshold_flags(34.0, 85.0, 50.0),
            ALERT_HYPOTHERMIA | ALERT_HYPOTENSION
        );
    }

    #[test]
    fn rate_of_change_detects_spike() {
        let mut d = RateOfChange::default();
        assert!(!(0..15).any(|i| d.check(120.0 + (i % 3) as f64)));
        assert!(d.check(170.0));
    }
}
