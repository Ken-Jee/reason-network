//! Reason sign-in proofs: a statement signed by Reason that a machine's key (its endpoint id) belongs to a Reason
//! account, and the name that account goes by. A compact JWS (Ed25519, `EdDSA`), checked here against Reason's public
//! keys, which are compiled in: verifying one never calls Reason. Every machine checks every proof it is shown itself.

use anyhow::{Result, anyhow, bail};
use data_encoding::BASE64URL_NOPAD;
use iroh::{EndpointId, PublicKey, Signature};
use serde::{Deserialize, Serialize};

const ISSUER: &str = "https://reasonmachines.com";
const AUDIENCE: &str = "reason-network";
const TYPE: &str = "reason-network-proof";
/// Longer than any proof Reason signs: refused before decoding.
const MAX_LEN: usize = 4096;
/// A proof issued further ahead than this is refused (a broken clock, or worse).
const MAX_SKEW_SECS: i64 = 86_400;

/// Reason's proof-signing public keys: key id, then the Ed25519 key in base64url. A placeholder until the production
/// key exists; in debug builds `REASON_NETWORK_TRUST_KEYS` (a JWKS) adds keys, for tests.
const KEYS: &[(&str, &str)] = &[];

/// What a proof says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    /// The Reason account.
    pub sub: String,
    /// The account's name in teams: 1 to 40 of `a-z`, `0-9` and `-`.
    pub name: String,
    pub email: String,
    /// The machine's endpoint id, as the engine prints it.
    pub endpoint: String,
    /// Issued and expires, in seconds since the epoch.
    pub iat: i64,
    pub exp: i64,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: String,
    typ: String,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    #[serde(flatten)]
    proof: Proof,
}

#[cfg(any(test, debug_assertions))]
#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[cfg(any(test, debug_assertions))]
#[derive(Deserialize)]
struct Jwk {
    kty: String,
    crv: String,
    kid: String,
    x: String,
}

fn key(x: &str) -> Option<PublicKey> {
    let bytes = BASE64URL_NOPAD.decode(x.trim_end_matches('=').as_bytes()).ok()?;
    PublicKey::from_bytes(&bytes.try_into().ok()?).ok()
}

/// The keys a proof may be signed with: the compiled-in ones and, in debug builds, those in
/// `REASON_NETWORK_TRUST_KEYS` (release builds trust only the compiled-in keys), and in tests a test key.
fn trusted() -> &'static [(String, PublicKey)] {
    static KEYS_: std::sync::OnceLock<Vec<(String, PublicKey)>> = std::sync::OnceLock::new();
    KEYS_.get_or_init(|| {
        #[allow(unused_mut)]
        let mut out: Vec<(String, PublicKey)> =
            KEYS.iter().filter_map(|(kid, x)| Some((kid.to_string(), key(x)?))).collect();
        #[cfg(any(test, debug_assertions))]
        if let Some(jwks) = std::env::var_os("REASON_NETWORK_TRUST_KEYS")
            && let Ok(jwks) = serde_json::from_str::<Jwks>(&jwks.to_string_lossy())
        {
            out.extend(
                jwks.keys
                    .into_iter()
                    .filter(|k| k.kty == "OKP" && k.crv == "Ed25519")
                    .filter_map(|k| Some((k.kid, key(&k.x)?))),
            );
        }
        #[cfg(test)]
        out.push((tests::KID.into(), tests::signer().public()));
        out
    })
}

/// Checks `jws` for the machine `endpoint`: Reason's signature, issuer, audience, expiry, and that it names this
/// machine.
pub fn verify(jws: &str, endpoint: &EndpointId) -> Result<Proof> {
    verify_with(jws, endpoint, crate::util::now_ms() / 1000, trusted(), true)
}

/// The same, except that a proof that expired still counts: for who may hold a name in the team, which mustn't change
/// as time passes (or differ between machines whose clocks do). Only "verified" goes by `verify`.
pub fn verify_ignoring_expiry(jws: &str, endpoint: &EndpointId) -> Result<Proof> {
    verify_with(jws, endpoint, crate::util::now_ms() / 1000, trusted(), false)
}

fn verify_with(
    jws: &str,
    endpoint: &EndpointId,
    now: i64,
    keys: &[(String, PublicKey)],
    expiry: bool,
) -> Result<Proof> {
    let bad = || anyhow!("this sign-in proof is damaged");
    if jws.len() > MAX_LEN {
        bail!("this sign-in proof is too long");
    }
    let mut parts = jws.trim().split('.');
    let (Some(h), Some(c), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    let decode = |p: &str| BASE64URL_NOPAD.decode(p.as_bytes()).map_err(|_| bad());
    let header: Header = serde_json::from_slice(&decode(h)?).map_err(|_| bad())?;
    if header.alg != "EdDSA" || header.typ != TYPE {
        bail!("not a Reason Network sign-in proof");
    }
    let Some((_, key)) = keys.iter().find(|(kid, _)| *kid == header.kid) else {
        bail!("this sign-in proof is signed with a key this version doesn't know (update Reason Network)");
    };
    let sig: [u8; Signature::LENGTH] = decode(s)?.try_into().map_err(|_| bad())?;
    if key.verify(format!("{h}.{c}").as_bytes(), &Signature::from_bytes(&sig)).is_err() {
        bail!("this sign-in proof's signature doesn't verify");
    }
    let claims: Claims = serde_json::from_slice(&decode(c)?).map_err(|_| bad())?;
    if claims.iss != ISSUER || claims.aud != AUDIENCE {
        bail!("this sign-in proof isn't from Reason Network");
    }
    let p = claims.proof;
    if expiry && p.exp <= now {
        bail!("this sign-in proof expired; sign in again");
    }
    if p.iat > now.saturating_add(MAX_SKEW_SECS) {
        bail!("this sign-in proof is dated in the future (is this computer's clock right?)");
    }
    if p.endpoint != endpoint.to_string() {
        bail!("this sign-in proof is for another machine");
    }
    if p.name.is_empty()
        || p.name.len() > 40
        || !p.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("this sign-in proof's name isn't a valid name");
    }
    Ok(p)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use iroh::SecretKey;
    use serde_json::json;

    pub const KID: &str = "test-1";

    /// The key test builds trust (see `trusted`), standing in for Reason's.
    pub fn signer() -> SecretKey {
        SecretKey::from_bytes(&[7; 32])
    }

    pub fn sign_with(key: &SecretKey, header: serde_json::Value, claims: serde_json::Value) -> String {
        let enc = |v: serde_json::Value| BASE64URL_NOPAD.encode(&serde_json::to_vec(&v).unwrap());
        let input = format!("{}.{}", enc(header), enc(claims));
        let sig = key.sign(input.as_bytes());
        format!("{input}.{}", BASE64URL_NOPAD.encode(&sig.to_bytes()))
    }

    pub fn claims(sub: &str, name: &str, endpoint: &EndpointId) -> serde_json::Value {
        let now = crate::util::now_ms() / 1000;
        json!({"iss": ISSUER, "aud": AUDIENCE, "sub": sub, "name": name, "email": format!("{name}@example.com"),
               "endpoint": endpoint.to_string(), "iat": now, "exp": now + 90 * 86_400})
    }

    /// A good proof that `endpoint` belongs to account `sub`, called `name`.
    pub fn proof(sub: &str, name: &str, endpoint: &EndpointId) -> String {
        sign_with(&signer(), json!({"alg": "EdDSA", "kid": KID, "typ": TYPE}), claims(sub, name, endpoint))
    }

    #[test]
    fn a_good_proof_verifies() {
        let me = SecretKey::generate().public();
        let p = verify(&proof("user_1", "alex", &me), &me).unwrap();
        assert_eq!((p.sub.as_str(), p.name.as_str(), p.email.as_str()), ("user_1", "alex", "alex@example.com"));
        assert!(verify(&format!("  {}\n", proof("user_1", "alex", &me)), &me).is_ok());
    }

    #[test]
    fn rejects_bad_proofs() {
        let me = SecretKey::generate().public();
        let header = json!({"alg": "EdDSA", "kid": KID, "typ": TYPE});
        let err = |jws: &str| verify(jws, &me).unwrap_err().to_string();
        let with = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut c = claims("user_1", "alex", &me);
            change(&mut c);
            sign_with(&signer(), header.clone(), c)
        };

        let forged = sign_with(&SecretKey::generate(), header.clone(), claims("user_1", "alex", &me));
        assert!(err(&forged).contains("signature doesn't verify"));
        // A good signature over other claims.
        let good = proof("user_1", "alex", &me);
        let other = with(&|c| c["name"] = json!("root"));
        let mixed = format!(
            "{}.{}.{}",
            good.split('.').next().unwrap(),
            other.split('.').nth(1).unwrap(),
            good.split('.').nth(2).unwrap()
        );
        assert!(err(&mixed).contains("signature doesn't verify"));
        assert!(err(&with(&|c| c["aud"] = json!("reason"))).contains("isn't from Reason Network"));
        assert!(err(&with(&|c| c["iss"] = json!("https://evil.example"))).contains("isn't from Reason Network"));
        assert!(err(&with(&|c| c["exp"] = json!(crate::util::now_ms() / 1000 - 1))).contains("expired"));
        assert!(err(&proof("user_1", "alex", &SecretKey::generate().public())).contains("for another machine"));
        for name in ["", "Alex", "alex smith", &"a".repeat(41)] {
            assert!(err(&with(&|c| c["name"] = json!(name))).contains("valid name"), "{name:?}");
        }
        let unknown =
            sign_with(&signer(), json!({"alg": "EdDSA", "kid": "other", "typ": TYPE}), claims("user_1", "alex", &me));
        assert!(err(&unknown).contains("doesn't know"));
        let alg = sign_with(&signer(), json!({"alg": "none", "kid": KID, "typ": TYPE}), claims("user_1", "alex", &me));
        assert!(err(&alg).contains("not a Reason Network sign-in proof"));
        for damaged in ["", "a.b", "a.b.c.d", &good[..good.len() - 4]] {
            assert!(verify(damaged, &me).is_err(), "{damaged:?}");
        }
    }

    #[test]
    fn only_trusted_keys_count() {
        let me = SecretKey::generate().public();
        let jws = proof("user_1", "alex", &me);
        let now = crate::util::now_ms() / 1000;
        assert!(verify_with(&jws, &me, now, &[], true).is_err(), "no keys, nothing verifies");
        let keys = [(KID.to_string(), signer().public())];
        assert!(verify_with(&jws, &me, now, &keys, true).is_ok());
        let later = now + 91 * 86_400;
        assert!(verify_with(&jws, &me, later, &keys, true).is_err(), "expired by then");
        assert!(verify_with(&jws, &me, later, &keys, false).is_ok(), "but still Reason's word, for who holds a name");
    }

    #[test]
    fn reads_a_jwks_key() {
        let x = BASE64URL_NOPAD.encode(signer().public().as_bytes());
        let jwks: Jwks =
            serde_json::from_value(json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": KID, "x": x}]})).unwrap();
        assert_eq!(key(&jwks.keys[0].x), Some(signer().public()));
        assert_eq!(key("not base64!"), None);
    }

    #[test]
    fn an_expired_proof_still_says_whose_machine_it_is() {
        let me = SecretKey::generate().public();
        let mut c = claims("user_1", "alex", &me);
        c["exp"] = json!(crate::util::now_ms() / 1000 - 1);
        let jws = sign_with(&signer(), json!({"alg": "EdDSA", "kid": KID, "typ": TYPE}), c);
        assert!(verify(&jws, &me).is_err());
        assert_eq!(verify_ignoring_expiry(&jws, &me).unwrap().sub, "user_1");
        // Everything else still counts.
        assert!(verify_ignoring_expiry(&jws, &SecretKey::generate().public()).is_err());
        let forged = sign_with(
            &SecretKey::generate(),
            json!({"alg": "EdDSA", "kid": KID, "typ": TYPE}),
            claims("user_1", "alex", &me),
        );
        assert!(verify_ignoring_expiry(&forged, &me).is_err());
    }

    #[test]
    fn refuses_proofs_from_the_future() {
        let me = SecretKey::generate().public();
        for iat in [json!(i64::MAX), json!(crate::util::now_ms() / 1000 + 2 * 86_400)] {
            let mut c = claims("user_1", "alex", &me);
            c["iat"] = iat;
            let jws = sign_with(&signer(), json!({"alg": "EdDSA", "kid": KID, "typ": TYPE}), c);
            assert!(verify(&jws, &me).unwrap_err().to_string().contains("in the future"));
            assert!(verify_ignoring_expiry(&jws, &me).is_err());
        }
        let mut c = claims("user_1", "alex", &me);
        c["iat"] = json!(crate::util::now_ms() / 1000 + 3600);
        assert!(
            verify(&sign_with(&signer(), json!({"alg": "EdDSA", "kid": KID, "typ": TYPE}), c), &me).is_ok(),
            "a clock a bit ahead"
        );
    }

    #[test]
    fn refuses_a_huge_proof_before_decoding_it() {
        let me = SecretKey::generate().public();
        let header = BASE64URL_NOPAD.encode(br#"{"alg":"EdDSA","kid":"test-1","typ":"reason-network-proof"}"#);
        for big in [
            format!("{}.A.{}", "A".repeat(4 << 20), "A".repeat(86)),
            format!("{header}.{}.{}", "A".repeat(4 << 20), "A".repeat(86)),
        ] {
            let t = std::time::Instant::now();
            assert!(verify(&big, &me).unwrap_err().to_string().contains("too long"));
            assert!(t.elapsed() < std::time::Duration::from_millis(50), "{:?}", t.elapsed());
        }
    }
}
