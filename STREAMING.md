# Native decoder-worker ownership

`AudioFileStream::open` is an experimental Rust embedder API. It uses the current
extension's `OpState`, a hosted native `AudioContext`, and an already-authorized
file-opening closure. The closure runs on the decoder thread after admission.
No JavaScript global, file permission, URL fetch, or package resolver is added.

The existing engine handles WAV, MP3, FLAC, Ogg/Vorbis, AAC-LC/M4A, ALAC/M4A, and
AIFF within its mono/stereo and 8–192 kHz limits. Its file decoder is seekable and
incremental; the host never assembles the complete encoded file or decoded sound.

## Accounting

Each stream reserves, before opening or spawning:

| Resource | Charge | Release point |
| --- | --- | --- |
| Streaming job | One, per isolate and shared host | Observer has joined the decoder |
| Working storage | 4 MiB, per isolate and shared host | Observer has joined the decoder |
| PCM queue | 16 KiB, per isolate and shared host | Graph reclamation and all queue/state handles released |
| Graph node | One, per isolate and shared host | Same physical lifetime as the PCM queue |
| Construction command | One, per isolate and shared host | Existing engine command-consumption/rollback contract |

`max_streaming_jobs` limits streaming workers independently of the whole-buffer
`max_decode_jobs` slots used by `decodeAudioData`. Default per-isolate limits allow
eight streams and four buffer decodes. Set an explicit shared-host streaming limit
as well: `AudioHostLimits::default()` and `unlimited()` have no aggregate ceiling.
Both paths still share `max_live_audio_bytes`, `max_graph_nodes`, and
`max_queued_control_commands`. Eight streams reserve 32.125 MiB for working storage
and queues, leaving 31.875 MiB in a 64 MiB host budget before other audio charges.
Cached buffers can play repeatedly without holding decoder jobs. Shared memory
exhaustion can still refuse a buffer load. Connections remain the native
caller's responsibility; this API does not substitute for the script graph's
connection admission and identity checks.

The 4 MiB working charge is a conservative allowance for the pinned engine's
bounded PCM packet, resampler, transfer storage, and associated working overhead.
The engine bounds each decoded packet to 65,536 mono/stereo frames; its resampler
has fixed 1024-frame input, 256-tap filters, and a maximum 24:1 rate ratio.
The allowance is held even when the file uses less storage. It is not an
allocator-enforced limit on process memory. Container indexes, aggregate metadata,
codec-private allocations, thread stacks, and arbitrary native opener captures
are not measured by this charge. Before enabling untrusted media, those need an
additional enforceable policy. Whole-buffer `max_encoded_bytes`,
`max_decoded_frames`, and `max_decoded_samples` are not stream-duration limits.

## Ownership and completion

The source is initially paused. Connect `stream.node()`, wait for enough
`queued_frames()` or decoder completion, then call `stream.node().start()`.
The writer applies backpressure at exactly 2048 queued stereo frames. The
decoder polls cancellation while backpressured and between decode calls.

`stop()` and source-handle Drop request cancellation and silence the source.
The extension's `OpState` owns cancellation independently of the runtime state
retained by quota guards. Dropping a runtime therefore cancels active workers
without synchronously joining on the isolate thread. Physical context teardown
also cancels the writer. A generation seal rejects later admission and active
decoders observe it as cancellation. An in-flight OS read or codec call must
return before its worker can exit; quotas and retirement remain pending meanwhile.

A native observer owns each decoder's join handle. Even dropping both the stream
and its completion receipt cannot detach that decoder or release its charges.
The generation's existing retirement receipt includes the join obligation when
a lifecycle registrar is configured. A decoder error or panic still permits a
clean lifecycle retirement once its thread has been joined; decoding success
and retirement success are different results.

`take_completion()` returns one join receipt. `Eof` means the decoder finished
and queued samples may still be playing; `node().ended()` reports playback end.
Errors stop the source and arrive as completion errors. Cancellation stops it
permanently. Releasing worker charges at EOF does not release the queued tail's
separate graph/queue charge.

The observer itself is a small detached native thread, following the existing
context-observer ownership pattern. It owns and joins the decoder; it never
passes that join handle to a future or the isolate. No worker, wait, or file I/O
is introduced on the render callback.

## Evidence

Tests cover EOF with retained PCM, per-isolate/shared quota refusal before open,
cross-isolate competition and recovery, cancellation while the queue is full,
source/isolate/context teardown, generation sealing, open/decode errors,
worker panic, and injected observer/decoder spawn failures. A gated thread-local destructor proves quotas and retirement stay
pending until actual thread exit, even after the completion receipt and isolate
are dropped. A real `JsRuntime` teardown test verifies the extension installs the
owner and cancels a blocked opener without abandoning its join obligation.
An output-capture test mixes eight synthetic speech streams with three voices
sharing a cached cue, then verifies cancelling one stream leaves the other audio
playing. A JavaScript integration test admits four concurrent `decodeAudioData`
calls and plays a cached cue eight times while all eight streaming slots are held.
