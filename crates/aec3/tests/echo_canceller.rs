//! The canceller end to end, on synthetic signals.

mod common;

use aec3::{Config, EchoCanceller, Metrics, FRAME, LATENCY, MAX_DELAY, SAMPLE_RATE};
use common::*;

const SECOND: usize = SAMPLE_RATE as usize;

/// Runs the canceller and returns its output with the latency removed, so that it lines up
/// with the input.
fn run(canceller: &mut EchoCanceller, reference: &[f32], mic: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(mic.len());
    for i in 0..mic.len() / FRAME {
        canceller.render(&frame(reference, i));
        let mut m = frame(mic, i);
        canceller.capture(&mut m);
        out.extend_from_slice(&m);
    }
    out.drain(..LATENCY);
    out.resize(mic.len(), 0.0);
    out
}

#[test]
fn the_latency_is_pinned() {
    // With a silent reference the microphone signal passes through, late by LATENCY.
    let mic = floor_noise(2 * SECOND, 3, 0.05);
    let reference = vec![0.0; mic.len()];
    let mut canceller = EchoCanceller::new(480, Config::default());
    let mut out = Vec::new();
    for i in 0..mic.len() / FRAME {
        canceller.render(&frame(&reference, i));
        let mut m = frame(&mic, i);
        canceller.capture(&mut m);
        out.extend_from_slice(&m);
    }

    let correlation = |lag: usize| -> f64 {
        (SECOND / 2..SECOND)
            .map(|n| mic[n] as f64 * out[n + lag] as f64)
            .sum()
    };
    let best = (0..1000)
        .max_by(|&a, &b| correlation(a).total_cmp(&correlation(b)))
        .unwrap();
    assert_eq!(best, LATENCY);
    assert_eq!(LATENCY, 334);

    // The band split costs white noise a little level at the band edges and nothing else.
    let change = rms_db(&out[SECOND..]) - rms_db(&mic[SECOND..]);
    assert!(change.abs() < 1.0, "{change} dB");
}

#[test]
fn a_synthetic_echo_is_removed() {
    let samples = 20 * SECOND;
    let reference = speech_like(samples, 1, 0.2, 0);
    let delay = 480;
    let echo_only = echo(&reference, &echo_path(delay));
    let mic = mix(&echo_only, &floor_noise(samples, 9, 0.0005));

    let mut canceller = EchoCanceller::new(delay, Config::default());
    let out = run(&mut canceller, &reference, &mic);

    let tail = 10 * SECOND..samples;
    let (echo_db, out_db) = (rms_db(&mic[tail.clone()]), rms_db(&out[tail]));
    assert!(echo_db > -35.0, "{echo_db}");
    assert!(
        out_db < echo_db - 30.0,
        "echo {echo_db} dBFS, output {out_db} dBFS"
    );

    let metrics = canceller.metrics();
    assert!(metrics.converged);
    assert!(metrics.erle_db > 20.0, "{metrics:?}");
    assert!(metrics.erl_db > 5.0, "{metrics:?}");
    // The direct path is 32 samples (at 16 kHz) into the filter.
    assert_eq!(metrics.filter_delay_blocks, 0);
}

#[test]
fn the_near_end_passes_when_the_far_end_is_silent() {
    let samples = 10 * SECOND;
    let near = speech_like(samples, 5, 0.1, 0);
    let mic = mix(&near, &floor_noise(samples, 9, 0.0005));
    let reference = vec![0.0; samples];
    let mut canceller = EchoCanceller::new(480, Config::default());
    let out = run(&mut canceller, &reference, &mic);
    let change = rms_db(&out[SECOND..]) - rms_db(&mic[SECOND..]);
    assert!(change.abs() < 0.5, "{change} dB");
    let metrics = canceller.metrics();
    assert!(!metrics.converged && !metrics.render_active, "{metrics:?}");
}

#[test]
fn the_near_end_survives_double_talk_over_a_weak_echo() {
    // A leak like open headphones: the echo is 30 dB below the near end.
    let samples = 30 * SECOND;
    let reference = speech_like(samples, 1, 0.2, 0);
    let delay = 480;
    let path: Vec<(usize, f32)> = echo_path(delay)
        .into_iter()
        .map(|(d, g)| (d, 0.1 * g))
        .collect();
    let echo_only = echo(&reference, &path);
    // The near end talks from 12 s on, in bursts that overlap the far end's.
    let mut near = speech_like(samples, 77, 0.2, 14_400);
    near[..12 * SECOND].fill(0.0);
    let noise = floor_noise(samples, 9, 0.0005);
    let mic = mix(&mix(&echo_only, &near), &noise);

    let mut canceller = EchoCanceller::new(delay, Config::default());
    let out = run(&mut canceller, &reference, &mic);

    // Far end only: the echo goes down to the noise.
    let far_only = 6 * SECOND..12 * SECOND;
    let echo_db = rms_db(&echo_only[far_only.clone()]);
    let residual_db = rms_db(&out[far_only]);
    assert!(
        residual_db < echo_db - 15.0,
        "echo {echo_db} dBFS, output {residual_db} dBFS"
    );

    // Double talk: the near end keeps its level.
    let double_talk = 14 * SECOND..samples;
    let change = rms_db(&out[double_talk.clone()]) - rms_db(&near[double_talk]);
    assert!(change.abs() < 1.5, "{change} dB");
}

#[test]
fn a_delay_change_is_followed() {
    let samples = 30 * SECOND;
    let reference = speech_like(samples, 1, 0.2, 0);
    let (first_delay, second_delay) = (480, 4800);
    // The echo path jumps from 10 ms to 100 ms at 10 s, out of the filter's reach.
    let half = 10 * SECOND;
    let mut echo_only = echo(&reference, &echo_path(first_delay));
    let later = echo(&reference, &echo_path(second_delay));
    echo_only[half..].copy_from_slice(&later[half..]);
    let mic = mix(&echo_only, &floor_noise(samples, 9, 0.0005));

    let measure = |tell_the_canceller: bool| -> (f32, Metrics) {
        let mut canceller = EchoCanceller::new(first_delay, Config::default());
        let mut out = Vec::with_capacity(samples);
        for i in 0..samples / FRAME {
            if tell_the_canceller && i * FRAME == half {
                canceller.set_delay(second_delay);
            }
            canceller.render(&frame(&reference, i));
            let mut m = frame(&mic, i);
            canceller.capture(&mut m);
            out.extend_from_slice(&m);
        }
        let tail = 20 * SECOND..samples;
        (
            rms_db(&mic[tail.clone()]) - rms_db(&out[tail]),
            canceller.metrics(),
        )
    };

    let (followed, metrics) = measure(true);
    assert!(followed > 30.0, "{followed} dB");
    assert!(metrics.converged && metrics.erle_db > 20.0, "{metrics:?}");
    // 4800 samples are 1600 at 16 kHz: 24 whole blocks after the headroom, which leaves the
    // direct path 64 samples into the filter.
    assert_eq!(metrics.filter_delay_blocks, 1);

    // Without the new delay the echo is out of the filter's reach, and of the reverb model's:
    // it passes. (The ERLE estimate holds its last value when the filter stops converging.)
    let (stale, _) = measure(false);
    assert!(stale < 3.0, "{stale} dB");
}

#[test]
fn an_underestimated_delay_still_cancels() {
    // The filter starts up to 6 ms early and spans 52 ms, so a delay given 10 ms short
    // leaves the direct path well inside it.
    let samples = 20 * SECOND;
    let reference = speech_like(samples, 1, 0.2, 0);
    let echo_only = echo(&reference, &echo_path(960));
    let mic = mix(&echo_only, &floor_noise(samples, 9, 0.0005));
    let mut canceller = EchoCanceller::new(480, Config::default());
    let out = run(&mut canceller, &reference, &mic);
    let tail = 10 * SECOND..samples;
    let reduction = rms_db(&mic[tail.clone()]) - rms_db(&out[tail]);
    assert!(reduction > 30.0, "{reduction} dB");
    // 960 - 480 samples at 48 kHz is 160 at 16 kHz, plus the 32 of headroom: block 3.
    assert_eq!(canceller.metrics().filter_delay_blocks, 3);
}

#[test]
fn longer_filters_can_be_configured() {
    let samples = 20 * SECOND;
    let reference = speech_like(samples, 1, 0.2, 0);
    // A reflection 80 ms after the direct path: beyond 13 blocks, inside 32.
    let mut path = echo_path(480);
    path.push((480 + 3840, 0.2));
    let echo_only = echo(&reference, &path);
    let mic = mix(&echo_only, &floor_noise(samples, 9, 0.0005));

    let erle = |blocks: usize| -> f32 {
        let config = Config {
            refined_blocks: blocks,
            coarse_blocks: blocks,
            ..Config::default()
        };
        let mut canceller = EchoCanceller::new(480, config);
        run(&mut canceller, &reference, &mic);
        canceller.metrics().erle_db
    };
    let (short, long) = (erle(13), erle(32));
    assert!(short < 12.0, "{short} dB");
    assert!(long > 20.0, "{long} dB");
}

#[test]
fn saturated_and_extreme_input_stays_finite() {
    let samples = 4 * SECOND;
    let mut noise = Noise(4);
    // Full-scale square bursts on both sides, then silence, then a delay at the limit.
    let reference: Vec<f32> = (0..samples)
        .map(|n| {
            if (n / 4800) % 2 == 0 {
                noise.next().signum()
            } else {
                0.0
            }
        })
        .collect();
    let mic: Vec<f32> = echo(&reference, &[(480, 1.5)])
        .iter()
        .map(|v| v.clamp(-1.0, 1.0))
        .collect();

    let mut canceller = EchoCanceller::new(480, Config::default());
    for i in 0..samples / FRAME {
        if i == 200 {
            canceller.set_delay(MAX_DELAY + 1000);
        }
        if i == 300 {
            canceller.set_delay(0);
        }
        canceller.render(&frame(&reference, i));
        let mut m = frame(&mic, i);
        canceller.capture(&mut m);
        assert!(
            m.iter().all(|v| v.is_finite() && v.abs() < 2.0),
            "frame {i}"
        );
    }
    let metrics = canceller.metrics();
    assert!(
        metrics.erle_db.is_finite() && metrics.erl_db.is_finite(),
        "{metrics:?}"
    );
}

#[test]
fn capture_without_render_counts_the_reference_as_silent() {
    let samples = 2 * SECOND;
    let mic = floor_noise(samples, 3, 0.05);
    let mut with_silence = EchoCanceller::new(480, Config::default());
    let mut without_render = EchoCanceller::new(480, Config::default());
    for i in 0..samples / FRAME {
        with_silence.render(&[0.0; FRAME]);
        let mut a = frame(&mic, i);
        with_silence.capture(&mut a);
        let mut b = frame(&mic, i);
        without_render.capture(&mut b);
        assert_eq!(a, b, "frame {i}");
    }
}
