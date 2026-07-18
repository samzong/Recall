use std::io::{self, BufRead};

use serde_json::Value;

/// Maximum size, in bytes, of a single JSONL record read from a session
/// file. Session files come from external tools (or `recall import`'s
/// stdin/file argument) and are not trusted: a corrupted or adversarial
/// file with one line and no trailing newline for gigabytes would
/// otherwise make `BufRead::lines()` grow an unbounded in-memory buffer.
/// 32 MiB is far larger than any legitimate transcript line but small
/// enough to fail fast rather than exhaust memory.
pub(crate) const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Reads one line (through and including the trailing `\n`, if any) from
/// `reader` into `buf`, without ever growing `buf` past `max_bytes` +
/// one internal buffer refill. Returns `Ok(0)` at EOF with nothing read,
/// mirroring `BufRead::read_line`'s contract. Bails with an `io::Error`
/// of kind `InvalidData` when the line would exceed `max_bytes`.
fn read_capped_line<R: BufRead + ?Sized>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<usize> {
    let mut read = 0;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let (consume_len, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (available.len(), false),
        };
        buf.extend_from_slice(&available[..consume_len]);
        reader.consume(consume_len);
        read += consume_len;
        // Check the cap on every chunk, not only when no newline was found yet:
        // a reader may hand back an entire over-cap line (newline included) in
        // a single `fill_buf` call, so the newline-found branch must not skip
        // the check.
        if buf.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("JSONL line exceeds {max_bytes}-byte cap"),
            ));
        }
        if found_newline {
            break;
        }
    }
    Ok(read)
}

/// Like `BufRead::lines()`, but caps each record at `max_bytes` to avoid
/// unbounded memory growth on adversarial or corrupted JSONL input. Stops
/// producing further items after the first error (matching `BufRead::lines`'
/// de-facto behavior for our callers, which all propagate the first error).
pub(crate) fn capped_lines<R: BufRead>(
    reader: R,
    max_bytes: usize,
) -> impl Iterator<Item = io::Result<String>> {
    struct CappedLines<R> {
        reader: R,
        max_bytes: usize,
        done: bool,
    }

    impl<R: BufRead> Iterator for CappedLines<R> {
        type Item = io::Result<String>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.done {
                return None;
            }
            let mut buf = Vec::new();
            match read_capped_line(&mut self.reader, &mut buf, self.max_bytes) {
                Ok(0) => {
                    self.done = true;
                    None
                }
                Ok(_) => {
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                        if buf.last() == Some(&b'\r') {
                            buf.pop();
                        }
                    }
                    match String::from_utf8(buf) {
                        Ok(s) => Some(Ok(s)),
                        Err(e) => {
                            self.done = true;
                            Some(Err(io::Error::new(io::ErrorKind::InvalidData, e)))
                        }
                    }
                }
                Err(e) => {
                    self.done = true;
                    Some(Err(e))
                }
            }
        }
    }

    CappedLines { reader, max_bytes, done: false }
}

pub(crate) fn rfc3339_ms(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|dt| dt.timestamp_millis())
}

pub(crate) fn json_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_f64().map(|value| value as i64))
}

pub(crate) fn jsonl_indexed(
    lines: impl IntoIterator<Item = io::Result<String>>,
) -> impl Iterator<Item = io::Result<(usize, Value)>> {
    lines.into_iter().enumerate().filter_map(|(index, line)| match line {
        Ok(line) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                None
            } else {
                serde_json::from_str::<Value>(trimmed).ok().map(|value| Ok((index, value)))
            }
        }
        Err(error) => Some(Err(error)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_should_return_err_when_single_line_exceeds_cap() {
        let oversized = vec![b'a'; 100];
        let mut data = oversized;
        data.push(b'\n');
        data.extend_from_slice(b"short\n");
        let cursor = Cursor::new(data);

        let mut lines = capped_lines(cursor, 10);

        let first = lines.next().expect("expected an item, got None");
        assert!(first.is_err(), "expected Err for oversized line, got {first:?}");
        assert_eq!(first.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_should_stop_iteration_when_prior_line_errored() {
        let mut data = vec![b'a'; 100];
        data.push(b'\n');
        data.extend_from_slice(b"short\n");
        let cursor = Cursor::new(data);

        let mut lines = capped_lines(cursor, 10);

        assert!(lines.next().expect("first item").is_err());
        assert!(lines.next().is_none(), "iterator must not yield past the first error");
    }

    #[test]
    fn test_should_yield_lines_when_all_under_cap() {
        let cursor = Cursor::new(b"hello\nworld\n".to_vec());

        let lines: Vec<String> =
            capped_lines(cursor, MAX_LINE_BYTES).map(|line| line.unwrap()).collect();

        assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn test_should_yield_final_line_when_missing_trailing_newline() {
        let cursor = Cursor::new(b"hello\nworld".to_vec());

        let lines: Vec<String> =
            capped_lines(cursor, MAX_LINE_BYTES).map(|line| line.unwrap()).collect();

        assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn test_should_return_none_when_input_is_empty() {
        let cursor = Cursor::new(Vec::new());

        let mut lines = capped_lines(cursor, MAX_LINE_BYTES);

        assert!(lines.next().is_none());
    }
}
