//! The canceller runs in the audio callback, which must never allocate. Every allocation in
//! this test binary goes through `AllocDisabler`, which aborts the process if one happens
//! inside `assert_no_alloc`.

mod common;

use aec3::{Config, EchoCanceller, FRAME, MAX_DELAY, SAMPLE_RATE};
use assert_no_alloc::{assert_no_alloc, AllocDisabler};
use common::*;

#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

const SECOND: usize = SAMPLE_RATE as usize;

/// Far end alone, double talk, silence on both sides, a delay change and saturation.
fn scenario(config: Config) {
    let samples = 16 * SECOND;
    let mut reference = speech_like(samples, 1, 0.2, 0);
    reference[8 * SECOND..10 * SECOND].fill(0.0);
    let mut echo_only = echo(&reference, &echo_path(480));
    let moved = echo(&reference, &echo_path(2400));
    echo_only[10 * SECOND..].copy_from_slice(&moved[10 * SECOND..]);
    let mut near = speech_like(samples, 77, 0.2, 14_400);
    near[..4 * SECOND].fill(0.0);
    near[8 * SECOND..10 * SECOND].fill(0.0);
    let mut mic = mix(&mix(&echo_only, &near), &floor_noise(samples, 9, 0.0005));
    mic[8 * SECOND..10 * SECOND].fill(0.0);
    // The last second clips.
    for v in &mut mic[15 * SECOND..] {
        *v = (*v * 40.0).clamp(-1.0, 1.0);
    }

    let mut canceller = EchoCanceller::new(480, config);
    let mut checksum = 0.0f64;
    let mut converged_at_some_point = false;
    assert_no_alloc(|| {
        for i in 0..samples / FRAME {
            if i * FRAME == 10 * SECOND {
                canceller.set_delay(2400);
            }
            if i * FRAME == 15 * SECOND + SECOND / 2 {
                canceller.set_delay(MAX_DELAY);
            }
            let r: &[f32; FRAME] = reference[i * FRAME..(i + 1) * FRAME].try_into().unwrap();
            let m: &mut [f32; FRAME] = (&mut mic[i * FRAME..(i + 1) * FRAME]).try_into().unwrap();
            canceller.render(r);
            canceller.capture(m);
            let metrics = canceller.metrics();
            converged_at_some_point |= metrics.converged && metrics.erle_db > 10.0;
            checksum += m.iter().map(|&v| v as f64).sum::<f64>();
        }
    });
    assert!(checksum.is_finite());
    // The scenario did exercise the filters and the suppressor, not just a passthrough.
    assert!(converged_at_some_point);
}

#[test]
fn processing_never_allocates() {
    scenario(Config::default());
}

#[test]
fn processing_never_allocates_with_long_filters() {
    scenario(Config {
        refined_blocks: 48,
        coarse_blocks: 32,
        ..Config::default()
    });
}

/// A canceller that took over a saved state runs on what `load` allocated.
#[test]
fn processing_never_allocates_after_a_load() {
    let config = Config {
        refined_blocks: 40,
        ..Config::default()
    };
    let samples = 8 * SECOND;
    let reference = speech_like(samples, 1, 0.2, 0);
    let mut mic = mix(
        &echo(&reference, &echo_path(480)),
        &floor_noise(samples, 9, 0.0005),
    );
    let mut canceller = EchoCanceller::new(480, config);
    let mut loaded = EchoCanceller::new(480, config);
    let mut frames = |canceller: &mut EchoCanceller, from: usize, to: usize| {
        assert_no_alloc(|| {
            for i in from..to {
                let r: &[f32; FRAME] = reference[i * FRAME..(i + 1) * FRAME].try_into().unwrap();
                let m: &mut [f32; FRAME] =
                    (&mut mic[i * FRAME..(i + 1) * FRAME]).try_into().unwrap();
                canceller.render(r);
                canceller.capture(m);
            }
        })
    };
    frames(&mut canceller, 0, 600);
    // Saving and loading allocate, on the thread that does them.
    let state = canceller.save();
    loaded.load(&state).unwrap();
    frames(&mut loaded, 600, 800);
    assert!(loaded.is_trained() && loaded.metrics().converged);
}
