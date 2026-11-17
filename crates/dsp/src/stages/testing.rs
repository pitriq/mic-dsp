//! Synthetic signals and measurements, for the tests and for the benchmark's fallback input.
//! No recording of the owner's voice is ever committed, so these stand in for one.

use super::db_to_lin;
#[cfg(test)]
use super::{lin_to_db, Stage};
use crate::FRAME;

/// A small deterministic generator, so every test signal is reproducible.
pub(crate) struct Rng(u32);

impl Rng {
    pub(crate) fn new(seed: u32) -> Rng {
        Rng(seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453) | 1)
    }

    /// Uniform in 0 to 1.
    pub(crate) fn unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 >> 8) as f32 / (1 << 24) as f32
    }

    pub(crate) fn between(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.unit()
    }

    /// Roughly normal, mean 0 and deviation 1.
    pub(crate) fn normal(&mut self) -> f32 {
        (0..12).map(|_| self.unit()).sum::<f32>() - 6.0
    }
}

#[cfg(test)]
pub(crate) fn sine(samples: usize, hz: f32, amplitude: f32) -> Vec<f32> {
    (0..samples)
        .map(|i| amplitude * (std::f64::consts::TAU * hz as f64 * i as f64 / 48_000.0).sin() as f32)
        .collect()
}

/// White noise, uniform in ±`amplitude`.
#[cfg(test)]
pub(crate) fn noise(samples: usize, amplitude: f32, seed: u32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    (0..samples)
        .map(|_| rng.between(-amplitude, amplitude))
        .collect()
}

#[cfg(test)]
fn rms(buf: &[f32]) -> f32 {
    (buf.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / buf.len() as f64).sqrt() as f32
}

/// The stage's gain for a steady sine, measured after it has settled.
#[cfg(test)]
pub(crate) fn gain_db<S: Stage>(stage: &mut S, hz: f32) -> f32 {
    let input = sine(96_000, hz, 0.25);
    let mut output = input.clone();
    for block in output.chunks_mut(FRAME) {
        stage.process(block);
    }
    lin_to_db(rms(&output[48_000..]) / rms(&input[48_000..]))
}

/// The settled output peak for a steady sine whose peak is `level_dbfs`.
#[cfg(test)]
pub(crate) fn steady_peak_db_at<S: Stage>(stage: &mut S, hz: f32, level_dbfs: f32) -> f32 {
    let mut signal = sine(96_000, hz, db_to_lin(level_dbfs));
    for block in signal.chunks_mut(FRAME) {
        stage.process(block);
    }
    lin_to_db(
        signal[86_400..]
            .iter()
            .fold(0.0, |peak, x| peak.max(x.abs())),
    )
}

#[cfg(test)]
pub(crate) fn steady_peak_db<S: Stage>(stage: &mut S, level_dbfs: f32) -> f32 {
    steady_peak_db_at(stage, 1000.0, level_dbfs)
}

/// Speech-like audio at the owner's baseline levels, with the frames a voice detector would
/// call speech.
pub(crate) struct Speechlike {
    pub samples: Vec<f32>,
    /// One flag per `FRAME`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub speech: Vec<bool>,
}

/// The baseline: 95th percentile of 100 ms speech peaks, and the hiss.
pub(crate) const SPEECH_P95_DBFS: f32 = -18.0;
pub(crate) const HISS_DBFS: f32 = -67.0;

/// Phrases of voiced syllables with pauses in between, over steady hiss. Levels vary from
/// syllable to syllable and from phrase to phrase like the owner's speech does: 100 ms peaks
/// have a deviation near 4 dB, and their 95th percentile sits about 7 dB above their mean.
///
/// `variation` scales that: 1.0 is the owner's speech, and a small value is a talker who
/// holds one level, which shows the trim's own step response.
pub(crate) fn speechlike(seconds: f32, seed: u32, variation: f32) -> Speechlike {
    let mut rng = Rng::new(seed);
    let total = (seconds * 48_000.0) as usize / FRAME * FRAME;
    let mut samples = vec![0.0f32; total];
    let mut speech = vec![false; total / FRAME];

    let mut at = (rng.between(0.2, 0.5) * 48_000.0) as usize;
    while at < total {
        // One phrase, a little louder or quieter than the last.
        let phrase_end = at + (rng.between(1.5, 4.0) * 48_000.0) as usize;
        let phrase_db = 2.0 * variation * rng.normal();
        let start = at;
        while at < phrase_end.min(total) {
            let length = (rng.between(0.12, 0.3) * 48_000.0) as usize;
            let level = db_to_lin(phrase_db + variation * (4.5 * rng.normal()).clamp(-15.0, 7.0));
            let hz = rng.between(100.0, 170.0);
            let mut phase = rng.unit();
            let (mut top, mut bottom) = (0.0f32, 0.0f32);
            for i in 0..length.min(total - at) {
                // A sawtooth between two one-pole low-passes: the harmonics from about 300
                // to 1500 Hz. With little below that, the chain's high-pass leaves the
                // peaks where they are.
                phase = (phase + hz / 48_000.0).fract();
                let saw = 2.0 * phase - 1.0;
                top += 0.2 * (saw - top);
                bottom += 0.04 * (saw - bottom);
                let envelope = (std::f32::consts::PI * i as f32 / length as f32)
                    .sin()
                    .powi(2);
                samples[at + i] = level * envelope * (top - bottom);
            }
            at += length + (rng.between(0.02, 0.08) * 48_000.0) as usize;
        }
        for flag in &mut speech[start / FRAME..(at.min(total)) / FRAME] {
            *flag = true;
        }
        at += (rng.between(0.4, 1.0) * 48_000.0) as usize;
    }

    // Put the 95th percentile of 100 ms speech peaks on the baseline, then add the hiss.
    let mut peaks: Vec<f32> = samples
        .chunks(10 * FRAME)
        .zip(speech.chunks(10))
        .filter(|(_, flags)| flags.iter().filter(|flag| **flag).count() >= 5)
        .map(|(window, _)| window.iter().fold(0.0f32, |peak, x| peak.max(x.abs())))
        .collect();
    peaks.sort_by(f32::total_cmp);
    let p95 = peaks
        .get(peaks.len() * 95 / 100)
        .copied()
        .unwrap_or(1.0)
        .max(1e-6);
    let gain = db_to_lin(SPEECH_P95_DBFS) / p95;
    let hiss = db_to_lin(HISS_DBFS) * 3f32.sqrt();
    for x in &mut samples {
        *x = *x * gain + rng.between(-hiss, hiss);
    }
    Speechlike { samples, speech }
}
