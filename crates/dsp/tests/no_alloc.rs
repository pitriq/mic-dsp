//! The audio callback must never allocate. Every allocation in this test binary goes
//! through `AllocDisabler`, which aborts the process if one happens inside
//! `assert_no_alloc`.

use assert_no_alloc::{assert_no_alloc, AllocDisabler};
use dsp::{Chain, Config};

#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

/// Tone bursts over noise: enough for every stage, the denoiser's full path included.
fn signal(frames: usize) -> Vec<f32> {
    let mut seed = 0x2545_f491u32;
    (0..frames)
        .flat_map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
            let t = i as f32 / 48_000.0;
            let burst = if (i / 12_000) % 2 == 0 { 0.2 } else { 0.0 };
            let tone = (std::f32::consts::TAU * 150.0 * t).sin()
                + 0.5 * (std::f32::consts::TAU * 450.0 * t).sin();
            let sample = burst * tone + 0.002 * noise;
            [sample, sample]
        })
        .collect()
}

/// Runs callbacks of several sizes on a thread the chain has never seen.
fn callbacks_on_a_new_thread(mut chain: Chain) -> Chain {
    std::thread::spawn(move || {
        let input = signal(48_000);
        let mut output = vec![0.0f32; input.len()];
        let mut at = 0;
        for frames in [480, 512, 64, 1024, 0, 7, 2000].into_iter().cycle() {
            if at + 2 * frames > input.len() {
                break;
            }
            let (x, y) = (
                &input[at..at + 2 * frames],
                &mut output[at..at + 2 * frames],
            );
            assert_no_alloc(|| chain.process(x, 2, y, 2));
            at += 2 * frames;
        }
        assert!(output.iter().any(|x| x.abs() > 0.01));
        chain
    })
    .join()
    .unwrap()
}

#[test]
fn process_never_allocates() {
    let (chain, mut control) = dsp::chain(&Config::default());

    // The very first callback, on a thread other than the one that built the chain.
    let chain = callbacks_on_a_new_thread(chain);
    assert!(control.status().frames > 40_000);

    // Applying a new config, with a new gain and every optional stage switched on. The gain
    // ramps during the first callbacks.
    let config = Config::from_toml(
        "[target]\npeak_dbfs = -14.0\n[highpass]\nhz = 90.0\n[gain]\ndb = 12.0\n\
         [cut]\nenabled = true\n[compressor]\nratio = 3.0\n[deesser]\nenabled = true\n\
         [expander]\nrange_db = 15.0\n",
    )
    .unwrap();
    control.set_config(&config);
    let chain = callbacks_on_a_new_thread(chain);
    assert_eq!(control.status().gain_db, 12.0);

    // And switching them off again, the gain back down.
    control.set_config(&Config::default());
    let chain = callbacks_on_a_new_thread(chain);
    assert_eq!(control.status().gain_db, 2.0);

    // A gain change while callbacks run on the audio thread.
    let worker = std::thread::spawn(move || callbacks_on_a_new_thread(chain));
    for db in [-6.0, 18.0, 0.0] {
        control.set_config(&Config::from_toml(&format!("[gain]\ndb = {db:?}")).unwrap());
        std::thread::yield_now();
    }
    drop(worker.join().unwrap());
}
