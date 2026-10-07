//! Web Push message encryption (RFC 8291: `aes128gcm` from RFC 8188 with keys from an
//! ECDH exchange) and VAPID authentication (RFC 8292), built on the `ring` primitives
//! the TLS stack already ships.

use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use ring::{aead, agreement, hkdf};

/// Record size in the header. A notification always fits in a single record.
const RECORD_SIZE: u32 = 4096;
/// How long a VAPID token is good for; push services accept up to 24 hours.
const TOKEN_HOURS: i64 = 12;

/// The server's VAPID identity: an ECDSA P-256 key that signs a token for each push.
pub struct Vapid {
    key: EcdsaKeyPair,
    /// The public key as browsers want it in `applicationServerKey` (base64url, uncompressed point).
    pub public_key: String,
}

impl Vapid {
    /// A new private key, PKCS#8-encoded for storage.
    pub fn generate() -> Result<Vec<u8>> {
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
            .map_err(|_| anyhow!("could not generate a VAPID key"))?;
        Ok(doc.as_ref().to_vec())
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &SystemRandom::new())
            .map_err(|e| anyhow!("unusable VAPID key: {e}"))?;
        let public_key = URL_SAFE_NO_PAD.encode(key.public_key().as_ref());
        Ok(Self { key, public_key })
    }

    /// The `Authorization` header for a push to `endpoint`: a signed ES256 token whose
    /// audience is the push service's origin, plus the public key that verifies it.
    pub fn authorization(&self, endpoint: &str, subject: &str, now: i64) -> Result<String> {
        let aud = url::Url::parse(endpoint).context("invalid push endpoint")?.origin().ascii_serialization();
        let header = URL_SAFE_NO_PAD.encode(r#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({ "aud": aud, "exp": now + TOKEN_HOURS * 3600, "sub": subject });
        let signing_input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let sig = self
            .key
            .sign(&SystemRandom::new(), signing_input.as_bytes())
            .map_err(|_| anyhow!("VAPID signing failed"))?;
        Ok(format!("vapid t={signing_input}.{}, k={}", URL_SAFE_NO_PAD.encode(sig.as_ref()), self.public_key))
    }
}

/// Encrypts `plaintext` for a subscription's `p256dh` key and `auth` secret, returning
/// the request body: the RFC 8188 header followed by a single encrypted record.
pub fn encrypt(p256dh: &[u8], auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    ensure!(p256dh.len() == 65 && p256dh[0] == 4, "p256dh is not an uncompressed P-256 key");
    ensure!(auth.len() == 16, "auth secret must be 16 bytes");
    let rng = SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| anyhow!("could not generate an ECDH key"))?;
    let public = private.compute_public_key().map_err(|_| anyhow!("could not derive the ECDH public key"))?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, p256dh);
    let secret = agreement::agree_ephemeral(private, &peer, <[u8]>::to_vec)
        .map_err(|_| anyhow!("the subscription's p256dh key was rejected"))?;
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).map_err(|_| anyhow!("no randomness for the salt"))?;
    encrypt_with(&secret, public.as_ref(), p256dh, auth, &salt, plaintext)
}

/// Everything after the key exchange, separate so the RFC's test vector can check it.
fn encrypt_with(
    ecdh_secret: &[u8],
    as_public: &[u8],
    ua_public: &[u8],
    auth: &[u8],
    salt: &[u8; 16],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    // RFC 8291 section 3.4: mix the auth secret and both public keys into the key material.
    let key_info = [b"WebPush: info\0".as_slice(), ua_public, as_public];
    let mut ikm = [0u8; 32];
    expand(&hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(ecdh_secret), &key_info, &mut ikm)?;
    // RFC 8188 section 2.2: the content encryption key and nonce.
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let mut cek = [0u8; 16];
    expand(&prk, &[b"Content-Encoding: aes128gcm\0"], &mut cek)?;
    let mut nonce = [0u8; 12];
    expand(&prk, &[b"Content-Encoding: nonce\0"], &mut nonce)?;

    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| anyhow!("bad content encryption key"))?,
    );
    let mut record = Vec::with_capacity(plaintext.len() + 1 + aead::AES_128_GCM.tag_len());
    record.extend_from_slice(plaintext);
    record.push(2); // padding delimiter: this is the last record
    key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record)
        .map_err(|_| anyhow!("encryption failed"))?;

    let mut body = Vec::with_capacity(16 + 4 + 1 + as_public.len() + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[&[u8]], out: &mut [u8]) -> Result<()> {
    prk.expand(info, Len(out.len())).and_then(|okm| okm.fill(out)).map_err(|_| anyhow!("HKDF expansion failed"))
}

/// Decodes base64url with or without padding, as browsers send subscription keys.
pub fn decode(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text.trim().trim_end_matches('=')).context("not base64url")
}

/// What a browser does with a push body: derive the same keys from its own private key
/// and open the record. Tests use it to read what was sent.
#[cfg(test)]
pub fn decrypt(ua_private: agreement::EphemeralPrivateKey, auth: &[u8], body: &[u8]) -> Result<Vec<u8>> {
    let ua_public = ua_private.compute_public_key().map_err(|_| anyhow!("bad key"))?;
    let (salt, rest) = body.split_at(16);
    let key_len = rest[4] as usize;
    let (as_public, record) = rest[5..].split_at(key_len);
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
    let secret = agreement::agree_ephemeral(ua_private, &peer, <[u8]>::to_vec).map_err(|_| anyhow!("bad peer key"))?;
    let key_info = [b"WebPush: info\0".as_slice(), ua_public.as_ref(), as_public];
    let mut ikm = [0u8; 32];
    expand(&hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(&secret), &key_info, &mut ikm)?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    expand(&prk, &[b"Content-Encoding: aes128gcm\0"], &mut cek)?;
    expand(&prk, &[b"Content-Encoding: nonce\0"], &mut nonce)?;
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
    let mut record = record.to_vec();
    let plain = key
        .open_in_place(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record)
        .map_err(|_| anyhow!("record does not open"))?;
    ensure!(plain.last() == Some(&2), "missing padding delimiter");
    Ok(plain[..plain.len() - 1].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> Vec<u8> {
        decode(&s.replace([' ', '\n'], "")).unwrap()
    }

    #[test]
    fn matches_the_rfc_8291_example() {
        // RFC 8291 section 5 and appendix A.
        let body = encrypt_with(
            &b64("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
            &b64("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"),
            &b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"),
            &b64("BTBZMqHH6r4Tts7J_aSIgg"),
            &b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap(),
            b"When I grow up, I want to be a watermelon",
        )
        .unwrap();
        let header = b64("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml
            mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        let ciphertext = b64("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ");
        assert_eq!(header.len(), 86);
        assert_eq!(body, [header, ciphertext].concat());
    }

    #[test]
    fn encrypts_for_a_fresh_subscription_key() {
        let rng = SystemRandom::new();
        let ua = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_public = ua.compute_public_key().unwrap();
        let body = encrypt(ua_public.as_ref(), &[7u8; 16], b"hello").unwrap();
        // salt + record size + key length + key + (plaintext + delimiter + tag)
        assert_eq!(body.len(), 16 + 4 + 1 + 65 + 5 + 1 + 16);
        assert_eq!(&body[16..20], &4096u32.to_be_bytes());
        assert_eq!(body[20], 65);
        assert!(encrypt(&[4u8; 64], &[7u8; 16], b"x").is_err());
        assert!(encrypt(ua_public.as_ref(), &[7u8; 8], b"x").is_err());
    }

    #[test]
    fn signs_a_vapid_token_the_public_key_verifies() {
        let vapid = Vapid::from_pkcs8(&Vapid::generate().unwrap()).unwrap();
        let header =
            vapid.authorization("https://fcm.googleapis.com/fcm/send/abc", "https://example.com", 1000).unwrap();
        let (token, key) = header.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(key, vapid.public_key);
        let (signed, sig) = token.rsplit_once('.').unwrap();
        let public = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, b64(key));
        public.verify(signed.as_bytes(), &b64(sig)).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&b64(signed.split('.').nth(1).unwrap())).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1000 + 12 * 3600);
        assert_eq!(claims["sub"], "https://example.com");
    }
}
