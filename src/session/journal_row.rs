//! Fixed-size identity facts for one complete durable JSONL event row.

use aws_lc_rs::digest::{Context, SHA256};

use super::{EventSeq, jsonl::MAX_JOURNAL_EVENT_LINE_BYTES};

const RAW_ROW_DIGEST_DOMAIN: &[u8] = b"dsh.raw-journal-row.v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RawRowDigest([u8; 32]);

pub(super) struct RawRowHasher(Context);

impl RawRowHasher {
    pub(super) fn new() -> Self {
        let mut context = Context::new(&SHA256);
        context.update(RAW_ROW_DIGEST_DOMAIN);
        Self(context)
    }

    pub(super) fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub(super) fn finish(self) -> RawRowDigest {
        let digest = self.0.finish();
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(digest.as_ref());
        RawRowDigest(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct JournalRowLocator {
    seq: EventSeq,
    offset: u64,
    length: u32,
    full_sha256: RawRowDigest,
}

impl JournalRowLocator {
    pub(super) fn new(seq: EventSeq, offset: u64, row: &[u8]) -> Option<Self> {
        if row.is_empty()
            || row.len() > MAX_JOURNAL_EVENT_LINE_BYTES
            || row.last() != Some(&b'\n')
            || row[..row.len() - 1].contains(&b'\n')
        {
            return None;
        }
        let length = u32::try_from(row.len()).ok()?;
        offset.checked_add(u64::from(length))?;
        Some(Self {
            seq,
            offset,
            length,
            full_sha256: raw_row_sha256(row),
        })
    }

    pub(super) fn seq(self) -> EventSeq {
        self.seq
    }

    pub(super) fn offset(self) -> u64 {
        self.offset
    }

    pub(super) fn length(self) -> u32 {
        self.length
    }

    pub(super) fn end(self) -> Option<u64> {
        self.offset.checked_add(u64::from(self.length))
    }

    pub(super) fn full_sha256(self) -> RawRowDigest {
        self.full_sha256
    }
}

pub(super) fn raw_row_sha256(row: &[u8]) -> RawRowDigest {
    let mut hasher = RawRowHasher::new();
    hasher.update(row);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::{JournalRowLocator, raw_row_sha256};
    use crate::session::EventSeq;

    #[test]
    fn locator_covers_one_complete_line_and_checks_offset() {
        let row = b"{\"type\":\"session/end-seed\"}\n";
        let seq = EventSeq::new(0).unwrap();
        let locator = JournalRowLocator::new(seq, 17, row).unwrap();
        assert_eq!(locator.seq(), seq);
        assert_eq!(locator.offset(), 17);
        assert_eq!(locator.length(), row.len() as u32);
        assert_eq!(locator.end(), Some(17 + row.len() as u64));
        assert_eq!(locator.full_sha256(), raw_row_sha256(row));
        assert!(JournalRowLocator::new(seq, 0, b"").is_none());
        assert!(JournalRowLocator::new(seq, 0, b"{}\n{}\n").is_none());
        assert!(JournalRowLocator::new(seq, u64::MAX, row).is_none());
    }
}
