// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Native Frida Core injector and control relay. Events never cross this pipe.
use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
use std::time::{Duration, Instant};

use perfr_frida_transport::profile;
#[cfg(windows)]
mod pipe;

/// Reads one line from stdin for the stop signal. On Unix the shared
/// `std::io::Stdin` reads fine from a second thread. On Windows a second
/// threaded read of the global buffered `Stdin` never delivers a piped
/// line, so the stop reader gets its own duplicated STD_INPUT_HANDLE and
/// reads it directly. The service's EOF (it drops the helper's stdin) and
/// an explicit "stop" line both end this read.
#[cfg(not(windows))]
fn wait_for_stop() {
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
}
/// The process stdin, owned here. Rust's `std::io::stdin` buffer and a second
/// handle on the same pipe do not observe a later write or the service's
/// close. One reader, used for the config line and then the stop line.
#[cfg(windows)]
fn owned_stdin() -> Option<std::fs::File> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    type Handle = *mut std::ffi::c_void;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // (DWORD)-10
    const DUPLICATE_SAME_ACCESS: u32 = 0x2;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetCurrentProcess() -> Handle;
        fn DuplicateHandle(
            sp: Handle,
            s: Handle,
            tp: Handle,
            t: *mut Handle,
            a: u32,
            inherit: i32,
            opts: u32,
        ) -> i32;
    }
    unsafe {
        let stdin = GetStdHandle(STD_INPUT_HANDLE);
        let mut dup: Handle = std::ptr::null_mut();
        if DuplicateHandle(
            GetCurrentProcess(),
            stdin,
            GetCurrentProcess(),
            &mut dup,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return None;
        }
        Some(std::fs::File::from(OwnedHandle::from_raw_handle(dup)))
    }
}

#[cfg(windows)]
fn wait_named_event(name: &str) {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut std::ffi::c_void;
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, millis: u32) -> u32;
    }
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // The service creates the event before it sends the config, so this
    // opens on the first try. Retry rather than treating a miss as "stop",
    // which would detach the capture immediately.
    let handle = loop {
        let handle = unsafe { OpenEventW(SYNCHRONIZE, 0, wide.as_ptr()) };
        if !handle.is_null() && handle != -1isize as *mut std::ffi::c_void {
            break handle;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
    unsafe {
        WaitForSingleObject(owned.as_raw_handle_wait(), 0xFFFF_FFFF);
    }
}

#[cfg(windows)]
trait RawWait {
    fn as_raw_handle_wait(&self) -> *mut std::ffi::c_void;
}
#[cfg(windows)]
impl RawWait for std::os::windows::io::OwnedHandle {
    fn as_raw_handle_wait(&self) -> *mut std::ffi::c_void {
        use std::os::windows::io::AsRawHandle;
        self.as_raw_handle() as *mut std::ffi::c_void
    }
}

#[cfg(windows)]
fn read_handle_line(file: &mut std::fs::File) -> std::io::Result<String> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if file.read(&mut byte)? == 0 {
            break;
        }
        out.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

extern "C" {
    fn perfr_core_inject(
        pid: u32,
        path: *const c_char,
        data: *const c_char,
        error: *mut c_char,
        capacity: usize,
        loader: *const c_void,
        loader_size: usize,
    ) -> *mut c_void;
    fn perfr_core_close(injector: *mut c_void);
}
struct Injector(*mut c_void);
impl Drop for Injector {
    fn drop(&mut self) {
        unsafe {
            perfr_core_close(self.0);
        }
    }
}

#[cfg(unix)]
fn peer_pid(stream: &UnixStream) -> std::io::Result<u32> {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        if libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut _,
            &mut len,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(cred.pid as u32)
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        // LOCAL_PEERPID is available for connected AF_UNIX sockets on Darwin.
        if libc::getsockopt(
            stream.as_raw_fd(),
            0,
            2,
            &mut pid as *mut _ as *mut _,
            &mut len,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(pid as u32)
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    let mut stdin_owner = owned_stdin();
    let mut config = String::new();
    #[cfg(windows)]
    {
        if let Some(file) = stdin_owner.as_mut() {
            config = read_handle_line(file)?;
        } else {
            std::io::stdin().lock().read_line(&mut config)?;
        }
    }
    #[cfg(not(windows))]
    {
        std::io::stdin().lock().read_line(&mut config)?;
    }
    let value: serde_json::Value = serde_json::from_str(&config)?;
    let pid = value["pid"]
        .as_u64()
        .filter(|&p| p > 0 && p <= i32::MAX as u64)
        .ok_or("invalid pid")? as u32;
    let agent = CString::new(value["agent"].as_str().ok_or("missing agent path")?)?;
    #[cfg(unix)]
    let (listener, data, _dir) = {
        let dir = tempfile::Builder::new()
            .prefix("perfr-core-")
            .tempdir_in("/tmp")?;
        // Permit an unprivileged target to traverse, but not list or modify, our
        // private directory. Authenticate the socket peer by kernel-reported PID.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o711))?;
        let path = dir.path().join("control");
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622))?;
        listener.set_nonblocking(true)?;
        let data = CString::new(path.to_str().ok_or("invalid socket path")?)?;
        (listener, data, dir)
    };
    #[cfg(windows)]
    let (listener, data) = {
        let server = pipe::Server::create()?;
        let data = CString::new(server.name())?;
        (server, data)
    };
    let mut error = [0 as c_char; 1024];
    #[cfg(target_os = "macos")]
    let loader: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/loader.dylib"));
    #[cfg(not(target_os = "macos"))]
    let loader: &[u8] = &[];
    let raw = profile::measure(
        &mut std::io::stdout().lock(),
        value["self_profile"].as_bool().unwrap_or(false),
        "Frida: inject native agent",
        || unsafe {
            perfr_core_inject(
                pid,
                agent.as_ptr(),
                data.as_ptr(),
                error.as_mut_ptr(),
                error.len(),
                loader.as_ptr().cast(),
                loader.len(),
            )
        },
    );
    if raw.is_null() {
        return Err(unsafe { CStr::from_ptr(error.as_ptr()) }
            .to_string_lossy()
            .into_owned()
            .into());
    }
    let _injector = Injector(raw);
    let deadline = Instant::now() + Duration::from_secs(20);
    #[cfg(unix)]
    let mut stream = {
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) if peer_pid(&s)? == pid => break s,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if Instant::now() >= deadline {
                return Err("agent connection timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Darwin inherits O_NONBLOCK from the listening socket; Linux does not.
        // Only accept is polled. The connected control stream is blocking on both.
        stream.set_nonblocking(false)?;
        stream
    };
    #[cfg(windows)]
    let mut stream = listener.accept(pid, deadline)?;
    stream.write_all(config.as_bytes())?;
    // A symbols request is one-shot. Sending an EOF-triggered Stop would
    // leave unread bytes when the agent closes, resetting a valid reply.
    if value["command"] != "symbols" {
        let mut stop = stream.try_clone()?;
        #[cfg(windows)]
        let stop_event = value
            .get("stop_event")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        #[cfg(windows)]
        let stop_stdin = stdin_owner.take();
        std::thread::spawn(move || {
            #[cfg(windows)]
            {
                if let Some(name) = stop_event {
                    wait_named_event(&name);
                } else if let Some(mut file) = stop_stdin {
                    let _ = read_handle_line(&mut file);
                }
            }
            #[cfg(not(windows))]
            wait_for_stop();
            // The Windows relay polls, so this write is not stuck behind a
            // synchronous read on the same pipe. That read was the deadlock.
            let _ = stop.write_all(b"stop\n");
            let _ = stop.flush();
            #[cfg(unix)]
            let _ = stop.shutdown(std::net::Shutdown::Write);
        });
    }
    let mut output = std::io::stdout().lock();
    #[cfg(windows)]
    relay_windows(&mut stream, &mut output)?;
    #[cfg(not(windows))]
    {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            writeln!(output, "{}", line)?;
            output.flush()?;
        }
    }
    Ok(())
}
/// No synchronous ReadFile stays outstanding, so the stop thread can write
/// `stop` on another handle of this pipe. A blocking read deadlocks that write.
#[cfg(windows)]
fn relay_windows(stream: &mut std::fs::File, output: &mut impl std::io::Write) -> std::io::Result<()> {
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn PeekNamedPipe(
            pipe: *mut std::ffi::c_void,
            buf: *mut u8,
            buf_size: u32,
            read: *mut u32,
            avail: *mut u32,
            left: *mut u32,
        ) -> i32;
    }
    let mut incoming = String::new();
    let mut byte = [0u8; 1];
    loop {
        let mut avail = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                stream.as_raw_handle() as *mut std::ffi::c_void,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut avail,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            break;
        }
        if avail == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        if stream.read(&mut byte)? == 0 {
            break;
        }
        incoming.push(byte[0] as char);
        if byte[0] == b'\n' {
            writeln!(output, "{}", incoming.trim_end())?;
            output.flush()?;
            incoming.clear();
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        println!("{}", serde_json::json!({"error":error.to_string()}));
        std::process::exit(1);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn control_socket_authenticates_the_connecting_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control");
        let listener = UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        assert_eq!(peer_pid(&server).unwrap(), std::process::id());
    }
}
