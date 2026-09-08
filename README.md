# deno_audio

A [Web Audio API](https://www.w3.org/TR/webaudio/) extension for
[`deno_core`](https://crates.io/crates/deno_core), written in the style of
Deno's standard Web Platform extensions. Install it into a `JsRuntime` and the
JavaScript inside gets `AudioContext`, `OfflineAudioContext`, and a growing
subset of the Web Audio node graph. DSP and rendering are delegated to a
[pinned fork](https://github.com/smudgy-mud/web-audio-api-rs) of
[orottier/web-audio-api-rs](https://github.com/orottier/web-audio-api-rs).

`deno_audio` was built for [smudgy](https://github.com/smudgy-mud/smudgy), a
scriptable MUD client that runs untrusted package scripts in per-package V8
isolates, and its design follows from that embedding:

- **The host owns the speaker.** The embedder supplies an
  `AudioOutputFactory` — or accepts the built-in system or silent outputs —
  so many isolates' contexts can be mixed into one host-controlled stream,
  including a fully silent, format-preserving emulation when no audio device
  exists.
- **Everything is budgeted.** Nodes, connections, control commands, scheduled
  sources, decoded PCM, and V8-exposed channel copies are reserved against
  per-isolate and shared-host ceilings *before* native mutation; refusal
  surfaces as a standard `QuotaExceededError` instead of unbounded
  allocation.
- **Lifecycle is owned and acknowledged.** Suspend/resume/close,
  render-thread retirement, and whole runtime-generation drains are
  serialized, receipted operations, so an embedder can tear down and replace
  runtimes deterministically.

## Status: experimental, AI-authored

**Current release: `v0.1.0-alpha.1`.** This crate — and the engine fork
beneath it — was written primarily by AI coding agents working under human
direction and review. It carries substantial test coverage, including a
pinned [Web Platform Tests](https://github.com/web-platform-tests/wpt)
subset, but it has little real-world exposure outside smudgy, its API will
change, and no version is supported for production use yet. Review it before
trusting it with anything that matters.

The JavaScript surface today: `AudioContext`, `OfflineAudioContext`,
destination, `GainNode`, fixed and custom-wave `OscillatorNode`,
`PeriodicWave`, `ConstantSourceNode`, one-shot `AudioBufferSourceNode`,
`AudioBuffer`, scalar `AudioParam.value` (plus the standard automation
methods on exposed offline parameters), explicit connect/disconnect,
lifecycle methods with `statechange`, exactly-once `ended`, and bounded
`decodeAudioData` in both promise and legacy callback forms. Online parameter
automation, further node families, and full browser conformance are future
work. The precise guarantees behind each of these are recorded in
[`SEMANTICS.md`](SEMANTICS.md).

[`AudioContext.createFileSource(source)`](STREAMING.md) plays local audio files
incrementally through the existing graph, without assembling the file or decoded
sound in memory. File access requires an embedder-installed `AudioFileResolver`;
it is denied by default. The Rust `AudioFileStream` API supplies admitted workers
with cancellation-independent join ownership. Its workspace charge is an allowance,
not an allocator-enforced process memory ceiling.

## Usage

The extension installs after Deno's Web Platform base extensions:

```rust
use deno_core::{Extension, JsRuntime, RuntimeOptions};

fn extensions() -> Vec<Extension> {
    vec![
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::init(
            deno_web::BlobStore::default_arc(),
            None,
            Default::default(),
            deno_web::InMemoryBroadcastChannel::default(),
        ),
        deno_audio::deno_audio::init(deno_audio::AudioExtensionOptions::default()),
    ]
}

let mut runtime = JsRuntime::new(RuntimeOptions {
    extensions: extensions(),
    ..Default::default()
});
```

`AudioExtensionOptions` configures each instance: resource limits, an
`AudioPermissions` authority, an optional `AudioOutputFactory`, event-pump
liveness, and an optional lifecycle registrar for runtime-generation drains.
Sharing one `Arc<AudioHost>` across instances makes the aggregate ceilings
span isolates. The integration tests under [`tests/`](tests/) are the best
worked examples; see
[`SEMANTICS.md`](SEMANTICS.md) for the embedder contract in detail.

## Feature flags

- **default (`cpal`)** — built-in system output through CPAL
  (WASAPI/ALSA/CoreAudio); `cpal-jack`, `cpal-pipewire`, and `cpal-asio`
  select those CPAL backends.
- **`cubeb`** — the cubeb backend instead of CPAL.
- **no default features** — no audio backend at all: hosted silent output,
  offline rendering, decoding, and every lifecycle path still work, which is
  how embedders that inject their own output factory (and CI) consume the
  crate.
- **`diagnostics`** — the engine's diagnostic instrumentation.

## Distribution and versioning

The crates.io name `deno_audio` is reserved by an intentionally empty
`0.0.1`. Functional versions are currently consumed from this repository at
an immutable tag or revision, because the engine is a pinned git dependency
(crates.io does not allow those) and the fork has no separate crates.io
identity. The pin is an exact commit recorded in `Cargo.toml`; no moving
branch is an acceptable substitute. The compatibility tuple (Deno crates,
engine commit, Rust floor) is recorded in
[`SEMANTICS.md`](SEMANTICS.md) and moves as one reviewed change.

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

Web Platform Test coverage is pinned to WPT commit
`fbc1b90e28b38e919acabeeb14d1b074f2c125ec`. See
[`tests/wpt/README.md`](tests/wpt/README.md) for the sparse-checkout command
and [`tests/wpt/expectations.json`](tests/wpt/expectations.json) for every
runnable file and reviewed skip reason. CI supplies the checkout and runs the
subset through this extension's `JsRuntime`; no browser or system audio
device is required.

The buy-versus-build survey and upstream engine baseline that motivated this
project are preserved under [`research/`](research/).

## Trademark note

`deno_audio` is an independent project. It is not affiliated with, endorsed
by, or maintained by Deno Land Inc. "Deno" is a trademark of Deno Land Inc.

## License

MIT. See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) for
direct-dependency provenance.
