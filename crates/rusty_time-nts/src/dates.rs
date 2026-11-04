//! A certificate's validity window, read from its DER.
//!
//! Only for [`crate::tls::client_config_ignoring_cert_dates`]: to evaluate a
//! chain at a moment every certificate in it was valid, that moment has to be
//! known, and rustls exposes no validity accessor. RFC 5280 §4.1:
//!
//! ```text
//! Certificate     ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
//! TBSCertificate  ::= SEQUENCE { [0] version OPTIONAL, serialNumber INTEGER,
//!                                signature AlgorithmIdentifier, issuer Name,
//!                                validity Validity, ... }
//! Validity        ::= SEQUENCE { notBefore Time, notAfter Time }
//! Time            ::= UTCTime (YYMMDDHHMMSSZ) | GeneralizedTime (YYYYMMDDHHMMSSZ)
//! ```
//!
//! Nothing here decides trust: rustls still verifies the whole chain. A
//! certificate this cannot read simply gets no special treatment.

/// `(notBefore, notAfter)` in Unix seconds.
pub fn validity(der: &[u8]) -> Option<(i64, i64)> {
    let (tag, cert, _) = element(der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = element(cert)?;
    if tag != 0x30 {
        return None;
    }
    let mut rest = tbs;
    // [0] EXPLICIT version, present for v2 and v3 certificates.
    let (tag, _, after) = element(rest)?;
    if tag == 0xA0 {
        rest = after;
    }
    // serialNumber, signature, issuer.
    for expected in [0x02, 0x30, 0x30] {
        let (tag, _, after) = element(rest)?;
        if tag != expected {
            return None;
        }
        rest = after;
    }
    let (tag, validity, _) = element(rest)?;
    if tag != 0x30 {
        return None;
    }
    let (tag_before, before, after) = element(validity)?;
    let (tag_after, not_after, _) = element(after)?;
    Some((time(tag_before, before)?, time(tag_after, not_after)?))
}

/// One DER element: `(tag, contents, what follows)`. Single-byte tags only,
/// which is all the fields read here use.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, input) = input.split_first()?;
    let (&first, mut input) = input.split_first()?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 || input.len() < count {
            return None;
        }
        let mut len = 0usize;
        for &b in &input[..count] {
            len = (len << 8) | usize::from(b);
        }
        input = &input[count..];
        len
    };
    if input.len() < len {
        return None;
    }
    Some((tag, &input[..len], &input[len..]))
}

/// UTCTime (tag 0x17) or GeneralizedTime (0x18), UTC ("Z") as RFC 5280
/// requires, to Unix seconds.
fn time(tag: u8, value: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(value).ok()?;
    let digits = s.strip_suffix('Z')?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (year, rest) = match tag {
        0x17 if digits.len() == 12 => {
            let yy: i64 = digits[..2].parse().ok()?;
            // RFC 5280 §4.1.2.5.1: 50-99 are 19xx, 00-49 are 20xx.
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &digits[2..])
        }
        0x18 if digits.len() == 14 => (digits[..4].parse().ok()?, &digits[4..]),
        _ => return None,
    };
    let field = |i: usize| -> Option<i64> { rest[i..i + 2].parse().ok() };
    let (month, day, hour, minute, second) =
        (field(0)?, field(2)?, field(4)?, field(6)?, field(8)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_arithmetic_matches_known_instants() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(time(0x17, b"700101000000Z"), Some(0));
        // 2000-02-29 (a leap day in a century year) 12:00:00.
        assert_eq!(time(0x17, b"000229120000Z"), Some(951_825_600));
        // The same instant as GeneralizedTime.
        assert_eq!(time(0x18, b"20000229120000Z"), Some(951_825_600));
        // 2038-01-19 03:14:08, one second past i32.
        assert_eq!(time(0x18, b"20380119031408Z"), Some(2_147_483_648));
        // UTCTime pivots at 50.
        assert_eq!(time(0x17, b"500101000000Z"), Some(-631_152_000));
    }

    #[test]
    fn refuses_what_rfc_5280_does_not_allow() {
        assert_eq!(time(0x17, b"700101000000"), None, "no Z");
        assert_eq!(time(0x17, b"7001010000Z"), None, "no seconds");
        assert_eq!(time(0x17, b"701301000000Z"), None, "month 13");
        assert_eq!(
            time(0x18, b"700101000000Z"),
            None,
            "UTCTime length under the GeneralizedTime tag"
        );
        assert_eq!(validity(&[0x30, 0x05, 0x01]), None, "truncated");
    }
}
