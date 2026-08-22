use std::sync::Arc;

use deno_audio::{AudioExtensionOptions, AudioHost, AudioHostLimits, AudioLimits};
use deno_core::{v8, JsRuntime, RuntimeOptions};

deno_core::extension!(
    deno_audio_bootstrap_test,
    esm_entry_point = "ext:deno_audio_bootstrap_test/main.js",
    esm = ["ext:deno_audio_bootstrap_test/main.js" = {
        source = r#"
          import { installGlobal } from "ext:deno_audio/00_webidl.js";
          import { AudioBuffer as InternalAudioBuffer } from
            "ext:deno_audio/01_audio_buffer.js";

          const OriginalTypeError = TypeError;
          const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
          const marker = { __proto__: null };
          Object.getOwnPropertyDescriptor = () => {
            throw new Error("polluted Object.getOwnPropertyDescriptor was called");
          };
          globalThis.TypeError = class PollutedTypeError extends Error {};

          installGlobal("__denoAudioInstallProbe", marker);
          installGlobal("__denoAudioInstallProbe", marker);
          installGlobal("AudioBuffer", InternalAudioBuffer);

          let collision;
          try {
            installGlobal("__denoAudioInstallProbe", { __proto__: null });
          } catch (error) {
            collision = error;
          }
          if (!(collision instanceof OriginalTypeError)) {
            throw new Error(`expected primordial TypeError, got ${collision}`);
          }
          const descriptor = getOwnPropertyDescriptor(
            globalThis,
            "__denoAudioInstallProbe",
          );
          if (descriptor.value !== marker || descriptor.enumerable ||
              !descriptor.writable || !descriptor.configurable) {
            throw new Error("idempotent install changed the global descriptor");
          }
        "#
    }],
);

fn runtime(host: Arc<AudioHost>, limits: AudioLimits) -> JsRuntime {
    JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(AudioExtensionOptions::new(host).limits(limits)),
        ],
        unsafe_expose_natives_and_gc: true,
        shared_array_buffer_store: Some(deno_core::SharedArrayBufferStore::default()),
        ..RuntimeOptions::default()
    })
}

fn collect_until(runtime: &mut JsRuntime, host: &AudioHost, expected: usize) {
    for _ in 0..32 {
        runtime
            .v8_isolate()
            .request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);
        if host.usage().live_audio_bytes() == expected {
            return;
        }
        std::thread::yield_now();
    }
}

/// V8 posts delayed garbage-collector tasks that `deno_core` schedules on the
/// tokio handle captured at `JsRuntime` construction and aborts the process
/// when none exists; synchronous tests enter this runtime's context before
/// constructing a `JsRuntime`.
fn tokio_context() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the test tokio runtime must build")
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one embedded JavaScript conformance scenario keeps descriptor, conversion, and copy assertions in the same realm"
)]
fn public_audio_buffer_has_webidl_shape_errors_and_copy_semantics() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());

    runtime
        .execute_script(
            "phase1_audio_buffer.js",
            r#"
              (() => {
                const { DOMException } = Deno.core.loadExtScript(
                  "ext:deno_web/01_dom_exception.js",
                );
                const fail = (message) => { throw new Error(message); };
                const assert = (condition, message) => {
                  if (!condition) fail(message);
                };
                const throws = (callback, constructor, name) => {
                  let error;
                  try { callback(); } catch (caught) { error = caught; }
                  assert(error instanceof constructor,
                    `expected ${constructor.name}, got ${error}`);
                  if (name !== undefined) {
                    assert(error.name === name,
                      `expected ${name}, got ${error.name}`);
                  }
                };

                const globalDescriptor = Object.getOwnPropertyDescriptor(
                  globalThis,
                  "AudioBuffer",
                );
                assert(globalDescriptor.value === AudioBuffer, "global value mismatch");
                assert(globalDescriptor.writable && globalDescriptor.configurable,
                  "global must be writable and configurable");
                assert(!globalDescriptor.enumerable, "global must not be enumerable");
                assert(AudioBuffer.length === 1 && AudioBuffer.name === "AudioBuffer",
                  "constructor metadata mismatch");
                assert(AudioBuffer.prototype.constructor === AudioBuffer,
                  "prototype constructor mismatch");
                assert(Object.getPrototypeOf(AudioBuffer) === Function.prototype,
                  "constructor prototype chain mismatch");
                assert(Object.getPrototypeOf(AudioBuffer.prototype) === Object.prototype,
                  "instance prototype chain mismatch");
                assert(AudioBuffer.prototype[Symbol.toStringTag] === "AudioBuffer",
                  "toStringTag mismatch");
                const constructorDescriptor = Object.getOwnPropertyDescriptor(
                  AudioBuffer.prototype,
                  "constructor",
                );
                assert(constructorDescriptor.value === AudioBuffer &&
                    constructorDescriptor.writable &&
                    constructorDescriptor.configurable &&
                    !constructorDescriptor.enumerable,
                  "prototype constructor descriptor mismatch");
                const tagDescriptor = Object.getOwnPropertyDescriptor(
                  AudioBuffer.prototype,
                  Symbol.toStringTag,
                );
                assert(tagDescriptor.value === "AudioBuffer" &&
                    !tagDescriptor.writable && tagDescriptor.configurable &&
                    !tagDescriptor.enumerable,
                  "toStringTag descriptor mismatch");

                for (const name of [
                  "sampleRate",
                  "length",
                  "duration",
                  "numberOfChannels",
                ]) {
                  const descriptor = Object.getOwnPropertyDescriptor(
                    AudioBuffer.prototype,
                    name,
                  );
                  assert(typeof descriptor.get === "function" && descriptor.set === undefined,
                    `${name} must be readonly`);
                  assert(descriptor.enumerable && descriptor.configurable,
                    `${name} descriptor mismatch`);
                }
                assert(AudioBuffer.prototype.getChannelData.length === 1,
                  "getChannelData length mismatch");
                assert(AudioBuffer.prototype.copyFromChannel.length === 2,
                  "copyFromChannel length mismatch");
                assert(AudioBuffer.prototype.copyToChannel.length === 2,
                  "copyToChannel length mismatch");
                for (const name of [
                  "getChannelData",
                  "copyFromChannel",
                  "copyToChannel",
                ]) {
                  const descriptor = Object.getOwnPropertyDescriptor(
                    AudioBuffer.prototype,
                    name,
                  );
                  assert(typeof descriptor.value === "function" &&
                      descriptor.writable && descriptor.enumerable &&
                      descriptor.configurable,
                    `${name} descriptor mismatch`);
                }

                throws(() => AudioBuffer({ length: 1, sampleRate: 48000 }), TypeError);
                throws(() => new AudioBuffer(), TypeError);
                throws(() => new AudioBuffer({}), TypeError);
                throws(() => new AudioBuffer({ length: 1 }), TypeError);
                throws(() => new AudioBuffer({ length: 1, sampleRate: NaN }), TypeError);
                throws(() => new AudioBuffer({ length: 1, sampleRate: Infinity }), TypeError);
                for (const options of [
                  { numberOfChannels: 0, length: 1, sampleRate: 48000 },
                  { numberOfChannels: 33, length: 1, sampleRate: 48000 },
                  { numberOfChannels: 1, length: 0, sampleRate: 48000 },
                  { numberOfChannels: 1, length: 1, sampleRate: 2999 },
                  { numberOfChannels: 1, length: 1, sampleRate: 768001 },
                ]) {
                  throws(
                    () => new AudioBuffer(options),
                    DOMException,
                    "NotSupportedError",
                  );
                }

                const buffer = new AudioBuffer({ length: 4, sampleRate: 48000 });
                assert(buffer.numberOfChannels === 1, "default channel count mismatch");
                assert(buffer.length === 4 && buffer.sampleRate === 48000,
                  "buffer attributes mismatch");
                assert(buffer.duration === 4 / 48000, "duration mismatch");
                assert(Object.prototype.toString.call(buffer) === "[object AudioBuffer]",
                  "platform object brand mismatch");
                throws(() => AudioBuffer.prototype.getChannelData.call({}), TypeError);
                throws(
                  () => Object.getOwnPropertyDescriptor(
                    AudioBuffer.prototype,
                    "length",
                  ).get.call({}),
                  TypeError,
                );
                throws(() => buffer.getChannelData(-1), DOMException, "IndexSizeError");
                throws(() => buffer.getChannelData(1), DOMException, "IndexSizeError");
                throws(
                  () => buffer.copyFromChannel(new Float32Array(1), -1),
                  DOMException,
                  "IndexSizeError",
                );
                throws(
                  () => buffer.copyToChannel(new Float32Array(1), 1),
                  DOMException,
                  "IndexSizeError",
                );
                throws(() => buffer.copyFromChannel([], 0), TypeError);
                throws(() => buffer.copyToChannel([], 0), TypeError);

                const shared = new Float32Array(new WebAssembly.Memory({
                  shared: true,
                  initial: 1,
                  maximum: 1,
                }).buffer);
                throws(() => buffer.copyFromChannel(shared, 0), TypeError);
                throws(() => buffer.copyToChannel(shared, 0), TypeError);

                buffer.copyToChannel(new Float32Array([1, 2, 3, 4]), 0);
                const destination = new Float32Array([9, 9, 9, 9, 9]);
                buffer.copyFromChannel(destination, 0, 2);
                assert(`${destination}` === "3,4,9,9,9", "copyFrom truncation mismatch");
                buffer.copyFromChannel(destination, 0, -1);
                assert(`${destination}` === "3,4,9,9,9", "wrapped offset must be a no-op");
                buffer.copyToChannel(new Float32Array([7, 8]), 0, 3);

                const channel = buffer.getChannelData(0);
                assert(channel === buffer.getChannelData(0), "same-object cache mismatch");
                assert(`${channel}` === "1,2,3,7", "copy visibility mismatch");
                buffer.copyToChannel(channel.subarray(0, 3), 0, 1);
                assert(`${channel}` === "1,1,2,3", "overlapping copyTo mismatch");
                buffer.copyFromChannel(channel.subarray(1, 4), 0);
                assert(`${channel}` === "1,1,1,2", "overlapping copyFrom mismatch");

                globalThis.__phase1AudioBuffer = buffer;
                globalThis.__phase1AudioView = channel;
              })();
            "#,
        )
        .expect("the public AudioBuffer surface should be conformant");

    assert_eq!(
        host.usage().live_audio_bytes(),
        32,
        "canonical and exposed four-frame mono storage are both charged"
    );

    runtime
        .execute_script(
            "phase1_audio_buffer_release.js",
            "globalThis.__phase1AudioBuffer = undefined; globalThis.__phase1AudioView = undefined;",
        )
        .unwrap();
    collect_until(&mut runtime, &host, 0);
    assert_eq!(host.usage().live_audio_bytes(), 0);
}

#[test]
fn copies_do_not_expose_channels_and_quota_failures_recover() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::unlimited().max_live_audio_bytes(Some(31)),
    ));
    let mut runtime = runtime(
        Arc::clone(&host),
        AudioLimits::unlimited().max_live_audio_bytes(Some(31)),
    );

    runtime
        .execute_script(
            "phase1_audio_buffer_quota.js",
            r#"
              (() => {
                const { DOMException } = Deno.core.loadExtScript(
                  "ext:deno_web/01_dom_exception.js",
                );
                const expectQuota = (callback) => {
                  let error;
                  try { callback(); } catch (caught) { error = caught; }
                  if (!(error instanceof DOMException) ||
                      error.name !== "QuotaExceededError") {
                    throw new Error(`expected QuotaExceededError, got ${error}`);
                  }
                };

                expectQuota(() => new AudioBuffer({
                  numberOfChannels: 2,
                  length: 4,
                  sampleRate: 48000,
                }));

                const buffer = new AudioBuffer({ length: 4, sampleRate: 48000 });
                buffer.copyToChannel(new Float32Array([1, 2, 3, 4]), 0);
                const destination = new Float32Array(4);
                buffer.copyFromChannel(destination, 0);
                if (`${destination}` !== "1,2,3,4") {
                  throw new Error("unexposed copy path lost PCM");
                }
                expectQuota(() => buffer.getChannelData(0));
                buffer.copyToChannel(new Float32Array([4, 3, 2, 1]), 0);
                buffer.copyFromChannel(destination, 0);
                if (`${destination}` !== "4,3,2,1") {
                  throw new Error("failed exposure corrupted canonical PCM");
                }
                globalThis.__quotaAudioBuffer = buffer;
              })();
            "#,
        )
        .expect("quota failures should be public, typed, and recoverable");

    assert_eq!(
        host.usage().live_audio_bytes(),
        16,
        "copy methods must not expose or double the channel"
    );
    runtime
        .execute_script(
            "phase1_audio_buffer_quota_release.js",
            "globalThis.__quotaAudioBuffer = undefined;",
        )
        .unwrap();
    collect_until(&mut runtime, &host, 0);
    assert_eq!(host.usage().live_audio_bytes(), 0);
}

#[test]
fn configured_buffer_dimensions_refuse_before_native_allocation() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(
        Arc::clone(&host),
        AudioLimits::unlimited()
            .max_channels(Some(1))
            .max_audio_buffer_samples(Some(3))
            .max_sample_rate(Some(44_100)),
    );

    runtime
        .execute_script(
            "phase1_audio_buffer_dimensions.js",
            r#"
              (() => {
                const { DOMException } = Deno.core.loadExtScript(
                  "ext:deno_web/01_dom_exception.js",
                );
                const expectQuota = (options) => {
                  let error;
                  try { new AudioBuffer(options); } catch (caught) { error = caught; }
                  if (!(error instanceof DOMException) ||
                      error.name !== "QuotaExceededError") {
                    throw new Error(`expected QuotaExceededError, got ${error}`);
                  }
                };
                expectQuota({ numberOfChannels: 2, length: 1, sampleRate: 44100 });
                expectQuota({ numberOfChannels: 1, length: 4, sampleRate: 44100 });
                expectQuota({ numberOfChannels: 1, length: 1, sampleRate: 48000 });

                globalThis.__limitedAudioBuffer = new AudioBuffer({
                  numberOfChannels: 1,
                  length: 3,
                  sampleRate: 44100,
                });
              })();
            "#,
        )
        .expect("configured buffer dimensions should fail before allocation");
    assert_eq!(host.usage().live_audio_bytes(), 12);

    runtime
        .execute_script(
            "phase1_audio_buffer_dimensions_release.js",
            "globalThis.__limitedAudioBuffer = undefined;",
        )
        .unwrap();
    collect_until(&mut runtime, &host, 0);
    assert_eq!(host.usage().live_audio_bytes(), 0);
}

#[test]
fn bootstrap_installation_is_idempotent_and_refuses_collisions() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let _runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(AudioExtensionOptions::default()),
            deno_audio_bootstrap_test::init(),
        ],
        // All tests in this process must agree on V8's process-global GC
        // testing flag, independent of which test initializes V8 first.
        unsafe_expose_natives_and_gc: true,
        ..RuntimeOptions::default()
    });
}

#[test]
fn public_buffer_paths_resist_prototype_pollution_and_reject_cloning() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::unlimited().max_live_audio_bytes(Some(31)),
    ));
    let mut runtime = runtime(
        Arc::clone(&host),
        AudioLimits::unlimited().max_live_audio_bytes(Some(31)),
    );

    runtime
        .execute_script(
            "phase1_audio_buffer_primordials.js",
            r#"
              (() => {
                const { DOMException } = Deno.core.loadExtScript(
                  "ext:deno_web/01_dom_exception.js",
                );
                const { structuredClone } = Deno.core.loadExtScript(
                  "ext:deno_web/13_message_port.js",
                );
                WeakMap.prototype.get = () => {
                  throw new Error("polluted WeakMap.get was called");
                };
                WeakMap.prototype.set = () => {
                  throw new Error("polluted WeakMap.set was called");
                };
                Float32Array.prototype.set = () => {
                  throw new Error("polluted Float32Array.set was called");
                };
                Float32Array.prototype.subarray = () => {
                  throw new Error("polluted Float32Array.subarray was called");
                };
                RangeError.prototype.name = "PollutedRangeError";

                const buffer = new AudioBuffer({ length: 4, sampleRate: 48000 });
                buffer.copyToChannel(new Float32Array([1, 2, 3, 4]), 0);
                const destination = new Float32Array(4);
                buffer.copyFromChannel(destination, 0);
                if (destination[0] !== 1 || destination[1] !== 2 ||
                    destination[2] !== 3 || destination[3] !== 4) {
                  throw new Error("prototype pollution changed copied PCM");
                }

                let cloneError;
                try { structuredClone(buffer); } catch (error) { cloneError = error; }
                if (!(cloneError instanceof DOMException) ||
                    cloneError.name !== "DataCloneError") {
                  throw new Error(`expected DataCloneError, got ${cloneError}`);
                }

                let quotaError;
                try { buffer.getChannelData(0); } catch (error) { quotaError = error; }
                if (!(quotaError instanceof DOMException) ||
                    quotaError.name !== "QuotaExceededError") {
                  throw new Error(`expected QuotaExceededError, got ${quotaError}`);
                }
              })();
            "#,
        )
        .expect("public buffer paths should use captured primordials");

    assert_eq!(host.usage().live_audio_bytes(), 16);
    collect_until(&mut runtime, &host, 0);
    assert_eq!(host.usage().live_audio_bytes(), 0);
}
