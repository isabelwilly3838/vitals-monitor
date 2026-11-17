//! `vitalsctl` - operator CLI (cross-platform, no openssl/python needed).
//!
//!   vitalsctl gen-secrets          create deploy/secrets/* and a self-signed TLS cert (idempotent)
//!   vitalsctl rotate-keys [--prune] prepend new encryption + JWT keys (or drop all but newest)
//!   vitalsctl hash-password <pw>   bcrypt hash for users.json
//!   vitalsctl check-config         load + validate configuration for $APP_ENVIRONMENT
//!   vitalsctl health [url]         container healthcheck (no curl needed in the image)

use std::fs;
use std::path::{Path, PathBuf};

use rand::Rng;
use rand::distributions::Alphanumeric;
use serde_json::json;
use vitals_monitor::security::crypto::generate_key;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn token(n: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

fn write_secret(dir: &Path, name: &str, value: &str) -> anyhow::Result<()> {
    let path = dir.join(name);
    if path.exists() {
        println!("    keep  deploy/secrets/{name}");
        return Ok(());
    }
    fs::write(&path, value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    println!("    wrote deploy/secrets/{name}");
    Ok(())
}

fn gen_secrets() -> anyhow::Result<()> {
    let dir = root().join("deploy/secrets");
    fs::create_dir_all(&dir)?;
    write_secret(&dir, "jwt_secrets", &token(64))?;
    write_secret(&dir, "encryption_keys", &generate_key())?;
    write_secret(&dir, "db_password", &token(32))?;
    write_secret(&dir, "grafana_admin", &token(20))?;
    if !dir.join("users.json").exists() {
        let mut users = vec![];
        let mut creds = String::new();
        for (name, role, pid) in [
            ("admin", "admin", None),
            ("clinician", "clinician", None),
            ("patient1", "patient", Some("P001")),
            ("gateway", "device", None),
        ] {
            let pw = token(16);
            creds.push_str(&format!("{name}: {pw}\n"));
            users.push(json!({"username": name, "roles": [role], "patient_id": pid,
                              "password_hash": bcrypt::hash(&pw, 12)?}));
        }
        write_secret(&dir, "users.json", &serde_json::to_string(&users)?)?;
        write_secret(&dir, "initial_passwords.txt", &creds)?;
        println!("    passwords for the docker stack: deploy/secrets/initial_passwords.txt");
    }

    let certs = root().join("deploy/nginx/certs");
    if !certs.join("server.crt").exists() {
        fs::create_dir_all(&certs)?;
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
        fs::write(certs.join("server.crt"), ck.cert.pem())?;
        fs::write(certs.join("server.key"), ck.key_pair.serialize_pem())?;
        println!("    wrote deploy/nginx/certs/server.crt (self-signed, localhost)");
    }
    Ok(())
}

fn rotate(prune: bool) -> anyhow::Result<()> {
    let dir = root().join("deploy/secrets");
    for (name, fresh) in [
        ("encryption_keys", generate_key()),
        ("jwt_secrets", token(64)),
    ] {
        let path = dir.join(name);
        let current = fs::read_to_string(&path)?;
        let mut keys: Vec<String> = current
            .trim()
            .split(',')
            .filter(|k| !k.is_empty())
            .map(String::from)
            .collect();
        if prune {
            keys.truncate(1);
        } else {
            keys.insert(0, fresh);
            keys.truncate(3);
        }
        fs::write(&path, keys.join(","))?;
        println!("{name}: {} key(s) active", keys.len());
    }
    if !prune {
        println!("Next: docker compose restart backend, then POST /admin/keys/rotate as admin,");
        println!("      then `vitalsctl rotate-keys --prune` to retire the old keys.");
    }
    Ok(())
}

/// Minimal HTTP/1.0 GET so the runtime image needs no curl/wget.
fn health(url: &str) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("http:// url required"))?;
    let (host, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let mut s = std::net::TcpStream::connect(host)?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n")?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    if resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        anyhow::bail!("unhealthy: {}", resp.lines().next().unwrap_or(""))
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("gen-secrets") => gen_secrets(),
        Some("rotate-keys") => rotate(args.iter().any(|a| a == "--prune")),
        Some("hash-password") => {
            let pw = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: hash-password <pw>"))?;
            println!("{}", bcrypt::hash(pw, 12)?);
            Ok(())
        }
        Some("check-config") => {
            let s = vitals_monitor::configuration::get_configuration()?;
            println!(
                "configuration OK for environment '{}'",
                s.environment.as_str()
            );
            Ok(())
        }
        Some("health") => health(
            args.get(1)
                .map_or("http://127.0.0.1:8000/health/ready", String::as_str),
        ),
        _ => {
            eprintln!(
                "usage: vitalsctl <gen-secrets | rotate-keys [--prune] | hash-password <pw> | check-config>"
            );
            std::process::exit(2)
        }
    }
}
