//! The chain's stages. Each is a concrete struct built with the sample rate it runs at;
//! `Chain` holds them by value and calls them statically.

pub(crate) mod compressor;
pub(crate) mod deesser;
pub(crate) mod denoise;
pub(crate) mod eq;
pub(crate) mod expander;
pub(crate) mod highpass;
pub(crate) mod limiter;
#[cfg(any(test, feature = "offline"))]
pub(crate) mod testing;

use std::f64::consts::PI;

/// The stage contract. It exists so the test helpers are generic; the chain never uses it
/// dynamically.
pub(crate) trait Stage {
    type Params: Copy + PartialEq;
    fn new(sample_rate: f32, params: Self::Params) -> Self;
    /// Real-time safe: coefficient math only, filter state is kept.
    fn set_params(&mut self, params: Self::Params);
    /// In place and real-time safe.
    fn process(&mut self, buf: &mut [f32]);
}

pub(crate) fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Clamped at -200 dB so silence stays finite.
pub(crate) fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-10).log10()
}

/// The per-step coefficient of a one-pole smoother with time constant `seconds`.
pub(crate) fn smoothing(seconds: f32, steps_per_second: f32) -> f32 {
    1.0 - (-1.0 / (seconds * steps_per_second)).exp()
}

/// One second-order section, transposed direct form II. Coefficients and state are f64, so
/// the 75 Hz high-pass keeps its precision at 48 kHz.
#[derive(Clone, Copy)]
pub(crate) struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    pub(crate) fn new(shape: Shape, sample_rate: f32) -> Biquad {
        let mut biquad = Biquad {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        };
        biquad.set(shape, sample_rate);
        biquad
    }

    /// New coefficients (Audio EQ Cookbook). The state carries over.
    pub(crate) fn set(&mut self, shape: Shape, sample_rate: f32) {
        let (hz, q) = match shape {
            Shape::Highpass { hz, q } | Shape::Peak { hz, q, .. } => (hz as f64, q as f64),
        };
        let w = 2.0 * PI * hz / sample_rate as f64;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        let (b, a) = match shape {
            Shape::Highpass { .. } => (
                [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
                [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
            ),
            Shape::Peak { gain_db, .. } => {
                let amp = 10f64.powf(gain_db as f64 / 40.0);
                (
                    [1.0 + alpha * amp, -2.0 * cos, 1.0 - alpha * amp],
                    [1.0 + alpha / amp, -2.0 * cos, 1.0 - alpha / amp],
                )
            }
        };
        self.b0 = b[0] / a[0];
        self.b1 = b[1] / a[0];
        self.b2 = b[2] / a[0];
        self.a1 = a[1] / a[0];
        self.a2 = a[2] / a[0];
    }

    #[inline]
    pub(crate) fn tick(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y as f32
    }

    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        for x in buf {
            *x = self.tick(*x);
        }
    }

    pub(crate) fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Shape {
    Highpass { hz: f32, q: f32 },
    Peak { hz: f32, q: f32, gain_db: f32 },
}
