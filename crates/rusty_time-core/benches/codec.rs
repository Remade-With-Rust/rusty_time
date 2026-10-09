//! The NTP packet codec, as a deterministic instruction-count workload.
//!
//! The third instrument. `hot_path` and `client_path` both drive the codec
//! incidentally, and in `hot_path` it reads as 3 Ir/request (1.27%) — which the
//! veins plan recorded as "already at its floor, not a vein". That reading was
//! taken on an instrument where `parse` and `write` are **inlined into
//! `hot_path::main`** and 52% of events are unattributed, so it cannot support
//! a floor claim about the codec's own cost.
//!
//! It also measures the wrong deployment. `ntp.rs` is the `no_std` leaf: on a
//! Cortex-M4F, an RV32 part or in the wasm client it is not 1.27% of the work,
//! it is ~all of it. This harness prices it on its own.
//!
//! `instruction-counting` §4 wants at least two corpus shapes because roughly a
//! third of probes move two instruments in opposite directions. This is the
//! third, and it is deliberately a *different shape* from the other two: no
//! client table, no filter, no clock — just bytes in, bytes out.
//!
//! Deterministic by construction: a fixed packet population built from a
//! splitmix64 LCG over a fixed seed, walked a fixed number of times. Two runs
//! of the same binary produce the same Ir count to the instruction.
//!
//! The gate is a CHECKSUM over every parsed field and every emitted byte. This
//! is an integer/exact path, so the bar is **byte-identity**: an
//! instruction-count change here must not move the arithmetic at all.
//!
//! Run:
//!   cargo build --release --bench codec
//!   valgrind --tool=callgrind --callgrind-out-file=cg.out ./codec
//!   callgrind_annotate cg.out

use rusty_time_core::ntp::{
    self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpShort, NtpTimestamp, Trailer,
};

/// Deterministic filler — splitmix64. Identical on every run and every host.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// How many distinct packets the corpus holds, and how many passes over it.
/// The product is the work count; both are printed so a moved anchor is visible.
const PACKETS: usize = 256;
const PASSES: usize = 2000;

fn main() {
    // ---------------------------------------------------------------- corpus
    //
    // Three shapes, because the codec's three entry points take different
    // routes and a corpus of only bare headers would never enter the
    // extension-field walk at all (`codec-measurement`: count path entries
    // before judging a path).
    //
    //   - 3/4 bare 48-byte headers (the overwhelming majority of real traffic)
    //   - 1/8 header + one well-formed extension field
    //   - 1/8 header + trailing bytes that cannot be a field (a legacy MAC)
    //
    // Version is forced into 3..=4 so `parse` reaches the body rather than
    // rejecting; a corpus that fails validation measures the error path.
    let mut seed = 0x0123_4567_89AB_CDEFu64;
    let mut corpus: Vec<Vec<u8>> = Vec::with_capacity(PACKETS);
    for i in 0..PACKETS {
        let mut p = vec![0u8; HEADER_LEN];
        for chunk in p.chunks_mut(8) {
            let w = splitmix(&mut seed).to_be_bytes();
            chunk.copy_from_slice(&w[..chunk.len()]);
        }
        // Force a valid version (3 or 4) into bits 3..6 of byte 0.
        let v = if i % 2 == 0 { 4u8 } else { 3u8 };
        p[0] = (p[0] & !0b0011_1000) | (v << 3);
        match i % 8 {
            // A well-formed RFC 7822 field: type, total length 20, 16 value bytes.
            1 => {
                p.extend_from_slice(&[0x01, 0x04, 0x00, 0x14]);
                for _ in 0..16 {
                    p.push(splitmix(&mut seed) as u8);
                }
            }
            // Trailing bytes too short to be a field — yielded as Opaque.
            5 => {
                for _ in 0..4 {
                    p.push(splitmix(&mut seed) as u8);
                }
            }
            _ => {}
        }
        corpus.push(p);
    }

    // ------------------------------------------------------------- the work
    let mut checksum: u64 = 0xcbf2_9ce4_8422_2325;
    let mut parsed: u64 = 0;
    let mut rejected: u64 = 0;
    let mut fields: u64 = 0;
    let mut opaque: u64 = 0;
    let mut written: u64 = 0;

    // The measuring tap, sized deliberately.
    //
    // The first version folded every field and every output word with an FNV
    // step each — 19 multiply-adds per packet, which callgrind priced at
    // **52.6% of the whole measurement**. A tap that large distorts every share
    // taken from it (`codec-measurement` §6; the same defect the `hot_path`
    // harness already had to correct once, at 20%).
    //
    // So: fields are mixed into a per-packet accumulator with a shift and an
    // XOR — one or two instructions each — and the expensive FNV step runs
    // ONCE per packet. Coverage is unchanged: every field and every emitted
    // byte still enters the checksum, and the rotate makes the mixing
    // order-dependent so two swapped fields do not cancel.
    #[inline(always)]
    fn mix(acc: &mut u64, v: u64) {
        *acc = (*acc).rotate_left(7) ^ v;
    }
    let fold = |checksum: &mut u64, v: u64| {
        *checksum = checksum.wrapping_mul(0x0100_0000_01b3).wrapping_add(v);
    };

    for _ in 0..PASSES {
        for pkt in &corpus {
            // --- parse: the field-extraction path
            match NtpPacket::parse(pkt) {
                Ok(h) => {
                    parsed += 1;
                    // Every field enters the checksum, so a change to how any
                    // of them is read cannot pass unnoticed -- but they are
                    // MIXED cheaply and folded once (see `mix` above).
                    // Coverage without a 19-op tap: `write` is the INVERSE of
                    // `parse` and re-emits all 48 bytes from all 13 fields, so
                    // folding the six output words covers every field
                    // transitively -- any misparse changes an output byte.
                    //
                    // The three enum/version values are mixed as well, and
                    // deliberately: `bits_for_gate` is an INDEPENDENT
                    // reimplementation in this bench, so it pins that `parse`
                    // chose the right variant even if a vein changed
                    // `from_bits` and `bits` in compensating ways (which byte 0
                    // alone would round-trip past).
                    let mut acc: u64 = 0;
                    mix(&mut acc, h.leap.bits_for_gate() as u64);
                    mix(&mut acc, h.version as u64);
                    mix(&mut acc, h.mode.bits_for_gate() as u64);

                    // --- write: the serialisation path, same header back out
                    let mut out = [0u8; HEADER_LEN];
                    h.write(&mut out);
                    written += 1;
                    let (words, _) = out.as_chunks::<8>();
                    for w in words {
                        mix(&mut acc, u64::from_le_bytes(*w));
                    }
                    fold(&mut checksum, acc);
                }
                Err(_) => rejected += 1,
            }

            // --- the extension-field walk
            for t in ntp::extension_fields(pkt) {
                match t {
                    Trailer::Extension(f) => {
                        fields += 1;
                        let mut acc: u64 = 0;
                        mix(&mut acc, f.field_type as u64);
                        mix(&mut acc, f.value.len() as u64);
                        for b in f.value {
                            mix(&mut acc, *b as u64);
                        }
                        fold(&mut checksum, acc);
                    }
                    Trailer::Opaque(b) => {
                        opaque += 1;
                        fold(&mut checksum, b.len() as u64);
                    }
                }
            }
        }
    }

    // --- the arithmetic leaves: exercised once per packet, not per pass, so
    // they are priced without dominating the count.
    let mut ts_acc: u64 = 0;
    for i in 0..PACKETS as i64 {
        let t = NtpTimestamp::from_unix(1_787_856_000 + i, (i as u32).wrapping_mul(7_919));
        ts_acc ^= t.0;
        ts_acc = ts_acc.wrapping_add(NtpShort::from_seconds(i as f64 / 1024.0).0 as u64);
    }
    fold(&mut checksum, ts_acc);

    println!("packets    {PACKETS}");
    println!("passes     {PASSES}");
    println!("parsed     {parsed}");
    println!("rejected   {rejected}");
    println!("written    {written}");
    println!("fields     {fields}");
    println!("opaque     {opaque}");
    println!("CHECKSUM   {checksum:#018x}");
}

/// The gate needs the wire value of the two bit-packed enums, and their `bits`
/// methods are private to the codec. These mirror them for checksum purposes
/// ONLY — if a vein changes the enums' representation, the gate still folds the
/// same wire numbers, so the checksum stays comparable across that change.
trait GateBits {
    fn bits_for_gate(self) -> u8;
}

impl GateBits for LeapIndicator {
    fn bits_for_gate(self) -> u8 {
        match self {
            LeapIndicator::NoWarning => 0,
            LeapIndicator::LastMinute61 => 1,
            LeapIndicator::LastMinute59 => 2,
            LeapIndicator::Unsynchronized => 3,
        }
    }
}

impl GateBits for Mode {
    fn bits_for_gate(self) -> u8 {
        match self {
            Mode::Reserved => 0,
            Mode::SymmetricActive => 1,
            Mode::SymmetricPassive => 2,
            Mode::Client => 3,
            Mode::Server => 4,
            Mode::Broadcast => 5,
            Mode::Control => 6,
            Mode::Private => 7,
        }
    }
}
