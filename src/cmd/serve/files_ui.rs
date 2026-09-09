use maud::Markup;
use maud::html;

use crate::cmd::drill::template::page_template;
use crate::cmd::serve::cards::CardRoot;
use crate::cmd::serve::config::slugify;
use crate::cmd::serve::files::LOOSE_FILE;
use crate::cmd::serve::files::TreeEntry;
use crate::cmd::serve::files::parse_buffer;
use crate::cmd::serve::href::encoded_path;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;
use crate::markdown::MarkdownRenderConfig;
use crate::media::resolve::MediaResolverBuilder;

/// What the editor's status line says when it has nothing else to report.
/// The limit is stated before it is hit rather than only on refusal.
const PASTE_HINT: &str = "Paste an image to add it — PNG, JPEG, GIF or WebP, up to 10 MB.";

/// One step of tree indentation per level, on the row rather than on the
/// list: the rows draw their own separators edge to edge, so the depth has
/// to move the label inside the row instead of moving the row.
fn indent(depth: usize) -> String {
    format!("padding-left: {}rem", 1.0 + depth as f32 * 1.25)
}

/// The file manager: the whole tree, with per-row rename and delete, and a
/// create form for each folder.
pub fn render_tree_page(tree: &[TreeEntry], flash: Option<Flash>) -> Markup {
    page_template(html! {
        div.landing {
            @if let Some(f) = &flash { (f.render()) }
            div.browse-header {
                a.back-link href="/" { "← Collections" }
                h1 { "My Cards" }
                a.back-link href="/trash" { "Trash" }
            }

            p.hint {
                "Each top-level folder is a collection. Files inside it are decks."
            }

            form.add-source-form action="/files/folder" method="post" {
                input type="hidden" name="parent" value="";
                div.add-source-row {
                    input.input.add-source-url type="text" name="name"
                        placeholder="New collection name" required;
                    input.btn.btn-primary type="submit" value="Add collection";
                }
            }

            @if tree.is_empty() {
                p.notice { "No cards yet. Create a collection to start." }
            }

            @if !tree.is_empty() {
                ul.file-tree {
                    @for entry in tree {
                        li.file-row style=(indent(entry.depth)) {
                            @if entry.is_dir {
                                span.file-name.folder { (entry.name) }
                            } @else if entry.depth == 0 {
                                // Not inside a collection, so the editor
                                // would open a page that cannot save.
                                span.file-name.file-loose title=(LOOSE_FILE) { (entry.name) }
                            } @else {
                                a.file-name href=(format!("/files/edit/{}", encoded_path(&entry.rel_path))) {
                                    (entry.name)
                                }
                            }
                            div.file-actions {
                                @if entry.is_dir {
                                    // One name, two destinations: the same
                                    // field makes a file or a folder,
                                    // depending on which button posts it.
                                    form.file-form action="/files/file" method="post" {
                                        input type="hidden" name="parent" value=(entry.rel_path);
                                        input.input.input-sm type="text" name="name"
                                            placeholder="new name" required;
                                        input.btn.btn-sm type="submit" value="Add file";
                                        input.btn.btn-sm type="submit" value="Add folder"
                                            formaction="/files/folder";
                                    }
                                }
                                form.file-form action="/files/rename" method="post" {
                                    input type="hidden" name="path" value=(entry.rel_path);
                                    input.input.input-sm type="text" name="name"
                                        placeholder="rename to" required;
                                    input.btn.btn-sm type="submit" value="Rename";
                                }
                                form.file-form action="/files/delete" method="post" {
                                    input type="hidden" name="path" value=(entry.rel_path);
                                    input.btn.btn-sm.btn-danger type="submit" value="Delete";
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// The document editor: raw markdown on the left with insert buttons, a
/// live parse preview on the right.
pub fn render_editor_page(
    rel_path: &str,
    content: &str,
    mtime: u64,
    flash: Option<Flash>,
) -> Markup {
    page_template(html! {
        div.editor {
            @if let Some(f) = &flash { (f.render()) }
            div.browse-header {
                a.back-link href="/files" { "← My Cards" }
                h1 { (rel_path) }
            }

            div.editor-toolbar {
                button.btn.btn-sm type="button" data-snippet="Q: \nA: " { "Q/A" }
                button.btn.btn-sm type="button" data-snippet="C: The [answer] goes here." { "Cloze" }
                button.btn.btn-sm type="button" data-snippet="T: \nD: " { "Term" }
                button.btn.btn-sm type="button" data-snippet="\n---\n" { "Separator" }
                button.btn.btn-sm type="button" data-snippet="$$\n\n$$" { "LaTeX" }
                // No snippet: an image reference to a file that is not
                // there fails the collection's media validation and takes
                // the whole collection page down. The button says how to
                // add one for real instead.
                button.btn.btn-sm #image-help type="button" { "Image" }
            }

            div.editor-panes {
                // The preview fetch needs the path as it is on disk, which
                // the encoded action cannot be turned back into safely, so
                // it travels beside it.
                form #editor-form data-path=(rel_path)
                    action=(format!("/files/edit/{}", encoded_path(rel_path))) method="post" {
                    textarea.textarea #editor-text name="content" spellcheck="false" { (content) }
                    div.editor-actions {
                        input type="hidden" name="mtime" value=(mtime);
                        input.btn.btn-primary type="submit" value="Save";
                        span.editor-hint #editor-status { (PASTE_HINT) }
                    }
                }
                div #preview .preview-pane {
                    p.hint { "Start typing to see your cards." }
                }
            }
        }
    })
}

/// Render an unsaved buffer's parse result.
///
/// Returns a fragment, not a page: the editor swaps it into the preview
/// pane. Errors carry their line number so the user can find them in the
/// textarea.
pub fn render_preview(root: &CardRoot, rel_path: &str, content: &str) -> Markup {
    let parsed = match parse_buffer(rel_path, content) {
        Ok(p) => p,
        Err(e) => {
            return html! {
                div.preview-error {
                    p { "This file does not parse yet." }
                    p.error-detail { (e.message()) }
                }
            };
        }
    };

    let config = match preview_render_config(root, rel_path) {
        Ok(c) => c,
        Err(e) => {
            return html! { div.preview-error { p { (e.to_string()) } } };
        }
    };

    html! {
        p.preview-count { (count_of(parsed.cards.len(), "card")) }
        @if !parsed.duplicates.is_empty() {
            p.preview-warning {
                (format!(
                    "{} will be skipped.",
                    count_of(parsed.duplicates.len(), "duplicate card")
                ))
            }
        }
        @for card in &parsed.cards {
            div.preview-card {
                div.preview-front.rich-text {
                    @match card.html_front(&config) {
                        Ok(m) => (m),
                        Err(e) => p.error-detail { (e.to_string()) },
                    }
                }
                div.preview-back.rich-text {
                    @match card.html_back(&config) {
                        Ok(m) => (m),
                        Err(e) => p.error-detail { (e.to_string()) },
                    }
                }
            }
        }
    }
}

/// A count and the noun it counts, agreeing in number: "1 card", "2 cards".
fn count_of(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Resolve media the way the collection itself will: relative to the
/// top-level folder, which is the collection root.
///
/// A file sitting loose in the user's root has no collection, and is
/// refused rather than given the root as a stand-in: the root has no slug,
/// so every image in the preview would be requested from
/// `/collection//file` and 404. Such a file cannot be saved either
/// (`collection_folder` refuses it), so the editor turns it away up front.
///
/// The path comes straight from the browser, so it is resolved inside the
/// user's root like every other client-supplied path, rather than joined
/// onto it.
fn preview_render_config(root: &CardRoot, rel_path: &str) -> Fallible<MarkdownRenderConfig> {
    let trimmed = rel_path.trim_matches('/');
    let (coll_dir, deck_rel, slug) = match trimmed.split_once('/') {
        Some((top, rest)) => (root.resolve(top)?, rest.to_string(), slugify(top)),
        None => return fail(LOOSE_FILE),
    };
    Ok(MarkdownRenderConfig {
        resolver: MediaResolverBuilder::new()
            .with_collection_path(coll_dir)?
            .with_deck_path(std::path::PathBuf::from(deck_rel))?
            .build()?,
        file_url_prefix: format!("/collection/{slug}/file"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::files::TreeEntry;
    use crate::error::Fallible;
    use crate::helper::create_tmp_directory;

    /// The preview is the one place a client-supplied path reaches the
    /// filesystem without going through the file manager, so it has to be
    /// resolved inside the user's root like every other path.
    #[test]
    fn preview_refuses_a_path_outside_the_root() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        assert!(preview_render_config(&root, "../../etc/x.md").is_err());
        assert!(preview_render_config(&root, "/etc/x.md").is_err());
        Ok(())
    }

    /// `validate_name` allows `#` and `?`, which end a URL's path. Left raw
    /// in an href, the browser would drop everything after them and the
    /// editor would open the wrong file — or none.
    #[test]
    fn tree_links_encode_reserved_characters() {
        let tree = vec![TreeEntry {
            rel_path: "Spanish/a#b?c.md".to_string(),
            name: "a#b?c.md".to_string(),
            is_dir: false,
            depth: 1,
        }];
        let html = render_tree_page(&tree, None).into_string();
        assert!(
            html.contains("/files/edit/Spanish/a%23b%3Fc.md"),
            "got: {html}"
        );
    }

    #[test]
    fn the_editor_form_posts_to_the_encoded_path() {
        let html = render_editor_page("Spanish/a#b.md", "", 0, None).into_string();
        assert!(html.contains("/files/edit/Spanish/a%23b.md"), "got: {html}");
        // The preview fetch needs the raw path, not the URL, so it travels
        // in its own attribute rather than being parsed back out.
        assert!(
            html.contains(r#"data-path="Spanish/a#b.md""#),
            "got: {html}"
        );
    }

    /// A count of one used to read "1 cards".
    #[test]
    fn preview_counts_agree_with_their_nouns() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::create_dir(root.path().join("Spanish"))?;

        let one = render_preview(&root, "Spanish/verbs.md", "Q: One\nA: Card").into_string();
        assert!(one.contains("1 card<"), "got: {one}");

        let two = render_preview(
            &root,
            "Spanish/verbs.md",
            "Q: One\nA: Card\nQ: Two\nA: Cards",
        )
        .into_string();
        assert!(two.contains("2 cards<"), "got: {two}");

        // One dropped copy is one duplicate card, not "1 duplicate cards".
        let dup = render_preview(
            &root,
            "Spanish/verbs.md",
            "Q: One\nA: Card\n---\nQ: One\nA: Card",
        )
        .into_string();
        assert!(dup.contains("1 duplicate card "), "got: {dup}");
        Ok(())
    }

    /// Regression: cloze rendering — the grey blank on the front, the
    /// highlighted reveal on the back — is scoped to `.rich-text` in the
    /// stylesheet. Without that class the preview shows bare dots and an
    /// answer that looks like an unchanged copy of the prompt.
    #[test]
    fn preview_cards_carry_the_rich_text_class() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::create_dir(root.path().join("Spanish"))?;
        let html = render_preview(
            &root,
            "Spanish/verbs.md",
            "C: An [agonist] activates a receptor.",
        )
        .into_string();
        assert!(html.contains("preview-front rich-text"), "got: {html}");
        assert!(html.contains("preview-back rich-text"), "got: {html}");
        Ok(())
    }

    /// Regression guard: the Image button used to insert `![](image.png)`,
    /// a reference to a file that is never there — which fails the
    /// collection's media validation and takes its whole page down. It must
    /// stay a pointer to the paste path, and the hint must name the limit
    /// before the user hits it.
    #[test]
    fn the_image_button_inserts_no_dead_reference() {
        let html = render_editor_page("Spanish/verbs.md", "", 0, None).into_string();
        assert!(html.contains(r#"id="image-help""#), "got: {html}");
        assert!(!html.contains("image.png"), "got: {html}");
        assert!(html.contains("up to 10 MB"), "got: {html}");
    }
}
