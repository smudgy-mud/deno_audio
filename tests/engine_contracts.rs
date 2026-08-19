//! Acceptance evidence for the selected native engine's Phase 0B contracts.

use std::error::Error;
use std::fmt;
use std::io::Cursor;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use web_audio_api::context::{
    AudioContext, AudioContextOptions, AudioContextShutdownMode, AudioContextShutdownOutcome,
    AudioContextState, AudioContextStateChangeOutcome, BaseAudioContext, OfflineAudioContext,
};
use web_audio_api::node::{AudioNode, AudioScheduledSourceNode, AudioScheduledSourceNodeExt};
use web_audio_api::output::SystemAudioOutput;
use web_audio_api::{
    BaseAudioContextDecodeBudgetExt, BudgetedDecodeError, DecodeBudget, DecodeBudgetError,
    DecodeBudgetKind, DecodeBudgetRequest, DecodeBudgetReservation,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn legacy_silent_context() -> AudioContext {
    AudioContext::try_new(AudioContextOptions {
        sample_rate: Some(48_000.0),
        sink_id: "none".to_owned(),
        ..AudioContextOptions::default()
    })
    .expect("the legacy silent endpoint should remain available")
}

fn hosted_silent_builder() -> web_audio_api::context::AudioContextBuilder {
    AudioContext::builder(Arc::new(SystemAudioOutput::new()))
        .options(AudioContextOptions {
            sample_rate: Some(48_000.0),
            sink_id: "none".to_owned(),
            ..AudioContextOptions::default()
        })
        .number_of_channels(2)
}

fn wait_until(predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for engine state"
        );
        thread::yield_now();
    }
}

#[test]
fn completion_tokens_survive_a_saturated_event_consumer() {
    const SOURCE_COUNT: usize = 320;

    let context = legacy_silent_context();
    wait_until(|| context.state() == AudioContextState::Running);

    let event_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (blocked_tx, blocked_rx) = mpsc::sync_channel(1);
    let mut sentinel = context.create_oscillator();
    sentinel.connect(&context.destination());
    let callback_gate = Arc::clone(&event_gate);
    sentinel.set_onended(move |_| {
        let (lock, wake) = &*callback_gate;
        let mut released = lock.lock().expect("event gate poisoned");
        blocked_tx.send(()).expect("test waiter disappeared");
        while !*released {
            released = wake.wait(released).expect("event gate poisoned");
        }
    });
    sentinel.start();
    sentinel.stop_at(context.current_time() + 0.03);
    blocked_rx
        .recv_timeout(TEST_TIMEOUT)
        .expect("sentinel must block the event consumer");

    let delivered = Arc::new(AtomicUsize::new(0));
    let mut sources = Vec::with_capacity(SOURCE_COUNT);
    let mut completions = Vec::with_capacity(SOURCE_COUNT);
    for _ in 0..SOURCE_COUNT {
        let mut source = context.create_oscillator();
        source.connect(&context.destination());
        let delivered = Arc::clone(&delivered);
        source.set_onended(move |_| {
            delivered.fetch_add(1, Ordering::AcqRel);
        });
        source.start();
        completions.push(source.completion_token());
        sources.push(source);
    }

    let stop_time = context.current_time() + 0.08;
    for source in &mut sources {
        source.stop_at(stop_time);
    }
    wait_until(|| {
        completions
            .iter()
            .all(web_audio_api::node::ScheduledSourceCompletionToken::is_complete)
    });

    assert_eq!(
        delivered.load(Ordering::Acquire),
        0,
        "native completion must not depend on the blocked event consumer"
    );

    let (lock, wake) = &*event_gate;
    *lock.lock().expect("event gate poisoned") = true;
    wake.notify_all();
    context.close_sync();
    drop(sentinel);
    drop(sources);
}

#[test]
fn hosted_context_drop_has_a_confirmed_silent_shutdown() {
    let context = hosted_silent_builder()
        .build()
        .expect("hosted silent context should build");
    let surviving_gain = context.create_gain();
    let shutdown = context
        .shutdown_receipt()
        .expect("hosted context exposes its shutdown observation");

    drop(context);

    let AudioContextShutdownOutcome::Confirmed(report) = shutdown.wait() else {
        panic!("hosted context Drop must autonomously confirm retirement");
    };
    assert_eq!(report.mode(), AudioContextShutdownMode::Silent);
    assert_eq!(surviving_gain.context().state(), AudioContextState::Closed);
}

#[test]
fn suspended_hosted_control_is_bounded_and_recovers() {
    let context = hosted_silent_builder()
        .initially_suspended(true)
        .build()
        .expect("hosted silent context should build suspended");
    assert_eq!(context.state(), AudioContextState::Suspended);

    let mut gains = Vec::new();
    let rejection = loop {
        let attempt = panic::catch_unwind(AssertUnwindSafe(|| context.create_gain()));
        match attempt {
            Ok(gain) => {
                gains.push(gain);
                assert!(
                    gains.len() <= 256,
                    "suspended graph traffic grew beyond its fixed node registry"
                );
            }
            Err(rejection) => break rejection,
        }
    };
    assert!(!gains.is_empty());
    assert_eq!(context.state(), AudioContextState::Suspended);
    assert!(
        panic_text(&*rejection).contains("LogicalCommandCredits"),
        "bounded rejection should identify the exhausted fixed command budget"
    );

    assert_eq!(
        context
            .request_resume()
            .expect("resume admission should remain available")
            .wait(),
        AudioContextStateChangeOutcome::Applied
    );
    assert_eq!(context.state(), AudioContextState::Running);
    let AudioContextShutdownOutcome::Confirmed(_) = context
        .request_close()
        .expect("close admission must survive bounded graph rejection")
        .wait()
    else {
        panic!("accepted suspended graph must retire cleanly");
    };
    drop(gains);
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("")
}

#[derive(Debug)]
struct DeliberateBudgetRefusal;

impl fmt::Display for DeliberateBudgetRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("deliberate resampling budget refusal")
    }
}

impl Error for DeliberateBudgetRefusal {}

struct ActiveReservation {
    active: Arc<AtomicUsize>,
}

impl DecodeBudgetReservation for ActiveReservation {
    fn shrink_to(&mut self, _amount: u64) {}
}

impl Drop for ActiveReservation {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct RejectResamplingBudget {
    active: Arc<AtomicUsize>,
    requests: Mutex<Vec<DecodeBudgetRequest>>,
}

impl DecodeBudget for RejectResamplingBudget {
    fn try_reserve(
        &self,
        request: DecodeBudgetRequest,
    ) -> Result<Box<dyn DecodeBudgetReservation>, DecodeBudgetError> {
        self.requests
            .lock()
            .expect("request log poisoned")
            .push(request);
        if request.kind() == DecodeBudgetKind::ResamplePcmBytes {
            return Err(Box::new(DeliberateBudgetRefusal));
        }

        self.active.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(ActiveReservation {
            active: Arc::clone(&self.active),
        }))
    }
}

#[test]
fn decode_resampling_reserves_output_before_growth() {
    const SOURCE_RATE: u32 = 3_000;
    const TARGET_RATE: f32 = 768_000.0;
    const TARGET_FRAMES: u64 = 768_000;
    const TARGET_BYTES: u64 = TARGET_FRAMES * size_of::<f32>() as u64;

    let encoded = mono_pcm16_wav(SOURCE_RATE, SOURCE_RATE);
    let context = OfflineAudioContext::new(1, 128, TARGET_RATE);
    let budget = Arc::new(RejectResamplingBudget::default());
    let result = context.decode_audio_data_sync_with_budget(
        Cursor::new(encoded),
        Arc::clone(&budget) as Arc<dyn DecodeBudget>,
    );

    let BudgetedDecodeError::BudgetRefused { request, .. } =
        result.expect_err("resampling output must be reserved before allocation")
    else {
        panic!("decode failed for a reason other than the configured output budget");
    };
    assert_eq!(request.kind(), DecodeBudgetKind::ResamplePcmBytes);
    assert_eq!(request.amount(), TARGET_BYTES);
    assert_eq!(
        budget.active.load(Ordering::Acquire),
        0,
        "every earlier decode reservation must unwind on refusal"
    );
    assert!(budget
        .requests
        .lock()
        .expect("request log poisoned")
        .iter()
        .any(|request| request.kind() == DecodeBudgetKind::CanonicalPcmBytes));
}

fn mono_pcm16_wav(sample_rate: u32, frames: u32) -> Vec<u8> {
    let data_len = frames * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0);
    wav
}
