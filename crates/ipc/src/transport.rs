use std::{
    io::{self, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::*,
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::{Pipes::*, Threading::*, IO::*},
    },
};

pub(crate) fn winerr(error: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32)
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub(crate) struct Handle(pub HANDLE);
// Owned kernel handles support concurrent overlapped I/O; each operation owns its event and buffers.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
pub(crate) struct Pipe {
    pub handle: Handle,
    pub stopped: AtomicBool,
}
impl Pipe {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        unsafe {
            let _ = CancelIoEx(self.handle.0, None);
        }
    }
    fn finish(
        &self,
        op: &mut OVERLAPPED,
        initial: windows::core::Result<()>,
        deadline: Option<Instant>,
    ) -> io::Result<usize> {
        if let Err(error) = initial {
            if error.code() != windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) {
                return Err(winerr(error));
            }
        }
        loop {
            let state = unsafe { WaitForSingleObject(op.hEvent, 100) };
            if state == WAIT_OBJECT_0 {
                break;
            }
            if state == WAIT_FAILED
                || self.stopped.load(Ordering::Acquire)
                || deadline.is_some_and(|d| Instant::now() >= d)
            {
                unsafe {
                    let _ = CancelIoEx(self.handle.0, Some(op));
                    let mut transferred = 0;
                    // Cancellation is asynchronous: drain before releasing OVERLAPPED/buffer/event.
                    let _ = GetOverlappedResult(self.handle.0, op, &mut transferred, true);
                }
                return Err(io::Error::new(io::ErrorKind::TimedOut, "Pipe I/O stopped or timed out"));
            }
        }
        let mut transferred = 0;
        unsafe {
            GetOverlappedResult(self.handle.0, op, &mut transferred, false).map_err(winerr)?;
        }
        Ok(transferred as usize)
    }
    fn operation(&self) -> io::Result<(Handle, OVERLAPPED)> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "Connection closed"));
        }
        let event = Handle(unsafe { CreateEventW(None, true, false, PCWSTR::null()).map_err(winerr)? });
        let op = OVERLAPPED { hEvent: event.0, ..Default::default() };
        Ok((event, op))
    }
    pub fn accept(&self) -> io::Result<()> {
        let (_event, mut op) = self.operation()?;
        let result = unsafe { ConnectNamedPipe(self.handle.0, Some(&mut op)) };
        if result.as_ref().is_err_and(|e| e.code() == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0)) {
            return Ok(());
        }
        self.finish(&mut op, result, None).map(|_| ())
    }
    fn read_bytes(&self, buf: &mut [u8]) -> io::Result<usize> {
        let (_event, mut op) = self.operation()?;
        let result = unsafe { ReadFile(self.handle.0, Some(buf), None, Some(&mut op)) };
        self.finish(&mut op, result, None)
    }
    fn write_bytes(&self, buf: &[u8]) -> io::Result<usize> {
        let (_event, mut op) = self.operation()?;
        let result = unsafe { WriteFile(self.handle.0, Some(buf), None, Some(&mut op)) };
        self.finish(&mut op, result, Some(Instant::now() + Duration::from_secs(30)))
    }
}
#[derive(Clone)]
pub(crate) struct PipeStream(pub Arc<Pipe>);
impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_bytes(buf)
    }
}
impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_bytes(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(crate) fn create(name: &str, sddl: &str, first: bool) -> io::Result<Arc<Pipe>> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let sddl = wide(sddl);
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(winerr)?;
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let name = wide(name);
    let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            5000,
            Some(&attrs),
        )
    };
    let error = if handle == INVALID_HANDLE_VALUE { Some(io::Error::last_os_error()) } else { None };
    unsafe {
        LocalFree(HLOCAL(descriptor.0));
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(Arc::new(Pipe { handle: Handle(handle), stopped: AtomicBool::new(false) }))
}
pub(crate) fn connect(name: &str, timeout: Duration) -> io::Result<Arc<Pipe>> {
    let name = wide(name);
    let deadline = Instant::now() + timeout;
    loop {
        let result = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IMPERSONATION,
                None,
            )
        };
        match result {
            Ok(handle) => return Ok(Arc::new(Pipe { handle: Handle(handle), stopped: AtomicBool::new(false) })),
            Err(error) => {
                let error = winerr(error);
                if !matches!(error.raw_os_error(), Some(2 | 231)) {
                    return Err(error);
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "Service pipe unavailable"));
                }
                unsafe {
                    let _ = WaitNamedPipeW(PCWSTR(name.as_ptr()), 50);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
pub(crate) fn exists(name: &str) -> bool {
    let name = wide(name);
    let result = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1) };
    result.as_bool() || matches!(unsafe { GetLastError() }, ERROR_SEM_TIMEOUT | ERROR_PIPE_BUSY | ERROR_ACCESS_DENIED)
}
pub(crate) fn client_info(pipe: &Pipe, require_admin: bool) -> io::Result<crate::server::ClientInfo> {
    let mut process_id = 0;
    unsafe {
        GetNamedPipeClientProcessId(pipe.handle.0, &mut process_id).map_err(winerr)?;
    }
    unsafe {
        ImpersonateNamedPipeClient(pipe.handle.0).map_err(winerr)?;
    }
    let check: io::Result<bool> = (|| {
        let mut storage = [0u32; 17];
        let mut size = std::mem::size_of_val(&storage) as u32;
        let sid = PSID(storage.as_mut_ptr().cast());
        unsafe {
            CreateWellKnownSid(WinBuiltinAdministratorsSid, PSID::default(), sid, &mut size).map_err(winerr)?;
        }
        let mut member = BOOL::default();
        unsafe {
            CheckTokenMembership(HANDLE::default(), sid, &mut member).map_err(winerr)?;
        }
        Ok(member.as_bool())
    })();
    // Never execute a handler while impersonating, even if membership lookup fails.
    if unsafe { RevertToSelf() }.is_err() {
        // Continuing under a client's token would compromise LocalSystem isolation.
        std::process::abort();
    }
    let is_admin = check?;
    if require_admin && !is_admin {
        return Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.0 as i32));
    }
    Ok(crate::server::ClientInfo { process_id, is_admin })
}
