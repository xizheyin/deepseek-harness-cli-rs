//! Bounded physical JSONL encoding for durable session artifacts.

use std::io;

use serde::ser::{SerializeMap as _, Serializer as _};
use thiserror::Error;

use super::{MAX_SAFE_INTEGER, PreparedEvent, SessionEvent, SessionHeader};

const MAX_TIMESTAMP_DIGITS: &[u8] = b"9007199254740991";

/// Complete tagged header line, including its terminating LF.
pub(crate) const MAX_JOURNAL_HEADER_LINE_BYTES: usize = 64 * 1024;
/// Complete event line, including its terminating LF.
pub(crate) const MAX_JOURNAL_EVENT_LINE_BYTES: usize = 9 * 1024 * 1024;

#[derive(Debug, Error)]
pub(crate) enum JsonlEncodeError {
    #[error("the session header cannot be represented as a tagged JSON object")]
    HeaderShape,
    #[error("the encoded session header exceeds its durable line limit")]
    HeaderTooLarge,
    #[error("the encoded session event exceeds its durable line limit")]
    EventTooLarge,
    #[error("the session record could not be encoded")]
    Encode(#[from] serde_json::Error),
    #[error("the session event timestamp field could not be located")]
    TimestampTemplate,
}

pub(crate) struct EventLineTemplate {
    bytes: Vec<u8>,
    timestamp_start: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct DurableTimestamp(u64);

impl DurableTimestamp {
    pub(crate) fn new(value: u64) -> Option<Self> {
        (value <= 9_007_199_254_740_991).then_some(Self(value))
    }
}

impl EventLineTemplate {
    pub(crate) fn new(event: &SessionEvent) -> Result<Self, JsonlEncodeError> {
        let bytes = encode_event_line(event)?;
        let marker = b"\"time\":9007199254740991";
        let marker_start = bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .ok_or(JsonlEncodeError::TimestampTemplate)?;
        let timestamp_start = marker_start + b"\"time\":".len();
        Ok(Self {
            bytes,
            timestamp_start,
        })
    }

    pub(crate) fn finish(mut self, timestamp: DurableTimestamp) -> Vec<u8> {
        let mut value = timestamp.0;
        let mut digits = [0_u8; MAX_TIMESTAMP_DIGITS.len()];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = decimal_digit(value % 10);
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let actual = &digits[start..];
        let old_end = self.timestamp_start + MAX_TIMESTAMP_DIGITS.len();
        self.bytes[self.timestamp_start..self.timestamp_start + actual.len()]
            .copy_from_slice(actual);
        if actual.len() < MAX_TIMESTAMP_DIGITS.len() {
            let new_end = self.timestamp_start + actual.len();
            let old_len = self.bytes.len();
            self.bytes.copy_within(old_end..old_len, new_end);
            self.bytes
                .truncate(old_len - (MAX_TIMESTAMP_DIGITS.len() - actual.len()));
        }
        self.bytes
    }

    pub(crate) fn encoded_len(&self) -> usize {
        self.bytes.len()
    }
}

fn decimal_digit(value: u64) -> u8 {
    match value {
        0 => b'0',
        1 => b'1',
        2 => b'2',
        3 => b'3',
        4 => b'4',
        5 => b'5',
        6 => b'6',
        7 => b'7',
        8 => b'8',
        _ => b'9',
    }
}

pub(crate) fn encode_header_line(header: &SessionHeader) -> Result<Vec<u8>, JsonlEncodeError> {
    let mut value = header.raw().as_value().clone();
    let fields = value.as_object_mut().ok_or(JsonlEncodeError::HeaderShape)?;
    if fields.contains_key("type") {
        return Err(JsonlEncodeError::HeaderShape);
    }
    fields.insert(
        "type".to_owned(),
        serde_json::Value::String("session".to_owned()),
    );
    encode_line(value, MAX_JOURNAL_HEADER_LINE_BYTES).map_err(|error| match error {
        LineEncodeError::TooLarge => JsonlEncodeError::HeaderTooLarge,
        LineEncodeError::Encode(error) => JsonlEncodeError::Encode(error),
    })
}

pub(crate) fn encode_event_line(event: &SessionEvent) -> Result<Vec<u8>, JsonlEncodeError> {
    let mut bytes = serde_json::to_vec(event)?;
    if bytes.len() >= MAX_JOURNAL_EVENT_LINE_BYTES {
        return Err(JsonlEncodeError::EventTooLarge);
    }
    bytes
        .try_reserve_exact(1)
        .map_err(|_| JsonlEncodeError::EventTooLarge)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Conservative complete-line charge for a prepared event at the largest
/// representable sequence and timestamp. This borrows the potentially large
/// payload instead of cloning its JSON tree.
pub(super) fn prepared_event_line_upper_bound(
    prepared: &PreparedEvent,
) -> Result<u64, JsonlEncodeError> {
    let mut counter = CountingWriter::default();
    {
        let mut serializer = serde_json::Serializer::new(&mut counter);
        let surface = prepared.event.surface.as_ref();
        let mut map = serializer.serialize_map(Some(
            4 + usize::from(
                surface
                    .and_then(|intent| intent.source_event_seqs.as_ref())
                    .is_some(),
            ) + usize::from(surface.is_some()),
        ))?;
        map.serialize_entry("type", prepared.event.kind.event_type())?;
        map.serialize_entry("seq", &MAX_SAFE_INTEGER)?;
        map.serialize_entry("time", &MAX_SAFE_INTEGER)?;
        map.serialize_entry("data", prepared.original_data.as_value())?;
        if let Some(sources) = surface.and_then(|intent| intent.source_event_seqs.as_ref()) {
            map.serialize_entry("sourceEventSeqs", sources)?;
        }
        if let Some(intent) = surface {
            map.serialize_entry("surfaceOp", &intent.operation)?;
        }
        map.end()?;
    }
    if counter.overflowed || counter.bytes >= MAX_JOURNAL_EVENT_LINE_BYTES {
        return Err(JsonlEncodeError::EventTooLarge);
    }
    let complete = counter
        .bytes
        .checked_add(1)
        .ok_or(JsonlEncodeError::EventTooLarge)?;
    u64::try_from(complete).map_err(|_| JsonlEncodeError::EventTooLarge)
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
    overflowed: bool,
}

impl io::Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.bytes.checked_add(buffer.len()) {
            Some(total) => self.bytes = total,
            None => self.overflowed = true,
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum LineEncodeError {
    TooLarge,
    Encode(serde_json::Error),
}

fn encode_line(value: serde_json::Value, maximum: usize) -> Result<Vec<u8>, LineEncodeError> {
    let mut bytes = serde_json::to_vec(&value).map_err(LineEncodeError::Encode)?;
    if bytes.len() >= maximum {
        return Err(LineEncodeError::TooLarge);
    }
    bytes
        .try_reserve_exact(1)
        .map_err(|_| LineEncodeError::TooLarge)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use crate::{
        session::{
            Clock, ClockError, EventKind, EventSeq, NewEvent, Session, SessionEvent, SurfaceIntent,
            UnixMillis,
        },
        workspace_authority::WorkspaceIdentity,
    };

    use super::{
        DurableTimestamp, EventLineTemplate, JsonlEncodeError, MAX_SAFE_INTEGER, encode_event_line,
        encode_header_line, prepared_event_line_upper_bound,
    };

    #[derive(Clone, Copy)]
    struct FixedClock(UnixMillis);

    impl Clock for FixedClock {
        fn now(&self) -> Result<UnixMillis, ClockError> {
            Ok(self.0)
        }
    }

    #[test]
    fn durable_header_is_tagged_once_and_event_lines_are_one_lf_record() {
        let header = crate::session::SessionHeader::new_durable(
            "session-550e8400-e29b-41d4-a716-446655440000",
            UnixMillis::new(7).unwrap(),
            "/workspace".to_owned(),
            WorkspaceIdentity::new_for_test(0x1a, 0x2b),
        )
        .unwrap();
        let header_line = encode_header_line(&header).unwrap();
        assert_eq!(header_line.last(), Some(&b'\n'));
        assert_eq!(header_line.iter().filter(|byte| **byte == b'\n').count(), 1);
        let value: serde_json::Value = serde_json::from_slice(&header_line).unwrap();
        assert_eq!(value["type"], "session");
        assert_eq!(value["delegationDepth"], 0);
        assert_eq!(value["rustWorkspaceIdentity"]["device"], "1a");
        assert_eq!(value["rustWorkspaceIdentity"]["inode"], "2b");

        let mut session = Session::new("jsonl-event").unwrap();
        let receipt = session
            .append(NewEvent::log(EventKind::turn_start(
                crate::session::TurnId::new(1).unwrap(),
            )))
            .unwrap();
        let event = &session.events()[usize::try_from(receipt.seq().get()).unwrap()];
        let event_line = encode_event_line(event).unwrap();
        assert_eq!(event_line.last(), Some(&b'\n'));
        let value: serde_json::Value = serde_json::from_slice(&event_line).unwrap();
        assert_eq!(value["seq"], EventSeq::new(0).unwrap().get());
        assert_eq!(value["type"], "turn/start");
    }

    #[test]
    fn durable_wrapper_can_reject_a_header_that_still_fits_the_memory_boundary() {
        let mut length = 64 * 1024;
        let error = loop {
            let cwd = format!("/{}", "x".repeat(length));
            match crate::session::SessionHeader::new_durable(
                "session-550e8400-e29b-41d4-a716-446655440000",
                UnixMillis::new(7).unwrap(),
                cwd,
                WorkspaceIdentity::new_for_test(1, 2),
            ) {
                Ok(header) => match encode_header_line(&header) {
                    Err(error @ JsonlEncodeError::HeaderTooLarge) => break error,
                    Ok(_) => length += 1,
                    Err(error) => panic!("unexpected encode error: {error}"),
                },
                Err(_) => length -= 1,
            }
        };
        assert!(matches!(error, JsonlEncodeError::HeaderTooLarge));
    }

    #[test]
    fn timestamp_template_matches_direct_encoding_at_each_boundary() {
        let maximum = UnixMillis::new(9_007_199_254_740_991).unwrap();
        let mut session = Session::with_clock("jsonl-template", FixedClock(maximum)).unwrap();
        session.append(NewEvent::log(EventKind::EndSeed)).unwrap();
        let source = session.events()[0].clone();

        for raw in [0_u64, 7, 1_234_567_890, 9_007_199_254_740_991] {
            let time = UnixMillis::new(i64::try_from(raw).unwrap()).unwrap();
            let mut direct = source.clone();
            direct.set_time_for_commit(time);
            let expected = encode_event_line(&direct).unwrap();
            let actual = EventLineTemplate::new(&source)
                .unwrap()
                .finish(DurableTimestamp::new(raw).unwrap());
            assert_eq!(actual, expected);
            assert_eq!(actual.last(), Some(&b'\n'));
            assert_eq!(actual.iter().filter(|byte| **byte == b'\n').count(), 1);
            let parsed: serde_json::Value = serde_json::from_slice(&actual).unwrap();
            assert_eq!(parsed["time"], raw);
        }
        assert!(DurableTimestamp::new(9_007_199_254_740_992).is_none());
    }

    #[test]
    fn prepared_line_count_matches_the_exact_largest_physical_row() {
        let maximum_seq = EventSeq::new(MAX_SAFE_INTEGER).unwrap();
        let maximum_time = UnixMillis::new(i64::try_from(MAX_SAFE_INTEGER).unwrap()).unwrap();
        let prepared = Session::prepare_event(NewEvent::surface(
            EventKind::EndSeed,
            SurfaceIntent::append().with_sources(vec![
                EventSeq::new(0).unwrap(),
                EventSeq::new(MAX_SAFE_INTEGER - 1).unwrap(),
            ]),
        ))
        .unwrap();
        let expected = prepared_event_line_upper_bound(&prepared).unwrap();
        let event = SessionEvent::from_new(
            maximum_seq,
            maximum_time,
            prepared.event,
            prepared.original_data,
        );
        let actual = encode_event_line(&event).unwrap();

        assert_eq!(u64::try_from(actual.len()).unwrap(), expected);
        let value: serde_json::Value = serde_json::from_slice(&actual).unwrap();
        assert_eq!(value["seq"], MAX_SAFE_INTEGER);
        assert_eq!(value["time"], MAX_SAFE_INTEGER);
        assert_eq!(value["surfaceOp"], "append");
        assert_eq!(value["sourceEventSeqs"][1], MAX_SAFE_INTEGER - 1);
    }
}
