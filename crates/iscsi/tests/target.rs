use blockdev::{BlockDevice, MemDevice};
use iscsi::{generate_chap_secret, iqn_for, ChapSecret, LunConfig, TargetConfig, TargetServer};
use md5::{Digest, Md5};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

struct Packet {
    h: [u8; 48],
    data: Vec<u8>,
}
impl Packet {
    fn new(op: u8) -> Self {
        let mut h = [0; 48];
        h[0] = op;
        Self { h, data: Vec::new() }
    }
    fn n(&self, at: usize) -> u32 {
        u32::from_be_bytes(self.h[at..at + 4].try_into().unwrap())
    }
    fn set(&mut self, at: usize, n: u32) {
        self.h[at..at + 4].copy_from_slice(&n.to_be_bytes());
    }
    fn write(&self, stream: &mut TcpStream) {
        let mut h = self.h;
        h[5..8].copy_from_slice(&(self.data.len() as u32).to_be_bytes()[1..]);
        stream.write_all(&h).unwrap();
        stream.write_all(&self.data).unwrap();
        stream.write_all(&[0; 3][..(4 - self.data.len() % 4) % 4]).unwrap();
    }
    fn read(stream: &mut TcpStream) -> Self {
        let mut h = [0; 48];
        stream.read_exact(&mut h).unwrap();
        assert_eq!(h[4], 0);
        let len = u32::from_be_bytes([0, h[5], h[6], h[7]]) as usize;
        assert!(len <= 262144);
        let mut data = vec![0; len];
        stream.read_exact(&mut data).unwrap();
        let mut padding = [0; 3];
        stream.read_exact(&mut padding[..(4 - len % 4) % 4]).unwrap();
        Self { h, data }
    }
}
fn keys(entries: &[(&str, &str)]) -> Vec<u8> {
    entries.iter().flat_map(|(k, v)| format!("{k}={v}\0").into_bytes()).collect()
}
fn value(data: &[u8], key: &str) -> String {
    let text = std::str::from_utf8(data).unwrap();
    text.split('\0')
        .find_map(|entry| entry.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v.to_owned()))
        .unwrap()
}
fn lun(dev: Arc<dyn BlockDevice>) -> LunConfig {
    LunConfig {
        device: dev,
        vendor: "INOCRIUM".into(),
        product: "Image disk".into(),
        revision: "0001".into(),
        serial: "test-serial".into(),
        device_id: [0x12; 16],
    }
}
fn server(dev: Arc<dyn BlockDevice>, chap: Option<ChapSecret>) -> TargetServer {
    TargetServer::start(TargetConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        iqn: iqn_for("test"),
        chap,
        luns: vec![lun(dev)],
    })
    .unwrap()
}
struct Client {
    stream: TcpStream,
    cmd: u32,
    stat: u32,
    tag: u32,
    initial: bool,
}
impl Client {
    fn new(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_nodelay(true).unwrap();
        Self { stream, cmd: 1, stat: 0, tag: 1, initial: true }
    }
    fn send(&mut self, mut p: Packet) {
        if matches!(p.h[0] & 0x3f, 0 | 1 | 2 | 3 | 4 | 6) {
            p.set(24, self.cmd);
            p.set(28, self.stat);
            if p.h[0] & 0x40 == 0 {
                self.cmd = self.cmd.wrapping_add(1);
            }
        }
        p.write(&mut self.stream);
    }
    fn recv(&mut self) -> Packet {
        let p = Packet::read(&mut self.stream);
        let op = p.h[0] & 0x3f;
        let sequenced = op != 0x25 || p.h[1] & 1 != 0;
        if sequenced {
            assert_eq!(p.n(24), self.stat, "StatSN opcode {op:x}");
            if op != 0x31 {
                self.stat = self.stat.wrapping_add(1);
            }
        }
        assert_eq!(p.n(28), self.cmd, "ExpCmdSN opcode {op:x}");
        assert_eq!(p.n(32), self.cmd.wrapping_add(31));
        p
    }
    fn login_packet(&mut self, flags: u8, data: Vec<u8>) -> Packet {
        let mut p = Packet::new(0x43);
        p.h[1] = flags;
        p.h[8..14].copy_from_slice(&[0x80, 0, 0, 0, 0, 1]);
        p.set(16, 1);
        p.data = data;
        self.send(p);
        let reply = self.recv();
        assert_eq!(reply.h[0], 0x23);
        reply
    }
    fn login(&mut self, chap: Option<&ChapSecret>, discovery: bool, initial: bool) -> Result<(), u16> {
        self.initial = initial;
        let name = iqn_for("test");
        let base = [
            ("InitiatorName", "iqn.2026-01.org.inocrium:test-client"),
            ("SessionType", if discovery { "Discovery" } else { "Normal" }),
            ("TargetName", &name),
            ("AuthMethod", if chap.is_some() { "CHAP" } else { "None" }),
        ];
        let mut r = self.login_packet(if chap.is_some() { 0 } else { 0x81 }, keys(&base));
        if let Some(chap) = chap {
            assert_eq!(value(&r.data, "AuthMethod"), "CHAP");
            r = self.login_packet(0, keys(&[("CHAP_A", "5")]));
            let id = value(&r.data, "CHAP_I").parse::<u8>().unwrap();
            let challenge = value(&r.data, "CHAP_C");
            let challenge: Vec<u8> = (2..challenge.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&challenge[i..i + 2], 16).unwrap())
                .collect();
            let mut md5 = Md5::new();
            md5.update([id]);
            md5.update(&chap.secret);
            md5.update(challenge);
            let response = format!("0x{:x}", md5.finalize());
            r = self.login_packet(0x81, keys(&[("CHAP_N", &chap.user), ("CHAP_R", &response)]));
        }
        let status = u16::from_be_bytes([r.h[36], r.h[37]]);
        if status != 0 {
            return Err(status);
        }
        assert_eq!(r.h[1], 0x81);
        let r = self.login_packet(
            0x87,
            keys(&[
                ("HeaderDigest", "None"),
                ("DataDigest", "None"),
                ("MaxConnections", "1"),
                ("InitialR2T", if initial { "Yes" } else { "No" }),
                ("ImmediateData", "Yes"),
                ("MaxRecvDataSegmentLength", "1024"),
                ("MaxBurstLength", "8192"),
                ("FirstBurstLength", "4096"),
                ("MaxOutstandingR2T", "1"),
                ("DataPDUInOrder", "Yes"),
                ("DataSequenceInOrder", "Yes"),
                ("ErrorRecoveryLevel", "0"),
                ("DefaultTime2Wait", "0"),
                ("DefaultTime2Retain", "0"),
            ]),
        );
        assert_eq!(&r.h[36..38], &[0, 0]);
        assert_eq!(r.h[1], 0x87);
        if discovery {
            assert_eq!(&r.h[14..16], &[0, 0]);
        } else {
            assert_ne!(&r.h[14..16], &[0, 0]);
        }
        assert_eq!(value(&r.data, "MaxRecvDataSegmentLength"), "262144");
        Ok(())
    }
    fn command(&mut self, cdb: [u8; 16], len: usize, flags: u8, immediate: Vec<u8>) -> u32 {
        self.tag += 1;
        let tag = self.tag;
        let mut p = Packet::new(1);
        p.h[1] = flags;
        p.set(16, tag);
        p.set(20, len as u32);
        p.h[32..48].copy_from_slice(&cdb);
        p.data = immediate;
        self.send(p);
        tag
    }
    fn scsi(&mut self, cdb: [u8; 16], len: usize) -> (Vec<u8>, Packet, usize) {
        let tag = self.command(cdb, len, if len == 0 { 0x80 } else { 0xc0 }, Vec::new());
        let mut bytes = Vec::new();
        let mut count = 0;
        loop {
            let p = self.recv();
            assert_eq!(p.n(16), tag);
            if p.h[0] == 0x21 {
                return (bytes, p, count);
            }
            assert_eq!(p.h[0], 0x25);
            assert_eq!(p.n(36), count as u32);
            assert_eq!(p.n(40), bytes.len() as u32);
            assert!(p.data.len() <= 1024);
            bytes.extend_from_slice(&p.data);
            count += 1;
            if p.h[1] & 1 != 0 {
                return (bytes, p, count);
            }
        }
    }
    fn burst(&mut self, tag: u32, ttt: u32, bytes: &[u8], start: usize, end: usize) {
        for (sequence, chunk) in bytes[start..end].chunks(1024).enumerate() {
            let mut p = Packet::new(5);
            p.h[1] = if start + sequence * 1024 + chunk.len() == end { 0x80 } else { 0 };
            p.set(16, tag);
            p.set(20, ttt);
            p.set(28, self.stat);
            p.set(36, sequence as u32);
            p.set(40, (start + sequence * 1024) as u32);
            p.data = chunk.to_vec();
            self.send(p);
        }
    }
    fn write(&mut self, cdb: [u8; 16], bytes: &[u8]) -> (Packet, usize) {
        let immediate = bytes.len().min(1024);
        let unsolicited = !self.initial && bytes.len() > immediate;
        let tag = self.command(cdb, bytes.len(), if unsolicited { 0x20 } else { 0xa0 }, bytes[..immediate].to_vec());
        let mut offset = immediate;
        if unsolicited {
            let end = bytes.len().min(4096);
            self.burst(tag, u32::MAX, bytes, offset, end);
            offset = end;
        }
        let mut count = 0;
        loop {
            let p = self.recv();
            assert_eq!(p.n(16), tag);
            if p.h[0] == 0x21 {
                return (p, count);
            }
            assert_eq!(p.h[0], 0x31);
            assert_eq!(p.n(36), count as u32);
            assert_eq!(p.n(40), offset as u32);
            let end = offset + p.n(44) as usize;
            assert!(end <= bytes.len());
            assert!(p.n(44) <= 8192);
            self.burst(tag, p.n(20), bytes, offset, end);
            offset = end;
            count += 1;
        }
    }
    fn logout(&mut self) {
        let mut p = Packet::new(6);
        p.h[1] = 0x80;
        p.set(16, 99);
        self.send(p);
        assert_eq!(self.recv().h[0], 0x26);
    }
}
fn cdb10(op: u8, lba: u32, blocks: u16) -> [u8; 16] {
    let mut c = [0; 16];
    c[0] = op;
    c[2..6].copy_from_slice(&lba.to_be_bytes());
    c[7..9].copy_from_slice(&blocks.to_be_bytes());
    c
}
fn check_sense(p: &Packet, key: u8, asc: u8) {
    assert_eq!(p.h[3], 2);
    assert_eq!(&p.data[..2], &[0, 18]);
    assert_eq!(p.data[4], key);
    assert_eq!(p.data[14], asc);
    assert_eq!(p.data[15], 0);
}
fn connected(dev: Arc<dyn BlockDevice>) -> (TargetServer, Client) {
    let s = server(dev, None);
    let mut c = Client::new(s.local_addr());
    c.login(None, false, true).unwrap();
    (s, c)
}

#[test]
fn discovery_nop_task_reject_logout() {
    let s = server(Arc::new(MemDevice::new(1024 * 1024, 512)), None);
    let mut c = Client::new(s.local_addr());
    c.login(None, true, true).unwrap();
    let mut p = Packet::new(4);
    p.h[1] = 0x80;
    p.set(16, 55);
    p.set(20, u32::MAX);
    p.data = keys(&[("SendTargets", "All")]);
    c.send(p);
    let r = c.recv();
    assert_eq!(r.h[0], 0x24);
    assert_eq!(r.n(20), u32::MAX);
    assert_eq!(value(&r.data, "TargetName"), s.iqn());
    assert_eq!(value(&r.data, "TargetAddress"), format!("{},1", s.local_addr()));
    let mut p = Packet::new(0x40);
    p.h[1] = 0x80;
    p.set(16, 9);
    p.set(20, u32::MAX);
    p.data = b"ping".to_vec();
    c.send(p);
    let r = c.recv();
    assert_eq!(r.h[0], 0x20);
    assert_eq!(r.data, b"ping");
    c.send(Packet::new(0x3e));
    assert_eq!(c.recv().h[0], 0x3f);
    c.logout();
    let mut c = Client::new(s.local_addr());
    c.login(None, false, true).unwrap();
    let mut p = Packet::new(2);
    p.h[1] = 0x81;
    p.set(16, 17);
    c.send(p);
    let r = c.recv();
    assert_eq!(r.h[0], 0x22);
    assert_eq!(r.h[2], 0);
    c.logout();
}

#[test]
fn chap_success_and_authentication_failure() {
    let secret = generate_chap_secret();
    let s = server(Arc::new(MemDevice::new(65536, 512)), Some(secret.clone()));
    let mut c = Client::new(s.local_addr());
    c.login(Some(&secret), false, true).unwrap();
    c.logout();
    let mut wrong = secret.clone();
    wrong.secret[0] ^= 1;
    let mut c = Client::new(s.local_addr());
    assert_eq!(c.login(Some(&wrong), false, true), Err(0x0201));
    let mut c = Client::new(s.local_addr());
    assert_eq!(c.login(None, false, true), Err(0x0201));
}

#[test]
fn inquiry_and_vpd() {
    let (_s, mut c) = connected(Arc::new(MemDevice::new(65536, 512)));
    let mut cmd = [0; 16];
    cmd[0] = 0x12;
    cmd[4] = 255;
    let (bytes, p, _) = c.scsi(cmd, 255);
    assert_eq!(p.h[3], 0);
    assert_eq!(bytes.len(), 36);
    assert_eq!(&bytes[8..16], b"INOCRIUM");
    assert_eq!(&bytes[16..32], b"Image disk      ");
    assert_eq!(p.h[1] & 2, 2);
    assert_eq!(p.n(44), 219);
    for page in [0, 0x80, 0x83, 0xb0, 0xb1, 0xb2] {
        cmd[1] = 1;
        cmd[2] = page;
        let (bytes, p, _) = c.scsi(cmd, 255);
        assert_eq!(p.h[3], 0);
        assert_eq!(bytes[1], page);
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]) as usize, bytes.len() - 4);
        if page == 0x80 {
            assert_eq!(&bytes[4..], b"test-serial");
        }
        if page == 0x83 {
            assert_eq!(&bytes[4..8], &[1, 3, 0, 16]);
            assert_eq!(bytes[8] >> 4, 6);
            assert_eq!(bytes.len(), 24);
        }
        if page == 0xb0 {
            assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 32768);
        }
    }
    cmd[2] = 0xff;
    check_sense(&c.scsi(cmd, 255).1, 5, 0x24);
    c.logout();
}

#[test]
fn large_write_read_both_r2t_modes() {
    for initial in [false, true] {
        let dev = Arc::new(MemDevice::new(1024 * 1024, 512));
        let s = server(dev.clone(), None);
        let mut c = Client::new(s.local_addr());
        c.login(None, false, initial).unwrap();
        let bytes: Vec<u8> = (0..65536).map(|i| ((i * 31 + i / 1024) % 251) as u8).collect();
        let (r, count) = c.write(cdb10(0x2a, 8, 128), &bytes);
        assert_eq!(r.h[3], 0);
        assert!(count >= 7);
        let (read, r, count) = c.scsi(cdb10(0x28, 8, 128), 65536);
        assert_eq!(r.h[3], 0);
        assert_eq!(count, 64);
        assert_eq!(read, bytes);
        let mut direct = vec![0; bytes.len()];
        dev.read_at(4096, &mut direct).unwrap();
        assert_eq!(direct, bytes);
        c.logout();
    }
}

struct ZeroDevice {
    size: u64,
    fail: bool,
    flush: AtomicUsize,
}
impl BlockDevice for ZeroDevice {
    fn size(&self) -> u64 {
        self.size
    }
    fn block_size(&self) -> u32 {
        512
    }
    fn read_only(&self) -> bool {
        false
    }
    fn read_at(&self, _: u64, bytes: &mut [u8]) -> io::Result<()> {
        if self.fail {
            Err(io::Error::other("read failure"))
        } else {
            bytes.fill(0);
            Ok(())
        }
    }
    fn write_at(&self, _: u64, _: &[u8]) -> io::Result<()> {
        if self.fail {
            Err(io::Error::other("write failure"))
        } else {
            Ok(())
        }
    }
    fn flush(&self) -> io::Result<()> {
        self.flush.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[test]
fn capacity_over_two_tib_and_read16() {
    let size = 3u64 << 40;
    let (_s, mut c) = connected(Arc::new(ZeroDevice { size, fail: false, flush: AtomicUsize::new(0) }));
    let (b, _, _) = c.scsi(cdb10(0x25, 0, 0), 8);
    assert_eq!(&b[..4], &[255; 4]);
    assert_eq!(&b[4..], &512u32.to_be_bytes());
    let mut cmd = [0; 16];
    cmd[0] = 0x9e;
    cmd[1] = 0x10;
    cmd[10..14].copy_from_slice(&32u32.to_be_bytes());
    let (b, _, _) = c.scsi(cmd, 32);
    assert_eq!(u64::from_be_bytes(b[..8].try_into().unwrap()), size / 512 - 1);
    cmd = [0; 16];
    cmd[0] = 0x88;
    cmd[2..10].copy_from_slice(&(u32::MAX as u64 + 1).to_be_bytes());
    cmd[10..14].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(c.scsi(cmd, 512).0, vec![0; 512]);
    c.logout();
}

#[test]
fn write_protection_mode_sense_and_sense_errors() {
    let (_s, mut c) = connected(Arc::new(MemDevice::new(65536, 512).with_read_only(true)));
    let (r, _) = c.write(cdb10(0x2a, 0, 1), &[0; 512]);
    check_sense(&r, 7, 0x27);
    for op in [0x1a, 0x5a] {
        let mut cmd = [0; 16];
        cmd[0] = op;
        cmd[1] = 8;
        cmd[2] = 0x3f;
        if op == 0x1a {
            cmd[4] = 255;
        } else {
            cmd[8] = 255;
        }
        let (b, _, _) = c.scsi(cmd, 255);
        assert_eq!(b[if op == 0x1a { 2 } else { 3 }] & 0x80, 0x80);
        assert_eq!(b[if op == 0x1a { 4 } else { 8 }], 8);
    }
    check_sense(&c.scsi(cdb10(0x28, 128, 1), 512).1, 5, 0x21);
    let mut cmd = [0; 16];
    cmd[0] = 0xff;
    check_sense(&c.scsi(cmd, 0).1, 5, 0x20);
    cmd[0] = 3;
    cmd[4] = 18;
    let (b, _, _) = c.scsi(cmd, 18);
    assert_eq!(b[2], 5);
    assert_eq!(b[12], 0x20);
    c.logout();
}

#[test]
fn io_failures_and_flush() {
    let dev = Arc::new(ZeroDevice { size: 65536, fail: true, flush: AtomicUsize::new(0) });
    let (_s, mut c) = connected(dev.clone());
    check_sense(&c.scsi(cdb10(0x28, 0, 1), 512).1, 3, 0x11);
    let (r, _) = c.write(cdb10(0x2a, 0, 1), &[0; 512]);
    check_sense(&r, 3, 0x0c);
    for op in [0x35, 0x91] {
        let mut cmd = [0; 16];
        cmd[0] = op;
        assert_eq!(c.scsi(cmd, 0).1.h[3], 0);
    }
    assert_eq!(dev.flush.load(Ordering::Relaxed), 2);
    c.logout();
}

#[test]
fn all_read_write_cdb_sizes_and_noop_commands() {
    let (_s, mut c) = connected(Arc::new(MemDevice::new(1024 * 1024, 512)));
    for (read, write) in [(8, 10), (0x28, 0x2a), (0xa8, 0xaa), (0x88, 0x8a)] {
        let mut cmd = [0; 16];
        cmd[0] = write;
        match write {
            10 => {
                cmd[3] = 2;
                cmd[4] = 1;
            }
            0x2a => {
                cmd[5] = 2;
                cmd[8] = 1;
            }
            0xaa => {
                cmd[5] = 2;
                cmd[9] = 1;
            }
            _ => {
                cmd[9] = 2;
                cmd[13] = 1;
            }
        }
        let (r, _) = c.write(cmd, &[write; 512]);
        assert_eq!(r.h[3], 0);
        cmd[0] = read;
        assert_eq!(c.scsi(cmd, 512).0, vec![write; 512]);
    }
    for op in [0, 0x1b, 0x1e, 0x2f, 0x8f] {
        let mut cmd = [0; 16];
        cmd[0] = op;
        assert_eq!(c.scsi(cmd, 0).1.h[3], 0);
    }
    for op in [0x5e, 0x5f] {
        let mut cmd = [0; 16];
        cmd[0] = op;
        check_sense(&c.scsi(cmd, 0).1, 5, 0x20);
    }
    let mut cmd = [0; 16];
    cmd[0] = 0x42;
    cmd[8] = 24;
    assert_eq!(c.write(cmd, &[0; 24]).0.h[3], 0);
    c.logout();
}

#[test]
fn concurrent_connections_multiple_luns_and_shutdown() {
    let dev = Arc::new(MemDevice::new(65536, 512));
    let mut s = TargetServer::start(TargetConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        iqn: iqn_for("test"),
        chap: None,
        luns: vec![lun(dev.clone()), lun(dev)],
    })
    .unwrap();
    let address = s.local_addr();
    let workers: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(move || {
                let mut c = Client::new(address);
                c.login(None, false, true).unwrap();
                let mut cmd = [0; 16];
                cmd[0] = 0xa0;
                cmd[9] = 64;
                let (b, _, _) = c.scsi(cmd, 64);
                assert_eq!(&b[..4], &16u32.to_be_bytes());
                assert_eq!(&b[16..18], &[0, 1]);
                for _ in 0..8 {
                    assert_eq!(c.scsi(cdb10(0x28, 0, 1), 512).0, vec![0; 512]);
                }
                c.logout();
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    let mut stalled = TcpStream::connect(address).unwrap();
    stalled.write_all(&[0x43, 0]).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let start = Instant::now();
    s.stop();
    s.stop();
    assert!(start.elapsed() < Duration::from_secs(2));
    let mut b = [0];
    assert!(matches!(stalled.read(&mut b), Ok(0) | Err(_)));
}

#[test]
fn invalid_config_is_rejected() {
    let mut cfg = TargetConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        iqn: iqn_for("test"),
        chap: None,
        luns: vec![lun(Arc::new(MemDevice::new(65536, 512)))],
    };
    cfg.luns[0].vendor = "vendor too long".into();
    assert!(TargetServer::start(cfg).is_err());
}

#[test]
fn pipelined_command_during_solicited_write() {
    let (_s, mut c) = connected(Arc::new(MemDevice::new(65536, 512)));
    let bytes = vec![0x5a; 16384];
    let write_tag = c.command(cdb10(0x2a, 0, 32), bytes.len(), 0xa0, Vec::new());
    let first = c.recv();
    assert_eq!(first.h[0], 0x31);
    let read_tag = c.command(cdb10(0x28, 0, 32), bytes.len(), 0xc0, Vec::new());
    let end = first.n(44) as usize;
    c.burst(write_tag, first.n(20), &bytes, 0, end);
    let second = c.recv();
    assert_eq!(second.h[0], 0x31);
    assert_eq!(second.n(36), 1);
    assert_eq!(second.n(40) as usize, end);
    c.burst(write_tag, second.n(20), &bytes, end, bytes.len());
    let response = c.recv();
    assert_eq!(response.h[0], 0x21);
    assert_eq!(response.n(16), write_tag);
    assert_eq!(response.h[3], 0);
    let mut read = Vec::new();
    for i in 0..16 {
        let p = c.recv();
        assert_eq!(p.h[0], 0x25);
        assert_eq!(p.n(16), read_tag);
        assert_eq!(p.n(36), i);
        assert_eq!(p.h[1] & 0x80 != 0, i == 7 || i == 15);
        assert_eq!(p.h[1] & 1 != 0, i == 15);
        if i != 15 {
            assert_eq!(p.n(24), 0);
        }
        read.extend(p.data);
    }
    assert_eq!(read, bytes);
    c.logout();
}

#[test]
fn disabled_immediate_data_and_read_overflow() {
    let (_s, mut c) = connected(Arc::new(MemDevice::new(65536, 512)));
    // Negotiate ImmediateData=No on a fresh connection.
    let mut other = Client::new(_s.local_addr());
    let name = iqn_for("test");
    let r = other.login_packet(0x81, keys(&[("TargetName", &name), ("AuthMethod", "None")]));
    assert_eq!(&r.h[36..38], &[0, 0]);
    let r = other.login_packet(
        0x87,
        keys(&[("ImmediateData", "No"), ("InitialR2T", "Yes"), ("MaxRecvDataSegmentLength", "1024")]),
    );
    assert_eq!(value(&r.data, "ImmediateData"), "No");
    let tag = other.command(cdb10(0x2a, 0, 1), 512, 0xa0, Vec::new());
    let r = other.recv();
    assert_eq!(r.h[0], 0x31);
    other.burst(tag, r.n(20), &[0x77; 512], 0, 512);
    assert_eq!(other.recv().h[3], 0);
    let (b, r, _) = c.scsi(cdb10(0x28, 0, 1), 256);
    assert_eq!(b, vec![0x77; 256]);
    assert_eq!(r.h[1] & 4, 4);
    assert_eq!(r.n(44), 256);
    other.logout();
    c.logout();
}

#[test]
fn partial_header_survives_socket_poll_timeouts() {
    let s = server(Arc::new(MemDevice::new(65536, 512)), None);
    let mut c = Client::new(s.local_addr());
    let mut p = Packet::new(0x43);
    p.h[1] = 0x81;
    p.set(16, 1);
    p.set(24, c.cmd);
    p.data = keys(&[("TargetName", s.iqn()), ("AuthMethod", "None")]);
    p.h[5..8].copy_from_slice(&(p.data.len() as u32).to_be_bytes()[1..]);
    c.stream.write_all(&p.h[..7]).unwrap();
    std::thread::sleep(Duration::from_millis(450));
    c.stream.write_all(&p.h[7..]).unwrap();
    c.stream.write_all(&p.data).unwrap();
    c.stream.write_all(&[0; 3][..(4 - p.data.len() % 4) % 4]).unwrap();
    let r = c.recv();
    assert_eq!(r.h[0], 0x23);
    assert_eq!(r.h[1], 0x81);
    let r = c.login_packet(0x87, Vec::new());
    assert_eq!(r.h[1], 0x87);
    c.logout();
}

#[test]
fn request_sense_is_cleared_and_sequence_wraps() {
    let s = server(Arc::new(MemDevice::new(65536, 512)), None);
    let mut c = Client::new(s.local_addr());
    c.cmd = u32::MAX;
    c.login(None, false, true).unwrap();
    let mut cmd = [0; 16];
    cmd[0] = 0xff;
    check_sense(&c.scsi(cmd, 0).1, 5, 0x20);
    assert_eq!(c.cmd, 0);
    cmd[0] = 3;
    cmd[4] = 18;
    assert_eq!(c.scsi(cmd, 18).0[2], 5);
    assert_eq!(c.scsi(cmd, 18).0[2], 0);
    c.logout();
}

#[cfg(windows)]
#[test]
#[ignore = "Requires an elevated shell and Microsoft iSCSI Initiator; run cargo test -p iscsi windows_initiator_end_to_end -- --ignored"]
fn windows_initiator_end_to_end() {
    use iscsi::initiator;
    // The target must remain alive until logout and portal removal complete.
    let secret = generate_chap_secret();
    // A unique name, so a session left by an interrupted run cannot block the login.
    let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let s = TargetServer::start(TargetConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        iqn: iqn_for(&format!("test-{unique}")),
        chap: Some(secret.clone()),
        luns: vec![lun(Arc::new(MemDevice::new(64 * 1024 * 1024, 512)))],
    })
    .unwrap();
    let addr = s.local_addr();
    initiator::ensure_initiator_service().unwrap();
    initiator::add_portal(addr, Some(&secret)).unwrap();
    let session = match initiator::login(s.iqn(), addr, Some(&secret)) {
        Ok(session) => session,
        Err(e) => {
            let _ = initiator::remove_portal(addr);
            panic!("{e}");
        }
    };
    let result = initiator::session_disks(&session, Duration::from_secs(30));
    let logout = initiator::logout(&session);
    let remove = initiator::remove_portal(addr);
    assert!(!result.unwrap().is_empty());
    logout.unwrap();
    remove.unwrap();
}

#[cfg(windows)]
#[test]
#[ignore = "Requires an elevated shell; logs out every session and portal left by inocrium targets on this machine"]
fn windows_cleanup_stale_sessions() {
    iscsi::initiator::cleanup_stale("iqn.2026-01.org.inocrium:image-").unwrap();
}
