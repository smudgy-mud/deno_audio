//! Phase 0D acceptance evidence for bounded decode ownership and recovery.

use std::sync::Arc;
use std::time::{Duration, Instant};

use deno_audio::{AudioExtensionOptions, AudioHost, AudioHostLimits, AudioLimits};
use deno_core::{JsRuntime, RuntimeOptions};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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
        ..RuntimeOptions::default()
    })
}

fn pcm16_wav(channels: u16, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    assert_ne!(channels, 0);
    assert_eq!(samples.len() % usize::from(channels), 0);
    let data_len = u32::try_from(std::mem::size_of_val(samples)).unwrap();
    let block_align = channels * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
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

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for decode cleanup"
        );
        std::thread::yield_now();
    }
}

async fn assert_decode_rejection(
    limits: AudioLimits,
    encoded: Vec<u8>,
    target_sample_rate: f64,
    expected: &str,
) {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), limits);
    runtime
        .execute_script(
            "phase0d_decode_rejection.js",
            format!(
                r#"
                  globalThis.__decodeRejected = (async () => {{
                    const request = new Deno.core.ops.NativePhase0DecodeRequest(
                      new Uint8Array([{}]),
                      {},
                    );
                    try {{
                      await Deno.core.ops.op_phase0_decode_off_thread(request);
                      throw new Error("decode unexpectedly succeeded");
                    }} catch (error) {{
                      const message = String(error);
                      if (!message.includes({:?})) throw error;
                    }}
                    globalThis.__decodeStillResponsive = 40 + 2;
                  }})();
                "#,
                byte_literal(&encoded),
                target_sample_rate,
                expected,
            ),
        )
        .expect("bounded decode should enter its contained error path");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("decode rejection should settle without killing the isolate");
    runtime
        .execute_script(
            "phase0d_decode_responsive.js",
            "if (globalThis.__decodeStillResponsive !== 42) throw new Error('isolate stalled');",
        )
        .expect("the isolate should remain responsive after decode rejection");
    wait_until(|| {
        let usage = host.usage();
        usage.live_audio_bytes() == 0 && usage.decode_jobs() == 0
    });
}

#[tokio::test(flavor = "current_thread")]
async fn every_decode_dimension_and_pcm_stage_refuses_and_recovers() {
    let mono_four = pcm16_wav(1, 3_000, &[1, 2, 3, 4]);
    let stereo_two = pcm16_wav(2, 3_000, &[1, 2, 3, 4]);

    assert_decode_rejection(
        AudioLimits::unlimited().max_channels(Some(1)),
        stereo_two.clone(),
        3_000.0,
        "DecodedChannels",
    )
    .await;
    assert_decode_rejection(
        AudioLimits::unlimited().max_decoded_frames(Some(3)),
        mono_four.clone(),
        3_000.0,
        "DecodedFrames",
    )
    .await;
    assert_decode_rejection(
        AudioLimits::unlimited().max_decoded_samples(Some(3)),
        stereo_two,
        3_000.0,
        "DecodedSamples",
    )
    .await;

    assert_decode_rejection(
        AudioLimits::unlimited().max_live_audio_bytes(Some(mono_four.len() + 15)),
        mono_four.clone(),
        3_000.0,
        "CanonicalPcmBytes",
    )
    .await;
    assert_decode_rejection(
        AudioLimits::unlimited().max_live_audio_bytes(Some(mono_four.len() + 16)),
        mono_four,
        3_000.0,
        "CanonicalPcmBytes",
    )
    .await;

    let resampled = pcm16_wav(1, 3_000, &[1, 2]);
    let target_bytes = 512 * size_of::<f32>();
    assert_decode_rejection(
        AudioLimits::unlimited().max_live_audio_bytes(Some(
            resampled.len() + 2 * size_of::<f32>() + target_bytes - 1,
        )),
        resampled,
        768_000.0,
        "ResamplePcmBytes",
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn encoded_job_corrupt_and_canceled_requests_release_all_reservations() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_live_audio_bytes(Some(1_024))
            .max_decode_jobs(Some(2)),
    ));
    let mut runtime = runtime(
        Arc::clone(&host),
        AudioLimits::unlimited()
            .max_encoded_bytes(Some(64))
            .max_decode_jobs(Some(1)),
    );

    runtime
        .execute_script(
            "phase0d_decode_outer_limits.js",
            r#"
              let encodedRejected = false;
              try {
                new Deno.core.ops.NativePhase0DecodeRequest(new Uint8Array(65), 48000);
              } catch { encodedRejected = true; }
              if (!encodedRejected) throw new Error("encoded limit did not reject");

              globalThis.__heldDecode = new Deno.core.ops.NativePhase0DecodeRequest(
                new Uint8Array(8),
                48000,
              );
              let jobRejected = false;
              try {
                new Deno.core.ops.NativePhase0DecodeRequest(new Uint8Array(1), 48000);
              } catch { jobRejected = true; }
              if (!jobRejected) throw new Error("decode-job limit did not reject");
            "#,
        )
        .expect("outer decode limits should reject synchronously");
    assert_eq!(host.usage().decode_jobs(), 1);
    assert_eq!(host.usage().live_audio_bytes(), 8);

    runtime
        .execute_script(
            "phase0d_release_held_decode.js",
            "globalThis.__heldDecode = undefined;",
        )
        .unwrap();
    runtime
        .v8_isolate()
        .request_garbage_collection_for_testing(deno_core::v8::GarbageCollectionType::Full);
    wait_until(|| host.usage().decode_jobs() == 0 && host.usage().live_audio_bytes() == 0);

    let valid = pcm16_wav(1, 48_000, &[0, 1, -1, 0]);
    runtime
        .execute_script(
            "phase0d_single_use_decode.js",
            format!(
                r#"
                  globalThis.__singleUseDecode = (async () => {{
                    const request = new Deno.core.ops.NativePhase0DecodeRequest(
                      new Uint8Array([{}]),
                      48000,
                    );
                    const first = Deno.core.ops.op_phase0_decode_off_thread(request);
                    let replayRejected = false;
                    try {{
                      await Deno.core.ops.op_phase0_decode_off_thread(request);
                    }} catch {{ replayRejected = true; }}
                    if (!replayRejected) throw new Error("decode token was replayable");
                    await first;
                  }})();
                "#,
                byte_literal(&valid),
            ),
        )
        .expect("a decode request should transfer its reservations exactly once");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("single-use decode ownership should settle");
    wait_until(|| host.usage().decode_jobs() == 0 && host.usage().live_audio_bytes() == 0);

    runtime
        .execute_script(
            "phase0d_discard_decode_waiter.js",
            format!(
                r"
                  const request = new Deno.core.ops.NativePhase0DecodeRequest(
                    new Uint8Array([{}]),
                    48000,
                  );
                  Deno.core.ops.op_phase0_decode_off_thread(request);
                ",
                byte_literal(&valid),
            ),
        )
        .expect("accepted decode should outlive its discarded promise");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("discarded decode waiter must not cancel native cleanup");
    wait_until(|| host.usage().decode_jobs() == 0 && host.usage().live_audio_bytes() == 0);

    assert_decode_rejection(
        AudioLimits::unlimited(),
        vec![0; 32],
        48_000.0,
        "audio decode failed",
    )
    .await;
}

#[test]
fn shared_encoded_bytes_and_jobs_exhaust_before_copy_and_recover() {
    let job_host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_decode_jobs(Some(1)),
    ));
    let mut first = runtime(Arc::clone(&job_host), AudioLimits::unlimited());
    first
        .execute_script(
            "phase0d_hold_shared_job.js",
            "globalThis.request = new Deno.core.ops.NativePhase0DecodeRequest(new Uint8Array(8), 48000);",
        )
        .unwrap();
    let mut second = runtime(Arc::clone(&job_host), AudioLimits::unlimited());
    second
        .execute_script(
            "phase0d_reject_shared_job.js",
            r#"
              let rejected = false;
              try {
                new Deno.core.ops.NativePhase0DecodeRequest(new Uint8Array(32), 48000);
              } catch { rejected = true; }
              if (!rejected) throw new Error("shared decode job did not reject");
            "#,
        )
        .unwrap();
    assert_eq!(job_host.usage().decode_jobs(), 1);
    assert_eq!(job_host.usage().live_audio_bytes(), 8);
    drop(second);
    drop(first);
    wait_until(|| job_host.usage().decode_jobs() == 0 && job_host.usage().live_audio_bytes() == 0);

    let byte_host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_live_audio_bytes(Some(12)),
    ));
    let mut first = runtime(Arc::clone(&byte_host), AudioLimits::unlimited());
    first
        .execute_script(
            "phase0d_hold_shared_bytes.js",
            "globalThis.request = new Deno.core.ops.NativePhase0DecodeRequest(new Uint8Array(8), 48000);",
        )
        .unwrap();
    let mut second = runtime(Arc::clone(&byte_host), AudioLimits::unlimited());
    second
        .execute_script(
            "phase0d_reject_shared_bytes.js",
            r#"
              let rejected = false;
              try {
                new Deno.core.ops.NativePhase0DecodeRequest(new Uint8Array(5), 48000);
              } catch { rejected = true; }
              if (!rejected) throw new Error("shared encoded bytes did not reject");
            "#,
        )
        .unwrap();
    assert_eq!(byte_host.usage().live_audio_bytes(), 8);
    assert_eq!(byte_host.usage().decode_jobs(), 1);
    drop(second);
    drop(first);
    wait_until(|| {
        byte_host.usage().decode_jobs() == 0 && byte_host.usage().live_audio_bytes() == 0
    });
}
