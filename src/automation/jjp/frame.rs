//! NDJSON framing with a cap enforced before allocation (section 9.2.1).
//!
//! Lines that end inside the chunk just read are handed out as ranges of
//! it, without a copy; only the unfinished tail of a chunk is kept, and it
//! never grows past the cap: a longer line is refused as soon as its first
//! byte past the cap arrives, before that byte is buffered. UTF-8 is checked
//! on whole lines by the caller, so a character split between chunks is
//! simply joined.
use std::ops::Range;

/// A complete line, without its newline.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Piece {
    /// A range of the chunk passed to [`Framer::split`].
    InChunk(Range<usize>),
    /// A line that began in an earlier chunk.
    Joined(Vec<u8>),
}

/// A line longer than the cap.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TooLong;

pub(super) struct Framer {
    partial: Vec<u8>,
    max: usize,
}

impl Framer {
    /// Lines of at most `max` bytes, newline excluded.
    pub(super) fn new(max: usize) -> Self {
        Self {
            partial: Vec::new(),
            max,
        }
    }

    /// Appends the complete lines of `chunk` to `out`, in order, and keeps
    /// its unfinished tail. On [`TooLong`], `out` still holds the lines
    /// before the long one; the connection must then close.
    pub(super) fn split(&mut self, chunk: &[u8], out: &mut Vec<Piece>) -> Result<(), TooLong> {
        let mut start = 0;
        while let Some(offset) = chunk
            .get(start..)
            .and_then(|rest| rest.iter().position(|byte| *byte == b'\n'))
        {
            let end = start + offset;
            let line = chunk.get(start..end).unwrap_or_default();
            if self.partial.is_empty() {
                if line.len() > self.max {
                    return Err(TooLong);
                }
                out.push(Piece::InChunk(start..end));
            } else {
                self.append(line)?;
                out.push(Piece::Joined(std::mem::take(&mut self.partial)));
            }
            start = end + 1;
        }
        let tail = chunk.get(start..).unwrap_or_default();
        if !tail.is_empty() {
            self.append(tail)?;
        }
        Ok(())
    }

    /// Adds to the unfinished line, refusing before it would pass the cap;
    /// capacity grows geometrically but never beyond the cap.
    fn append(&mut self, bytes: &[u8]) -> Result<(), TooLong> {
        let needed = self.partial.len() + bytes.len();
        if needed > self.max {
            return Err(TooLong);
        }
        if needed > self.partial.capacity() {
            let target = needed.max(self.partial.capacity() * 2).min(self.max);
            self.partial.reserve_exact(target - self.partial.len());
        }
        self.partial.extend_from_slice(bytes);
        Ok(())
    }

    /// Bytes of an unfinished line held.
    #[cfg(test)]
    pub(super) fn buffered(&self) -> usize {
        self.partial.len()
    }

    /// What the unfinished line's buffer allocated.
    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.partial.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(framer: &mut Framer, chunks: &[&[u8]]) -> Result<Vec<Vec<u8>>, TooLong> {
        let mut found = Vec::new();
        for chunk in chunks {
            let mut out = Vec::new();
            let result = framer.split(chunk, &mut out);
            for piece in out {
                found.push(match piece {
                    Piece::InChunk(range) => chunk[range].to_vec(),
                    Piece::Joined(line) => line,
                });
            }
            result?;
        }
        Ok(found)
    }

    #[test]
    fn lines_survive_any_split_and_joined_messages() {
        let text = "{\"a\":\"ação\"}\n{\"b\":\"日本\"}\n\n{\"c\":1}\n".as_bytes();
        let expected: Vec<Vec<u8>> = text
            .split(|byte| *byte == b'\n')
            .take(4)
            .map(<[u8]>::to_vec)
            .collect();
        // Every split point, including inside a multi-byte character.
        for first in 0..=text.len() {
            for second in first..=text.len() {
                let mut framer = Framer::new(64);
                let found = lines(
                    &mut framer,
                    &[&text[..first], &text[first..second], &text[second..]],
                )
                .unwrap();
                assert_eq!(found, expected, "split at {first} and {second}");
                assert_eq!(framer.buffered(), 0);
            }
        }
    }

    #[test]
    fn a_line_at_the_cap_passes_and_one_byte_more_is_refused_before_buffering() {
        let mut framer = Framer::new(1024);
        let exact = [vec![b'x'; 1024], vec![b'\n']].concat();
        assert_eq!(
            lines(&mut framer, &[&exact]).unwrap(),
            vec![vec![b'x'; 1024]]
        );

        // The cap plus one byte, without a newline, in pieces: refused when
        // the byte past the cap arrives, with no more than the cap held.
        let mut framer = Framer::new(1024);
        let mut out = Vec::new();
        for _ in 0..4 {
            framer.split(&[b'y'; 256], &mut out).unwrap();
        }
        assert_eq!(framer.buffered(), 1024);
        assert!(framer.capacity() <= 1024);
        assert_eq!(framer.split(b"y", &mut out), Err(TooLong));
        assert!(framer.capacity() <= 1024);
        assert!(out.is_empty());

        // A long line inside one chunk is refused without being copied, and
        // the lines before it are still handed out.
        let mut framer = Framer::new(8);
        let mut out = Vec::new();
        assert_eq!(framer.split(b"ok\n123456789\n", &mut out), Err(TooLong));
        assert_eq!(out, vec![Piece::InChunk(0..2)]);
        assert_eq!(framer.capacity(), 0);
    }
}
