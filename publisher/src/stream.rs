//! One terminal's outgoing byte stream: offsets, retention and resynchronisation.
//!
//! This lives with whatever owns the pseudo-terminal, never with whatever owns the
//! relay connection, and that placement is the whole point of the module. The relay
//! accepts an output frame only when its start offset equals the terminal's
//! `next_offset` (spec §6.1), so bytes must be retained until acknowledged durable and
//! sent again from wherever the relay says the stream continues. If the retained bytes
//! lived in a process that can die independently of the shell — a multiplexing daemon,
//! say — then that process dying would leave the relay's offsets contiguous while the
//! bytes behind them were simply gone: a hole in the stream that nothing anywhere
//! could detect. Keeping them beside the byte source makes that failure impossible.

use std::collections::VecDeque;

/// Bytes sent but not yet acknowledged durable, kept so they can be sent again.
pub struct Retained {
    /// Offset of the first byte in `bytes`.
    start: u64,
    bytes: VecDeque<u8>,
}

impl Default for Retained {
    fn default() -> Self {
        Self::new()
    }
}

impl Retained {
    pub fn new() -> Self {
        Self {
            start: 0,
            bytes: VecDeque::new(),
        }
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn append(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk.iter().copied());
    }

    /// Forget everything the relay has committed to storage.
    pub fn release_through(&mut self, durable_offset: u64) {
        if durable_offset <= self.start {
            return;
        }
        let drop_count = (durable_offset - self.start).min(self.bytes.len() as u64) as usize;
        self.bytes.drain(..drop_count);
        self.start += drop_count as u64;
    }

    /// The retained bytes from `offset` onwards, for retransmission.
    ///
    /// `None` means those bytes are gone. Saying so is essential: inventing them here
    /// would corrupt the stream for every subscriber, silently and permanently.
    pub fn from(&self, offset: u64) -> Option<Vec<u8>> {
        if offset < self.start || offset > self.end() {
            return None;
        }
        let skip = (offset - self.start) as usize;
        Some(self.bytes.iter().skip(skip).copied().collect())
    }

    /// Start again from `offset`, keeping nothing. Used when the relay is ahead of
    /// anything retained, which happens when an earlier process published under the
    /// same `local_ref`: there is nothing to resend and nothing that needs resending.
    pub fn restart_at(&mut self, offset: u64) {
        self.start = offset;
        self.bytes.clear();
    }
}

/// What the caller should do next with a terminal's stream.
#[derive(Debug, PartialEq, Eq)]
pub enum Resync {
    /// Send these bytes again, starting at the given offset.
    Retransmit { start: u64, bytes: Vec<u8> },
    /// Nothing to do: the relay is where this side thought it was.
    Nothing,
    /// The bytes the relay is asking for have been released and cannot be rebuilt.
    /// The stream is unrecoverable and the terminal must be closed rather than spliced.
    Unrecoverable,
}

/// The sending half of one terminal's stream.
pub struct Outgoing {
    retained: Retained,
    /// Where the next byte handed to the relay belongs.
    next_offset: u64,
    /// Whether the relay has said where the stream continues. Nothing is framed
    /// against a guess: a wrong start offset is refused, not merged.
    positioned: bool,
    /// The last offset a resynchronisation was aimed at. The relay answers *every*
    /// frame that was already in flight when it first complained, each repeating the
    /// same authoritative offset; acting on more than the first would have the two
    /// sides trading retransmissions for as long as output kept flowing.
    resync_target: Option<u64>,
    /// The relay's per-terminal unacknowledged window, from `ready`. Exceeding it gets
    /// the whole connection closed (spec §7.2), so it is a hard gate, not a hint.
    window: u64,
}

/// Until the relay says otherwise, assume the smallest window its schema allows, so
/// this side can never be the one that overshoots.
const CONSERVATIVE_WINDOW: u64 = 64 * 1024;

impl Default for Outgoing {
    fn default() -> Self {
        Self::new()
    }
}

impl Outgoing {
    pub fn new() -> Self {
        Self {
            retained: Retained::new(),
            next_offset: 0,
            positioned: false,
            resync_target: None,
            window: CONSERVATIVE_WINDOW,
        }
    }

    pub fn positioned(&self) -> bool {
        self.positioned
    }

    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    pub fn unacked(&self) -> u64 {
        self.next_offset.saturating_sub(self.retained.start())
    }

    /// True while the relay will accept more from this terminal.
    pub fn may_send(&self) -> bool {
        self.positioned && self.unacked() < self.window
    }

    pub fn set_window(&mut self, window: u64) {
        self.window = window.max(CONSERVATIVE_WINDOW);
    }

    /// The connection is gone. Nothing may be framed until the relay says again where
    /// this stream continues; the retained bytes are exactly what survives that.
    pub fn detached(&mut self) {
        self.positioned = false;
        self.resync_target = None;
    }

    /// The relay has (re)opened this terminal and says the stream continues at
    /// `next_offset`. Returns whatever has to be sent again from there.
    pub fn attached(&mut self, next_offset: u64) -> Resync {
        self.positioned = true;
        self.resync_target = None;
        // Nothing has been sent yet, so wherever the relay is, that is where this
        // stream starts.
        if self.retained.is_empty() && self.retained.start() == 0 && self.next_offset == 0 {
            self.retained.restart_at(next_offset);
        }

        // Behind bytes this side has already released. `from` answers `None` to both
        // "before the window" and "past the end", and the two could hardly be more
        // different: past the end means an earlier process got further and there is
        // nothing to resend, while *before* the window means the relay has lost output
        // it already acknowledged durable. Resuming there would send the same offsets
        // twice under different bytes, which no subscriber could ever detect.
        if next_offset < self.retained.start() {
            return Resync::Unrecoverable;
        }

        match self.retained.from(next_offset) {
            Some(bytes) => {
                self.next_offset = next_offset;
                if bytes.is_empty() {
                    Resync::Nothing
                } else {
                    let start = next_offset;
                    self.next_offset = next_offset + bytes.len() as u64;
                    Resync::Retransmit { start, bytes }
                }
            }
            None => {
                // Past the end: an earlier process published under this local_ref and
                // the relay kept bytes this one never saw. Nothing to resend, and the
                // stream is still contiguous.
                self.retained.restart_at(next_offset);
                self.next_offset = next_offset;
                Resync::Nothing
            }
        }
    }

    /// Hand `chunk` to the stream. Returns the offset it starts at, or `None` when the
    /// terminal is not positioned and the bytes must wait.
    pub fn send(&mut self, chunk: &[u8]) -> Option<u64> {
        if !self.positioned {
            return None;
        }
        let start = self.next_offset;
        self.retained.append(chunk);
        self.next_offset += chunk.len() as u64;
        Some(start)
    }

    /// Bytes the relay has committed. Everything up to here can be forgotten.
    pub fn acknowledge(&mut self, durable_offset: u64) {
        self.retained.release_through(durable_offset);
    }

    /// The relay refused a frame and named where it actually is (spec §6.1). It did
    /// not append, so this side is the one that has to move.
    pub fn mismatch(&mut self, authoritative: u64) -> Resync {
        if self.resync_target == Some(authoritative) {
            // An echo: this is one of the frames that was already in flight when the
            // first refusal was raised, and it has already been answered.
            return Resync::Nothing;
        }
        self.resync_target = Some(authoritative);
        match self.retained.from(authoritative) {
            Some(bytes) => {
                self.next_offset = authoritative + bytes.len() as u64;
                if bytes.is_empty() {
                    Resync::Nothing
                } else {
                    Resync::Retransmit {
                        start: authoritative,
                        bytes,
                    }
                }
            }
            None => Resync::Unrecoverable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledged_bytes_are_released_and_the_rest_kept() {
        let mut retained = Retained::new();
        retained.append(b"abcdefgh");
        assert_eq!(retained.end(), 8);

        retained.release_through(3);
        assert_eq!(retained.start(), 3);
        assert_eq!(retained.from(3).unwrap(), b"defgh");
        // Everything from the acknowledged point on must still be retransmittable.
        assert_eq!(retained.from(5).unwrap(), b"fgh");
    }

    #[test]
    fn an_offset_below_what_was_released_cannot_be_reconstructed() {
        let mut retained = Retained::new();
        retained.append(b"abcdefgh");
        retained.release_through(4);
        assert!(retained.from(2).is_none());
    }

    #[test]
    fn an_offset_past_the_end_is_refused() {
        let mut retained = Retained::new();
        retained.append(b"abc");
        assert!(retained.from(99).is_none());
        assert_eq!(retained.from(3).unwrap(), b"");
    }

    #[test]
    fn releasing_beyond_what_is_held_does_not_run_past_the_buffer() {
        let mut retained = Retained::new();
        retained.append(b"abc");
        retained.release_through(1000);
        assert_eq!(retained.start(), 3);
        assert!(retained.is_empty());
    }

    #[test]
    fn nothing_is_framed_before_the_relay_says_where_the_stream_is() {
        let mut stream = Outgoing::new();
        assert!(!stream.may_send());
        // Framing against a guess would be refused by the relay, not merged.
        assert_eq!(stream.send(b"hello"), None);
    }

    #[test]
    fn a_reconnect_resends_exactly_what_was_not_committed() {
        let mut stream = Outgoing::new();
        stream.attached(0);
        stream.send(b"hello ");
        stream.send(b"world");
        stream.acknowledge(6);
        assert_eq!(stream.unacked(), 5);

        stream.detached();
        // The relay restarted and fell back to what it had committed.
        assert_eq!(
            stream.attached(6),
            Resync::Retransmit {
                start: 6,
                bytes: b"world".to_vec()
            }
        );
        assert_eq!(stream.next_offset(), 11);
    }

    #[test]
    fn a_relay_that_has_gone_backwards_past_the_window_is_not_spliced_over() {
        let mut stream = Outgoing::new();
        stream.attached(0);
        stream.send(b"0123456789");
        stream.acknowledge(8);
        stream.detached();
        // The relay has lost output it already called durable. Continuing from 4 would
        // put different bytes at offsets 4..8 than the ones subscribers already hold,
        // and nothing downstream could ever tell.
        assert_eq!(stream.attached(4), Resync::Unrecoverable);
    }

    #[test]
    fn a_relay_that_is_ahead_of_everything_retained_simply_continues() {
        let mut stream = Outgoing::new();
        // A previous process published under this local_ref and got to 4096.
        assert_eq!(stream.attached(4096), Resync::Nothing);
        assert_eq!(stream.next_offset(), 4096);
        assert_eq!(stream.send(b"more"), Some(4096));
    }

    #[test]
    fn repeated_mismatches_for_one_refusal_are_answered_once() {
        let mut stream = Outgoing::new();
        stream.attached(0);
        stream.send(b"0123456789");

        // Three frames were in flight; the relay refuses each and reports the same
        // authoritative offset every time. Answering more than once would have the two
        // sides trading retransmissions for as long as output kept flowing.
        assert_eq!(
            stream.mismatch(4),
            Resync::Retransmit {
                start: 4,
                bytes: b"456789".to_vec()
            }
        );
        assert_eq!(stream.mismatch(4), Resync::Nothing);
        assert_eq!(stream.mismatch(4), Resync::Nothing);
    }

    #[test]
    fn a_mismatch_below_what_was_released_is_unrecoverable_not_spliced() {
        let mut stream = Outgoing::new();
        stream.attached(0);
        stream.send(b"0123456789");
        stream.acknowledge(8);
        // Resuming from 2 would need bytes that are gone. Splicing the stream is worse
        // than closing the terminal, because nothing downstream could ever detect it.
        assert_eq!(stream.mismatch(2), Resync::Unrecoverable);
    }

    #[test]
    fn the_unacknowledged_window_gates_sending() {
        let mut stream = Outgoing::new();
        stream.set_window(128 * 1024);
        stream.attached(0);
        assert!(stream.may_send());
        stream.send(&[0u8; 200 * 1024]);
        // Over the relay's window: continuing would have the connection closed for
        // every terminal on it, not just this one.
        assert!(!stream.may_send());
        stream.acknowledge(200 * 1024);
        assert!(stream.may_send());
    }

    #[test]
    fn the_window_never_drops_below_the_schema_minimum() {
        let mut stream = Outgoing::new();
        stream.set_window(1);
        stream.attached(0);
        stream.send(&[0u8; 1024]);
        assert!(stream.may_send());
    }
}
