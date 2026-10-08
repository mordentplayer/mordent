//! Non-consuming playback queue: one `Vec<Track>` for the whole queue plus a
//! pointer (`pos`) to the current entry. Played tracks stay in the list —
//! history is simply everything before the pointer — until removed (row ✕) or
//! cleared. Advancing never pops; it only moves the pointer.
//!
//! All index bookkeeping (append auto-start, insert-after-current, pointer
//! shifts around removals and drag moves, shuffle of the up-next region) lives
//! here as pure logic so it is unit-testable without the UI or audio.

use crate::api::Track;
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeat {
    Off,
    All,
    One,
}

/// What a natural-end advance decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Advance {
    /// Replay the current entry (repeat-one); the pointer stays.
    Replay,
    /// Play the entry at this index; the pointer already moved there.
    Next(usize),
    /// No next entry: stop cleanly. The pointer stays on the last entry so
    /// the finished session stays listed (and a later `+` can resume).
    Stop,
}

/// What a row removal implies for playback.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoveEffect {
    /// The current entry was removed; its successor now sits at this index —
    /// play it to keep the music going.
    PlayNext(usize),
    /// The removed current entry had no successor: stop cleanly (pointer →
    /// None).
    Stopped,
    /// The pointer is untouched (removed a history/up-next row, or nothing
    /// was current).
    Nothing,
}

#[derive(Clone, Debug, Default)]
pub struct Queue {
    pub tracks: Vec<Track>,
    /// Index of the current (most recently started) entry.
    pub pos: Option<usize>,
    /// Full track order from before shuffle was switched on (restore source).
    shuffle_saved: Option<Vec<Track>>,
}

impl Queue {
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.pos
    }

    pub fn current(&self) -> Option<&Track> {
        self.pos.and_then(|p| self.tracks.get(p))
    }

    fn current_id(&self) -> Option<String> {
        self.current().map(|t| t.id.clone())
    }

    /// `+` (single row, album `+`, Queue All): ALWAYS appends at the end, in
    /// order — never at the pointer. Returns the index the caller should
    /// start playing (the pointer is already moved there), which happens only
    /// when the player is "empty":
    ///   (a) nothing ever started (`pos` None, e.g. fresh launch or after a
    ///       clear), or
    ///   (b) the pointer is on the queue's last entry and playback has
    ///       finished (`idle_finished` — natural end ran out of queue).
    /// Every other state (playing, paused mid-queue, pointer before the end)
    /// appends only and touches nothing else.
    pub fn append(&mut self, new: Vec<Track>, idle_finished: bool) -> Option<usize> {
        if new.is_empty() {
            return None;
        }
        let first = self.tracks.len();
        let start = match self.pos {
            None => true,
            Some(p) => idle_finished && p + 1 == first,
        };
        self.tracks.extend(new);
        if start {
            self.pos = Some(first);
            Some(first)
        } else {
            None
        }
    }

    /// "Play now" (Spotify-style): insert `block` right after the current
    /// entry and move the pointer to the block's first track, so the previous
    /// current becomes history and stays listed. With no current entry the
    /// block is appended and the pointer lands on its head. Returns the index
    /// to start playing (the block head).
    pub fn insert_after_current(&mut self, block: Vec<Track>) -> usize {
        debug_assert!(!block.is_empty(), "insert_after_current: empty block");
        let at = match self.pos {
            Some(p) => p + 1,
            None => self.tracks.len(),
        };
        let old_len = self.tracks.len();
        self.tracks.extend(block);
        // `extend` parked the block at the end; rotate it up to slot `at`.
        let k = self.tracks.len() - old_len;
        self.tracks[at..].rotate_right(k);
        self.pos = Some(at);
        at
    }

    /// Jump the pointer to an existing entry (queue-row click). Returns the
    /// index when in bounds; the caller starts playback.
    pub fn play_at(&mut self, idx: usize) -> Option<usize> {
        if idx >= self.tracks.len() {
            return None;
        }
        self.pos = Some(idx);
        Some(idx)
    }

    /// Next entry without moving (manual ⏭ decision + MPRIS CanGoNext).
    pub fn peek_next(&self) -> Option<usize> {
        self.pos.and_then(|p| (p + 1 < self.tracks.len()).then_some(p + 1))
    }

    /// Previous entry without moving (⏮ decision + MPRIS CanGoPrevious).
    pub fn peek_prev(&self) -> Option<usize> {
        self.pos.and_then(|p| p.checked_sub(1))
    }

    /// Natural end of the current track: repeat-one replays the same entry;
    /// otherwise move to the next entry; repeat-all wraps to index 0; with no
    /// next entry, stop (the pointer stays where it is).
    pub fn advance(&mut self, repeat: Repeat) -> Advance {
        let Some(p) = self.pos else {
            return Advance::Stop;
        };
        if repeat == Repeat::One {
            return Advance::Replay;
        }
        if p + 1 < self.tracks.len() {
            self.pos = Some(p + 1);
            return Advance::Next(p + 1);
        }
        if repeat == Repeat::All && !self.tracks.is_empty() {
            self.pos = Some(0);
            return Advance::Next(0);
        }
        Advance::Stop
    }

    /// Remove row `idx` (queue ✕). Keeps the pointer consistent: rows before
    /// it shift it up, rows after it leave it alone, removing the current row
    /// advances to the successor (kept playing by the caller) or stops when
    /// there is none.
    pub fn remove(&mut self, idx: usize) -> RemoveEffect {
        if idx >= self.tracks.len() {
            return RemoveEffect::Nothing;
        }
        self.tracks.remove(idx);
        match self.pos {
            None => RemoveEffect::Nothing,
            Some(p) if idx < p => {
                self.pos = Some(p - 1);
                RemoveEffect::Nothing
            }
            Some(p) if idx > p => RemoveEffect::Nothing,
            Some(p) => {
                if p < self.tracks.len() {
                    RemoveEffect::PlayNext(p)
                } else {
                    self.pos = None;
                    RemoveEffect::Stopped
                }
            }
        }
    }

    /// Move row `from` to final index `to` (▲▼ neighbour steps and ⠿ drag
    /// both land here after their index math). The pointer follows the row
    /// when it is the one moved and compensates when rows cross it; moving
    /// rows entirely above/below the pointer leaves it alone.
    pub fn move_row(&mut self, from: usize, to: usize) {
        let n = self.tracks.len();
        if from >= n || n == 0 {
            return;
        }
        let to = to.min(n - 1);
        if from == to {
            return;
        }
        let t = self.tracks.remove(from);
        self.tracks.insert(to, t);
        self.pos = match self.pos {
            None => None,
            Some(p) if p == from => Some(to),
            Some(p) if from < p && to >= p => Some(p - 1),
            Some(p) if from > p && to <= p => Some(p + 1),
            Some(p) => Some(p),
        };
    }

    /// Shuffle keeps the played region — everything up to and including the
    /// current entry — intact and shuffles only the up-next region
    /// (Spotify-like); with nothing current the whole list shuffles.
    /// Toggling off restores the saved order minus anything removed while
    /// shuffled, and re-points at the same current track.
    pub fn toggle_shuffle(&mut self) {
        if self.shuffle_saved.is_none() {
            self.shuffle_saved = Some(self.tracks.clone());
            let keep = self.pos.map_or(0, |p| p + 1).min(self.tracks.len());
            shuffle_slice(&mut self.tracks[keep..]);
        } else {
            let cur = self.current_id();
            let keep: HashSet<String> = self.tracks.iter().map(|t| t.id.clone()).collect();
            let saved = self.shuffle_saved.take().expect("checked above");
            self.tracks = saved.into_iter().filter(|t| keep.contains(&t.id)).collect();
            self.pos = cur.and_then(|id| self.tracks.iter().position(|t| t.id == id));
        }
    }

    /// Queue panel "clear" / MPRIS Stop: everything goes, pointer None.
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.pos = None;
        self.shuffle_saved = None;
    }
}

/// Fisher–Yates over `v` with a nanoseeded xorshift (deterministic given the
/// seed; the selftest only relies on the toggle round-trip, not the order).
fn shuffle_slice(v: &mut [Track]) {
    let mut seed = std::time::Instant::now().elapsed().as_nanos() as u64 | 1;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for i in (1..v.len()).rev() {
        let j = (rnd() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trk(id: &str) -> Track {
        Track {
            id: id.to_string(),
            name: id.to_string(),
            artist: "a".into(),
            album: String::new(),
            duration_ms: 1_000,
            track_no: 1,
            disc_no: 1,
            artwork: None,
            lossless: false,
        }
    }

    fn mkq(tracks: &[&str], pos: Option<usize>) -> Queue {
        Queue {
            tracks: tracks.iter().map(|s| trk(s)).collect(),
            pos,
            shuffle_saved: None,
        }
    }

    fn ids(q: &Queue) -> Vec<&str> {
        q.tracks.iter().map(|t| t.id.as_str()).collect()
    }

    // ---- append (`+`) ----

    #[test]
    fn append_when_never_started_autostarts_first_new() {
        let mut q = mkq(&[], None);
        assert_eq!(q.append(vec![trk("A"), trk("B")], false), Some(0));
        assert_eq!(q.pos, Some(0));
        assert_eq!(ids(&q), ["A", "B"]);
    }

    #[test]
    fn append_while_playing_only_appends() {
        let mut q = mkq(&["A"], Some(0));
        assert_eq!(q.append(vec![trk("B"), trk("C")], false), None);
        assert_eq!(ids(&q), ["A", "B", "C"]);
        assert_eq!(q.pos, Some(0));
    }

    #[test]
    fn append_finished_at_last_autostarts_new_block() {
        let mut q = mkq(&["A", "B"], Some(1));
        assert_eq!(q.append(vec![trk("C")], true), Some(2));
        assert_eq!(q.pos, Some(2));
    }

    #[test]
    fn append_paused_at_last_does_not_autostart() {
        let mut q = mkq(&["A", "B"], Some(1));
        assert_eq!(q.append(vec![trk("C")], false), None);
        assert_eq!(q.pos, Some(1));
    }

    #[test]
    fn append_paused_mid_queue_never_jumps() {
        let mut q = mkq(&["A", "B", "C"], Some(1));
        assert_eq!(q.append(vec![trk("D")], false), None);
        assert_eq!(ids(&q), ["A", "B", "C", "D"]);
        assert_eq!(q.pos, Some(1));
    }

    #[test]
    fn append_empty_is_a_noop() {
        let mut q = mkq(&["A"], Some(0));
        assert_eq!(q.append(vec![], true), None);
        assert_eq!(q.len(), 1);
    }

    // ---- play now / insert after current ----

    #[test]
    fn insert_after_current_keeps_old_current_listed() {
        let mut q = mkq(&["A", "B", "C"], Some(1));
        assert_eq!(q.insert_after_current(vec![trk("X"), trk("Y")]), 2);
        assert_eq!(ids(&q), ["A", "B", "X", "Y", "C"]);
        assert_eq!(q.pos, Some(2));
    }

    #[test]
    fn insert_when_idle_appends_and_points_at_head() {
        let mut q = mkq(&["A"], None);
        assert_eq!(q.insert_after_current(vec![trk("X"), trk("Y")]), 1);
        assert_eq!(ids(&q), ["A", "X", "Y"]);
        assert_eq!(q.pos, Some(1));
    }

    #[test]
    fn insert_into_empty_queue_starts_at_zero() {
        let mut q = mkq(&[], None);
        assert_eq!(q.insert_after_current(vec![trk("X")]), 0);
        assert_eq!(q.pos, Some(0));
    }

    // ---- natural advance ----

    #[test]
    fn advance_moves_pointer_without_consuming() {
        let mut q = mkq(&["A", "B", "C"], Some(0));
        assert_eq!(q.advance(Repeat::Off), Advance::Next(1));
        assert_eq!(q.pos, Some(1));
        assert_eq!(q.len(), 3, "queue must not be consumed");
    }

    #[test]
    fn advance_repeat_one_replays_in_place() {
        let mut q = mkq(&["A", "B"], Some(0));
        assert_eq!(q.advance(Repeat::One), Advance::Replay);
        assert_eq!(q.pos, Some(0));
    }

    #[test]
    fn advance_repeat_all_wraps_to_zero() {
        let mut q = mkq(&["A", "B", "C"], Some(2));
        assert_eq!(q.advance(Repeat::All), Advance::Next(0));
        assert_eq!(q.pos, Some(0));
    }

    #[test]
    fn advance_at_end_without_repeat_stops_pointer_stays() {
        let mut q = mkq(&["A", "B"], Some(1));
        assert_eq!(q.advance(Repeat::Off), Advance::Stop);
        assert_eq!(q.pos, Some(1), "finished session stays listed");
    }

    #[test]
    fn advance_without_current_stops() {
        let mut q = mkq(&["A"], None);
        assert_eq!(q.advance(Repeat::All), Advance::Stop);
    }

    // ---- peeks / play_at ----

    #[test]
    fn peek_bounds() {
        let q = mkq(&["A", "B", "C"], Some(0));
        assert_eq!(q.peek_next(), Some(1));
        assert_eq!(q.peek_prev(), None);
        let q = mkq(&["A", "B", "C"], Some(2));
        assert_eq!(q.peek_next(), None);
        assert_eq!(q.peek_prev(), Some(1));
    }

    #[test]
    fn play_at_moves_pointer_and_rejects_out_of_range() {
        let mut q = mkq(&["A", "B"], Some(0));
        assert_eq!(q.play_at(1), Some(1));
        assert_eq!(q.pos, Some(1));
        assert_eq!(q.play_at(9), None);
        assert_eq!(q.pos, Some(1));
    }

    // ---- removal ----

    #[test]
    fn remove_before_pointer_shifts_it() {
        let mut q = mkq(&["A", "B", "C"], Some(2));
        assert_eq!(q.remove(0), RemoveEffect::Nothing);
        assert_eq!(ids(&q), ["B", "C"]);
        assert_eq!(q.pos, Some(1));
    }

    #[test]
    fn remove_after_pointer_leaves_it() {
        let mut q = mkq(&["A", "B", "C"], Some(0));
        assert_eq!(q.remove(2), RemoveEffect::Nothing);
        assert_eq!(q.pos, Some(0));
    }

    #[test]
    fn remove_current_plays_successor() {
        let mut q = mkq(&["A", "B", "C"], Some(1));
        assert_eq!(q.remove(1), RemoveEffect::PlayNext(1));
        assert_eq!(ids(&q), ["A", "C"]);
        assert_eq!(q.pos, Some(1));
        assert_eq!(q.current().map(|t| t.id.as_str()), Some("C"));
    }

    #[test]
    fn remove_current_at_end_stops() {
        let mut q = mkq(&["A", "B"], Some(1));
        assert_eq!(q.remove(1), RemoveEffect::Stopped);
        assert_eq!(q.pos, None);
        assert_eq!(ids(&q), ["A"]);
    }

    #[test]
    fn remove_with_no_current_is_noop() {
        let mut q = mkq(&["A"], None);
        assert_eq!(q.remove(0), RemoveEffect::Nothing);
        assert_eq!(q.pos, None);
    }

    #[test]
    fn remove_out_of_range_is_noop() {
        let mut q = mkq(&["A"], Some(0));
        assert_eq!(q.remove(5), RemoveEffect::Nothing);
        assert_eq!(q.pos, Some(0));
    }

    // ---- row moves ----

    #[test]
    fn move_current_row_pointer_follows() {
        let mut q = mkq(&["A", "B", "C"], Some(1));
        q.move_row(1, 2);
        assert_eq!(ids(&q), ["A", "C", "B"]);
        assert_eq!(q.pos, Some(2));
    }

    #[test]
    fn move_row_crossing_down_shifts_pointer_back() {
        let mut q = mkq(&["A", "B", "C"], Some(2));
        q.move_row(0, 2);
        assert_eq!(ids(&q), ["B", "C", "A"]);
        assert_eq!(q.pos, Some(1));
    }

    #[test]
    fn move_row_crossing_up_shifts_pointer_forward() {
        let mut q = mkq(&["A", "B", "C"], Some(0));
        q.move_row(2, 0);
        assert_eq!(ids(&q), ["C", "A", "B"]);
        assert_eq!(q.pos, Some(1));
    }

    #[test]
    fn move_row_within_history_keeps_pointer() {
        let mut q = mkq(&["A", "B", "C", "D"], Some(2));
        q.move_row(0, 1);
        assert_eq!(ids(&q), ["B", "A", "C", "D"]);
        assert_eq!(q.pos, Some(2));
    }

    #[test]
    fn move_row_clamps_destination_and_ignores_bad_source() {
        let mut q = mkq(&["A", "B", "C"], Some(0));
        q.move_row(1, 99);
        assert_eq!(ids(&q), ["A", "C", "B"]);
        q.move_row(99, 0);
        assert_eq!(ids(&q), ["A", "C", "B"]);
        assert_eq!(q.pos, Some(0));
    }

    #[test]
    fn move_row_without_pointer_is_fine() {
        let mut q = mkq(&["A", "B"], None);
        q.move_row(0, 1);
        assert_eq!(ids(&q), ["B", "A"]);
        assert_eq!(q.pos, None);
    }

    // ---- shuffle ----

    #[test]
    fn shuffle_keeps_played_region_and_restores_round_trip() {
        let mut q = mkq(&["A", "B", "C", "D", "E"], Some(2));
        q.toggle_shuffle();
        assert_eq!(ids(&q)[..3], ["A", "B", "C"], "played region stays put");
        assert_eq!(q.pos, Some(2));
        let tail: Vec<&str> = ids(&q)[3..].to_vec();
        assert_eq!(tail.len(), 2);
        let mut sorted = tail.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, ["D", "E"], "tail is a permutation of up-next");
        q.toggle_shuffle();
        assert_eq!(ids(&q), ["A", "B", "C", "D", "E"]);
        assert_eq!(q.pos, Some(2));
    }

    #[test]
    fn shuffle_restore_survives_removals_and_retracks_current() {
        let mut q = mkq(&["A", "B", "C", "D"], Some(1));
        q.toggle_shuffle();
        // remove an up-next row while shuffled
        q.remove(3);
        q.toggle_shuffle();
        assert_eq!(ids(&q), ["A", "B", "C"]);
        assert_eq!(q.pos, Some(1), "pointer re-tracks the current track");
    }

    // ---- clear ----

    #[test]
    fn clear_resets_everything() {
        let mut q = mkq(&["A", "B"], Some(0));
        q.toggle_shuffle();
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.pos, None);
        q.toggle_shuffle(); // must be a fresh shuffle (no stale saved state)
        assert!(q.is_empty());
    }
}
