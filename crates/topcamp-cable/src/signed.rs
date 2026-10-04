//! Signed Turbo stream names (our `signed_stream_name`).
//!
//! Upstream signs with Rails `MessageVerifier` (HMAC-SHA1, Marshal/JSON
//! serializer, `{"_rails":...}` envelope); those exact bytes are scoped
//! out (TRACES_SUPPLEMENT.md §4), so the guard here is
//! authorization-equivalent instead: `data--sig`, where `data` is the
//! URL-safe unpadded Base64 of the stream name and `sig` is the URL-safe
//! unpadded Base64 of `HMAC-SHA256(key, "turbo-stream:" || stream)`.
//! Same access decision (only the server can mint subscribable names),
//! no Rails bytes.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Domain separation: this key signs Turbo stream names only.
const PURPOSE: &[u8] = b"turbo-stream:";

/// Mint a `signed_stream_name` for `stream`.
pub fn sign_stream(key: &[u8], stream: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("any key length works");
    mac.update(PURPOSE);
    mac.update(stream.as_bytes());
    let sig = mac.finalize().into_bytes();
    format!(
        "{}--{}",
        URL_SAFE_NO_PAD.encode(stream.as_bytes()),
        URL_SAFE_NO_PAD.encode(sig)
    )
}

/// Verify a `signed_stream_name`, returning the stream on success.
/// Anything malformed or forged fails closed to `None`.
pub fn verify_stream(key: &[u8], signed: &str) -> Option<String> {
    let (data, sig) = signed.split_once("--")?;
    let stream = URL_SAFE_NO_PAD.decode(data).ok()?;
    let presented = URL_SAFE_NO_PAD.decode(sig).ok()?;
    let mut mac = HmacSha256::new_from_slice(key).expect("any key length works");
    mac.update(PURPOSE);
    mac.update(&stream);
    // Constant-time compare; anything forged fails closed.
    mac.verify_slice(&presented).ok()?;
    String::from_utf8(stream).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-key-0123456789abcdef";

    #[test]
    fn round_trips() {
        let signed = sign_stream(KEY, "abc123:messages");
        assert_eq!(
            verify_stream(KEY, &signed).as_deref(),
            Some("abc123:messages")
        );
    }

    #[test]
    fn wrong_key_fails() {
        let signed = sign_stream(KEY, "abc123:messages");
        assert_eq!(verify_stream(b"other-key", &signed), None);
    }

    #[test]
    fn tampered_stream_fails() {
        let signed = sign_stream(KEY, "abc123:messages");
        let forged_data = URL_SAFE_NO_PAD.encode("user_1_reads");
        let forged = format!("{forged_data}--{}", signed.split_once("--").unwrap().1);
        assert_eq!(verify_stream(KEY, &forged), None);
    }

    #[test]
    fn malformed_fails() {
        assert_eq!(verify_stream(KEY, ""), None);
        assert_eq!(verify_stream(KEY, "no-separator"), None);
        assert_eq!(verify_stream(KEY, "--"), None);
        assert_eq!(verify_stream(KEY, "!!!---!!!"), None);
    }
}
