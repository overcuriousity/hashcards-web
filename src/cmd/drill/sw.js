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

// The service worker.
//
// `sw.rs` declares CACHE, PRECACHE, RUNTIME and OFFLINE_URL above this line:
// the cache is named after the bytes of this build's assets, so a build that
// changes any of them changes this file, which is how a browser notices there
// is a new worker to install at all.
//
// What it will serve from a cache is deliberately narrow. Only the immutable,
// revisioned assets under RUNTIME are ever stored, plus the offline page.
// Nothing belonging to a user — no page, no card, no media, no POST — is
// cached, because a page here is one user's and a stale drill screen would
// grade a card the session has already moved past.

self.addEventListener("install", function (event) {
  event.waitUntil(
    caches
      .open(CACHE)
      .then(function (cache) {
        return cache.addAll(PRECACHE);
      })
      // Nothing in the cache is user-specific or version-ambiguous, so there
      // is no reason to make the user close every tab before the new worker
      // takes over.
      .then(function () {
        return self.skipWaiting();
      }),
  );
});

self.addEventListener("activate", function (event) {
  event.waitUntil(
    caches
      .keys()
      .then(function (names) {
        // Every cache but this build's. The revisioned paths mean stale
        // entries are harmless, but they are also dead weight forever.
        return Promise.all(
          names
            .filter(function (name) {
              return name !== CACHE;
            })
            .map(function (name) {
              return caches.delete(name);
            }),
        );
      })
      .then(function () {
        return self.clients.claim();
      }),
  );
});

// Only the revisioned copy of an asset may be cached. The same prefixes also
// serve the fixed paths that builds before revisioning rendered into their
// HTML, and those do *not* name their contents: cached here, one would be
// pinned for as long as the browser kept it.
var REVISIONED = /^\/[a-z]+\/(fonts\/)?[0-9a-f]{16}\//;

function isRuntimeAsset(url) {
  if (!REVISIONED.test(url.pathname)) {
    return false;
  }
  return RUNTIME.some(function (prefix) {
    return url.pathname.indexOf(prefix) === 0;
  });
}

self.addEventListener("fetch", function (event) {
  var request = event.request;
  // A POST is the user's work reaching the server. It must fail loudly when
  // it cannot get there, never be answered from here.
  if (request.method !== "GET") {
    return;
  }
  var url = new URL(request.url);
  if (url.origin !== self.location.origin) {
    return;
  }

  // Pages always come from the server. The offline page stands in only when
  // the network has actually failed, so a served page is never a stale one.
  if (request.mode === "navigate") {
    event.respondWith(
      fetch(request).catch(function () {
        return caches.open(CACHE).then(function (cache) {
          return cache.match(OFFLINE_URL).then(function (hit) {
            return hit || Response.error();
          });
        });
      }),
    );
    return;
  }

  // Everything else the server knows best: a user's media, the scripts whose
  // contents depend on the collection, the MCP endpoint. Left alone.
  if (!isRuntimeAsset(url)) {
    return;
  }

  // These paths name their own contents and are served `immutable`, so a hit
  // is right by construction and a miss is worth keeping.
  event.respondWith(
    caches.open(CACHE).then(function (cache) {
      return cache.match(request).then(function (hit) {
        if (hit) {
          return hit;
        }
        return fetch(request).then(function (response) {
          if (response.ok) {
            // A worker may be terminated as soon as the response it is
            // serving settles. The write is tied to the event so that it
            // outlives this one: unawaited, it would be dropped and the
            // asset would be missing on the load that needed it.
            event.waitUntil(cache.put(request, response.clone()));
          }
          return response;
        });
      });
    }),
  );
});
