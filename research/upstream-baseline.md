# `web-audio-api` 1.7.0 baseline

Date verified: 2026-08-17

## Compatibility and reference revisions

| Project | Release | Commit | Role |
| --- | --- | --- | --- |
| [Deno](https://github.com/denoland/deno/tree/v2.9.5) | v2.9.5 | [`17fadf33a8df3af9488b9f42efd1f2290d6dc7a3`](https://github.com/denoland/deno/commit/17fadf33a8df3af9488b9f42efd1f2290d6dc7a3) | Extension conventions and the coordinated Deno crate tuple |
| [`web-audio-api-rs`](https://github.com/orottier/web-audio-api-rs/tree/v1.7.0) | v1.7.0 | [`2af21af6ff2bbee17201b2dd2a5ebc56f55c3511`](https://github.com/orottier/web-audio-api-rs/commit/2af21af6ff2bbee17201b2dd2a5ebc56f55c3511) | Native behavior characterized below |
| [`node-web-audio-api`](https://github.com/ircam-ismm/node-web-audio-api/tree/v2.2.0) | v2.2.0 | [`9f789566acab91f14257e3444d49a22a9080e672`](https://github.com/ircam-ismm/node-web-audio-api/commit/9f789566acab91f14257e3444d49a22a9080e672) | Behavioral and WPT-runner reference only; no source is adapted here |

The exact Deno crate versions are recorded in `Cargo.toml` and `Cargo.lock`.
Registry checksums plus these immutable upstream revisions make the research
inputs reviewable without treating a moving branch as provenance.

## Native engine baseline

Crate: `web-audio-api` 1.7.0

Upstream tag: `v1.7.0`

Upstream commit: `2af21af6ff2bbee17201b2dd2a5ebc56f55c3511`

Fork: <https://github.com/smudgy-mud/web-audio-api-rs>

Manifest source: the exact commit above, through Cargo `git` + `rev`

Version 1.7.0 is a characterization baseline, not the production engine. Phase
0B implemented the required contracts in a maintained Smudgy fork based on this
commit. Historical reproduction results are preserved below; the replacement
acceptance tests in `tests/engine_contracts.rs` and `tests/output_ownership.rs`
exercise the selected fork without opening a physical audio device.

Upstream issues, PRs, and design discussions are intentionally deferred until
the fork and extension work end to end. They are not a Phase 0 gate.

| Seam | Executable evidence | 1.7.0 source anchor | Required production contract |
| --- | --- | --- | --- |
| Terminal completion | A dedicated sentinel callback is provably blocked before a 640-source burst ends; exactly 256 burst callbacks survive the saturated event queue and 384 are lost. | [`src/io/mod.rs`](https://github.com/orottier/web-audio-api-rs/blob/v1.7.0/src/io/mod.rs#L108-L127), [`src/render/processor.rs`](https://github.com/orottier/web-audio-api-rs/blob/v1.7.0/src/render/processor.rs#L40-L61) | Non-blocking render notification plus exactly-once reconciliation on the control side. |
| Running `Drop` | A silent-backend source still ends after its owning `AudioContext` is dropped. | [`src/context/online.rs`](https://github.com/orottier/web-audio-api-rs/blob/v1.7.0/src/context/online.rs#L164-L171) | Idempotent shutdown with acknowledgement after backend resources are gone. |
| Suspended traffic | A suspended context accepts 2,048 `GainNode`s infallibly and diagnostics prove all 4,096 node/parameter registrations survive replay, well past the live channel's 256-record bound. | [`src/context/concrete_base.rs`](https://github.com/orottier/web-audio-api-rs/blob/v1.7.0/src/context/concrete_base.rs#L272-L319) | Bounded, fallible submission with observable retirement. |
| Decode growth | A 6,044-byte, 3 kHz PCM wave decodes and resamples into 3,072,000 bytes at 768 kHz without a reservation hook. | [`src/decoding.rs`](https://github.com/orottier/web-audio-api-rs/blob/v1.7.0/src/decoding.rs#L15-L53) | Reserve channels, frames, output bytes, and resampling growth before allocation. |
| Output ownership | The source audit records that the online constructor directly calls the crate-selected private `io::build_output`; constructor hard-wiring has no public compile-time observation seam. | [`src/context/online.rs`](https://github.com/orottier/web-audio-api-rs/blob/v1.7.0/src/context/online.rs#L249-L252), [`src/io/mod.rs`](https://github.com/orottier/web-audio-api-rs/blob/v1.7.0/src/io/mod.rs#L151-L173) | A public, real-time-safe output factory suitable for a host-owned mixer/device endpoint. |

These are baseline assertions: a production dependency that fixes a seam will
require the corresponding test to be replaced with an acceptance test for the
new contract. A green baseline suite alone is not evidence of readiness.

## Selected production fork

Phase 0B selected reviewed fork commit
`2c2b260516a7557b5ee487bc89cc1dd08c048d35`. It retains the upstream package
name and the legacy behavior characterized above while adding a public hosted
`AudioContext` builder, bounded exact lifecycle/control ownership, a public
`AudioOutputFactory`, and `SystemAudioOutput` implementations for the silent,
CPAL, and Cubeb paths. The dependency remains Git-only; the canonical remote
must publish that exact commit before a fresh checkout can reproduce the lockfile
without the maintainer's local repository.

The working-lockfile acceptance evidence maps the five gates as follows:

| Contract | Selected-fork evidence |
| --- | --- |
| Scheduled-source completion | `engine_contracts::completion_tokens_survive_a_saturated_event_consumer` blocks event dispatch while every source reaches its monotonic native completion token. |
| Online backend teardown | `engine_contracts::hosted_context_drop_has_a_confirmed_silent_shutdown` observes autonomous confirmed retirement after `AudioContext::drop`, including terminal state through a surviving node. |
| Suspended/control traffic | `engine_contracts::suspended_hosted_control_is_bounded_and_recovers` reaches the fixed admission limit before unbounded growth, then resumes and closes the accepted graph cleanly. |
| Decode growth | `engine_contracts::decode_resampling_reserves_output_before_growth` rejects the resampling reservation and proves all earlier reservation guards are released. |
| Output ownership | `output_ownership` injects a public host factory before graph construction and renders a real hosted graph through public `SystemAudioOutput` using the hardware-free `none` sink. |
