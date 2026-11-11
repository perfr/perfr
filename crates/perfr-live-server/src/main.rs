// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Standalone live-viewer service (the HTTP stack `perfr` embeds).
//!
//! Useful when you only want the viewer and the demo and bench producers,
//! or to exercise the ring and the stream with no capture behind them.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use perfr_live_server::{http, LiveService, ServerConfig, DEFAULT_HTTP_PORT, DEFAULT_RING_BYTES};

fn parse_args(args: impl IntoIterator<Item = String>) -> ServerConfig {
    let mut cfg = ServerConfig {
        wire: perfr_live_server::env_wire(),
        ..ServerConfig::default()
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => {
                let host: IpAddr = args
                    .next()
                    .expect("missing host")
                    .parse()
                    .expect("invalid host: an IP address, 0.0.0.0 for every interface");
                cfg.bind.set_ip(host);
            }
            "--http-port" | "--http_port" => {
                let port: u16 = args
                    .next()
                    .expect("missing port")
                    .parse()
                    .expect("invalid port");
                cfg.bind.set_port(port);
            }
            "--ring-buffer-bytes" | "--ring_buffer_bytes" => {
                cfg.ring_buffer_bytes = parse_size(&args.next().expect("missing size"));
            }
            "--spill-path" | "--spill_path" => {
                let p = args.next().expect("missing path");
                if !p.is_empty() {
                    cfg.spill_path = Some(PathBuf::from(p));
                }
            }
            "--wire" => {
                let v = args.next().expect("missing wire format: raw, packed or deflate");
                cfg.wire = perfr_live_server::WireFormat::parse(&v)
                    .unwrap_or_else(|| panic!("unknown wire format {v:?}: use raw, packed or deflate"));
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_help();
                std::process::exit(2);
            }
        }
    }
    cfg
}

fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let (num, mul) = if let Some(rest) = s.strip_suffix(['k', 'K']) {
        (rest, 1024u64)
    } else if let Some(rest) = s.strip_suffix(['m', 'M']) {
        (rest, 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix(['g', 'G']) {
        (rest, 1024 * 1024 * 1024)
    } else {
        (s, 1)
    };
    num.parse::<u64>().expect("invalid size") * mul
}

fn print_help() {
    eprintln!(
        "perfr-live-service — WASM live viewer HTTP server\n\
         \n\
         --host ADDR                   listen address (default 127.0.0.1; 0.0.0.0 opens the\n\
         \x20                             service, which has no authentication, to the network)\n\
         --http-port PORT              listen port (default {DEFAULT_HTTP_PORT})\n\
         --ring-buffer-bytes N[kKmMgG] in-process ring size (default {DEFAULT_RING_BYTES})\n\
         --spill-path DIR              optional overflow serialize directory\n\
         --wire raw|packed|deflate     event batch format on the WebSocket (default packed,\n\
         \x20                             or PERFR_LIVE_WIRE)\n\
         \n\
         Open http://127.0.0.1:<port>/ and click Demo."
    );
}

#[tokio::main]
async fn main() {
    let cfg = parse_args(std::env::args().skip(1));
    eprintln!(
        "perfr live viewer on http://{}/  ring={} bytes  spill={:?}",
        cfg.bind, cfg.ring_buffer_bytes, cfg.spill_path
    );
    let service = LiveService::new(cfg).expect("create live service");
    let svc = Arc::clone(&service);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        crate::demo_stop_and_exit(&svc);
    });
    http::serve(service).await.expect("http serve");
}

fn demo_stop_and_exit(svc: &LiveService) {
    perfr_live_server::demo::stop(svc);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> ServerConfig {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn the_service_listens_on_this_machine_only_unless_told_a_host() {
        assert_eq!(parsed(&[]).bind.to_string(), format!("127.0.0.1:{DEFAULT_HTTP_PORT}"));
        // A port alone does not open it up.
        assert_eq!(parsed(&["--http-port", "9000"]).bind.to_string(), "127.0.0.1:9000");
        assert_eq!(parsed(&["--host", "0.0.0.0", "--http-port", "9000"]).bind.to_string(), "0.0.0.0:9000");
        assert_eq!(parsed(&["--host", "::1"]).bind.ip().to_string(), "::1");
    }
}
