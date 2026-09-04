//! Play-queue model: ordering, shuffle, repeat. Pure logic — no IO, no gpui
//! side effects — so it is unit-testable.

use subsonic::Song;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

/// The queue holds songs in canonical order; `order` is the play order
/// (identity when shuffle is off, permutation when on). `current` indexes
/// into `order`.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Queue {
    songs: Vec<Song>,
    order: Vec<usize>,
    current: Option<usize>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    /// Bumped by every change to the contents or the play order.
    ///
    /// `PlayerState` notifies its observers on every event, position ticks
    /// included, so a view that draws the queue cannot tell a new track from
    /// the same track a quarter-second later. Comparing this against the last
    /// value it drew answers that in one integer, with no walk of the songs.
    /// It is deliberately `#[serde(skip)]`: it counts edits within a session
    /// and means nothing across a restore.
    #[serde(skip)]
    revision: u64,
}

impl Queue {
    /// Counter bumped by every mutation. See [`Queue::revision`]'s field docs.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Structural sanity check for queues deserialized from disk: `order`
    /// must be a permutation of song indices and `current` must be in range.
    pub fn is_valid(&self) -> bool {
        if self.order.len() != self.songs.len() {
            return false;
        }
        let mut seen = vec![false; self.songs.len()];
        for &idx in &self.order {
            if idx >= self.songs.len() || seen[idx] {
                return false;
            }
            seen[idx] = true;
        }
        self.current.is_none_or(|c| c < self.order.len())
    }

    /// Replace the queue and start at `songs[start]`.
    pub fn replace(&mut self, songs: Vec<Song>, start: usize) {
        self.songs = songs;
        self.order = (0..self.songs.len()).collect();
        self.current = if start < self.songs.len() {
            Some(start)
        } else {
            None
        };
        if self.shuffle {
            self.reshuffle();
        }
        self.touch();
    }

    /// Append songs at the end of the play order.
    pub fn append(&mut self, songs: Vec<Song>) {
        let base = self.songs.len();
        self.songs.extend(songs);
        self.order.extend(base..self.songs.len());
        self.touch();
    }

    /// Insert songs directly after the current one in play order.
    pub fn play_next(&mut self, songs: Vec<Song>) {
        let base = self.songs.len();
        let count = songs.len();
        self.songs.extend(songs);
        let at = self.current.map(|c| c + 1).unwrap_or(0);
        let at = at.min(self.order.len());
        self.order.splice(at..at, base..base + count);
        self.touch();
    }

    /// Remove the item at `order_pos` (position in play order).
    pub fn remove(&mut self, order_pos: usize) {
        if order_pos >= self.order.len() {
            return;
        }
        let song_idx = self.order.remove(order_pos);
        self.songs.remove(song_idx);
        // Reindex order entries past the removed song.
        for o in &mut self.order {
            if *o > song_idx {
                *o -= 1;
            }
        }
        match self.current {
            Some(c) if order_pos < c => self.current = Some(c - 1),
            Some(c) if order_pos == c => {
                // Current removed: stay at same position (now the next song),
                // clamping at the end.
                if self.order.is_empty() {
                    self.current = None;
                } else {
                    self.current = Some(c.min(self.order.len() - 1));
                }
            }
            _ => {}
        }
        self.touch();
    }

    /// Move an item within the play order.
    pub fn move_item(&mut self, from: usize, to: usize) {
        if from >= self.order.len() || to >= self.order.len() || from == to {
            return;
        }
        let item = self.order.remove(from);
        self.order.insert(to, item);
        if let Some(c) = self.current {
            self.current = Some(match c {
                c if c == from => to,
                c if from < c && to >= c => c - 1,
                c if from > c && to <= c => c + 1,
                c => c,
            });
        }
        self.touch();
    }

    pub fn clear(&mut self) {
        self.songs.clear();
        self.order.clear();
        self.current = None;
        self.touch();
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Songs in play order, with their order positions.
    pub fn iter_ordered(&self) -> impl Iterator<Item = (usize, &Song)> {
        self.order
            .iter()
            .enumerate()
            .map(|(pos, &i)| (pos, &self.songs[i]))
    }

    pub fn current_pos(&self) -> Option<usize> {
        self.current
    }

    pub fn current_song(&self) -> Option<&Song> {
        self.current.map(|c| &self.songs[self.order[c]])
    }

    /// Jump to an explicit position in play order.
    pub fn jump_to(&mut self, order_pos: usize) -> Option<&Song> {
        if order_pos < self.order.len() {
            self.current = Some(order_pos);
            self.touch();
            self.current_song()
        } else {
            None
        }
    }

    /// Position that would play after the current track ends naturally
    /// (honors repeat). None = playback stops.
    pub fn next_pos(&self) -> Option<usize> {
        let c = self.current?;
        match self.repeat {
            RepeatMode::One => Some(c),
            _ if c + 1 < self.order.len() => Some(c + 1),
            RepeatMode::All if !self.order.is_empty() => Some(0),
            _ => None,
        }
    }

    /// Position for an explicit "next" button press (repeat One is ignored —
    /// the user asked to move on).
    pub fn skip_next_pos(&self) -> Option<usize> {
        let c = self.current?;
        if c + 1 < self.order.len() {
            Some(c + 1)
        } else if self.repeat == RepeatMode::All && !self.order.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    /// Position for an explicit "previous" button press.
    pub fn prev_pos(&self) -> Option<usize> {
        let c = self.current?;
        if c > 0 {
            Some(c - 1)
        } else if self.repeat == RepeatMode::All && !self.order.is_empty() {
            Some(self.order.len() - 1)
        } else {
            None
        }
    }

    pub fn advance_to(&mut self, order_pos: usize) {
        if order_pos < self.order.len() {
            self.current = Some(order_pos);
            self.touch();
        }
    }

    /// Toggle shuffle. Turning it on re-randomizes with the current song
    /// moved to the front; turning it off restores canonical order (keeping
    /// the current song as current).
    pub fn set_shuffle(&mut self, shuffle: bool) {
        let current_song_idx = self.current.map(|c| self.order[c]);
        self.shuffle = shuffle;
        if shuffle {
            self.reshuffle();
        } else {
            self.order = (0..self.songs.len()).collect();
            self.current = current_song_idx;
        }
        self.touch();
    }

    fn reshuffle(&mut self) {
        use rand::seq::SliceRandom;
        let current_song_idx = self.current.map(|c| self.order[c]);
        self.order = (0..self.songs.len()).collect();
        self.order.shuffle(&mut rand::rng());
        // Current song plays first in the new order.
        if let Some(idx) = current_song_idx {
            let pos = self
                .order
                .iter()
                .position(|&o| o == idx)
                .expect("current song present in order");
            self.order.swap(0, pos);
            self.current = Some(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        serde_json::from_str(&format!(r#"{{"id":"{id}","title":"t-{id}"}}"#)).unwrap()
    }

    fn songs(n: usize) -> Vec<Song> {
        (0..n).map(|i| song(&i.to_string())).collect()
    }

    fn ids(q: &Queue) -> Vec<String> {
        q.iter_ordered().map(|(_, s)| s.id.clone()).collect()
    }

    #[test]
    fn replace_and_walk() {
        let mut q = Queue::default();
        q.replace(songs(3), 0);
        assert_eq!(q.current_song().unwrap().id, "0");
        assert_eq!(q.next_pos(), Some(1));
        q.advance_to(1);
        q.advance_to(2);
        assert_eq!(q.next_pos(), None); // repeat off, end of queue
    }

    #[test]
    fn repeat_all_wraps() {
        let mut q = Queue::default();
        q.replace(songs(2), 1);
        q.repeat = RepeatMode::All;
        assert_eq!(q.next_pos(), Some(0));
        assert_eq!(q.skip_next_pos(), Some(0));
        assert_eq!(q.prev_pos(), Some(0));
    }

    #[test]
    fn repeat_one_repeats_naturally_but_skip_moves_on() {
        let mut q = Queue::default();
        q.replace(songs(3), 1);
        q.repeat = RepeatMode::One;
        assert_eq!(q.next_pos(), Some(1)); // natural end: same song
        assert_eq!(q.skip_next_pos(), Some(2)); // explicit skip: next song
    }

    #[test]
    fn play_next_inserts_after_current() {
        let mut q = Queue::default();
        q.replace(songs(3), 0); // 0 1 2
        q.play_next(vec![song("9")]);
        assert_eq!(ids(&q), vec!["0", "9", "1", "2"]);
        assert_eq!(q.current_song().unwrap().id, "0");
    }

    #[test]
    fn append_goes_to_end() {
        let mut q = Queue::default();
        q.replace(songs(2), 1);
        q.append(vec![song("9")]);
        assert_eq!(ids(&q), vec!["0", "1", "9"]);
    }

    #[test]
    fn remove_before_current_shifts_current() {
        let mut q = Queue::default();
        q.replace(songs(3), 2);
        q.remove(0);
        assert_eq!(q.current_song().unwrap().id, "2");
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn remove_current_keeps_position() {
        let mut q = Queue::default();
        q.replace(songs(3), 1);
        q.remove(1);
        // Position 1 now holds what was song 2.
        assert_eq!(q.current_song().unwrap().id, "2");
    }

    #[test]
    fn remove_last_current_clamps() {
        let mut q = Queue::default();
        q.replace(songs(2), 1);
        q.remove(1);
        assert_eq!(q.current_song().unwrap().id, "0");
        q.remove(0);
        assert!(q.current_song().is_none());
        assert!(q.is_empty());
    }

    #[test]
    fn move_item_adjusts_current() {
        let mut q = Queue::default();
        q.replace(songs(4), 2); // current = song "2"
        q.move_item(0, 3); // 1 2 3 0
        assert_eq!(q.current_song().unwrap().id, "2");
        assert_eq!(ids(&q), vec!["1", "2", "3", "0"]);
        q.move_item(1, 0); // move current itself
        assert_eq!(q.current_song().unwrap().id, "2");
        assert_eq!(q.current_pos(), Some(0));
    }

    #[test]
    fn shuffle_puts_current_first_and_unshuffle_restores() {
        let mut q = Queue::default();
        q.replace(songs(10), 5);
        q.set_shuffle(true);
        assert_eq!(q.current_pos(), Some(0));
        assert_eq!(q.current_song().unwrap().id, "5");
        let mut sorted = ids(&q);
        sorted.sort_by_key(|s| s.parse::<u32>().unwrap());
        assert_eq!(sorted.len(), 10); // nothing lost
        q.set_shuffle(false);
        assert_eq!(ids(&q)[..3], ["0", "1", "2"]);
        assert_eq!(q.current_song().unwrap().id, "5");
    }

    #[test]
    fn shuffle_empty_queue_no_panic() {
        let mut q = Queue::default();
        q.set_shuffle(true);
        assert!(q.current_song().is_none());
    }
}
