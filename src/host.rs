use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Shared ceilings for every isolate using one [`AudioHost`].
///
/// A missing ceiling is explicitly unlimited. The type is non-exhaustive so
/// later resource families can be added without turning the host into a policy
/// or communication object.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
#[allow(
    clippy::struct_field_names,
    reason = "the private max_ prefix keeps every non-exhaustive limit unambiguous"
)]
pub struct AudioHostLimits {
    max_online_contexts: Option<usize>,
    max_live_audio_bytes: Option<usize>,
    max_graph_nodes: Option<usize>,
    max_graph_connections: Option<usize>,
    max_scheduled_sources: Option<usize>,
    max_automation_events: Option<usize>,
    max_queued_control_commands: Option<usize>,
    max_queued_events: Option<usize>,
    max_decode_jobs: Option<usize>,
    max_offline_render_jobs: Option<usize>,
}

impl AudioHostLimits {
    /// Creates an explicitly unlimited shared budget.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_online_contexts: None,
            max_live_audio_bytes: None,
            max_graph_nodes: None,
            max_graph_connections: None,
            max_scheduled_sources: None,
            max_automation_events: None,
            max_queued_control_commands: None,
            max_queued_events: None,
            max_decode_jobs: None,
            max_offline_render_jobs: None,
        }
    }

    /// Sets the maximum number of concurrently owned online contexts.
    #[must_use]
    pub const fn max_online_contexts(mut self, limit: Option<usize>) -> Self {
        self.max_online_contexts = limit;
        self
    }

    /// Sets the aggregate wrapper-owned live audio-byte ceiling.
    #[must_use]
    pub const fn max_live_audio_bytes(mut self, limit: Option<usize>) -> Self {
        self.max_live_audio_bytes = limit;
        self
    }

    /// Sets the aggregate live graph-node ceiling.
    #[must_use]
    pub const fn max_graph_nodes(mut self, limit: Option<usize>) -> Self {
        self.max_graph_nodes = limit;
        self
    }

    /// Sets the aggregate live explicit-connection ceiling.
    #[must_use]
    pub const fn max_graph_connections(mut self, limit: Option<usize>) -> Self {
        self.max_graph_connections = limit;
        self
    }

    /// Sets the aggregate live scheduled-source ceiling.
    #[must_use]
    pub const fn max_scheduled_sources(mut self, limit: Option<usize>) -> Self {
        self.max_scheduled_sources = limit;
        self
    }

    /// Sets the aggregate queued automation-event ceiling.
    #[must_use]
    pub const fn max_automation_events(mut self, limit: Option<usize>) -> Self {
        self.max_automation_events = limit;
        self
    }

    /// Sets the aggregate queued graph-control command ceiling.
    #[must_use]
    pub const fn max_queued_control_commands(mut self, limit: Option<usize>) -> Self {
        self.max_queued_control_commands = limit;
        self
    }

    /// Sets the aggregate queued native-event ceiling.
    #[must_use]
    pub const fn max_queued_events(mut self, limit: Option<usize>) -> Self {
        self.max_queued_events = limit;
        self
    }

    /// Sets the aggregate concurrent decode-job ceiling.
    #[must_use]
    pub const fn max_decode_jobs(mut self, limit: Option<usize>) -> Self {
        self.max_decode_jobs = limit;
        self
    }

    /// Sets the aggregate concurrent offline-render-job ceiling.
    #[must_use]
    pub const fn max_offline_render_jobs(mut self, limit: Option<usize>) -> Self {
        self.max_offline_render_jobs = limit;
        self
    }
}

/// Aggregate resource family accounted by [`AudioHost`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AudioHostResourceKind {
    /// Open online audio contexts.
    OnlineContexts,
    /// Wrapper-owned encoded and PCM audio bytes.
    LiveAudioBytes,
    /// Live graph nodes.
    GraphNodes,
    /// Live explicit graph connections.
    GraphConnections,
    /// Live scheduled sources.
    ScheduledSources,
    /// Queued automation events.
    AutomationEvents,
    /// Queued graph-control commands.
    QueuedControlCommands,
    /// Queued native events.
    QueuedEvents,
    /// Concurrent decode jobs.
    DecodeJobs,
    /// Concurrent offline-render jobs.
    OfflineRenderJobs,
}

/// Immediate failure to reserve a shared audio resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioHostLimitError {
    resource: AudioHostResourceKind,
    current: usize,
    requested: usize,
    limit: usize,
}

impl AudioHostLimitError {
    /// Resource family whose ceiling rejected the reservation.
    #[must_use]
    pub const fn resource(&self) -> AudioHostResourceKind {
        self.resource
    }

    /// Amount already reserved when the request was rejected.
    #[must_use]
    pub const fn current(&self) -> usize {
        self.current
    }

    /// Additional amount requested.
    #[must_use]
    pub const fn requested(&self) -> usize {
        self.requested
    }

    /// Configured ceiling.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

impl fmt::Display for AudioHostLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "shared audio {:?} limit {} exceeded by reservation of {} at {}",
            self.resource, self.limit, self.requested, self.current
        )
    }
}

impl Error for AudioHostLimitError {}

/// Point-in-time aggregate usage for one [`AudioHost`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct AudioHostUsage {
    online_contexts: usize,
    live_audio_bytes: usize,
    graph_nodes: usize,
    graph_connections: usize,
    scheduled_sources: usize,
    automation_events: usize,
    queued_control_commands: usize,
    queued_events: usize,
    decode_jobs: usize,
    offline_render_jobs: usize,
}

impl AudioHostUsage {
    /// Number of online contexts whose output lifetime is still reserved.
    #[must_use]
    pub const fn online_contexts(self) -> usize {
        self.online_contexts
    }

    /// Wrapper-owned encoded and PCM audio bytes.
    #[must_use]
    pub const fn live_audio_bytes(self) -> usize {
        self.live_audio_bytes
    }

    /// Live graph nodes.
    #[must_use]
    pub const fn graph_nodes(self) -> usize {
        self.graph_nodes
    }

    /// Live explicit graph connections.
    #[must_use]
    pub const fn graph_connections(self) -> usize {
        self.graph_connections
    }

    /// Live scheduled sources.
    #[must_use]
    pub const fn scheduled_sources(self) -> usize {
        self.scheduled_sources
    }

    /// Queued automation events.
    #[must_use]
    pub const fn automation_events(self) -> usize {
        self.automation_events
    }

    /// Queued graph-control commands.
    #[must_use]
    pub const fn queued_control_commands(self) -> usize {
        self.queued_control_commands
    }

    /// Queued native events.
    #[must_use]
    pub const fn queued_events(self) -> usize {
        self.queued_events
    }

    /// Concurrent decode jobs.
    #[must_use]
    pub const fn decode_jobs(self) -> usize {
        self.decode_jobs
    }

    /// Concurrent offline-render jobs.
    #[must_use]
    pub const fn offline_render_jobs(self) -> usize {
        self.offline_render_jobs
    }
}

#[derive(Debug, Default)]
struct AudioHostCounters {
    online_contexts: AtomicUsize,
    live_audio_bytes: AtomicUsize,
    graph_nodes: AtomicUsize,
    graph_connections: AtomicUsize,
    scheduled_sources: AtomicUsize,
    automation_events: AtomicUsize,
    queued_control_commands: AtomicUsize,
    queued_events: AtomicUsize,
    decode_jobs: AtomicUsize,
    offline_render_jobs: AtomicUsize,
}

/// Shared, nonblocking aggregate accounting for mutually independent isolates.
///
/// Sharing one `Arc<AudioHost>` defines the accounting domain. This type owns
/// no context handle, output endpoint, permission policy, or cross-isolate
/// communication channel.
#[derive(Debug)]
pub struct AudioHost {
    limits: AudioHostLimits,
    counters: AudioHostCounters,
}

impl Default for AudioHost {
    fn default() -> Self {
        Self::new(AudioHostLimits::default())
    }
}

impl AudioHost {
    /// Constructs a shared accounting domain.
    #[must_use]
    pub fn new(limits: AudioHostLimits) -> Self {
        Self {
            limits,
            counters: AudioHostCounters::default(),
        }
    }

    /// Returns a lock-free point-in-time usage snapshot.
    #[must_use]
    pub fn usage(&self) -> AudioHostUsage {
        AudioHostUsage {
            online_contexts: self.counters.online_contexts.load(Ordering::Acquire),
            live_audio_bytes: self.counters.live_audio_bytes.load(Ordering::Acquire),
            graph_nodes: self.counters.graph_nodes.load(Ordering::Acquire),
            graph_connections: self.counters.graph_connections.load(Ordering::Acquire),
            scheduled_sources: self.counters.scheduled_sources.load(Ordering::Acquire),
            automation_events: self.counters.automation_events.load(Ordering::Acquire),
            queued_control_commands: self
                .counters
                .queued_control_commands
                .load(Ordering::Acquire),
            queued_events: self.counters.queued_events.load(Ordering::Acquire),
            decode_jobs: self.counters.decode_jobs.load(Ordering::Acquire),
            offline_render_jobs: self.counters.offline_render_jobs.load(Ordering::Acquire),
        }
    }

    pub(crate) fn try_reserve_online_context(
        self: &Arc<Self>,
    ) -> Result<AudioHostOnlineContextPermit, AudioHostLimitError> {
        let reservation = self.try_reserve(AudioHostResourceKind::OnlineContexts, 1)?;
        Ok(AudioHostOnlineContextPermit {
            reservation: Some(reservation),
            committed: false,
        })
    }

    pub(crate) fn try_reserve(
        self: &Arc<Self>,
        resource: AudioHostResourceKind,
        amount: usize,
    ) -> Result<AudioHostReservation, AudioHostLimitError> {
        let (counter, limit) = self.counter_and_limit(resource);
        reserve_atomic(counter, limit, resource, amount)?;
        Ok(AudioHostReservation {
            host: Arc::clone(self),
            resource,
            amount,
            release: true,
        })
    }

    fn counter_and_limit(&self, resource: AudioHostResourceKind) -> (&AtomicUsize, Option<usize>) {
        match resource {
            AudioHostResourceKind::OnlineContexts => (
                &self.counters.online_contexts,
                self.limits.max_online_contexts,
            ),
            AudioHostResourceKind::LiveAudioBytes => (
                &self.counters.live_audio_bytes,
                self.limits.max_live_audio_bytes,
            ),
            AudioHostResourceKind::GraphNodes => {
                (&self.counters.graph_nodes, self.limits.max_graph_nodes)
            }
            AudioHostResourceKind::GraphConnections => (
                &self.counters.graph_connections,
                self.limits.max_graph_connections,
            ),
            AudioHostResourceKind::ScheduledSources => (
                &self.counters.scheduled_sources,
                self.limits.max_scheduled_sources,
            ),
            AudioHostResourceKind::AutomationEvents => (
                &self.counters.automation_events,
                self.limits.max_automation_events,
            ),
            AudioHostResourceKind::QueuedControlCommands => (
                &self.counters.queued_control_commands,
                self.limits.max_queued_control_commands,
            ),
            AudioHostResourceKind::QueuedEvents => {
                (&self.counters.queued_events, self.limits.max_queued_events)
            }
            AudioHostResourceKind::DecodeJobs => {
                (&self.counters.decode_jobs, self.limits.max_decode_jobs)
            }
            AudioHostResourceKind::OfflineRenderJobs => (
                &self.counters.offline_render_jobs,
                self.limits.max_offline_render_jobs,
            ),
        }
    }
}

fn reserve_atomic(
    counter: &AtomicUsize,
    limit: Option<usize>,
    resource: AudioHostResourceKind,
    amount: usize,
) -> Result<(), AudioHostLimitError> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount) else {
            return Err(AudioHostLimitError {
                resource,
                current,
                requested: amount,
                limit: limit.unwrap_or(usize::MAX),
            });
        };
        if limit.is_some_and(|limit| next > limit) {
            return Err(AudioHostLimitError {
                resource,
                current,
                requested: amount,
                limit: limit.expect("checked limit is present"),
            });
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

pub(crate) struct AudioHostReservation {
    host: Arc<AudioHost>,
    resource: AudioHostResourceKind,
    amount: usize,
    release: bool,
}

impl AudioHostReservation {
    pub(crate) fn shrink_to(&mut self, amount: usize) {
        if amount > self.amount {
            return;
        }
        let released = self.amount - amount;
        self.amount = amount;
        if self.release {
            let (counter, _) = self.host.counter_and_limit(self.resource);
            release_atomic(counter, released);
        }
    }

    fn abandon_without_release(mut self) {
        self.release = false;
    }
}

impl Drop for AudioHostReservation {
    fn drop(&mut self) {
        if self.release {
            let (counter, _) = self.host.counter_and_limit(self.resource);
            release_atomic(counter, self.amount);
        }
    }
}

pub(crate) fn release_atomic(counter: &AtomicUsize, amount: usize) {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_sub(amount) else {
            // Accounting corruption must fail closed rather than wrap the
            // counter or panic from a resource-guard destructor.
            return;
        };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

pub(crate) struct AudioHostOnlineContextPermit {
    reservation: Option<AudioHostReservation>,
    committed: bool,
}

impl AudioHostOnlineContextPermit {
    pub(crate) fn commit(&mut self) {
        self.committed = true;
    }

    pub(crate) fn release_confirmed(mut self) {
        self.committed = false;
        drop(self.reservation.take());
    }
}

impl Drop for AudioHostOnlineContextPermit {
    fn drop(&mut self) {
        if self.committed {
            if let Some(reservation) = self.reservation.take() {
                reservation.abandon_without_release();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_permit_is_cancelable_then_fail_closed_after_commit() {
        let host = Arc::new(AudioHost::new(
            AudioHostLimits::default().max_online_contexts(Some(1)),
        ));

        drop(host.try_reserve_online_context().unwrap());
        assert_eq!(host.usage().online_contexts(), 0);

        let mut permit = host.try_reserve_online_context().unwrap();
        permit.commit();
        drop(permit);
        assert_eq!(host.usage().online_contexts(), 1);
        assert!(host.try_reserve_online_context().is_err());
    }

    #[test]
    fn confirmed_release_and_other_resource_reservations_are_exact() {
        let host = Arc::new(AudioHost::new(
            AudioHostLimits::default()
                .max_online_contexts(Some(1))
                .max_live_audio_bytes(Some(8)),
        ));
        let mut permit = host.try_reserve_online_context().unwrap();
        permit.commit();
        let bytes = host
            .try_reserve(AudioHostResourceKind::LiveAudioBytes, 8)
            .unwrap();
        assert_eq!(host.usage().online_contexts(), 1);
        assert_eq!(host.usage().live_audio_bytes(), 8);
        assert!(host
            .try_reserve(AudioHostResourceKind::LiveAudioBytes, 1)
            .is_err());

        drop(bytes);
        permit.release_confirmed();
        assert_eq!(host.usage(), AudioHostUsage::default());
    }
}
