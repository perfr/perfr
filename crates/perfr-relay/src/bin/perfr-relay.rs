// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `perfr-relay [--bind ADDR] [--idle-secs N] [--ring-slots N]`
//!
//! The hard cap is 15 minutes and is not a flag. Idle defaults to 60 seconds.
//! Frames show up on `/ws` once something publishes them (the capture
//! forwarder, or the synthetic source).

use std::net::SocketAddr;
use std::time::Duration;

use perfr_relay::{serve, Relay, RelayConfig, DEFAULT_CONTROL_PER_SEC, DEFAULT_IDLE};

fn main() {
    let mut bind: SocketAddr = "0.0.0.0:44780".parse().unwrap();
    let mut idle = DEFAULT_IDLE;
    let mut ring_slots: usize = 4096;
    let mut coalesce_lag: u64 = 4;
    let mut control_per_sec = DEFAULT_CONTROL_PER_SEC;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next().unwrap_or_else(|| {
                eprintln!("missing value after {arg}");
                std::process::exit(2);
            })
        };
        match arg.as_str() {
            "--bind" => {
                bind = value().parse().unwrap_or_else(|err| {
                    eprintln!("--bind: {err}");
                    std::process::exit(2);
                });
            }
            "--idle-secs" => {
                let secs: u64 = value().parse().unwrap_or_else(|_| {
                    eprintln!("--idle-secs wants a number of seconds");
                    std::process::exit(2);
                });
                idle = Duration::from_secs(secs.max(1));
            }
            "--ring-slots" => {
                ring_slots = value().parse().unwrap_or_else(|_| {
                    eprintln!("--ring-slots wants an integer");
                    std::process::exit(2);
                });
            }
            "--coalesce-lag" => {
                coalesce_lag = value().parse().unwrap_or_else(|_| {
                    eprintln!("--coalesce-lag wants an integer");
                    std::process::exit(2);
                });
            }
            "--control-per-sec" => {
                control_per_sec = value().parse().unwrap_or_else(|_| {
                    eprintln!("--control-per-sec wants an integer");
                    std::process::exit(2);
                });
            }
            "--help" | "-h" => {
                eprintln!(
                    "perfr-relay [--bind ADDR] [--idle-secs N] [--ring-slots N] [--coalesce-lag N] [--control-per-sec N]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    let relay = Relay::new(RelayConfig {
        ring_slots,
        coalesce_lag,
        idle,
        hard_cap: perfr_relay::HARD_CAP,
        control_per_sec,
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        serve(bind, relay).await.unwrap_or_else(|err| {
            eprintln!("perfr-relay: {err}");
            std::process::exit(1);
        });
    });
}
