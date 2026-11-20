//! Context continuation, explicit play-next, and history are separate streams.
use player_domain::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
mod session;
pub use session::*;

pub const MAX_QUEUE_ITEMS: usize = 4096;
pub const MAX_PLAY_NEXT: usize = 64;
pub const MAX_HISTORY: usize = 256;

#[derive(Clone, Debug, Serialize)]
pub struct QueueSnapshot {
    pub revision: u64,
    pub context: PlaybackContext,
    pub current: Option<MediaItem>,
    pub upcoming: Vec<MediaItem>,
    pub history: Vec<MediaId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_queue: Option<DeviceQueueDetails>,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub play_next_count: usize,
    pub forward_count: usize,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueEngine {
    items: Vec<MediaItem>,
    remaining: VecDeque<usize>,
    priority: VecDeque<MediaItem>,
    forward: Vec<MediaItem>,
    current: Option<MediaItem>,
    history: VecDeque<MediaItem>,
    context: Option<PlaybackContext>,
    repeat: RepeatMode,
    shuffle: bool,
    shuffle_seed: u64,
    revision: u64,
}
impl QueueEngine {
    pub fn replace(
        &mut self,
        items: Vec<MediaItem>,
        start: usize,
        context: PlaybackContext,
    ) -> Result<MediaItem, PlayerError> {
        if items.len() > MAX_QUEUE_ITEMS {
            return Err(PlayerError::invalid("Queue is too large"));
        }
        let item = items
            .get(start)
            .cloned()
            .ok_or_else(|| PlayerError::invalid("Queue start index is out of range"))?;
        self.remaining = (start + 1..items.len()).collect();
        self.shuffle_seed = items
            .iter()
            .flat_map(|i| i.id.as_str().bytes())
            .fold(7_u64, |s, b| s.wrapping_mul(31).wrapping_add(u64::from(b)));
        self.items = items;
        self.priority.clear();
        self.forward.clear();
        self.history.clear();
        self.context = Some(context);
        self.current = Some(item.clone());
        self.changed();
        if self.shuffle {
            self.shuffle_remaining();
        }
        Ok(item)
    }
    pub fn play_next(&mut self, item: MediaItem) -> Result<(), PlayerError> {
        if self.priority.len() == MAX_PLAY_NEXT {
            return Err(PlayerError::invalid("Play Next is full"));
        }
        self.priority.push_back(item);
        self.changed();
        Ok(())
    }
    pub fn update_current(&mut self, item: MediaItem) {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.id == item.id)
        {
            self.current = Some(item.clone());
            for stored in &mut self.items {
                if stored.id == item.id {
                    *stored = item.clone();
                }
            }
            self.changed();
        }
    }
    pub fn current(&self) -> Option<&MediaItem> {
        self.current.as_ref()
    }
    pub fn set_repeat(&mut self, mode: RepeatMode) {
        if self.repeat != mode {
            self.repeat = mode;
            self.changed();
        }
    }
    pub fn set_shuffle(&mut self, enabled: bool) {
        if self.shuffle == enabled {
            return;
        }
        self.shuffle = enabled;
        self.changed();
        if enabled {
            self.shuffle_remaining();
        } else {
            self.remaining.make_contiguous().sort_unstable();
        }
    }
    fn shuffle_remaining(&mut self) {
        let order = self.remaining.make_contiguous();
        let mut seed = self.shuffle_seed.max(1);
        for i in (1..order.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            order.swap(i, seed as usize % (i + 1));
        }
        self.shuffle_seed = seed;
    }
    /// Only completion repeats one. Explicit insertions and forward history resume before context.
    pub fn next(&mut self, automatic: bool) -> Option<MediaItem> {
        if automatic && self.repeat == RepeatMode::One {
            return self.current.clone();
        }
        let next = if let Some(item) = self.priority.pop_front() {
            item
        } else if let Some(item) = self.forward.pop() {
            item
        } else {
            if self.remaining.is_empty() && self.repeat == RepeatMode::All && !self.items.is_empty()
            {
                self.remaining = (0..self.items.len()).collect();
                if self.shuffle {
                    self.shuffle_remaining();
                    if let Some(current) = &self.current
                        && self
                            .remaining
                            .front()
                            .is_some_and(|i| self.items[*i].id == current.id)
                        && let Some(other) = self
                            .remaining
                            .iter()
                            .position(|i| self.items[*i].id != current.id)
                    {
                        self.remaining.swap(0, other);
                    }
                }
            }
            self.items[self.remaining.pop_front()?].clone()
        };
        if let Some(previous) = self.current.replace(next.clone()) {
            if self.history.len() == MAX_HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(previous);
        }
        self.changed();
        Some(next)
    }
    pub fn previous(&mut self) -> Option<MediaItem> {
        let previous = self.history.pop_back()?;
        if let Some(current) = self.current.replace(previous.clone()) {
            if self.forward.len() == MAX_HISTORY {
                self.forward.remove(0);
            }
            self.forward.push(current);
        }
        self.changed();
        Some(previous)
    }
    pub fn snapshot(&self) -> QueueSnapshot {
        let mut upcoming: Vec<_> = self.priority.iter().cloned().collect();
        upcoming.extend(self.forward.iter().rev().cloned());
        upcoming.extend(self.remaining.iter().map(|i| self.items[*i].clone()));
        QueueSnapshot {
            revision: self.revision,
            context: self.context.clone().unwrap_or(PlaybackContext::UserQueue),
            current: self.current.clone(),
            upcoming,
            history: self.history.iter().map(|i| i.id.clone()).collect(),
            device_queue: None,
            repeat: self.repeat,
            shuffle: self.shuffle,
            play_next_count: self.priority.len(),
            forward_count: self.forward.len(),
        }
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn check_revision(&self, revision: u64) -> Result<(), PlayerError> {
        if revision == self.revision {
            Ok(())
        } else {
            Err(PlayerError::invalid("The queue changed. Try again."))
        }
    }
    pub fn remove_upcoming(&mut self, index: usize) -> Result<MediaItem, PlayerError> {
        let item = if index < self.priority.len() {
            self.priority.remove(index)
        } else if index < self.priority.len() + self.forward.len() {
            Some(
                self.forward
                    .remove(self.forward.len() - 1 - (index - self.priority.len())),
            )
        } else {
            self.remaining
                .remove(index.saturating_sub(self.priority.len() + self.forward.len()))
                .map(|i| self.items[i].clone())
        }
        .ok_or_else(|| PlayerError::invalid("Queue item is no longer available"))?;
        self.changed();
        Ok(item)
    }
    pub fn jump(&mut self, index: usize) -> Result<MediaItem, PlayerError> {
        let count = self.priority.len() + self.forward.len() + self.remaining.len();
        if index >= count {
            return Err(PlayerError::invalid("Queue item is no longer available"));
        }
        for _ in 0..index {
            self.remove_upcoming(0)?;
        }
        let next = self.remove_upcoming(0)?;
        if let Some(previous) = self.current.replace(next.clone()) {
            if self.history.len() == MAX_HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(previous);
        }
        self.changed();
        Ok(next)
    }
    pub fn clear(&mut self) {
        let revision = self.revision.wrapping_add(1);
        *self = Self {
            revision,
            repeat: self.repeat,
            shuffle: self.shuffle,
            ..Self::default()
        };
    }
    pub fn visit_items_mut(&mut self, mut visit: impl FnMut(&mut MediaItem)) {
        for item in self
            .items
            .iter_mut()
            .chain(self.priority.iter_mut())
            .chain(self.forward.iter_mut())
            .chain(self.history.iter_mut())
            .chain(self.current.iter_mut())
        {
            visit(item);
        }
    }
    pub fn validate(&self) -> Result<(), PlayerError> {
        if self.items.len() > MAX_QUEUE_ITEMS
            || self.priority.len() > MAX_PLAY_NEXT
            || self.history.len() > MAX_HISTORY
            || self.forward.len() > MAX_HISTORY
            || self.remaining.len() > self.items.len()
        {
            return Err(PlayerError::invalid("Saved queue exceeds its limits"));
        }
        let mut seen = HashSet::new();
        if self
            .remaining
            .iter()
            .any(|i| *i >= self.items.len() || !seen.insert(*i))
        {
            return Err(PlayerError::invalid("Saved queue order is invalid"));
        }
        for item in self
            .items
            .iter()
            .chain(self.priority.iter())
            .chain(self.forward.iter())
            .chain(self.history.iter())
            .chain(self.current.iter())
        {
            item.validate()?;
        }
        if let Some(PlaybackContext::Search(text) | PlaybackContext::LocalFolder(text)) =
            &self.context
            && text.len() > 4096
        {
            return Err(PlayerError::invalid("Queue context is too large"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(n: u32) -> MediaItem {
        MediaItem {
            id: MediaId::parse(format!("mock:track:{n}")).unwrap(),
            title: n.to_string(),
            artists: vec![],
            album: None,
            duration_ms: 1000,
            artwork: None,
            location: None,
            capabilities: Capabilities::DEMO,
        }
    }
    fn queue() -> QueueEngine {
        let mut q = QueueEngine::default();
        q.replace((0..5).map(item).collect(), 0, PlaybackContext::UserQueue)
            .unwrap();
        q
    }
    #[test]
    fn insertion_previous_and_new_insertion_preserve_context() {
        let mut q = queue();
        q.play_next(item(9)).unwrap();
        assert_eq!(q.next(false), Some(item(9)));
        assert_eq!(q.next(false), Some(item(1)));
        assert_eq!(q.previous(), Some(item(9)));
        q.play_next(item(8)).unwrap();
        assert_eq!(
            q.snapshot().upcoming,
            vec![item(8), item(1), item(2), item(3), item(4)]
        );
        for expected in [8, 1, 2, 3, 4] {
            assert_eq!(q.next(false), Some(item(expected)));
        }
        assert_eq!(q.next(false), None);
    }
    #[test]
    fn shuffle_changes_only_unplayed_context_and_is_idempotent() {
        let mut q = queue();
        q.next(false);
        q.next(false);
        q.previous();
        let history = q.snapshot().history;
        q.set_shuffle(true);
        let snapshot = q.snapshot();
        q.set_shuffle(true);
        assert_eq!(q.snapshot().upcoming, snapshot.upcoming);
        assert_eq!(q.snapshot().history, history);
        assert_eq!(q.current(), Some(&item(1)));
        assert_eq!(q.next(false), Some(item(2)));
        let mut rest = vec![q.next(false).unwrap(), q.next(false).unwrap()];
        rest.sort_by_key(|i| i.title.clone());
        assert_eq!(rest, vec![item(3), item(4)]);
        assert_eq!(q.next(false), None);
    }
    #[test]
    fn repeat_manual_next_and_cycle_restart_are_distinct() {
        let mut q = queue();
        q.set_repeat(RepeatMode::One);
        assert_eq!(q.next(true), Some(item(0)));
        assert_eq!(q.next(false), Some(item(1)));
        for _ in 0..3 {
            q.next(false);
        }
        q.set_repeat(RepeatMode::All);
        assert_eq!(q.next(true), Some(item(0)));
        q.set_repeat(RepeatMode::Off);
        for _ in 0..4 {
            q.next(true);
        }
        assert_eq!(q.next(true), None);
    }
    #[test]
    fn invalid_replace_is_atomic_and_history_is_bounded() {
        let mut q = queue();
        assert!(q.replace(vec![], 0, PlaybackContext::UserQueue).is_err());
        assert_eq!(q.current(), Some(&item(0)));
        q.set_repeat(RepeatMode::All);
        for _ in 0..1000 {
            q.next(false);
        }
        assert_eq!(q.snapshot().history.len(), MAX_HISTORY);
    }
    #[test]
    fn repeated_insert_and_previous_keeps_forward_history_bounded() {
        let mut q = queue();
        for _ in 0..1000 {
            q.play_next(item(9)).unwrap();
            q.next(false);
            q.previous();
        }
        assert!(q.snapshot().upcoming.len() <= MAX_HISTORY + 4);
        assert_eq!(q.current(), Some(&item(0)));
        for _ in 0..MAX_HISTORY {
            assert_eq!(q.next(false), Some(item(9)));
        }
        assert_eq!(q.next(false), Some(item(1)));
    }
    #[test]
    fn shuffle_cycles_never_repeat_the_same_track_at_the_boundary() {
        let mut q = queue();
        q.set_shuffle(true);
        q.set_repeat(RepeatMode::All);
        for _ in 0..4 {
            q.next(true);
        }
        for _ in 0..40 {
            let mut cycle = HashSet::new();
            for _ in 0..5 {
                let old = q.current().unwrap().id.clone();
                let next = q.next(true).unwrap();
                assert_ne!(old, next.id);
                cycle.insert(next.id);
            }
            assert_eq!(cycle.len(), 5);
        }
    }
    #[test]
    fn removing_and_jumping_across_insertion_forward_and_context_preserve_history() {
        let mut q = queue();
        q.next(false);
        q.previous();
        q.play_next(item(9)).unwrap();
        let revision = q.revision();
        assert!(q.check_revision(revision).is_ok());
        assert_eq!(q.remove_upcoming(1).unwrap(), item(1));
        assert!(q.check_revision(revision).is_err());
        assert_eq!(
            q.snapshot().upcoming,
            vec![item(9), item(2), item(3), item(4)]
        );
        assert_eq!(q.jump(2).unwrap(), item(3));
        assert_eq!(q.snapshot().history, vec![item(0).id]);
        assert_eq!(q.snapshot().upcoming, vec![item(4)]);
        assert!(q.remove_upcoming(99).is_err());
        assert!(q.jump(99).is_err());
        assert_eq!(q.current(), Some(&item(3)));
    }
}
