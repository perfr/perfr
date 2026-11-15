// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Only scheduling and IO differ; service.rs owns the protocol and reply ordering.
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Default)]
pub struct Http;

#[cfg(target_arch = "wasm32")]
impl Http {
    pub fn url(&self, path: &str) -> String {
        path.to_string()
    }

    pub fn spawn<F: std::future::Future<Output = ()> + 'static>(
        &self,
        task: impl FnOnce(Self) -> F,
    ) {
        wasm_bindgen_futures::spawn_local(task(self.clone()));
    }

    pub async fn get_text(&self, path: &str) -> Result<String, String> {
        perfr_viewer::net::web::get_text(path).await
    }

    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>, String> {
        perfr_viewer::net::web::get_bytes(path).await
    }

    pub async fn send_text(&self, method: &str, path: &str, body: &str) -> Result<String, String> {
        crate::service::send_text(method, path, body).await
    }

    pub async fn send_bytes(
        &self,
        path: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<String, String> {
        crate::service::send_bytes(path, bytes, content_type).await
    }
}
