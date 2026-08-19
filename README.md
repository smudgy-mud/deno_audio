# deno_audio

A [Web Audio API](https://webaudio.github.io/web-audio-api/) extension for
[`deno_core`](https://crates.io/crates/deno_core), in the style of Deno's
standard Web Platform extensions, delegating DSP and rendering to
the [Smudgy `web-audio-api-rs` fork](https://github.com/smudgy-mud/web-audio-api-rs).

**Status: Phase 1 in progress.** The native engine, bounded ownership model,
and hosted silent/CPAL/Cubeb output paths have passed their Phase 0 acceptance
gates. The extension now installs its first public Web Audio global,
`AudioBuffer`. Contexts and nodes are not public yet, so this is a real but
deliberately incomplete JavaScript surface rather than an application-ready Web
Audio implementation.

The public `AudioBuffer` implements Web IDL conversion, nominal-range and brand
checks, standard prototype descriptors, `getChannelData()`,
`copyFromChannel()`, and `copyToChannel()`. Canonical PCM remains Rust-owned.
Each first channel exposure creates a separately budgeted V8 copy and returns
the same view until the native acquire algorithm detaches it. Copy methods use
canonical storage directly when a channel has never been exposed. Shared typed
arrays are rejected, directly structured-cloning a buffer raises `DataCloneError`,
and configured channel, logical-sample, sample-rate, per-isolate byte, and
shared-host byte ceilings reject before their represented allocation.

Extension JavaScript uses Deno's captured primordials. Installation is
idempotent for the same constructor and refuses to overwrite an unrelated
global. A startup-snapshot smoke test proves that immutable class bootstrap does
not capture an `AudioHost` or other runtime-native authority.

The remaining private proof objects and ops cover `CppGC`, owned asynchronous
work, offline rendering, shared `AudioHost` accounting, online lifecycle
ownership, render-death handling, scheduled-source reconciliation, and bounded
off-thread decoding. Exact oscillator graph nodes and their construction and
runtime commands are reserved against isolate and shared-host ceilings before
mutation and remain charged through typed rollback or physical retirement. A
bounded fake process mixer proves that independent hosted graphs and native
input can share one physical-output owner without coupling clocks or
retirement. These seams remain private until their corresponding Phase 1 or
Phase 2 JavaScript classes land.

The research baseline is deliberately locked to one compatibility set:

- `deno_core` 0.410.0
- `deno_webidl` 0.257.0
- `deno_web` 0.288.0
- `deno_error` 0.7.1
- `serde_v8` 0.319.0
- the Smudgy `web-audio-api` fork at commit
  `c7244534cbc28992b0b2c8c88a3e647061b7a215` (the reviewed hosted-output
  compatibility stack plus fixed scheduled-source capacity and opaque graph-node/control-command
  reservation lifetimes, including the permanent hosted bootstrap, based on 1.7.0)

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
`decodeAudioData`; supported JavaScript behavior is limited to the surface
listed above and will expand as each vertical slice passes its gate.

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
private Phase 0D proof surface uses this same ownership shape. Decode requests
reserve their encoded copy, concurrent job, result dimensions, and every
wrapper-owned PCM allocation before spawning or growing. Offline render
requests likewise reserve dimensions, output PCM, and a job slot before native
context construction. The finite per-isolate command default covers the 21-command permanent
graph bootstrap and every currently exposed user graph batch, including suspended staging.
Lifecycle barriers and mandatory teardown records remain governed by their separate bounded
upstream channels so cleanup never depends on user quota. These objects remain
available only through unsafe native exposure in tests and are not a supported
JavaScript API. `AudioBuffer` is the sole supported global in the current
slice; in particular, `AudioContext` and `OfflineAudioContext` remain absent.

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
