// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Opens N WebSocket clients against a running `perfr-relay` and prints
//! fan-out throughput, latency percentiles, and the relay's CPU and RSS.
//!
//! ```text
//! perfr-relay-load --spawn --clients 1,8,32,64 --duration-secs 3 --rate 20000
//! ```
//!
//! `--spawn` starts `perfr-relay --synthetic` beside this binary and samples
//! that process. Latency is the gap from the batch's `start_ns` (unix time,
//! stamped when the synthetic source built the batch) to when this client
//! finished reading the WebSocket frame. It is same-machine loopback, not a
//! browser on a gigabit link.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;

const FRAME_EVENT_BATCH: u8 = 2;
const RESERVOIR: usize = 4096;

struct Stats {
    events: u64,
    bytes: u64,
    /// Steady-state latencies in microseconds. A ring, so the warmup falls out.
    samples: Vec<u64>,
    next: usize,
}

impl Stats {
    fn new() -> Self {
        Self {
            events: 0,
            bytes: 0,
            samples: Vec::with_capacity(RESERVOIR),
            next: 0,
        }
    }

    fn clear(&mut self) {
        self.events = 0;
        self.bytes = 0;
        self.samples.clear();
        self.next = 0;
    }

    fn record(&mut self, events: u64, bytes: usize, latency_us: u64) {
        self.events += events;
        self.bytes += bytes as u64;
        if self.samples.len() < RESERVOIR {
            self.samples.push(latency_us);
        } else {
            self.samples[self.next % RESERVOIR] = latency_us;
            self.next += 1;
        }
    }
}

fn main() {
    let mut clients = vec![1usize, 8, 32, 64];
    let mut duration = Duration::from_secs(3);
    let mut rate = 20_000u64;
    let mut url: Option<String> = None;
    let mut relay_pid: Option<u32> = None;
    let mut spawn = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next().unwrap_or_else(|| {
                eprintln!("missing value after {arg}");
                std::process::exit(2);
            })
        };
        match arg.as_str() {
            "--clients" => {
                clients = value()
                    .split(',')
                    .map(|s| s.parse().expect("client count"))
                    .collect();
            }
            "--duration-secs" => {
                duration = Duration::from_secs(value().parse().expect("seconds"));
            }
            "--rate" => rate = value().parse().expect("rate"),
            "--url" => url = Some(value()),
            "--relay-pid" => relay_pid = Some(value().parse().expect("pid")),
            "--spawn" => spawn = true,
            "--help" | "-h" => {
                eprintln!(
                    "perfr-relay-load [--spawn] [--url ws://HOST/ws] [--relay-pid PID] \
                     [--clients 1,8,32] [--duration-secs N] [--rate N]"
                );
                return;
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    let mut child = None;
    let url = if let Some(url) = url {
        url
    } else {
        spawn = true;
        "ws://127.0.0.1:44790/ws".to_string()
    };
    if spawn {
        child = Some(spawn_relay(&url, rate));
        relay_pid = child.as_ref().map(|c| c.id());
    }
    let pid = relay_pid;
    let http = http_base(&url);
    wait_until_up(&http);

    println!(
        "relay pid: {}",
        pid.map(|p| p.to_string())
            .unwrap_or_else(|| "unknown".into())
    );
    println!("url: {url}");
    println!("synthetic rate requested: {rate} events/s");
    println!("clients\tevents/s\tMiB/s\tp50_us\tp95_us\tp99_us\trelay_cpu_%\trss_mib\tstatus_eps");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .expect("runtime");
    for n in clients {
        let row = runtime.block_on(measure(&url, &http, n, duration, pid));
        println!(
            "{}\t{:.0}\t{:.2}\t{}\t{}\t{}\t{:.0}\t{:.1}\t{}",
            row.clients,
            row.events_per_s,
            row.mib_per_s,
            row.p50_us,
            row.p95_us,
            row.p99_us,
            row.cpu_pct,
            row.rss_mib,
            row.status_eps,
        );
        if row.p99_us > 50_000 && n > 1 {
            println!("stopping: p99 exceeded 50 ms at {n} clients");
            break;
        }
    }
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct Row {
    clients: usize,
    events_per_s: f64,
    mib_per_s: f64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    cpu_pct: f64,
    rss_mib: f64,
    status_eps: u64,
}

async fn measure(url: &str, http: &str, n: usize, duration: Duration, pid: Option<u32>) -> Row {
    let stop = Arc::new(AtomicBool::new(false));
    let mut stats = Vec::with_capacity(n);
    let mut tasks = Vec::with_capacity(n);
    for _ in 0..n {
        let stats_i = Arc::new(Mutex::new(Stats::new()));
        stats.push(Arc::clone(&stats_i));
        let url = url.to_string();
        let stop = Arc::clone(&stop);
        tasks.push(tokio::spawn(
            async move { client(url, stop, stats_i).await },
        ));
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    for s in &stats {
        s.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    let ticks0 = pid.and_then(cpu_ticks);
    let started = Instant::now();
    tokio::time::sleep(duration).await;
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let ticks1 = pid.and_then(cpu_ticks);
    let rss = pid.and_then(rss_kb).unwrap_or(0);
    let status_eps = fetch_eps(http).await;
    stop.store(true, Ordering::Relaxed);
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }

    let mut events = 0u64;
    let mut bytes = 0u64;
    let mut samples = Vec::new();
    for s in &stats {
        let g = s.lock().unwrap_or_else(|e| e.into_inner());
        events += g.events;
        bytes += g.bytes;
        samples.extend_from_slice(&g.samples);
    }
    samples.sort_unstable();
    let cpu_pct = match (ticks0, ticks1) {
        (Some(a), Some(b)) => {
            let ticks = b.saturating_sub(a) as f64;
            ticks / clk_tck() / elapsed * 100.0
        }
        _ => 0.0,
    };
    Row {
        clients: n,
        events_per_s: events as f64 / elapsed,
        mib_per_s: bytes as f64 / elapsed / (1024.0 * 1024.0),
        p50_us: percentile(&samples, 0.50),
        p95_us: percentile(&samples, 0.95),
        p99_us: percentile(&samples, 0.99),
        cpu_pct,
        rss_mib: rss as f64 / 1024.0,
        status_eps,
    }
}

async fn client(url: String, stop: Arc<AtomicBool>, stats: Arc<Mutex<Stats>>) {
    let Ok((mut ws, _)) = tokio_tungstenite::connect_async(&url).await else {
        return;
    };
    while !stop.load(Ordering::Relaxed) {
        let next = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
        let Ok(next) = next else { continue };
        match next {
            Some(Ok(Message::Binary(bytes))) => {
                if let Some((events, start_ns)) = batch_stamp(&bytes) {
                    let now = unix_ns();
                    let us = now.saturating_sub(start_ns) / 1_000;
                    stats
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .record(events, bytes.len(), us);
                } else {
                    let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                    stats.bytes += bytes.len() as u64;
                }
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
            Some(Ok(_)) => {}
        }
    }
}

/// Raw event batch: first event's `start_ns`, and the event count.
fn batch_stamp(frame: &[u8]) -> Option<(u64, u64)> {
    if frame.len() < 9 || frame[4] != FRAME_EVENT_BATCH {
        return None;
    }
    let count = u32::from_le_bytes(frame[5..9].try_into().ok()?) as u64;
    if count == 0 || frame.len() < 9 + 8 {
        return None;
    }
    let start_ns = u64::from_le_bytes(frame[9..17].try_into().ok()?);
    Some((count, start_ns))
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn spawn_relay(ws_url: &str, rate: u64) -> Child {
    let bind = ws_url
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .split('/')
        .next()
        .unwrap_or("127.0.0.1:44790");
    let bin = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("perfr-relay"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| {
            eprintln!("perfr-relay is not next to this binary; pass --url and --relay-pid");
            std::process::exit(1);
        });
    let rate_s = rate.to_string();
    Command::new(bin)
        .args([
            "--bind",
            bind,
            "--synthetic",
            "--rate",
            &rate_s,
            "--idle-secs",
            "600",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn perfr-relay")
}

fn http_base(ws_url: &str) -> String {
    let host = ws_url
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .split('/')
        .next()
        .unwrap_or("127.0.0.1:44790");
    format!("http://{host}")
}

fn wait_until_up(http: &str) {
    for _ in 0..50 {
        if std::process::Command::new("curl")
            .args(["-sf", "--max-time", "1", &format!("{http}/status")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("relay did not answer {http}/status");
    std::process::exit(1);
}

async fn fetch_eps(http: &str) -> u64 {
    let url = format!("{http}/status");
    let mut totals = Vec::new();
    for _ in 0..3 {
        let url = url.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new("curl")
                .args(["-sf", "--max-time", "1", &url])
                .output()
        })
        .await
        .ok()
        .and_then(|r| r.ok())
        .filter(|o| o.status.success());
        if let Some(body) = output {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body.stdout) {
                if let Some(n) = v.get("events_per_sec").and_then(|n| n.as_u64()) {
                    totals.push(n);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if totals.is_empty() {
        0
    } else {
        totals.iter().sum::<u64>() / totals.len() as u64
    }
}

fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

fn rss_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn clk_tck() -> f64 {
    static TICKS: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *TICKS.get_or_init(|| {
        std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(100.0)
    })
}
