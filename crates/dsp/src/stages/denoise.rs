//! nnnoiseless behind the stage contract: i16 scaling, a dry path delayed by the denoiser's
//! latency, and a wet amount driven by its voice detection.

use super::{smoothing, Stage};
use crate::FRAME;
use nnnoiseless::DenoiseState;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    /// Wet amount during speech. Pauses are always fully wet.
    pub speech_wet: f32,
}

/// What the denoiser heard in the frame it just consumed.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub(crate) struct Voice {
    /// Voice probability, 0 to 1.
    pub prob: f32,
    /// `prob` through the hysteresis.
    pub speech: bool,
}

pub(crate) const VAD_ON: f32 = 0.6;
pub(crate) const VAD_OFF: f32 = 0.35;

/// The denoiser's output is its input one frame earlier. The tests pin it.
pub(crate) const LATENCY: usize = FRAME;

/// nnnoiseless works at i16 scale.
const SCALE: f32 = 32768.0;
const WET_SMOOTHING_S: f32 = 0.03;

pub(crate) struct Denoise {
    state: Box<DenoiseState<'static>>,
    params: Params,
    /// The previous input frame: the dry path, delayed by `LATENCY`.
    dry: [f32; FRAME],
    scaled: [f32; FRAME],
    denoised: [f32; FRAME],
    wet: f32,
    wet_coef: f32,
    voice: Voice,
}

impl Denoise {
    pub(crate) fn voice(&self) -> Voice {
        self.voice
    }

    fn frame(&mut self, frame: &mut [f32]) {
        for (scaled, x) in self.scaled.iter_mut().zip(frame.iter()) {
            *scaled = x * SCALE;
        }
        let prob = self.state.process_frame(&mut self.denoised, &self.scaled);
        let speech = if prob >= VAD_ON {
            true
        } else if prob < VAD_OFF {
            false
        } else {
            self.voice.speech
        };
        self.voice = Voice { prob, speech };

        let target = if speech { self.params.speech_wet } else { 1.0 };
        let from = self.wet;
        let step = (target - from) * self.wet_coef / FRAME as f32;
        for (i, x) in frame.iter_mut().enumerate() {
            let wet = from + step * (i + 1) as f32;
            let out = wet * self.denoised[i] / SCALE + (1.0 - wet) * self.dry[i];
            self.dry[i] = *x;
            *x = out;
        }
        self.wet = from + step * FRAME as f32;
    }
}

impl Stage for Denoise {
    type Params = Params;

    /// Allocates. The shared nnnoiseless tables are primed separately, by `prime`.
    fn new(sample_rate: f32, params: Params) -> Denoise {
        Denoise {
            state: DenoiseState::new(),
            params,
            dry: [0.0; FRAME],
            scaled: [0.0; FRAME],
            denoised: [0.0; FRAME],
            wet: 1.0,
            wet_coef: smoothing(WET_SMOOTHING_S, sample_rate / FRAME as f32),
            voice: Voice::default(),
        }
    }

    fn set_params(&mut self, params: Params) {
        self.params = params;
    }

    /// Whole frames only: the chain re-blocks to `FRAME` and is the only caller.
    fn process(&mut self, buf: &mut [f32]) {
        debug_assert!(buf.len().is_multiple_of(FRAME));
        for frame in buf.chunks_exact_mut(FRAME) {
            self.frame(frame);
        }
    }
}

/// Runs a throwaway denoiser so nnnoiseless builds its shared lazy tables on the calling
/// thread. Without this the first audio callback would build them.
pub(crate) fn prime() {
    let mut state = DenoiseState::new();
    let mut out = [0.0; FRAME];
    let mut seed = 1u32;
    for _ in 0..4 {
        let noise: [f32; FRAME] = std::array::from_fn(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 2000.0
        });
        state.process_frame(&mut out, &noise);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone too quiet for nnnoiseless to treat as audio passes through its transform
    /// unchanged, so the output must equal the input delayed by exactly `LATENCY`.
    #[test]
    fn denoiser_latency_is_one_frame() {
        let input: Vec<f32> = (0..FRAME * 8)
            .map(|i| 0.02 * (std::f32::consts::TAU * 3000.0 * i as f32 / 48_000.0).sin())
            .collect();
        let mut state = DenoiseState::new();
        let mut output = vec![0.0; input.len()];
        for (x, y) in input
            .chunks_exact(FRAME)
            .zip(output.chunks_exact_mut(FRAME))
        {
            assert_eq!(
                state.process_frame(y, x),
                0.0,
                "the tone must stay on the silent path"
            );
        }
        let error = |lag: usize| {
            let (mut err, mut energy) = (0.0f64, 0.0f64);
            for i in FRAME * 3..FRAME * 7 {
                err += ((output[i] - input[i - lag]) as f64).powi(2);
                energy += (input[i - lag] as f64).powi(2);
            }
            (err / energy).sqrt()
        };
        assert!(error(LATENCY) < 0.02, "relative error {}", error(LATENCY));
        assert!(error(LATENCY - 1) > 0.2);
        assert!(error(LATENCY + 1) > 0.2);
    }

    /// With the stage mostly dry, an impulse must come out once, `LATENCY` samples later.
    /// An undelayed dry path would put it in the same frame it went in.
    #[test]
    fn impulse_is_aligned_on_the_dry_and_wet_paths() {
        let mut denoise = Denoise::new(48_000.0, Params { speech_wet: 0.8 });
        denoise.wet = 0.0;
        let at = 2 * FRAME + 137;
        let mut signal = vec![0.0f32; FRAME * 6];
        signal[at] = 0.5;
        denoise.process(&mut signal);

        let peak = (0..signal.len()).max_by(|&a, &b| signal[a].abs().total_cmp(&signal[b].abs()));
        assert_eq!(peak, Some(at + LATENCY));
        assert!(signal[at + LATENCY] > 0.1);
        assert!(signal[..at + LATENCY - 240].iter().all(|x| x.abs() < 1e-3));
    }

    #[test]
    fn wet_is_full_in_pauses() {
        let mut denoise = Denoise::new(48_000.0, Params { speech_wet: 0.8 });
        let mut hiss = crate::stages::testing::noise(FRAME * 100, 0.001, 3);
        denoise.process(&mut hiss);
        assert!(!denoise.voice().speech);
        assert_eq!(denoise.wet, 1.0);
    }
}
