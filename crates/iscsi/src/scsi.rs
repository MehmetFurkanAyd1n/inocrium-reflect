use crate::{pdu::MAX_TRANSFER, LunConfig};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sense(pub u8, pub u8, pub u8);
impl Sense {
    pub fn fixed(self) -> Vec<u8> {
        vec![0x70, 0, self.0, 0, 0, 0, 0, 10, 0, 0, 0, 0, self.1, self.2, 0, 0, 0, 0]
    }
}
pub(crate) const INVALID_FIELD: Sense = Sense(5, 0x24, 0);
pub(crate) const NO_LUN: Sense = Sense(5, 0x25, 0);
pub(crate) const PROTECTED: Sense = Sense(7, 0x27, 0);

pub(crate) enum Action {
    Data(Vec<u8>),
    Read { offset: u64, len: usize },
    Write { offset: u64, len: usize, fua: bool },
    Flush,
    IgnoreWrite(usize),
    Good,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes(b[..2].try_into().unwrap())
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}
fn data(mut bytes: Vec<u8>, allocation: usize) -> Action {
    bytes.truncate(allocation);
    Action::Data(bytes)
}

pub(crate) fn plan(lun: &LunConfig, c: &[u8; 16], count: usize, sense: Sense) -> Result<Action, Sense> {
    let dev = &lun.device;
    let block = dev.block_size();
    let blocks = dev.size() / u64::from(block);
    match c[0] {
        0x00 | 0x1b | 0x1e => Ok(Action::Good),
        0x03 => Ok(data(sense.fixed(), c[4] as usize)),
        0x12 => inquiry(lun, c),
        0x25 => {
            let mut bytes = (blocks.saturating_sub(1).min(u32::MAX as u64) as u32).to_be_bytes().to_vec();
            bytes.extend(block.to_be_bytes());
            Ok(Action::Data(bytes))
        }
        0x9e if c[1] & 0x1f == 0x10 => {
            let mut bytes = vec![0; 32];
            bytes[..8].copy_from_slice(&blocks.saturating_sub(1).to_be_bytes());
            bytes[8..12].copy_from_slice(&block.to_be_bytes());
            Ok(data(bytes, be32(&c[10..]) as usize))
        }
        0x08 | 0x0a | 0x28 | 0x2a | 0xa8 | 0xaa | 0x88 | 0x8a | 0x2f | 0x8f => {
            let (lba, n) = match c[0] {
                0x08 | 0x0a => (
                    ((c[1] as u64 & 0x1f) << 16) | ((c[2] as u64) << 8) | c[3] as u64,
                    if c[4] == 0 { 256 } else { c[4] as u64 },
                ),
                0x28 | 0x2a | 0x2f => (be32(&c[2..]) as u64, be16(&c[7..]) as u64),
                0xa8 | 0xaa => (be32(&c[2..]) as u64, be32(&c[6..]) as u64),
                _ => (be64(&c[2..]), be32(&c[10..]) as u64),
            };
            if lba.checked_add(n).is_none_or(|end| end > blocks) {
                return Err(Sense(5, 0x21, 0));
            }
            if matches!(c[0], 0x2f | 0x8f) {
                return if c[1] & 6 == 0 { Ok(Action::Good) } else { Err(INVALID_FIELD) };
            }
            let len =
                n.checked_mul(block as u64).filter(|len| *len <= MAX_TRANSFER as u64).ok_or(INVALID_FIELD)? as usize;
            let offset = lba * block as u64;
            if matches!(c[0], 0x0a | 0x2a | 0xaa | 0x8a) {
                if dev.read_only() {
                    return Err(PROTECTED);
                }
                Ok(Action::Write { offset, len, fua: c[0] != 0x0a && c[1] & 8 != 0 })
            } else {
                Ok(Action::Read { offset, len })
            }
        }
        0x1a | 0x5a => mode(lun, c),
        0xa0 => {
            let mut bytes = vec![0; 8 + 8 * count];
            bytes[..4].copy_from_slice(&((count * 8) as u32).to_be_bytes());
            for i in 0..count {
                bytes[8 + i * 8..10 + i * 8].copy_from_slice(&(i as u16).to_be_bytes());
            }
            Ok(data(bytes, be32(&c[6..]) as usize))
        }
        0x35 | 0x91 => Ok(Action::Flush),
        0x42 => {
            if dev.read_only() {
                Err(PROTECTED)
            } else {
                Ok(Action::IgnoreWrite(be16(&c[7..]) as usize))
            }
        }
        // Cleanly reject reservations instead of pretending to hold one.
        _ => Err(Sense(5, 0x20, 0)),
    }
}

fn inquiry(lun: &LunConfig, c: &[u8; 16]) -> Result<Action, Sense> {
    let allocation = be16(&c[3..]) as usize;
    if c[1] & 1 == 0 {
        if c[2] != 0 {
            return Err(INVALID_FIELD);
        }
        let mut bytes = vec![0; 36];
        bytes[2] = 6;
        bytes[3] = 2;
        bytes[4] = 31;
        bytes[7] = 2;
        for (start, len, text) in [(8, 8, &lun.vendor), (16, 16, &lun.product), (32, 4, &lun.revision)] {
            bytes[start..start + len].fill(b' ');
            bytes[start..start + text.len()].copy_from_slice(text.as_bytes());
        }
        return Ok(data(bytes, allocation));
    }
    let page = c[2];
    let payload = match page {
        0x00 => vec![0, 0x80, 0x83, 0xb0, 0xb1, 0xb2],
        0x80 => lun.serial.as_bytes().to_vec(),
        0x83 => {
            let mut id = lun.device_id;
            id[0] = (id[0] & 0x0f) | 0x60;
            let mut bytes = vec![1, 3, 0, 16];
            bytes.extend(id);
            bytes
        }
        0xb0 => {
            let mut bytes = vec![0; 60];
            bytes[4..8].copy_from_slice(&((MAX_TRANSFER as u32) / lun.device.block_size()).to_be_bytes());
            bytes
        }
        0xb1 => {
            let mut bytes = vec![0; 60];
            bytes[1] = 1;
            bytes
        }
        0xb2 => vec![0; 4],
        _ => return Err(INVALID_FIELD),
    };
    let mut bytes = vec![0, page];
    bytes.extend((payload.len() as u16).to_be_bytes());
    bytes.extend(payload);
    Ok(data(bytes, allocation))
}

fn mode(lun: &LunConfig, c: &[u8; 16]) -> Result<Action, Sense> {
    if c[2] >> 6 == 3 || c[3] != 0 {
        return Err(INVALID_FIELD);
    }
    let ten = c[0] == 0x5a;
    let mut bytes = vec![0; if ten { 8 } else { 4 }];
    if lun.device.read_only() {
        bytes[if ten { 3 } else { 2 }] = 0x80;
    }
    if c[1] & 8 == 0 {
        let block = lun.device.block_size().to_be_bytes();
        let mut descriptor = [0; 8];
        descriptor[5..8].copy_from_slice(&block[1..]);
        bytes.extend(descriptor);
        bytes[if ten { 7 } else { 3 }] = 8;
    }
    let page = c[2] & 0x3f;
    if !matches!(page, 8 | 10 | 0x3f) {
        return Err(INVALID_FIELD);
    }
    if matches!(page, 8 | 0x3f) {
        let mut p = vec![0; 20];
        p[0] = 8;
        p[1] = 18;
        if !lun.device.read_only() && c[2] >> 6 != 1 {
            p[2] = 4;
        }
        bytes.extend(p);
    }
    if matches!(page, 10 | 0x3f) {
        let mut p = vec![0; 12];
        p[0] = 10;
        p[1] = 10;
        bytes.extend(p);
    }
    if ten {
        let len = (bytes.len() - 2) as u16;
        bytes[..2].copy_from_slice(&len.to_be_bytes());
    } else {
        bytes[0] = (bytes.len() - 1) as u8;
    }
    Ok(data(bytes, if ten { be16(&c[7..]) as usize } else { c[4] as usize }))
}
