# deno_audio

A [Web Audio API](https://webaudio.github.io/web-audio-api/) extension for
[`deno_core`](https://crates.io/crates/deno_core), in the style of Deno's
standard Web Platform extensions, delegating DSP and rendering to
the [Smudgy `web-audio-api-rs` fork](https://github.com/smudgy-mud/web-audio-api-rs).

**Status: Phase 0 binding spike.** The native engine and both hosted-output
paths have passed the Phase 0B acceptance gate. The extension now contains
private proof objects and ops for `CppGC`, `AudioBuffer`, owned asynchronous
work, offline rendering, shared `AudioHost` accounting, online lifecycle
ownership, render-death handling, and scheduled-source reconciliation. It does
not install Web Audio globals and is not ready for application use. These
private seams exist to make the eventual JavaScript surface reviewable rather
than to provide an interim API.

The research baseline is deliberately locked to one compatibility set:

- `deno_core` 0.410.0
- `deno_webidl` 0.257.0
- `deno_web` 0.288.0
- `deno_error` 0.7.1
- `serde_v8` 0.319.0
- the Smudgy `web-audio-api` fork at commit
  `dd83ee6530136a71caff97da06269302f129cb09` (the reviewed hosted-output
  compatibility stack plus the fixed scheduled-source saturation capacity,
  based on 1.7.0)

The Deno crates move as a coordinated tuple. Upgrade them together in one
reviewed change, and keep `Cargo.lock` committed so Phase 0 evidence remains
reproducible. The locked Phase 0 graph requires Rust 1.91 or newer.

The audio fork is intentionally a pinned Git dependency and has no separate
crates.io identity. Consequently, functional `deno_audio` versions are consumed
from this repository at an immutable tag or revision rather than from crates.io.

The selected engine compiles its complete Symphonia decoder set unconditionally:
AAC, ADPCM, ALAC, FLAC, MPEG layers I/II/III, PCM, and Vorbis, with AIFF, CAF,
ISO MP4/M4A, Matroska/WebM, Ogg, and WAV containers. These are native dependency
capabilities, not a claim that the current extension exposes
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
cargo doc --no-deps --locked
cargo deny check
```

The default feature selects CPAL. The dependency graph, hosted silent output,
private Deno binding proofs, decode worker, and offline render/suspend path also
pass without default features. This is build-path evidence, not a claim that a
public JavaScript `OfflineAudioContext` exists yet.

Each extension instance is initialized with `AudioExtensionOptions`. Sharing
one `Arc<AudioHost>` across options makes the configured aggregate limits span
isolates; output and permission services remain separate authorities. The
private Phase 0D proof surface uses this same ownership shape, but is available
only through unsafe native exposure in tests and is not a supported JavaScript
API.

The engine revision is currently available only in the maintainer's local fork
history. A fresh external checkout cannot resolve the canonical Git dependency
until that exact reviewed revision is published; no moving branch is an
acceptable substitute.

Phase 0 evidence is recorded in [`research/upstream-baseline.md`](research/upstream-baseline.md)
and [`research/existing-deno-audio-packages.md`](research/existing-deno-audio-packages.md).

`deno_audio` is an independent project. It is not affiliated with, endorsed
by, or maintained by Deno Land Inc. "Deno" is a trademark of Deno Land Inc.

## License

MIT. See `THIRD_PARTY_NOTICES.md` for direct-dependency provenance.
