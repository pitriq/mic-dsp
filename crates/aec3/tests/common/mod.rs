//! Synthetic signals for the integration tests: nothing here is a recording.
#![allow(dead_code)]

use aec3::FRAME;

/// White noise from a linear congruential generator, in -1.0 to 1.0.
pub struct Noise(pub u32);

impl Noise {
    pub fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1 << 23) as f32 - 1.0
    }
}

/// Noise shaped like speech: most of its energy below 4 kHz, in bursts of 600 ms with pauses
/// of 400 ms between. `offset` shifts the bursts, in samples.
pub fn speech_like(samples: usize, seed: u32, amplitude: f32, offset: usize) -> Vec<f32> {
    let mut noise = Noise(seed);
    let (mut low1, mut low2) = (0.0f32, 0.0f32);
    (0..samples)
        .map(|n| {
            // Two one-pole low-passes near 2.5 kHz.
            low1 += 0.28 * (noise.next() - low1);
            low2 += 0.28 * (low1 - low2);
            let phase = ((n + offset) % 48_000) as f32 / 48_000.0;
            let envelope = if phase < 0.6 {
                (std::f32::consts::PI * phase / 0.6).sin().powf(0.3)
            } else {
                0.0
            };
            amplitude * 3.0 * envelope * low2
        })
        .collect()
}

/// A small room: the direct path at `delay` samples and a few reflections after it.
pub fn echo_path(delay: usize) -> Vec<(usize, f32)> {
    vec![
        (delay, 0.30),
        (delay + 53, -0.14),
        (delay + 131, 0.09),
        (delay + 310, -0.05),
        (delay + 620, 0.03),
        (delay + 1100, 0.015),
    ]
}

/// The echo of `reference` through `path`.
pub fn echo(reference: &[f32], path: &[(usize, f32)]) -> Vec<f32> {
    let mut out = vec![0.0f32; reference.len()];
    for &(delay, gain) in path {
        for n in delay..reference.len() {
            out[n] += gain * reference[n - delay];
        }
    }
    out
}

pub fn frame(signal: &[f32], index: usize) -> [f32; FRAME] {
    signal[index * FRAME..(index + 1) * FRAME]
        .try_into()
        .unwrap()
}

/// Faint white noise, the microphone's own.
pub fn floor_noise(samples: usize, seed: u32, amplitude: f32) -> Vec<f32> {
    let mut noise = Noise(seed);
    (0..samples).map(|_| amplitude * noise.next()).collect()
}

pub fn mix(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| x + y).collect()
}

pub fn rms_db(signal: &[f32]) -> f32 {
    let power = signal.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / signal.len() as f64;
    (10.0 * power.max(1e-20).log10()) as f32
}
