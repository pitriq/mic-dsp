//! Updates the frequency response for the reverb (reverb_frequency_response.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1};

/// Computes the ratio of the energies between the direct path and the tail. The energy is
/// computed in the power spectrum domain discarding the DC contributions.
fn average_decay_within_filter(freq_resp_direct_path: &Spectrum, freq_resp_tail: &Spectrum) -> f32 {
    // Skipping the DC for the ratio computation
    const SKIP_BINS: usize = 1;

    let direct_path_energy: f32 = freq_resp_direct_path[SKIP_BINS..]
        .iter()
        .fold(0.0, |acc, v| acc + v);

    if direct_path_energy == 0.0 {
        return 0.0;
    }

    let tail_energy: f32 = freq_resp_tail[SKIP_BINS..]
        .iter()
        .fold(0.0, |acc, v| acc + v);
    tail_energy / direct_path_energy
}

pub(crate) struct ReverbFrequencyResponse {
    average_decay: f32,
    tail_response: Spectrum,
}

impl ReverbFrequencyResponse {
    pub fn new() -> ReverbFrequencyResponse {
        ReverbFrequencyResponse {
            average_decay: 0.0,
            tail_response: [0.0; FFT_LENGTH_BY_2_PLUS_1],
        }
    }

    /// Updates the frequency response estimate of the reverb.
    pub fn update(
        &mut self,
        frequency_response: &[Spectrum],
        filter_delay_blocks: usize,
        linear_filter_quality: Option<f32>,
    ) {
        let Some(linear_filter_quality) = linear_filter_quality else {
            return;
        };

        let freq_resp_tail = &frequency_response[frequency_response.len() - 1];
        let freq_resp_direct_path = &frequency_response[filter_delay_blocks];

        let average_decay = average_decay_within_filter(freq_resp_direct_path, freq_resp_tail);

        let smoothing = 0.2 * linear_filter_quality;
        self.average_decay += smoothing * (average_decay - self.average_decay);

        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            self.tail_response[k] = freq_resp_direct_path[k] * self.average_decay;
        }

        // The conservative tail frequency response (on by default): never below the response
        // of the last partition.
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            self.tail_response[k] = freq_resp_tail[k].max(self.tail_response[k]);
        }

        for k in 1..FFT_LENGTH_BY_2 {
            let avg_neighbour = 0.5 * (self.tail_response[k - 1] + self.tail_response[k + 1]);
            self.tail_response[k] = self.tail_response[k].max(avg_neighbour);
        }
    }

    /// Returns the estimated frequency response for the reverb.
    pub fn frequency_response(&self) -> &Spectrum {
        &self.tail_response
    }
}
