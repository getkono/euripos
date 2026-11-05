//! The network monitor: one per process, it keeps the current
//! [`NetworkFacts`] and tells consumers when the network changes.
//!
//! A platform supplies a [`Source`]: a way to read the facts now and a signal
//! that they may have changed. The monitor decides what counts as a change,
//! the same way on every platform. A platform signal is only a reason to look
//! again: the monitor reads the facts once the signals settle, and publishes
//! them only when they differ from what it published last. A new network, or
//! the loss of one, is published only once a second reading confirms it, so a
//! moment in which the platform reports nothing, or a flood of signals that
//! change nothing, never reads as a network change. See
//! `docs/design/network-context.md`.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};

use crate::NetworkFacts;
use crate::facts::NetworkId;
use crate::platform;

/// How long the platform must stay quiet after a signal before the facts are
/// read.
const SETTLE: Duration = Duration::from_millis(50);

/// The longest a stream of signals may delay a reading.
const MAX_SETTLE: Duration = Duration::from_secs(1);

/// How long after a reading that shows a new network, or none, the second
/// reading that confirms it is taken.
const CONFIRM: Duration = Duration::from_millis(500);

/// How long the first caller waits for the first reading.
const STARTUP: Duration = Duration::from_millis(500);

/// What a platform reports about the network.
pub(crate) trait Source {
    /// The facts as the platform reports them now. It may block briefly on
    /// a local system call: the monitor has its thread to itself.
    fn observe(&mut self) -> NetworkFacts;

    /// How many blocks the platform put on this process's traffic and then
    /// ended on the same network, counted from when the source opened, as of
    /// the last [`observe`](Self::observe). Counted by the platform as each
    /// block ends, so a block that begins and ends between two readings is
    /// still counted. Only Android blocks traffic (Doze, App Standby).
    fn resumes(&self) -> u64 {
        0
    }

    /// Resolves when something the facts are read from may have changed.
    ///
    /// Cancel-safe: the monitor drops it to stop waiting.
    fn changed(&mut self) -> impl Future<Output = ()>;
}

/// What the monitor publishes: the facts, and the platform's count of ended
/// blocks ([`Source::resumes`]). The count is internal, so it stays out of
/// [`NetworkFacts`] and the public impls derived on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) facts: NetworkFacts,
    pub(crate) resumes: u64,
}

impl Reading {
    const UNKNOWN: Self = Self {
        facts: NetworkFacts::UNKNOWN,
        resumes: 0,
    };

    fn of<S: Source>(source: &mut S) -> Self {
        let facts = source.observe();
        Self {
            facts,
            resumes: source.resumes(),
        }
    }
}

/// A change between two published readings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the network-keyed pool, #6")
)]
pub(crate) enum NetworkEvent {
    /// Traffic now leaves through another network.
    Changed(NetworkId),
    /// The network is the same, and another fact about it changed.
    FactsChanged,
    /// The network is the same, and the platform stopped blocking this
    /// process's traffic on it (Android's Doze or App Standby ended):
    /// connections that sat idle through the block may be dead.
    Resumed,
    /// No network is left to send traffic through.
    Lost,
}

impl NetworkEvent {
    /// The event that turns `from` into `to`, if they differ.
    #[cfg_attr(not(test), expect(dead_code, reason = "consumed by #6"))]
    ///
    /// A resume is told from the count of ended blocks, not from a blocked
    /// state in the two readings: a block that began and ended between them
    /// leaves the facts as they were.
    pub(crate) fn between(from: &Reading, to: &Reading) -> Option<Self> {
        match (from.facts.network, to.facts.network) {
            (_, Some(network)) if from.facts.network != to.facts.network => {
                Some(Self::Changed(network))
            }
            (Some(_), None) => Some(Self::Lost),
            _ if from.resumes != to.resumes => Some(Self::Resumed),
            _ if from.facts != to.facts => Some(Self::FactsChanged),
            _ => None,
        }
    }
}

/// The process-wide network monitor.
pub(crate) struct NetworkMonitor {
    facts: watch::Receiver<Reading>,
}

impl NetworkMonitor {
    /// The monitor, started on first use. `None` where the platform reports
    /// nothing about the network, or the monitor could not start.
    pub(crate) fn global() -> Option<&'static Self> {
        static MONITOR: OnceLock<Option<NetworkMonitor>> = OnceLock::new();
        MONITOR.get_or_init(Self::start).as_ref()
    }

    /// Start the monitor on a thread of its own, and wait up to [`STARTUP`]
    /// for its first reading.
    ///
    /// The monitor runs on its own current-thread runtime, so it outlives
    /// the runtime of whichever caller started it and needs none.
    fn start() -> Option<Self> {
        if !platform::OBSERVES_NETWORK {
            return None;
        }
        let (tx, rx) = watch::channel(Reading::UNKNOWN);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("euripos-network".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    let Some(source) = platform::network_source().await else {
                        return;
                    };
                    run(source, |reading| {
                        tx.send_replace(reading);
                        // The first reading is in; a caller may have given up waiting.
                        let _ = ready_tx.try_send(());
                    })
                    .await;
                });
            })
            .ok()?;
        // Bounded: a monitor slower than this starts out `Unknown`.
        let _ = ready_rx.recv_timeout(STARTUP);
        Some(Self { facts: rx })
    }

    /// The facts published last.
    pub(crate) fn facts(&self) -> NetworkFacts {
        self.facts.borrow().facts.clone()
    }

    /// The changes published from now on.
    #[cfg_attr(not(test), expect(dead_code, reason = "consumed by #6"))]
    pub(crate) fn changes(&self) -> Changes {
        let mut facts = self.facts.clone();
        let last = facts.borrow_and_update().clone();
        Changes { facts, last }
    }
}

/// The changes a monitor publishes, in order. Changes published faster than
/// they are read are folded into one, from the last reading read to the
/// newest.
#[cfg_attr(not(test), expect(dead_code, reason = "consumed by #6"))]
pub(crate) struct Changes {
    facts: watch::Receiver<Reading>,
    last: Reading,
}

#[cfg_attr(not(test), expect(dead_code, reason = "consumed by #6"))]
impl Changes {
    /// Resolves to the next change, or `None` once the monitor has stopped.
    pub(crate) async fn next(&mut self) -> Option<NetworkEvent> {
        loop {
            self.facts.changed().await.ok()?;
            let next = self.facts.borrow_and_update().clone();
            let event = NetworkEvent::between(&self.last, &next);
            self.last = next;
            if event.is_some() {
                return event;
            }
        }
    }
}

/// What to do with a reading.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Publish this reading.
    Publish(Reading),
    /// Read again after [`CONFIRM`]; publish nothing yet.
    Confirm,
    /// Nothing to publish.
    Hold,
}

/// Decides which readings are published.
///
/// A reading on the network published last is published as soon as any fact,
/// or the count of ended blocks, differs. A reading on another network, or on
/// none, is held until the next reading names the same network, so a reading
/// taken while the platform is mid-change is never published on its own.
struct Settler {
    published: Reading,
    candidate: Option<Reading>,
}

impl Settler {
    fn new(published: Reading) -> Self {
        Self {
            published,
            candidate: None,
        }
    }

    fn step(&mut self, reading: Reading) -> Step {
        if reading.facts.network == self.published.facts.network {
            self.candidate = None;
            if reading == self.published {
                return Step::Hold;
            }
        } else if self
            .candidate
            .take()
            .is_none_or(|candidate| candidate.facts.network != reading.facts.network)
        {
            self.candidate = Some(reading);
            return Step::Confirm;
        }
        self.published = reading.clone();
        Step::Publish(reading)
    }
}

/// Run the monitor over `source`, calling `publish` with the first reading
/// and with every change after it. Never returns.
async fn run<S: Source>(mut source: S, mut publish: impl FnMut(Reading)) {
    let first = Reading::of(&mut source);
    publish(first.clone());
    let mut settler = Settler::new(first);
    let mut confirm_at = None;
    loop {
        if let Some(at) = confirm_at.take() {
            absorb_until(&mut source, at).await;
        } else {
            source.changed().await;
            settle(&mut source).await;
        }
        match settler.step(Reading::of(&mut source)) {
            Step::Publish(reading) => publish(reading),
            Step::Confirm => confirm_at = Some(Instant::now() + CONFIRM),
            Step::Hold => {}
        }
    }
}

/// Wait until `source` has been quiet for [`SETTLE`], or [`MAX_SETTLE`] has
/// passed.
async fn settle<S: Source>(source: &mut S) {
    let deadline = Instant::now() + MAX_SETTLE;
    loop {
        let quiet = (Instant::now() + SETTLE).min(deadline);
        tokio::select! {
            () = source.changed() => {}
            () = sleep_until(quiet) => return,
        }
    }
}

/// Wait until `at`, taking every signal `source` gives meanwhile.
async fn absorb_until<S: Source>(source: &mut S, at: Instant) {
    loop {
        tokio::select! {
            () = source.changed() => {}
            () = sleep_until(at) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio::time::sleep;

    use super::{CONFIRM, MAX_SETTLE, NetworkEvent, Reading, Settler, Source, Step, run};
    use crate::facts::{IpAvailability, Metering, NetworkFacts, NetworkId};

    fn reading(facts: NetworkFacts) -> Reading {
        Reading { facts, resumes: 0 }
    }

    fn on(network: u32) -> Reading {
        reading(NetworkFacts {
            network: Some(NetworkId::from_key(&network)),
            ipv4: IpAvailability::Available,
            ..NetworkFacts::UNKNOWN
        })
    }

    fn metered(network: u32) -> Reading {
        let mut metered = on(network);
        metered.facts.metering = Metering::Metered;
        metered
    }

    fn lost() -> Reading {
        reading(NetworkFacts {
            ipv4: IpAvailability::Unavailable,
            ipv6: IpAvailability::Unavailable,
            ..NetworkFacts::UNKNOWN
        })
    }

    /// `reading` once the platform has ended `resumes` blocks.
    fn resumed(reading: Reading, resumes: u64) -> Reading {
        Reading { resumes, ..reading }
    }

    /// A platform whose readings and signals the test sets.
    #[derive(Clone)]
    struct Fake {
        now: Rc<RefCell<Reading>>,
        readings: Rc<Cell<usize>>,
    }

    struct FakeSource {
        fake: Fake,
        signals: mpsc::UnboundedReceiver<()>,
        resumes: u64,
    }

    impl Source for FakeSource {
        fn observe(&mut self) -> NetworkFacts {
            self.fake.readings.set(self.fake.readings.get() + 1);
            let now = self.fake.now.borrow().clone();
            self.resumes = now.resumes;
            now.facts
        }

        fn resumes(&self) -> u64 {
            self.resumes
        }

        async fn changed(&mut self) {
            if self.signals.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Run the monitor over a fake platform that starts on `initial`, and
    /// return the fake, its signal sender and every published reading.
    fn start(initial: Reading) -> (Fake, mpsc::UnboundedSender<()>, Rc<RefCell<Vec<Reading>>>) {
        let fake = Fake {
            now: Rc::new(RefCell::new(initial)),
            readings: Rc::new(Cell::new(0)),
        };
        let (signal, signals) = mpsc::unbounded_channel();
        let published = Rc::new(RefCell::new(Vec::new()));
        let source = FakeSource {
            fake: fake.clone(),
            signals,
            resumes: 0,
        };
        let sink = Rc::clone(&published);
        tokio::task::spawn_local(run(source, move |reading| sink.borrow_mut().push(reading)));
        (fake, signal, published)
    }

    fn set(fake: &Fake, reading: Reading) {
        *fake.now.borrow_mut() = reading;
    }

    async fn local<F: Future>(test: F) -> F::Output {
        tokio::task::LocalSet::new().run_until(test).await
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_reading_is_published_at_once() {
        local(async {
            let (_fake, _signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            assert_eq!(*published.borrow(), [on(1)]);
        })
        .await;
    }

    // tailscale#17523: the platform briefly reports no interfaces, then the
    // same ones again.
    #[tokio::test(start_paused = true)]
    async fn a_brief_empty_reading_with_a_signal_back_is_not_a_change() {
        local(async {
            let (fake, signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            set(&fake, lost());
            signal.send(()).unwrap();
            sleep(Duration::from_millis(200)).await;
            set(&fake, on(1));
            signal.send(()).unwrap();
            sleep(Duration::from_secs(10)).await;
            assert_eq!(*published.borrow(), [on(1)]);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_brief_empty_reading_with_no_signal_back_is_not_a_change() {
        local(async {
            let (fake, signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            set(&fake, lost());
            signal.send(()).unwrap();
            sleep(Duration::from_millis(100)).await;
            set(&fake, on(1));
            sleep(Duration::from_secs(10)).await;
            assert_eq!(*published.borrow(), [on(1)]);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_brief_other_network_is_not_a_change() {
        local(async {
            let (fake, signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            set(&fake, on(2));
            signal.send(()).unwrap();
            sleep(Duration::from_millis(100)).await;
            set(&fake, on(1));
            sleep(Duration::from_secs(10)).await;
            assert_eq!(*published.borrow(), [on(1)]);
        })
        .await;
    }

    // A flood of route signals, none of which changes the facts.
    #[tokio::test(start_paused = true)]
    async fn a_signal_flood_that_changes_nothing_is_not_a_change() {
        local(async {
            let (fake, signal, published) = start(on(1));
            for _ in 0..1_000 {
                for _ in 0..100 {
                    signal.send(()).unwrap();
                }
                sleep(Duration::from_millis(10)).await;
            }
            sleep(Duration::from_secs(10)).await;
            assert_eq!(*published.borrow(), [on(1)]);
            // 100 000 signals over 10 s are read about once per MAX_SETTLE,
            // not once per signal: the first reading, ten during the flood
            // and one after it, with one to spare.
            assert_eq!(MAX_SETTLE, Duration::from_secs(1));
            assert!(
                fake.readings.get() <= 13,
                "{} readings",
                fake.readings.get()
            );
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_lasting_new_network_is_published_once_confirmed() {
        local(async {
            let (fake, signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            set(&fake, on(2));
            signal.send(()).unwrap();
            sleep(CONFIRM / 2).await;
            assert_eq!(*published.borrow(), [on(1)]);
            sleep(Duration::from_secs(10)).await;
            assert_eq!(*published.borrow(), [on(1), on(2)]);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_lasting_loss_is_published_once_confirmed() {
        local(async {
            let (fake, signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            set(&fake, lost());
            signal.send(()).unwrap();
            sleep(Duration::from_secs(10)).await;
            assert_eq!(*published.borrow(), [on(1), lost()]);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_fact_change_on_the_same_network_is_published_without_confirmation() {
        local(async {
            let (fake, signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            set(&fake, metered(1));
            signal.send(()).unwrap();
            sleep(CONFIRM / 2).await;
            assert_eq!(*published.borrow(), [on(1), metered(1)]);
        })
        .await;
    }

    #[test]
    fn the_settler_confirms_only_a_repeated_network() {
        let mut settler = Settler::new(on(1));
        assert_eq!(settler.step(on(2)), Step::Confirm);
        assert_eq!(settler.step(on(3)), Step::Confirm);
        assert_eq!(settler.step(on(3)), Step::Publish(on(3)));
        assert_eq!(settler.step(on(3)), Step::Hold);
        assert_eq!(settler.step(lost()), Step::Confirm);
        assert_eq!(settler.step(on(3)), Step::Hold);
        assert_eq!(settler.step(lost()), Step::Confirm);
        assert_eq!(settler.step(lost()), Step::Publish(lost()));
    }

    #[test]
    fn events_name_what_changed() {
        assert_eq!(
            NetworkEvent::between(&on(1), &on(2)),
            Some(NetworkEvent::Changed(NetworkId::from_key(&2_u32)))
        );
        assert_eq!(
            NetworkEvent::between(&lost(), &on(1)),
            Some(NetworkEvent::Changed(NetworkId::from_key(&1_u32)))
        );
        assert_eq!(
            NetworkEvent::between(&on(1), &lost()),
            Some(NetworkEvent::Lost)
        );
        assert_eq!(
            NetworkEvent::between(&on(1), &metered(1)),
            Some(NetworkEvent::FactsChanged)
        );
        assert_eq!(NetworkEvent::between(&on(1), &on(1)), None);
    }

    #[test]
    fn the_end_of_a_block_on_the_same_network_is_a_resume() {
        assert_eq!(
            NetworkEvent::between(&on(1), &resumed(on(1), 1)),
            Some(NetworkEvent::Resumed)
        );
        // Two blocks ended between readings are one resume.
        assert_eq!(
            NetworkEvent::between(&resumed(on(1), 1), &resumed(on(1), 3)),
            Some(NetworkEvent::Resumed)
        );
        // A resume names itself over another fact that changed with it.
        assert_eq!(
            NetworkEvent::between(&on(1), &resumed(metered(1), 1)),
            Some(NetworkEvent::Resumed)
        );
        // A resume onto another network is that network's change.
        assert_eq!(
            NetworkEvent::between(&on(1), &resumed(on(2), 1)),
            Some(NetworkEvent::Changed(NetworkId::from_key(&2_u32)))
        );
    }

    // Doze ends: its end is published at once, as any fact on the same
    // network is.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_a_block_is_published_without_confirmation() {
        local(async {
            let (fake, signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            set(&fake, resumed(on(1), 1));
            signal.send(()).unwrap();
            sleep(CONFIRM / 2).await;
            assert_eq!(*published.borrow(), [on(1), resumed(on(1), 1)]);
        })
        .await;
    }

    // Doze ends before the signals settle, as when a frozen app thaws: the
    // facts after them equal those before the block.
    #[tokio::test(start_paused = true)]
    async fn a_block_ended_within_one_settle_window_is_a_resume() {
        local(async {
            let (fake, signal, published) = start(on(1));
            sleep(Duration::from_millis(1)).await;
            // The block begins: the platform signals and nothing else changes.
            signal.send(()).unwrap();
            sleep(Duration::from_millis(10)).await;
            set(&fake, resumed(on(1), 1));
            signal.send(()).unwrap();
            sleep(Duration::from_secs(10)).await;
            let published = published.borrow();
            let resumed = published
                .windows(2)
                .filter_map(|pair| NetworkEvent::between(&pair[0], &pair[1]))
                .any(|event| event == NetworkEvent::Resumed);
            assert!(resumed, "{published:?}");
        })
        .await;
    }

    // The end of a block and a later change on the same network, both
    // published before the reader reads.
    #[tokio::test(start_paused = true)]
    async fn a_block_and_its_end_folded_into_one_change_are_a_resume() {
        let (tx, rx) = tokio::sync::watch::channel(on(1));
        let monitor = super::NetworkMonitor { facts: rx };
        let mut changes = monitor.changes();
        tx.send_replace(resumed(on(1), 1));
        tx.send_replace(resumed(metered(1), 1));
        let next = tokio::time::timeout(Duration::from_secs(1), changes.next()).await;
        assert_eq!(next, Ok(Some(NetworkEvent::Resumed)));
    }

    #[tokio::test]
    async fn changes_fold_published_facts_into_events() {
        let (tx, rx) = tokio::sync::watch::channel(on(1));
        let monitor = super::NetworkMonitor { facts: rx };
        let mut changes = monitor.changes();
        tx.send_replace(on(1));
        tx.send_replace(on(2));
        assert_eq!(
            changes.next().await,
            Some(NetworkEvent::Changed(NetworkId::from_key(&2_u32)))
        );
        tx.send_replace(on(2));
        tx.send_replace(lost());
        assert_eq!(changes.next().await, Some(NetworkEvent::Lost));
        drop(tx);
        assert_eq!(changes.next().await, None);
    }
}
