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
`dd83ee6530136a71caff97da06269302f129cb09`. It retains the upstream package
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

## Phase 0C binding and offline evidence

Date verified: 2026-08-19

The extension still installs no Web Audio globals. Its private native seams now
provide executable evidence for the representation decisions that the public
surface will use:

| Seam | Executable evidence | Result |
| --- | --- | --- |
| `CppGC` ownership | `binding::tests::cppgc_dispatch_tracing_and_finalization_are_real` | Constructor and method dispatch are branded; a traced `Member` keeps its child alive; clearing the member and releasing the final JavaScript root finalize exactly the expected objects. |
| Erased native nodes | `node::tests::erased_concrete_nodes_connect_and_render_through_dyn_audio_node` | One closed `NativeAudioNode` enum stores destination, gain, and oscillator nodes and connects unlike variants through `&dyn AudioNode`; the resulting graph renders non-silent audio. |
| `AudioBuffer` exposure | `buffer::tests::exposure_acquisition_transfer_reservations_and_gc_are_coherent` | Repeated `getChannelData` calls return the same traced view; unexposed access remains Rust-side; acquisition copies exposed data then detaches; an already transferred view cannot overwrite canonical PCM; re-exposure creates a fresh view. |
| Native audio-byte accounting | Both `buffer::tests` | Canonical and exposed bytes are reserved before their represented allocations. A V8 backing-store-owned reservation follows an external transfer and releases only after the transferred store is collected; wrapper collection releases canonical and cached-view reservations. Limit refusal happens before canonical or exposed allocation. |
| Owned asynchronous boundary | `offline::tests::decode_and_render_ops_leave_the_isolate_thread` | A synchronous single-use request reserves and copies encoded input before the async op; neither blocking task accepts `OpState` or V8 handles, and both decode and offline render execute on a blocking worker rather than the isolate thread. |
| Offline suspension | `offline::tests::offline_suspend_and_resume_observe_the_render_quantum` | With default features disabled, upstream `suspend(time)` pauses before frame 128 and `resume()` makes frames 128 onward audible. The future coverage manifest should therefore mark offline suspend/resume supported rather than `upstream-missing`. |
| Thread bounds | The erased-node test plus the blocking-op type checks | `AudioBuffer`, `OfflineAudioContext`, the selected concrete node types, the erased enum, and blocking request/result ownership satisfy the required `Send`/`Sync` or `Send` bounds at compile time. |

The buffer proof deliberately uses safe `rusty_v8` backing-store ownership, not
an external raw PCM pointer. Actual `AudioBufferSourceNode` and `ConvolverNode`
acquisition remain later public-surface work; their native consumers must retain
the canonical byte lease alongside every acquired engine buffer clone.

## Phase 0D lifecycle and resource checkpoint

Date verified: 2026-08-19

The first Phase 0D checkpoint keeps its native objects private while proving
the ownership model that the public `WebIDL` facade will use:

| Contract | Executable evidence | Result |
| --- | --- | --- |
| Shared online permits | `phase0d_lifecycle::shared_host_rejects_before_construction_and_releases_only_after_shutdown` | Two runtimes share a one-permit `AudioHost`; the second is rejected before its output factory runs, confirmed close releases the permit, and isolate drop autonomously confirms retirement. |
| Fail-closed shutdown | `phase0d_lifecycle::unconfirmed_isolate_drop_retains_the_shared_permit` | An endpoint that cannot prove callback destruction yields unconfirmed shutdown and permanently retains the shared permit for the host accounting lifetime. |
| Cancellation independence | `phase0d_lifecycle::accepted_state_and_close_commands_outlive_javascript_waiters` | Discarded JavaScript promises do not cancel accepted resume, suspend, or close work; invalid input rejects before admission. |
| Render death | `phase0d_lifecycle::render_callback_death_poison_closes_and_releases_after_acknowledgement` | A callback protocol death occurs without V8 access, closes the context through the same native lifecycle, rejects later resume, and releases the permit only after confirmed teardown. |
| Cross-isolate bytes | `phase0d_lifecycle::shared_audio_bytes_exhaust_and_recover_across_isolates` | Canonical and exposed `AudioBuffer` bytes reserve both per-isolate and shared counters before allocation; a second isolate is rejected while the first remains live and recovers after destruction. |
| Lossy ended reconciliation | `phase0d_lifecycle::saturated_scheduled_sources_dispatch_once_or_release_on_close` | Three hundred exact oscillators end at one common render quantum while the isolate is blocked. The native wake queue stays bounded at 256, one reconciliation dispatches a unique bounded batch from authoritative completion tokens, and confirmed close releases every undispatched root and reservation. |
| Decode dimensions and PCM | `phase0d_decode::every_decode_dimension_and_pcm_stage_refuses_and_recovers` | Channel, cumulative-frame, logical-sample, canonical chunk, overlapping final assembly, and source-plus-target resampling reservations each reject at the fork's preallocation boundary; every outer byte/job reservation returns to zero. |
| Decode input and jobs | `phase0d_decode::encoded_job_corrupt_and_canceled_requests_release_all_reservations` | Encoded bytes and a decode-job slot are reserved before the single owned copy or blocking spawn. Oversize, concurrent-job refusal, corrupt input, and a discarded JavaScript waiter all leave the isolate responsive and release the represented resources. |
| Cross-isolate decode recovery | `phase0d_decode::shared_encoded_bytes_and_jobs_exhaust_before_copy_and_recover` | Two runtimes sharing one host exhaust decode jobs and encoded live bytes before the rejected request copies input; dropping the owning runtime restores both shared counters. |
| Offline render ownership | All `phase0d_offline` tests | Channels, frames, logical samples, output PCM, and isolate/shared job slots are reserved before native context construction. Single-use transfer and discarded waiters complete off-isolate; limit failures and cross-isolate contention recover every shared byte and job counter. |
| Shared process mixer | All `phase0d_mixer` tests | One bounded joinable physical-output surrogate mixes two independently hosted exact graphs with a third native input using preallocated render scratch and deterministic ordering. One graph can suspend, resume, or retire without advancing or destroying the other; fixed-capacity rejection retains callback cleanup in the failed-build receipt, and authoritative factory death closes both contexts. |

The saturation proof exposed an incompatible 256 ordinary-node ceiling in the
otherwise reviewed hosted engine: one exact oscillator consumes three lifetime
slots. Fork commit `c0ec361` raises only the preallocated ordinary node/topology
capacity to 1024. The independent explicit-edge and logical-control ceilings
remain 256. This is a bounded control-thread allocation change and preserves
the renderer's no-growth invariant while making 257-or-more simultaneous
scheduled sources representable.
