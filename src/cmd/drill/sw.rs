// Copyright 2025 Fernando Borretti
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::LazyLock;

use axum::http::HeaderName;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::response::Html;
use maud::Markup;
use maud::html;

use crate::cmd::drill::template::STYLE_URL;
use crate::cmd::drill::template::page_template;
use crate::utils::CACHE_CONTROL_REVALIDATE;
use crate::utils::revision;

/// Where the worker is registered from, and the only path it can be served
/// at: a worker's scope is the directory it was fetched from, and it has to
/// see every page.
pub const SW_URL: &str = "/sw.js";

/// The page shown in place of one that could not be fetched.
pub const OFFLINE_URL: &str = "/offline";

/// The asset routes the worker may answer from its cache.
///
/// Each is served `immutable` under a revision naming this build's bytes, so
/// a hit is correct by construction. Nothing else qualifies — `/icons/` is
/// deliberately absent, being a fixed path the system reads once at install
/// rather than something a page waits on.
pub const RUNTIME_PREFIXES: &[&str] = &["/style/", "/fonts/", "/katex/", "/hljs/"];

/// Fetched when the worker installs, so the offline page is legible the
/// first time it is needed.
pub static PRECACHE: LazyLock<[&'static str; 2]> =
    LazyLock::new(|| [OFFLINE_URL, STYLE_URL.as_str()]);

const SW_SOURCE: &str = include_str!("sw.js");

/// The worker, with this build's cache name and paths declared above it.
///
/// The cache is named after the bytes of everything it holds, which does two
/// things: a build that changes an asset changes this file, which is the only
/// way a browser learns there is a new worker to install; and the new worker
/// then drops the previous build's cache instead of inheriting it.
pub static SW_JS: LazyLock<String> = LazyLock::new(|| {
    let precache = json_array(PRECACHE.iter().copied());
    let runtime = json_array(RUNTIME_PREFIXES.iter().copied());
    let offline = offline_page().into_string();
    let cache = revision(&[
        SW_SOURCE.as_bytes(),
        precache.as_bytes(),
        runtime.as_bytes(),
        offline.as_bytes(),
    ]);
    format!(
        "const CACHE = \"hashcards-{cache}\";\n\
         const OFFLINE_URL = \"{OFFLINE_URL}\";\n\
         const PRECACHE = {precache};\n\
         const RUNTIME = {runtime};\n\n\
         {SW_SOURCE}"
    )
});

/// A JSON array of paths. They are ours, not user input, and a path that
/// needed escaping would be a bug rather than something to encode around —
/// so it is refused here instead.
fn json_array<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let quoted: Vec<String> = items
        .map(|item| {
            debug_assert!(
                !item.contains(['"', '\\']),
                "an asset path needing JSON escaping: {item}"
            );
            format!("\"{item}\"")
        })
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// Shown when a page could not be fetched.
///
/// It is read exactly when nothing else can be loaded, so it asks for no
/// script: the way on is a plain link, which is a navigation, which goes to
/// the network and lands back here if the network is still gone.
pub fn offline_page() -> Markup {
    page_template(html! {
        div.offline-page {
            div.browse-header {
                h1 { "Offline" }
            }
            p {
                "This page needs the server, and the server cannot be "
                "reached just now."
            }
            p.empty {
                "Nothing has been lost. Every grade is written as it is "
                "given, so a session that was interrupted kept the answers "
                "you had already made."
            }
            p { a.btn href="/" { "Try again" } }
        }
    })
}

/// The worker itself must never be served from a cache: it is what a browser
/// re-fetches to discover that there is a new one.
pub async fn sw_handler() -> (StatusCode, [(HeaderName, &'static str); 2], &'static str) {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "text/javascript"),
            (CACHE_CONTROL, CACHE_CONTROL_REVALIDATE),
        ],
        SW_JS.as_str(),
    )
}

/// `Cache.addAll` fetches through the browser's own cache, and this is a
/// fixed path. A copy retained across a build would be precached by the new
/// worker while naming the previous build's stylesheet — which that worker
/// does not hold, leaving the one page it exists to serve unstyled.
pub async fn offline_handler() -> ([(HeaderName, &'static str); 1], Html<String>) {
    (
        [(CACHE_CONTROL, CACHE_CONTROL_REVALIDATE)],
        Html(offline_page().into_string()),
    )
}

#[cfg(test)]
mod tests {
    use axum::http::header::CACHE_CONTROL;
    use axum::response::IntoResponse;

    use super::OFFLINE_URL;
    use super::PRECACHE;
    use super::RUNTIME_PREFIXES;
    use super::SW_JS;
    use super::offline_handler;
    use super::offline_page;
    use crate::cmd::drill::template::STYLE_URL;

    /// Nothing a user made may be stored by the worker: a cache here is
    /// shared by every tab of the app on the device, and a page is one
    /// user's.
    #[test]
    fn test_precache_holds_nothing_user_specific() {
        for url in PRECACHE.iter() {
            assert!(
                *url == OFFLINE_URL || url.starts_with("/style/"),
                "`{url}` is precached but is not a public, fixed asset"
            );
        }
        assert!(
            PRECACHE.contains(&OFFLINE_URL),
            "the offline page is what the worker exists to serve: {PRECACHE:?}"
        );
        assert!(
            PRECACHE.contains(&STYLE_URL.as_str()),
            "the offline page would be served unstyled: {PRECACHE:?}"
        );
    }

    /// Serving a cached response without revalidating is only honest for a
    /// path that names the bytes it returns. Every prefix cached at runtime
    /// must be one of those, and the worker must additionally insist on
    /// seeing the revision in the path.
    #[test]
    fn test_runtime_cache_covers_only_revisioned_assets() {
        for prefix in RUNTIME_PREFIXES {
            assert!(
                ["/style/", "/fonts/", "/katex/", "/hljs/"].contains(prefix),
                "`{prefix}` is cached at runtime but is not a revisioned asset route"
            );
        }
        assert!(
            SW_JS.contains("[0-9a-f]{16}"),
            "the worker does not check for a revision, so the legacy fixed \
             paths (`/style.css`, `/fonts/{{name}}`) would be pinned forever"
        );
    }

    /// The cache is named after this build's assets. If it were not, a build
    /// that changed the stylesheet would leave every installed client serving
    /// the old one out of a cache it had no reason to drop.
    #[test]
    fn test_cache_name_follows_the_assets() {
        let version = SW_JS
            .split_once("const CACHE = \"hashcards-")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(v, _)| v.to_string())
            .unwrap_or_default();
        assert_eq!(
            version.len(),
            16,
            "no revision in the cache name: {version}"
        );
        assert!(
            version.chars().all(|c| c.is_ascii_hexdigit()),
            "the cache name is not a revision: {version}"
        );
        assert!(
            SW_JS.contains(STYLE_URL.as_str()),
            "the worker does not name this build's stylesheet, so the cache \
             name cannot move when the stylesheet does"
        );
    }

    /// A page is always fetched, and the offline page stands in only when
    /// that fetch fails. Caching one would show a user another user's screen,
    /// or their own from an hour ago.
    #[test]
    fn test_pages_and_writes_are_never_cached() {
        assert!(
            SW_JS.contains(r#"request.method !== "GET""#),
            "a POST could be answered from the cache"
        );
        assert!(
            SW_JS.contains(r#"request.mode === "navigate""#),
            "navigations are not singled out"
        );
        for path in ["/collection/", "/files", "/mcp", "/file/"] {
            assert!(
                !SW_JS.contains(path),
                "`{path}` is named in the worker; it must be left to the server"
            );
        }
    }

    /// A worker may be killed as soon as the response it is serving settles.
    /// A `cache.put` still running then is dropped on the floor, and the
    /// asset the user will need offline was never stored.
    #[test]
    fn test_runtime_cache_writes_outlive_the_response() {
        assert!(
            SW_JS.contains("event.waitUntil(cache.put("),
            "a runtime cache write is not tied to the event's lifetime, so \
             the worker may be terminated before it lands"
        );
    }

    /// `Cache.addAll` fetches through the browser's own cache, and the
    /// offline page is at a fixed path. A retained copy would be precached
    /// by a new worker, and it names the *previous* build's stylesheet —
    /// which that worker has no reason to hold.
    #[tokio::test]
    async fn test_offline_page_is_never_held_by_the_browser() {
        let response = offline_handler().await.into_response();
        let cache_control = response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            cache_control, "no-cache",
            "the offline page may be served from a browser cache, so a new \
             worker can precache the previous build's copy"
        );
    }

    /// It is shown exactly when the network is gone, so it must not need
    /// anything from the network to be legible.
    #[test]
    fn test_offline_page_stands_on_its_own() {
        let html = offline_page().into_string();
        assert!(
            html.contains("offline") || html.contains("Offline"),
            "the offline page does not say what happened: {html}"
        );
        assert!(
            html.contains(STYLE_URL.as_str()),
            "the offline page must ask for the precached stylesheet: {html}"
        );
    }
}
