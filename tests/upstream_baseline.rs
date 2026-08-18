//! Executable characterizations of release-blocking `web-audio-api` 1.7.0
//! behavior. These tests deliberately assert the defective baseline. Phase 0B
//! must replace or invert them when a production engine is selected.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use web_audio_api::context::{
    AudioContext, AudioContextOptions, AudioContextState, BaseAudioContext, OfflineAudioContext,
};
use web_audio_api::node::{AudioNode, AudioScheduledSourceNode, GainNode};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5);
const BASELINE_EVENT_CHANNEL_CAPACITY: usize = 256;

fn silent_context() -> AudioContext {
    AudioContext::try_new(AudioContextOptions {
        sample_rate: Some(48_000.0),
        sink_id: "none".to_owned(),
        ..Default::default()
    })
    .expect("the silent backend should always be available")
}

fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    predicate()
}

#[test]
fn terminal_events_are_lost_when_the_bounded_queue_saturates() {
    const BURST_SOURCE_COUNT: usize = 640;

    let context = silent_context();
    assert!(wait_until(Duration::from_secs(2), || {
        context.state() == AudioContextState::Running
    }));

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let sentinel_observed = Arc::new(AtomicUsize::new(0));
    let (sentinel_blocked_tx, sentinel_blocked_rx) = mpsc::sync_channel(1);
    let mut sentinel = context.create_oscillator();
    sentinel.connect(&context.destination());

    let gate_for_sentinel = Arc::clone(&gate);
    let sentinel_observed_for_callback = Arc::clone(&sentinel_observed);
    sentinel.set_onended(move |_| {
        sentinel_observed_for_callback.fetch_add(1, Ordering::SeqCst);

        let (lock, wake) = &*gate_for_sentinel;
        let mut released = lock.lock().expect("event gate poisoned");
        sentinel_blocked_tx
            .send(())
            .expect("the test should still be waiting for the sentinel");
        while !*released {
            let (guard, timeout) = wake
                .wait_timeout(released, CALLBACK_TIMEOUT)
                .expect("event gate poisoned while waiting");
            released = guard;
            if timeout.timed_out() {
                break;
            }
        }
    });
    sentinel.start();
    sentinel.stop_at(context.current_time() + 0.05);

    sentinel_blocked_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the sentinel callback should reach its closed gate");
    {
        // The callback sends while holding this mutex and releases it only by
        // entering Condvar::wait_timeout. Acquiring it here proves the event
        // thread is inside the false-gated wait before the burst is scheduled.
        let (lock, _) = &*gate;
        let released = lock.lock().expect("event gate poisoned");
        assert!(!*released, "the sentinel gate must still be closed");
    }

    let burst_observed = Arc::new(AtomicUsize::new(0));
    let mut sources = Vec::with_capacity(BURST_SOURCE_COUNT);

    for _ in 0..BURST_SOURCE_COUNT {
        let mut source = context.create_oscillator();
        source.connect(&context.destination());

        let burst_observed_for_callback = Arc::clone(&burst_observed);
        source.set_onended(move |_| {
            burst_observed_for_callback.fetch_add(1, Ordering::SeqCst);
        });

        source.start();
        sources.push(source);
    }

    // Give every source the same stop time after setup is complete. This makes
    // it possible to prove that every terminal event has been emitted by the
    // render thread before the blocked event thread is released.
    let stop_time = context.current_time() + 0.1;
    for source in &mut sources {
        source.stop_at(stop_time);
    }

    // The sentinel holds the event thread while the render thread emits every
    // burst event into its 256-record lossy queue. Waiting on the render clock
    // avoids treating a slow machine as evidence of loss.
    assert!(
        wait_until(Duration::from_secs(2), || {
            context.current_time() >= stop_time + 0.05
        }),
        "the silent render clock did not advance past every scheduled stop"
    );
    let (lock, wake) = &*gate;
    *lock.lock().expect("event gate poisoned") = true;
    wake.notify_all();

    let expected_burst_delivered = BASELINE_EVENT_CHANNEL_CAPACITY;
    assert!(
        wait_until(Duration::from_secs(2), || {
            burst_observed.load(Ordering::SeqCst) == expected_burst_delivered
        }),
        "the saturated event queue should deliver exactly {expected_burst_delivered} burst events"
    );
    let sentinel_delivered = sentinel_observed.load(Ordering::SeqCst);
    let burst_delivered = burst_observed.load(Ordering::SeqCst);

    assert_eq!(sentinel_delivered, 1);
    assert_eq!(burst_delivered, expected_burst_delivered);
    assert_eq!(BURST_SOURCE_COUNT - burst_delivered, 384);

    context.close_sync();
    drop(sentinel);
    drop(sources);
}

#[test]
fn dropping_a_running_context_keeps_its_silent_backend_alive() {
    let context = silent_context();
    assert!(wait_until(Duration::from_secs(2), || {
        context.state() == AudioContextState::Running
    }));

    let (ended_tx, ended_rx) = mpsc::sync_channel(1);
    let mut source = context.create_oscillator();
    source.connect(&context.destination());
    source.set_onended(move |_| {
        let _ = ended_tx.send(Instant::now());
    });
    source.start();
    let stop_time = context.current_time() + 0.5;
    source.stop_at(stop_time);

    let remaining = stop_time - context.current_time();
    assert!(
        remaining >= 0.4,
        "the scheduled event must still be in the future before AudioContext::drop"
    );
    assert_eq!(ended_rx.try_recv(), Err(mpsc::TryRecvError::Empty));

    drop(context);
    let drop_completed = Instant::now();

    let callback_time = ended_rx.recv_timeout(Duration::from_secs(2)).expect(
        "the post-Drop callback proves 1.7.0 leaked the running silent backend and render thread",
    );
    let elapsed_after_drop = callback_time.checked_duration_since(drop_completed).expect(
        "the ended callback ran before AudioContext::drop completed, so it is not leak evidence",
    );
    assert!(
        elapsed_after_drop >= Duration::from_millis(100),
        "the callback arrived too soon after drop to prove the future render continued"
    );
    drop(source);
}

#[test]
fn suspended_control_messages_are_retained_past_the_live_channel_bound() {
    const NODE_COUNT: usize = 2_048;
    const REGISTRATIONS_PER_GAIN: usize = 2;

    let context = silent_context();
    assert!(wait_until(Duration::from_secs(2), || {
        context.state() == AudioContextState::Running
    }));
    let baseline_node_count = diagnostics_node_count(&context);

    context.suspend_sync();
    assert_eq!(context.state(), AudioContextState::Suspended);

    let started = Instant::now();
    let nodes: Vec<GainNode> = (0..NODE_COUNT).map(|_| context.create_gain()).collect();

    assert_eq!(nodes.len(), NODE_COUNT);
    assert!(
        started.elapsed() < CALLBACK_TIMEOUT,
        "node creation unexpectedly blocked at the 256-message live-channel bound"
    );

    // Resuming and observing every node in the render graph proves the messages
    // were retained rather than rejected or silently discarded while suspended.
    context.resume_sync();
    assert_eq!(context.state(), AudioContextState::Running);
    assert_eq!(
        diagnostics_node_count(&context),
        baseline_node_count + NODE_COUNT * REGISTRATIONS_PER_GAIN,
        "each GainNode and its AudioParam should survive suspended-message replay"
    );

    context.close_sync();
    drop(nodes);
}

fn diagnostics_node_count(context: &AudioContext) -> usize {
    let (send, recv) = mpsc::sync_channel(1);
    context.run_diagnostics(move |diagnostics| {
        let _ = send.send(diagnostics.graph.node_count);
    });
    recv.recv_timeout(CALLBACK_TIMEOUT)
        .expect("the silent backend should return graph diagnostics")
}

#[test]
fn decode_resampling_can_expand_without_an_output_budget() {
    const SOURCE_RATE: u32 = 3_000;
    const TARGET_RATE: f32 = 768_000.0;
    const TARGET_FRAMES: usize = 768_000;
    const SOURCE_FRAMES: u32 = SOURCE_RATE;

    let encoded = mono_pcm16_wav(SOURCE_RATE, SOURCE_FRAMES);
    let context = OfflineAudioContext::new(1, 128, TARGET_RATE);
    let decoded = context
        .decode_audio_data_sync(std::io::Cursor::new(encoded.clone()))
        .expect("the generated PCM wave should decode");

    let decoded_bytes = decoded.length() * decoded.number_of_channels() * size_of::<f32>();
    assert!((decoded.sample_rate() - TARGET_RATE).abs() < f32::EPSILON);
    assert_eq!(decoded.length(), TARGET_FRAMES);
    assert!(
        decoded_bytes > encoded.len() * 100,
        "expected resampling expansion, got {} input bytes and {decoded_bytes} output bytes",
        encoded.len()
    );
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
