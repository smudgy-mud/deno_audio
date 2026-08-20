# deno_audio

A [Web Audio API](https://webaudio.github.io/web-audio-api/) extension for
[`deno_core`](https://crates.io/crates/deno_core), in the style of Deno's
standard Web Platform extensions, delegating DSP and rendering to
the [Smudgy `web-audio-api-rs` fork](https://github.com/smudgy-mud/web-audio-api-rs).

**Status: `0.1.0-alpha.1`.** The native engine, bounded ownership model, and
hosted silent/CPAL/Cubeb output paths have passed their Phase 0 acceptance
gates. The deterministic Phase 1 offline graph and the planned Phase 2 useful
online playback MVP are complete. This alpha exposes `AudioContext`,
destination, `GainNode`, fixed and custom-wave
`OscillatorNode`, `PeriodicWave`, `ConstantSourceNode`,
`AudioBufferSourceNode`, scalar `AudioParam.value`, explicit connections,
lifecycle methods, `statechange`, exactly-once `ended`, and bounded off-isolate
`decodeAudioData` with both promise and legacy callback forms. The same
JavaScript surface renders through built-in system output or an
embedder-supplied `AudioOutputFactory`. Online parameter automation, additional
node families, and full browser conformance remain later releases, so this is
not yet a complete Web Audio API.

Same-context `suspend()` and `resume()` calls enter one FIFO coordinator before
native admission. An accepted transition remains owned after its JavaScript
promise is discarded; later callers observe that receipt before submitting
their own transition. Transient native contention or capacity refusal retains
no native command credit and sleeps until an explicit state, terminal, or Close
wake rather than spinning the isolate. Close is outside the waiter queue and
dominates active retry. The per-context waiter ceiling defaults to 64, is
configured for contexts in an isolate through
`AudioLimits::max_lifecycle_waiters`, and rejects overflow before native
mutation with `QuotaExceededError`.

The public `AudioBuffer` implements Web IDL conversion, nominal-range and brand
checks, standard prototype descriptors, `getChannelData()`,
`copyFromChannel()`, and `copyToChannel()`. Canonical PCM remains Rust-owned.
Each first channel exposure creates a separately budgeted V8 copy and returns
the same view until the native acquire algorithm detaches it. Copy methods use
canonical storage directly when a channel has never been exposed. Shared typed
arrays are rejected, directly structured-cloning a buffer raises `DataCloneError`,
and configured channel, logical-sample, sample-rate, per-isolate byte, and
shared-host byte ceilings reject before their represented allocation.

The offline graph reserves native nodes, scheduled sources, explicit edges,
automation records, legacy control traffic, render jobs, and output PCM before
the represented operation mutates the engine. It supports both
`OfflineAudioContext` constructor forms, branded/cross-context port checks,
duplicate and no-match connection semantics, one-shot `startRendering()`,
`complete`/`ended` delivery, constant sources, the four fixed oscillator
waveforms, bounded custom `PeriodicWave` playback, and the standard
scalar/automation methods on the exposed parameters. Graph ownership remains
charged while retained native node wrappers could still enqueue their
pre-reserved teardown records; rendering itself runs off the isolate thread.
Value-curve conversion is iterator-safe and capped at 1,048,576 samples before
the Rust copy; that copy is additionally charged to the isolate and shared-host
live-byte budgets.

Both graph modes support one-shot `AudioBufferSourceNode` playback with
`playbackRate`, `detune`, loop bounds, start offset/duration, stop, and `ended`.
Assigning a buffer acquires the Rust-owned canonical PCM only after the required
native control capacity is reserved, copies any exposed channel edits back, and
detaches the exposed views. This bounded slice permits at most one non-null
buffer assignment and does not permit clearing it afterward. A started online
source is rooted by its exact native generation until authoritative completion;
lossy event wakeups are reconciled from completion tokens, and confirmed context
shutdown releases any callback whose event could not be delivered.

Both graph modes also expose `ConstantSourceNode` with its scalar `offset`
parameter and the same generation-safe scheduled-source rooting. Each
`PeriodicWave` accepts at most 8,192 Fourier components and owns one fixed
32-KiB native table whose isolate and shared-host byte lease follows every
native clone, staged command, renderer replacement, and final reclamation. The
JavaScript wrapper carries only a weak context identity, so retaining a wave
does not retain an otherwise unreachable online context.

Extension JavaScript uses Deno's captured primordials. Installation is
idempotent for the same constructor and refuses to overwrite an unrelated
global. A startup-snapshot smoke test proves that immutable class bootstrap does
not capture an `AudioHost` or other runtime-native authority.

The remaining private proof objects and ops cover `CppGC`, owned asynchronous
work, shared `AudioHost` accounting, and bounded off-thread decoding. Online
lifecycle ownership, render-death handling, scheduled-source reconciliation,
exact oscillator and buffer-source graph nodes, and exact connection/control
reservations now back the public `AudioContext` slice. Their resources are
reserved against isolate and shared-host ceilings before mutation and remain
charged through typed rollback or physical retirement. A bounded fake process
mixer proves that independent hosted graphs and native input can share one
physical-output owner without coupling clocks or retirement, including decoded
PCM played through the public buffer-source surface.

The research baseline is deliberately locked to one compatibility set:

- `deno_core` 0.410.0
- `deno_webidl` 0.257.0
- `deno_web` 0.288.0
- `deno_error` 0.7.1
- `serde_v8` 0.319.0
- the Smudgy `web-audio-api` fork at commit
  `d0e967a136472389312c475ac1851b6836ad7292` (the reviewed hosted-output
  compatibility stack plus exact node, connection, scheduled-source,
  buffer-source, ConstantSource, PeriodicWave, PCM-storage, and control-command
  reservation lifetimes, based on 1.7.0)

The Deno crates move as a coordinated tuple. Upgrade them together in one
reviewed change, and keep `Cargo.lock` committed so Phase 0 evidence remains
reproducible. The locked Phase 0 graph requires Rust 1.91 or newer.

The audio fork is intentionally a pinned Git dependency and has no separate
crates.io identity. Consequently, functional `deno_audio` versions are consumed
from this repository at an immutable tag or revision rather than from crates.io.

The selected engine compiles its complete Symphonia decoder set unconditionally:
AAC, ADPCM, ALAC, FLAC, MPEG layers I/II/III, PCM, and Vorbis, with AIFF, CAF,
ISO MP4/M4A, Matroska/WebM, Ogg, and WAV containers. These are native dependency
capabilities. The extension exposes them through bounded `decodeAudioData`;
supported JavaScript behavior is otherwise limited to the surface listed above
and will expand as each vertical slice passes its gate.

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
private Deno binding proofs, decode worker, and offline/online lifecycle paths
also pass without default features.

Phase 1 Web Platform Test coverage is pinned to WPT commit
`fbc1b90e28b38e919acabeeb14d1b074f2c125ec`. See
[`tests/wpt/README.md`](tests/wpt/README.md) for the sparse-checkout command and
[`tests/wpt/expectations.json`](tests/wpt/expectations.json) for every runnable
file and reviewed skip reason. CI supplies the checkout and runs the subset
through this extension's `JsRuntime`; no browser or system audio device is
required. The reviewed subset contains seven passing files and six explicitly
skipped files across constructors, `AudioBuffer`, `PeriodicWave`,
connect/disconnect, and offline rendering.

Each extension instance is initialized with `AudioExtensionOptions`. Sharing
one `Arc<AudioHost>` across options makes the configured aggregate limits span
isolates; output and permission services remain separate authorities. The
default `AudioEventPumpLiveness::Unref` lets an otherwise idle `JsRuntime`
finish even while a context is open; session-style embedders can select
`AudioEventPumpLiveness::Ref` to keep the event loop alive until online context
observers retire. Neither mode weakens cancellation-independent native
teardown. Decode requests
reserve their encoded copy, concurrent job, result dimensions, and every
wrapper-owned PCM allocation before spawning or growing. Offline render
requests likewise reserve dimensions, output PCM, and a job slot before native
context construction. The finite per-isolate command default covers the 21-command permanent
graph bootstrap and every currently exposed user graph batch, including suspended staging.
Lifecycle barriers and mandatory teardown records remain governed by their separate bounded
upstream channels so cleanup never depends on user quota. The raw proof objects
remain available only through unsafe native exposure in tests and are not a
supported JavaScript API. Public code should use `OfflineAudioContext` for
deterministic rendering and `AudioContext` for the supported audible subset.

Published revisions of this crate resolve the engine through the exact Git
commit named above. No moving branch is an acceptable substitute for that
reviewed dependency tuple.

Phase 0 evidence is recorded in [`research/upstream-baseline.md`](research/upstream-baseline.md)
and [`research/existing-deno-audio-packages.md`](research/existing-deno-audio-packages.md).

`deno_audio` is an independent project. It is not affiliated with, endorsed
by, or maintained by Deno Land Inc. "Deno" is a trademark of Deno Land Inc.

## License

MIT. See `THIRD_PARTY_NOTICES.md` for direct-dependency provenance.
