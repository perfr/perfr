// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Whose connection this is, when it comes from this machine.
//!
//! Nothing behind the port is authenticated, and the port is TCP: on a
//! machine with more than one account, binding loopback keeps the network
//! out and nobody else. Every local user can connect, and the service
//! then reads files, opens captures and starts captures of processes for
//! them with its own rights, which are often root's.
//!
//! A TCP connection made on this machine has both of its sockets here, and
//! the kernel lists every socket with the user that created it
//! (`/proc/net/tcp`, the table `ss` and an ident daemon read). So the
//! service can tell who is calling without the caller presenting anything:
//! the other end of an accepted connection is the one socket whose local
//! address is this connection's remote address and the other way round.
//! A connection from another machine has no such socket, which is how the
//! two are told apart.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Who is at the other end of an accepted connection.
// Only Linux can tell: elsewhere every caller is `Untold`, and the other
// three are made by the tests alone.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caller {
    /// A process on this machine, of this user.
    Local { uid: u32, established: bool },
    /// No socket on this machine is the other end: another machine.
    Remote,
    /// From a loopback address, and yet no socket here is its other end:
    /// it closed before it could be looked at, or is in a place the table
    /// does not show.
    LocalUnknown,
    /// This platform does not say.
    Untold,
}

/// The connection a request came in on, as `axum::serve` hands it to the
/// origin middleware.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Connection {
    pub caller: Caller,
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_>> for Connection {
    fn connect_info(target: axum::serve::IncomingStream<'_>) -> Self {
        // Looked up as the connection is accepted, once for every request
        // it then carries: the caller's socket is certain to exist only
        // while the connection is new.
        let caller = match target.local_addr() {
            Ok(local) => caller_of(target.remote_addr(), local),
            Err(_) => Caller::Untold,
        };
        Connection { caller }
    }
}

/// Which local users may call: the one the service runs as, the one who
/// started it through `sudo`, and root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owners {
    pub service_uid: u32,
    pub invoker_uid: Option<u32>,
}

impl Owners {
    #[cfg(unix)]
    pub(crate) fn of_this_process() -> Owners {
        // SAFETY: no arguments, cannot fail.
        let service_uid = unsafe { libc::geteuid() };
        let invoker_uid = (service_uid == 0)
            .then(|| std::env::var("SUDO_UID").ok().and_then(|v| v.parse::<u32>().ok()))
            .flatten();
        Owners { service_uid, invoker_uid }
    }

    #[cfg(not(unix))]
    pub(crate) fn of_this_process() -> Owners {
        Owners { service_uid: 0, invoker_uid: None }
    }

    /// Whether `caller` may use the API. `anyone` is the operator saying
    /// that every account on this machine may (`--any-local-user`).
    pub(crate) fn allow(self, caller: Caller, anyone: bool) -> bool {
        match caller {
            // Reachable from the network only when the service was told
            // to listen there, which is the operator's decision.
            Caller::Remote | Caller::Untold => true,
            _ if anyone => true,
            // Root may, as it may everything else. But a socket its process
            // has closed is listed as root's too, whoever made it: sending
            // a request and hanging up at once would be a way to be taken
            // for root. Such a socket has left the established state.
            Caller::Local { uid: 0, established } => established,
            Caller::Local { uid, .. } => uid == self.service_uid || Some(uid) == self.invoker_uid,
            Caller::LocalUnknown => false,
        }
    }
}

/// The caller of a connection accepted on `local` from `remote`.
#[cfg(target_os = "linux")]
pub(crate) fn caller_of(remote: SocketAddr, local: SocketAddr) -> Caller {
    let tables = ["/proc/net/tcp", "/proc/net/tcp6"].map(|path| std::fs::read_to_string(path).ok());
    if tables.iter().all(Option::is_none) {
        return Caller::Untold;
    }
    let found = tables.iter().flatten().find_map(|table| socket_owner(table, remote, local));
    match found {
        Some((uid, established)) => Caller::Local { uid, established },
        None if remote.ip().to_canonical().is_loopback() => Caller::LocalUnknown,
        None => Caller::Remote,
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn caller_of(_remote: SocketAddr, _local: SocketAddr) -> Caller {
    Caller::Untold
}

/// The user and state of the socket in a `/proc/net/tcp` or `tcp6` table
/// whose local end is `from` and whose remote end is `to`: `(uid, whether
/// it is established)`.
///
/// ```text
///   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
///    0: 0100007F:AE9E 0100007F:AECE 01 00000000:00000000 00:00000000 00000000  1000        0 91
/// ```
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn socket_owner(table: &str, from: SocketAddr, to: SocketAddr) -> Option<(u32, bool)> {
    let same = |a: SocketAddr, b: SocketAddr| a.port() == b.port() && a.ip().to_canonical() == b.ip().to_canonical();
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let local = table_addr(fields.nth(1)?)?;
        let remote = table_addr(fields.next()?)?;
        if !same(local, from) || !same(remote, to) {
            return None;
        }
        // TCP_ESTABLISHED is 1.
        let established = u8::from_str_radix(fields.next()?, 16).ok()? == 1;
        let uid = fields.nth(3)?.parse::<u32>().ok()?;
        Some((uid, established))
    })
}

/// An address as the table prints it: the bytes of the address in memory
/// order, read as 32-bit words and printed in hex, then the port in hex.
fn table_addr(text: &str) -> Option<SocketAddr> {
    let (addr, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| -> Option<[u8; 4]> { Some(u32::from_str_radix(addr.get(i * 8..i * 8 + 8)?, 16).ok()?.to_ne_bytes()) };
    let ip = match addr.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(0)?)),
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNERS: Owners = Owners { service_uid: 0, invoker_uid: Some(1000) };

    #[test]
    fn the_user_who_started_the_service_and_root_may_call_and_no_other_account() {
        let local = |uid| Caller::Local { uid, established: true };
        assert!(OWNERS.allow(local(1000), false), "the user who ran sudo");
        assert!(OWNERS.allow(local(0), false), "root");
        assert!(!OWNERS.allow(local(1001), false), "another account");
        assert!(!OWNERS.allow(local(65534), false), "nobody");
        // Run without sudo: the service's own user, and still root.
        let plain = Owners { service_uid: 1000, invoker_uid: None };
        assert!(plain.allow(local(1000), false));
        assert!(plain.allow(local(0), false));
        assert!(!plain.allow(local(1001), false));
        // Another machine is the bind address's business, not this rule's.
        assert!(OWNERS.allow(Caller::Remote, false));
        assert!(OWNERS.allow(Caller::Untold, false));
        // The operator said every account may.
        assert!(OWNERS.allow(local(1001), true));
        assert!(OWNERS.allow(Caller::LocalUnknown, true));
    }

    #[test]
    fn a_caller_that_hung_up_is_not_taken_for_root() {
        // A closed socket is listed with uid 0 until the connection is
        // torn down, and is in a closing state by then.
        assert!(!OWNERS.allow(Caller::Local { uid: 0, established: false }, false));
        // One that cannot be found at all came from loopback and is gone.
        assert!(!OWNERS.allow(Caller::LocalUnknown, false));
        // The owner's own half-closed socket is still the owner's.
        assert!(OWNERS.allow(Caller::Local { uid: 1000, established: false }, false));
    }

    #[test]
    fn the_socket_table_is_read_by_both_ends_of_the_connection() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
            \x20  0: 0100007F:AECE 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 90 1 0 100 0 0 10 0\n\
            \x20  1: 0100007F:AE9E 0100007F:AECE 01 00000000:00000000 00:00000000 00000000  1000        0 91 1 0 20 4 30 10 -1\n\
            \x20  2: 0100007F:AECE 0100007F:AE9E 01 00000000:00000000 00:00000000 00000000     0        0 92 1 0 20 4 30 10 -1\n\
            \x20  3: 0100007F:AEA0 0100007F:AECE 04 00000000:00000000 00:00000000 00000000     0        0 0 1 0 20 4 30 10 -1\n";
        let service: SocketAddr = "127.0.0.1:44750".parse().unwrap();
        let client: SocketAddr = "127.0.0.1:44702".parse().unwrap();
        if cfg!(target_endian = "little") {
            // The client's socket, not the service's accepted one (uid 0 here).
            assert_eq!(socket_owner(table, client, service), Some((1000, true)));
            assert_eq!(socket_owner(table, service, client), Some((0, true)));
            // A client that closed: uid 0, and FIN_WAIT1.
            assert_eq!(socket_owner(table, "127.0.0.1:44704".parse().unwrap(), service), Some((0, false)));
            assert_eq!(socket_owner(table, "127.0.0.1:1".parse().unwrap(), service), None);
            // The service's address as an IPv6 listener reports an IPv4 client.
            let mapped: SocketAddr = "[::ffff:127.0.0.1]:44702".parse().unwrap();
            assert_eq!(socket_owner(table, mapped, service), Some((1000, true)));
        }
        let six = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
            \x20  0: 00000000000000000000000001000000:AE9E 00000000000000000000000001000000:AECE 01 00000000:00000000 00:00000000 00000000  1000        0 93 1 0 20 4 30 10 -1\n";
        if cfg!(target_endian = "little") {
            assert_eq!(socket_owner(six, "[::1]:44702".parse().unwrap(), "[::1]:44750".parse().unwrap()), Some((1000, true)));
        }
        assert_eq!(socket_owner("garbage\nmore garbage: x y\n", client, service), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_connection_made_by_this_process_is_found_as_this_users() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, remote) = listener.accept().unwrap();
        // SAFETY: no arguments, cannot fail.
        let me = unsafe { libc::geteuid() };
        assert_eq!(caller_of(remote, accepted.local_addr().unwrap()), Caller::Local { uid: me, established: true });
        // An address no socket here has is another machine's.
        assert_eq!(caller_of("192.0.2.7:40000".parse().unwrap(), accepted.local_addr().unwrap()), Caller::Remote);
        // From loopback with no socket to show for it: not let in.
        assert_eq!(caller_of("127.0.0.1:9".parse().unwrap(), accepted.local_addr().unwrap()), Caller::LocalUnknown);
        drop(client);
    }
}
