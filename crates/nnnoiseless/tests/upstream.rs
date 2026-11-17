//! The vendored copy must match upstream nnnoiseless on the same input.

fn signal(n: usize) -> Vec<f32> {
    // Voiced-ish tone bursts plus deterministic noise, at i16 scale.
    let mut seed = 0x1234_5678u32;
    (0..n)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            let t = i as f32 / 48_000.0;
            let burst = if (i / 24_000) % 2 == 0 { 1.0 } else { 0.0 };
            let tone = (2.0 * std::f32::consts::PI * 180.0 * t).sin()
                + 0.5 * (2.0 * std::f32::consts::PI * 360.0 * t).sin();
            (burst * 6000.0 * tone) + 800.0 * noise
        })
        .collect()
}

#[test]
fn matches_upstream() {
    let input = signal(48_000 * 3);
    let mut ours = nnnoiseless::DenoiseState::new();
    let mut theirs = upstream::DenoiseState::new();
    let (mut a, mut b) = ([0f32; 480], [0f32; 480]);
    let (mut err, mut energy) = (0f64, 0f64);
    for frame in input.chunks_exact(480) {
        let va = ours.process_frame(&mut a, frame);
        let vb = theirs.process_frame(&mut b, frame);
        assert!((va - vb).abs() < 1e-3, "vad {} vs {}", va, vb);
        for (x, y) in a.iter().zip(&b) {
            err += ((x - y) as f64).powi(2);
            energy += (*y as f64).powi(2);
        }
    }
    assert!(energy > 0.0);
    assert!(err / energy < 1e-8, "relative error {}", err / energy);
}
