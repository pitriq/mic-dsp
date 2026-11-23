//! Selects whether the suppressor is in the nearend or echo state
//! (dominant_nearend_detector.h/.cc, behind nearend_detector.h).

use crate::aec3_common::Spectrum;
use crate::config::DominantNearendDetection;

fn low_frequency_energy(spectrum: &Spectrum) -> f32 {
    spectrum[1..16].iter().fold(0.0, |acc, v| acc + v)
}

pub(crate) struct DominantNearendDetector {
    enr_threshold: f32,
    voiced_enr_threshold: f32,
    /// The caller's voice detection hears the near end.
    voice: bool,
    enr_exit_threshold: f32,
    snr_threshold: f32,
    hold_duration: i32,
    trigger_threshold: i32,
    nearend_state: bool,
    trigger_counter: i32,
    hold_counter: i32,
}

impl DominantNearendDetector {
    pub fn new(config: &DominantNearendDetection) -> DominantNearendDetector {
        DominantNearendDetector {
            enr_threshold: config.enr_threshold,
            voiced_enr_threshold: config.voiced_enr_threshold,
            voice: false,
            enr_exit_threshold: config.enr_exit_threshold,
            snr_threshold: config.snr_threshold,
            hold_duration: config.hold_duration,
            trigger_threshold: config.trigger_threshold,
            nearend_state: false,
            trigger_counter: 0,
            hold_counter: 0,
        }
    }

    /// Whether a voice detection outside the canceller hears the near end. It lowers the bar for
    /// entering the nearend state; leaving it is unchanged.
    pub fn set_voice(&mut self, voice: bool) {
        self.voice = voice;
    }

    /// Returns whether the current state is the nearend state.
    pub fn is_nearend_state(&self) -> bool {
        self.nearend_state
    }

    /// Updates the state selection based on latest spectral estimates. The detector also runs
    /// during the initial phase (`use_during_initial_phase` is on).
    pub fn update(
        &mut self,
        nearend_spectrum: &Spectrum,
        residual_echo_spectrum: &Spectrum,
        comfort_noise_spectrum: &Spectrum,
    ) {
        let ne_sum = low_frequency_energy(nearend_spectrum);
        let echo_sum = low_frequency_energy(residual_echo_spectrum);
        let noise_sum = low_frequency_energy(comfort_noise_spectrum);

        // Detect strong active nearend if the nearend is sufficiently stronger than the echo and
        // the nearend noise.
        let enr_threshold = if self.voice {
            self.voiced_enr_threshold
        } else {
            self.enr_threshold
        };
        if echo_sum < enr_threshold * ne_sum && ne_sum > self.snr_threshold * noise_sum {
            self.trigger_counter += 1;
            if self.trigger_counter >= self.trigger_threshold {
                // After a period of strong active nearend activity, flag nearend mode.
                self.hold_counter = self.hold_duration;
                self.trigger_counter = self.trigger_threshold;
            }
        } else {
            // Forget previously detected strong active nearend activity.
            self.trigger_counter = (self.trigger_counter - 1).max(0);
        }

        // Exit nearend-state early at strong echo.
        if echo_sum > self.enr_exit_threshold * ne_sum && echo_sum > self.snr_threshold * noise_sum
        {
            self.hold_counter = 0;
        }

        // Remain in any nearend mode for a certain duration.
        self.hold_counter = (self.hold_counter - 1).max(0);
        self.nearend_state = self.hold_counter > 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aec3_common::FFT_LENGTH_BY_2_PLUS_1;
    use crate::config::EchoCanceller3Config;

    #[test]
    fn strong_nearend_triggers_and_holds() {
        let config = EchoCanceller3Config::default()
            .suppressor
            .dominant_nearend_detection;
        let mut detector = DominantNearendDetector::new(&config);
        let nearend = [1.0e6f32; FFT_LENGTH_BY_2_PLUS_1];
        let echo = [1.0e3f32; FFT_LENGTH_BY_2_PLUS_1];
        let noise = [1.0e2f32; FFT_LENGTH_BY_2_PLUS_1];

        for _ in 0..config.trigger_threshold - 1 {
            detector.update(&nearend, &echo, &noise);
            assert!(!detector.is_nearend_state());
        }
        detector.update(&nearend, &echo, &noise);
        assert!(detector.is_nearend_state());

        // Weak nearend keeps the state for the hold duration.
        let quiet = [1.0e2f32; FFT_LENGTH_BY_2_PLUS_1];
        for _ in 0..config.hold_duration - 2 {
            detector.update(&quiet, &quiet, &noise);
            assert!(detector.is_nearend_state());
        }
        detector.update(&quiet, &quiet, &noise);
        assert!(!detector.is_nearend_state());
    }

    #[test]
    fn a_voice_lowers_the_bar_for_entering() {
        let mut config = EchoCanceller3Config::default()
            .suppressor
            .dominant_nearend_detection;
        config.voiced_enr_threshold = 2.0;
        // Echo at the nearend's level: above the normal threshold, under the voiced one.
        let nearend = [1.0e6f32; FFT_LENGTH_BY_2_PLUS_1];
        let echo = [1.0e6f32; FFT_LENGTH_BY_2_PLUS_1];
        let noise = [1.0e2f32; FFT_LENGTH_BY_2_PLUS_1];
        let entered = |voice: bool| {
            let mut detector = DominantNearendDetector::new(&config);
            detector.set_voice(voice);
            for _ in 0..config.trigger_threshold {
                detector.update(&nearend, &echo, &noise);
            }
            detector.is_nearend_state()
        };
        assert!(!entered(false));
        assert!(entered(true));
    }

    #[test]
    fn strong_echo_exits_at_once() {
        let config = EchoCanceller3Config::default()
            .suppressor
            .dominant_nearend_detection;
        let mut detector = DominantNearendDetector::new(&config);
        let nearend = [1.0e6f32; FFT_LENGTH_BY_2_PLUS_1];
        let echo = [1.0e3f32; FFT_LENGTH_BY_2_PLUS_1];
        let noise = [1.0e2f32; FFT_LENGTH_BY_2_PLUS_1];
        for _ in 0..config.trigger_threshold {
            detector.update(&nearend, &echo, &noise);
        }
        assert!(detector.is_nearend_state());
        let strong_echo = [1.0e8f32; FFT_LENGTH_BY_2_PLUS_1];
        detector.update(&nearend, &strong_echo, &noise);
        assert!(!detector.is_nearend_state());
    }
}
