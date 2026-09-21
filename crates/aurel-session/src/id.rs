//! Stable session IDs for persistent AUREL sessions.
//!
//! An ID looks like `20260918-120301-a1b2c3d4`: UTC date, UTC time, and 8
//! hex digits of process-unique randomness. Sortable, typable, and safe to
//! embed in a filename — parsing rejects everything else (slashes, `..`,
//! NUL bytes, wrong lengths), so an ID can never escape the store
//! directory. Resume additionally accepts an unambiguous prefix.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::SessionError;

/// A validated persistent-session identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Generate a fresh ID: UTC `YYYYMMDD-HHMMSS` plus 8 hex digits mixed
    /// from the clock, PID, and a process counter. Uniqueness within a
    /// process is structural (counter); across processes it is practical
    /// (time + PID + randomness) and enforced by [`crate::SessionStore`],
    /// which regenerates on collision.
    pub fn generate() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let secs = (nanos / 1_000_000_000) as u64;
        let (year, month, day) = civil_date(secs / 86_400);
        let time_of_day = secs % 86_400;
        // xorshift64*: no RNG dependency for a local filename nonce.
        let mut state = nanos as u64
            ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ count.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        // High 32 bits: the multiply's upper half mixes best, and 8 hex
        // digits keep the ID at its fixed 24 characters.
        let rand = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32;
        let (hour, minute, second) = (
            (time_of_day / 3600) % 24,
            (time_of_day / 60) % 60,
            time_of_day % 60,
        );
        SessionId(format!(
            "{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{rand:08x}"
        ))
    }

    /// Validate `text` as a session ID. Anything malformed — including path
    /// separators, parent references, and NUL bytes — is rejected so IDs
    /// stay usable as bare filenames.
    pub fn parse(text: &str) -> Result<Self, SessionError> {
        if text.len() != 24 {
            return Err(SessionError::InvalidId(text.to_string()));
        }
        let bytes = text.as_bytes();
        if bytes[8] != b'-' || bytes[15] != b'-' {
            return Err(SessionError::InvalidId(text.to_string()));
        }
        let (date, clock, rand) = (&text[..8], &text[9..15], &text[16..24]);
        if !date.bytes().all(|b| b.is_ascii_digit())
            || !clock.bytes().all(|b| b.is_ascii_digit())
            || !rand.bytes().all(|b| b.is_ascii_hexdigit())
            || rand.bytes().any(|b| b.is_ascii_uppercase())
        {
            return Err(SessionError::InvalidId(text.to_string()));
        }
        let number = |part: &str| part.parse::<u32>().unwrap_or(99);
        let (month, day) = (number(&date[4..6]), number(&date[6..8]));
        let (hour, minute, second) = (
            number(&clock[0..2]),
            number(&clock[2..4]),
            number(&clock[4..6]),
        );
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return Err(SessionError::InvalidId(text.to_string()));
        }
        Ok(SessionId(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's civil
/// algorithm: exact integer math, no timezone database (IDs are UTC).
fn civil_date(days: u64) -> (u64, u64, u64) {
    let shifted = days as i64 + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u64;
    let month = (if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    }) as u64;
    (
        (if month <= 2 { year + 1 } else { year }) as u64,
        month,
        day,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_ids_parse_and_differ() {
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let id = SessionId::generate();
            assert_eq!(id.as_str().len(), 24, "got: {id}");
            SessionId::parse(id.as_str()).expect("generated IDs must parse");
            assert!(seen.insert(id.as_str().to_string()), "duplicate ID: {id}");
        }
    }

    #[test]
    fn malformed_ids_reject_safely() {
        for bad in [
            "",
            "20260918-120301-a1b2c3d",    // short
            "20260918-120301-a1b2c3d45",  // long
            "20260918120301a1b2c3d4",     // missing dashes
            "../etc/passwd.............", // traversal attempt
            "../../20260918-120301-x",    // traversal-shaped
            "20260918-120301-A1B2C3D4",   // uppercase hex
            "20260918-120301-zzzzzzzz",   // non-hex
            "20261301-120301-a1b2c3d4",   // month 13
            "20260932-120301-a1b2c3d4",   // day 32
            "20260918-253001-a1b2c3d4",   // hour 25
            "20260918-120361-a1b2c3d4",   // minute 61
            "20260918-120301-a1b2c3d\0",  // NUL byte
            "20260918/120301/a1b2c3d4",   // separators
        ] {
            assert!(
                matches!(SessionId::parse(bad), Err(SessionError::InvalidId(_))),
                "must reject {bad:?}"
            );
            assert!(
                !bad.contains('/') && !bad.contains('\0') || SessionId::parse(bad).is_err(),
                "path-hostile input must never parse: {bad:?}"
            );
        }
    }

    #[test]
    fn civil_date_matches_known_days() {
        // 1970-01-01 is day 0; 2026-09-18 is day 20714.
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(20714), (2026, 9, 18));
        assert_eq!(civil_date(365), (1971, 1, 1));
    }
}
