//! Rails `signed_id` for session transfer (`User#transfer_id` and
//! `users_from_transfer_id`), mirroring upstream
//! `crates/rails_compat/src/{signed_id,message_verifier,metadata,
//! key_generator}.rs`.
//!
//! Tokens are the modern Rails shape only: JSON envelope, SHA256
//! HMAC, URL-safe Base64. The legacy SHA1/strict and Base64/Marshal
//! reads are deliberately not implemented — this app never mints
//! them, and upstream accepts everything it mints here. Key
//! derivation is uncached PBKDF2 (1000 rounds): transfers are rare
//! and sub-millisecond either way.

use base64::Engine as _;

const SIGNED_ID_SALT: &[u8] = b"active_record/signed_id";
const SIGNED_ID_ITERATIONS: u32 = 1000;
const TRANSFER_PURPOSE: &str = "user/transfer";
const TRANSFER_TTL_MILLIS: u64 = 4 * 60 * 60 * 1000;

/// `Rails.application.key_generator.generate_key(salt, 64)`.
fn derive_signing_key(secret_key_base: &[u8]) -> [u8; 64] {
    let mut key = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
        secret_key_base,
        SIGNED_ID_SALT,
        SIGNED_ID_ITERATIONS,
        &mut key,
    );
    key
}

/// Days since 1970-01-01 from a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_prime = (month as i64 + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

/// Civil date from days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719468;
    let era = days.div_euclid(146097);
    let day_of_era = days.rem_euclid(146097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        (month_prime + 3) as u32
    } else {
        (month_prime - 9) as u32
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `Time#iso8601(3)`: `2033-05-18T12:00:00.000Z`.
fn iso8601_millis(unix_millis: u64) -> String {
    let secs = unix_millis.div_euclid(1000) as i64;
    let millis = unix_millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

/// Parse exactly what `iso8601_millis` writes (upstream mints the
/// same shape; anything else is rejected).
fn parse_iso8601_millis(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return None;
    }
    let number = |from: usize, to: usize| text[from..to].parse::<u64>().ok();
    let (year, month, day) = (
        number(0, 4)? as i64,
        number(5, 7)? as u32,
        number(8, 10)? as u32,
    );
    let (hour, minute, second, millis) = (
        number(11, 13)?,
        number(14, 16)?,
        number(17, 19)?,
        number(20, 23)?,
    );
    if !(1..=12).contains(&month)
        || day == 0
        || day > 31
        || hour > 23
        || minute > 59
        || second > 60
        || millis > 999
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add((hour * 3600 + minute * 60 + second) as i64)?;
    let total = u64::try_from(secs)
        .ok()?
        .checked_mul(1000)?
        .checked_add(millis)?;
    // Round-trip through the civil date: rejects e.g. February 30th.
    (iso8601_millis(total) == text).then_some(total)
}

/// `Base64.urlsafe_decode64`: both alphabets, padding implied.
/// Exactly one trailing character can never decode.
fn urlsafe_decode(input: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, URL_SAFE};
    let normalized: String = input
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    let padded = match normalized.len() % 4 {
        0 => normalized,
        1 => return None,
        _ => format!("{normalized}{}", "=".repeat(4 - normalized.len() % 4)),
    };
    STANDARD
        .decode(padded.as_bytes())
        .or_else(|_| URL_SAFE.decode(padded.as_bytes()))
        .ok()
}

fn urlsafe_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0, |diff, (a, b)| diff | (a ^ b))
        == 0
}

/// `user.transfer_id`: the id signed for the transfer purpose, good for 4 hours.
pub fn transfer_id(secret_key_base: &[u8], user_id: i64, now_unix_millis: u64) -> String {
    transfer_id_at(
        secret_key_base,
        user_id,
        now_unix_millis.saturating_add(TRANSFER_TTL_MILLIS),
    )
}

fn transfer_id_at(secret_key_base: &[u8], user_id: i64, expires_unix_millis: u64) -> String {
    let envelope = format!(
        "{{\"_rails\":{{\"data\":{user_id},\"exp\":\"{}\",\"pur\":\"{TRANSFER_PURPOSE}\"}}}}",
        iso8601_millis(expires_unix_millis)
    );
    sign(secret_key_base, envelope.as_bytes())
}

fn sign(secret_key_base: &[u8], serialized: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let encoded = urlsafe_encode(serialized);
    let key = derive_signing_key(secret_key_base);
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key).expect("HMAC takes any key size");
    mac.update(encoded.as_bytes());
    format!("{encoded}--{}", hex::encode(mac.finalize().into_bytes()))
}

/// `users_from_transfer_id`: the id back, when the signature, purpose
/// and expiry all check out.
pub fn user_id_from_transfer_id(
    secret_key_base: &[u8],
    token: &str,
    now_unix_millis: u64,
) -> Option<i64> {
    use hmac::{Hmac, Mac};
    // The digest is the last 64 characters when `--` precedes them.
    let index = token.len().checked_sub(66)?;
    if !token.is_char_boundary(index) || &token[index..index + 2] != "--" {
        return None;
    }
    let (encoded, digest) = (&token[..index], &token[index + 2..]);
    if encoded.trim().is_empty() || digest.trim().is_empty() {
        return None;
    }
    let key = derive_signing_key(secret_key_base);
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key).expect("HMAC takes any key size");
    mac.update(encoded.as_bytes());
    if !constant_time_eq(digest, &hex::encode(mac.finalize().into_bytes())) {
        return None;
    }
    let decoded = urlsafe_decode(encoded)?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let rails = value.get("_rails")?.as_object()?;
    match rails.get("exp") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::String(exp)) => {
            if now_unix_millis >= parse_iso8601_millis(exp)? {
                return None;
            }
        }
        Some(_) => return None,
    }
    if ruby_to_s(rails.get("pur")) != TRANSFER_PURPOSE {
        return None;
    }
    match rails.get("data")? {
        serde_json::Value::Number(number) => number.as_i64(),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// `Object#to_s` for the JSON values a purpose could hold.
fn ruby_to_s(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        // Arrays and hashes to_s as their #inspect, which no purpose
        // we compare against looks like.
        Some(other) => format!("\u{0}{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret-key-base-for-transfer";
    // 2046-01-01T00:00:00Z in millis (independent of this module: from `date -u`).
    const NOW: u64 = 2398377600000;

    #[test]
    fn key_derivation_matches_ruby() {
        assert_eq!(
            hex::encode(derive_signing_key(SECRET)),
            "dce2149806f010a396f1a424c0f04a138058cb9b7d26c62bcdb60afcf84e063cef7791492fa9f55f1a912af059ea12caff82b5b6a597c390fdc0943e1b60b709"
        );
    }

    #[test]
    fn minting_matches_ruby_byte_for_byte() {
        // Computed with Python's stdlib (hashlib + hmac), not this code.
        assert_eq!(
            transfer_id_at(SECRET, 7, 2398420800000),
            "eyJfcmFpbHMiOnsiZGF0YSI6NywiZXhwIjoiMjA0Ni0wMS0wMVQxMjowMDowMC4wMDBaIiwicHVyIjoidXNlci90cmFuc2ZlciJ9fQ--6c11232c0f9bf8ea0b24fb6a4ef78ef9d1aa1712d3ed58f1393485b351452728"
        );
    }

    #[test]
    fn round_trip_holds_until_expiry() {
        let token = transfer_id(SECRET, 7, NOW);
        assert_eq!(user_id_from_transfer_id(SECRET, &token, NOW), Some(7));
        assert_eq!(
            user_id_from_transfer_id(SECRET, &token, NOW + TRANSFER_TTL_MILLIS - 1),
            Some(7)
        );
        assert_eq!(
            user_id_from_transfer_id(SECRET, &token, NOW + TRANSFER_TTL_MILLIS),
            None
        );
    }

    #[test]
    fn rejects_tampering_and_wrong_secrets() {
        let token = transfer_id(SECRET, 7, NOW);
        let mut tampered = token.clone();
        tampered.replace_range(10..11, "A");
        assert_eq!(user_id_from_transfer_id(SECRET, &tampered, NOW), None);
        assert_eq!(
            user_id_from_transfer_id(b"another-secret", &token, NOW),
            None
        );
        assert_eq!(user_id_from_transfer_id(SECRET, "junk", NOW), None);
        assert_eq!(user_id_from_transfer_id(SECRET, "", NOW), None);
    }

    #[test]
    fn rejects_legacy_and_foreign_shapes() {
        // No digest, short digest, and non-JSON payloads.
        assert_eq!(user_id_from_transfer_id(SECRET, "e30--", NOW), None);
        let short = format!("{}--{}", urlsafe_encode(b"{}"), "ab".repeat(16));
        assert_eq!(user_id_from_transfer_id(SECRET, &short, NOW), None);
        // Valid signature over a non-envelope payload.
        let encoded = urlsafe_encode(b"{\"data\":7}");
        assert_eq!(
            user_id_from_transfer_id(SECRET, &sign(SECRET, b"{\"data\":7}"), NOW),
            None
        );
        assert!(!encoded.is_empty());
    }

    #[test]
    fn rejects_wrong_purposes_and_data() {
        let envelope = |body: &str| sign(SECRET, body.as_bytes());
        let wrap = |data: &str, exp: &str, pur: &str| {
            format!("{{\"_rails\":{{\"data\":{data},\"exp\":{exp},\"pur\":{pur}}}}}")
        };
        let exp = "\"2046-01-01T12:00:00.000Z\"";
        assert_eq!(
            user_id_from_transfer_id(SECRET, &envelope(&wrap("7", exp, "\"user/login\"")), NOW),
            None
        );
        assert_eq!(
            user_id_from_transfer_id(SECRET, &envelope(&wrap("7", exp, "null")), NOW),
            None
        );
        assert_eq!(
            user_id_from_transfer_id(
                SECRET,
                &envelope(&wrap("\" 7 \"", exp, "\"user/transfer\"")),
                NOW
            ),
            Some(7)
        );
        assert_eq!(
            user_id_from_transfer_id(
                SECRET,
                &envelope(&wrap("[7]", exp, "\"user/transfer\"")),
                NOW
            ),
            None
        );
        assert_eq!(
            user_id_from_transfer_id(
                SECRET,
                &envelope(&wrap(
                    "7",
                    "\"2000-01-01T00:00:00.000Z\"",
                    "\"user/transfer\""
                )),
                NOW
            ),
            None
        );
        assert_eq!(
            user_id_from_transfer_id(SECRET, &envelope(&wrap("7", "7", "\"user/transfer\"")), NOW),
            None
        );
    }

    #[test]
    fn iso8601_millis_round_trips() {
        assert_eq!(iso8601_millis(2398420800000), "2046-01-01T12:00:00.000Z");
        assert_eq!(iso8601_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_millis(1_749_827_223_456),
            "2025-06-13T15:07:03.456Z"
        );
        for millis in [0, 1, 999, 1000, 2398420800000, 4102444799999] {
            assert_eq!(parse_iso8601_millis(&iso8601_millis(millis)), Some(millis));
        }
        assert_eq!(parse_iso8601_millis("2025-02-30T00:00:00.000Z"), None);
        assert_eq!(parse_iso8601_millis("2025-06-15T15:20:23Z"), None);
        assert_eq!(parse_iso8601_millis("junk"), None);
    }

    #[test]
    fn urlsafe_decode_matches_ruby() {
        for (input, expected) in [
            ("aHR0cDovL2NhbXBmaXJlLnRlc3Q", "http://campfire.test"),
            ("aHR0cDovL2NhbXBmaXJlLnRlc3Q=", "http://campfire.test"),
            ("", ""),
        ] {
            assert_eq!(
                urlsafe_decode(input).as_deref(),
                Some(expected.as_bytes()),
                "{input}"
            );
        }
        for input in ["a", "ab=", "ab=c", "aB==", "a*bc"] {
            assert_eq!(urlsafe_decode(input), None, "{input}");
        }
        // Either alphabet decodes (a byte 251 payload needs -_ or +/).
        assert_eq!(urlsafe_decode("-w"), Some(vec![251]));
        assert_eq!(urlsafe_decode("+w"), Some(vec![251]));
    }
}
