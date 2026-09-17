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

document.addEventListener("DOMContentLoaded", function () {
  try {
    if (typeof katex !== "undefined") {
      // Render inline math
      document.querySelectorAll(".math-inline").forEach(function (element) {
        katex.render(element.textContent, element, {
          displayMode: false,
          throwOnError: false,
          macros: MACROS,
        });
      });
      // Render display math
      document.querySelectorAll(".math-display").forEach(function (element) {
        katex.render(element.textContent, element, {
          displayMode: true,
          throwOnError: false,
          macros: MACROS,
        });
      });
    }
    // Initialize syntax highlighting
    if (typeof hljs !== "undefined") {
      hljs.highlightAll();
    }
  } finally {
    // The card content must become visible no matter what failed above
    // (BUG-25): the page bootstraps with `.card-content { opacity: 0 }`
    // to avoid a flash of unrendered math.
    const cardContent = document.querySelector(".card-content");
    if (cardContent) {
      cardContent.style.opacity = "1";
    }
  }
});

document.addEventListener("keydown", function (event) {
  // A held-down key fires repeated keydown events; only the first physical
  // press should act (BUG-06).
  if (event.repeat) {
    return;
  }
  // Skip during text input or textarea.
  if (
    (event.target.tagName === "INPUT" && event.target.type === "text") ||
    event.target.tagName === "TEXTAREA"
  ) {
    return;
  }

  const keybindings = {
    " ": "reveal", // Space
    u: "undo",
    b: "bookmark",
    1: "forgot",
    2: "hard",
    3: "good",
    4: "easy",
  };

  if (keybindings[event.key]) {
    // Ignore modifiers.
    if (event.shiftKey || event.ctrlKey || event.altKey || event.metaKey) {
      return;
    }
    event.preventDefault();
    const id = keybindings[event.key];
    const node = document.getElementById(id);
    if (node) {
      node.click();
    }
  }
});

// The theme switch.
//
// Follows the system until it is touched; a remembered two-state switch from
// then on. The inline script in the head is what applies a stored choice
// before anything is drawn — this only keeps the button honest and moves the
// installed app's status-bar colour with it.
//
// The button is rendered hidden and revealed here: without script it could
// neither remember a choice nor relabel itself.
document.addEventListener("DOMContentLoaded", function () {
  const btn = document.querySelector("[data-theme-toggle]");
  if (!btn) {
    return;
  }
  const label = btn.querySelector("[data-theme-label]");

  function current() {
    const set = document.documentElement.getAttribute("data-theme");
    if (set) {
      return set;
    }
    return window.matchMedia("(prefers-color-scheme: dark)").matches
      ? "dark"
      : "light";
  }

  function paint() {
    const now = current();
    // Names the destination, not the state: a button reading "Dark" while the
    // page is dark reads as a label rather than as something to press.
    label.textContent = now === "dark" ? "Light" : "Dark";
    // An installed app frames the page in this colour. The two media-scoped
    // tags in the head answer the system rather than the choice, so the
    // choice needs one of its own.
    let meta = document.querySelector('meta[name="theme-color"]:not([media])');
    if (!meta) {
      meta = document.createElement("meta");
      meta.setAttribute("name", "theme-color");
      document.head.appendChild(meta);
    }
    meta.setAttribute("content", now === "dark" ? "#14171d" : "#f2f0ea");
  }

  btn.addEventListener("click", function () {
    const next = current() === "dark" ? "light" : "dark";
    document.documentElement.setAttribute("data-theme", next);
    try {
      localStorage.setItem("hashcards.theme", next);
    } catch (e) {
      // Storage can be disabled. The choice then lasts this page only.
    }
    paint();
  });

  paint();
  btn.hidden = false;
});

// Markdown editor: put text in at the caret and let the preview know.
function insertAtCaret(textarea, text) {
  var start = textarea.selectionStart;
  var end = textarea.selectionEnd;
  textarea.value = textarea.value.slice(0, start) + text + textarea.value.slice(end);
  textarea.selectionStart = textarea.selectionEnd = start + text.length;
  textarea.focus();
  textarea.dispatchEvent(new Event('input'));
}

// Markdown editor: insert a card skeleton at the caret.
document.querySelectorAll('.editor-toolbar button[data-snippet]').forEach(function (button) {
  button.addEventListener('click', function () {
    var textarea = document.getElementById('editor-text');
    if (!textarea) return;
    insertAtCaret(textarea, button.getAttribute('data-snippet'));
  });
});

// Markdown editor: paste an image straight into the card.
//
// The upload is what makes the reference valid: a card pointing at a file
// that is not in the collection fails media validation and takes the whole
// collection page down, which is why the Image button no longer inserts a
// placeholder and says this instead.
(function () {
  var textarea = document.getElementById('editor-text');
  var form = document.getElementById('editor-form');
  var status = document.getElementById('editor-status');
  if (!textarea || !form || !status) return;

  var idle = status.textContent;
  function say(message, kind) {
    status.textContent = message || idle;
    status.className = 'editor-hint' + (kind ? ' editor-status-' + kind : '');
  }

  var help = document.getElementById('image-help');
  if (help) {
    help.addEventListener('click', function () {
      say('Copy an image, then paste it here with Ctrl+V — it is stored with the collection and referenced for you.', null);
      textarea.focus();
    });
  }

  // The path travels raw in `data-path`; the URL needs each component
  // encoded, and `/` kept as the separator.
  function mediaUrl() {
    var parts = form.getAttribute('data-path').split('/').map(encodeURIComponent);
    return '/files/media/' + parts.join('/');
  }

  textarea.addEventListener('paste', function (event) {
    var files = event.clipboardData && event.clipboardData.files;
    if (!files || !files.length) return;
    var file = files[0];
    if (file.type.indexOf('image/') !== 0) return;
    event.preventDefault();

    say('Uploading the image…', null);
    fetch(mediaUrl(), {
      method: 'POST',
      headers: { 'Content-Type': file.type },
      body: file,
    })
      .then(function (response) {
        return response.text().then(function (body) {
          return { ok: response.ok, body: body };
        });
      })
      .then(function (result) {
        if (!result.ok) {
          say(result.body, 'error');
          return;
        }
        insertAtCaret(textarea, '![](' + result.body + ')');
        say('Image added. Save to keep it.', 'ok');
      })
      .catch(function () {
        say('The image could not be uploaded.', 'error');
      });
  });
})();

// Markdown editor: debounced live parse preview.
(function () {
  var textarea = document.getElementById('editor-text');
  var pane = document.getElementById('preview');
  var form = document.getElementById('editor-form');
  if (!textarea || !pane || !form) return;

  var timer = null;
  // Only the newest request may paint. Without this an earlier, slower
  // preview can land after a later one and leave the pane showing a buffer
  // the user has already moved on from, until the next keystroke.
  var inFlight = null;
  function refresh() {
    var body = new URLSearchParams();
    body.set('path', form.getAttribute('data-path'));
    body.set('content', textarea.value);
    if (inFlight) inFlight.abort();
    var controller = new AbortController();
    inFlight = controller;
    fetch('/files/preview', {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: body.toString(),
      signal: controller.signal,
    })
      .then(function (r) { return r.text(); })
      .then(function (html) {
        if (inFlight !== controller) return;
        inFlight = null;
        pane.innerHTML = html;
      })
      .catch(function () { /* leave the last good preview in place */ });
  }

  textarea.addEventListener('input', function () {
    if (timer) clearTimeout(timer);
    timer = setTimeout(refresh, 300);
  });
  refresh();
})();

// Sources page: copy the starter template.
(function () {
  var button = document.getElementById('copy-template');
  var block = document.getElementById('card-template');
  if (!button || !block) return;
  function done(label, ms) {
    button.textContent = label;
    setTimeout(function () { button.textContent = 'Copy template'; }, ms);
  }
  // `navigator.clipboard` is undefined outside a secure context, and serving
  // over plain HTTP on a LAN address is a supported deployment. Select the
  // block instead, so Ctrl+C still works, and say so on the button.
  function selectInstead() {
    var range = document.createRange();
    range.selectNodeContents(block);
    var selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    done('Press Ctrl+C', 4000);
  }
  button.addEventListener('click', function () {
    if (!navigator.clipboard) {
      selectInstead();
      return;
    }
    navigator.clipboard.writeText(block.textContent).then(function () {
      done('Copied', 2000);
    }, selectInstead);
  });
})();

// Register the service worker.
//
// It caches the revisioned assets and stands in with an offline page when a
// navigation cannot reach the server; everything else goes to the network as
// before, so a browser that refuses to register one loses nothing.
//
// It will refuse outside a secure context, which for this app means plain
// HTTP on anything but localhost — a supported way to run it on a LAN. That
// is why the failure is silent: there is nothing the user could do about it
// and nothing they lose by it.
(function () {
  if (!("serviceWorker" in navigator)) return;
  window.addEventListener("load", function () {
    try {
      navigator.serviceWorker.register("/sw.js").catch(function () {});
    } catch (e) {
      // A browser that throws rather than rejecting. Same outcome.
    }
  });
})();
