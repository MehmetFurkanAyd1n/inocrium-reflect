use crate::{
    login::Negotiation,
    pdu::{self, invalid, Pdu, MAX_SEGMENT},
    scsi::{self, Action, Sense},
    TargetConfig,
};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

static NEXT_SESSION: AtomicU16 = AtomicU16::new(1);

/// An owned TCP listener and its connection threads. Dropping it stops all work.
pub struct TargetServer {
    cfg: Arc<TargetConfig>,
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    connections: Arc<Mutex<HashMap<u64, TcpStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl TargetServer {
    /// Validate configuration, bind the listener and start accepting connections.
    pub fn start(cfg: TargetConfig) -> io::Result<Self> {
        validate(&cfg)?;
        let listener = TcpListener::bind(cfg.bind)?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let cfg = Arc::new(cfg);
        let stopping = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(HashMap::<u64, TcpStream>::new()));
        let (config, stop, sockets) = (cfg.clone(), stopping.clone(), connections.clone());
        let thread = thread::Builder::new().name("iscsi-listener".into()).spawn(move || {
            let mut workers: Vec<JoinHandle<()>> = Vec::new();
            let mut next = 0u64;
            while !stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Winsock accepted sockets inherit the listener's nonblocking mode.
                        if stream
                            .set_nonblocking(false)
                            .and_then(|()| stream.set_read_timeout(Some(Duration::from_millis(200))))
                            .and_then(|()| stream.set_write_timeout(Some(Duration::from_millis(200))))
                            .is_err()
                        {
                            continue;
                        }
                        let _ = stream.set_nodelay(true);
                        let id = next;
                        next = next.wrapping_add(1);
                        let Ok(copy) = stream.try_clone() else { continue };
                        sockets.lock().unwrap().insert(id, copy);
                        if stop.load(Ordering::Acquire) {
                            let _ = stream.shutdown(Shutdown::Both);
                            break;
                        }
                        let (cfg, registry, stopping) = (config.clone(), sockets.clone(), stop.clone());
                        match thread::Builder::new().name(format!("iscsi-{id}")).spawn(move || {
                            if let Err(error) = Connection::new(stream, cfg, address, stopping).run() {
                                tracing::debug!(%error,"iSCSI connection closed");
                            }
                            registry.lock().unwrap().remove(&id);
                        }) {
                            Ok(worker) => workers.push(worker),
                            Err(_) => {
                                sockets.lock().unwrap().remove(&id);
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
                let mut i = 0;
                while i < workers.len() {
                    if workers[i].is_finished() {
                        let _ = workers.swap_remove(i).join();
                    } else {
                        i += 1;
                    }
                }
            }
            for socket in sockets.lock().unwrap().values() {
                let _ = socket.shutdown(Shutdown::Both);
            }
            for worker in workers {
                let _ = worker.join();
            }
        })?;
        Ok(Self { cfg, address, stopping, connections, thread: Some(thread) })
    }
    /// Bound address, including the assigned ephemeral port.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
    /// Qualified name exposed by this target.
    pub fn iqn(&self) -> &str {
        &self.cfg.iqn
    }
    /// Close the listener and connections and join their threads. Idempotent.
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        for socket in self.connections.lock().unwrap().values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for TargetServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate(cfg: &TargetConfig) -> io::Result<()> {
    let valid_text = |s: &str, max: usize| s.is_ascii() && s.len() <= max && !s.bytes().any(|b| b < 32 || b == 127);
    if !cfg.iqn.starts_with("iqn.") || !valid_text(&cfg.iqn, 223) || cfg.iqn.contains(['=', ',', ' ']) {
        return Err(invalid("invalid target IQN"));
    }
    if cfg.luns.is_empty() || cfg.luns.len() > 256 {
        return Err(invalid("one to 256 LUNs required"));
    }
    for lun in &cfg.luns {
        if !valid_text(&lun.vendor, 8)
            || !valid_text(&lun.product, 16)
            || !valid_text(&lun.revision, 4)
            || !valid_text(&lun.serial, 252)
            || !matches!(lun.device.block_size(), 512 | 4096)
            || lun.device.size() == 0
            || lun.device.size() % lun.device.block_size() as u64 != 0
        {
            return Err(invalid("invalid LUN identity or geometry"));
        }
    }
    if let Some(chap) = &cfg.chap {
        if chap.user.is_empty()
            || !valid_text(&chap.user, 223)
            || !matches!(chap.secret.len(), 12..=16)
            || !chap.secret.iter().all(|b| (32..=126).contains(b))
        {
            return Err(invalid("CHAP needs a username and twelve to sixteen printable secret bytes"));
        }
    }
    Ok(())
}

// shutdown on a duplicated Winsock handle does not reliably interrupt a pending recv.
// Retry timeouts inside Read/Write so partial headers survive and stop stays bounded.
struct InterruptibleStream {
    inner: TcpStream,
    stopping: Arc<AtomicBool>,
}
impl InterruptibleStream {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
    fn check_stop(&self) -> io::Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, "target stopped"))
        } else {
            Ok(())
        }
    }
}
impl Read for InterruptibleStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check_stop()?;
            match self.inner.read(buf) {
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
                result => return result,
            }
        }
    }
}
impl Write for InterruptibleStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            self.check_stop()?;
            match self.inner.write(buf) {
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct Connection {
    stream: InterruptibleStream,
    cfg: Arc<TargetConfig>,
    address: SocketAddr,
    negotiation: Negotiation,
    stat: u32,
    command: u32,
    full: bool,
    stage: u8,
    tsih: u16,
    sense: Vec<Sense>,
    login_fragment: Vec<u8>,
    pending: VecDeque<Pdu>,
}

impl Connection {
    fn new(stream: TcpStream, cfg: Arc<TargetConfig>, address: SocketAddr, stopping: Arc<AtomicBool>) -> Self {
        let sense = vec![Sense(0, 0, 0); cfg.luns.len()];
        Self {
            stream: InterruptibleStream { inner: stream, stopping },
            cfg,
            address,
            negotiation: Negotiation::default(),
            stat: 0,
            command: 0,
            full: false,
            stage: 0,
            tsih: NEXT_SESSION.fetch_add(1, Ordering::Relaxed).max(1),
            sense,
            login_fragment: Vec::new(),
            pending: VecDeque::new(),
        }
    }
    fn numbers(&mut self, p: &mut Pdu, advance: bool) {
        p.put32(24, self.stat);
        p.put32(28, self.command);
        p.put32(32, self.command.wrapping_add(31));
        if advance {
            self.stat = self.stat.wrapping_add(1);
        }
    }
    fn answer(&mut self, request: &Pdu, opcode: u8) -> Pdu {
        let mut p = Pdu::new(opcode);
        p.header[1] = 0x80;
        p.header[8..16].copy_from_slice(&request.header[8..16]);
        p.put32(16, request.get32(16));
        p
    }
    fn run(mut self) -> io::Result<()> {
        let first = Pdu::read(&mut self.stream)?;
        if first.opcode() != 3 {
            return Err(invalid("first PDU must be login"));
        }
        self.command = first.get32(24);
        if !self.login(&first)? {
            return Ok(());
        }
        loop {
            let (p, accepted) = match self.pending.pop_front() {
                Some(p) => (p, true),
                None => (Pdu::read(&mut self.stream)?, false),
            };
            if !self.full {
                if p.opcode() != 3 {
                    return Err(invalid("command before login"));
                }
                if !self.login(&p)? {
                    return Ok(());
                }
                continue;
            }
            if p.opcode() == 3 {
                return Err(invalid("login after full feature phase"));
            }
            if !accepted && !self.accept_command(&p)? {
                continue;
            }
            match p.opcode() {
                0 => self.nop(&p)?,
                1 if !self.negotiation.discovery => self.scsi(&p)?,
                2 if !self.negotiation.discovery => self.task(&p)?,
                4 => self.text(&p)?,
                6 => {
                    let mut reply = self.answer(&p, 0x26);
                    self.numbers(&mut reply, true);
                    reply.write(&mut self.stream)?;
                    return Ok(());
                }
                _ => self.reject(&p, 4)?,
            }
        }
    }
    fn accept_command(&mut self, p: &Pdu) -> io::Result<bool> {
        if matches!(p.opcode(), 0 | 1 | 2 | 4 | 6) && p.header[0] & 0x40 == 0 {
            if p.get32(24) != self.command {
                self.reject(p, 9)?;
                return Ok(false);
            }
            self.command = self.command.wrapping_add(1);
        }
        Ok(true)
    }
    fn login(&mut self, p: &Pdu) -> io::Result<bool> {
        let csg = (p.header[1] >> 2) & 3;
        let nsg = p.header[1] & 3;
        let transit = p.header[1] & 0x80 != 0;
        let continued = p.header[1] & 0x40 != 0;
        let mut status = 0u16;
        if p.header[2] != 0
            || csg != self.stage
            || (transit && continued)
            || (transit && !matches!((csg, nsg), (0, 1) | (0, 3) | (1, 3)))
            || p.header[14..16] != [0, 0]
        {
            status = 0x0200;
        }
        self.login_fragment.extend_from_slice(&p.data);
        if self.login_fragment.len() > MAX_SEGMENT {
            return Err(invalid("login text too large"));
        }
        let mut reply_keys = Vec::new();
        if !continued && status == 0 {
            let keys = pdu::text(&std::mem::take(&mut self.login_fragment))?;
            match self.negotiation.security(&self.cfg, &keys) {
                Ok(keys) => reply_keys.extend(keys),
                Err(s) => status = s,
            }
            if status == 0 && csg == 1 {
                if !self.negotiation.authenticated {
                    status = 0x0201;
                } else {
                    match self.negotiation.operational(&keys) {
                        Ok(keys) => reply_keys.extend(keys),
                        Err(_) => status = 0x0200,
                    }
                }
            }
        }
        let allowed = transit && status == 0 && self.negotiation.authenticated;
        if allowed && nsg == 3 && !self.negotiation.discovery && !self.negotiation.target_seen {
            status = 0x0203;
        }
        let mut reply = self.answer(p, 0x23);
        reply.header[1] = (csg << 2) | if allowed && status == 0 { 0x80 | nsg } else { 0 };
        reply.header[8..14].copy_from_slice(&p.header[8..14]);
        if allowed && status == 0 && nsg == 3 {
            if !self.negotiation.discovery {
                reply.header[14..16].copy_from_slice(&self.tsih.to_be_bytes());
            }
            self.full = true;
        }
        reply.header[36..38].copy_from_slice(&status.to_be_bytes());
        reply.data = pdu::encode_text(&reply_keys);
        self.numbers(&mut reply, true);
        reply.write(&mut self.stream)?;
        if allowed && status == 0 {
            self.stage = nsg;
        }
        Ok(status == 0)
    }
    fn text(&mut self, p: &Pdu) -> io::Result<()> {
        if p.header[1] & 0x40 != 0 || p.get32(20) != u32::MAX {
            return self.reject(p, 4);
        }
        let keys = pdu::text(&p.data)?;
        let mut response = Vec::new();
        for (k, v) in keys {
            if k == "SendTargets" && (v == "All" || v == self.cfg.iqn) {
                response.push(("TargetName".into(), self.cfg.iqn.clone()));
                let ip =
                    if self.address.ip().is_unspecified() { self.stream.local_addr()?.ip() } else { self.address.ip() };
                response.push(("TargetAddress".into(), format!("{},1", SocketAddr::new(ip, self.address.port()))));
            } else {
                response.push((k, "NotUnderstood".into()));
            }
        }
        let mut reply = self.answer(p, 0x24);
        reply.put32(20, u32::MAX);
        reply.data = pdu::encode_text(&response);
        self.numbers(&mut reply, true);
        reply.write(&mut self.stream)
    }
    fn nop(&mut self, p: &Pdu) -> io::Result<()> {
        if p.get32(16) == u32::MAX {
            return Ok(());
        }
        let mut reply = self.answer(p, 0x20);
        reply.put32(20, u32::MAX);
        reply.data = p.data.clone();
        self.numbers(&mut reply, true);
        reply.write(&mut self.stream)
    }
    fn task(&mut self, p: &Pdu) -> io::Result<()> {
        let mut reply = self.answer(p, 0x22);
        self.numbers(&mut reply, true);
        reply.write(&mut self.stream)
    }
    fn reject(&mut self, p: &Pdu, reason: u8) -> io::Result<()> {
        let mut reply = Pdu::new(0x3f);
        reply.header[1] = 0x80;
        reply.header[2] = reason;
        reply.put32(16, u32::MAX);
        reply.data = p.header.to_vec();
        self.numbers(&mut reply, true);
        reply.write(&mut self.stream)
    }
    fn status(&mut self, p: &Pdu, sense: Option<Sense>, transferred: usize) -> io::Result<()> {
        let mut reply = self.answer(p, 0x21);
        reply.put32(36, 0);
        if let Some(sense) = sense {
            reply.header[3] = 2;
            let bytes = sense.fixed();
            reply.data = (bytes.len() as u16).to_be_bytes().to_vec();
            reply.data.extend(bytes);
        }
        residual(&mut reply, p.get32(20) as usize, transferred);
        self.numbers(&mut reply, true);
        reply.write(&mut self.stream)
    }
    fn scsi(&mut self, p: &Pdu) -> io::Result<()> {
        let lun_index = (((p.header[8] & 0x3f) as usize) << 8) | p.header[9] as usize;
        if p.header[8] & 0xc0 > 0x40 || p.header[10..16] != [0; 6] || lun_index >= self.cfg.luns.len() {
            return self.status(p, Some(scsi::NO_LUN), 0);
        }
        let cdb: [u8; 16] = p.header[32..48].try_into().unwrap();
        let action = scsi::plan(&self.cfg.luns[lun_index], &cdb, self.cfg.luns.len(), self.sense[lun_index]);
        let result = match action {
            Ok(Action::Data(bytes)) => {
                if cdb[0] == 3 {
                    self.sense[lun_index] = Sense(0, 0, 0);
                }
                return self.data_in(p, &bytes);
            }
            Ok(Action::Read { offset, len }) => {
                if p.header[1] & 0x40 == 0 || !p.data.is_empty() {
                    Err(scsi::INVALID_FIELD)
                } else {
                    let mut bytes = vec![0; len];
                    match self.cfg.luns[lun_index].device.read_at(offset, &mut bytes) {
                        Ok(()) => return self.data_in(p, &bytes),
                        Err(_) => Err(Sense(3, 0x11, 0)),
                    }
                }
            }
            Ok(Action::Write { offset, len, fua }) => {
                if p.header[1] & 0x20 == 0 || p.get32(20) as usize != len {
                    Err(scsi::INVALID_FIELD)
                } else {
                    let bytes = self.write_data(p, len)?;
                    let dev = &self.cfg.luns[lun_index].device;
                    dev.write_at(offset, &bytes)
                        .and_then(|()| if fua { dev.flush() } else { Ok(()) })
                        .map(|()| len)
                        .map_err(|_| Sense(3, 0x0c, 0))
                }
            }
            Ok(Action::IgnoreWrite(len)) => {
                if p.header[1] & 0x20 == 0 || p.get32(20) as usize != len {
                    Err(scsi::INVALID_FIELD)
                } else {
                    self.write_data(p, len)?;
                    Ok(len)
                }
            }
            Ok(Action::Flush) => self.cfg.luns[lun_index].device.flush().map(|()| 0).map_err(|_| Sense(3, 0x0c, 0)),
            Ok(Action::Good) => Ok(0),
            Err(sense) => Err(sense),
        };
        match result {
            Ok(len) => {
                if cdb[0] == 3 {
                    self.sense[lun_index] = Sense(0, 0, 0);
                }
                self.status(p, None, len)
            }
            Err(sense) => {
                self.sense[lun_index] = sense;
                self.status(p, Some(sense), 0)
            }
        }
    }
    fn data_in(&mut self, p: &Pdu, bytes: &[u8]) -> io::Result<()> {
        let expected = p.get32(20) as usize;
        let actual = bytes.len();
        let bytes = &bytes[..bytes.len().min(expected)];
        if bytes.is_empty() {
            return self.status(p, None, actual);
        }
        let segment = self.negotiation.peer_segment.min(MAX_SEGMENT);
        let mut offset = 0;
        let mut data_sn = 0;
        for burst in bytes.chunks(self.negotiation.max_burst) {
            let mut burst_offset = 0;
            for chunk in burst.chunks(segment) {
                let last = offset + chunk.len() == bytes.len();
                let final_sequence = burst_offset + chunk.len() == burst.len();
                let mut reply = self.answer(p, 0x25);
                reply.header[1] = if final_sequence { 0x80 } else { 0 };
                if last {
                    reply.header[1] |= 1;
                }
                reply.put32(20, u32::MAX);
                reply.put32(36, data_sn);
                reply.put32(40, offset as u32);
                reply.data = chunk.to_vec();
                self.numbers(&mut reply, last);
                if last {
                    residual(&mut reply, expected, actual);
                } else {
                    reply.put32(24, 0);
                }
                reply.write(&mut self.stream)?;
                offset += chunk.len();
                burst_offset += chunk.len();
                data_sn += 1;
            }
        }
        Ok(())
    }
    fn write_data(&mut self, p: &Pdu, len: usize) -> io::Result<Vec<u8>> {
        let n = &self.negotiation;
        if p.data.len() > len || p.data.len() > n.first_burst || (!n.immediate_data && !p.data.is_empty()) {
            return Err(invalid("invalid immediate write data"));
        }
        let mut bytes = p.data.clone();
        if !n.initial_r2t && p.header[1] & 0x80 == 0 && bytes.len() < len {
            let end = n.first_burst.min(len);
            self.data_out(p, u32::MAX, &mut bytes, end)?;
        }
        let mut sequence = 0;
        while bytes.len() < len {
            let end = (bytes.len() + self.negotiation.max_burst).min(len);
            let tag = sequence;
            let mut reply = self.answer(p, 0x31);
            reply.put32(20, tag);
            self.numbers(&mut reply, false);
            reply.put32(36, sequence);
            reply.put32(40, bytes.len() as u32);
            reply.put32(44, (end - bytes.len()) as u32);
            reply.write(&mut self.stream)?;
            self.data_out(p, tag, &mut bytes, end)?;
            if bytes.len() != end {
                return Err(invalid("short solicited write burst"));
            }
            sequence += 1;
        }
        Ok(bytes)
    }
    fn data_out(&mut self, command: &Pdu, tag: u32, bytes: &mut Vec<u8>, end: usize) -> io::Result<()> {
        let mut sequence = 0;
        loop {
            let p = Pdu::read(&mut self.stream)?;
            if !self.accept_command(&p)? {
                continue;
            }
            if p.opcode() == 0 {
                self.nop(&p)?;
                continue;
            }
            if p.opcode() == 2 {
                self.task(&p)?;
                continue;
            }
            if matches!(p.opcode(), 1 | 4 | 6) {
                if self.pending.len() >= 31 {
                    return Err(invalid("command queue exceeds window"));
                }
                self.pending.push_back(p);
                continue;
            }
            if p.opcode() != 5
                || p.get32(16) != command.get32(16)
                || p.get32(20) != tag
                || p.get32(36) != sequence
                || p.get32(40) as usize != bytes.len()
                || p.header[8..16] != command.header[8..16]
                || p.data.is_empty()
                || bytes.len() + p.data.len() > end
            {
                return Err(invalid("invalid Data-Out sequence"));
            }
            bytes.extend(p.data);
            sequence += 1;
            if p.header[1] & 0x80 != 0 {
                return Ok(());
            }
            if bytes.len() == end {
                return Err(invalid("missing final flag on Data-Out burst"));
            }
        }
    }
}

fn residual(p: &mut Pdu, expected: usize, actual: usize) {
    if actual < expected {
        p.header[1] |= 2;
        p.put32(44, (expected - actual) as u32);
    }
    if actual > expected {
        p.header[1] |= 4;
        p.put32(44, (actual - expected) as u32);
    }
}
