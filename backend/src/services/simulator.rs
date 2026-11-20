//! Wearable sensor simulator: body temperature + blood pressure per patient.
//!
//! Per-patient baseline, compressed circadian drift, mean-reverting random walk,
//! measurement noise, and occasional clinical episodes (fever, hypertensive spike,
//! hypotension) so the dashboard has realistic events to show.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

#[derive(Debug, Clone)]
pub struct PatientProfile {
    pub patient_id: &'static str,
    pub pseudonym: &'static str,
    pub gender: &'static str,
    pub birth_year: i32,
    pub device_id: &'static str,
    pub device_model: &'static str,
    /// `false` = a real sensor reports for this patient (via the USB bridge); the
    /// simulator leaves it alone so real and synthetic data never mix.
    pub simulated: bool,
    pub base_temp: f64,
    pub base_sys: f64,
    pub base_dia: f64,
}

pub const SIMULATED_MODEL: &str = "VitalPatch BT-2 (simulated)";

/// P001-P003 are simulated; P004 is the real Arduino sensor.
pub const DEMO_PATIENTS: [PatientProfile; 4] = [
    PatientProfile {
        patient_id: "P001",
        pseudonym: "STUDY-0001",
        gender: "female",
        birth_year: 1958,
        device_id: "D001",
        device_model: SIMULATED_MODEL,
        simulated: true,
        base_temp: 36.7,
        base_sys: 128.0,
        base_dia: 82.0,
    },
    PatientProfile {
        patient_id: "P002",
        pseudonym: "STUDY-0002",
        gender: "male",
        birth_year: 1971,
        device_id: "D002",
        device_model: SIMULATED_MODEL,
        simulated: true,
        base_temp: 36.9,
        base_sys: 131.0,
        base_dia: 84.0,
    },
    PatientProfile {
        patient_id: "P003",
        pseudonym: "STUDY-0003",
        gender: "female",
        birth_year: 1994,
        device_id: "D003",
        device_model: SIMULATED_MODEL,
        simulated: true,
        base_temp: 36.5,
        base_sys: 112.0,
        base_dia: 72.0,
    },
    PatientProfile {
        patient_id: "P004",
        pseudonym: "STUDY-0004",
        gender: "unknown",
        birth_year: 2000,
        device_id: "D004",
        device_model: "Elegoo UNO R3 + 10k NTC thermistor (hardware/arduino)",
        simulated: false,
        base_temp: 36.8,
        base_sys: 120.0,
        base_dia: 80.0,
    },
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Episode {
    Fever,
    Hypertension,
    Hypotension,
}

pub struct VitalsSimulator {
    pub profile: PatientProfile,
    hz: f64,
    rng: StdRng,
    temp: f64,
    sys: f64,
    dia: f64,
    episode: Option<(Episode, u32)>,
    t: f64,
    pub seq: u32,
}

impl VitalsSimulator {
    pub fn new(profile: PatientProfile, hz: f64, seed: Option<u64>) -> Self {
        let rng = match seed {
            Some(s) => StdRng::seed_from_u64(s),
            None => StdRng::from_entropy(),
        };
        Self {
            temp: profile.base_temp,
            sys: profile.base_sys,
            dia: profile.base_dia,
            profile,
            hz,
            rng,
            episode: None,
            t: 0.0,
            seq: 0,
        }
    }

    fn gauss(&mut self, sd: f64) -> f64 {
        // Box-Muller
        let (u1, u2): (f64, f64) = (self.rng.r#gen::<f64>().max(1e-12), self.rng.r#gen());
        sd * (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    /// Advance one tick; returns (temp_c, systolic, diastolic).
    pub fn step(&mut self) -> (f64, f64, f64) {
        let dt = 1.0 / self.hz;
        self.t += dt;
        self.seq += 1;

        if self.episode.is_none() && self.rng.r#gen::<f64>() < 0.004 * dt {
            let kind = [Episode::Fever, Episode::Hypertension, Episode::Hypotension]
                [self.rng.gen_range(0..3)];
            self.episode = Some((kind, (self.rng.gen_range(40.0..120.0) * self.hz) as u32));
        }
        let p = &self.profile;
        let (mut tt, mut ts, mut td) = (p.base_temp, p.base_sys, p.base_dia);
        match self.episode {
            Some((Episode::Fever, _)) => tt += 1.9,
            Some((Episode::Hypertension, _)) => {
                ts += 32.0;
                td += 14.0
            }
            Some((Episode::Hypotension, _)) => {
                ts -= 30.0;
                td -= 18.0
            }
            None => {}
        }
        if let Some((kind, left)) = self.episode {
            self.episode = if left <= 1 {
                None
            } else {
                Some((kind, left - 1))
            };
        }

        let circadian = 0.25 * (2.0 * std::f64::consts::PI * self.t / 600.0).sin(); // 10-min "day"
        let (kt, kbp) = (0.03 * dt, 0.08 * dt);
        let n = (self.gauss(0.015), self.gauss(0.9), self.gauss(0.6));
        self.temp += kt * (tt + circadian - self.temp) + n.0;
        self.sys += kbp * (ts - self.sys) + n.1;
        self.dia += kbp * (td - self.dia) + n.2;

        let temp = round(self.temp + self.gauss(0.03), 2);
        let sys = round((self.sys + self.gauss(1.5)).max(60.0), 1);
        let dia = round((self.dia + self.gauss(1.0)).clamp(35.0, sys - 15.0), 1);
        (temp, sys, dia)
    }
}

fn round(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}
