//! Acceptance evidence for the selected hosted-output contract.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use web_audio_api::context::{AudioContext, AudioContextOptions, BaseAudioContext};
use web_audio_api::node::{AudioNode, AudioScheduledSourceNode, AudioScheduledSourceNodeExt};
use web_audio_api::output::{
    AudioOutputError, AudioOutputErrorKind, AudioOutputFactory, AudioOutputRequest,
    PreparedAudioOutput, SystemAudioOutput,
};

#[derive(Default)]
struct ObservedRejectingFactory {
    calls: AtomicUsize,
}

impl AudioOutputFactory for ObservedRejectingFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        assert_ne!(request.context_id().get(), 0);
        assert_eq!(request.requested_sample_rate(), Some(48_000.));
        assert_eq!(request.number_of_channels(), 2);
        assert_eq!(request.sink_id(), "host-owned-test-output");
        Err(AudioOutputError::new(
            AudioOutputErrorKind::DeviceUnavailable,
            "host rejected the observed request",
        ))
    }
}

#[test]
fn host_output_factory_is_publicly_injectable_before_graph_construction() {
    fn accept_public_factory(_: Arc<dyn AudioOutputFactory>) {}

    let factory = Arc::new(ObservedRejectingFactory::default());
    accept_public_factory(factory.clone());
    let result = AudioContext::builder(factory.clone())
        .options(AudioContextOptions {
            sample_rate: Some(48_000.),
            sink_id: "host-owned-test-output".into(),
            ..AudioContextOptions::default()
        })
        .number_of_channels(2)
        .build();
    let Err(error) = result else {
        panic!("rejecting host factory unexpectedly constructed a context");
    };
    assert_eq!(factory.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        error.output_error().map(AudioOutputError::kind),
        Some(AudioOutputErrorKind::DeviceUnavailable)
    );
}

#[test]
fn public_system_output_renders_the_hosted_exact_graph_without_hardware() {
    let context = AudioContext::builder(Arc::new(SystemAudioOutput::new()))
        .options(AudioContextOptions {
            sample_rate: Some(48_000.),
            sink_id: "none".into(),
            ..AudioContextOptions::default()
        })
        .number_of_channels(2)
        .build()
        .expect("the built-in silent system endpoint should always prepare");

    let gain = context.create_gain();
    gain.gain().set_value(0.25);
    let mut oscillator = context.create_oscillator();
    oscillator.connect(&gain);
    gain.connect(&context.destination());
    oscillator.start();
    oscillator.stop_at(context.current_time() + 0.02);

    let completion = oscillator.completion_token();
    let started = std::time::Instant::now();
    while !completion.is_complete() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "hosted silent output did not render the scheduled oscillator"
        );
        std::thread::yield_now();
    }

    context.close_sync();
}
