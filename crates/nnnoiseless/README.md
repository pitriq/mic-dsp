# nnnoiseless (vendored)

nnnoiseless 0.5.2 by Joe Neeman, from crates.io, under the BSD-3-Clause license in `COPYING`.

Upstream computes its FFTs through easyfft, which keeps plans and scratch in a thread-local cache. The first frame processed on any new thread allocates, which the audio callback can't allow. This copy changes `src/features.rs` so each `DenoiseFeatures` owns its realfft plans and scratch, and drops the C API, the dasp signal adapter, the training code and the CLI. `tests/upstream.rs` checks the output against the upstream crate.
