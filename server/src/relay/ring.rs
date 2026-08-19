//! Bounded in-memory replay buffer (spec §7.1).
//!
//! The buffer always holds a contiguous suffix of a terminal's output stream:
//!
//! ```text
//! retained_bytes = end_offset - earliest_offset  <=  capacity
//! ```

use std::collections::VecDeque;

pub struct Ring {
    buf: VecDeque<u8>,
    earliest_offset: u64,
    capacity: usize,
}

impl Ring {
    pub fn new(capacity: usize, earliest_offset: u64) -> Self {
        Self {
            buf: VecDeque::new(),
            earliest_offset,
            capacity: capacity.max(1),
        }
    }

    /// Rebuild from a durable checkpoint.
    pub fn from_retained(capacity: usize, earliest_offset: u64, retained: Vec<u8>) -> Self {
        let mut ring = Self::new(capacity, earliest_offset);
        // A stored suffix longer than a since-reduced capacity is trimmed to the
        // newest bytes, preserving the "newest contiguous suffix" invariant.
        if retained.len() > ring.capacity {
            let drop = retained.len() - ring.capacity;
            ring.earliest_offset += drop as u64;
            ring.buf.extend(&retained[drop..]);
        } else {
            ring.buf.extend(retained);
        }
        ring
    }

    pub fn earliest_offset(&self) -> u64 {
        self.earliest_offset
    }

    pub fn end_offset(&self) -> u64 {
        self.earliest_offset + self.buf.len() as u64
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Apply a capacity change from a settings update, evicting immediately when the
    /// new capacity is smaller. Returns the number of bytes evicted.
    pub fn set_capacity(&mut self, capacity: usize) -> usize {
        let capacity = capacity.max(1);
        if capacity == self.capacity {
            return 0;
        }
        self.capacity = capacity;
        let overflow = self.buf.len().saturating_sub(capacity);
        if overflow > 0 {
            self.buf.drain(..overflow);
            self.earliest_offset += overflow as u64;
        }
        overflow
    }

    /// Append payload, evicting the oldest bytes first when the window would
    /// overflow. Returns the number of bytes evicted.
    ///
    /// A single frame larger than the whole window is accepted, as the
    /// specification requires: the stream advances by the frame's full length and
    /// only its last `capacity` bytes are retained.
    pub fn append(&mut self, data: &[u8]) -> usize {
        if data.is_empty() {
            return 0;
        }
        let new_end = self.end_offset() + data.len() as u64;

        if data.len() >= self.capacity {
            let evicted = self.buf.len() + (data.len() - self.capacity);
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.capacity..]);
            self.earliest_offset = new_end - self.capacity as u64;
            return evicted;
        }

        let overflow = (self.buf.len() + data.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.buf.drain(..overflow);
            self.earliest_offset += overflow as u64;
        }
        self.buf.extend(data);
        overflow
    }

    /// Copy out `[from, to)`, or `None` if any of it is outside the retained window.
    pub fn read_range(&self, from: u64, to: u64) -> Option<Vec<u8>> {
        if to < from || from < self.earliest_offset || to > self.end_offset() {
            return None;
        }
        let start = (from - self.earliest_offset) as usize;
        let len = (to - from) as usize;
        let mut out = Vec::with_capacity(len);

        // Copy via the deque's contiguous halves rather than byte-by-byte.
        let (first, second) = self.buf.as_slices();
        let from_first = start.min(first.len());
        let take_first = (first.len() - from_first).min(len);
        out.extend_from_slice(&first[from_first..from_first + take_first]);

        if out.len() < len {
            let remaining = len - out.len();
            let from_second = start.saturating_sub(first.len());
            out.extend_from_slice(&second[from_second..from_second + remaining]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_back() {
        let mut ring = Ring::new(16, 0);
        assert_eq!(ring.append(b"hello"), 0);
        assert_eq!(ring.append(b" world"), 0);
        assert_eq!(ring.earliest_offset(), 0);
        assert_eq!(ring.end_offset(), 11);
        assert_eq!(ring.read_range(0, 11).unwrap(), b"hello world");
        assert_eq!(ring.read_range(6, 11).unwrap(), b"world");
    }

    #[test]
    fn evicts_oldest_and_keeps_newest_suffix() {
        let mut ring = Ring::new(8, 0);
        ring.append(b"abcdefgh");
        assert_eq!(ring.append(b"ijk"), 3);
        assert_eq!(ring.earliest_offset(), 3);
        assert_eq!(ring.end_offset(), 11);
        assert_eq!(ring.len(), 8);
        assert_eq!(ring.read_range(3, 11).unwrap(), b"defghijk");
        // Evicted bytes are no longer readable.
        assert!(ring.read_range(2, 11).is_none());
    }

    #[test]
    fn frame_larger_than_capacity_advances_fully_and_retains_tail() {
        let mut ring = Ring::new(4, 100);
        ring.append(b"xy");
        // 10 bytes into a 4-byte window: offsets advance by 10, tail is retained.
        let evicted = ring.append(b"0123456789");
        assert_eq!(evicted, 2 + 6);
        assert_eq!(ring.end_offset(), 112);
        assert_eq!(ring.earliest_offset(), 108);
        assert_eq!(ring.read_range(108, 112).unwrap(), b"6789");
    }

    #[test]
    fn eviction_may_split_a_previously_appended_frame() {
        let mut ring = Ring::new(6, 0);
        ring.append(b"aaaa");
        ring.append(b"bbbb");
        // The first frame is half evicted; the retained range still starts mid-frame.
        assert_eq!(ring.earliest_offset(), 2);
        assert_eq!(ring.read_range(2, 8).unwrap(), b"aabbbb");
    }

    #[test]
    fn shrinking_capacity_evicts_immediately() {
        let mut ring = Ring::new(16, 0);
        ring.append(b"0123456789");
        assert_eq!(ring.set_capacity(4), 6);
        assert_eq!(ring.earliest_offset(), 6);
        assert_eq!(ring.read_range(6, 10).unwrap(), b"6789");
    }

    #[test]
    fn read_range_spans_the_wrapped_halves() {
        let mut ring = Ring::new(8, 0);
        ring.append(b"abcdefgh");
        ring.append(b"ijkl"); // forces the deque to wrap
        assert_eq!(ring.read_range(4, 12).unwrap(), b"efghijkl");
        assert_eq!(ring.read_range(6, 9).unwrap(), b"ghi");
    }

    #[test]
    fn rebuild_from_retained_trims_to_capacity() {
        let ring = Ring::from_retained(4, 10, b"abcdefgh".to_vec());
        assert_eq!(ring.earliest_offset(), 14);
        assert_eq!(ring.end_offset(), 18);
        assert_eq!(ring.read_range(14, 18).unwrap(), b"efgh");
    }

    #[test]
    fn empty_append_is_a_no_op() {
        let mut ring = Ring::new(8, 5);
        assert_eq!(ring.append(b""), 0);
        assert_eq!(ring.end_offset(), 5);
        assert!(ring.is_empty());
    }
}
