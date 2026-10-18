//! G7 secret release + generate (contract §21, §20.1).
//!
//! - `secret.release { name, module, as: "env:NAME" | "file:<path>" }` → a Confirmed kernel card every time
//!   ("Give <secret> to <module>? It will be able to read the value."). Released secrets are delivered to that
//!   module's processes only: `env:` at every start of its module server and services (the registry `env` hook),
//!   `file:` as a 0600 file inside the VM at `/run/openapp/secrets/<module>/<path>` (guest tmpfs: never on the
//!   host disk or in the workspace the agent edits; `$OPENAPP_SECRETS_DIR` points there), written through the
//!   bridge before the server / each service (re)starts. A release restarts the module's server and services so
//!   the value arrives at once. `secret.revoke { name, module, as? }` removes it (file deleted, processes restarted).
//!   Records (never values) in `<state>/secret_releases.json`; research log without values.
//! - `secret.generate { name, kind: password | ed25519 | x25519 | rsa-4096, label?, length? }` → generated on the
//!   Mac (aws-lc-rs CSPRNG), stored in the Keychain (`password`, or `keypair` = PKCS#8 PEM private key), returns only
//!   the public part (`public_pem`, `public_openssh` for ed25519, `public_b64` raw for ed25519/x25519).

use crate::research::Kind;
use crate::util::MutexExt;
use super::*;
use crate::modules::{safe_rel, valid_module_id};
use crate::state::now_ts;
use base64::Engine;
use std::time::Duration;

pub const GUEST_SECRETS: &str = "/run/openapp/secrets";
const RESERVED_ENV: &[&str] = &["PATH", "HOME", "USER", "SHELL", "PWD", "LD_PRELOAD", "LD_LIBRARY_PATH", "NODE_OPTIONS", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "OPENAPP_SOCKS_URL"];

#[derive(Clone, Debug, PartialEq)]
pub enum As {
    Env(String),
    File(String),
}

impl As {
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(n) = s.strip_prefix("env:") {
            let ok = !n.is_empty()
                && n.len() <= 64
                && n.chars().next().is_some_and(|c| c.is_ascii_uppercase() || c == '_')
                && n.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !ok {
                return Err(format!("\"as\": env name \"{n}\" must match [A-Z_][A-Z0-9_]*"));
            }
            if n.starts_with("OPENAPP_") || RESERVED_ENV.contains(&n) || n.ends_with("_PROXY") {
                return Err(format!("\"as\": env name \"{n}\" is reserved"));
            }
            return Ok(As::Env(n.to_string()));
        }
        if let Some(p) = s.strip_prefix("file:") {
            if !safe_rel(p) || p.len() > 200 {
                return Err(format!("\"as\": file path \"{p}\" must be relative (no \"..\"), e.g. \"file:bridge/password\""));
            }
            return Ok(As::File(p.to_string()));
        }
        Err("\"as\" must be \"env:NAME\" or \"file:<path>\"".into())
    }
    pub fn text(&self) -> String {
        match self {
            As::Env(n) => format!("env:{n}"),
            As::File(p) => format!("file:{p}"),
        }
    }
}

/// Guest path of a file release.
pub fn guest_file(module: &str, rel: &str) -> String {
    format!("{GUEST_SECRETS}/{module}/{rel}")
}

fn path(k: &Kernel) -> std::path::PathBuf {
    k.state_dir.join("secret_releases.json")
}

#[derive(Default)]
struct RelState(Mutex<()>);

/// All release records (name, module, as, ts, actor).
pub fn releases(k: &Kernel) -> Vec<Value> {
    std::fs::read(path(k)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

fn write(k: &Kernel, list: &[Value]) {
    let p = path(k);
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(list).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, &p);
    }
}

fn of_module(k: &Kernel, module: &str) -> Vec<(String, As)> {
    releases(k)
        .iter()
        .filter(|r| r["module"] == module)
        .filter_map(|r| Some((r["name"].as_str()?.to_string(), As::parse(r["as"].as_str()?).ok()?)))
        .collect()
}

/// Registry env hook: released `env:` secrets (+ `OPENAPP_SECRETS_DIR` when the module has file releases).
fn env_for(k: &Kernel, module: &str, _in_vm: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let rel = of_module(k, module);
    if rel.iter().any(|(_, a)| matches!(a, As::File(_))) {
        out.push(("OPENAPP_SECRETS_DIR".into(), format!("{GUEST_SECRETS}/{module}")));
    }
    for (name, a) in rel {
        if let As::Env(var) = a {
            match k.secrets.get(&name) {
                Ok((_, v)) => {
                    out.push((var, String::from_utf8_lossy(&v).to_string()));
                    k.secrets.note_use(&name, &format!("module:{module}"));
                }
                Err(e) => log::warn!("secret release {name} → {module}: {e}"),
            }
        }
    }
    out
}

/// Write the module's `file:` releases into the VM (0600, dir 0700). Called before the module server and before
/// each service (re)start, so a fresh VM (tmpfs) gets them again. No-op without file releases.
pub async fn deliver_files(k: &Kernel, vm: &crate::vm::AppVm, module: &str) {
    let files: Vec<(String, String)> = of_module(k, module).into_iter().filter_map(|(n, a)| if let As::File(p) = a { Some((n, p)) } else { None }).collect();
    if files.is_empty() {
        return;
    }
    let mut written = Vec::new();
    for (name, rel) in files {
        let Ok((_, v)) = k.secrets.get(&name) else { continue };
        let gp = guest_file(module, &rel);
        // create the file 0600 before the value goes in
        let dir = gp.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let _ = vm.sh(&format!("umask 077; mkdir -p '{dir}' && chmod 700 '{GUEST_SECRETS}/{module}' && : > '{gp}' && chmod 600 '{gp}'"), &[]).await;
        match vm.write_file(&gp, &v).await {
            Ok(()) => {
                let _ = vm.sh(&format!("chmod 600 '{gp}'"), &[]).await;
                k.secrets.note_use(&name, &format!("module:{module}"));
                written.push(name);
            }
            Err(e) => log::warn!("secret file {name} → {module}: {e}"),
        }
    }
    if !written.is_empty() {
        k.research.log("kernel", Kind::SecretRelease, json!({ "phase": "delivered", "module": module, "names": written, "as": "file" }));
    }
}

/// Restart the module's server and services so released values arrive (or disappear) now.
async fn redeliver(k: &Arc<Kernel>, module: &str) {
    if let Some(vm) = k.vm() {
        deliver_files(k, &vm, module).await;
    }
    let running = k.lock().modules.get(module).is_some_and(|m| m.status == "running" && m.client.is_some());
    if running {
        let _ = k.module_load(module).await;
    }
    for s in super::services::statuses(k, Some(module)) {
        if let Some(n) = s["name"].as_str() {
            let _ = call_action(k, "service.restart", json!({ "module": module, "name": n }), Caller::new("kernel")).await;
        }
    }
}

fn module_param(k: &Kernel, p: &Value, c: &Caller) -> Result<String, String> {
    let m = match (c.module(), arg(p, "module")) {
        (Some(own), Some(m)) if own != m => return Err(format!("{} may only ask for secrets for itself (not \"{m}\")", c.id)),
        (Some(own), _) => own.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => return Err("\"module\" is required".into()),
    };
    if !valid_module_id(&m) || !k.lock().modules.contains_key(&m) {
        return Err(format!("unknown module \"{m}\""));
    }
    Ok(m)
}

async fn release(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, crate::kernel::HostError> {
    let name = arg(&p, "name").ok_or("secret.release needs \"name\"")?.to_string();
    let module = module_param(&k, &p, &c)?;
    let how = As::parse(arg(&p, "as").ok_or("secret.release needs \"as\": \"env:NAME\" or \"file:<path>\"")?)?;
    let meta = k.secrets.meta(&name).ok_or_else(|| format!("no secret \"{name}\" (ask the user with secret.request first)"))?;
    if of_module(&k, &module).iter().any(|(n, a)| *n == name && *a == how) {
        return Ok(json!({ "ok": true, "name": name, "module": module, "as": how.text(), "already": true }));
    }
    let label = meta["label"].as_str().unwrap_or(&name).to_string();
    let mname = k.lock().modules.get(&module).map(|m| m.name.clone()).unwrap_or_else(|| module.clone());
    let who = crate::secrets::actor_words(&k, &c.id);
    let where_ = match &how {
        As::Env(v) => format!("as the environment variable {v} of its server and services"),
        As::File(f) => format!("as a private file ({f}) inside the app VM"),
    };
    let title = format!("Give \"{label}\" to {mname}?");
    let text = format!(
        "{who} asks. {mname} will be able to read the value; it gets it {where_}, now and every time it restarts. Other modules do not. You can take it back later."
    );
    let req = crate::consent::ConsentReq::new("secret_release", title, text)
        .options(
            json!([
                { "optionId": "give", "name": format!("Give to {mname}"), "kind": "allow_once" },
                { "optionId": "deny", "name": "Don't give", "kind": "reject_once" }
            ]),
            &["give"],
        )
        .info(json!({ "name": name, "module": module, "as": how.text(), "actor": c.id }))
        .timeout(Duration::from_secs(600));
    if !crate::consent::ask(&k, req).await.allowed {
        k.research.log("user", Kind::SecretRelease, json!({ "phase": "denied", "name": name, "module": module, "as": how.text(), "by": c.id }));
        return Err(crate::kernel::HostError::forbidden(format!("the user did not give \"{name}\" to {module}")));
    }
    {
        let st = k.prims.get::<RelState>();
        let _l = st.0.locked();
        let mut list = releases(&k);
        list.retain(|r| !(r["name"] == name.as_str() && r["module"] == module.as_str() && r["as"] == how.text().as_str()));
        list.push(json!({ "name": name, "module": module, "as": how.text(), "ts": now_ts(), "actor": c.id }));
        write(&k, &list);
    }
    k.research.log("user", Kind::SecretRelease, json!({ "phase": "released", "name": name, "module": module, "as": how.text(), "by": c.id }));
    redeliver(&k, &module).await;
    k.emit_snapshot();
    let mut out = json!({ "ok": true, "name": name, "module": module, "as": how.text() });
    if let As::File(f) = &how {
        out["path"] = json!(guest_file(&module, f));
        out["env"] = json!("OPENAPP_SECRETS_DIR");
    }
    Ok(out)
}

async fn revoke(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let name = arg(&p, "name").ok_or("secret.revoke needs \"name\"")?.to_string();
    let module = module_param(&k, &p, &c)?;
    let only = arg(&p, "as").map(As::parse).transpose()?;
    let gone = {
        let st = k.prims.get::<RelState>();
        let _l = st.0.locked();
        let (gone, keep): (Vec<Value>, Vec<Value>) = releases(&k).into_iter().partition(|r| {
            r["name"] == name.as_str() && r["module"] == module.as_str() && only.as_ref().is_none_or(|a| r["as"] == a.text().as_str())
        });
        if !gone.is_empty() {
            write(&k, &keep);
        }
        gone
    };
    if gone.is_empty() {
        return Err(format!("\"{name}\" is not released to {module}"));
    }
    if let Some(vm) = k.vm() {
        for r in &gone {
            if let Some(As::File(f)) = r["as"].as_str().and_then(|a| As::parse(a).ok()) {
                let _ = vm.sh(&format!("rm -f '{}'", guest_file(&module, &f)), &[]).await;
            }
        }
    }
    k.research.log(&c.id, Kind::SecretRelease, json!({ "phase": "revoked", "name": name, "module": module, "count": gone.len() }));
    redeliver(&k, &module).await;
    k.emit_snapshot();
    Ok(json!({ "ok": true, "revoked": gone.len(), "name": name, "module": module }))
}

// ---------- generate ----------

fn pem(label: &str, der: &[u8]) -> String {
    let b = base64::engine::general_purpose::STANDARD.encode(der);
    let mut s = format!("-----BEGIN {label}-----\n");
    for chunk in b.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        s.push('\n');
    }
    s.push_str(&format!("-----END {label}-----\n"));
    s
}

fn ssh_string(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

/// (secret bytes, stored kind, public parts).
type Generated = (Vec<u8>, &'static str, Map<String, Value>);

/// (secret value, kind, public metadata).
pub fn generate_value(kind: &str, length: usize) -> Result<Generated, String> {
    use aws_lc_rs::encoding::{AsBigEndian, AsDer, Curve25519SeedBin, Pkcs8V1Der, PublicKeyX509Der};
    use aws_lc_rs::rand::{SecureRandom, SystemRandom};
    use aws_lc_rs::signature::KeyPair;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let mut meta = Map::new();
    meta.insert("algorithm".into(), json!(kind));
    match kind {
        "password" => {
            const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789-_.!";
            let n = length.clamp(12, 128);
            let rng = SystemRandom::new();
            let mut out = Vec::with_capacity(n);
            while out.len() < n {
                let mut b = [0u8; 64];
                rng.fill(&mut b).map_err(|_| "random generator failed")?;
                // rejection sampling: no modulo bias
                let lim = 256 - (256 % CHARS.len());
                out.extend(b.iter().filter(|x| (**x as usize) < lim).map(|x| CHARS[*x as usize % CHARS.len()]).take(n - out.len()));
            }
            meta.insert("length".into(), json!(n));
            Ok((out, "password", meta))
        }
        "ed25519" => {
            let kp = aws_lc_rs::signature::Ed25519KeyPair::generate().map_err(|_| "ed25519 key generation failed")?;
            let der: Pkcs8V1Der = kp.as_der().map_err(|_| "ed25519 encoding failed")?;
            let pubraw = kp.public_key().as_ref().to_vec();
            let spki: PublicKeyX509Der = kp.public_key().as_der().map_err(|_| "ed25519 public encoding failed")?;
            let mut blob = Vec::new();
            ssh_string(&mut blob, b"ssh-ed25519");
            ssh_string(&mut blob, &pubraw);
            meta.insert("public_b64".into(), json!(b64(&pubraw)));
            meta.insert("public_openssh".into(), json!(format!("ssh-ed25519 {}", b64(&blob))));
            meta.insert("public_pem".into(), json!(pem("PUBLIC KEY", spki.as_ref())));
            Ok((pem("PRIVATE KEY", der.as_ref()).into_bytes(), "keypair", meta))
        }
        "x25519" => {
            let sk = aws_lc_rs::agreement::PrivateKey::generate(&aws_lc_rs::agreement::X25519).map_err(|_| "x25519 key generation failed")?;
            let pk = sk.compute_public_key().map_err(|_| "x25519 public key failed")?;
            let seed: Curve25519SeedBin = sk.as_be_bytes().map_err(|_| "x25519 encoding failed")?;
            // RFC 8410 PKCS#8 / SubjectPublicKeyInfo for X25519 (OID 1.3.101.110)
            let mut der = vec![0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e, 0x04, 0x22, 0x04, 0x20];
            der.extend_from_slice(seed.as_ref());
            let mut spki = vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e, 0x03, 0x21, 0x00];
            spki.extend_from_slice(pk.as_ref());
            meta.insert("public_b64".into(), json!(b64(pk.as_ref())));
            meta.insert("public_pem".into(), json!(pem("PUBLIC KEY", &spki)));
            Ok((pem("PRIVATE KEY", &der).into_bytes(), "keypair", meta))
        }
        "rsa-4096" => {
            let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa4096).map_err(|_| "RSA key generation failed")?;
            let der: Pkcs8V1Der = kp.as_der().map_err(|_| "RSA encoding failed")?;
            let spki: PublicKeyX509Der = kp.public_key().as_der().map_err(|_| "RSA public encoding failed")?;
            meta.insert("public_pem".into(), json!(pem("PUBLIC KEY", spki.as_ref())));
            Ok((pem("PRIVATE KEY", der.as_ref()).into_bytes(), "keypair", meta))
        }
        other => Err(format!("unknown kind \"{other}\" (password, ed25519, x25519, rsa-4096)")),
    }
}

async fn generate(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let name = arg(&p, "name").ok_or("secret.generate needs \"name\"")?.to_string();
    if !crate::secrets::valid_name(&name) {
        return Err(format!("bad secret name \"{name}\" (letters, digits, . _ -; at most 64)"));
    }
    let kind = arg(&p, "kind").unwrap_or("password").to_string();
    if k.secrets.meta(&name).is_some() && p.get("replace").and_then(Value::as_bool) != Some(true) {
        return Err(format!("secret \"{name}\" exists (pass replace: true to generate a new one)"));
    }
    let length = p.get("length").and_then(Value::as_u64).unwrap_or(32) as usize;
    let k2 = kind.clone();
    let (value, store_kind, mut meta) = tokio::task::spawn_blocking(move || generate_value(&k2, length)).await.map_err(|e| e.to_string())??;
    meta.insert("label".into(), json!(arg(&p, "label").unwrap_or(&name)));
    meta.insert("generated".into(), json!(true));
    meta.insert("requested_by".into(), json!(c.id));
    k.secrets.put(&name, store_kind, &value, meta.clone())?;
    k.research.log(&c.id, Kind::Secret, json!({ "phase": "generated", "name": name, "kind": kind }));
    k.emit_snapshot();
    let mut out = json!({ "ok": true, "name": name, "kind": kind });
    for key in ["public_b64", "public_openssh", "public_pem", "length"] {
        if let Some(v) = meta.get(key) {
            out[key] = v.clone();
        }
    }
    Ok(out)
}

pub fn primitive() -> Primitive {
    let rel = handler(release);
    let rev = handler(revoke);
    let gen_ = handler(generate);
    Primitive {
        name: "secret_release",
        purpose: "Give one module's own processes a secret it must hold itself (env or a private file in the VM; Confirmed card each time), and generate passwords / key pairs in the Keychain (public part returned).",
        example: r#"{ "host": "secret.release", "name": "proton-password", "module": "proton", "as": "env:BRIDGE_PASSWORD" } · { "host": "secret.generate", "name": "deploy-key", "kind": "ed25519", "label": "Deploy key" } → { public_openssh }"#,
        class: Class::Confirmed,
        actions: vec![
            Action { name: "secret.release", args: "{ name, module, as: \"env:NAME\" | \"file:<path>\" } — Confirmed card; env at every start of the module's server + services, file 0600 at /run/openapp/secrets/<module>/<path> ($OPENAPP_SECRETS_DIR)", handler: rel.clone() },
            Action { name: "secret.revoke", args: "{ name, module, as? } — take a released secret back (processes restart)", handler: rev.clone() },
            Action { name: "secret.generate", args: "{ name, kind: password|ed25519|x25519|rsa-4096, label?, length?, replace? } → { public_pem?, public_openssh?, public_b64? } (private part stays in the Keychain)", handler: gen_.clone() },
        ],
        endpoints: vec![
            Endpoint { method: "secret.release", args: "{ name, as } (for the calling module)", handler: rel },
            Endpoint { method: "secret.revoke", args: "{ name, as? }", handler: rev },
            Endpoint { method: "secret.generate", args: "{ name, kind, label? }", handler: gen_ },
        ],
        data: vec![DataSource {
            name: "secret_releases",
            doc: "list items [{ id, title: secret name, subtitle: \"→ module as env:NAME\", icon: \"key-round\", name, module, as }] — secrets given to modules (never values)",
            get: data_fn(|k| {
                Value::Array(
                    releases(k)
                        .iter()
                        .map(|r| json!({ "id": format!("{}:{}:{}", r["name"].as_str().unwrap_or(""), r["module"].as_str().unwrap_or(""), r["as"].as_str().unwrap_or("")), "title": r["name"], "subtitle": format!("→ {} as {}", r["module"].as_str().unwrap_or(""), r["as"].as_str().unwrap_or("")), "icon": "key-round", "name": r["name"], "module": r["module"], "as": r["as"], "ts": r["ts"] }))
                        .collect(),
                )
            }),
        }],
        env: Some(Arc::new(env_for)),
        describe_keys: vec![(
            "secret_release",
            data_fn(|k| json!({
                "doc": "Default stays host injection (net.fetch adds the secret on the Mac). Only when a program must hold the credential itself (e.g. a bridge daemon in the VM): secret.release { name, module, as }. The user confirms every release.",
                "releases": releases(k),
            })),
        )],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_parse_and_generate() {
        assert_eq!(As::parse("env:BRIDGE_PASSWORD").unwrap(), As::Env("BRIDGE_PASSWORD".into()));
        assert_eq!(As::parse("file:bridge/pw").unwrap(), As::File("bridge/pw".into()));
        assert!(As::parse("env:PATH").is_err() && As::parse("env:OPENAPP_X").is_err() && As::parse("env:lower").is_err());
        assert!(As::parse("file:../x").is_err() && As::parse("file:/etc/x").is_err() && As::parse("x").is_err());
        let (pw, kind, m) = generate_value("password", 24).unwrap();
        assert_eq!((pw.len(), kind, m["length"].as_u64()), (24, "password", Some(24)));
        let (pw2, _, _) = generate_value("password", 24).unwrap();
        assert_ne!(pw, pw2);
        let (sk, kind, m) = generate_value("ed25519", 0).unwrap();
        assert_eq!(kind, "keypair");
        assert!(String::from_utf8(sk).unwrap().starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(m["public_openssh"].as_str().unwrap().starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5"));
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(m["public_b64"].as_str().unwrap()).unwrap().len(), 32);
        let (_, _, m) = generate_value("x25519", 0).unwrap();
        assert!(m["public_pem"].as_str().unwrap().contains("PUBLIC KEY"));
    }
}
