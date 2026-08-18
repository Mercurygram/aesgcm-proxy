// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! FCM WebPush leg.
//!
//! Google Play Services can hand an app a plain WebPush endpoint
//! (`https://fcm.googleapis.com/fcm/send/<token>`) without any Google library on
//! the client, but FCM only accepts pushes to it that carry a VAPID
//! authorization. Telegram does not speak VAPID, so this proxy signs on its
//! behalf: the app registers the `/fcm/<token>` route here as its endpoint, and
//! every push Telegram sends is folded (headers into the body, exactly like the
//! `/aesgcm` route) and forwarded to FCM with a VAPID JWT.
//!
//! The private key lives here only. The app carries the matching public key,
//! which it passes to Play Services at registration time, so FCM binds the
//! subscription to this proxy.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::SecretKey;
use reqwest::header::HeaderValue;
use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

/// FCM rejects a JWT valid for more than 24h. 12h leaves room for clock skew.
const JWT_LIFETIME_SECS: u64 = 12 * 60 * 60;
/// Re-sign this long before expiry so an in-flight push never carries a stale JWT.
const JWT_REFRESH_MARGIN_SECS: u64 = 60 * 60;
/// base64url of `{"typ":"JWT","alg":"ES256"}`, fixed for every JWT this mints.
const JWT_HEADER: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9";
const FCM_AUDIENCE: &str = "https://fcm.googleapis.com";
/// RFC 8292 requires a contact; FCM does not act on it, but it must be present.
const VAPID_SUBJECT: &str = "https://mercurygram.org/";

pub const FCM_SEND_PREFIX: &str = "https://fcm.googleapis.com/fcm/send/";
/// FCM answers 400 ("binary data passed in the request must be less than 4096
/// bytes") above this, before it even looks at the authorization header.
pub const MAX_PAYLOAD: usize = 4096;

pub struct VapidSigner {
    key: SigningKey,
    /// base64url of the uncompressed public point, as passed to Play Services.
    public_key: String,
    /// Ready-to-send `Authorization` value and the JWT expiry it carries.
    /// Expiry 0 forces a mint on first use.
    cached: Mutex<(HeaderValue, u64)>,
}

impl VapidSigner {
    /// `private_key` is the base64url-encoded raw 32-byte P-256 scalar, the
    /// format every WebPush tool emits.
    pub fn from_base64(private_key: &str) -> Result<Self, String> {
        let raw = URL_SAFE_NO_PAD
            .decode(private_key.trim().trim_end_matches('='))
            .map_err(|e| format!("VAPID private key is not base64url: {e}"))?;
        let secret =
            SecretKey::from_slice(&raw).map_err(|e| format!("VAPID private key invalid: {e}"))?;
        Ok(Self::from_secret(secret))
    }

    fn from_secret(secret: SecretKey) -> Self {
        let public_key =
            URL_SAFE_NO_PAD.encode(secret.public_key().to_encoded_point(false).as_bytes());
        Self {
            key: SigningKey::from(&secret),
            public_key,
            cached: Mutex::new((HeaderValue::from_static(""), 0)),
        }
    }

    pub fn generate() -> Self {
        Self::from_secret(SecretKey::random(
            &mut p256::elliptic_curve::rand_core::OsRng,
        ))
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    pub fn private_key_base64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.to_bytes())
    }

    /// `Authorization` header value for a push to FCM, minting a fresh JWT when
    /// the cached one is close to expiry.
    pub fn authorization(&self) -> HeaderValue {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut cached = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        if cached.1 <= now + JWT_REFRESH_MARGIN_SECS {
            let exp = now + JWT_LIFETIME_SECS;
            let header = format!("vapid t={},k={}", self.sign_jwt(exp), self.public_key);
            // Both halves are base64url plus the fixed key names, so this cannot
            // contain a byte a header value rejects.
            *cached = (HeaderValue::from_str(&header).unwrap(), exp);
        }
        cached.0.clone()
    }

    fn sign_jwt(&self, exp: u64) -> String {
        // Fixed-shape JSON with no user input, so it is built literally rather
        // than dragging in a serializer.
        let claims = URL_SAFE_NO_PAD.encode(
            format!(r#"{{"aud":"{FCM_AUDIENCE}","exp":{exp},"sub":"{VAPID_SUBJECT}"}}"#).as_bytes(),
        );
        let signing_input = format!("{JWT_HEADER}.{claims}");
        let signature: Signature = self.key.sign(signing_input.as_bytes());
        format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }
}

/// Resolve an opaque FCM token into its send URL, or `None` when the token
/// cannot be a single path segment under [`FCM_SEND_PREFIX`].
///
/// The token is appended as a path segment rather than joined as a relative
/// reference: an FCM token has the shape `<id>:<rest>`, and a relative join
/// reads an alphanumeric `<id>:` as a URL scheme, turning the whole thing into
/// an absolute URL that no longer points at FCM.
pub fn send_url(token: &str) -> Option<Url> {
    if token.is_empty() || token.contains('/') || token == "." || token == ".." {
        return None;
    }
    let mut url = Url::parse(FCM_SEND_PREFIX).ok()?;
    url.path_segments_mut().ok()?.pop_if_empty().push(token);
    let s = url.as_str();
    (s.len() > FCM_SEND_PREFIX.len() && s.starts_with(FCM_SEND_PREFIX)).then_some(url)
}

/// Lazy `Display` wrapper for logging a push destination. An FCM URL carries a
/// device token, so only enough of it to correlate log lines is printed.
pub struct Redacted<'a>(pub &'a Url);

impl fmt::Display for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_str().strip_prefix(FCM_SEND_PREFIX) {
            Some(token) => {
                let end = token.char_indices().nth(6).map_or(token.len(), |(i, _)| i);
                write!(f, "{FCM_SEND_PREFIX}{}…", &token[..end])
            }
            None => f.write_str(self.0.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;
    use p256::EncodedPoint;

    #[test]
    fn jwt_header_constant_matches_its_json() {
        assert_eq!(
            JWT_HEADER,
            URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#)
        );
    }

    #[test]
    fn jwt_verifies_against_advertised_public_key() {
        let signer = VapidSigner::generate();
        let header = signer.authorization();
        let header = header.to_str().unwrap();

        let (t, k) = header
            .trim_start_matches("vapid ")
            .split_once(',')
            .expect("header has both parts");
        let token = t.strip_prefix("t=").expect("t= part");
        let advertised = k.strip_prefix("k=").expect("k= part");
        assert_eq!(advertised, signer.public_key());

        let (signing_input, signature) = token.rsplit_once('.').unwrap();

        let point = EncodedPoint::from_bytes(URL_SAFE_NO_PAD.decode(advertised).unwrap()).unwrap();
        let verifying = VerifyingKey::from_encoded_point(&point).unwrap();
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
        verifying
            .verify(signing_input.as_bytes(), &signature)
            .expect("signature matches the advertised key");

        let claims = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(signing_input.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(claims.contains(r#""aud":"https://fcm.googleapis.com""#));
    }

    #[test]
    fn round_trips_the_private_key() {
        let signer = VapidSigner::generate();
        let reloaded = VapidSigner::from_base64(&signer.private_key_base64()).unwrap();
        assert_eq!(signer.public_key(), reloaded.public_key());
        assert!(VapidSigner::from_base64("not base64!!").is_err());
    }

    #[test]
    fn send_url_stays_under_the_fcm_prefix() {
        let url = send_url("fMe1_ab:APA91bHq-token.value").expect("plain token");
        assert_eq!(
            url.as_str(),
            "https://fcm.googleapis.com/fcm/send/fMe1_ab:APA91bHq-token.value"
        );
        // Pure alphanumeric before the colon: a relative join would read it as a
        // scheme and leave FCM entirely.
        let url = send_url("e5fNOEHUN4E:APA91bEKcyXKGhix").expect("scheme-shaped token");
        assert_eq!(
            url.as_str(),
            "https://fcm.googleapis.com/fcm/send/e5fNOEHUN4E:APA91bEKcyXKGhix"
        );
        assert!(send_url("").is_none());
        assert!(send_url("..").is_none());
        assert!(send_url("../../etc/passwd").is_none());
        assert!(send_url("//evil.example/x").is_none());
        assert!(send_url("https://evil.example/x").is_none());
    }

    #[test]
    fn redacts_the_token_in_logs() {
        let url = send_url("fMe1_ab:APA91bHq").unwrap();
        assert_eq!(
            Redacted(&url).to_string(),
            "https://fcm.googleapis.com/fcm/send/fMe1_a…"
        );
        let plain = Url::parse("https://up.example/UP?token=1").unwrap();
        assert_eq!(Redacted(&plain).to_string(), plain.as_str());
    }
}
