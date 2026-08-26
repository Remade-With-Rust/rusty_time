//! Reference-clock samples: what a GPS, a PPS edge or a PTP hardware clock
//! hands us, and what must be true before any of it is believed.
//!
//! Portable on purpose. The transports differ wildly per platform — SysV shared
//! memory, a Unix datagram, an ioctl on a character device — but what arrives
//! is always the same shape: "at *my* time T, the reference said R". Deciding
//! whether that pairing is usable is arithmetic, and it belongs here where it
//! can be tested without a GPS on the desk.

use core::fmt;

/// One reading from a reference clock.
///
/// **The offset is the stored quantity, not the reference time.** Storing a
/// reference timestamp and recovering the offset by subtraction loses about
/// 100 ns: an f64 holding a Unix-epoch value (~1.76e9) has only ~1e-7 s of
/// resolution left below the decimal point, so `local + offset - local` does
/// not return `offset`. That silently caps every refclock at roughly 100 ns
/// however precise the hardware is — fatal for a PPS source, and it was found
/// by a round-trip test asserting exactness rather than "close enough".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RefclockSample {
    /// Our own clock when the reading was taken, Unix seconds.
    pub local_s: f64,
    /// Seconds to ADD to our clock to match the reference. Authoritative.
    pub offset_s: f64,
    /// The reference's own claim about its precision, log2 seconds.
    pub precision_log2: i8,
    pub leap: LeapWarning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeapWarning {
    None,
    AddSecond,
    DeleteSecond,
    /// The reference is not currently trustworthy.
    NotSynchronized,
}

impl LeapWarning {
    /// Decode the leap field used by both the SHM and SOCK protocols.
    pub fn from_wire(value: i32) -> LeapWarning {
        match value {
            0 => LeapWarning::None,
            1 => LeapWarning::AddSecond,
            2 => LeapWarning::DeleteSecond,
            _ => LeapWarning::NotSynchronized,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefclockError {
    /// The reference says it is not synchronized.
    NotSynchronized,
    /// The offset is larger than any sane reference could imply.
    OffsetTooLarge,
    /// A timestamp was zero, negative or otherwise not a time.
    Implausible,
    /// This reading is not newer than the one before it.
    Stale,
}

impl fmt::Display for RefclockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RefclockError::NotSynchronized => "reference clock reports it is unsynchronized",
            RefclockError::OffsetTooLarge => "reference offset is implausibly large",
            RefclockError::Implausible => "reference timestamps are not plausible times",
            RefclockError::Stale => "reference sample is not newer than the last one",
        };
        f.write_str(s)
    }
}

/// The widest offset a reference clock may imply before we refuse it.
///
/// A GPS receiver that has lost lock, or a shared-memory segment left by a dead
/// process, can present a wildly stale reading that is otherwise well formed.
/// Sixteen seconds is far beyond any real refclock's error and far short of the
/// jumps a stale sample produces.
pub const MAX_REFCLOCK_OFFSET_S: f64 = 16.0;

/// Earliest time we will accept as a real reading (2020-01-01). A zeroed or
/// partially-written segment reads as an epoch-ish timestamp, and this is what
/// catches it.
const MIN_PLAUSIBLE_UNIX_S: f64 = 1_577_836_800.0;

impl RefclockSample {
    /// Seconds to ADD to our clock to match the reference.
    pub fn offset_s(&self) -> f64 {
        self.offset_s
    }

    /// What the reference says our `local_s` instant actually was.
    ///
    /// Derived, and therefore subject to the same epoch-magnitude rounding the
    /// struct exists to avoid — use it for display, never to recompute the
    /// offset.
    pub fn reference_s(&self) -> f64 {
        self.local_s + self.offset_s
    }

    /// Is this reading usable? `last_local_s` is the previous accepted
    /// reading's local timestamp, if any.
    pub fn validate(&self, last_local_s: Option<f64>) -> Result<(), RefclockError> {
        if self.leap == LeapWarning::NotSynchronized {
            return Err(RefclockError::NotSynchronized);
        }
        if !self.local_s.is_finite()
            || !self.offset_s.is_finite()
            || self.local_s < MIN_PLAUSIBLE_UNIX_S
            || self.reference_s() < MIN_PLAUSIBLE_UNIX_S
        {
            return Err(RefclockError::Implausible);
        }
        if let Some(previous) = last_local_s
            && self.local_s <= previous
        {
            // Re-reading the same segment must not be mistaken for a new
            // sample: a dead producer would otherwise keep "confirming" a
            // frozen time forever.
            return Err(RefclockError::Stale);
        }
        if self.offset_s().abs() > MAX_REFCLOCK_OFFSET_S {
            return Err(RefclockError::OffsetTooLarge);
        }
        Ok(())
    }

    /// The dispersion this sample implies, from the reference's own precision
    /// claim.
    pub fn dispersion_s(&self) -> f64 {
        2f64.powi(self.precision_log2 as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(local: f64, reference: f64) -> RefclockSample {
        RefclockSample {
            local_s: local,
            offset_s: reference - local,
            precision_log2: -20,
            leap: LeapWarning::None,
        }
    }

    /// Build directly from an offset, which is how the transports do it.
    fn with_offset(local: f64, offset: f64) -> RefclockSample {
        RefclockSample {
            local_s: local,
            offset_s: offset,
            precision_log2: -20,
            leap: LeapWarning::None,
        }
    }

    #[test]
    fn offset_is_reference_minus_local() {
        let s = sample(1_756_224_000.0, 1_756_224_000.5);
        assert!((s.offset_s() - 0.5).abs() < 1e-12);
        assert!(s.validate(None).is_ok());
    }

    #[test]
    fn an_unsynchronized_reference_is_refused() {
        let mut s = sample(1_756_224_000.0, 1_756_224_000.0);
        s.leap = LeapWarning::NotSynchronized;
        assert_eq!(s.validate(None), Err(RefclockError::NotSynchronized));
    }

    #[test]
    fn a_stale_segment_cannot_keep_confirming_a_frozen_time() {
        // The failure this prevents: a producer dies, its shared memory keeps
        // the last value, and every read looks like a fresh confirmation.
        let s = sample(1_756_224_000.0, 1_756_224_000.0);
        assert!(s.validate(Some(1_756_223_999.0)).is_ok());
        assert_eq!(
            s.validate(Some(1_756_224_000.0)),
            Err(RefclockError::Stale),
            "the same timestamp twice is not two samples"
        );
        assert_eq!(s.validate(Some(1_756_224_001.0)), Err(RefclockError::Stale));
    }

    #[test]
    fn a_zeroed_segment_reads_as_implausible_not_as_1970() {
        // An uninitialised or half-written segment is all zeroes, which is a
        // valid-looking struct describing the epoch.
        let s = sample(0.0, 0.0);
        assert_eq!(s.validate(None), Err(RefclockError::Implausible));
        // Half-written: local is current but the reference is not.
        let half = sample(1_756_224_000.0, 0.0);
        assert_eq!(half.validate(None), Err(RefclockError::Implausible));
    }

    #[test]
    fn a_wildly_stale_reading_is_refused_however_well_formed() {
        // A GPS that lost lock an hour ago still writes a tidy struct.
        let s = sample(1_756_224_000.0, 1_756_224_000.0 - 3600.0);
        assert_eq!(s.validate(None), Err(RefclockError::OffsetTooLarge));
        // Just inside the bound is fine.
        let ok = sample(1_756_224_000.0, 1_756_224_000.0 + 15.0);
        assert!(ok.validate(None).is_ok());
    }

    #[test]
    fn an_offset_survives_a_round_trip_exactly() {
        // The defect this guards: storing a reference timestamp and
        // subtracting to recover the offset loses ~100 ns at epoch magnitude,
        // which would cap a nanosecond-class reference at 100 ns.
        for offset in [1e-9, -1e-9, 1.5e-3, -1.5e-3, 123e-6] {
            let s = with_offset(1_756_224_000.0, offset);
            assert_eq!(
                s.offset_s(),
                offset,
                "offset {offset} did not survive storage exactly"
            );
        }
    }

    #[test]
    fn nan_and_infinity_are_not_times() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                sample(bad, 1_756_224_000.0).validate(None),
                Err(RefclockError::Implausible)
            );
            assert_eq!(
                with_offset(1_756_224_000.0, bad).validate(None),
                Err(RefclockError::Implausible)
            );
        }
    }

    #[test]
    fn leap_decoding_matches_the_wire_values() {
        assert_eq!(LeapWarning::from_wire(0), LeapWarning::None);
        assert_eq!(LeapWarning::from_wire(1), LeapWarning::AddSecond);
        assert_eq!(LeapWarning::from_wire(2), LeapWarning::DeleteSecond);
        // Anything else, including the protocol's own "3", means do not trust.
        for unknown in [3, 4, -1, 99] {
            assert_eq!(
                LeapWarning::from_wire(unknown),
                LeapWarning::NotSynchronized
            );
        }
    }

    #[test]
    fn dispersion_follows_the_precision_claim() {
        let mut s = sample(1_756_224_000.0, 1_756_224_000.0);
        s.precision_log2 = -20; // ~1 us
        assert!((s.dispersion_s() - 9.5367e-7).abs() < 1e-9);
        s.precision_log2 = 0; // 1 s: a very coarse reference
        assert!((s.dispersion_s() - 1.0).abs() < 1e-12);
    }
}
