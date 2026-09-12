//! A zero-copy CSV parser.
//!
//! Each record's fields are sliced out of one buffer that [`Parser`] reuses
//! across calls to [`Parser::read_record`], rather than allocating fresh
//! storage per row.
//!
//! # Example
//!
//! ```no_run
//! use csv_parser::Parser;
//!
//! # fn main() -> std::io::Result<()> {
//! let mut parser = Parser::open("data.csv")?;
//! while let Some(record) = parser.read_record()? {
//!     if let Some(name) = record.get("name") {
//!         println!("{}", String::from_utf8_lossy(name));
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::Path,
    rc::Rc,
    str::Utf8Error,
};

#[derive(Clone)]
struct Span {
    offset: usize,
    len: usize,
}

impl Span {
    fn extend(carry: Option<Self>, fresh_offset: usize, extra_len: usize) -> Self {
        match carry {
            Some(Span { offset, len }) => Span {
                offset,
                len: len + extra_len,
            },
            None => Span {
                offset: fresh_offset,
                len: extra_len,
            },
        }
    }
}

/// A single parsed row, with fields sliced out of one shared byte buffer.
///
/// Obtained via [`Parser::read_record`], which reuses the same `Record`
/// across calls — see that method's docs for what that means for borrowing.
#[derive(Clone)]
pub struct Record {
    data: Vec<u8>,
    spans: Vec<Span>,
    mapping: Rc<HashMap<String, usize>>,
}

impl Record {
    fn new(mapping: Rc<HashMap<String, usize>>) -> Self {
        Self {
            data: Vec::new(),
            spans: Vec::new(),
            mapping,
        }
    }

    fn set_mapping(&mut self, mapping: Rc<HashMap<String, usize>>) {
        self.mapping = mapping;
    }

    fn parse(&mut self, bufread: &mut impl BufRead, separator: u8) -> io::Result<()> {
        self.data.clear();
        self.spans.clear();
        let mut carry = None;
        loop {
            let buff = bufread.fill_buf()?;
            let buff_len = buff.len();
            if buff.is_empty() {
                if let Some(span) = carry {
                    self.spans.push(span);
                }
                return Ok(());
            }
            let mut start = 0;
            let end = memchr::memchr2_iter(separator, b'\n', buff).find(|&pos| {
                let span = Span::extend(carry.take(), self.data.len(), pos - start);
                self.data.extend_from_slice(&buff[start..pos]);
                self.spans.push(span);
                start = pos + 1;
                buff[pos] == b'\n'
            });
            match end {
                None => {
                    carry = Some(Span::extend(
                        carry.take(),
                        self.data.len(),
                        buff_len - start,
                    ));
                    self.data.extend_from_slice(&buff[start..]);
                    bufread.consume(buff_len);
                }
                Some(pos) => {
                    bufread.consume(pos + 1);
                    return Ok(());
                }
            }
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
        self.spans
            .get(index)
            .map(|&Span { offset, len }| &self.data[offset..offset + len])
    }

    /// Like [`Record::get_at`], but validated and returned as `&str`.
    ///
    /// Returns `None` if `index` is out of range, or `Some(Err(_))` if the
    /// field's bytes aren't valid UTF-8.
    pub fn get_str_at(&self, index: usize) -> Option<Result<&str, Utf8Error>> {
        self.get_at(index).map(str::from_utf8)
    }

    /// Returns the raw bytes of the field named `name` in the header row,
    /// or `None` if there's no column with that name.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.get_at(*self.mapping.get(name)?)
    }

    /// Like [`Record::get`], but validated and returned as `&str`.
    pub fn get_str(&self, name: &str) -> Option<Result<&str, Utf8Error>> {
        self.get_str_at(*self.mapping.get(name)?)
    }

    /// Iterates over all fields in this record, in column order.
    pub fn iter(&self) -> Fields<'_> {
        Fields {
            data: &self.data,
            spans: self.spans.iter(),
        }
    }
}

/// Iterator over a [`Record`]'s fields, in column order.
///
/// See [`Record::iter`] and `impl IntoIterator for &Record`.
pub struct Fields<'a> {
    data: &'a [u8],
    spans: std::slice::Iter<'a, Span>,
}

impl<'a> Iterator for Fields<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        self.spans
            .next()
            .map(|&Span { offset, len }| &self.data[offset..offset + len])
    }
}

impl<'a> IntoIterator for &'a Record {
    type Item = &'a [u8];
    type IntoIter = Fields<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Column metadata parsed from a CSV's header row: column count and
/// name-to-index lookup. Obtained via [`Parser::header`].
pub struct Header {
    mapping: Rc<HashMap<String, usize>>,
    len: usize,
}

fn utf8_to_io_error(e: Utf8Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

impl Header {
    fn new(record: &Record) -> Result<(Self, Rc<HashMap<String, usize>>), Utf8Error> {
        let len = record.len();
        let mut mapping = HashMap::with_capacity(len);
        for (i, field) in record.iter().enumerate() {
            mapping.insert(str::from_utf8(field)?.to_string(), i);
        }
        let mapping = Rc::new(mapping);
        Ok((
            Self {
                mapping: Rc::clone(&mapping),
                len,
            },
            mapping,
        ))
    }

    /// Returns the column index for `name`, or `None` if no such column
    /// exists.
    ///
    /// If the header row has duplicate column names, the last occurrence
    /// wins.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.mapping.get(name).copied()
    }

    /// Number of columns in the header row.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the header row had no columns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The default field separator (`,`) used by [`Parser::open`],
/// [`Parser::from_reader`], and [`Parser::new`].
pub const DEFAULT_SEPARATOR: u8 = b',';

/// Reads CSV records from an underlying [`BufRead`], one at a time.
///
/// The first row is always parsed as the header immediately on
/// construction (see [`Parser::header`]); subsequent rows are read one at
/// a time via [`Parser::read_record`].
pub struct Parser<R: BufRead> {
    header: Header,
    bufread: R,
    separator: u8,
    record: Record,
}

impl Parser<BufReader<File>> {
    /// Opens `path` and parses it as CSV using [`DEFAULT_SEPARATOR`].
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_separator(path, DEFAULT_SEPARATOR)
    }

    /// Like [`Parser::open`], with a custom field separator byte.
    pub fn open_with_separator(path: impl AsRef<Path>, separator: u8) -> io::Result<Self> {
        let f = File::open(path)?;
        Self::from_reader_with_separator(f, separator)
    }
}

impl<R: Read> Parser<BufReader<R>> {
    /// Wraps `reader` in a [`BufReader`] and parses it as CSV using
    /// [`DEFAULT_SEPARATOR`].
    pub fn from_reader(reader: R) -> io::Result<Self> {
        Self::from_reader_with_separator(reader, DEFAULT_SEPARATOR)
    }

    /// Like [`Parser::from_reader`], with a custom field separator byte.
    pub fn from_reader_with_separator(reader: R, separator: u8) -> io::Result<Self> {
        Self::with_separator(BufReader::new(reader), separator)
    }
}

impl<R: BufRead> Parser<R> {
    /// Parses `bufread` as CSV using [`DEFAULT_SEPARATOR`].
    pub fn new(bufread: R) -> io::Result<Self> {
        Self::with_separator(bufread, DEFAULT_SEPARATOR)
    }

    /// Like [`Parser::new`], with a custom field separator byte.
    ///
    /// Parses and consumes the header row immediately.
    pub fn with_separator(mut bufread: R, separator: u8) -> io::Result<Self> {
        let mut record = Record::new(Rc::new(HashMap::new()));
        record.parse(&mut bufread, separator)?;
        let (header, mapping) = Header::new(&record).map_err(utf8_to_io_error)?;
        record.set_mapping(mapping);
        Ok(Self {
            header,
            bufread,
            separator,
            record,
        })
    }

    /// The parsed header row's column metadata.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Reads the next record, or `None` at end of input.
    ///
    /// The returned `&Record` borrows `self` and is backed by an internal
    /// buffer reused on every call — it's only valid until the next call to
    /// `read_record`, so it can't be held across iterations. This also
    /// means it can't be combined with another call needing `&self` (like
    /// [`Parser::header`]) while the returned reference is still live:
    /// fetch what you need from `header()` before the loop, or
    /// [`Clone`] the record, if you need both at once.
    pub fn read_record(&mut self) -> io::Result<Option<&Record>> {
        self.record.parse(&mut self.bufread, self.separator)?;
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
        let mut parser = Parser::from_reader(Cursor::new(data)).unwrap();

        let record = parser.read_record().unwrap().expect("first record");
        assert_eq!(record.len(), 3);
        assert_eq!(record.get_at(0), Some(&b"1"[..]));
        assert_eq!(record.get_at(1), Some(&b"2"[..]));
        assert_eq!(record.get_at(2), Some(&b"3"[..]));

        let record = parser.read_record().unwrap().expect("second record");
        assert_eq!(record.len(), 3);
        assert_eq!(record.get_at(0), Some(&b"4"[..]));
        assert_eq!(record.get_at(1), Some(&b"5"[..]));
        assert_eq!(record.get_at(2), Some(&b"6"[..]));

        assert!(parser.read_record().unwrap().is_none());
    }

    #[test]
    fn parses_final_record_without_trailing_newline() {
        let data = b"a,b\n1,2\n3,4".to_vec();
        let mut parser = Parser::from_reader(Cursor::new(data)).unwrap();

        let record = parser.read_record().unwrap().expect("first record");
        assert_eq!(record.get_at(0), Some(&b"1"[..]));
        assert_eq!(record.get_at(1), Some(&b"2"[..]));

        let record = parser.read_record().unwrap().expect("second record");
        assert_eq!(record.get_at(0), Some(&b"3"[..]));
        assert_eq!(record.get_at(1), Some(&b"4"[..]));

        assert!(parser.read_record().unwrap().is_none());
    }

    #[test]
    fn iterates_record_fields() {
        let data = b"a,b,c\n1,2,3\n".to_vec();
        let mut parser = Parser::from_reader(Cursor::new(data)).unwrap();
        let record = parser.read_record().unwrap().expect("record");

        let fields: Vec<&[u8]> = record.into_iter().collect();
        assert_eq!(fields, vec![&b"1"[..], &b"2"[..], &b"3"[..]]);

        let mut count = 0;
        for _ in record {
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn looks_up_fields_by_name() {
        let data = b"anzsic06,Area,year,geo_count,ec_count\nA,A100100,2025,81,150\n".to_vec();
        let mut parser = Parser::from_reader(Cursor::new(data)).unwrap();

        assert_eq!(parser.header().index_of("year"), Some(2));
        assert_eq!(parser.header().index_of("missing"), None);

        let record = parser.read_record().unwrap().expect("record");
        assert_eq!(record.get("geo_count"), Some(&b"81"[..]));
        assert_eq!(record.get_str("Area").unwrap().unwrap(), "A100100");
    }

    #[test]
    fn preserves_field_spanning_multiple_buffer_refills() {
        let data = b"h1,h2\naaaaaaaaaaaa,b\n".to_vec();
        let reader = std::io::BufReader::with_capacity(4, Cursor::new(data));
        let mut parser = Parser::new(reader).unwrap();
        let record = parser.read_record().unwrap().expect("record");
        assert_eq!(record.get_at(0), Some(&b"aaaaaaaaaaaa"[..]));
        assert_eq!(record.get_at(1), Some(&b"b"[..]));
    }

    #[test]
    fn parses_with_custom_separator() {
        let data = b"a;b;c\n1;2;3\n".to_vec();
        let mut parser = Parser::from_reader_with_separator(Cursor::new(data), b';').unwrap();
        assert_eq!(parser.header().index_of("b"), Some(1));

        let record = parser.read_record().unwrap().expect("record");
        assert_eq!(record.get_at(0), Some(&b"1"[..]));
        assert_eq!(record.get_at(1), Some(&b"2"[..]));
        assert_eq!(record.get_at(2), Some(&b"3"[..]));
    }

    #[test]
    fn reuses_buffer_across_read_record_calls() {
        let data = b"a,b\n1,22\n333,4\n".to_vec();
        let mut parser = Parser::from_reader(Cursor::new(data)).unwrap();

        let record = parser.read_record().unwrap().expect("first record");
        assert_eq!(record.get_at(0), Some(&b"1"[..]));
        assert_eq!(record.get_at(1), Some(&b"22"[..]));

        let record = parser.read_record().unwrap().expect("second record");
        assert_eq!(record.get_at(0), Some(&b"333"[..]));
        assert_eq!(record.get_at(1), Some(&b"4"[..]));

        assert!(parser.read_record().unwrap().is_none());
    }
}
