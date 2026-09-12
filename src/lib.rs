use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::Path,
    str::Utf8Error,
};

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

pub struct Record {
    data: Vec<u8>,
    spans: Vec<Span>,
}

impl Record {
    fn parse(bufread: &mut impl BufRead, separator: u8) -> io::Result<Self> {
        let mut res = Self {
            data: Vec::new(),
            spans: Vec::new(),
        };
        let mut carry = None;
        loop {
            let buff = bufread.fill_buf()?;
            let buff_len = buff.len();
            if buff.is_empty() {
                if let Some(span) = carry {
                    res.spans.push(span);
                }
                return Ok(res);
            }
            let mut start = 0;
            let end = memchr::memchr2_iter(separator, b'\n', buff).find(|&pos| {
                let span = Span::extend(carry.take(), res.data.len(), pos - start);
                res.data.extend_from_slice(&buff[start..pos]);
                res.spans.push(span);
                start = pos + 1;
                buff[pos] == b'\n'
            });
            match end {
                None => {
                    carry = Some(Span::extend(carry.take(), res.data.len(), buff_len - start));
                    res.data.extend_from_slice(&buff[start..]);
                    bufread.consume(buff_len);
                }
                Some(pos) => {
                    bufread.consume(pos + 1);
                    return Ok(res);
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&[u8]> {
        self.spans
            .get(index)
            .map(|&Span { offset, len }| &self.data[offset..offset + len])
    }

    pub fn get_str(&self, index: usize) -> Option<Result<&str, Utf8Error>> {
        self.get(index).map(|v| str::from_utf8(v))
    }

    pub fn iter(&self) -> Fields<'_> {
        Fields {
            data: &self.data,
            spans: self.spans.iter(),
        }
    }
}

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

pub struct Header {
    record: Record,
    mapping: HashMap<String, usize>,
}

fn utf8_to_io_error(e: Utf8Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

impl Header {
    fn new(record: Record) -> Result<Self, Utf8Error> {
        let mut mapping = HashMap::with_capacity(record.len());
        for (i, field) in record.iter().enumerate() {
            mapping.insert(str::from_utf8(field)?.to_string(), i);
        }
        Ok(Self { record, mapping })
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.mapping.get(name).copied()
    }

    pub fn get<'a>(&self, record: &'a Record, name: &str) -> Option<&'a [u8]> {
        record.get(self.index_of(name)?)
    }

    pub fn get_str<'a>(
        &self,
        record: &'a Record,
        name: &str,
    ) -> Option<Result<&'a str, Utf8Error>> {
        record.get_str(self.index_of(name)?)
    }

    pub fn len(&self) -> usize {
        self.record.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.record.is_empty()
    }
}

pub const DEFAULT_SEPARATOR: u8 = b',';

pub struct Parser<R: BufRead> {
    header: Header,
    bufread: R,
    separator: u8,
}

impl Parser<BufReader<File>> {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_separator(path, DEFAULT_SEPARATOR)
    }

    pub fn open_with_separator(path: impl AsRef<Path>, separator: u8) -> io::Result<Self> {
        let f = File::open(path)?;
        Self::from_reader_with_separator(f, separator)
    }
}

impl<R: Read> Parser<BufReader<R>> {
    pub fn from_reader(reader: R) -> io::Result<Self> {
        Self::from_reader_with_separator(reader, DEFAULT_SEPARATOR)
    }

    pub fn from_reader_with_separator(reader: R, separator: u8) -> io::Result<Self> {
        Self::with_separator(BufReader::new(reader), separator)
    }
}

impl<R: BufRead> Parser<R> {
    pub fn new(bufread: R) -> io::Result<Self> {
        Self::with_separator(bufread, DEFAULT_SEPARATOR)
    }

    pub fn with_separator(mut bufread: R, separator: u8) -> io::Result<Self> {
        let record = Record::parse(&mut bufread, separator)?;
        let header = Header::new(record).map_err(utf8_to_io_error)?;
        Ok(Self {
            header,
            bufread,
            separator,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }
}

impl<R: BufRead> Iterator for Parser<R> {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        Record::parse(&mut self.bufread, self.separator)
            .map(|record| (!record.is_empty()).then_some(record))
            .transpose()
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

        let record = parser.next().expect("first record").unwrap();
        assert_eq!(record.len(), 3);
        assert_eq!(record.get(0), Some(&b"1"[..]));
        assert_eq!(record.get(1), Some(&b"2"[..]));
        assert_eq!(record.get(2), Some(&b"3"[..]));

        let record = parser.next().expect("second record").unwrap();
        assert_eq!(record.len(), 3);
        assert_eq!(record.get(0), Some(&b"4"[..]));
        assert_eq!(record.get(1), Some(&b"5"[..]));
        assert_eq!(record.get(2), Some(&b"6"[..]));

        assert!(parser.next().is_none());
    }

    #[test]
    fn parses_final_record_without_trailing_newline() {
        let data = b"a,b\n1,2\n3,4".to_vec();
        let mut parser = Parser::from_reader(Cursor::new(data)).unwrap();

        let record = parser.next().expect("first record").unwrap();
        assert_eq!(record.get(0), Some(&b"1"[..]));
        assert_eq!(record.get(1), Some(&b"2"[..]));

        let record = parser.next().expect("second record").unwrap();
        assert_eq!(record.get(0), Some(&b"3"[..]));
        assert_eq!(record.get(1), Some(&b"4"[..]));

        assert!(parser.next().is_none());
    }

    #[test]
    fn iterates_record_fields() {
        let data = b"a,b,c\n1,2,3\n".to_vec();
        let mut parser = Parser::from_reader(Cursor::new(data)).unwrap();
        let record = parser.next().expect("record").unwrap();

        let fields: Vec<&[u8]> = (&record).into_iter().collect();
        assert_eq!(fields, vec![&b"1"[..], &b"2"[..], &b"3"[..]]);

        let mut count = 0;
        for _ in &record {
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn looks_up_fields_by_header_name() {
        let data = b"anzsic06,Area,year,geo_count,ec_count\nA,A100100,2025,81,150\n".to_vec();
        let mut parser = Parser::from_reader(Cursor::new(data)).unwrap();

        assert_eq!(parser.header().index_of("year"), Some(2));
        assert_eq!(parser.header().index_of("missing"), None);

        let record = parser.next().expect("record").unwrap();
        assert_eq!(parser.header().get(&record, "geo_count"), Some(&b"81"[..]));
        assert_eq!(
            parser.header().get_str(&record, "Area").unwrap().unwrap(),
            "A100100"
        );
    }

    #[test]
    fn preserves_field_spanning_multiple_buffer_refills() {
        let data = b"h1,h2\naaaaaaaaaaaa,b\n".to_vec();
        let reader = std::io::BufReader::with_capacity(4, Cursor::new(data));
        let mut parser = Parser::new(reader).unwrap();
        let record = parser.next().expect("record").unwrap();
        assert_eq!(record.get(0), Some(&b"aaaaaaaaaaaa"[..]));
        assert_eq!(record.get(1), Some(&b"b"[..]));
    }

    #[test]
    fn parses_with_custom_separator() {
        let data = b"a;b;c\n1;2;3\n".to_vec();
        let mut parser = Parser::from_reader_with_separator(Cursor::new(data), b';').unwrap();
        assert_eq!(parser.header().index_of("b"), Some(1));

        let record = parser.next().expect("record").unwrap();
        assert_eq!(record.get(0), Some(&b"1"[..]));
        assert_eq!(record.get(1), Some(&b"2"[..]));
        assert_eq!(record.get(2), Some(&b"3"[..]));
    }
}
