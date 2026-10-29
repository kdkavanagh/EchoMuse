//! A bounded store of one stream epoch's samples, addressed by stream position.
//!
//! Every position is in one of four states: held (with a tag saying what the samples are), told
//! missing, not yet arrived, or older than the store keeps. Reads report the worst state in the
//! range, so a missing or evicted sample is never read as silence and a sample that has not
//! arrived yet is waited for rather than guessed.

/// A position the device reported missing, or that was skipped over.
const MISSING: u8 = 0;

/// What a read found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Read {
    /// Every sample is held; `tags` is the bitwise OR of their tags.
    Held { tags: u8 },
    /// Some sample is missing, evicted, or before the stream's first sample, and never will be
    /// held.
    Missing,
    /// Some sample has not arrived yet, and none is missing.
    Pending,
}

/// The samples of one stream epoch, in a ring.
#[derive(Clone, Debug)]
pub(crate) struct SampleStore<T> {
    data: Vec<T>,
    tags: Vec<u8>,
    /// The oldest position held (the stream's first position until the ring wraps).
    start: u64,
    /// Every position before this is held or missing.
    end: u64,
    /// Whether any position has arrived yet.
    begun: bool,
    /// Nothing at or after `end` will ever arrive.
    closed: bool,
}

impl<T: Copy + Default> SampleStore<T> {
    /// A store that keeps the newest `capacity` positions.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            data: vec![T::default(); capacity],
            tags: vec![MISSING; capacity],
            start: 0,
            end: 0,
            begun: false,
            closed: false,
        }
    }

    /// Forgets everything, keeping the allocation.
    pub(crate) fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
        self.begun = false;
        self.closed = false;
    }

    /// Every position before this has arrived or is known missing.
    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// Declares that nothing at or after the current end will arrive.
    pub(crate) fn close(&mut self) {
        self.closed = true;
    }

    /// Whether the store was closed.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    fn capacity(&self) -> u64 {
        self.data.len() as u64
    }

    /// Makes room for position `end` and returns its slot.
    fn advance(&mut self) -> usize {
        let slot = (self.end % self.capacity()) as usize;
        self.end += 1;
        if self.end - self.start > self.capacity() {
            self.start = self.end - self.capacity();
        }
        slot
    }

    fn begin_at(&mut self, first: u64) {
        if !self.begun {
            self.begun = true;
            self.start = first;
            self.end = first;
        }
    }

    /// Appends samples from `first` with `tag` (nonzero). Positions before the current end are
    /// already decided and are skipped (the first copy wins); a hole between the end and
    /// `first` is recorded as missing, never filled.
    pub(crate) fn append(&mut self, first: u64, tag: u8, samples: impl IntoIterator<Item = T>) {
        debug_assert_ne!(tag, MISSING);
        self.begin_at(first);
        if first > self.end {
            self.append_missing(first);
        }
        let skip = self.end.saturating_sub(first);
        for s in samples.into_iter().skip(skip as usize) {
            let slot = self.advance();
            self.data[slot] = s;
            self.tags[slot] = tag;
        }
    }

    /// Appends `n` copies of `value` from `first` with `tag`.
    pub(crate) fn append_fill(&mut self, first: u64, n: u64, tag: u8, value: T) {
        self.append(first, tag, std::iter::repeat_n(value, n as usize));
    }

    /// Records `[first, end)` missing. Positions before the current end are already decided.
    pub(crate) fn mark_missing(&mut self, first: u64, end: u64) {
        self.begin_at(first);
        self.append_missing(end);
    }

    fn append_missing(&mut self, end: u64) {
        // A hole longer than the ring only needs the ring's worth of slots written.
        if end > self.end + self.capacity() {
            self.end = end - self.capacity();
            self.start = self.end;
        }
        while self.end < end {
            let slot = self.advance();
            self.tags[slot] = MISSING;
        }
    }

    /// The state of `[from, to)` (positions may be negative: before the epoch began).
    pub(crate) fn state(&self, from: i64, to: i64) -> Read {
        if to <= from {
            return Read::Held { tags: 0 };
        }
        if !self.begun {
            return if self.closed {
                Read::Missing
            } else {
                Read::Pending
            };
        }
        if from < self.start as i64 {
            return Read::Missing;
        }
        let held_to = to.min(self.end as i64);
        let mut tags = 0;
        for p in from..held_to {
            let t = self.tags[(p as u64 % self.capacity()) as usize];
            if t == MISSING {
                return Read::Missing;
            }
            tags |= t;
        }
        if to > self.end as i64 {
            return if self.closed {
                Read::Missing
            } else {
                Read::Pending
            };
        }
        Read::Held { tags }
    }

    /// Copies `[from, from + dst.len())`, which must be held, into `dst`.
    pub(crate) fn copy_to(&self, from: i64, dst: &mut [T]) {
        let cap = self.capacity();
        for (i, d) in dst.iter_mut().enumerate() {
            *d = self.data[((from as u64 + i as u64) % cap) as usize];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_report_held_missing_and_pending() {
        let mut s = SampleStore::<f32>::new(100);
        assert_eq!(s.state(0, 10), Read::Pending);
        s.append(10, 1, (0..20).map(|i| i as f32));
        assert_eq!(s.state(10, 30), Read::Held { tags: 1 });
        assert_eq!(s.state(5, 15), Read::Missing, "before the first sample");
        assert_eq!(s.state(25, 35), Read::Pending);
        // A hole is missing, not silence.
        s.append(40, 2, [1.0; 10]);
        assert_eq!(s.state(29, 31), Read::Missing);
        assert_eq!(s.state(40, 50), Read::Held { tags: 2 });
        let mut dst = [0.0; 3];
        s.copy_to(12, &mut dst);
        assert_eq!(dst, [2.0, 3.0, 4.0]);
        // Once closed, what never arrived is missing.
        s.close();
        assert_eq!(s.state(45, 55), Read::Missing);
    }

    #[test]
    fn duplicates_keep_the_first_copy_and_the_ring_evicts() {
        let mut s = SampleStore::<f32>::new(8);
        s.append(0, 1, [1.0; 6]);
        s.append(4, 1, [9.0; 4]);
        let mut dst = [0.0; 8];
        s.copy_to(0, &mut dst[..8]);
        assert_eq!(&dst[..8], &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 9.0, 9.0]);
        s.append(8, 1, [2.0; 4]);
        assert_eq!(s.state(3, 12), Read::Missing, "evicted");
        assert_eq!(s.state(4, 12), Read::Held { tags: 1 });
        s.mark_missing(12, 1_000);
        assert_eq!(s.end(), 1_000);
        assert_eq!(s.state(992, 1_000), Read::Missing);
    }
}
