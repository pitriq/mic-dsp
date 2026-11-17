//! The config file's schema, and the one place where the output target becomes numbers.

use crate::cal;
use crate::stages::{compressor, deesser, denoise, eq, expander, highpass, limiter};
use serde::Deserialize;
use std::fmt;

/// A validated config. It only comes from `from_toml` or `Default`.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Config(Schema);

#[derive(Clone, PartialEq, Debug)]
pub enum ConfigError {
    /// Not TOML, or a key or type the schema doesn't have.
    Parse(String),
    /// A value outside its range. Nonfinite values land here too.
    OutOfRange {
        field: &'static str,
        value: f32,
        min: f32,
        max: f32,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConfigError::Parse(message) => write!(f, "config does not parse: {}", message.trim()),
            ConfigError::OutOfRange {
                field,
                value,
                min,
                max,
            } => {
                write!(
                    f,
                    "config value {field} = {value} is outside {min} to {max}"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Missing keys take their defaults. Unknown keys, out-of-range values and nonfinite
    /// values are errors, so a typo never changes the sound silently.
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        let schema: Schema =
            toml::from_str(text).map_err(|error| ConfigError::Parse(error.to_string()))?;
        for (field, value, min, max) in schema.ranges() {
            if !(min..=max).contains(&value) {
                return Err(ConfigError::OutOfRange {
                    field,
                    value,
                    min,
                    max,
                });
            }
        }
        Ok(Config(schema))
    }

    /// The output target: the 95th percentile of 100 ms speech peaks. The report checks the
    /// output against it.
    #[cfg(feature = "offline")]
    pub(crate) fn target_peak_dbfs(&self) -> f32 {
        self.0.target.peak_dbfs
    }

    #[cfg(feature = "offline")]
    pub(crate) fn highpass_hz(&self) -> f32 {
        self.0.highpass.hz
    }
}

macro_rules! sections {
    ($($(#[$doc:meta])* $name:ident { $($field:ident: $type:ty = $default:expr),* $(,)? })*) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Debug, Deserialize)]
        #[cfg_attr(test, derive(serde::Serialize))]
        #[serde(default, deny_unknown_fields)]
        struct $name { $($field: $type),* }

        impl Default for $name {
            fn default() -> $name {
                $name { $($field: $default),* }
            }
        }
    )*};
}

sections! {
    /// The only level setting. Everything else that is a level derives from it.
    Target { peak_dbfs: f32 = -11.0 }
    Highpass { hz: f32 = 75.0 }
    Denoise { speech_wet: f32 = 0.8 }
    Expander { range_db: f32 = 12.0 }
    Cut { enabled: bool = false, hz: f32 = 250.0, gain_db: f32 = -3.0, q: f32 = 1.0 }
    /// The threshold sits `depth_db` below the target peak.
    Compressor { depth_db: f32 = 6.0, ratio: f32 = 2.0 }
    Presence { enabled: bool = true, hz: f32 = 4000.0, gain_db: f32 = 2.0, q: f32 = 0.7 }
    /// `threshold_db` is relative to the target peak.
    Deesser { enabled: bool = false, hz: f32 = 6000.0, threshold_db: f32 = -18.0, range_db: f32 = 6.0 }
}

#[derive(Clone, Copy, PartialEq, Debug, Default, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(default, deny_unknown_fields)]
struct Schema {
    target: Target,
    highpass: Highpass,
    denoise: Denoise,
    expander: Expander,
    cut: Cut,
    compressor: Compressor,
    presence: Presence,
    deesser: Deesser,
}

impl Schema {
    fn ranges(&self) -> [(&'static str, f32, f32, f32); 16] {
        [
            ("target.peak_dbfs", self.target.peak_dbfs, -24.0, -6.0),
            ("highpass.hz", self.highpass.hz, 40.0, 150.0),
            ("denoise.speech_wet", self.denoise.speech_wet, 0.7, 0.85),
            ("expander.range_db", self.expander.range_db, 10.0, 15.0),
            ("cut.hz", self.cut.hz, 80.0, 1000.0),
            ("cut.gain_db", self.cut.gain_db, -12.0, 0.0),
            ("cut.q", self.cut.q, 0.3, 4.0),
            ("compressor.depth_db", self.compressor.depth_db, 0.0, 12.0),
            ("compressor.ratio", self.compressor.ratio, 1.0, 8.0),
            ("presence.hz", self.presence.hz, 1000.0, 10_000.0),
            ("presence.gain_db", self.presence.gain_db, 0.0, 6.0),
            ("presence.q", self.presence.q, 0.3, 4.0),
            ("deesser.hz", self.deesser.hz, 3000.0, 10_000.0),
            (
                "deesser.threshold_db",
                self.deesser.threshold_db,
                -40.0,
                0.0,
            ),
            ("deesser.range_db", self.deesser.range_db, 0.0, 12.0),
            // The compressor's makeup has to fit under the limiter.
            (
                "target.peak_dbfs + compressor makeup",
                self.target.peak_dbfs + self.makeup_db(),
                -24.0,
                LIMITER_CEILING_DBFS - 3.0,
            ),
        ]
    }

    /// With this makeup the compressor's static curve maps the target peak to itself.
    fn makeup_db(&self) -> f32 {
        self.compressor.depth_db * (1.0 - 1.0 / self.compressor.ratio)
    }
}

const LIMITER_CEILING_DBFS: f32 = -1.0;

/// Every stage's parameters. `Copy`, so it crosses to the audio thread through the handoff.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    pub highpass: highpass::Params,
    pub cal: cal::Params,
    pub denoise: denoise::Params,
    pub expander: expander::Params,
    pub cut: eq::Params,
    pub compressor: compressor::Params,
    pub presence: eq::Params,
    pub deesser: deesser::Params,
    pub limiter: limiter::Params,
}

impl Params {
    pub(crate) fn derive(config: &Config) -> Params {
        let Schema {
            target,
            highpass,
            denoise,
            expander,
            cut,
            compressor,
            presence,
            deesser,
        } = config.0;
        Params {
            highpass: highpass::Params { hz: highpass.hz },
            // The trim puts the 95th percentile of speech peaks on the target.
            cal: cal::Params {
                target_dbfs: target.peak_dbfs,
            },
            denoise: denoise::Params {
                speech_wet: denoise.speech_wet,
            },
            expander: expander::Params {
                range_db: expander.range_db,
            },
            cut: eq::Params {
                enabled: cut.enabled,
                hz: cut.hz,
                gain_db: cut.gain_db,
                q: cut.q,
            },
            compressor: compressor::Params {
                threshold_dbfs: target.peak_dbfs - compressor.depth_db,
                ratio: compressor.ratio,
                makeup_db: config.0.makeup_db(),
            },
            presence: eq::Params {
                enabled: presence.enabled,
                hz: presence.hz,
                gain_db: presence.gain_db,
                q: presence.q,
            },
            deesser: deesser::Params {
                enabled: deesser.enabled,
                hz: deesser.hz,
                threshold_dbfs: target.peak_dbfs + deesser.threshold_db,
                range_db: deesser.range_db,
            },
            limiter: limiter::Params {
                ceiling_dbfs: LIMITER_CEILING_DBFS,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_the_default() {
        assert_eq!(Config::from_toml(""), Ok(Config::default()));
    }

    #[test]
    fn round_trips_through_toml() {
        let mut schema = Schema::default();
        schema.target.peak_dbfs = -14.0;
        schema.cut.enabled = true;
        schema.compressor.ratio = 3.0;
        schema.deesser.hz = 7000.0;
        let text = toml::to_string(&schema).unwrap();
        assert_eq!(Config::from_toml(&text), Ok(Config(schema)));
        assert_ne!(Config(schema), Config::default());

        let text = toml::to_string(&Schema::default()).unwrap();
        assert_eq!(Config::from_toml(&text), Ok(Config::default()));
    }

    #[test]
    fn a_partial_section_keeps_the_other_defaults() {
        let config = Config::from_toml("[presence]\ngain_db = 3\n").unwrap();
        let presence = Params::derive(&config).presence;
        assert_eq!(
            presence,
            eq::Params {
                enabled: true,
                hz: 4000.0,
                gain_db: 3.0,
                q: 0.7
            }
        );
    }

    #[test]
    fn rejects_out_of_range_values() {
        for text in [
            "[target]\npeak_dbfs = 0.0",
            "[highpass]\nhz = 10.0",
            "[denoise]\nspeech_wet = 0.5",
            "[expander]\nrange_db = 30.0",
            "[compressor]\nratio = 0.5",
            "[presence]\ngain_db = -2.0",
            "[target]\npeak_dbfs = -6.0\n[compressor]\ndepth_db = 12.0\nratio = 8.0",
        ] {
            let error = Config::from_toml(text).unwrap_err();
            assert!(
                matches!(error, ConfigError::OutOfRange { .. }),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn rejects_nonfinite_values() {
        for value in ["nan", "inf", "-inf"] {
            let text = format!("[target]\npeak_dbfs = {value}");
            let error = Config::from_toml(&text).unwrap_err();
            assert!(
                matches!(error, ConfigError::OutOfRange { .. }),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn rejects_unknown_keys_and_wrong_types() {
        for text in [
            "[target]\npeak = -11.0",
            "[reverb]\nwet = 1.0",
            "[cut]\nenabled = 1",
            "not toml",
        ] {
            let error = Config::from_toml(text).unwrap_err();
            assert!(matches!(error, ConfigError::Parse(_)), "{text}: {error}");
        }
    }

    #[test]
    fn everything_derives_from_the_one_target() {
        let params = Params::derive(&Config::default());
        assert_eq!(params.cal.target_dbfs, -11.0);
        assert_eq!(params.compressor.threshold_dbfs, -17.0);
        assert_eq!(params.compressor.makeup_db, 3.0);
        assert_eq!(params.limiter.ceiling_dbfs, -1.0);

        let quieter = Config::from_toml("[target]\npeak_dbfs = -15.0").unwrap();
        let params = Params::derive(&quieter);
        assert_eq!(params.cal.target_dbfs, -15.0);
        assert_eq!(params.compressor.threshold_dbfs, -21.0);
        assert_eq!(params.deesser.threshold_dbfs, -33.0);
    }
}
