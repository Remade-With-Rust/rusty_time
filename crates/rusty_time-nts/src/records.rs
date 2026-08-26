//! NTS-KE record codec (RFC 8915 §4): 16-bit critical+type, 16-bit body length.
//!
//! Parsing never panics on malformed input; a malformed tail is reported once and
//! iteration ends (the fuzz target in `fuzz/` holds this line).

use core::fmt;

/// Well-known NTS-KE record types (RFC 8915 §4.1).
pub mod record_type {
    pub const END_OF_MESSAGE: u16 = 0;
    pub const NEXT_PROTOCOL: u16 = 1;
    pub const ERROR: u16 = 2;
    pub const WARNING: u16 = 3;
    pub const AEAD_ALGORITHM: u16 = 4;
    pub const NEW_COOKIE: u16 = 5;
    pub const SERVER_NEGOTIATION: u16 = 6;
    pub const PORT_NEGOTIATION: u16 = 7;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record<'a> {
    pub critical: bool,
    pub record_type: u16,
    pub body: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// Stream ended inside a record header or body.
    Truncated { at: usize },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Truncated { at } => write!(f, "NTS-KE stream truncated at byte {at}"),
        }
    }
}

impl std::error::Error for RecordError {}

/// Iterate records in a buffer. Ends after END_OF_MESSAGE, at end of input, or at
/// the first malformed record (yielded as `Err`).
pub fn records(buf: &[u8]) -> RecordIter<'_> {
    RecordIter {
        buf,
        at: 0,
        done: false,
    }
}

pub struct RecordIter<'a> {
    buf: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = Result<Record<'a>, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.at >= self.buf.len() {
            return None;
        }
        let header: Option<(&[u8], &[u8])> = self
            .buf
            .get(self.at..self.at + 2)
            .zip(self.buf.get(self.at + 2..self.at + 4));
        let Some((tb, lb)) = header else {
            self.done = true;
            return Some(Err(RecordError::Truncated { at: self.at }));
        };
        let type_field = u16::from_be_bytes([tb[0], tb[1]]);
        let body_len = u16::from_be_bytes([lb[0], lb[1]]) as usize;
        let body_start = self.at + 4;
        let Some(body) = self.buf.get(body_start..body_start + body_len) else {
            self.done = true;
            return Some(Err(RecordError::Truncated { at: self.at }));
        };
        self.at = body_start + body_len;
        let record_type = type_field & 0x7FFF;
        if record_type == record_type::END_OF_MESSAGE {
            self.done = true;
        }
        Some(Ok(Record {
            critical: type_field & 0x8000 != 0,
            record_type,
            body,
        }))
    }
}

/// Append one record.
pub fn write_record(out: &mut Vec<u8>, critical: bool, record_type: u16, body: &[u8]) {
    let type_field = (record_type & 0x7FFF) | if critical { 0x8000 } else { 0 };
    out.extend_from_slice(&type_field.to_be_bytes());
    out.extend_from_slice(&(body.len().min(u16::MAX as usize) as u16).to_be_bytes());
    out.extend_from_slice(&body[..body.len().min(u16::MAX as usize)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_client_hello() {
        // The canonical NTS-KE client request: next-proto NTPv4, AEAD SIV-CMAC-256, EOM.
        let mut buf = Vec::new();
        write_record(
            &mut buf,
            true,
            record_type::NEXT_PROTOCOL,
            &0u16.to_be_bytes(),
        );
        write_record(
            &mut buf,
            true,
            record_type::AEAD_ALGORITHM,
            &15u16.to_be_bytes(),
        );
        write_record(&mut buf, true, record_type::END_OF_MESSAGE, &[]);

        let parsed: Vec<Record<'_>> = records(&buf).collect::<Result<_, _>>().expect("parse");
        assert_eq!(parsed.len(), 3);
        assert!(parsed.iter().all(|r| r.critical));
        assert_eq!(parsed[0].record_type, record_type::NEXT_PROTOCOL);
        assert_eq!(parsed[1].body, &15u16.to_be_bytes());
        assert_eq!(parsed[2].record_type, record_type::END_OF_MESSAGE);
    }

    #[test]
    fn iteration_stops_after_eom() {
        let mut buf = Vec::new();
        write_record(&mut buf, true, record_type::END_OF_MESSAGE, &[]);
        write_record(&mut buf, false, record_type::NEW_COOKIE, &[1, 2, 3]);
        let parsed: Vec<_> = records(&buf).collect();
        assert_eq!(parsed.len(), 1, "records after EOM must not be yielded");
    }

    #[test]
    fn truncated_body_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        write_record(&mut buf, false, record_type::NEW_COOKIE, &[0xAA; 100]);
        buf.truncate(20);
        let parsed: Vec<_> = records(&buf).collect();
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].is_err());
    }
}
