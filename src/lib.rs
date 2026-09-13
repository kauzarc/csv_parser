//! A single `RingBuffer` (a reused `VecDeque<u8>`), owned by `Record`, is
//! read into directly from the underlying `Read`. Fields are sliced out of
//! it directly wherever they're physically contiguous; a field that
//! straddles the ring's physical wrap point is the one case that can't be
//! returned as a single slice, so those are reassembled once per record,
//! lazily, into `Record::split_buffer`.

use std::{
    collections::VecDeque,
    io::{self, Read},
    ops::Range,
};

pub mod legacy;

const DEFAULT_CHUNK_SIZE: usize = 1024 * 8;

pub struct Parser<R> {
    reader: R,
    record: Record,
}

pub struct Record {
    buffer: RingBuffer,
    split_buffer: Vec<u8>,
    spans: Vec<Span>,
    /// Bytes to drop from the front of `buffer` at the start of the *next*
    /// `parse()` call, so the `&Record` returned by `read_record` stays
    /// valid until the next call to it.
    pending_advance: usize,
}

struct RingBuffer {
    data: VecDeque<u8>,
    /// Invariant: `written <= data.len()`. `[0, written)` is real data;
    /// `[written, data.len())` is reserved space, already pushed so it can
    /// be read into directly, but not yet meaningful.
    written: usize,
    chunk_size: usize,
}

enum Span {
    /// A logical offset/len into `RingBuffer`.
    Contiguous { offset: usize, len: usize },
    /// An offset/len into `Record::split_buffer`.
    Split { offset: usize, len: usize },
}

impl RingBuffer {
    fn new(chunk_size: usize) -> Self {
        Self {
            data: VecDeque::new(),
            written: 0,
            chunk_size,
        }
    }

    /// Clamps `[from, to)` into a `(s0, s1)` physical decomposition (as
    /// returned by `VecDeque::as_slices`): each end lands in whichever
    /// physical slice it falls in, and a range collapses to empty once it's
    /// entirely outside that slice's bounds.
    #[inline]
    fn split_ranges(len0: usize, from: usize, to: usize) -> (Range<usize>, Range<usize>) {
        (
            from.min(len0)..to.min(len0),
            from.saturating_sub(len0)..to.saturating_sub(len0),
        )
    }

    #[inline]
    fn physical_range<'a>(
        s0: &'a [u8],
        s1: &'a [u8],
        from: usize,
        to: usize,
    ) -> (&'a [u8], &'a [u8]) {
        let (r0, r1) = Self::split_ranges(s0.len(), from, to);
        (&s0[r0], &s1[r1])
    }

    /// The `s0`/`s1` boundary: bytes before this logical offset live in the
    /// deque's first physical slice, at/after it in the second.
    #[inline]
    fn wrap_point(&self) -> usize {
        self.data.as_slices().0.len()
    }

    /// The not-yet-scanned bytes at or after logical offset `from`.
    #[inline]
    fn readable_from(&self, from: usize) -> (&[u8], &[u8]) {
        let (s0, s1) = self.data.as_slices();
        Self::physical_range(s0, s1, from, self.written)
    }

    /// A finalized field's bytes, split across the physical wrap if needed.
    #[inline]
    fn field_at(&self, offset: usize, len: usize) -> (&[u8], &[u8]) {
        let (s0, s1) = self.data.as_slices();
        Self::physical_range(s0, s1, offset, offset + len)
    }

    /// The next physically-contiguous chunk of reserved, writable space.
    #[inline]
    fn spare_capacity_mut(&mut self) -> &mut [u8] {
        let written = self.written;
        let (s0, s1) = self.data.as_mut_slices();
        let (r0, r1) = Self::split_ranges(s0.len(), written, s0.len() + s1.len());
        if r0.is_empty() {
            &mut s1[r1]
        } else {
            &mut s0[r0]
        }
    }

    /// Reads more data into the buffer, growing reserved space if needed.
    /// Returns the number of bytes read (0 = EOF).
    fn fill(&mut self, reader: &mut impl Read) -> io::Result<usize> {
        if self.data.len() - self.written < self.chunk_size {
            // TODO(read_buf): once `Read::read_buf`/`BorrowedBuf` stabilizes,
            // grow via reserved-but-uninitialized capacity instead of
            // zero-filling here.
            let new_len = self.data.len() + self.chunk_size;
            self.data.resize(new_len, 0);
        }
        loop {
            match reader.read(self.spare_capacity_mut()) {
                Ok(n) => {
                    self.written += n;
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Reclaims space used by a fully-consumed record.
    fn advance_front(&mut self, n: usize) {
        debug_assert!(n <= self.written);
        self.data.drain(..n);
        self.written -= n;
    }
}

enum ScanOutcome {
    NeedMoreData(usize),
    RecordComplete(usize),
}

/// Scans a physically-contiguous slice `hay` (logically starting at `base`)
/// for `,`/`\n`, pushing a `Contiguous` span for each field it finds and
/// advancing `field_start` past each one. Takes `spans`/`field_start`
/// directly rather than `&mut Record`, since `hay` borrows `Record::buffer`
/// and must stay alive alongside a mutable borrow of `Record::spans`.
fn scan_segment(
    spans: &mut Vec<Span>,
    field_start: &mut usize,
    base: usize,
    hay: &[u8],
) -> ScanOutcome {
    let mut local = 0;
    while let Some(i) = memchr::memchr2(b',', b'\n', &hay[local..]) {
        let pos = base + local + i;
        let is_newline = hay[local + i] == b'\n';
        spans.push(Span::Contiguous {
            offset: *field_start,
            len: pos - *field_start,
        });
        *field_start = pos + 1;
        local += i + 1;
        if is_newline {
            return ScanOutcome::RecordComplete(pos + 1);
        }
    }
    ScanOutcome::NeedMoreData(base + hay.len())
}

impl Record {
    fn parse(&mut self, reader: &mut impl Read) -> io::Result<()> {
        self.buffer.advance_front(self.pending_advance);
        self.pending_advance = 0;
        self.spans.clear();

        let mut field_start = 0usize;
        let mut scan_pos = 0usize;
        // Bytes already resident from a prior over-read count immediately;
        // mirrors legacy.rs's carry.is_some() true-EOF-vs-empty distinction.
        let mut any_data = self.buffer.written > 0;

        'scan: loop {
            let (hay0, hay1) = self.buffer.readable_from(scan_pos);
            for hay in [hay0, hay1] {
                match scan_segment(&mut self.spans, &mut field_start, scan_pos, hay) {
                    ScanOutcome::RecordComplete(pos) => {
                        scan_pos = pos;
                        break 'scan;
                    }
                    ScanOutcome::NeedMoreData(pos) => scan_pos = pos,
                }
            }

            if self.buffer.fill(reader)? == 0 {
                if any_data {
                    self.spans.push(Span::Contiguous {
                        offset: field_start,
                        len: scan_pos - field_start,
                    });
                }
                break 'scan;
            }
            any_data = true;
        }

        self.pending_advance = scan_pos;
        self.finalize_spans();
        Ok(())
    }

    /// Rewrites any `Contiguous` span that turns out to straddle the
    /// buffer's physical wrap point into a `Split` span backed by
    /// `split_buffer`. Must run exactly once, after the buffer is done being
    /// mutated for this record: the wrap point can only move (grow) while
    /// scanning, which would invalidate an earlier field's classification if
    /// it were decided incrementally instead of all at once, here.
    fn finalize_spans(&mut self) {
        let wrap_point = self.buffer.wrap_point();
        self.split_buffer.clear();
        for span in &mut self.spans {
            let Span::Contiguous { offset, len } = *span else {
                continue;
            };
            let end = offset + len;
            if offset >= wrap_point || end <= wrap_point {
                continue;
            }
            let (a, b) = self.buffer.field_at(offset, len);
            let split_offset = self.split_buffer.len();
            self.split_buffer.extend_from_slice(a);
            self.split_buffer.extend_from_slice(b);
            *span = Span::Split {
                offset: split_offset,
                len,
            };
        }
    }

    /// Number of fields in this record.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Returns `true` if this record has no fields (an empty line).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the raw bytes of the field at `index`, or `None` if `index`
    /// is out of range.
    pub fn get_at(&self, index: usize) -> Option<&[u8]> {
        match *self.spans.get(index)? {
            Span::Contiguous { offset, len } => {
                let (a, b) = self.buffer.field_at(offset, len);
                Some(if b.is_empty() { a } else { b })
            }
            Span::Split { offset, len } => Some(&self.split_buffer[offset..offset + len]),
        }
    }

    #[cfg(test)]
    fn span_kinds(&self) -> (bool, bool) {
        let has_contiguous = self
            .spans
            .iter()
            .any(|s| matches!(s, Span::Contiguous { .. }));
        let has_split = self.spans.iter().any(|s| matches!(s, Span::Split { .. }));
        (has_contiguous, has_split)
    }
}

impl<R: Read> Parser<R> {
    pub fn new(reader: R) -> Self {
        Self::with_chunk_size(reader, DEFAULT_CHUNK_SIZE)
    }

    fn with_chunk_size(reader: R, chunk_size: usize) -> Self {
        Self {
            reader,
            record: Record {
                buffer: RingBuffer::new(chunk_size),
                split_buffer: Vec::new(),
                spans: Vec::new(),
                pending_advance: 0,
            },
        }
    }

    /// Reads the next record, or `None` at end of input.
    ///
    /// The returned `&Record` borrows `self` and is backed by an internal
    /// buffer reused on every call — it's only valid until the next call to
    /// `read_record`.
    pub fn read_record(&mut self) -> io::Result<Option<&Record>> {
        self.record.parse(&mut self.reader)?;
        Ok((!self.record.is_empty()).then_some(&self.record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_multiple_records_and_terminates() {
        let data = b"a,b,c\n1,2,3\n4,5,6\n".to_vec();
        let mut parser = Parser::new(Cursor::new(data));

        let record = parser.read_record().unwrap().expect("first record");
        assert_eq!(record.len(), 3);
        assert_eq!(record.get_at(0), Some(&b"a"[..]));
        assert_eq!(record.get_at(1), Some(&b"b"[..]));
        assert_eq!(record.get_at(2), Some(&b"c"[..]));

        let record = parser.read_record().unwrap().expect("second record");
        assert_eq!(record.get_at(0), Some(&b"1"[..]));
        assert_eq!(record.get_at(1), Some(&b"2"[..]));
        assert_eq!(record.get_at(2), Some(&b"3"[..]));

        let record = parser.read_record().unwrap().expect("third record");
        assert_eq!(record.get_at(0), Some(&b"4"[..]));
        assert_eq!(record.get_at(1), Some(&b"5"[..]));
        assert_eq!(record.get_at(2), Some(&b"6"[..]));

        assert!(parser.read_record().unwrap().is_none());
    }

    #[test]
    fn parses_final_record_without_trailing_newline() {
        let data = b"a,b\n1,2".to_vec();
        let mut parser = Parser::new(Cursor::new(data));

        let record = parser.read_record().unwrap().expect("first record");
        assert_eq!(record.get_at(0), Some(&b"a"[..]));
        assert_eq!(record.get_at(1), Some(&b"b"[..]));

        let record = parser.read_record().unwrap().expect("second record");
        assert_eq!(record.get_at(0), Some(&b"1"[..]));
        assert_eq!(record.get_at(1), Some(&b"2"[..]));

        assert!(parser.read_record().unwrap().is_none());
    }

    #[test]
    fn reuses_buffer_across_read_record_calls() {
        let data = b"a,b\n1,22\n333,4\n".to_vec();
        let mut parser = Parser::new(Cursor::new(data));

        let record = parser.read_record().unwrap().expect("first record");
        assert_eq!(record.get_at(0), Some(&b"a"[..]));
        assert_eq!(record.get_at(1), Some(&b"b"[..]));

        let record = parser.read_record().unwrap().expect("second record");
        assert_eq!(record.get_at(0), Some(&b"1"[..]));
        assert_eq!(record.get_at(1), Some(&b"22"[..]));

        let record = parser.read_record().unwrap().expect("third record");
        assert_eq!(record.get_at(0), Some(&b"333"[..]));
        assert_eq!(record.get_at(1), Some(&b"4"[..]));

        assert!(parser.read_record().unwrap().is_none());
    }

    #[test]
    fn preserves_field_spanning_multiple_buffer_refills() {
        let data = b"aaaaaaaaaaaa,b\n".to_vec();
        let mut parser = Parser::with_chunk_size(Cursor::new(data), 4);

        let record = parser.read_record().unwrap().expect("record");
        assert_eq!(record.get_at(0), Some(&b"aaaaaaaaaaaa"[..]));
        assert_eq!(record.get_at(1), Some(&b"b"[..]));

        assert!(parser.read_record().unwrap().is_none());
    }

    #[test]
    fn physical_range_matches_logical_order_across_a_wrap() {
        let mut buffer = RingBuffer::new(4);
        buffer.data.reserve_exact(8);
        let cap = buffer.data.capacity();
        for _ in 0..cap {
            buffer.data.push_back(0);
        }
        buffer.data.drain(..cap / 2);
        for &b in b"the-wrapped-payload" {
            buffer.data.push_back(b);
        }
        buffer.written = buffer.data.len();

        let (s0, _) = buffer.data.as_slices();
        assert!(
            s0.len() < buffer.data.len(),
            "test setup didn't produce a wrapped layout"
        );

        let logical: Vec<u8> = buffer.data.iter().copied().collect();
        for from in 0..=logical.len() {
            for to in from..=logical.len() {
                let (s0, s1) = buffer.data.as_slices();
                let (a, b) = RingBuffer::physical_range(s0, s1, from, to);
                let mut got = a.to_vec();
                got.extend_from_slice(b);
                assert_eq!(got, logical[from..to], "range [{from}, {to})");
            }
        }
    }

    #[test]
    fn splits_fields_across_the_ring_wrap_under_sustained_load() {
        let mut data = Vec::new();
        let mut expected: Vec<Vec<Vec<u8>>> = Vec::new();
        for row in 0..40 {
            let mut fields = Vec::new();
            for col in 0..3 {
                let field = format!("r{row}c{col}-{}", "x".repeat(col + row % 5));
                data.extend_from_slice(field.as_bytes());
                data.push(if col == 2 { b'\n' } else { b',' });
                fields.push(field.into_bytes());
            }
            expected.push(fields);
        }

        let mut parser = Parser::with_chunk_size(Cursor::new(data), 8);
        let mut saw_split = false;
        let mut saw_mixed = false;
        for row in expected {
            let record = parser.read_record().unwrap().expect("record");
            assert_eq!(record.len(), row.len());
            for (i, field) in row.iter().enumerate() {
                assert_eq!(record.get_at(i), Some(field.as_slice()));
            }
            let (has_contiguous, has_split) = record.span_kinds();
            saw_split |= has_split;
            saw_mixed |= has_contiguous && has_split;
        }
        assert!(parser.read_record().unwrap().is_none());
        assert!(saw_split, "test setup never exercised the Split-field path");
        assert!(
            saw_mixed,
            "test setup never exercised a record mixing Contiguous and Split fields"
        );
    }
}
