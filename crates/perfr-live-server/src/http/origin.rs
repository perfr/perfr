// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Who may call the API: the check in front of every route, by the
//! request's `Origin` and `Host`, and by what the browser says of the page
//! that sent it (`Sec-Fetch-Site`).

use std::sync::Arc;

use axum::extract::State;
use axum::http::header::HeaderName;
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use super::peer::{Connection, Owners};
use crate::LiveService;

/// Answers 403 to a request that `request_allowed` turns down, before any
/// handler or body extractor sees it.
pub(super) async fn origin_middleware(
    State(svc): State<Arc<LiveService>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Decided in a block of its own: the borrows of `req` must end before
    // the request moves on, or the future is not `Send`.
    let allowed = {
        // A header that is not text is not a name we know: it reads as
        // present and empty, which matches nothing.
        let text = |name: header::HeaderName| req.headers().get(name).map(|v| v.to_str().unwrap_or_default());
        let origin = text(header::ORIGIN);
        // HTTP/2 carries the host in the request target, not in a header.
        let host = text(header::HOST).or_else(|| req.uri().authority().map(|a| a.as_str()));
        let (bind, any_local_user) = {
            let config = svc.config.lock();
            (config.bind, config.any_local_user)
        };
        // Who made the connection, before anything the request says: an
        // account of this machine that is neither the service's, nor the
        // one that started it, nor root, has no business here. Absent only
        // when the router is driven without a listener.
        let caller = req.extensions().get::<axum::extract::ConnectInfo<Connection>>().map(|info| info.0.caller);
        if caller.is_some_and(|caller| !Owners::of_this_process().allow(caller, any_local_user)) {
            return (
                StatusCode::FORBIDDEN,
                "perfr answers the user who started it. Another account on this machine may call it \
                 only when it was started with --any-local-user",
            )
                .into_response();
        }
        let fetch = FetchMetadata {
            site: text(HeaderName::from_static("sec-fetch-site")),
            mode: text(HeaderName::from_static("sec-fetch-mode")),
            dest: text(HeaderName::from_static("sec-fetch-dest")),
        };
        request_allowed(origin, host, bind, &svc.machine_name()) && fetch.allows(req.method(), req.uri().path())
    };
    if !allowed {
        return (
            StatusCode::FORBIDDEN,
            "perfr answers its own viewer and clients that are not web pages, addressed by IP address, as localhost or by this machine's own host name",
        )
            .into_response();
    }
    next.run(req).await
}

/// Whether a request may reach the API, by its `Origin` and `Host`. Nothing
/// behind this is authenticated and the service often runs as root, so the
/// two things a browser lets a hostile page do have to be refused here:
///
/// * A page on another site can send requests to this port. Reads and JSON
///   writes need the server's consent (CORS), which is no longer given, but
///   a POST with no body and a WebSocket handshake need none. Every such
///   request names the page in `Origin`; it is let through only when that
///   page is this service's own viewer (`share_origin_allowed`).
/// * A page whose DNS name is re-pointed at this machine after it loaded
///   (rebinding) is same-origin to the browser, and its GETs carry no
///   `Origin` at all. Its `Host` is still the attacker's name, so every
///   request must be addressed to a name the attacker's DNS has no say in:
///   an IP literal, `localhost`, the bound address, or this machine's own
///   name (`is_machine_name`). The last is what a peer told
///   `--connect <machine>` dials and what someone on the LAN types to open
///   the viewer; a rebinding page cannot carry it, because it would have
///   had to be served under that name in the first place.
///
///   That last step holds for public DNS, not for the network the machine
///   sits on: mDNS answers `<machine>.local` for whoever on the link speaks
///   first, and a DHCP-supplied resolver answers the bare name. Someone on
///   the same network can serve a page under this machine's name and then
///   point the name at 127.0.0.1. So the name counts only on a service
///   that listens beyond loopback, which that same someone can call
///   without any of this; a service bound to loopback is never reached by
///   its machine's name in the first place.
///
/// A request with neither header is HTTP/1.0 from something that is not a
/// browser, and is let through.
pub(crate) fn request_allowed(origin: Option<&str>, host: Option<&str>, bind: std::net::SocketAddr, machine: &str) -> bool {
    let reachable = |host: &str| {
        let name = host_name(host);
        host_needs_no_dns(name, bind) || (!bind.ip().is_loopback() && is_machine_name(name, machine))
    };
    match (origin, host) {
        (Some(origin), Some(host)) => origin_is_host(origin, host) && reachable(host),
        (Some(_), None) => false,
        (None, Some(host)) => reachable(host),
        (None, None) => true,
    }
}

/// What a browser says about the page behind a request (Fetch Metadata):
/// headers a page cannot set or remove, sent with every request to an
/// origin the browser trusts to keep them, which `http://127.0.0.1` and
/// `http://localhost` are and a LAN address over plain HTTP is not.
///
/// `Origin` and `Host` leave one kind of request from another site alone:
/// a GET the page does not read. `<img src="http://127.0.0.1:44766/api/..">`
/// and an `<iframe>` of the viewer carry no `Origin`, and are addressed to
/// this machine by address. The page learns nothing from the first (the
/// answer is refused to it), but the service did the work, as often as the
/// page asked; and the second puts the working viewer, Record button and
/// all, under whatever the page draws over it. `Sec-Fetch-Site` names both
/// for what they are.
#[derive(Clone, Copy, Default)]
pub(crate) struct FetchMetadata<'a> {
    pub site: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub dest: Option<&'a str>,
}

impl FetchMetadata<'_> {
    /// Whether the request may go on. One from this service's own pages
    /// (`same-origin`), one the user made (`none`: the address bar, a
    /// bookmark, a link in another program) and one that says nothing (not
    /// a browser, or a page opened by LAN address) may. From any other
    /// page, another port of this machine included, only following a link
    /// to the viewer may: a top-level navigation, never a frame, and never
    /// to the API.
    pub(crate) fn allows(self, method: &Method, path: &str) -> bool {
        match self.site {
            None | Some("same-origin") | Some("none") => true,
            Some(_) => {
                method == Method::GET
                    && self.mode == Some("navigate")
                    && self.dest == Some("document")
                    && !(path == "/ws" || path == "/api" || path.starts_with("/api/"))
            }
        }
    }
}

/// Whether `name` is the host name this service took from the OS at start,
/// bare or as the `.local` name mDNS answers for it. Public DNS serves
/// neither for anyone else: the first has no dot to delegate, the second is
/// reserved for the local link.
pub(super) fn is_machine_name(name: &str, machine: &str) -> bool {
    if machine.is_empty() {
        return false;
    }
    let bare = match name.len().checked_sub(".local".len()) {
        Some(cut) if name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(".local") => &name[..cut],
        _ => name,
    };
    bare.eq_ignore_ascii_case(machine)
}

/// Whether a browser `Origin` may trigger an upload. The page must be this
/// service's own viewer: same scheme-less origin as the request's `Host`,
/// and that host one a page cannot reach by DNS rebinding -- an IP literal,
/// `localhost`, or the address the service was told to bind. A rebinding
/// attack needs a name the attacker controls, which resolves to us only
/// after the page loaded; such a name matches none of these.
pub(super) fn share_origin_allowed(origin: &str, host: &str, bind: std::net::SocketAddr) -> bool {
    origin_is_host(origin, host) && host_needs_no_dns(host_name(host), bind)
}

/// Whether the page named by `origin` was served by the very `host` (name
/// and port) this request is addressed to.
pub(super) fn origin_is_host(origin: &str, host: &str) -> bool {
    let Some(origin_host) = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) else {
        return false;
    };
    !origin_host.is_empty() && origin_host == host
}

/// The name in a `Host` header, without its port.
pub(super) fn host_name(host: &str) -> &str {
    // `[::1]:8080` is bracketed; `1.2.3.4:8080` and `localhost:8080` are not.
    match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => match host.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => host,
        },
    }
}

/// Whether `name` reaches this service whatever DNS says.
pub(super) fn host_needs_no_dns(name: &str, bind: std::net::SocketAddr) -> bool {
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<std::net::IpAddr>().is_ok()
        || name.eq_ignore_ascii_case(&bind.ip().to_string())
}
