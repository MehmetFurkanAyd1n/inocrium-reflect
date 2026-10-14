use std::io::{self, Read, Write};

pub(crate) const MAX_SEGMENT: usize = 262_144;
pub(crate) const MAX_TRANSFER: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct Pdu {
    pub header: [u8; 48],
    pub data: Vec<u8>,
}

impl Pdu {
    pub fn new(opcode: u8) -> Self {
        Self {
            header: {
                let mut h = [0; 48];
                h[0] = opcode;
                h
            },
            data: Vec::new(),
        }
    }
    pub fn opcode(&self) -> u8 {
        self.header[0] & 0x3f
    }
    pub fn get32(&self, offset: usize) -> u32 {
        u32::from_be_bytes(self.header[offset..offset + 4].try_into().unwrap())
    }
    pub fn put32(&mut self, offset: usize, value: u32) {
        self.header[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    pub fn read(reader: &mut impl Read) -> io::Result<Self> {
        let mut header = [0; 48];
        reader.read_exact(&mut header)?;
        let len = u32::from_be_bytes([0, header[5], header[6], header[7]]) as usize;
        if len > MAX_SEGMENT || header[4] != 0 {
            return Err(invalid("oversized segment or unsupported additional header"));
        }
        let mut data = vec![0; len];
        reader.read_exact(&mut data)?;
        let mut padding = [0; 3];
        reader.read_exact(&mut padding[..(4 - len % 4) % 4])?;
        Ok(Self { header, data })
    }
    pub fn write(&self, writer: &mut impl Write) -> io::Result<()> {
        if self.data.len() > 0xff_ffff {
            return Err(invalid("segment too large"));
        }
        let mut header = self.header;
        header[5..8].copy_from_slice(&(self.data.len() as u32).to_be_bytes()[1..]);
        writer.write_all(&header)?;
        writer.write_all(&self.data)?;
        writer.write_all(&[0; 3][..(4 - self.data.len() % 4) % 4])
    }
}

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn text(data: &[u8]) -> io::Result<Vec<(String, String)>> {
    if !data.is_empty() && data.last() != Some(&0) {
        return Err(invalid("unterminated text key"));
    }
    data.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let entry = std::str::from_utf8(entry).map_err(|_| invalid("non-UTF8 text"))?;
            let (key, value) = entry.split_once('=').ok_or_else(|| invalid("missing text separator"))?;
            if key.is_empty() {
                return Err(invalid("empty text key"));
            }
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

pub(crate) fn encode_text(keys: &[(String, String)]) -> Vec<u8> {
    keys.iter().flat_map(|(k, v)| format!("{k}={v}\0").into_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_and_keys() {
        for len in 0..9 {
            let mut p = Pdu::new(0x43);
            p.data = vec![42; len];
            p.put32(16, 99);
            let mut bytes = Vec::new();
            p.write(&mut bytes).unwrap();
            let r = Pdu::read(&mut &bytes[..]).unwrap();
            assert_eq!(r.data, p.data);
            assert_eq!(r.get32(16), 99);
            assert_eq!(bytes.len() % 4, 0);
        }
        assert_eq!(text(b"A=B=C\0X=Y\0").unwrap()[0], ("A".into(), "B=C".into()));
        for bad in [b"A=B".as_slice(), b"=x\0", b"x\0", b"a=\xff\0"] {
            assert!(text(bad).is_err());
        }
        let mut h = [0; 48];
        h[4] = 1;
        assert!(Pdu::read(&mut &h[..]).is_err());
    }
}
