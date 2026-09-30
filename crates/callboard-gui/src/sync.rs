//! Decides what to refetch and when. Event notices drive refetches while the
//! stream is live; while it is down, a fixed-interval poll takes over.
use crate::{
    backend::{List, Request, Target},
    events::{Notice, Signal},
};
use callboard_core::store::Change;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

/// Fallback refresh interval while the event stream is unavailable, and the
/// retry delay for failed fetches.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Streams rotate every 25 seconds; a reconnect within this window is not an outage.
pub const OFFLINE_GRACE: Duration = Duration::from_secs(2);
/// A clean end counts as the service's 25-second rotation only after this
/// long. A shutdown also ends streams cleanly, but at an arbitrary time.
pub const ROTATION_MIN: Duration = Duration::from_secs(20);
/// A resync after a routine rotation is skipped (§6.5), but everything open is
/// refetched at least this often, bounding anything missed during a reconnect.
pub const MAX_STALE: Duration = Duration::from_secs(5 * 60);
/// At startup, wait this long for the stream to connect (its resync then
/// drives the first fetch) or fail (fetch at once, auto-starting the service).
pub const STARTUP_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Live,
    /// No event stream; refreshing every [`POLL_INTERVAL`].
    Polling,
}

/// Which parts of a batch failed. Only those are retried.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub lists_failed: Vec<List>,
    pub failed: Vec<Target>,
}

impl Outcome {
    pub fn all_failed(request: &Request) -> Self {
        Self {
            lists_failed: request.lists.iter().copied().collect(),
            failed: request.targets.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Stream {
    Up { since: Instant },
    Down { since: Instant },
}

pub struct Scheduler {
    started: Instant,
    stream_seen: bool,
    ever_connected: bool,
    stream: Stream,
    /// When a long-lived stream last ended cleanly (routine rotation).
    rotated_at: Option<Instant>,
    /// Skip the resync that opens a stream reconnected after rotation.
    skip_resync: bool,
    last_error: Option<String>,
    dirty_lists: BTreeSet<List>,
    lists_retry: Option<Instant>,
    /// Refetch every open target on the next batch.
    everything: bool,
    last_full: Option<Instant>,
    /// Feeds whose items changed: boards referencing them need refetching,
    /// because board reads resolve references at display time.
    feeds_changed: BTreeSet<String>,
    dirty: BTreeSet<Target>,
    /// Scheduled refetches: snooze expiry and failed-target retries.
    wake: BTreeMap<Target, Instant>,
    /// Refetch the feed list when a snooze expires, which changes its counts.
    feed_list_wake: Option<Instant>,
    next_poll: Instant,
    in_flight: Option<Request>,
}

impl Scheduler {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            stream_seen: false,
            ever_connected: false,
            stream: Stream::Down { since: now },
            rotated_at: None,
            skip_resync: false,
            last_error: None,
            dirty_lists: List::ALL.into_iter().collect(),
            lists_retry: None,
            everything: false,
            last_full: None,
            feeds_changed: BTreeSet::new(),
            dirty: BTreeSet::new(),
            wake: BTreeMap::new(),
            feed_list_wake: None,
            next_poll: now,
            in_flight: None,
        }
    }

    pub fn link(&self, now: Instant) -> Link {
        match self.stream {
            Stream::Up { .. } => Link::Live,
            Stream::Down { since } if now.duration_since(since) >= OFFLINE_GRACE => Link::Polling,
            Stream::Down { .. } if self.ever_connected => Link::Live,
            Stream::Down { .. } => Link::Connecting,
        }
    }

    /// The last reason the event stream dropped, while it is down.
    pub fn stream_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    pub fn signal(&mut self, signal: Signal, now: Instant) {
        match signal {
            Signal::Connected => {
                self.stream_seen = true;
                self.skip_resync = self
                    .rotated_at
                    .is_some_and(|t| now.duration_since(t) < OFFLINE_GRACE)
                    && self
                        .last_full
                        .is_some_and(|t| now.duration_since(t) < MAX_STALE);
                self.rotated_at = None;
                self.stream = Stream::Up { since: now };
                self.ever_connected = true;
                self.last_error = None;
            }
            Signal::Disconnected(reason) => {
                self.stream_seen = true;
                self.rotated_at = match self.stream {
                    Stream::Up { since }
                        if reason.is_none() && now.duration_since(since) >= ROTATION_MIN =>
                    {
                        Some(now)
                    }
                    _ => None,
                };
                if let Stream::Up { .. } = self.stream {
                    self.stream = Stream::Down { since: now };
                }
                self.skip_resync = false;
                if reason.is_some() {
                    self.last_error = reason;
                }
            }
            Signal::Notice(Notice::Resync) => {
                // Only the resync opening a rotated stream is skipped; a lag
                // resync mid-stream always refetches.
                if !std::mem::take(&mut self.skip_resync) {
                    self.refresh_all();
                }
            }
            Signal::Notice(Notice::Change(change)) => {
                self.skip_resync = false;
                self.invalidate(change);
            }
        }
    }

    /// Refetch lists and every open target (resync, or the user asked).
    pub fn refresh_all(&mut self) {
        self.dirty_lists.extend(List::ALL);
        self.lists_retry = None;
        self.everything = true;
    }

    pub fn refresh_layouts(&mut self) {
        self.dirty_lists.insert(List::Layouts);
        self.lists_retry = None;
    }

    fn invalidate(&mut self, change: Change) {
        match change {
            Change::Feed { name } => {
                // Title, error/stale status, creation, and deletion.
                self.dirty_lists.insert(List::Feeds);
                self.dirty.insert(Target::Feed(name.clone()));
                self.feeds_changed.insert(name);
            }
            // The archive is not in any list.
            Change::Board { id: 1 } => {
                self.dirty.insert(Target::Archive);
            }
            Change::Board { id } => {
                // Notices do not say whether a board was renamed, created, or
                // deleted, or only its items changed; the list is small.
                self.dirty_lists.insert(List::Boards);
                self.dirty.insert(Target::Board(id));
            }
            Change::Layout { .. } => {
                self.dirty_lists.insert(List::Layouts);
            }
        }
    }

    /// Refetch a board (or the archive, ID 1) and the board list, as its
    /// change notice would, after this window changed it.
    pub fn board_changed(&mut self, id: i64) {
        self.invalidate(Change::Board { id });
    }

    /// Fetch `target` soon, for example when a card first shows it. Already
    /// in flight: its response is coming, so it is not queued again.
    pub fn want(&mut self, target: Target) {
        if !self
            .in_flight
            .as_ref()
            .is_some_and(|r| r.targets.contains(&target))
        {
            self.dirty.insert(target);
        }
    }

    /// Refetch `target` at `at`, for example when a snooze expires.
    pub fn wake_at(&mut self, target: Target, at: Instant) {
        self.wake
            .entry(target)
            .and_modify(|t| *t = (*t).min(at))
            .or_insert(at);
    }

    /// Refetch the feed list at `at`, replacing any earlier schedule. Each
    /// feed list names its own next snooze deadline.
    pub fn wake_feed_list_at(&mut self, at: Option<Instant>) {
        self.feed_list_wake = at;
    }

    /// The next batch to fetch, given the targets currently open.
    /// `references(board, feed)` says whether an open board or archive
    /// references `feed`; it should answer true when unsure (not yet loaded).
    pub fn poll(
        &mut self,
        now: Instant,
        open: &BTreeSet<Target>,
        references: impl Fn(&Target, &str) -> bool,
    ) -> Option<Request> {
        if self.in_flight.is_some() {
            return None;
        }
        if !self.stream_seen && now < self.started + STARTUP_WAIT {
            return None;
        }
        let polling = self.link(now) == Link::Polling && now >= self.next_poll;
        let stale = self
            .last_full
            .is_some_and(|t| now.duration_since(t) >= MAX_STALE);
        if polling || stale {
            self.refresh_all();
        }
        let due: Vec<_> = self
            .wake
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(t, _)| t.clone())
            .collect();
        for target in due {
            self.wake.remove(&target);
            self.dirty.insert(target);
        }
        if self.feed_list_wake.is_some_and(|at| at <= now) {
            self.feed_list_wake = None;
            self.dirty_lists.insert(List::Feeds);
        }
        self.wake.retain(|t, _| open.contains(t));
        if std::mem::take(&mut self.everything) {
            self.dirty.extend(open.iter().cloned());
            self.last_full = Some(now);
            self.next_poll = now + POLL_INTERVAL;
        }
        let changed = std::mem::take(&mut self.feeds_changed);
        if !changed.is_empty() {
            self.dirty.extend(
                open.iter()
                    .filter(|t| matches!(t, Target::Board(_) | Target::Archive))
                    .filter(|t| changed.iter().any(|feed| references(t, feed)))
                    .cloned(),
            );
        }
        self.dirty.retain(|t| open.contains(t));
        let lists_ready = self.lists_retry.is_none_or(|t| now >= t);
        let lists = if lists_ready {
            std::mem::take(&mut self.dirty_lists)
        } else {
            BTreeSet::new()
        };
        if lists.is_empty() && self.dirty.is_empty() {
            return None;
        }
        let request = Request {
            lists,
            targets: std::mem::take(&mut self.dirty).into_iter().collect(),
        };
        self.in_flight = Some(request.clone());
        Some(request)
    }

    /// Record the outcome of the batch returned by [`Self::poll`]. Failed parts
    /// are retried after [`POLL_INTERVAL`]; the rest of the queue is not held.
    pub fn finished(&mut self, now: Instant, outcome: &Outcome) {
        let Some(request) = self.in_flight.take() else {
            return;
        };
        if !outcome.lists_failed.is_empty() {
            self.dirty_lists
                .extend(outcome.lists_failed.iter().copied());
            self.lists_retry = Some(now + POLL_INTERVAL);
        } else if !request.lists.is_empty() {
            self.lists_retry = None;
        }
        for target in &outcome.failed {
            self.wake_at(target.clone(), now + POLL_INTERVAL);
        }
    }

    /// How long the UI may sleep before [`Self::poll`] could return work.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        // The worker wakes the UI when this batch finishes. Timers cannot
        // start another batch meanwhile, even if their deadlines have passed.
        if self.busy() {
            return None;
        }
        let mut deadlines: Vec<Instant> = self.wake.values().copied().collect();
        deadlines.extend(self.feed_list_wake);
        if !self.dirty_lists.is_empty()
            && let Some(retry) = self.lists_retry
        {
            deadlines.push(retry);
        }
        if !self.stream_seen {
            deadlines.push(self.started + STARTUP_WAIT);
        }
        if let Stream::Down { since } = self.stream {
            deadlines.push(self.next_poll.max(since + OFFLINE_GRACE));
        }
        if let Some(full) = self.last_full {
            deadlines.push(full + MAX_STALE);
        }
        deadlines
            .into_iter()
            .min()
            .map(|t| t.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_deadlines_sleep_until_the_in_flight_batch_finishes() {
        let now = Instant::now();
        let mut scheduler = Scheduler::new(now);
        scheduler.signal(Signal::Disconnected(Some("offline".into())), now);
        let open = BTreeSet::from([Target::Feed("a".into())]);
        scheduler.wake_at(Target::Feed("a".into()), now + Duration::from_secs(3));
        assert!(
            scheduler
                .poll(now + OFFLINE_GRACE, &open, |_, _| false)
                .is_some()
        );
        let later = now + Duration::from_secs(8);
        assert!(scheduler.poll(later, &open, |_, _| false).is_none());
        assert_eq!(scheduler.next_deadline(later), None);
        scheduler.finished(later, &Outcome::default());
        assert_eq!(scheduler.next_deadline(later), Some(Duration::ZERO));
        assert!(scheduler.poll(later, &open, |_, _| false).is_some());
    }

    fn open(targets: &[Target]) -> BTreeSet<Target> {
        targets.iter().cloned().collect()
    }

    fn live(now: Instant) -> Scheduler {
        let mut s = Scheduler::new(now);
        s.signal(Signal::Connected, now);
        s.signal(Signal::Notice(Notice::Resync), now);
        s
    }

    fn any(_: &Target, _: &str) -> bool {
        true
    }

    fn ok() -> Outcome {
        Outcome::default()
    }

    fn change(change: Change) -> Signal {
        Signal::Notice(Notice::Change(change))
    }

    #[test]
    fn resync_refetches_all_open_targets_and_notices_refetch_only_what_changed() {
        let t0 = Instant::now();
        let feed = Target::Feed("reviews".into());
        let other = Target::Feed("alerts".into());
        let board = Target::Board(2);
        let visible = open(&[feed.clone(), other.clone(), board.clone(), Target::Archive]);
        let mut s = live(t0);
        let first = s.poll(t0, &visible, any).unwrap();
        assert_eq!(first.lists.len(), 3);
        assert_eq!(first.targets.len(), 4);
        assert!(s.poll(t0, &visible, any).is_none(), "one batch in flight");
        // Notices arriving mid-flight are kept for the next batch.
        s.signal(change(Change::Board { id: 2 }), t0);
        s.finished(t0, &ok());
        let next = s.poll(t0, &visible, any).unwrap();
        assert_eq!(next.targets, vec![board.clone()]);
        s.finished(t0, &ok());
        assert!(s.poll(t0, &visible, any).is_none(), "live: no polling");
        // A feed change also refreshes board views, whose references resolve live.
        s.signal(
            change(Change::Feed {
                name: "reviews".into(),
            }),
            t0,
        );
        let next = s.poll(t0, &visible, any).unwrap();
        assert_eq!(
            next.targets,
            vec![feed.clone(), board.clone(), Target::Archive]
        );
        s.finished(t0, &ok());
        // A feed that is not placed may still be referenced from open boards.
        s.signal(
            change(Change::Feed {
                name: "unplaced".into(),
            }),
            t0,
        );
        assert_eq!(
            s.poll(t0, &visible, any).unwrap().targets,
            vec![board.clone(), Target::Archive]
        );
        s.finished(t0, &ok());
        // Other changes to targets that are not open only refresh their list.
        s.signal(change(Change::Board { id: 9 }), t0);
        assert_eq!(
            s.poll(t0, &visible, any).unwrap(),
            Request {
                lists: [List::Boards].into(),
                targets: vec![]
            }
        );
        s.finished(t0, &ok());
        s.signal(change(Change::Layout { name: "Day".into() }), t0);
        assert_eq!(
            s.poll(t0, &visible, any).unwrap().lists,
            [List::Layouts].into()
        );
        s.finished(t0, &ok());
        // Lag resync mid-stream refetches everything again.
        s.signal(Signal::Notice(Notice::Resync), t0);
        assert_eq!(s.poll(t0, &visible, any).unwrap().targets.len(), 4);
    }

    #[test]
    fn startup_waits_briefly_for_the_stream_so_the_first_load_happens_once() {
        let t0 = Instant::now();
        let feed = Target::Feed("reviews".into());
        let visible = open(std::slice::from_ref(&feed));
        // Stream connects first: its resync drives the single initial load.
        let mut s = Scheduler::new(t0);
        s.want(feed.clone());
        assert!(s.poll(t0, &visible, any).is_none());
        assert_eq!(s.next_deadline(t0), Some(STARTUP_WAIT));
        let t1 = t0 + Duration::from_millis(100);
        s.signal(Signal::Connected, t1);
        s.signal(Signal::Notice(Notice::Resync), t1);
        let first = s.poll(t1, &visible, any).unwrap();
        assert!(!first.lists.is_empty() && first.targets == vec![feed.clone()]);
        s.finished(t1, &ok());
        assert!(s.poll(t0 + OFFLINE_GRACE * 2, &visible, any).is_none());
        // No service yet: the stream fails and the load (auto-start) runs at once.
        let mut s = Scheduler::new(t0);
        s.want(feed.clone());
        s.signal(Signal::Disconnected(Some("refused".into())), t1);
        assert!(s.poll(t1, &visible, any).is_some());
        // Neither: the load still starts after the wait.
        let mut s = Scheduler::new(t0);
        assert!(s.poll(t0 + STARTUP_WAIT, &visible, any).is_some());
    }

    #[test]
    fn falls_back_to_bounded_polling_while_stream_is_down_and_stops_when_live() {
        let t0 = Instant::now();
        let feed = Target::Feed("reviews".into());
        let visible = open(std::slice::from_ref(&feed));
        let mut s = Scheduler::new(t0);
        assert_eq!(s.link(t0), Link::Connecting);
        s.signal(Signal::Disconnected(Some("refused".into())), t0);
        s.want(feed.clone());
        assert!(!s.poll(t0, &visible, any).unwrap().lists.is_empty());
        s.finished(t0, &ok());
        let t1 = t0 + OFFLINE_GRACE;
        assert_eq!(s.link(t1), Link::Polling);
        let polled = s.poll(t1, &visible, any).unwrap();
        assert!(!polled.lists.is_empty() && polled.targets == vec![feed.clone()]);
        s.finished(t1, &ok());
        assert!(s.poll(t1 + Duration::from_secs(1), &visible, any).is_none());
        assert_eq!(s.next_deadline(t1), Some(POLL_INTERVAL));
        let t2 = t1 + POLL_INTERVAL;
        assert!(s.poll(t2, &visible, any).is_some(), "polls every interval");
        s.finished(t2, &ok());
        // Stream comes back: its resync refreshes once, then polling stops.
        s.signal(Signal::Connected, t2);
        assert_eq!(s.link(t2), Link::Live);
        s.signal(Signal::Notice(Notice::Resync), t2);
        assert!(s.poll(t2, &visible, any).is_some());
        s.finished(t2, &ok());
        assert!(s.poll(t2 + POLL_INTERVAL * 3, &visible, any).is_none());
        // A real outage switches to polling after the grace period.
        let t3 = t2 + Duration::from_secs(30);
        s.signal(Signal::Disconnected(Some("refused".into())), t3);
        assert_eq!(s.link(t3 + Duration::from_secs(1)), Link::Live);
        assert_eq!(s.link(t3 + OFFLINE_GRACE), Link::Polling);
        assert_eq!(s.stream_error(), Some("refused"));
        assert!(s.poll(t3 + OFFLINE_GRACE, &visible, any).is_some());
    }

    #[test]
    fn routine_rotation_skips_its_resync_but_refetches_at_least_every_max_stale() {
        let t0 = Instant::now();
        let feed = Target::Feed("reviews".into());
        let visible = open(std::slice::from_ref(&feed));
        let mut s = live(t0);
        s.poll(t0, &visible, any).unwrap();
        s.finished(t0, &ok());
        let rotate = |s: &mut Scheduler, at: Instant, gap: Duration| {
            s.signal(Signal::Disconnected(None), at);
            s.signal(Signal::Connected, at + gap);
            s.signal(Signal::Notice(Notice::Resync), at + gap);
        };
        let t1 = t0 + Duration::from_secs(25);
        rotate(&mut s, t1, Duration::from_millis(5));
        assert_eq!(s.link(t1), Link::Live);
        assert!(
            s.poll(t1 + Duration::from_millis(5), &visible, any)
                .is_none()
        );
        // A lag resync later on the same stream is never skipped.
        s.signal(Signal::Notice(Notice::Resync), t1 + Duration::from_secs(1));
        assert!(s.poll(t1 + Duration::from_secs(1), &visible, any).is_some());
        s.finished(t1 + Duration::from_secs(1), &ok());
        // A slow reconnect, or an error between streams, refetches.
        let t2 = t1 + Duration::from_secs(25);
        rotate(&mut s, t2, OFFLINE_GRACE);
        assert!(s.poll(t2 + OFFLINE_GRACE, &visible, any).is_some());
        s.finished(t2 + OFFLINE_GRACE, &ok());
        let t3 = t2 + Duration::from_secs(25);
        s.signal(Signal::Disconnected(None), t3);
        s.signal(Signal::Disconnected(Some("refused".into())), t3);
        s.signal(Signal::Connected, t3 + Duration::from_millis(500));
        s.signal(
            Signal::Notice(Notice::Resync),
            t3 + Duration::from_millis(500),
        );
        assert!(
            s.poll(t3 + Duration::from_millis(500), &visible, any)
                .is_some()
        );
        s.finished(t3 + Duration::from_millis(500), &ok());
        // Skipped rotations still get a full refetch once MAX_STALE passes.
        let last_full = t3 + Duration::from_millis(500);
        let mut at = last_full;
        while at + Duration::from_secs(25) < last_full + MAX_STALE {
            at += Duration::from_secs(25);
            rotate(&mut s, at, Duration::ZERO);
            assert!(s.poll(at, &visible, any).is_none());
        }
        assert_eq!(s.next_deadline(at), Some(last_full + MAX_STALE - at));
        let refetch = s.poll(last_full + MAX_STALE, &visible, any).unwrap();
        assert!(!refetch.lists.is_empty() && refetch.targets == vec![feed]);
    }

    #[test]
    fn a_snooze_deadline_refetches_the_feed_list_for_its_counts() {
        let t0 = Instant::now();
        let mut s = live(t0);
        let nothing = open(&[]);
        s.poll(t0, &nothing, any).unwrap();
        s.finished(t0, &ok());
        let at = t0 + Duration::from_secs(30);
        s.wake_feed_list_at(Some(t0 + Duration::from_secs(90)));
        s.wake_feed_list_at(Some(at)); // Each feed list replaces the schedule.
        assert_eq!(s.next_deadline(t0), Some(Duration::from_secs(30)));
        assert!(
            s.poll(at - Duration::from_millis(1), &nothing, any)
                .is_none()
        );
        assert_eq!(
            s.poll(at, &nothing, any).unwrap(),
            Request {
                lists: BTreeSet::from([List::Feeds]),
                targets: vec![],
            }
        );
        s.finished(at, &ok());
        assert!(
            s.poll(at + Duration::from_secs(60), &nothing, any)
                .is_none()
        );
    }

    #[test]
    fn only_failed_parts_retry_after_the_poll_interval_and_snoozes_wake_their_feed() {
        let t0 = Instant::now();
        let feed = Target::Feed("reviews".into());
        let failing = Target::Feed("alerts".into());
        let visible = open(&[feed.clone(), failing.clone()]);
        let mut s = live(t0);
        s.poll(t0, &visible, any).unwrap();
        s.finished(
            t0,
            &Outcome {
                lists_failed: vec![List::Feeds],
                failed: vec![failing.clone()],
            },
        );
        assert!(s.poll(t0, &visible, any).is_none());
        // Other work is not held back by the failure.
        s.signal(
            change(Change::Feed {
                name: "reviews".into(),
            }),
            t0,
        );
        assert_eq!(
            s.poll(t0, &visible, any).unwrap(),
            Request {
                lists: BTreeSet::new(),
                targets: vec![feed.clone()]
            }
        );
        s.finished(t0, &ok());
        assert_eq!(s.next_deadline(t0), Some(POLL_INTERVAL));
        let retry = s.poll(t0 + POLL_INTERVAL, &visible, any).unwrap();
        assert!(!retry.lists.is_empty() && retry.targets == vec![failing.clone()]);
        s.finished(t0 + POLL_INTERVAL, &ok());
        let t1 = t0 + POLL_INTERVAL;
        assert!(s.poll(t1, &visible, any).is_none());
        s.wake_at(feed.clone(), t1 + Duration::from_secs(60));
        s.wake_at(feed.clone(), t1 + Duration::from_secs(30));
        assert_eq!(s.next_deadline(t1), Some(Duration::from_secs(30)));
        assert!(
            s.poll(t1 + Duration::from_secs(29), &visible, any)
                .is_none()
        );
        assert_eq!(
            s.poll(t1 + Duration::from_secs(30), &visible, any).unwrap(),
            Request {
                lists: BTreeSet::new(),
                targets: vec![feed]
            }
        );
    }

    #[test]
    fn a_target_already_in_flight_is_not_queued_again() {
        let t0 = Instant::now();
        let feed = Target::Feed("reviews".into());
        let visible = open(std::slice::from_ref(&feed));
        let mut s = live(t0);
        s.poll(t0, &visible, any).unwrap();
        // The UI keeps asking while nothing is cached yet.
        s.want(feed.clone());
        s.want(feed.clone());
        s.finished(t0, &ok());
        assert!(s.poll(t0, &visible, any).is_none(), "fetched once");
    }

    #[test]
    fn only_a_long_lived_clean_stream_end_counts_as_rotation() {
        let t0 = Instant::now();
        let feed = Target::Feed("reviews".into());
        let visible = open(std::slice::from_ref(&feed));
        let mut s = live(t0);
        s.poll(t0, &visible, any).unwrap();
        s.finished(t0, &ok());
        // A shutdown ends the stream cleanly after 5 s; a new service is up at
        // once. That is a restart, not a rotation: its resync refetches.
        let t1 = t0 + Duration::from_secs(5);
        s.signal(Signal::Disconnected(None), t1);
        s.signal(Signal::Connected, t1);
        s.signal(Signal::Notice(Notice::Resync), t1);
        assert!(s.poll(t1, &visible, any).is_some());
        s.finished(t1, &ok());
        let t2 = t1 + ROTATION_MIN;
        s.signal(Signal::Disconnected(None), t2);
        s.signal(Signal::Connected, t2);
        s.signal(Signal::Notice(Notice::Resync), t2);
        assert!(s.poll(t2, &visible, any).is_none(), "rotation skipped");
    }

    #[test]
    fn feed_changes_refetch_only_boards_that_reference_the_feed() {
        let t0 = Instant::now();
        let visible = open(&[Target::Board(2), Target::Board(3), Target::Archive]);
        let mut s = live(t0);
        s.poll(t0, &visible, any).unwrap();
        s.finished(t0, &ok());
        s.signal(
            change(Change::Feed {
                name: "reviews".into(),
            }),
            t0,
        );
        let refs = |t: &Target, feed: &str| *t == Target::Board(3) && feed == "reviews";
        assert_eq!(
            s.poll(t0, &visible, refs).unwrap(),
            Request {
                lists: [List::Feeds].into(),
                targets: vec![Target::Board(3)]
            }
        );
    }
}
