//! Password hashes for users who sign in with HTTP Basic (#825), as Fuseki's Shiro users do.
//!
//! Two forms are read:
//! - `$pbkdf2-sha256$ROUNDS$SALT$HASH`: PBKDF2-HMAC-SHA256, salt and hash in passlib's
//!   base64 (`.` for `+`, no padding; standard base64 is read too). `tripletrail hash-password`
//!   writes this form, with [`ROUNDS`] rounds, and passlib reads and writes it.
//! - `$shiro1$SHA-256$ROUNDS$SALT$HASH` (or `SHA-512`): Apache Shiro's hashes, as
//!   `shiro hasher` prints them for a Fuseki `shiro.ini`, so its users move over as they are.

use sha2::{Digest, Sha256, Sha512};
use std::num::NonZeroU32;

/// PBKDF2 rounds of a new hash: OWASP's advice for PBKDF2-HMAC-SHA256 (2023).
pub const ROUNDS: u32 = 600_000;

/// A stored password hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasswordHash {
    Pbkdf2Sha256 {
        rounds: NonZeroU32,
        salt: Vec<u8>,
        hash: Vec<u8>,
    },
    /// Shiro's iterated digest: `digest(salt ‖ password)`, then the digest of that, until
    /// `rounds` digests were taken.
    Shiro1 {
        sha512: bool,
        rounds: NonZeroU32,
        salt: Vec<u8>,
        hash: Vec<u8>,
    },
}

impl PasswordHash {
    /// Reads a hash in one of the forms above.
    pub fn parse(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.split('$').collect();
        let rounds = |r: &str| {
            r.parse::<NonZeroU32>()
                .map_err(|_| format!("{r:?} is not a number of rounds"))
        };
        let bytes = |b: &str, what: &str| {
            base64_decode(b)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("the {what} is not base64"))
        };
        match parts.as_slice() {
            ["", "pbkdf2-sha256", r, salt, hash] => Ok(Self::Pbkdf2Sha256 {
                rounds: rounds(r)?,
                salt: bytes(salt, "salt")?,
                hash: bytes(hash, "hash")?,
            }),
            [
                "",
                "shiro1",
                algorithm @ ("SHA-256" | "SHA-512"),
                r,
                salt,
                hash,
            ] => Ok(Self::Shiro1 {
                sha512: *algorithm == "SHA-512",
                rounds: rounds(r)?,
                salt: bytes(salt, "salt")?,
                hash: bytes(hash, "hash")?,
            }),
            ["", "shiro1", algorithm, ..] => Err(format!(
                "Shiro hashes with {algorithm} are not supported (SHA-256 and SHA-512 are)"
            )),
            _ => Err(
                "a password must be a hash: $pbkdf2-sha256$… (tripletrail hash-password prints one) \
                 or Shiro's $shiro1$SHA-256$…"
                    .to_owned(),
            ),
        }
    }

    /// Whether `password` is the one hashed, compared in constant time.
    pub fn verify(&self, password: &[u8]) -> bool {
        match self {
            Self::Pbkdf2Sha256 { rounds, salt, hash } => {
                constant_time_eq(&pbkdf2_sha256(password, salt, *rounds, hash.len()), hash)
            }
            Self::Shiro1 {
                sha512,
                rounds,
                salt,
                hash,
            } => {
                let digest = if *sha512 {
                    shiro_digest::<Sha512>(salt, password, *rounds)
                } else {
                    shiro_digest::<Sha256>(salt, password, *rounds)
                };
                constant_time_eq(&digest, hash)
            }
        }
    }
}

/// Shiro's iterated digest of a salted password.
fn shiro_digest<D: Digest>(salt: &[u8], password: &[u8], rounds: NonZeroU32) -> Vec<u8> {
    let mut digest = D::new()
        .chain_update(salt)
        .chain_update(password)
        .finalize();
    for _ in 1..rounds.get() {
        digest = D::digest(&digest);
    }
    digest.to_vec()
}

/// PBKDF2-HMAC-SHA256 (RFC 8018), `length` bytes. The HMAC's inner and outer hashes of the
/// key are computed once and cloned for each of the many rounds.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], rounds: NonZeroU32, length: usize) -> Vec<u8> {
    let mut key = [0u8; 64];
    if password.len() > 64 {
        key[..32].copy_from_slice(&Sha256::digest(password));
    } else {
        key[..password.len()].copy_from_slice(password);
    }
    let padded = |byte: u8| Sha256::new().chain_update(key.map(|b| b ^ byte));
    let (inner, outer) = (padded(0x36), padded(0x5c));
    let hmac = |parts: &[&[u8]]| -> [u8; 32] {
        let mut h = inner.clone();
        for p in parts {
            h.update(p);
        }
        outer.clone().chain_update(h.finalize()).finalize().into()
    };
    let mut out = Vec::with_capacity(length);
    for block in 1..=length.div_ceil(32) as u32 {
        let mut u = hmac(&[salt, &block.to_be_bytes()]);
        let mut t = u;
        for _ in 1..rounds.get() {
            u = hmac(&[&u]);
            for (t, u) in t.iter_mut().zip(&u) {
                *t ^= u;
            }
        }
        out.extend_from_slice(&t);
    }
    out.truncate(length);
    out
}

/// A new `$pbkdf2-sha256$` hash of `password`, with a random 16-byte salt.
pub fn hash(password: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    Ok(hash_with(
        password,
        &salt,
        NonZeroU32::new(ROUNDS).expect("not zero"),
    ))
}

fn hash_with(password: &str, salt: &[u8], rounds: NonZeroU32) -> String {
    let out = pbkdf2_sha256(password.as_bytes(), salt, rounds, 32);
    format!(
        "$pbkdf2-sha256${rounds}${}${}",
        ab64_encode(salt),
        ab64_encode(&out)
    )
}

/// The `[users]` section of a Shiro INI file (`name = password, role1, role2`) as `auth.users`
/// entries: roles become groups, a `$shiro1$` hash is kept, and a password in the clear is
/// hashed with [`hash`]. Fuseki's usual `admin` role becomes the group `tripletrail:admin`.
pub fn import_shiro(ini: &str) -> Result<Vec<serde_json::Value>, String> {
    let mut users = Vec::new();
    let mut in_users = false;
    for (n, line) in ini.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            in_users = line.eq_ignore_ascii_case("[users]");
            continue;
        }
        if !in_users {
            continue;
        }
        let (name, rest) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected name = password, roles", n + 1))?;
        let mut parts = rest.split(',').map(str::trim);
        let password = parts.next().unwrap_or_default();
        if password.is_empty() {
            return Err(format!("line {}: {} has no password", n + 1, name.trim()));
        }
        let password = if password.starts_with("$shiro1$") {
            PasswordHash::parse(password).map_err(|e| format!("line {}: {e}", n + 1))?;
            password.to_owned()
        } else {
            hash(password)?
        };
        let groups: Vec<String> = parts
            .filter(|r| !r.is_empty())
            .map(|r| {
                if r == "admin" {
                    "tripletrail:admin".to_owned()
                } else {
                    r.to_owned()
                }
            })
            .collect();
        users.push(serde_json::json!({
            "name": name.trim(),
            "password": password,
            "groups": groups,
        }));
    }
    if users.is_empty() {
        return Err("no [users] in the file".to_owned());
    }
    Ok(users)
}

/// A hash that matches no password, to spend the same time on an unknown user as on a
/// known one.
pub fn decoy() -> &'static PasswordHash {
    static DECOY: std::sync::OnceLock<PasswordHash> = std::sync::OnceLock::new();
    DECOY.get_or_init(|| PasswordHash::Pbkdf2Sha256 {
        rounds: NonZeroU32::new(ROUNDS).expect("not zero"),
        salt: vec![0; 16],
        hash: vec![0; 32],
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// passlib's base64: `.` for `+`, no padding.
fn ab64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            let c = ALPHABET[((n >> (18 - 6 * i)) & 63) as usize];
            out.push(if c == b'+' { '.' } else { c as char });
        }
    }
    out
}

/// Standard base64, with or without padding; passlib's `.` is read as `+`.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut bits = 0u32;
    let mut count = 0;
    for c in text.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'.' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    // A single leftover character (6 bits) cannot end valid base64.
    (count < 6).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_matches_the_published_vectors() {
        // RFC 7914, section 11: PBKDF2-HMAC-SHA256 of "passwd" with salt "salt", 1 round.
        let h = hash_with("passwd", b"salt", NonZeroU32::new(1).unwrap());
        let PasswordHash::Pbkdf2Sha256 { hash, .. } = PasswordHash::parse(&h).unwrap() else {
            panic!("{h}")
        };
        assert_eq!(
            hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
        let parsed = PasswordHash::parse(&h).unwrap();
        assert!(parsed.verify(b"passwd"));
        assert!(!parsed.verify(b"passwe"));
    }

    #[test]
    fn pbkdf2_matches_rfc_7914_and_long_passwords() {
        let hex = |b: Vec<u8>| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let n = |r| NonZeroU32::new(r).unwrap();
        // RFC 7914, section 11, both PBKDF2-HMAC-SHA256 vectors (two output blocks each).
        assert_eq!(
            hex(pbkdf2_sha256(b"passwd", b"salt", n(1), 64)),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
             49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
        assert_eq!(
            hex(pbkdf2_sha256(b"Password", b"NaCl", n(80000), 64)),
            "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56\
             a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d"
        );
        // A password longer than SHA-256's block is hashed first (Python's hashlib).
        assert_eq!(
            hex(pbkdf2_sha256(&[b'p'; 100], b"salt", n(3), 40)),
            "f598272d35e2ca276ac07694cf01636c4d643ad3075956477cfdd83eda46d9f6ff98cc0164001b63"
        );
    }

    #[test]
    fn reads_shiro_sha512_hashes() {
        // SHA-512 of "salt" ‖ "pw", four digests in all (Python's hashlib).
        let h = PasswordHash::parse(
            "$shiro1$SHA-512$4$c2FsdA==$rMQ7ERZuwtAbRruDIn9OPXKlcfgqILQ/SCTz1rZ2M8MkBObwcR5S+d7FC5x9j4KxxQD0nRAqVEODgypIXDu0uw==",
        )
        .unwrap();
        assert!(h.verify(b"pw"));
        assert!(!h.verify(b"pw "));
    }

    #[test]
    fn reads_passlib_hashes() {
        // passlib's form of PBKDF2-HMAC-SHA256("password", "saltsaltsaltsalt", 1000), the hash
        // computed with Python's hashlib.pbkdf2_hmac.
        let line = "$pbkdf2-sha256$1000$c2FsdHNhbHRzYWx0c2FsdA$8nX7hwFEzIB8aPajJTYK8weHQc5Ngz0pFVAKvSu4jQA";
        let h = PasswordHash::parse(line).unwrap();
        let PasswordHash::Pbkdf2Sha256 { rounds, salt, .. } = &h else {
            panic!()
        };
        assert_eq!(rounds.get(), 1000);
        assert_eq!(salt, b"saltsaltsaltsalt");
        assert_eq!(hash_with("password", b"saltsaltsaltsalt", *rounds), line);
        assert!(h.verify(b"password"));
        assert!(!h.verify(b"Password"));
    }

    #[test]
    fn reads_shiro_hashes() {
        // SHA-256 of "salt" ‖ "pw", then of that, three digests in all.
        let mut d = Sha256::digest(b"saltpw");
        for _ in 1..3 {
            d = Sha256::digest(d);
        }
        let line = format!("$shiro1$SHA-256$3$c2FsdA==${}", ab64_encode(d.as_ref()));
        let h = PasswordHash::parse(&line).unwrap();
        assert!(h.verify(b"pw"));
        assert!(!h.verify(b"pW"));
        // Two digests is a different hash.
        let line = format!("$shiro1$SHA-256$2$c2FsdA==${}", ab64_encode(d.as_ref()));
        assert!(!PasswordHash::parse(&line).unwrap().verify(b"pw"));
    }

    #[test]
    fn new_hashes_verify_and_differ() {
        let a = hash("secret").unwrap();
        assert!(a.starts_with("$pbkdf2-sha256$600000$"), "{a}");
        assert!(PasswordHash::parse(&a).unwrap().verify(b"secret"));
        // A new random salt each time.
        let salts: std::collections::HashSet<String> = (0..2)
            .map(|_| {
                let mut salt = [0u8; 16];
                getrandom::fill(&mut salt).unwrap();
                ab64_encode(&salt)
            })
            .collect();
        assert_eq!(salts.len(), 2);
        assert!(!decoy().verify(b""));
    }

    #[test]
    fn refuses_what_is_not_a_hash() {
        for bad in [
            "secret",
            "$pbkdf2-sha256$0$c2FsdA$c2FsdA",
            "$pbkdf2-sha256$10$!!$c2FsdA",
            "$shiro1$MD5$1$c2FsdA$c2FsdA",
            "$2b$12$abcdefghijklmnopqrstuv",
        ] {
            assert!(PasswordHash::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn imports_shiro_users() {
        let users = import_shiro(
            "[main]\nfoo = bar\n\n[users]\n# the admin\nadmin = pw, admin\n\
             jan = $shiro1$SHA-256$3$c2FsdA==$c2FsdA==, editors , readers\n[urls]\n/** = anon\n",
        )
        .unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0]["name"], "admin");
        assert_eq!(users[0]["groups"], serde_json::json!(["tripletrail:admin"]));
        let hashed = users[0]["password"].as_str().unwrap();
        assert!(
            PasswordHash::parse(hashed).unwrap().verify(b"pw"),
            "{hashed}"
        );
        assert_eq!(users[1]["password"], "$shiro1$SHA-256$3$c2FsdA==$c2FsdA==");
        assert_eq!(
            users[1]["groups"],
            serde_json::json!(["editors", "readers"])
        );
        assert!(import_shiro("[main]\na = b\n").is_err());
    }

    #[test]
    fn base64_round_trips() {
        for n in 0..10 {
            let bytes: Vec<u8> = (0..n).map(|i: u8| i.wrapping_mul(37)).collect();
            assert_eq!(base64_decode(&ab64_encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_decode("dXNlcjp0b2tlbg==").unwrap(), b"user:token");
        assert!(base64_decode("a").is_none());
    }
}
