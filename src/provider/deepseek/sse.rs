//! Bounded incremental Server-Sent Events framing.

use thiserror::Error;

/// Maximum raw bytes read from one successful streaming response.
pub const MAX_DEEPSEEK_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum bytes retained for one physical SSE line.
pub const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes in one joined SSE data event.
pub const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

const UTF8_BOM: &[u8; 3] = b"\xef\xbb\xbf";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SseItem {
    Data(String),
    Comment,
}

/// Stateful decoder whose output is independent of network chunk boundaries.
#[derive(Debug, Default)]
pub(super) struct SseDecoder {
    bom_prefix: Vec<u8>,
    bom_decided: bool,
    line: Vec<u8>,
    data: String,
    has_data: bool,
    skip_lf_after_cr: bool,
    total_bytes: usize,
    done: bool,
}

impl SseDecoder {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseItem>, SseDecodeFailure> {
        let mut output = Vec::new();
        for &byte in bytes {
            if self.done {
                break;
            }
            self.total_bytes = self.total_bytes.checked_add(1).ok_or_else(|| {
                SseDecodeFailure::new(
                    std::mem::take(&mut output),
                    SseError::ResponseSize {
                        maximum: MAX_DEEPSEEK_RESPONSE_BYTES,
                    },
                )
            })?;
            if self.total_bytes > MAX_DEEPSEEK_RESPONSE_BYTES {
                return Err(SseDecodeFailure::new(
                    output,
                    SseError::ResponseSize {
                        maximum: MAX_DEEPSEEK_RESPONSE_BYTES,
                    },
                ));
            }
            if self.bom_decided {
                if let Err(error) = self.push_body_byte(byte, &mut output) {
                    return Err(SseDecodeFailure::new(output, error));
                }
                continue;
            }
            self.bom_prefix.push(byte);
            if UTF8_BOM.starts_with(&self.bom_prefix) && self.bom_prefix.len() < UTF8_BOM.len() {
                continue;
            }
            self.bom_decided = true;
            if self.bom_prefix.as_slice() == UTF8_BOM {
                self.bom_prefix.clear();
                continue;
            }
            let prefix = std::mem::take(&mut self.bom_prefix);
            for prefix_byte in prefix {
                if let Err(error) = self.push_body_byte(prefix_byte, &mut output) {
                    return Err(SseDecodeFailure::new(output, error));
                }
            }
        }
        Ok(output)
    }

    pub(super) fn finish(&mut self) -> Result<Vec<SseItem>, SseDecodeFailure> {
        let mut output = Vec::new();
        if !self.bom_decided {
            self.bom_decided = true;
            let prefix = std::mem::take(&mut self.bom_prefix);
            for byte in prefix {
                if let Err(error) = self.push_body_byte(byte, &mut output) {
                    return Err(SseDecodeFailure::new(output, error));
                }
            }
        }
        // The SSE standard dispatches only on a blank line. Deliberately do
        // not process an unterminated physical line or incomplete event here.
        Ok(output)
    }

    fn push_body_byte(&mut self, byte: u8, output: &mut Vec<SseItem>) -> Result<(), SseError> {
        if self.skip_lf_after_cr {
            self.skip_lf_after_cr = false;
            if byte == b'\n' {
                return Ok(());
            }
        }
        match byte {
            b'\r' => {
                self.process_line(output)?;
                self.skip_lf_after_cr = true;
            }
            b'\n' => self.process_line(output)?,
            _ => {
                self.line.push(byte);
                if self.line.len() > MAX_SSE_LINE_BYTES {
                    return Err(SseError::LineLength {
                        maximum: MAX_SSE_LINE_BYTES,
                    });
                }
            }
        }
        Ok(())
    }

    fn process_line(&mut self, output: &mut Vec<SseItem>) -> Result<(), SseError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if self.has_data {
                let data = std::mem::take(&mut self.data);
                self.has_data = false;
                self.done = data == super::response::DONE;
                output.push(SseItem::Data(data));
            }
            return Ok(());
        }
        if line[0] == b':' {
            output.push(SseItem::Comment);
            return Ok(());
        }

        let colon = line.iter().position(|byte| *byte == b':');
        let (field, raw_value) = match colon {
            Some(index) => (&line[..index], &line[index + 1..]),
            None => (line.as_slice(), &[][..]),
        };
        if field != b"data" {
            return Ok(());
        }
        let raw_value = raw_value.strip_prefix(b" ").unwrap_or(raw_value);
        let value = String::from_utf8_lossy(raw_value);
        let separator = usize::from(self.has_data);
        let next_len = self
            .data
            .len()
            .checked_add(separator)
            .and_then(|length| length.checked_add(value.len()))
            .ok_or(SseError::EventData {
                maximum: MAX_SSE_EVENT_BYTES,
            })?;
        if next_len > MAX_SSE_EVENT_BYTES {
            return Err(SseError::EventData {
                maximum: MAX_SSE_EVENT_BYTES,
            });
        }
        if self.has_data {
            self.data.push('\n');
        }
        self.data.push_str(&value);
        self.has_data = true;
        Ok(())
    }
}

/// A framing failure plus every complete item that preceded it in the same read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SseDecodeFailure {
    pub(super) items: Vec<SseItem>,
    pub(super) error: SseError,
}

impl SseDecodeFailure {
    fn new(items: Vec<SseItem>, error: SseError) -> Self {
        Self { items, error }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum SseError {
    #[error("SSE response exceeds the maximum of {maximum} bytes")]
    ResponseSize { maximum: usize },
    #[error("SSE line exceeds the maximum of {maximum} bytes")]
    LineLength { maximum: usize },
    #[error("SSE event exceeds the maximum of {maximum} bytes")]
    EventData { maximum: usize },
}
