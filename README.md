# deno_audio

A [Web Audio API](https://webaudio.github.io/web-audio-api/) extension for
[`deno_core`](https://crates.io/crates/deno_core), in the style of Deno's
standard Web Platform extensions, delegating DSP and rendering to
the [Smudgy `web-audio-api-rs` fork](https://github.com/smudgy-mud/web-audio-api-rs).

**Status: Phase 0 compatibility spike.** The crate currently provides only an
empty extension and a runtime compatibility test. It does not install Web Audio
globals and is not ready for application use.

The research baseline is deliberately locked to one compatibility set:

- `deno_core` 0.410.0
- `deno_webidl` 0.257.0
- `deno_web` 0.288.0
- `deno_error` 0.7.1
- `serde_v8` 0.319.0
- the Smudgy `web-audio-api` fork at commit
  `2af21af6ff2bbee17201b2dd2a5ebc56f55c3511` (currently the 1.7.0 baseline)

The Deno crates move as a coordinated tuple. Upgrade them together in one
reviewed change, and keep `Cargo.lock` committed so Phase 0 evidence remains
reproducible. The locked Phase 0 graph requires Rust 1.91 or newer.

The audio fork is intentionally a pinned Git dependency and has no separate
crates.io identity. Consequently, functional `deno_audio` versions are consumed
from this repository at an immutable tag or revision rather than from crates.io.

The baseline engine compiles its complete Symphonia decoder set unconditionally:
AAC, ADPCM, ALAC, FLAC, MPEG layers I/II/III, PCM, and Vorbis, with AIFF, CAF,
ISO MP4/M4A, Matroska/WebM, Ogg, and WAV containers. These are native dependency
capabilities, not a claim that the current empty extension exposes
`decodeAudioData`; supported JavaScript behavior will be documented as it lands.

## Development

Run the baseline checks with:

```console
cargo fmt --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo test --all-targets --no-default-features --locked
cargo test --doc --locked
cargo clippy --all-targets --locked -- -D warnings
cargo deny check
```

The default feature selects CPAL. An offline/no-default-features configuration
is still being evaluated and is not yet a supported configuration.

Phase 0 evidence is recorded in [`research/upstream-baseline.md`](research/upstream-baseline.md)
and [`research/existing-deno-audio-packages.md`](research/existing-deno-audio-packages.md).

`deno_audio` is an independent project. It is not affiliated with, endorsed
by, or maintained by Deno Land Inc. "Deno" is a trademark of Deno Land Inc.

## License

MIT. See `THIRD_PARTY_NOTICES.md` for direct-dependency provenance.
