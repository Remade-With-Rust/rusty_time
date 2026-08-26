//! TIMECORP scenario definitions (mission plan §7.2).
//!
//! Only scenarios the harness actually implements are listed; the ledger names
//! the rest as pending rather than this file pretending. Numbers mirror the
//! mission plan's sketches.

#[derive(Clone, Copy, Debug)]
pub struct Scenario {
    pub name: &'static str,
    pub what: &'static str,
    /// Simulated duration, seconds.
    pub duration_s: f64,
    /// local − true at t = 0, seconds.
    pub initial_offset_s: f64,
    /// Undisciplined local clock frequency error, ppm.
    pub base_freq_ppm: f64,
    /// Random-walk frequency wander, ppm per √s.
    pub wander_ppm_sqrt_s: f64,
    /// Path round-trip, seconds.
    pub rtt_s: f64,
    /// One-way jitter standard deviation, seconds.
    pub jitter_sd_s: f64,
    /// Extra one-way delay on the outbound path only (asymmetry), seconds.
    pub asym_extra_s: f64,
    /// Packet loss probability per exchange.
    pub loss: f64,
}

pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "S1",
        what: "LAN symmetric — best-case convergence + floor accuracy",
        duration_s: 14_400.0,
        initial_offset_s: 0.010,
        base_freq_ppm: 20.0,
        wander_ppm_sqrt_s: 0.001,
        rtt_s: 200e-6,
        jitter_sd_s: 10e-6,
        asym_extra_s: 0.0,
        loss: 0.0,
    },
    Scenario {
        name: "S6",
        what: "cold start, 500 ms initial offset — initial convergence",
        duration_s: 3_600.0,
        initial_offset_s: 0.500,
        base_freq_ppm: 20.0,
        wander_ppm_sqrt_s: 0.001,
        rtt_s: 200e-6,
        jitter_sd_s: 10e-6,
        asym_extra_s: 0.0,
        loss: 0.0,
    },
    Scenario {
        name: "S8",
        what: "drifty oscillator, +100 ppm with wander — frequency tracking",
        duration_s: 28_800.0,
        initial_offset_s: 0.010,
        base_freq_ppm: 100.0,
        wander_ppm_sqrt_s: 0.01,
        rtt_s: 200e-6,
        jitter_sd_s: 10e-6,
        asym_extra_s: 0.0,
        loss: 0.0,
    },
];

pub fn by_name(name: &str) -> Option<&'static Scenario> {
    SCENARIOS.iter().find(|s| s.name.eq_ignore_ascii_case(name))
}
