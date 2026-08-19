use web_audio_api::node::{AudioDestinationNode, AudioNode, GainNode, OscillatorNode};

/// Type-erased storage for the concrete native node families in the Phase 0
/// Web Audio slice.
///
/// The upstream `AudioNode` trait is object safe, but an enum remains the
/// ownership boundary because JavaScript wrappers must retain the concrete
/// node value. Adding a node family therefore requires an explicit variant
/// and cannot turn into an unchecked native pointer or an open-ended `Any`
/// downcast.
#[allow(
    dead_code,
    reason = "Phase 0C proves the private representation before installing Web IDL wrappers"
)]
pub(crate) enum NativeAudioNode {
    Destination(Box<AudioDestinationNode>),
    Gain(Box<GainNode>),
    Oscillator(Box<OscillatorNode>),
}

#[allow(
    dead_code,
    reason = "Phase 0C proves the private representation before installing Web IDL wrappers"
)]
impl NativeAudioNode {
    pub(crate) fn as_audio_node(&self) -> &dyn AudioNode {
        match self {
            Self::Destination(node) => node.as_ref(),
            Self::Gain(node) => node.as_ref(),
            Self::Oscillator(node) => node.as_ref(),
        }
    }

    pub(crate) fn connect(&self, destination: &Self) {
        self.as_audio_node().connect(destination.as_audio_node());
    }
}

impl From<AudioDestinationNode> for NativeAudioNode {
    fn from(value: AudioDestinationNode) -> Self {
        Self::Destination(Box::new(value))
    }
}

impl From<GainNode> for NativeAudioNode {
    fn from(value: GainNode) -> Self {
        Self::Gain(Box::new(value))
    }
}

impl From<OscillatorNode> for NativeAudioNode {
    fn from(value: OscillatorNode) -> Self {
        Self::Oscillator(Box::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use web_audio_api::context::{BaseAudioContext, OfflineAudioContext};
    use web_audio_api::node::AudioScheduledSourceNode;
    use web_audio_api::AudioBuffer;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn erased_concrete_nodes_connect_and_render_through_dyn_audio_node() {
        assert_send_sync::<AudioBuffer>();
        assert_send_sync::<OfflineAudioContext>();
        assert_send_sync::<GainNode>();
        assert_send_sync::<OscillatorNode>();
        assert_send_sync::<NativeAudioNode>();

        let mut context = OfflineAudioContext::new(1, 128, 48_000.0);
        let mut oscillator = context.create_oscillator();
        oscillator.frequency().set_value(440.0);
        oscillator.start();

        let gain = context.create_gain();
        gain.gain().set_value(0.5);

        let oscillator = NativeAudioNode::from(oscillator);
        let gain = NativeAudioNode::from(gain);
        let destination = NativeAudioNode::from(context.destination());

        oscillator.connect(&gain);
        gain.connect(&destination);

        assert_eq!(oscillator.as_audio_node().number_of_inputs(), 0);
        assert_eq!(gain.as_audio_node().number_of_inputs(), 1);
        assert_eq!(destination.as_audio_node().number_of_outputs(), 1);

        let rendered = context.start_rendering_sync();
        assert!(
            rendered
                .get_channel_data(0)
                .iter()
                .any(|sample| sample.abs() > 0.01),
            "the erased oscillator -> gain -> destination graph must render"
        );
    }
}
