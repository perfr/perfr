// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use futures_util::StreamExt;
use perfr_viewer::net::{Inbox, Queued};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::watch;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("client IO runtime")
    })
}

#[derive(Clone)]
pub struct Http {
    base: reqwest::Url,
    client: reqwest::Client,
}

impl Http {
    pub fn new(origin: &str) -> Result<Self, String> {
        let base = reqwest::Url::parse(origin).map_err(|e| format!("service URL: {e}"))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.username().is_empty()
            || base.password().is_some()
        {
            return Err(
                "service URL must be an HTTP(S) origin, e.g. http://127.0.0.1:44766".into(),
            );
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { base, client })
    }

    pub fn url(&self, path: &str) -> String {
        self.base
            .join(path)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| path.into())
    }

    pub fn spawn<F>(&self, task: impl FnOnce(Self) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        runtime().spawn(task(self.clone()));
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<(&[u8], &str)>,
    ) -> Result<Vec<u8>, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
        let mut request = self.client.request(method, self.url(path));
        if let Some((bytes, content_type)) = body {
            request = request
                .header("content-type", content_type)
                .body(bytes.to_vec());
        }
        let response = request.send().await.map_err(|e| format!("{path}: {e}"))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| format!("{path}: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "{path}: {} {}",
                status.as_u16(),
                String::from_utf8_lossy(&bytes)
            ));
        }
        Ok(bytes.to_vec())
    }

    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>, String> {
        self.request("GET", path, None).await
    }

    pub async fn get_text(&self, path: &str) -> Result<String, String> {
        String::from_utf8(self.get_bytes(path).await?).map_err(|e| e.to_string())
    }

    pub async fn send_text(&self, method: &str, path: &str, body: &str) -> Result<String, String> {
        String::from_utf8(
            self.request(method, path, Some((body.as_bytes(), "application/json")))
                .await?,
        )
        .map_err(|e| e.to_string())
    }

    pub async fn send_bytes(
        &self,
        path: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<String, String> {
        String::from_utf8(
            self.request("POST", path, Some((bytes, content_type)))
                .await?,
        )
        .map_err(|e| e.to_string())
    }
}

pub struct Socket {
    cancel: watch::Sender<bool>,
}

impl Socket {
    pub fn close(&self) -> Result<(), watch::error::SendError<bool>> {
        self.cancel.send(true)
    }
}

/// The occupied slot includes connecting/closing sockets. Only their own task
/// clears it, so reconnect cannot be erased by a late close from an older task.
pub fn start_ws(inbox: Arc<Mutex<Inbox>>, slot: Arc<Mutex<Option<Socket>>>, http: &Http) {
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return;
    }
    let (cancel, mut cancelled) = watch::channel(false);
    *guard = Some(Socket { cancel });
    drop(guard);
    let mut url = http.base.join("/ws").expect("valid service origin");
    url.set_scheme(if http.base.scheme() == "https" {
        "wss"
    } else {
        "ws"
    })
    .unwrap();
    runtime().spawn(async move {
        let result = tokio::select! {
            _ = cancelled.changed() => Ok(()),
            result = async {
                let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5),
                    tokio_tungstenite::connect_async(url.as_str())).await
                    .map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
                inbox.lock().unwrap_or_else(|e| e.into_inner()).ws_ok = true;
                while let Some(message) = ws.next().await {
                    match message.map_err(|e| e.to_string())? {
                        tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
                            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                            g.bytes_in += bytes.len() as u64;
                            // A minimized native window may stop draining as well.
                            if g.queue_frame(bytes, true) == Queued::Overflow {
                                return Err("event stream backlog full; reconnecting".to_string());
                            }
                        }
                        tokio_tungstenite::tungstenite::Message::Close(_) => break,
                        _ => {}
                    }
                }
                Err("WebSocket closed".to_string())
            } => result,
        };
        let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
        g.ws_ok = false;
        if let Err(error) = result {
            g.error = Some(error);
        }
        // Hold inbox until the old slot is cleared; a new connection can never
        // publish ws_ok=true before the old connection publishes false.
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    });
}
