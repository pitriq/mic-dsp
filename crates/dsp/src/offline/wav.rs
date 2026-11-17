//! WAV files in and out.

use super::Error;
use crate::SAMPLE_RATE;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::path::Path;

/// Channel 1 of a 48 kHz file, as ±1.0 floats. Other rates are rejected: the chain never
/// resamples. So are nonfinite samples and a channel of exact zeros (an unplugged interface or
/// a denied mic permission), which would otherwise measure as a perfect noise floor.
pub(crate) fn read(path: &Path) -> Result<Vec<f32>, Error> {
    let samples = read_channel(path)?;
    if let Some(at) = samples.iter().position(|x| !x.is_finite()) {
        return Err(Error::new(
            path,
            format!("sample {at} is not a finite number"),
        ));
    }
    if samples.iter().all(|&x| x == 0.0) {
        return Err(Error::new(path, "channel 1 is exact zeros"));
    }
    Ok(samples)
}

fn read_channel(path: &Path) -> Result<Vec<f32>, Error> {
    let fail = |error: hound::Error| Error::new(path, error);
    let mut reader = WavReader::open(path).map_err(fail)?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE {
        return Err(Error::new(
            path,
            format!("{} Hz, but only 48000 Hz is supported", spec.sample_rate),
        ));
    }
    let channels = spec.channels as usize;
    match spec.sample_format {
        SampleFormat::Float => reader
            .samples::<f32>()
            .step_by(channels)
            .collect::<Result<_, _>>()
            .map_err(fail),
        SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .step_by(channels)
                .map(|sample| sample.map(|value| value as f32 * scale))
                .collect::<Result<_, _>>()
                .map_err(fail)
        }
    }
}

/// 24-bit stereo with the mono signal on both channels, like the virtual mic.
pub(crate) fn write(path: &Path, mono: &[f32]) -> Result<(), Error> {
    let fail = |error: hound::Error| Error::new(path, error);
    let spec = WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 24,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(path, spec).map_err(fail)?;
    const FULL_SCALE: f32 = 8_388_607.0;
    for x in mono {
        let sample = (x * FULL_SCALE).round().clamp(-FULL_SCALE, FULL_SCALE) as i32;
        writer.write_sample(sample).map_err(fail)?;
        writer.write_sample(sample).map_err(fail)?;
    }
    writer.finalize().map_err(fail)
}
