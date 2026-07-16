//! package-signer — first-party Ed25519 signing of release bundles (TRUST-001).
//!
//! A signed `package-manifest.json` pins the SHA-256 + size of every file in a
//! bundle under a publisher key whose PUBLIC half is compiled into FormLogic
//! Desktop. Desktop verifies the signature and every digest before staging AND
//! again before launch, and quarantines anything tampered — so a dropped or
//! swapped executable in a plugin directory cannot run.
//!
//! The manifest signs the RAW payload bytes (carried base64 in the envelope),
//! so there is no JSON-canonicalisation surface at all: what was signed is
//! byte-for-byte what the verifier hashes.
//!
//! Commands:
//!   keygen                                    mint a keypair (seed hex on stdout, pub b64)
//!   sign   --dir D --name N --version V --key-id ID (--key-file F | --key-env VAR)
//!   verify --dir D --pubkey B64
//!
//! The signing SEED (32-byte hex) is a secret: keep it in the CI secret store /
//! an operator key file, never in the repository.

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "package-manifest.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub format: u32,
    pub alg: String,
    pub key_id: String,
    /// base64(standard) of the exact payload JSON bytes that were signed.
    pub payload_b64: String,
    /// base64url (no pad) detached signature over those bytes.
    pub signature: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub name: String,
    pub version: String,
    pub created_at: String,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// Forward-slash path relative to the bundle root.
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err("hex length must be even".into());
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex_encode(&hasher.finalize()), size))
}

/// Every regular file under `dir` (recursive), relative forward-slash paths,
/// excluding the manifest itself. Sorted for a deterministic payload.
fn collect_files(dir: &Path) -> Result<Vec<FileEntry>, String> {
    fn walk(root: &Path, base: &Path, out: &mut Vec<FileEntry>) -> Result<(), String> {
        for entry in
            std::fs::read_dir(root).map_err(|e| format!("read_dir {}: {e}", root.display()))?
        {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let ft = entry.file_type().map_err(|e| e.to_string())?;
            if ft.is_symlink() {
                return Err(format!("refusing to sign a symlink: {}", path.display()));
            }
            if ft.is_dir() {
                walk(&path, base, out)?;
                continue;
            }
            let rel = path
                .strip_prefix(base)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if rel == MANIFEST_FILE {
                continue;
            }
            let (sha256, size) = sha256_file(&path)?;
            out.push(FileEntry {
                path: rel,
                sha256,
                size,
            });
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    if out.is_empty() {
        return Err(format!("no files to sign under {}", dir.display()));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub fn sign_dir(
    dir: &Path,
    name: &str,
    version: &str,
    key_id: &str,
    seed: &[u8; 32],
    created_at: String,
) -> Result<Envelope, String> {
    let files = collect_files(dir)?;
    let payload = Payload {
        name: name.to_string(),
        version: version.to_string(),
        created_at,
        files,
    };
    let payload_bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let key = SigningKey::from_bytes(seed);
    let sig = key.sign(&payload_bytes);
    Ok(Envelope {
        format: 1,
        alg: "Ed25519".into(),
        key_id: key_id.to_string(),
        payload_b64: base64::engine::general_purpose::STANDARD.encode(&payload_bytes),
        signature: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes()),
    })
}

/// Verify an envelope's signature + every file digest against `dir`.
/// Mirrors (and is test-locked against) FormLogic Desktop's verifier.
pub fn verify_dir(dir: &Path, pubkey_b64: &str) -> Result<Payload, String> {
    let text = std::fs::read_to_string(dir.join(MANIFEST_FILE))
        .map_err(|e| format!("read {MANIFEST_FILE}: {e}"))?;
    let envelope: Envelope =
        serde_json::from_str(&text).map_err(|e| format!("parse envelope: {e}"))?;
    if envelope.alg != "Ed25519" {
        return Err(format!("unsupported alg {:?}", envelope.alg));
    }
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.payload_b64.trim())
        .map_err(|e| format!("payload base64: {e}"))?;
    let sig_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(envelope.signature.trim().trim_end_matches('='))
        .map_err(|e| format!("signature base64: {e}"))?;
    let pk_bytes = base64::engine::general_purpose::STANDARD
        .decode(pubkey_b64.trim())
        .map_err(|e| format!("pubkey base64: {e}"))?;
    let pk = VerifyingKey::from_bytes(
        &<[u8; 32]>::try_from(pk_bytes.as_slice()).map_err(|_| "pubkey must be 32 bytes")?,
    )
    .map_err(|e| format!("invalid pubkey: {e}"))?;
    let sig = Signature::from_bytes(
        &<[u8; 64]>::try_from(sig_bytes.as_slice()).map_err(|_| "signature must be 64 bytes")?,
    );
    pk.verify(&payload_bytes, &sig)
        .map_err(|e| format!("signature verification failed: {e}"))?;

    let payload: Payload =
        serde_json::from_slice(&payload_bytes).map_err(|e| format!("parse payload: {e}"))?;
    // Every listed file must exist with the exact digest + size…
    for f in &payload.files {
        if f.path.contains("..") {
            return Err(format!("manifest lists a traversal path {:?}", f.path));
        }
        let disk = dir.join(&f.path);
        let (sha, size) = sha256_file(&disk).map_err(|e| format!("{}: {e}", f.path))?;
        if sha != f.sha256.to_lowercase() || size != f.size {
            return Err(format!("digest mismatch for {}", f.path));
        }
    }
    // …and no UNLISTED file may exist (a dropped extra binary is a tamper).
    let on_disk = collect_files(dir)?;
    for d in &on_disk {
        if !payload.files.iter().any(|f| f.path == d.path) {
            return Err(format!("unlisted file present: {}", d.path));
        }
    }
    Ok(payload)
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn load_seed(args: &[String]) -> Result<[u8; 32], String> {
    let hex = if let Some(file) = arg_value(args, "--key-file") {
        std::fs::read_to_string(&file).map_err(|e| format!("read key file {file}: {e}"))?
    } else if let Some(var) = arg_value(args, "--key-env") {
        std::env::var(&var).map_err(|_| format!("env var {var} is not set"))?
    } else {
        return Err("provide --key-file <path> or --key-env <VAR>".into());
    };
    let bytes = hex_decode(&hex)?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| "signing seed must be 32 bytes of hex".into())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let result = match cmd {
        "keygen" => keygen(),
        "sign" => cmd_sign(&args),
        "verify" => cmd_verify(&args),
        _ => Err("usage: package-signer keygen | sign --dir D --name N --version V --key-id ID (--key-file F | --key-env VAR) | verify --dir D --pubkey B64".into()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn keygen() -> Result<(), String> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| e.to_string())?;
    let key = SigningKey::from_bytes(&seed);
    println!("private seed (hex, SECRET — CI secret / operator key file):");
    println!("{}", hex_encode(&seed));
    println!("public key (base64 — pin in FormLogic Desktop TRUSTED_PUBLISHER_KEYS):");
    println!(
        "{}",
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
    );
    Ok(())
}

fn cmd_sign(args: &[String]) -> Result<(), String> {
    let dir = PathBuf::from(arg_value(args, "--dir").ok_or("--dir required")?);
    let name = arg_value(args, "--name").ok_or("--name required")?;
    let version = arg_value(args, "--version").ok_or("--version required")?;
    let key_id = arg_value(args, "--key-id").ok_or("--key-id required")?;
    let seed = load_seed(args)?;
    // RFC3339 UTC without a chrono dependency.
    let created_at = httpdate_now();
    let envelope = sign_dir(&dir, &name, &version, &key_id, &seed, created_at)?;
    let out = dir.join(MANIFEST_FILE);
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&envelope).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write {}: {e}", out.display()))?;
    // Self-check with the just-derived public key so a bad write can't ship.
    let pubkey = base64::engine::general_purpose::STANDARD
        .encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes());
    let payload = verify_dir(&dir, &pubkey)?;
    println!(
        "signed {} v{} — {} files, keyId {key_id}, manifest {}",
        payload.name,
        payload.version,
        payload.files.len(),
        out.display()
    );
    Ok(())
}

fn cmd_verify(args: &[String]) -> Result<(), String> {
    let dir = PathBuf::from(arg_value(args, "--dir").ok_or("--dir required")?);
    let pubkey = arg_value(args, "--pubkey").ok_or("--pubkey required")?;
    let payload = verify_dir(&dir, &pubkey)?;
    println!(
        "OK: {} v{} — {} files verified",
        payload.name,
        payload.version,
        payload.files.len()
    );
    Ok(())
}

/// Seconds-precision UTC RFC3339 (civil-from-days, dependency-free).
fn httpdate_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("pkg-signer-{tag}-{n}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const SEED: [u8; 32] = [7u8; 32];

    fn pubkey() -> String {
        base64::engine::general_purpose::STANDARD
            .encode(SigningKey::from_bytes(&SEED).verifying_key().to_bytes())
    }

    fn sign_into(dir: &Path) {
        let env = sign_dir(dir, "test-bundle", "1.2.3", "test-key", &SEED, "t0".into()).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_string(&env).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn sign_verify_round_trip() {
        let d = tmp("ok");
        std::fs::write(d.join("plugin.exe"), b"binary bytes").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("model.onnx"), b"weights").unwrap();
        sign_into(&d);
        let payload = verify_dir(&d, &pubkey()).unwrap();
        assert_eq!(payload.name, "test-bundle");
        assert_eq!(payload.files.len(), 2);
        assert!(payload.files.iter().any(|f| f.path == "sub/model.onnx"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tampered_file_fails() {
        let d = tmp("tamper");
        std::fs::write(d.join("plugin.exe"), b"binary bytes").unwrap();
        sign_into(&d);
        std::fs::write(d.join("plugin.exe"), b"EVIL bytes!!").unwrap();
        let err = verify_dir(&d, &pubkey()).unwrap_err();
        assert!(err.contains("digest mismatch"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn extra_dropped_file_fails() {
        let d = tmp("extra");
        std::fs::write(d.join("plugin.exe"), b"binary bytes").unwrap();
        sign_into(&d);
        std::fs::write(d.join("evil.dll"), b"hijack").unwrap();
        let err = verify_dir(&d, &pubkey()).unwrap_err();
        assert!(err.contains("unlisted file"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_listed_file_fails() {
        let d = tmp("missing");
        std::fs::write(d.join("plugin.exe"), b"binary bytes").unwrap();
        std::fs::write(d.join("helper.exe"), b"helper").unwrap();
        sign_into(&d);
        std::fs::remove_file(d.join("helper.exe")).unwrap();
        assert!(verify_dir(&d, &pubkey()).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn wrong_key_fails() {
        let d = tmp("wrongkey");
        std::fs::write(d.join("plugin.exe"), b"binary bytes").unwrap();
        sign_into(&d);
        let other = base64::engine::general_purpose::STANDARD.encode(
            SigningKey::from_bytes(&[9u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        let err = verify_dir(&d, &other).unwrap_err();
        assert!(err.contains("signature verification failed"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tampered_payload_fails_signature() {
        let d = tmp("payload");
        std::fs::write(d.join("plugin.exe"), b"binary bytes").unwrap();
        let mut env = sign_dir(&d, "test-bundle", "1.2.3", "test-key", &SEED, "t0".into()).unwrap();
        // Swap the payload for one claiming different bytes — signature must fail
        // BEFORE any digest is even consulted.
        let forged = Payload {
            name: "test-bundle".into(),
            version: "1.2.3".into(),
            created_at: "t0".into(),
            files: vec![FileEntry {
                path: "plugin.exe".into(),
                sha256: "00".repeat(32),
                size: 5,
            }],
        };
        env.payload_b64 =
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&forged).unwrap());
        std::fs::write(d.join(MANIFEST_FILE), serde_json::to_string(&env).unwrap()).unwrap();
        let err = verify_dir(&d, &pubkey()).unwrap_err();
        assert!(err.contains("signature verification failed"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
