//! StreamJsonRpc-compatible framing. Lengths count UTF-8 bytes, not characters.
use std::io::{self, Read, Write};
pub const MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;
const MAX_HEADER_LENGTH: usize = 8192;

pub fn write_frame(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    if body.len() > MAX_CONTENT_LENGTH {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)
}

pub fn read_frame(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= MAX_HEADER_LENGTH {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "header too large"));
        }
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    let header =
        std::str::from_utf8(&header).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid header"))?;
    let mut length = None;
    for line in header.split("\r\n").filter(|line| !line.is_empty()) {
        let (name, value) =
            line.split_once(':').ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid header field"))?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate Content-Length"));
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid length"))?,
            );
        }
    }
    let length = length
        .filter(|n| *n <= MAX_CONTENT_LENGTH)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing or oversized length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    struct Partial(Cursor<Vec<u8>>);
    impl Read for Partial {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(2);
            self.0.read(&mut buf[..n])
        }
    }
    #[test]
    fn partial_and_consecutive_unicode_frames() {
        let mut bytes = vec![];
        write_frame(&mut bytes, "{\"text\":\"Türkçe\"}".as_bytes()).unwrap();
        write_frame(&mut bytes, b"{}").unwrap();
        let mut reader = Partial(Cursor::new(bytes));
        assert_eq!(read_frame(&mut reader).unwrap(), "{\"text\":\"Türkçe\"}".as_bytes());
        assert_eq!(read_frame(&mut reader).unwrap(), b"{}");
    }
    #[test]
    fn rejects_bad_headers_and_truncation() {
        for input in [
            "\r\n\r\n",
            "Content-Length: -1\r\n\r\n",
            "Content-Length: 999999999\r\n\r\n",
            "Content-Length: 1\r\nContent-Length: 1\r\n\r\na",
            "Content-Length: 3\r\n\r\nx",
        ] {
            assert!(read_frame(&mut Cursor::new(input.as_bytes())).is_err());
        }
    }
}
