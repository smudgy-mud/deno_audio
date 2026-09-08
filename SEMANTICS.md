# Recorded semantics

The [README](README.md) describes what `deno_audio` is; this document
records exactly what the current surface guarantees, as reviewed at each
vertical-slice acceptance gate. It is reference material for embedders and
reviewers, not an introduction.

## Exposed JavaScript surface

The `0.1.0-alpha.1` slice exposes `AudioContext`, destination, `GainNode`,
fixed and custom-wave `OscillatorNode`, `PeriodicWave`, `ConstantSourceNode`,
`AudioBufferSourceNode`, scalar `AudioParam.value`, explicit connections,
lifecycle methods, `statechange`, exactly-once `ended`, and bounded
off-isolate `decodeAudioData` with both promise and legacy callback forms. The
same JavaScript surface renders through built-in system output or an
embedder-supplied `AudioOutputFactory`. Online parameter automation,
additional node families, and full browser conformance remain later releases,
so this is not yet a complete Web Audio API.

## Context lifecycle

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

## AudioBuffer

The public `AudioBuffer` implements Web IDL conversion, nominal-range and brand
checks, standard prototype descriptors, `getChannelData()`,
`copyFromChannel()`, and `copyToChannel()`. Canonical PCM remains Rust-owned.
Each first channel exposure creates a separately budgeted V8 copy and returns
the same view until the native acquire algorithm detaches it. Copy methods use
canonical storage directly when a channel has never been exposed. Shared typed
arrays are rejected, directly structured-cloning a buffer raises `DataCloneError`,
and configured channel, logical-sample, sample-rate, per-isolate byte, and
shared-host byte ceilings reject before their represented allocation.

## Offline graph

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

## AudioBufferSourceNode

Both graph modes support one-shot `AudioBufferSourceNode` playback with
`playbackRate`, `detune`, loop bounds, start offset/duration, stop, and `ended`.
Assigning a buffer acquires the Rust-owned canonical PCM only after the required
native control capacity is reserved, copies any exposed channel edits back, and
detaches the exposed views. This bounded slice permits at most one non-null
buffer assignment and does not permit clearing it afterward. A started online
source is rooted by its exact native generation until authoritative completion;
lossy event wakeups are reconciled from completion tokens, and confirmed context
shutdown releases any callback whose event could not be delivered.

## ConstantSourceNode and PeriodicWave

Both graph modes also expose `ConstantSourceNode` with its scalar `offset`
parameter and the same generation-safe scheduled-source rooting. Each
`PeriodicWave` accepts at most 8,192 Fourier components and owns one fixed
32-KiB native table whose isolate and shared-host byte lease follows every
native clone, staged command, renderer replacement, and final reclamation. The
JavaScript wrapper carries only a weak context identity, so retaining a wave
does not retain an otherwise unreachable online context.

## Extension JavaScript and snapshots

Extension JavaScript uses Deno's captured primordials. Installation is
idempotent for the same constructor and refuses to overwrite an unrelated
global. A startup-snapshot smoke test proves that immutable class bootstrap does
not capture an `AudioHost` or other runtime-native authority.

## Native proof objects

The remaining private proof objects and ops cover `CppGC`, owned asynchronous
work, shared `AudioHost` accounting, and bounded off-thread decoding. Online
lifecycle ownership, render-death handling, scheduled-source reconciliation,
exact oscillator and buffer-source graph nodes, and exact connection/control
reservations now back the public `AudioContext` slice. Their resources are
reserved against isolate and shared-host ceilings before mutation and remain
charged through typed rollback or physical retirement. A bounded fake process
mixer proves that independent hosted graphs and native input can share one
physical-output owner without coupling clocks or retirement, including decoded
PCM played through the public buffer-source surface. The raw proof objects
remain available only through unsafe native exposure in tests and are not a
supported JavaScript API. Public code should use `OfflineAudioContext` for
deterministic rendering and `AudioContext` for the supported audible subset.

## Embedder integration

Each extension instance is initialized with `AudioExtensionOptions`. Sharing
one `Arc<AudioHost>` across options makes the configured aggregate limits span
isolates; output and permission services remain separate authorities. The
default `AudioEventPumpLiveness::Unref` lets an otherwise idle `JsRuntime`
finish even while a context is open; session-style embedders can select
`AudioEventPumpLiveness::Ref` to keep the event loop alive until online context
observers retire. Neither mode weakens cancellation-independent native
teardown.

Embedders that replace whole runtime generations can create an
`AudioLifecycleOwner`/`AudioLifecycleRegistrar` pair, install registrar clones
through `AudioExtensionOptions::lifecycle_registrar`, consume the owner to seal
the generation, drop its runtimes, and await the exact retirement receipt
before constructing a replacement. The receipt covers every admitted online
context across those extension instances and succeeds only after confirmed
native shutdown releases its host and isolate permits. Offline contexts are
unchanged and are not part of this drain.

Decode requests reserve their encoded copy, concurrent job, result dimensions,
and every wrapper-owned PCM allocation before spawning or growing. Offline
render requests likewise reserve dimensions, output PCM, and a job slot before
native context construction. The finite per-isolate command default covers the
21-command permanent graph bootstrap and every currently exposed user graph
batch, including suspended staging. Lifecycle barriers and mandatory teardown
records remain governed by their separate bounded upstream channels so cleanup
never depends on user quota.

Embedders that provide a shared logical mixer can implement the re-exported
`AudioOutputFactory`, `PreparedAudioOutput`, and `RunningAudioOutput` contracts
using only `deno_audio`'s public API. These names are direct aliases of the
pinned engine contracts, not wrapper traits, so
`AudioExtensionOptions::output_factory` accepts the implementation without a
second public type boundary.

## Compatibility baseline

The research baseline is deliberately locked to one compatibility set:

- `deno_core` 0.410.0
- `deno_webidl` 0.257.0
- `deno_web` 0.288.0
- `deno_error` 0.7.1
- `serde_v8` 0.319.0
- the smudgy `web-audio-api` fork at the exact commit recorded in
  `Cargo.toml` (the reviewed hosted-output compatibility stack plus exact
  node, connection, scheduled-source, buffer-source, ConstantSource,
  PeriodicWave, PCM-storage, and control-command reservation lifetimes,
  rebased onto upstream `c01bb99712c1a68387d2898cfcbab55caf92d0b1`, which
  adds borrowed-reader decoding after 1.7.0)

The Deno crates move as a coordinated tuple. Upgrade them together in one
reviewed change, and keep `Cargo.lock` committed so Phase 0 evidence remains
reproducible. The locked Phase 0 graph requires Rust 1.91 or newer. Published
revisions of this crate resolve the engine through the exact pinned Git
commit; no moving branch is an acceptable substitute for that reviewed
dependency tuple.

## Audio decoding

The selected engine compiles its complete Symphonia decoder set unconditionally:
AAC, ADPCM, ALAC, FLAC, MPEG layers I/II/III, PCM, and Vorbis, with AIFF, CAF,
ISO MP4/M4A, Matroska/WebM, Ogg, and WAV containers. These are native dependency
capabilities. The extension exposes them through bounded `decodeAudioData`;
supported JavaScript behavior is otherwise limited to the surface listed above
and will expand as each vertical slice passes its gate.

## Research evidence

Phase 0 evidence is recorded in
[`research/upstream-baseline.md`](research/upstream-baseline.md) and
[`research/existing-deno-audio-packages.md`](research/existing-deno-audio-packages.md).
