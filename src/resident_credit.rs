//! Deterministic resident-memory credits shared across model and Session owners.

use std::{
    io,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

/// Product limit shared by one durable attempt and its pending journal data.
pub(super) const MAX_RESIDENT_ATTEMPT_JOURNAL_BYTES: usize = 32 * 1024 * 1024;

// The counter is deliberately conservative and deterministic instead of
// depending on allocator-specific usable-size APIs. Every non-empty backing
// allocation pays one fixed header and is rounded to a common heap alignment.
const HEAP_ALLOCATION_OVERHEAD: usize = 32;
const HEAP_ALIGNMENT: usize = 16;

#[derive(Clone)]
pub(super) struct ResidentCreditPool {
    inner: Arc<ResidentCreditPoolInner>,
}

struct ResidentCreditPoolInner {
    limit: usize,
    used: AtomicUsize,
}

pub(super) struct ResidentCreditLease {
    pool: Arc<ResidentCreditPoolInner>,
    bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ResidentCreditLimit {
    maximum: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChargedBytesError {
    Limit(ResidentCreditLimit),
    Capacity,
}

impl ResidentCreditLimit {
    pub(super) fn maximum(self) -> usize {
        self.maximum
    }
}

impl ResidentCreditPool {
    pub(super) fn for_durable_session() -> Self {
        Self::with_limit(MAX_RESIDENT_ATTEMPT_JOURNAL_BYTES)
    }

    fn with_limit(limit: usize) -> Self {
        Self {
            inner: Arc::new(ResidentCreditPoolInner {
                limit,
                used: AtomicUsize::new(0),
            }),
        }
    }

    pub(super) fn try_acquire(
        &self,
        bytes: usize,
    ) -> Result<ResidentCreditLease, ResidentCreditLimit> {
        let maximum = self.inner.limit;
        self.inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= maximum)
            })
            .map_err(|_| ResidentCreditLimit { maximum })?;
        Ok(ResidentCreditLease {
            pool: Arc::clone(&self.inner),
            bytes,
        })
    }

    #[cfg(test)]
    pub(super) fn with_limit_for_test(limit: usize) -> Self {
        Self::with_limit(limit)
    }

    #[cfg(test)]
    pub(super) fn used_for_test(&self) -> usize {
        self.inner.used.load(Ordering::Acquire)
    }
}

impl ResidentCreditLease {
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Split already charged ownership without changing the pool total.
    pub(super) fn split_off(&mut self, bytes: usize) -> Result<Self, ResidentCreditLimit> {
        if bytes > self.bytes {
            return Err(ResidentCreditLimit {
                maximum: self.pool.limit,
            });
        }
        self.bytes -= bytes;
        Ok(Self {
            pool: Arc::clone(&self.pool),
            bytes,
        })
    }

    /// Merge two unique leases from the same Session pool.
    pub(super) fn merge(&mut self, mut other: Self) -> Result<(), ResidentCreditLimit> {
        if !Arc::ptr_eq(&self.pool, &other.pool) {
            return Err(ResidentCreditLimit {
                maximum: self.pool.limit,
            });
        }
        self.bytes = self
            .bytes
            .checked_add(other.bytes)
            .ok_or(ResidentCreditLimit {
                maximum: self.pool.limit,
            })?;
        other.bytes = 0;
        Ok(())
    }
}

impl Drop for ResidentCreditLease {
    fn drop(&mut self) {
        let previous = self.pool.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

/// One byte buffer whose charged backing moves with its sole owner.
pub(super) struct ChargedBytes {
    bytes: Vec<u8>,
    _lease: ResidentCreditLease,
}

impl ChargedBytes {
    #[cfg(test)]
    pub(super) fn try_new(
        bytes: Vec<u8>,
        pool: &ResidentCreditPool,
    ) -> Result<Self, ResidentCreditLimit> {
        let charge = byte_buffer_charge(bytes.capacity()).ok_or(ResidentCreditLimit {
            maximum: pool.inner.limit,
        })?;
        let lease = pool.try_acquire(charge)?;
        Ok(Self {
            bytes,
            _lease: lease,
        })
    }

    /// Allocate a byte buffer only after its requested backing is charged.
    pub(super) fn try_with_capacity(
        capacity: usize,
        pool: &ResidentCreditPool,
    ) -> Result<Self, ChargedBytesError> {
        let requested = byte_buffer_charge(capacity).ok_or(ChargedBytesError::Capacity)?;
        let mut lease = pool
            .try_acquire(requested)
            .map_err(ChargedBytesError::Limit)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| ChargedBytesError::Capacity)?;
        let actual = byte_buffer_charge(bytes.capacity()).ok_or(ChargedBytesError::Capacity)?;
        match actual.cmp(&requested) {
            std::cmp::Ordering::Greater => {
                let extra = pool
                    .try_acquire(actual - requested)
                    .map_err(ChargedBytesError::Limit)?;
                lease.merge(extra).map_err(ChargedBytesError::Limit)?;
            }
            std::cmp::Ordering::Less => {
                drop(
                    lease
                        .split_off(requested - actual)
                        .map_err(ChargedBytesError::Limit)?,
                );
            }
            std::cmp::Ordering::Equal => {}
        }
        Ok(Self {
            bytes,
            _lease: lease,
        })
    }

    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    #[cfg(test)]
    pub(super) fn charge(&self) -> usize {
        self._lease.bytes()
    }

    /// Append only when the existing charged allocation is already large enough.
    pub(super) fn extend_from_slice_in_place(&mut self, source: &[u8]) -> bool {
        if self.capacity().saturating_sub(self.len()) < source.len() {
            return false;
        }
        self.bytes.extend_from_slice(source);
        true
    }

    pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
        self.bytes.as_mut_slice()
    }

    pub(super) fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
    }

    pub(super) fn push_in_place(&mut self, byte: u8) -> bool {
        if self.len() == self.capacity() {
            return false;
        }
        self.bytes.push(byte);
        true
    }

    /// Split physical bytes from their unique charge without changing the
    /// pool total. The writer flight keeps the lease while the command owns
    /// the bytes, so cancellation cannot release or duplicate either owner.
    pub(super) fn into_parts(self) -> (Vec<u8>, ResidentCreditLease) {
        let Self { bytes, _lease } = self;
        (bytes, _lease)
    }
}

impl io::Write for ChargedBytes {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if !self.extend_from_slice_in_place(buffer) {
            return Err(io::Error::other("charged byte buffer capacity exceeded"));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Deref for ChargedBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.bytes.as_slice()
    }
}

/// Conservative charge for one `Vec<u8>` backing allocation.
pub(super) fn byte_buffer_charge(capacity: usize) -> Option<usize> {
    heap_allocation_charge(capacity, 1)
}

/// Deterministic charge for one heap allocation with `capacity` payload bytes.
/// Deterministic product charge for one allocation.
///
/// Stable Rust does not expose an allocator's actual size class. This policy
/// therefore rounds by at least the common heap alignment and adds one fixed
/// allowance for allocator metadata. It is deliberately conservative and is
/// shared by every owner that participates in the 32 MiB Session budget.
fn heap_allocation_charge(size: usize, align: usize) -> Option<usize> {
    if size == 0 {
        return Some(0);
    }
    let align = align.max(HEAP_ALIGNMENT);
    let rounded = round_up(size, align)?;
    rounded.checked_add(HEAP_ALLOCATION_OVERHEAD)
}

fn round_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

#[cfg(test)]
mod tests {
    use super::{ChargedBytes, ResidentCreditPool, byte_buffer_charge};

    #[test]
    fn resident_credit_exact_one_over_and_drop_are_atomic() {
        let charge = byte_buffer_charge(64).unwrap();
        assert_eq!(charge, 96);
        let pool = ResidentCreditPool::with_limit_for_test(charge);
        let lease = pool.try_acquire(charge).unwrap();
        assert_eq!(pool.used_for_test(), charge);
        let Err(error) = pool.try_acquire(1) else {
            panic!("one byte over the resident limit must fail");
        };
        assert_eq!(error.maximum(), charge);
        assert_eq!(pool.used_for_test(), charge);
        drop(lease);
        assert_eq!(pool.used_for_test(), 0);

        let empty = pool.try_acquire(0).unwrap();
        assert_eq!(pool.used_for_test(), 0);
        drop(empty);
    }

    #[test]
    fn byte_buffer_charge_handles_empty_alignment_and_overflow() {
        assert_eq!(byte_buffer_charge(0), Some(0));
        assert_eq!(byte_buffer_charge(1), Some(48));
        assert_eq!(byte_buffer_charge(16), Some(48));
        assert_eq!(byte_buffer_charge(17), Some(64));
        assert_eq!(byte_buffer_charge(usize::MAX), None);
    }

    #[test]
    fn resident_credit_split_merge_and_charged_capacity_preserve_the_total() {
        let pool = ResidentCreditPool::with_limit_for_test(1024);
        let mut whole = pool.try_acquire(512).unwrap();
        let part = whole.split_off(128).unwrap();
        assert_eq!(whole.bytes(), 384);
        assert_eq!(part.bytes(), 128);
        assert_eq!(pool.used_for_test(), 512);
        whole.merge(part).unwrap();
        assert_eq!(whole.bytes(), 512);
        assert_eq!(pool.used_for_test(), 512);
        drop(whole);
        assert_eq!(pool.used_for_test(), 0);

        let mut bytes = ChargedBytes::try_with_capacity(17, &pool).unwrap();
        assert_eq!(
            bytes.charge(),
            byte_buffer_charge(bytes.capacity()).unwrap()
        );
        assert!(bytes.extend_from_slice_in_place(b"12345678901234567"));
        assert!(!bytes.extend_from_slice_in_place(b"x"));
        drop(bytes);
        assert_eq!(pool.used_for_test(), 0);
    }
}
