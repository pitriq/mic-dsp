//! The audio callback must never allocate. Every allocation in this test binary goes
//! through `AllocDisabler`, which aborts the process if one happens inside
//! `assert_no_alloc`.

use assert_no_alloc::{assert_no_alloc, AllocDisabler};
use dsp::{Chain, Config, EchoTrial};

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

/// A far end that plays for a while, stops long enough for the echo canceller to go idle,
/// and plays again. Its two channels differ in level, so the chain has to mix them.
fn far_end(frames: usize) -> Vec<f32> {
    signal(frames)
        .chunks(2)
        .enumerate()
        .flat_map(|(i, frame)| {
            let playing = !(60_000..120_000).contains(&i);
            let sample = if playing { 4.0 * frame[0] } else { 0.0 };
            [sample, 0.5 * sample]
        })
        .collect()
}

/// Runs callbacks of several sizes on a thread the chain has never seen, with the far end's
/// echo in the mic, then without a reference.
fn callbacks_on_a_new_thread(mut chain: Chain) -> Chain {
    std::thread::spawn(move || {
        let far = far_end(150_000);
        let mut input = signal(150_000);
        for (i, x) in input.iter_mut().enumerate() {
            *x += 0.3 * i.checked_sub(2 * 480).map_or(0.0, |j| far[j]);
        }
        let mut output = vec![0.0f32; input.len()];
        let mut at = 0;
        for frames in [480, 512, 64, 1024, 0, 7, 2000].into_iter().cycle() {
            if at + 2 * frames > input.len() {
                break;
            }
            let (x, r, y) = (
                &input[at..at + 2 * frames],
                &far[at..at + 2 * frames],
                &mut output[at..at + 2 * frames],
            );
            if at < 2 * 130_000 {
                assert_no_alloc(|| chain.process_with_reference(x, 2, r, 2, y, 2));
            } else {
                assert_no_alloc(|| chain.process(x, 2, y, 2));
            }
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
    assert_eq!(control.status().gain_db, 4.5);

    // A gain change while callbacks run on the audio thread.
    let worker = std::thread::spawn(move || callbacks_on_a_new_thread(chain));
    for db in [-6.0, 18.0, 0.0] {
        control.set_config(&Config::from_toml(&format!("[gain]\ndb = {db:?}")).unwrap());
        std::thread::yield_now();
    }
    drop(worker.join().unwrap());
}

/// A far end of noise in bursts, which a canceller can learn from, and the mic that hears it
/// 10 ms later at `gain` over faint noise. Both interleaved stereo.
fn call(frames: usize, gain: f32) -> (Vec<f32>, Vec<f32>) {
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1 << 23) as f32 - 1.0
    };
    let mut low = 0.0f32;
    let far: Vec<f32> = (0..frames)
        .map(|i| {
            low += 0.3 * (noise() - low);
            if (i / 28_800) % 5 < 4 {
                0.5 * low
            } else {
                0.0
            }
        })
        .collect();
    let mic = (0..frames).flat_map(|i| {
        let x = 0.0005 * noise() + i.checked_sub(480).map_or(0.0, |j| gain * far[j]);
        [x, x]
    });
    let mic = mic.collect();
    (far.iter().flat_map(|x| [*x, *x]).collect(), mic)
}

/// Callbacks of 512 frames on a thread of their own, inside `assert_no_alloc`.
fn session(mut chain: Chain, far: Vec<f32>, mic: Vec<f32>) -> Chain {
    std::thread::spawn(move || {
        let mut output = [0.0f32; 2 * 512];
        for (x, r) in mic.chunks(2 * 512).zip(far.chunks(2 * 512)) {
            let y = &mut output[..x.len()];
            assert_no_alloc(|| chain.process_with_reference(x, 2, r, 2, y, 2));
        }
        chain
    })
    .join()
    .unwrap()
}

/// What a chain's canceller has learned from 8 s of an echo at `gain`.
fn learned(gain: f32) -> Vec<u8> {
    let (chain, _control) = dsp::chain(&Config::default());
    let (far, mic) = call(8 * 48_000, gain);
    // Saving allocates, on the thread that asks for it.
    session(chain, far, mic)
        .echo_state()
        .expect("8 s of far end train the canceller")
}

/// A session that starts with two saved states runs both cancellers, decides and crosses
/// over to the winner, all on memory the chain had from the start or that the control thread
/// loaded into it.
#[test]
fn a_session_that_tries_two_echo_states_never_allocates() {
    let (strong, weak) = (learned(0.3), learned(0.003));
    let (mut chain, mut control) = dsp::chain(&Config::default());
    chain.load_echo_preset(&weak).unwrap();
    chain.start_echo_session(Some(&strong)).unwrap();
    assert_eq!(control.status().echo.trial, EchoTrial::Running);

    let (far, mic) = call(4 * 48_000, 0.3);
    let mut chain = session(chain, far.clone(), mic.clone());
    let echo = control.status().echo;
    assert_eq!(echo.trial, EchoTrial::Switched, "{echo:?}");
    assert!(echo.far_frames > 100);

    // The next session on the same chain tries the state that lost against the one in use.
    chain.start_echo_session(Some(&weak)).unwrap();
    let chain = session(chain, far, mic);
    let echo = control.status().echo;
    assert_eq!(echo.trial, EchoTrial::Kept, "{echo:?}");
    assert!(chain.echo_state().is_some());
}
