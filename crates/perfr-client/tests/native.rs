// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(all(feature = "native", not(target_arch = "wasm32")))]

use perfr_viewer::net::{Backend, Inbox};
use std::time::{Duration, Instant};

async fn until(net: &dyn Backend, mut check: impl FnMut(Inbox) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if check(net.take()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the native backend"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn native_client_controls_and_reconnects_to_a_real_service() {
    let service = perfr_live_server::LiveService::new(perfr_live_server::ServerConfig {
        ring_buffer_bytes: 4 * 1024 * 1024,
        ..Default::default()
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = perfr_live_server::http::router(service.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let host = perfr_client::native_host(&origin).unwrap();
    let net = (host.connect)(None);
    assert!(host.service());
    assert_eq!(
        net.resolve_url("/api/capture/export?format=bundle"),
        format!("{origin}/api/capture/export?format=bundle")
    );
    until(&*net, |inbox| inbox.ws_ok && inbox.bytes_in > 0).await;

    net.get_status();
    until(&*net, |inbox| inbox.http_ok && inbox.status.is_some()).await;
    net.get_processes();
    until(&*net, |inbox| inbox.processes.is_some()).await;

    net.start_demo();
    until(&*net, |inbox| {
        net.get_status();
        inbox
            .status
            .is_some_and(|status| status.demo && status.events_live > 0)
    })
    .await;
    net.stop_demo();
    until(&*net, |inbox| {
        net.get_status();
        inbox.status.is_some_and(|status| !status.demo)
    })
    .await;

    net.fetch_capture("/api/capture/export?format=stream");
    until(&*net, |inbox| {
        inbox.loaded_stream_by_address && inbox.loaded_stream.is_some_and(|bytes| !bytes.is_empty())
    })
    .await;

    net.close_ws();
    until(&*net, |inbox| !inbox.ws_ok).await;
    net.reconnect_ws_if_closed();
    until(&*net, |inbox| inbox.ws_ok && !inbox.frames.is_empty()).await;

    net.clear_capture();
    until(&*net, |inbox| {
        net.get_status();
        inbox.status.is_some_and(|status| status.events_live == 0)
    })
    .await;

    // The standalone service has no bundle importer; its HTTP failure must
    // reach the UI, not leave the import waiting forever.
    net.import_capture(b"PK".to_vec());
    until(&*net, |inbox| {
        inbox
            .error
            .is_some_and(|error| error.contains("open capture:") && error.contains("501"))
    })
    .await;

    drop(net);
    perfr_live_server::demo::stop(&service);
    server.abort();
}

#[tokio::test]
async fn a_missing_service_reports_the_http_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let host = perfr_client::native_host(&origin).unwrap();
    let net = (host.connect)(None);
    net.get_status();
    until(&*net, |inbox| {
        !inbox.http_ok && inbox.error.is_some_and(|e| e.contains("/api/status"))
    })
    .await;
}

#[test]
fn service_addresses_are_origins_not_file_paths_or_other_schemes() {
    for url in [
        "file:///tmp/capture",
        "ftp://localhost",
        "http://localhost/api",
        "http://localhost/?x=1",
        "http://user:password@localhost",
    ] {
        assert!(perfr_client::native_host(url).is_err(), "{url}");
    }
    assert!(perfr_client::native_host("http://127.0.0.1:44766").is_ok());
    assert!(perfr_client::native_host("https://example.com").is_ok());
}
