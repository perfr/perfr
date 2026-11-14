// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The parameters the viewer reads from its own URL.
//!
//! | parameter | read by | meaning |
//! |---|---|---|
//! | `?capture=<url>` | here | open a `.perfr.stream` instead of connecting to a service |
//! | `?story=<url>\|demo\|box3d` | here | open a story over the capture |
//! | `?chrome=compact` | here | a short bar, for an iframe |
//! | `?collapse=scheduler` | here | start with the scheduler track folded |
//! | `?tracks=<words>` | here | start with the tracks box filled in |
//! | `?report=<tab>` | here | open the report panel on a tab |
//! | `?theme=<key>` | here | pick a colour theme |
//! | `?webgpu` | here | WebGPU instead of WebGL2 |
//! | `?trace=<path>` | `chrome_load.rs` | load a same-origin trace file |
//! | `?share_base=<url>` | `share.rs` | a loopback share server, for testing |
//! | `?threads=<n>` | `viewer-dist/index.html` | size of the worker pool |
//! | `?download=<url>` | `viewer-dist/index.html` | link to a shared capture's archive |
//! | `#share_token=<token>` | `viewer-dist/index.html`, `share.rs` | the bearer a login redirect brings back |
//!
//! Each reader comes as a pair: a pure function over the query string, which
//! the tests call, and a `_from_location` wrapper that asks the browser.

/// `?report=flat|top_down|bottom_up|modules` — open the report panel on a
/// given tab.
///
/// A deep link is worth having on its own, and it is what makes the
/// screenshot suite deterministic: egui paints to a canvas, so there is no
/// DOM node for a tab pill to click. Automating the UI otherwise means
/// synthesising mouse events at hard-coded coordinates, which breaks the
/// first time a pill moves.
pub fn query_report_tab_from_location() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .and_then(|s| query_report_tab(&s))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

/// `?webgpu` (or `?webgpu=1`) -- opt back into the WebGPU backend. The viewer
/// defaults to WebGL2, because WebGPU (Dawn) init fails on some Linux/NVIDIA
/// drivers ("vkGetSemaphoreFdKHR ... VK_ERROR_INITIALIZATION_FAILED"), and
/// WebGL2 renders the instanced timeline fine. This is the escape hatch for a
/// machine where WebGPU works and its extra headroom is wanted.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn query_prefers_webgpu_from_location() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .map(|s| query_prefers_webgpu(&s))
            .unwrap_or(false)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn query_prefers_webgpu(search: &str) -> bool {
    search
        .trim_start_matches('?')
        .split('&')
        .any(|kv| kv == "webgpu" || kv == "webgpu=1" || kv == "webgpu=true")
}

/// `?capture=<url>` -- open a capture stream file (the `stream` export)
/// instead of connecting to a service: the static web page's mode. The
/// value is handed to `fetch` as it is once decoded: a path relative to the
/// page, or an absolute URL such as a share's presigned link.
pub fn query_capture_url_from_location() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .and_then(|s| query_capture_url(&s))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

/// The page URL, fragment included, for the login `next` parameter.
pub fn location_href() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().href().ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

/// The page's query string, including the leading `?` when the browser has one.
pub fn location_search() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn query_capture_url(search: &str) -> Option<String> {
    let value = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix("capture="))?;
    if value.is_empty() {
        return None;
    }
    Some(crate::util::percent_decode(value))
}

/// `?story=<url>` -- open a story on top of the capture. `demo` and
/// `box3d` are the story shipped with the viewer. Any other value is a
/// URL, fetched the same way as `capture=`.
pub fn query_story_url_from_location() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .and_then(|s| query_story_url(&s))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn query_story_url(search: &str) -> Option<String> {
    let value = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix("story="))?;
    if value.is_empty() {
        return None;
    }
    Some(crate::util::percent_decode(value))
}

/// `?chrome=compact` -- a short bar for an iframe. Capture controls stay
/// available from the story bar's Share button, and `?capture=` still loads.
pub fn query_chrome_compact_from_location() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .map(|s| query_chrome_compact(&s))
            .unwrap_or(false)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn query_chrome_compact(search: &str) -> bool {
    search.trim_start_matches('?').split('&').any(|kv| kv == "chrome=compact")
}

/// `?collapse=scheduler` -- start with the machine-wide scheduler track
/// folded, so a process's own lanes are in view without scrolling. On a
/// 32-core box the scheduler alone is taller than a screenshot, and the
/// screenshot suite exists to show the process, not the cores.
pub fn query_collapse_scheduler_from_location() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .map(|s| query_collapses_scheduler(&s))
            .unwrap_or(false)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

/// Parses `collapse=` out of a location query string; `scheduler` is the
/// only value so far.
#[cfg(any(target_arch = "wasm32", test))]
pub fn query_collapses_scheduler(search: &str) -> bool {
    search
        .trim_start_matches('?')
        .split('&')
        .any(|pair| pair == "collapse=scheduler")
}

/// Parses `report=` out of a location query string.
#[cfg(any(target_arch = "wasm32", test))]
pub fn query_report_tab(search: &str) -> Option<String> {
    for pair in search.trim_start_matches('?').split('&') {
        if let Some(value) = pair.strip_prefix("report=") {
            if !value.is_empty() {
                return Some(value.to_ascii_lowercase());
            }
        }
    }
    None
}

/// `?tracks=<words>` -- start with the tracks box filled, so a shared link or
/// a screenshot opens on just the tracks that matter. `+` and `%20` are
/// spaces, as a browser writes them.
pub fn query_track_filter_from_location() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .and_then(|s| query_track_filter(&s))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn query_track_filter(search: &str) -> Option<String> {
    for pair in search.trim_start_matches('?').split('&') {
        if let Some(value) = pair.strip_prefix("tracks=") {
            let value = value.replace('+', " ").replace("%20", " ");
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// `?theme=<key>` -- pick a colour scheme at load (perfr, dracula, nord,
/// gruvbox, solarized, solarized-light, gruvbox-light). Lets a screenshot
/// or a shared link pin the look. Without it, the viewer uses Nord unless
/// `perfr_theme` is already saved.
pub fn query_theme_from_location() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .and_then(|s| query_theme(&s))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn query_theme(search: &str) -> Option<String> {
    for pair in search.trim_start_matches('?').split('&') {
        if let Some(value) = pair.strip_prefix("theme=") {
            if !value.is_empty() {
                return Some(value.to_ascii_lowercase());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_query_reads_the_key() {
        assert_eq!(query_theme("?theme=dracula").as_deref(), Some("dracula"));
        assert_eq!(query_theme("foo=1&theme=Nord").as_deref(), Some("nord"));
        assert_eq!(query_theme("?theme="), None);
        assert_eq!(query_theme("?report=flat"), None);
        // A key the registry does not know is simply ignored downstream.
        assert!(perfr_live_event::theme::by_key("dracula").is_some());
        assert!(perfr_live_event::theme::by_key("nope").is_none());
    }

    #[test]
    fn a_report_tab_is_read_from_the_query_string() {
        assert_eq!(query_report_tab("?report=bottom_up").as_deref(), Some("bottom_up"));
        assert_eq!(query_report_tab("?dev=0&report=Modules").as_deref(), Some("modules"));
        assert_eq!(query_report_tab("?report="), None);
        assert_eq!(query_report_tab("?dev=0"), None);
        // A different parameter ending in the same letters must not match.
        assert_eq!(query_report_tab("?myreport=flat"), None);
    }

    #[test]
    fn collapse_scheduler_is_read_from_the_query_string() {
        assert!(query_collapses_scheduler("?collapse=scheduler"));
        assert!(query_collapses_scheduler("?report=flat&collapse=scheduler"));
        assert!(!query_collapses_scheduler("?collapse=machine"));
        assert!(!query_collapses_scheduler("?xcollapse=scheduler"));
    }

    #[test]
    fn tracks_query_is_words_with_plus_and_percent20_as_spaces() {
        assert_eq!(query_track_filter("?tracks=physics"), Some("physics".to_string()));
        assert_eq!(query_track_filter("?collapse=scheduler&tracks=physics+render"), Some("physics render".to_string()));
        assert_eq!(query_track_filter("?tracks=a%20b"), Some("a b".to_string()));
        assert_eq!(query_track_filter("?tracks="), None);
        assert_eq!(query_track_filter("?xtracks=a"), None);
        assert!(!query_collapses_scheduler(""));
    }

    #[test]
    fn the_capture_query_is_read_and_decoded() {
        assert_eq!(query_capture_url("?capture=captures/box3d.perfr.stream"), Some("captures/box3d.perfr.stream".into()));
        assert_eq!(
            query_capture_url("?collapse=scheduler&capture=..%2Fcaptures%2Fa%20b.perfr.stream"),
            Some("../captures/a b.perfr.stream".into())
        );
        assert_eq!(query_capture_url("?capture=https%3A%2F%2Fbucket.s3.amazonaws.com%2Fa.perfr.stream%3FX-Amz-Signature%3Dx%252Fy%26token%3Da%2Bb&download=archive"),
            Some("https://bucket.s3.amazonaws.com/a.perfr.stream?X-Amz-Signature=x%2Fy&token=a+b".into()));
        assert_eq!(query_capture_url("?capture="), None);
        assert_eq!(query_capture_url("?report=live"), None);
        assert_eq!(query_capture_url("?capture=x%2"), Some("x%2".into()));
        // A shared link opens the viewer with this path. It is same-origin
        // and has no file suffix; fetch must be handed it unchanged.
        assert_eq!(query_capture_url("?capture=/api/share/abc"), Some("/api/share/abc".into()));
        assert_eq!(
            query_capture_url("?share_base=http%3A%2F%2F127.0.0.1%3A9&capture=%2Fapi%2Fshare%2Fabc"),
            Some("/api/share/abc".into())
        );
        // A story and a compact bar ride along. They must not change which capture the page opens.
        assert_eq!(
            query_capture_url("?capture=captures/box3d.perfr.stream&story=stories/box3d.perfr.story&chrome=compact"),
            Some("captures/box3d.perfr.stream".into())
        );
    }

    #[test]
    fn story_and_chrome_queries_are_independent_of_capture() {
        let q = "?capture=captures/box3d.perfr.stream&story=stories%2Fbox3d.perfr.story&chrome=compact";
        assert_eq!(query_capture_url(q), Some("captures/box3d.perfr.stream".into()));
        assert_eq!(query_story_url(q), Some("stories/box3d.perfr.story".into()));
        assert!(query_chrome_compact(q));
        assert_eq!(query_story_url("?story=demo"), Some("demo".into()));
        assert_eq!(query_story_url("?story="), None);
        assert_eq!(query_story_url("?capture=x"), None);
        assert!(!query_chrome_compact("?capture=x&chrome=full"));
        assert!(!query_chrome_compact(""));
    }
}
