//! Saving a canceller's state and loading it into another, on synthetic signals.

mod common;

use aec3::{Config, EchoCanceller, StateError, FRAME, SAMPLE_RATE};
use common::*;

const SECOND: usize = SAMPLE_RATE as usize;
const DELAY: usize = 480;

/// The settings the chain runs with, next to WebRTC's.
const LONG: Config = Config {
    refined_blocks: 40,
    voiced_nearend_enr_threshold: 2.0,
    ..Config::DEFAULT
};

/// A far end, and a mic that hears its echo through `path` over faint noise.
fn call(seconds: usize, seed: u32, path: &[(usize, f32)]) -> (Vec<f32>, Vec<f32>) {
    let samples = seconds * SECOND;
    let reference = speech_like(samples, seed, 0.2, 0);
    let mic = mix(
        &echo(&reference, path),
        &floor_noise(samples, seed + 100, 0.0005),
    );
    (reference, mic)
}

/// Runs whole frames and returns the output, as late as the canceller makes it.
fn run(canceller: &mut EchoCanceller, reference: &[f32], mic: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(mic.len());
    for i in 0..mic.len() / FRAME {
        canceller.render(&frame(reference, i));
        let mut m = frame(mic, i);
        canceller.capture(&mut m);
        out.extend_from_slice(&m);
    }
    out
}

fn trained(config: Config, seconds: usize) -> EchoCanceller {
    let (reference, mic) = call(seconds, 1, &echo_path(DELAY));
    let mut canceller = EchoCanceller::new(DELAY, config);
    run(&mut canceller, &reference, &mic);
    assert!(canceller.is_trained());
    canceller
}

fn bits(signal: &[f32]) -> Vec<u32> {
    signal.iter().map(|x| x.to_bits()).collect()
}

/// The state holds everything but the signal histories. So a canceller whose signal histories
/// are empty, and a new one loaded with its state, are the same canceller: from the same
/// frames they make the same output and end in the same state, bit for bit.
///
/// The histories of what goes in are empty after a second of digital silence on both sides,
/// which is the warm-up before the save. The saved canceller must also have run an even
/// number of frames, because frames are cut into blocks in a pattern two frames long. The
/// histories of what comes out still hold the last comfort noise then, and take two frames to
/// leave: those are the warm-up after the load.
#[test]
fn a_loaded_canceller_carries_on_bit_for_bit() {
    for config in [Config::default(), LONG] {
        let path = echo_path(DELAY);
        // 10 s of far end with a little double talk at the end, then the far end stops. Its
        // echo rings out, and the mic goes to digital silence with it.
        let (mut reference, _) = call(12, 1, &path);
        reference[10 * SECOND..].fill(0.0);
        let mut near = speech_like(12 * SECOND, 5, 0.1, 9_600);
        near[..8 * SECOND].fill(0.0);
        let mut mic = mix(
            &mix(&echo(&reference, &path), &near),
            &floor_noise(12 * SECOND, 9, 0.0005),
        );
        mic[10 * SECOND..].copy_from_slice(&echo(&reference, &path)[10 * SECOND..]);
        assert!(mic[10 * SECOND + 4 * FRAME..].iter().all(|x| *x == 0.0));

        let mut saved = EchoCanceller::new(DELAY, config);
        run(&mut saved, &reference, &mic);
        assert_eq!((mic.len() / FRAME) % 2, 0);
        assert!(saved.metrics().converged, "{:?}", saved.metrics());
        let state = saved.save();

        let mut loaded = EchoCanceller::new(DELAY, config);
        loaded.load(&state).unwrap();
        assert!(loaded.is_trained());
        assert_eq!(loaded.save(), state, "a loaded state saves as itself");

        // A second call: far end alone, then double talk, then the mic alone, with another
        // echo path half way through so that the filters have to move.
        let (far, _) = call(8, 21, &path);
        let moved = echo(&far, &echo_path(DELAY + 96));
        let mut echoes = echo(&far, &path);
        echoes[4 * SECOND..].copy_from_slice(&moved[4 * SECOND..]);
        let mut talk = speech_like(8 * SECOND, 22, 0.2, 4_800);
        talk[..2 * SECOND].fill(0.0);
        let mut far = far;
        far[6 * SECOND..].fill(0.0);
        echoes[6 * SECOND + 2 * FRAME..].fill(0.0);
        let mic = mix(&mix(&echoes, &talk), &floor_noise(8 * SECOND, 23, 0.0005));

        let (a, b) = (run(&mut saved, &far, &mic), run(&mut loaded, &far, &mic));
        // The first two frames differ only by the comfort noise the saved canceller still
        // had in its overlap.
        let start: f32 = a[..2 * FRAME]
            .iter()
            .zip(&b[..2 * FRAME])
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max);
        assert!(start < 1e-3, "the outputs start {start} apart");
        assert!(
            bits(&a[2 * FRAME..]) == bits(&b[2 * FRAME..]),
            "the outputs differ after the warm-up"
        );
        assert!(a[SECOND..].iter().any(|x| x.abs() > 0.01));
        assert_eq!(saved.save(), loaded.save(), "the states differ at the end");
    }
}

/// Loading replaces everything: a canceller that has run on another room and a new one are
/// the same canceller once both have loaded the same state, from the first frame.
#[test]
fn loading_into_a_used_canceller_is_loading_into_a_new_one() {
    let state = trained(LONG, 10).save();

    let mut used = EchoCanceller::new(2400, LONG);
    let (reference, mic) = call(7, 31, &echo_path(2400));
    run(&mut used, &reference, &mic[..7 * SECOND - FRAME]);
    used.set_voice(true);
    used.load(&state).unwrap();
    let mut new = EchoCanceller::new(DELAY, LONG);
    new.load(&state).unwrap();

    let (reference, mic) = call(4, 41, &echo_path(DELAY));
    let (a, b) = (
        run(&mut used, &reference, &mic),
        run(&mut new, &reference, &mic),
    );
    assert!(bits(&a) == bits(&b));
    assert_eq!(used.save(), new.save());
}

/// What the state is for: the echo is gone from the first frame, where a new canceller lets
/// it through while it learns.
#[test]
fn a_loaded_canceller_cancels_from_its_first_frame() {
    let state = trained(LONG, 12).save();
    let mut loaded = EchoCanceller::new(DELAY, LONG);
    loaded.load(&state).unwrap();
    let metrics = loaded.metrics();
    assert!(
        metrics.converged && metrics.erle_db > 10.0,
        "{metrics:?} before any frame"
    );

    let mut new = EchoCanceller::new(DELAY, LONG);
    assert!(!new.is_trained());
    let (reference, mic) = call(3, 51, &echo_path(DELAY));
    let (with, without) = (
        run(&mut loaded, &reference, &mic),
        run(&mut new, &reference, &mic),
    );
    // The first burst of the far end, 0.6 s long.
    let burst = FRAME..SECOND / 2;
    let (echo_db, with_db, without_db) = (
        rms_db(&mic[burst.clone()]),
        rms_db(&with[burst.clone()]),
        rms_db(&without[burst]),
    );
    assert!(echo_db > -35.0, "{echo_db}");
    assert!(
        with_db < echo_db - 30.0,
        "echo {echo_db} dBFS, loaded {with_db} dBFS"
    );
    assert!(
        with_db < without_db - 10.0,
        "loaded {with_db} dBFS, new {without_db} dBFS"
    );
}

/// The state comes with the delay it was learned at, so that the filters stay where they are.
/// Giving the same delay again changes nothing, and another one restarts the filters as it
/// does in any canceller.
#[test]
fn a_loaded_state_brings_its_delay() {
    let state = trained(LONG, 10).save();
    let (reference, mic) = call(1, 61, &echo_path(DELAY));

    // Built for another delay, and never told the state's.
    let mut loaded = EchoCanceller::new(4800, LONG);
    loaded.load(&state).unwrap();
    run(&mut loaded, &reference, &mic);
    assert!(loaded.is_trained() && loaded.metrics().converged);
    assert_eq!(loaded.metrics().echo_delay, DELAY - 96);

    // 600 samples are within the same 4 ms block of the alignment.
    loaded.set_delay(600);
    run(&mut loaded, &reference, &mic);
    assert!(loaded.is_trained() && loaded.metrics().converged);

    loaded.set_delay(4800);
    run(&mut loaded, &reference[..FRAME], &mic[..FRAME]);
    assert!(!loaded.is_trained());
    assert!(!loaded.metrics().converged);
    // The alignment is in whole blocks of 192 samples, less the headroom.
    assert_eq!(loaded.metrics().echo_delay, 24 * 192);
}

/// A state that cannot be used is refused, and the canceller goes on as if nothing had been
/// tried: next to a twin that was never offered the states, it makes the same output.
#[test]
fn a_bad_state_is_refused_and_leaves_the_canceller_alone() {
    let state = trained(LONG, 10).save();
    let (reference, mic) = call(8, 71, &echo_path(960));
    let half = 4 * SECOND;

    let mut canceller = EchoCanceller::new(960, LONG);
    let mut twin = EchoCanceller::new(960, LONG);
    let mut out = run(&mut canceller, &reference[..half], &mic[..half]);
    let mut expected = run(&mut twin, &reference[..half], &mic[..half]);
    let before = canceller.save();

    // Cut short anywhere, the header included.
    for cut in [
        0,
        5,
        8,
        20,
        27,
        28,
        36,
        1000,
        state.len() / 2,
        state.len() - 1,
    ] {
        let error = canceller.load(&state[..cut]).unwrap_err();
        let expected = if cut < 8 {
            StateError::NotAState
        } else {
            StateError::Truncated
        };
        assert_eq!(error, expected, "cut at {cut}");
    }
    // Damaged: one flipped bit in the header, in a filter and in the checksum.
    for at in [16, 300, state.len() / 2, state.len() - 1] {
        let mut damaged = state.clone();
        damaged[at] ^= 1;
        assert_eq!(canceller.load(&damaged), Err(StateError::Corrupt), "{at}");
    }
    let mut longer = state.clone();
    longer.extend_from_slice(&[0; 4]);
    assert_eq!(canceller.load(&longer), Err(StateError::Corrupt));
    // Not a state, and one from another version of the format.
    assert_eq!(
        canceller.load(b"RIFF\0\0\0\0WAVEfmt "),
        Err(StateError::NotAState)
    );
    let mut newer = state.clone();
    newer[8..12].copy_from_slice(&7u32.to_le_bytes());
    assert_eq!(canceller.load(&newer), Err(StateError::Version(7)));
    // Saved with other filter lengths: WebRTC's 13 blocks against the chain's 40.
    let short = trained(Config::default(), 10).save();
    assert_eq!(canceller.load(&short), Err(StateError::Config));
    assert!(short.len() < state.len());
    let mut other = EchoCanceller::new(DELAY, Config::default());
    assert_eq!(other.load(&state), Err(StateError::Config));
    other.load(&short).unwrap();

    assert_eq!(
        canceller.save(),
        before,
        "a refused state changed the canceller"
    );
    out.extend(run(&mut canceller, &reference[half..], &mic[half..]));
    expected.extend(run(&mut twin, &reference[half..], &mic[half..]));
    assert!(bits(&out) == bits(&expected));
}

/// A canceller in AEC3's initial phase has nothing to hand on, and a loaded canceller must not
/// be in that phase. So its state is refused: a new canceller's, one that heard too little far
/// end, and one whose delay just changed.
#[test]
fn a_state_from_the_initial_phase_is_refused() {
    let mut target = trained(LONG, 10);
    let before = target.save();

    let mut new = EchoCanceller::new(DELAY, LONG);
    assert!(!new.is_trained());
    assert_eq!(target.load(&new.save()), Err(StateError::Untrained));

    // The far end plays for 0.6 s of each second, and the initial phase lasts for 2.5 s of
    // far end.
    let (reference, mic) = call(6, 81, &echo_path(DELAY));
    run(&mut new, &reference[..3 * SECOND], &mic[..3 * SECOND]);
    assert!(!new.is_trained());
    assert_eq!(target.load(&new.save()), Err(StateError::Untrained));
    run(&mut new, &reference[3 * SECOND..], &mic[3 * SECOND..]);
    assert!(new.is_trained());
    let learned = new.save();

    new.set_delay(4800);
    run(&mut new, &reference[..FRAME], &mic[..FRAME]);
    assert!(!new.is_trained());
    assert_eq!(target.load(&new.save()), Err(StateError::Untrained));

    assert_eq!(target.save(), before);
    target.load(&learned).unwrap();
    assert_eq!(target.save(), learned);
}

#[test]
fn the_state_is_small() {
    // Two filters of 40 and 13 blocks, the impulse response and its high-passed copy, and
    // some forty spectra.
    let state = trained(LONG, 10).save();
    assert!((40_000..70_000).contains(&state.len()), "{}", state.len());
}
