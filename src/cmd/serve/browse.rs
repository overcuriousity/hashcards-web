use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::path::PathBuf;

use maud::Markup;
use maud::html;

use crate::cmd::drill::template::page_template;
use crate::cmd::serve::href::encoded_path;
use crate::collection::Collection;
use crate::db::Database;
use crate::error::Fallible;
use crate::flash::Flash;
use crate::parser::DuplicateCard;
use crate::types::card_hash::CardHash;
use crate::types::date::Date;
use crate::types::timestamp::Timestamp;

/// A node in the deck tree. Leaves have cards; parents aggregate children.
pub struct DeckNode {
    /// Display name for this segment (e.g., "particles").
    pub name: String,
    /// Full deck path (e.g., "grammar/particles"). Empty for the root.
    pub path: String,
    /// Number of cards directly in this deck (0 for pure parent nodes).
    pub total_cards: usize,
    /// Number of cards due today directly in this deck.
    pub due_today: usize,
    /// Child nodes.
    pub children: Vec<DeckNode>,
}

impl DeckNode {
    /// Total cards in this node and all descendants.
    pub fn total_cards_recursive(&self) -> usize {
        self.total_cards
            + self
                .children
                .iter()
                .map(|c| c.total_cards_recursive())
                .sum::<usize>()
    }

    /// Due cards in this node and all descendants.
    pub fn due_today_recursive(&self) -> usize {
        self.due_today
            + self
                .children
                .iter()
                .map(|c| c.due_today_recursive())
                .sum::<usize>()
    }
}

/// Counts per deck name.
struct DeckCounts {
    total: usize,
    due: usize,
}

/// The deck tree for a collection, plus what loading it turned up that the
/// user should be told about.
pub struct BrowseData {
    pub tree: DeckNode,
    /// Byte-identical cards found in two places. One copy is dropped at load
    /// time, so only one of them carries review history.
    pub duplicates: Vec<DuplicateCard>,
    /// The directory the collection was loaded from, stripped from duplicate
    /// locations before they are shown.
    pub coll_dir: PathBuf,
    /// Topic name to the file its cards came from, relative to the
    /// collection folder — the target of the topic's edit link.
    ///
    /// Not derived from the topic *name*: that defaults to the file's path,
    /// but a file's frontmatter `name:` overrides it, so the name is not a
    /// path. A topic whose cards come from more than one file gets no
    /// entry, since there is no one file to open.
    pub edit_paths: HashMap<String, PathBuf>,
}

/// Build a deck tree from a collection, computing per-deck due/total counts.
pub fn build_deck_tree(coll_dir: &Path, db: Database) -> Fallible<BrowseData> {
    let collection = Collection::open(coll_dir.to_path_buf(), db)?;
    let session_started_at = Timestamp::now();
    let today: Date = session_started_at.date();

    // Sync new cards to DB
    let db_hashes: HashSet<CardHash> = collection.db.card_hashes()?;
    for card in collection.cards.iter() {
        if !db_hashes.contains(&card.hash()) {
            collection.db.insert_card(card.hash(), session_started_at)?;
        }
    }

    let due_hashes: HashSet<CardHash> = collection.db.due_today(today)?;

    // Count per deck
    let mut counts: HashMap<String, DeckCounts> = HashMap::new();
    // `None` marks a topic seen in more than one file.
    let mut sources: HashMap<String, Option<PathBuf>> = HashMap::new();
    for card in &collection.cards {
        let entry = counts
            .entry(card.deck_name().clone())
            .or_insert(DeckCounts { total: 0, due: 0 });
        entry.total += 1;
        if due_hashes.contains(&card.hash()) {
            entry.due += 1;
        }
        let rel = card.relative_file_path(&collection.directory).ok();
        match sources.entry(card.deck_name().clone()) {
            Entry::Occupied(mut slot) => {
                if slot.get().as_ref() != rel.as_ref() {
                    slot.insert(None);
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(rel);
            }
        }
    }
    let edit_paths: HashMap<String, PathBuf> = sources
        .into_iter()
        .filter_map(|(name, path)| path.map(|p| (name, p)))
        .collect();

    Ok(BrowseData {
        tree: build_tree_from_counts(counts),
        duplicates: collection.duplicates,
        coll_dir: coll_dir.to_path_buf(),
        edit_paths,
    })
}

/// Build a hierarchical tree from flat deck name → counts mapping.
fn build_tree_from_counts(counts: HashMap<String, DeckCounts>) -> DeckNode {
    let mut root = DeckNode {
        name: String::new(),
        path: String::new(),
        total_cards: 0,
        due_today: 0,
        children: Vec::new(),
    };

    // Sort deck names for deterministic ordering.
    let mut names: Vec<String> = counts.keys().cloned().collect();
    names.sort();

    for deck_name in names {
        let deck_counts = &counts[&deck_name];
        let segments: Vec<&str> = deck_name.split('/').collect();
        insert_into_tree(&mut root, &segments, 0, &deck_name, deck_counts);
    }

    root
}

fn insert_into_tree(
    node: &mut DeckNode,
    segments: &[&str],
    depth: usize,
    full_path: &str,
    counts: &DeckCounts,
) {
    if depth == segments.len() {
        // We've reached the leaf — set counts on this node.
        node.total_cards = counts.total;
        node.due_today = counts.due;
        return;
    }

    let segment = segments[depth];

    // Find or create child for this segment.
    let child_idx = node.children.iter().position(|c| c.name == segment);
    let child_idx = match child_idx {
        Some(idx) => idx,
        None => {
            let child_path = if depth + 1 == segments.len() {
                full_path.to_string()
            } else {
                segments[..=depth].join("/")
            };
            node.children.push(DeckNode {
                name: segment.to_string(),
                path: child_path,
                total_cards: 0,
                due_today: 0,
                children: Vec::new(),
            });
            node.children.len() - 1
        }
    };

    insert_into_tree(
        &mut node.children[child_idx],
        segments,
        depth + 1,
        full_path,
        counts,
    );
}

/// The duplicates notice. `count` is the number of dropped copies, not the
/// number of cards involved, so a collection with one duplicate pair counts
/// as one.
fn duplicates_summary(count: usize) -> String {
    let subject = if count == 1 {
        "1 card in this collection is a byte-identical copy of another card".to_string()
    } else {
        format!("{count} cards in this collection are byte-identical copies of other cards")
    };
    format!("{subject}. Only one copy is drilled, and only that copy carries review history.")
}

/// Render a collection's own page: its topics, and the things you can do
/// with the collection as a whole.
///
/// The list page drills a collection in one tap, so this page is no longer a
/// gate on the way in — it is where you come to drill part of a collection,
/// or to look at what is in it.
pub fn render_browse_page(
    collection_name: &str,
    slug: &str,
    browse: &BrowseData,
    bookmark_count: usize,
    interrupted_sessions_closed: usize,
    flash: Option<Flash>,
) -> Markup {
    let BrowseData {
        tree,
        duplicates,
        coll_dir,
        edit_paths,
    } = browse;
    let total_due = tree.due_today_recursive();
    page_template(html! {
        @if let Some(f) = &flash { (f.render()) }
        div.browse {
            div.browse-header {
                a.back-link href="/" { "\u{2190} Collections" }
                h1 { (collection_name) }
            }
            @if !duplicates.is_empty() {
                div.notice.notice-warning {
                    p { (duplicates_summary(duplicates.len())) }
                    ul {
                        @for duplicate in duplicates {
                            li { (duplicate.display_under(coll_dir)) }
                        }
                    }
                }
            }
            @if interrupted_sessions_closed > 0 {
                p.notice {
                    (format!(
                        "{interrupted_sessions_closed} interrupted session(s) from an earlier run were closed. All reviews already made were kept; interrupted sessions cannot be resumed because the card queue is not saved."
                    ))
                }
            }
            @if tree.children.is_empty() {
                p.empty { "No topics found in this collection." }
            } @else {
                h2.section-title { "Topics" }
                form action=(format!("/collection/{slug}/start")) method="post" {
                    div.deck-tree {
                        @for child in &tree.children {
                            (render_deck_node(child, 0, collection_name, edit_paths))
                        }
                    }
                    div.browse-controls {
                        span.select-controls {
                            a.select-all href="#" onclick="selectAll(true); return false;" { "Select all" }
                            " / "
                            a.select-none href="#" onclick="selectAll(false); return false;" { "Select none" }
                        }
                        div.limit-drill {
                            select name="limit" class="limit-select" {
                                option value="0" selected { "All" }
                                option value="10" { "10" }
                                option value="20" { "20" }
                                option value="50" { "50" }
                            }
                            input
                                type="submit"
                                value=(format!("Start ({total_due} due)"))
                                class="drill-button btn btn-primary"
                                disabled[total_due == 0];
                        }
                    }
                }
                div.bookmark-bar {
                    a.btn.btn-secondary href=(format!("/collection/{slug}/stats")) {
                        "Stats"
                    }
                    @if bookmark_count > 0 {
                        a.btn.btn-secondary href=(format!("/collection/{slug}/bookmarks")) {
                            "\u{2605} Bookmarks (" (bookmark_count) ")"
                        }
                    }
                    // The review databases live under `data_dir` and are in
                    // nobody's repository; under `[oidc]` this link is the
                    // only way a user can get their own history out.
                    a.btn.btn-secondary href=(format!("/collection/{slug}/export")) {
                        "Export"
                    }
                }
                script {
                    (maud::PreEscaped(BROWSE_SCRIPT))
                }
            }
        }
    })
}

/// A leaf topic links to the file it lives in, opened in the in-app editor.
/// The target is a path we construct, so it needs no scheme check, and it
/// opens in the same tab.
fn render_deck_node(
    node: &DeckNode,
    depth: usize,
    collection_name: &str,
    edit_paths: &HashMap<String, PathBuf>,
) -> Markup {
    let total = node.total_cards_recursive();
    let due = node.due_today_recursive();
    let has_children = !node.children.is_empty();
    // A parent aggregates several files; there is no one file to open.
    let edit_url = if has_children {
        None
    } else {
        edit_paths.get(&node.path).map(|rel| {
            // `/`-separated, like every other path the file manager takes.
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            format!(
                "/files/edit/{}",
                encoded_path(&format!("{collection_name}/{rel}"))
            )
        })
    };
    html! {
        div.deck-node {
            div.deck-row style=(format!("padding-left: {}px", depth * 24)) {
                @if has_children {
                    span.toggle-children onclick="toggleChildren(this)" { "\u{25bc}" }
                } @else {
                    span.toggle-placeholder {}
                }
                label.deck-label {
                    @if has_children {
                        input
                            type="checkbox"
                            checked[due > 0]
                            data-parent
                            onchange="onCheckboxChange(this)";
                    } @else {
                        input
                            type="checkbox"
                            name="decks"
                            value=(node.path)
                            checked[due > 0]
                            onchange="onCheckboxChange(this)";
                    }
                    span.deck-name { (node.name) }
                }
                span.deck-counts {
                    // One `class` attribute: a second is emitted verbatim and
                    // the browser keeps only the first, so a topic with
                    // nothing due never dimmed.
                    span class=(if due == 0 { "deck-due muted" } else { "deck-due" }) { (due) }
                    " / "
                    span.deck-total { (total) }
                }
                @if let Some(url) = edit_url {
                    a.edit-link href=(url) { "Edit" }
                }
            }
            @if has_children {
                div.deck-children {
                    @for child in &node.children {
                        (render_deck_node(child, depth + 1, collection_name, edit_paths))
                    }
                }
            }
        }
    }
}

const BROWSE_SCRIPT: &str = r#"
function selectAll(checked) {
    document.querySelectorAll('.deck-tree input[type="checkbox"]').forEach(function(cb) {
        cb.checked = checked;
    });
    updateDrillButton();
}

function toggleChildren(el) {
    var children = el.closest('.deck-node').querySelector('.deck-children');
    if (children) {
        var collapsed = children.style.display === 'none';
        children.style.display = collapsed ? '' : 'none';
        el.textContent = collapsed ? '\u25bc' : '\u25b6';
    }
}

function onCheckboxChange(cb) {
    // If this is a parent checkbox, toggle all children
    if (cb.hasAttribute('data-parent')) {
        var children = cb.closest('.deck-node').querySelector('.deck-children');
        if (children) {
            children.querySelectorAll('input[type="checkbox"]').forEach(function(child) {
                child.checked = cb.checked;
            });
        }
    }
    // Update parent checkboxes
    updateParentCheckboxes(cb);
    updateDrillButton();
}

function updateParentCheckboxes(cb) {
    var parentNode = cb.closest('.deck-node').parentElement;
    if (!parentNode || !parentNode.classList.contains('deck-children')) return;
    var grandparent = parentNode.closest('.deck-node');
    if (!grandparent) return;
    var parentCb = grandparent.querySelector(':scope > .deck-row input[type="checkbox"]');
    if (!parentCb) return;
    var siblings = parentNode.querySelectorAll(':scope > .deck-node > .deck-row input[type="checkbox"]');
    var allChecked = true;
    var anyChecked = false;
    siblings.forEach(function(s) {
        if (s.checked) anyChecked = true;
        else allChecked = false;
    });
    parentCb.checked = anyChecked;
    parentCb.indeterminate = anyChecked && !allChecked;
    updateParentCheckboxes(parentCb);
}

function updateDrillButton() {
    var btn = document.querySelector('.drill-button');
    if (!btn) return;
    // Count due cards from checked leaf checkboxes
    var checked = document.querySelectorAll('.deck-tree input[type="checkbox"]:checked:not([data-parent])');
    var totalDue = 0;
    checked.forEach(function(cb) {
        var row = cb.closest('.deck-row');
        var dueEl = row.querySelector('.deck-due');
        if (dueEl) totalDue += parseInt(dueEl.textContent) || 0;
    });
    btn.value = 'Start (' + totalDue + ' due)';
    btn.disabled = totalDue === 0;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::create_tmp_directory;
    use crate::types::collection_id::CollectionId;
    use crate::user_db::UserDatabase;

    /// A one-collection view on a database of its own, which is all these
    /// tests need: they are about what the page renders, not about scoping.
    fn open_test_db(path: &Path) -> Fallible<Database> {
        Ok(UserDatabase::open(path)?.collection(CollectionId::new("test-collection")?))
    }

    /// The link points at the file the topic's cards actually live in, not
    /// at its name: a file's frontmatter `name:` renames the topic without
    /// moving the file, and a link built from the name would 404.
    #[test]
    fn a_topic_links_to_the_file_its_cards_live_in() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        std::fs::create_dir_all(dir.join("grammar"))?;
        std::fs::write(
            dir.join("grammar").join("particles.md"),
            "---\nname = \"Little words\"\n---\n\nQ: wa\nA: topic marker\n",
        )?;
        let browse = build_deck_tree(&dir, open_test_db(&dir.join("test.db"))?)?;
        let html = render_browse_page("My Cards", "My-Cards", &browse, 0, 0, None).into_string();
        assert!(
            html.contains("/files/edit/My%20Cards/grammar/particles.md"),
            "got: {html}"
        );
        Ok(())
    }

    /// Byte-identical cards are silently deduplicated at load time, so one
    /// copy's review history is the one that counts. The drill CLI warned
    /// about that on stderr; with the CLI gone the browse page is the only
    /// place a user can be told.
    #[test]
    fn browse_page_reports_duplicate_cards() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        std::fs::write(dir.join("One.md"), "Q: Same?\nA: Yes.\n")?;
        std::fs::write(dir.join("Two.md"), "Q: Same?\nA: Yes.\n")?;
        let browse = build_deck_tree(&dir, open_test_db(&dir.join("hashcards.db"))?)?;
        assert_eq!(
            browse.duplicates.len(),
            1,
            "the two identical cards must be reported as one duplicate"
        );

        let html = render_browse_page("Coll", "coll", &browse, 0, 0, None).into_string();
        assert!(
            html.contains("duplicate card"),
            "the duplicate must be named on the page: {html}"
        );
        assert!(
            html.contains("One.md") && html.contains("Two.md"),
            "both locations must be named: {html}"
        );
        // The locations are relative to the collection. Under `[oidc]` the
        // reader has no filesystem access, and the server's directory layout
        // is not theirs to see.
        let collection_dir = dir.display().to_string();
        assert!(
            !html.contains(&collection_dir),
            "the server's path to the collection must not be shown: {html}"
        );
        Ok(())
    }

    /// A topic with nothing due is dimmed. It used to be rendered with two
    /// `class` attributes, of which a browser keeps only the first, so the
    /// muted style never reached the page.
    #[test]
    fn browse_page_dims_a_topic_with_nothing_due() {
        let tree = DeckNode {
            name: String::new(),
            path: String::new(),
            total_cards: 0,
            due_today: 0,
            children: vec![DeckNode {
                name: "quiet".to_string(),
                path: "quiet".to_string(),
                total_cards: 4,
                due_today: 0,
                children: vec![],
            }],
        };
        let browse = BrowseData {
            tree,
            duplicates: Vec::new(),
            coll_dir: PathBuf::new(),
            edit_paths: HashMap::new(),
        };
        let html = render_browse_page("Coll", "coll", &browse, 0, 0, None).into_string();
        assert!(
            html.contains(r#"class="deck-due muted""#),
            "a topic with nothing due must be dimmed: {html}"
        );
        assert!(
            !html.contains(r#"class="deck-due" class="#),
            "one class attribute, not two: {html}"
        );
    }

    /// One duplicate is one card, and the sentence has to say so.
    #[test]
    fn test_duplicates_summary_is_singular_for_one() {
        let summary = duplicates_summary(1);
        assert!(
            summary.starts_with("1 card in this collection is"),
            "got: {summary}"
        );
        let summary = duplicates_summary(2);
        assert!(
            summary.starts_with("2 cards in this collection are"),
            "got: {summary}"
        );
    }

    /// A collection with no duplicates shows no warning at all.
    #[test]
    fn browse_page_omits_the_duplicate_warning_when_there_are_none() {
        let tree = DeckNode {
            name: String::new(),
            path: String::new(),
            total_cards: 0,
            due_today: 0,
            children: vec![],
        };
        let browse = BrowseData {
            tree,
            duplicates: Vec::new(),
            coll_dir: PathBuf::new(),
            edit_paths: HashMap::new(),
        };
        let html = render_browse_page("Coll", "coll", &browse, 0, 0, None).into_string();
        assert!(!html.contains("duplicate"), "html: {html}");
    }
}
