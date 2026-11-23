//! An exponential reverberant model that can be applied over power spectra (reverb_model.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2_PLUS_1};

pub(crate) struct ReverbModel {
    reverb: Spectrum,
}

impl ReverbModel {
    pub fn new() -> ReverbModel {
        ReverbModel {
            reverb: [0.0; FFT_LENGTH_BY_2_PLUS_1],
        }
    }

    /// Returns the reverb.
    pub fn reverb(&self) -> &Spectrum {
        &self.reverb
    }

    // The two update methods update the estimate of the reverberation contribution to an
    // input/output power spectrum. Before applying the exponential reverberant model, the input
    // power spectrum is pre-scaled: per frequency in `update_reverb`, and with the same scaling
    // for all the frequencies in `update_reverb_no_freq_shaping`.

    pub fn update_reverb_no_freq_shaping(
        &mut self,
        power_spectrum: &Spectrum,
        power_spectrum_scaling: f32,
        reverb_decay: f32,
    ) {
        if reverb_decay > 0.0 {
            // Update the estimate of the reverberant power.
            for (reverb, power) in self.reverb.iter_mut().zip(power_spectrum) {
                *reverb = (*reverb + power * power_spectrum_scaling) * reverb_decay;
            }
        }
    }

    /// Update the reverb based on new data.
    pub fn update_reverb(
        &mut self,
        power_spectrum: &Spectrum,
        power_spectrum_scaling: &Spectrum,
        reverb_decay: f32,
    ) {
        if reverb_decay > 0.0 {
            // Update the estimate of the reverberant power.
            for ((reverb, power), scaling) in self
                .reverb
                .iter_mut()
                .zip(power_spectrum)
                .zip(power_spectrum_scaling)
            {
                *reverb = (*reverb + power * scaling) * reverb_decay;
            }
        }
    }
}
