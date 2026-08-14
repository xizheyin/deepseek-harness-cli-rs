use std::collections::VecDeque;

/// A bounded byte suffix. Bytes are decoded only by the shell result renderer.
pub(super) struct TailCapture {
    bytes: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl TailCapture {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(limit),
            limit,
            truncated: false,
        }
    }

    pub(super) fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if self.limit == 0 {
            self.truncated = true;
            return;
        }
        if chunk.len() >= self.limit {
            self.truncated |= !self.bytes.is_empty() || chunk.len() > self.limit;
            self.bytes.clear();
            self.bytes.extend(
                chunk[chunk.len().saturating_sub(self.limit)..]
                    .iter()
                    .copied(),
            );
            return;
        }

        self.bytes.extend(chunk.iter().copied());
        let overflow = self.bytes.len().saturating_sub(self.limit);
        if overflow != 0 {
            self.bytes.drain(..overflow);
            self.truncated = true;
        }
    }

    pub(super) fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    pub(super) fn finish(self) -> (Vec<u8>, bool) {
        (self.bytes.into_iter().collect(), self.truncated)
    }
}

/// Counts both streams together and permits exactly one byte past the ceiling.
pub(super) struct ObservedBudget {
    observed: usize,
    limit: usize,
    exceeded: bool,
}

impl ObservedBudget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            observed: 0,
            limit,
            exceeded: false,
        }
    }

    pub(super) fn next_read_len(&self, chunk_limit: usize) -> usize {
        if self.exceeded {
            return 0;
        }
        self.limit
            .saturating_sub(self.observed)
            .saturating_add(1)
            .min(chunk_limit)
    }

    /// Returns true once the first over-limit byte has been observed.
    pub(super) fn record(&mut self, count: usize) -> bool {
        self.observed = self.observed.saturating_add(count);
        self.exceeded |= self.observed > self.limit;
        self.exceeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_accepts_the_exact_limit_without_truncation() {
        let mut tail = TailCapture::new(4);
        tail.push(b"abcd");

        assert_eq!(tail.finish(), (b"abcd".to_vec(), false));
    }

    #[test]
    fn tail_keeps_only_the_last_bytes_after_the_limit() {
        let mut tail = TailCapture::new(4);
        tail.push(b"abc");
        tail.push(b"def");

        assert_eq!(tail.finish(), (b"cdef".to_vec(), true));
    }

    #[test]
    fn aggregate_budget_requests_one_detection_byte() {
        let mut budget = ObservedBudget::new(4);
        assert_eq!(budget.next_read_len(8), 5);
        assert!(!budget.record(4));
        assert_eq!(budget.next_read_len(8), 1);
        assert!(budget.record(1));
        assert_eq!(budget.next_read_len(8), 0);
    }
}
