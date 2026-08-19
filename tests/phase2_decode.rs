//! Public Phase 2 `decodeAudioData` ownership, detachment, and callback evidence.

use std::sync::Arc;
use std::time::{Duration, Instant};

use deno_audio::{AudioExtensionOptions, AudioHost, AudioLimits};
use deno_core::{JsRuntime, RuntimeOptions};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn pcm16_wav(channels: u16, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    assert_ne!(channels, 0);
    assert_eq!(samples.len() % usize::from(channels), 0);
    let data_len = u32::try_from(std::mem::size_of_val(samples)).unwrap();
    let block_align = channels * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

fn byte_literal(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

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
        ..RuntimeOptions::default()
    })
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for cleanup");
        std::thread::yield_now();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn public_decode_detaches_resamples_and_invokes_the_legacy_callback() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::unlimited());
    let wav = pcm16_wav(1, 6_000, &[0, 16_384, -16_384, 0]);
    runtime
        .execute_script(
            "phase2_public_decode.js",
            format!(
                r#"
                  globalThis.__phase2Decode = (async () => {{
                    const context = new AudioContext({{
                      sampleRate: 6000,
                      sinkId: {{ type: "none" }},
                    }});
                    const encoded = new Uint8Array([{}]).buffer;
                    let callbackValue;
                    const promise = context.decodeAudioData(
                      encoded,
                      (value) => {{ callbackValue = value; }},
                    );
                    if (encoded.byteLength !== 0) {{
                      throw new Error("decodeAudioData did not synchronously detach its input");
                    }}
                    const decoded = await promise;
                    await Promise.resolve();
                    if (!(decoded instanceof AudioBuffer) || callbackValue !== decoded) {{
                      throw new Error("promise and legacy success callback diverged");
                    }}
                    if (decoded.numberOfChannels !== 1 || decoded.length !== 4 ||
                        decoded.sampleRate !== 6000) {{
                      throw new Error("decoded AudioBuffer dimensions were wrong");
                    }}
                    const data = decoded.getChannelData(0);
                    if (Math.abs(data[1] - 0.5) > 0.001 ||
                        Math.abs(data[2] + 0.5) > 0.001) {{
                      throw new Error("decoded PCM did not reach the public AudioBuffer");
                    }}
                    await context.close();

                    // A closed context retains its immutable sample rate and remains a valid
                    // decode target; decoding is independent of its retired output endpoint.
                    const afterClose = new Uint8Array([{}]).buffer;
                    const second = await context.decodeAudioData(afterClose, null, null);
                    if (afterClose.byteLength !== 0 || second.sampleRate !== 6000) {{
                      throw new Error("closed-context decode did not retain its target rate");
                    }}
                    globalThis.__decodedHold = second;
                  }})();
                "#,
                byte_literal(&wav),
                byte_literal(&wav),
            ),
        )
        .expect("public decode scenario should start");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("public decode scenario should settle");
    assert!(
        host.usage().live_audio_bytes() >= 16,
        "the decoded AudioBuffer must retain its canonical PCM reservation"
    );
    drop(runtime);
    wait_until(|| {
        let usage = host.usage();
        usage.live_audio_bytes() == 0 && usage.decode_jobs() == 0 && usage.online_contexts() == 0
    });
}

#[tokio::test(flavor = "current_thread")]
async fn public_decode_rejects_detached_corrupt_and_limited_inputs_honestly() {
    let host = Arc::new(AudioHost::default());
    let limits = AudioLimits::unlimited().max_encoded_bytes(Some(48));
    let mut runtime = runtime(Arc::clone(&host), limits);
    let wav = pcm16_wav(1, 6_000, &[0, 1, -1, 0]);
    runtime
        .execute_script(
            "phase2_public_decode_errors.js",
            format!(
                r#"
                  globalThis.__phase2DecodeErrors = (async () => {{
                    const context = new OfflineAudioContext(1, 4, 6000);
                    if (BaseAudioContext.prototype.decodeAudioData.length !== 1) {{
                      throw new Error("decodeAudioData WebIDL length was wrong");
                    }}

                    const illegalInput = new ArrayBuffer(8);
                    let illegal = false;
                    try {{
                      BaseAudioContext.prototype.decodeAudioData.call({{}}, illegalInput);
                    }} catch (error) {{ illegal = error instanceof TypeError; }}
                    if (!illegal || illegalInput.byteLength !== 8) {{
                      throw new Error("decodeAudioData brand check mutated its input");
                    }}

                    const callbackTypeInput = new ArrayBuffer(8);
                    let threw = false;
                    try {{
                      context.decodeAudioData(callbackTypeInput, 1);
                    }} catch (error) {{
                      threw = error instanceof TypeError;
                    }}
                    if (!threw || callbackTypeInput.byteLength !== 8) {{
                      throw new Error("callback WebIDL validation ran after input detachment");
                    }}

                    const detached = new ArrayBuffer(8);
                    detached.transferToFixedLength();
                    let detachedCallback;
                    const detachedPromise = context.decodeAudioData(
                      detached,
                      null,
                      (error) => {{ detachedCallback = error; }},
                    );
                    let detachedError;
                    try {{ await detachedPromise; }} catch (error) {{ detachedError = error; }}
                    await Promise.resolve();
                    if (detachedError?.name !== "DataCloneError" ||
                        detachedCallback !== detachedError) {{
                      throw new Error("detached input did not reject through both APIs");
                    }}

                    const corrupt = new Uint8Array([1, 2, 3, 4]).buffer;
                    let corruptCallback;
                    let corruptError;
                    try {{
                      await context.decodeAudioData(
                        corrupt,
                        undefined,
                        (error) => {{ corruptCallback = error; }},
                      );
                    }} catch (error) {{ corruptError = error; }}
                    await Promise.resolve();
                    if (corrupt.byteLength !== 0 || corruptError?.name !== "EncodingError" ||
                        corruptCallback !== corruptError) {{
                      throw new Error("corrupt input did not report EncodingError exactly once");
                    }}

                    const limited = new Uint8Array([{}]).buffer;
                    let limitError;
                    try {{ await context.decodeAudioData(limited); }}
                    catch (error) {{ limitError = error; }}
                    if (limited.byteLength !== 0 || limitError?.name !== "QuotaExceededError") {{
                      throw new Error("configured input limit was not a detached promise rejection");
                    }}
                  }})();
                "#,
                byte_literal(&wav),
            ),
        )
        .expect("public decode error scenario should start");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("public decode errors should settle");
    drop(runtime);
    wait_until(|| {
        let usage = host.usage();
        usage.live_audio_bytes() == 0 && usage.decode_jobs() == 0
    });
}
