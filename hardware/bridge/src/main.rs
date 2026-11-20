//! vitals-bridge: reads the Arduino sensor over USB serial and posts each reading to the
//! backend as a FHIR R4B body-temperature Observation, logged in as the `device` role.
//!
//!   vitals-bridge                       auto-detect the Arduino port
//!   vitals-bridge --port /dev/cu.usbmodem1101
//!   vitals-bridge --list                list serial ports
//!   vitals-bridge --stdin               read sensor lines from stdin (testing, no hardware)
//!
//! Options: --server URL (default http://127.0.0.1:8000)  --patient P004  --device D004
//!          --user gateway  --all (also post readings that have not settled yet)
//! Password: $BRIDGE_PASSWORD, defaulting to the dev-only gateway password.

use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};

const DEV_PASSWORD: &str = "device-dev-pass";
/// Same plausibility range the backend enforces (fhir/codes.rs TEMP_RANGE).
const PLAUSIBLE_C: (f64, f64) = (25.0, 45.0);

#[derive(Debug, PartialEq)]
enum Line {
    Temp { celsius: f64, stable: bool },
    Fault(String),
    Ignore,
}

/// Parse one line of the sensor protocol (see hardware/arduino/vitals_sensor.ino).
fn parse_line(line: &str) -> Line {
    let line = line.trim();
    let mut parts = line.split(',');
    match parts.next() {
        Some("T") => {
            let celsius = parts.next().and_then(|v| v.trim().parse::<f64>().ok());
            let stable = parts.next().map(str::trim) == Some("1");
            match celsius {
                Some(c) if c.is_finite() => Line::Temp { celsius: c, stable },
                _ => Line::Ignore,
            }
        }
        Some("E") => Line::Fault(parts.next().unwrap_or("unknown").trim().to_string()),
        _ => Line::Ignore,
    }
}

/// FHIR R4B Observation conforming to the vital-signs body-temperature profile.
fn observation(patient: &str, device: &str, celsius: f64) -> Value {
    let celsius = (celsius * 100.0).round() / 100.0;
    json!({
        "resourceType": "Observation",
        "meta": {"profile": [
            "http://hl7.org/fhir/StructureDefinition/vitalsigns",
            "http://hl7.org/fhir/StructureDefinition/bodytemp"
        ]},
        "status": "final",
        "category": [{"coding": [{
            "system": "http://terminology.hl7.org/CodeSystem/observation-category",
            "code": "vital-signs", "display": "Vital Signs"}], "text": "Vital Signs"}],
        "code": {"coding": [{"system": "http://loinc.org", "code": "8310-5",
                             "display": "Body temperature"}], "text": "Body temperature"},
        "subject": {"reference": format!("Patient/{patient}")},
        "device": {"reference": format!("Device/{device}")},
        "effectiveDateTime": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "valueQuantity": {"value": celsius, "unit": "Cel",
                          "system": "http://unitsofmeasure.org", "code": "Cel"},
    })
}

struct Config {
    server: String,
    patient: String,
    device: String,
    user: String,
    password: String,
    post_all: bool,
}

struct Client {
    http: reqwest::blocking::Client,
    cfg: Config,
    token: Option<(String, Instant)>,
}

impl Client {
    fn login(&mut self) -> anyhow::Result<String> {
        if let Some((t, exp)) = &self.token
            && Instant::now() < *exp
        {
            return Ok(t.clone());
        }
        let resp = self
            .http
            .post(format!("{}/auth/token", self.cfg.server))
            .form(&[
                ("username", &self.cfg.user),
                ("password", &self.cfg.password),
            ])
            .send()
            .with_context(|| format!("cannot reach backend at {}", self.cfg.server))?;
        if !resp.status().is_success() {
            bail!(
                "login as '{}' failed: HTTP {}",
                self.cfg.user,
                resp.status()
            );
        }
        let body: Value = resp.json()?;
        let token = body["access_token"]
            .as_str()
            .context("no access_token in login response")?
            .to_string();
        let ttl = body["expires_in"].as_u64().unwrap_or(300);
        // refresh 30 s before the JWT expires
        let exp = Instant::now() + Duration::from_secs(ttl.saturating_sub(30));
        self.token = Some((token.clone(), exp));
        eprintln!("logged in as '{}' (device role)", self.cfg.user);
        Ok(token)
    }

    fn post(&mut self, obs: &Value) -> anyhow::Result<String> {
        for attempt in 0..2 {
            let token = self.login()?;
            let resp = self
                .http
                .post(format!("{}/fhir/Observation", self.cfg.server))
                .bearer_auth(token)
                .header("Content-Type", "application/fhir+json")
                .body(obs.to_string())
                .send()?;
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                self.token = None; // token expired or key rotated: log in again
                continue;
            }
            let body: Value = resp.json().unwrap_or(Value::Null);
            if status.is_success() {
                return Ok(body["id"].as_str().unwrap_or("?").to_string());
            }
            let why: Vec<&str> = body["issue"]
                .as_array()
                .map(|a| a.iter().filter_map(|i| i["diagnostics"].as_str()).collect())
                .unwrap_or_default();
            bail!("backend rejected reading: HTTP {status} {}", why.join("; "));
        }
        bail!("still unauthorized after re-login")
    }

    fn handle(&mut self, raw: &str) {
        match parse_line(raw) {
            Line::Ignore => {}
            Line::Fault(why) => eprintln!("sensor fault: {why} - check the thermistor wiring"),
            Line::Temp { celsius, stable } => {
                let tag = if stable { "stable" } else { "settling" };
                if !stable && !self.cfg.post_all {
                    eprintln!("{celsius:6.2} °C  {tag} (not sent yet)");
                } else if !(PLAUSIBLE_C.0..=PLAUSIBLE_C.1).contains(&celsius) {
                    eprintln!("{celsius:6.2} °C  outside {PLAUSIBLE_C:?} - sensor not on body?");
                } else {
                    let obs = observation(&self.cfg.patient, &self.cfg.device, celsius);
                    match self.post(&obs) {
                        Ok(id) => println!("{celsius:6.2} °C  {tag} -> Observation/{id}"),
                        Err(e) => eprintln!("{celsius:6.2} °C  NOT sent: {e:#}"),
                    }
                }
            }
        }
    }
}

fn find_port() -> anyhow::Result<String> {
    let ports = serialport::available_ports().unwrap_or_default();
    let looks_like_arduino = |n: &str| {
        ["usbmodem", "usbserial", "wchusbserial", "ttyACM", "ttyUSB"]
            .iter()
            .any(|k| n.contains(k))
    };
    // macOS lists every device twice (tty.* and cu.*); cu.* is the one to open
    let mut names: Vec<String> = ports
        .into_iter()
        .map(|p| p.port_name)
        .filter(|n| looks_like_arduino(n) && !n.contains("/tty.usb"))
        .collect();
    names.sort();
    names
        .into_iter()
        .next()
        .context("no Arduino found - is it plugged in? (try --list, then --port <name>)")
}

fn run_serial(client: &mut Client, port: Option<String>) -> anyhow::Result<()> {
    loop {
        let name = match &port {
            Some(p) => p.clone(),
            None => find_port()?,
        };
        match serialport::new(&name, 9600)
            .timeout(Duration::from_secs(5))
            .open()
        {
            Ok(sp) => {
                eprintln!("reading {name} at 9600 baud (Ctrl+C to stop)");
                let mut reader = BufReader::new(sp);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => client.handle(&line),
                        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                            eprintln!("no data from the Arduino for 5 s - is the sketch uploaded?")
                        }
                        Err(e) => {
                            eprintln!("serial error: {e} - reconnecting");
                            break;
                        }
                    }
                }
            }
            Err(e) => eprintln!("cannot open {name}: {e} (Arduino IDE serial monitor open?)"),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let opt = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1).cloned())
    };
    if flag("--help") || flag("-h") {
        println!(
            "{}",
            include_str!("main.rs")
                .lines()
                .take(11)
                .collect::<Vec<_>>()
                .join("\n")
        );
        return Ok(());
    }
    if flag("--list") {
        for p in serialport::available_ports()? {
            println!("{}", p.port_name);
        }
        return Ok(());
    }
    let password = std::env::var("BRIDGE_PASSWORD").unwrap_or_else(|_| {
        eprintln!("BRIDGE_PASSWORD not set - using the dev-only gateway password");
        DEV_PASSWORD.into()
    });
    let mut client = Client {
        http: reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?,
        cfg: Config {
            server: opt("--server")
                .unwrap_or_else(|| "http://127.0.0.1:8000".into())
                .trim_end_matches('/')
                .to_string(),
            patient: opt("--patient").unwrap_or_else(|| "P004".into()),
            device: opt("--device").unwrap_or_else(|| "D004".into()),
            user: opt("--user").unwrap_or_else(|| "gateway".into()),
            password,
            post_all: flag("--all"),
        },
        token: None,
    };
    client.login()?; // fail fast on a wrong URL or password

    if flag("--stdin") {
        for line in std::io::stdin().lock().lines() {
            client.handle(&line?);
        }
        Ok(())
    } else {
        run_serial(&mut client, opt("--port"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_sensor_protocol() {
        assert_eq!(
            parse_line("T,36.84,1\r\n"),
            Line::Temp {
                celsius: 36.84,
                stable: true
            }
        );
        assert_eq!(
            parse_line("T,33.10,0"),
            Line::Temp {
                celsius: 33.10,
                stable: false
            }
        );
        assert_eq!(parse_line("E,open"), Line::Fault("open".into()));
        assert_eq!(parse_line("# vitals-sensor v1 thermistor"), Line::Ignore);
        assert_eq!(parse_line("T,nan,1"), Line::Ignore);
        assert_eq!(parse_line("T,garbage"), Line::Ignore);
        assert_eq!(parse_line(""), Line::Ignore);
    }

    #[test]
    fn observation_is_a_vital_signs_body_temperature() {
        let o = observation("P004", "D004", 36.8449);
        assert_eq!(o["code"]["coding"][0]["code"], "8310-5");
        assert_eq!(o["valueQuantity"]["value"], 36.84);
        assert_eq!(o["valueQuantity"]["code"], "Cel");
        assert_eq!(o["subject"]["reference"], "Patient/P004");
        assert_eq!(o["category"][0]["coding"][0]["code"], "vital-signs");
    }
}
