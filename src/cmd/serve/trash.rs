//! The trash: where deleting puts things instead of destroying them.
//!
//! One per user tree, at `{data_dir}/trash/{tree}/`. Each entry is a
//! directory holding the removed bytes under a fixed name and a
//! `manifest.toml` saying what they were and where they came from.
//!
//! Deleting a collection leaves its review rows alone. A card hash is a
//! content address, so a restored folder addresses its own rows again and
//! the whole review history comes back with nothing to replay; until then
//! they are orphans, which every read path already ignores. Emptying the
//! trash is what erases them, and it is the only thing in hashcards that
//! destroys anything.

use std::cmp::Reverse;
use std::fs::read_dir;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::cmd::serve::cards::CardRoot;
use crate::cmd::serve::config::slugify;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::collection_id::CollectionId;
use crate::types::timestamp::Timestamp;
use crate::utils::ensure_dir;

/// What the trash holds inside one entry, whatever it was.
///
/// A fixed name rather than the original one: a path that survived
/// `CardRoot::resolve_entry` is still an arbitrary file name, and putting
/// it back on disk under a name we chose means restoring never has to trust
/// it a second time. `manifest.toml` records what it was called.
const CONTENT: &str = "content";

const MANIFEST: &str = "manifest.toml";

/// What was deleted. The distinction matters at restore time: a collection
/// brings its `.hashcards.toml`, and so its id, back with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrashKind {
    File,
    Folder,
    Collection,
}

impl TrashKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrashKind::File => "file",
            TrashKind::Folder => "folder",
            TrashKind::Collection => "collection",
        }
    }

    pub fn parse(raw: &str) -> Fallible<Self> {
        match raw {
            "file" => Ok(TrashKind::File),
            "folder" => Ok(TrashKind::Folder),
            "collection" => Ok(TrashKind::Collection),
            other => fail(format!("A trashed item cannot be a `{other}`.")),
        }
    }
}

/// One entry's directory name, `{timestamp}-{slug}`.
///
/// It arrives from a form, so it is parsed rather than trusted: it names a
/// directory, and a value with a separator or a `..` in it would name one
/// outside the trash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrashId {
    inner: String,
}

impl TrashId {
    pub fn parse(raw: &str) -> Fallible<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return fail("No trash entry was named.");
        }
        let ok = trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !ok || trimmed.starts_with('.') {
            return fail(format!("`{trimmed}` is not a trash entry."));
        }
        Ok(Self {
            inner: trimmed.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.inner
    }
}

impl std::fmt::Display for TrashId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)
    }
}

/// One thing in the trash, as the page and the MCP both see it.
pub struct TrashEntry {
    pub id: TrashId,
    pub kind: TrashKind,
    pub original_path: String,
    pub deleted_at: Timestamp,
    pub collection_id: Option<CollectionId>,
}

/// `manifest.toml`, on disk.
#[derive(Serialize, Deserialize)]
struct Manifest {
    kind: String,
    original_path: String,
    deleted_at: String,
    collection_id: Option<String>,
}

/// One user's trash: `{data_dir}/trash/{tree}/`.
pub fn tree_trash_dir(data_dir: &Path, tree: &str) -> PathBuf {
    data_dir.join("trash").join(tree)
}

/// One entry inside it.
pub fn entry_dir(data_dir: &Path, tree: &str, id: &TrashId) -> PathBuf {
    tree_trash_dir(data_dir, tree).join(id.as_str())
}

/// Move `rel` out of the user's tree and into their trash.
///
/// The bytes move; nothing is copied and nothing is read, so a collection
/// of any size costs one rename where the filesystem allows it.
///
/// **Review rows are deliberately left alone.** A card hash is a content
/// address, so a restored folder addresses its own rows again and its
/// history comes back for free. Until then the rows are orphans, which
/// every read path already ignores. Emptying the trash is what collects
/// them.
pub fn move_to_trash(
    data_dir: &Path,
    root: &CardRoot,
    rel: &str,
    kind: TrashKind,
    collection_id: Option<CollectionId>,
    now: Timestamp,
) -> Fallible<TrashId> {
    let entry = root.resolve_entry(rel)?;
    if !entry.path.exists() {
        return fail(format!("`{}` does not exist.", entry.rel));
    }
    let tree = root.tree_name()?;
    let dir = tree_trash_dir(data_dir, tree);
    ensure_dir(&dir, "trash directory")?;

    let id = allocate_id(&dir, &entry.rel, now)?;
    let target = dir.join(id.as_str());
    ensure_dir(&target, "trash entry")?;

    // The manifest is written *after* the bytes arrive: an entry with no
    // manifest is skipped by `list_trash`, so a crash between the two
    // leaves something inert rather than something that claims to hold
    // what it does not.
    move_path(&entry.path, &target.join(CONTENT))?;

    let manifest = Manifest {
        kind: kind.as_str().to_string(),
        original_path: entry.rel.clone(),
        deleted_at: now.to_string(),
        collection_id: collection_id.map(|c| c.to_string()),
    };
    let toml = toml::to_string_pretty(&manifest)
        .map_err(|e| ErrorReport::new(format!("Could not record the deletion: {e}")))?;
    std::fs::write(target.join(MANIFEST), toml)?;
    Ok(id)
}

/// A directory name nothing else has taken.
///
/// The timestamp has one-second resolution, so two deletions of the same
/// path in the same second would otherwise land on each other and the
/// second would destroy the first -- which is the one thing the trash
/// exists to prevent.
fn allocate_id(dir: &Path, rel: &str, now: Timestamp) -> Fallible<TrashId> {
    let stamp = now.into_inner().format("%Y%m%dT%H%M%S");
    let slug = slugify(rel);
    for n in 0..1000 {
        let candidate = if n == 0 {
            format!("{stamp}-{slug}")
        } else {
            format!("{stamp}-{slug}-{n}")
        };
        if !dir.join(&candidate).exists() {
            return TrashId::parse(&candidate);
        }
    }
    fail("Too many things were deleted at once. Try again in a moment.")
}

/// Rename where the filesystem allows it, copy and remove where it does not
/// -- `data_dir` and the trash are normally the same device, but a bind
/// mount or a container volume can put them on two.
fn move_path(from: &Path, to: &Path) -> Fallible<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        copy_dir(from, to)?;
        std::fs::remove_dir_all(from)?;
    } else {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Fallible<()> {
    ensure_dir(to, "trash entry")?;
    for entry in read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

/// Everything in one user's trash, newest first.
///
/// An entry with no readable manifest is skipped and logged rather than
/// failing the listing: one damaged directory must not hide the rest of
/// somebody's trash from them.
pub fn list_trash(data_dir: &Path, tree: &str) -> Fallible<Vec<TrashEntry>> {
    let dir = tree_trash_dir(data_dir, tree);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(id) = TrashId::parse(&name) else {
            continue;
        };
        match read_entry(&entry.path(), id) {
            Ok(e) => out.push(e),
            Err(e) => log::warn!("Skipping unreadable trash entry `{name}`: {e}"),
        }
    }
    // Newest first. The timestamp's format sorts lexicographically because
    // it is written most-significant-first; the id breaks a tie between two
    // things deleted in the same second, so the order is stable.
    out.sort_by_key(|e| Reverse((e.deleted_at.to_string(), e.id.as_str().to_string())));
    Ok(out)
}

fn read_entry(dir: &Path, id: TrashId) -> Fallible<TrashEntry> {
    let raw = std::fs::read_to_string(dir.join(MANIFEST))?;
    let manifest: Manifest = toml::from_str(&raw)
        .map_err(|e| ErrorReport::new(format!("its manifest does not parse: {e}")))?;
    Ok(TrashEntry {
        id,
        kind: TrashKind::parse(&manifest.kind)?,
        original_path: manifest.original_path,
        deleted_at: Timestamp::try_from(manifest.deleted_at)?,
        collection_id: match manifest.collection_id {
            Some(c) => Some(CollectionId::new(c)?),
            None => None,
        },
    })
}

/// Put a trashed entry back where it came from, and return the path it
/// went to.
///
/// The original path is re-resolved through `CardRoot::resolve_entry`
/// rather than joined raw: it has been sitting on disk in a file somebody
/// could have edited, so it is checked exactly as hard as a path arriving
/// from a browser.
pub fn restore_from_trash(data_dir: &Path, root: &CardRoot, id: &TrashId) -> Fallible<String> {
    let tree = root.tree_name()?;
    let dir = entry_dir(data_dir, tree, id);
    if !dir.is_dir() {
        return fail("That item is not in the trash any more.");
    }
    let entry = read_entry(&dir, id.clone())?;
    let target = root.resolve_entry(&entry.original_path)?;
    if target.path.exists() {
        return fail(format!(
            "`{}` already exists, so the deleted copy was left in the trash. Rename or move \
             what is there now, then restore again.",
            target.rel
        ));
    }
    // The collection this belonged to may itself have been deleted since.
    if let Some(parent) = target.path.parent() {
        ensure_dir(parent, "card folder")?;
    }
    move_path(&dir.join(CONTENT), &target.path)?;
    std::fs::remove_dir_all(&dir)?;
    Ok(target.rel)
}

/// Destroy one trashed entry.
///
/// The only thing in hashcards that destroys anything. Returns the
/// collection whose review rows are now unreachable and must be erased by
/// the caller -- which cannot happen here, because the trash knows nothing
/// about databases.
pub fn purge_entry(data_dir: &Path, tree: &str, id: &TrashId) -> Fallible<Option<CollectionId>> {
    let dir = entry_dir(data_dir, tree, id);
    if !dir.is_dir() {
        return fail("That item is not in the trash any more.");
    }
    let entry = read_entry(&dir, id.clone())?;
    std::fs::remove_dir_all(&dir)?;
    Ok(entry.collection_id)
}

/// Destroy everything in one user's trash, and name every collection whose
/// rows the caller must now erase.
pub fn purge_all(data_dir: &Path, tree: &str) -> Fallible<Vec<CollectionId>> {
    let mut erased = Vec::new();
    for entry in list_trash(data_dir, tree)? {
        if let Some(id) = purge_entry(data_dir, tree, &entry.id)? {
            erased.push(id);
        }
    }
    Ok(erased)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A data directory with one user tree holding `Spanish/verbs.md`.
    fn fixture() -> Fallible<(TempDir, PathBuf, CardRoot)> {
        let dir = TempDir::new()?;
        let data_dir = dir.path().to_path_buf();
        let root = CardRoot::for_user(&data_dir, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        std::fs::write(
            root.path().join("Spanish/verbs.md"),
            "Q: hablar\nA: to speak\n",
        )?;
        Ok((dir, data_dir, root))
    }

    #[test]
    fn a_trashed_file_leaves_the_tree_and_arrives_in_the_trash() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        assert!(!root.path().join("Spanish/verbs.md").exists());
        let entry = entry_dir(&data_dir, "default", &id);
        assert!(entry.join("manifest.toml").is_file());
        assert_eq!(
            std::fs::read_to_string(entry.join("content"))?,
            "Q: hablar\nA: to speak\n"
        );
        Ok(())
    }

    #[test]
    fn a_trashed_folder_keeps_its_contents() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Folder,
            None,
            Timestamp::now(),
        )?;
        let content = entry_dir(&data_dir, "default", &id).join("content");
        assert!(content.join("verbs.md").is_file());
        Ok(())
    }

    #[test]
    fn the_manifest_records_what_was_deleted_and_from_where() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Collection,
            Some(CollectionId::new("abc12345")?),
            Timestamp::now(),
        )?;
        let entries = list_trash(&data_dir, "default")?;
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.id, id);
        assert_eq!(entry.original_path, "Spanish");
        assert!(matches!(entry.kind, TrashKind::Collection));
        assert_eq!(
            entry.collection_id.as_ref().map(|c| c.as_str()),
            Some("abc12345")
        );
        Ok(())
    }

    /// Two deletions of the same path in the same second must not collide.
    #[test]
    fn deleting_the_same_path_twice_makes_two_entries() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let now = Timestamp::now();
        let first = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            now,
        )?;
        std::fs::write(
            root.path().join("Spanish/verbs.md"),
            "Q: comer\nA: to eat\n",
        )?;
        let second = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            now,
        )?;
        assert_ne!(first, second);
        assert_eq!(list_trash(&data_dir, "default")?.len(), 2);
        Ok(())
    }

    /// One user's trash is not another's, keyed the way `cards/` and `db/`
    /// already are.
    #[test]
    fn each_tree_has_its_own_trash() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Folder,
            None,
            Timestamp::now(),
        )?;
        let other = CardRoot::for_user(&data_dir, Some("you@example.com"))?;
        let other_tree = other.tree_name()?.to_string();
        assert!(list_trash(&data_dir, &other_tree)?.is_empty());
        assert_eq!(list_trash(&data_dir, "default")?.len(), 1);
        Ok(())
    }

    #[test]
    fn an_empty_trash_lists_nothing() -> Fallible<()> {
        let (_dir, data_dir, _root) = fixture()?;
        assert!(list_trash(&data_dir, "default")?.is_empty());
        Ok(())
    }

    #[test]
    fn trashing_something_that_is_not_there_is_refused() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let err = move_to_trash(
            &data_dir,
            &root,
            "Spanish/nope.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )
        .unwrap_err();
        assert!(err.message().contains("nope.md"), "{}", err.message());
        Ok(())
    }

    /// A trash id comes back from a form, so it is checked before it is
    /// used to build a path.
    #[test]
    fn a_trash_id_cannot_escape_the_trash_directory() {
        assert!(TrashId::parse("../../etc").is_err());
        assert!(TrashId::parse("a/b").is_err());
        assert!(TrashId::parse("").is_err());
        assert!(TrashId::parse("20260909T120000-Spanish").is_ok());
    }

    #[test]
    fn a_restored_file_comes_back_where_it_was() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        let rel = restore_from_trash(&data_dir, &root, &id)?;
        assert_eq!(rel, "Spanish/verbs.md");
        assert_eq!(
            std::fs::read_to_string(root.path().join("Spanish/verbs.md"))?,
            "Q: hablar\nA: to speak\n"
        );
        assert!(list_trash(&data_dir, "default")?.is_empty());
        Ok(())
    }

    #[test]
    fn a_restored_collection_brings_its_id_back_with_it() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        std::fs::write(
            root.path().join("Spanish/.hashcards.toml"),
            "id = \"abc12345\"\n",
        )?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Collection,
            Some(CollectionId::new("abc12345")?),
            Timestamp::now(),
        )?;
        restore_from_trash(&data_dir, &root, &id)?;
        let meta = std::fs::read_to_string(root.path().join("Spanish/.hashcards.toml"))?;
        assert!(meta.contains("abc12345"), "{meta}");
        Ok(())
    }

    /// The restore must not overwrite whatever took the name in the
    /// meantime -- that would delete something without trashing it.
    #[test]
    fn restoring_onto_an_occupied_path_is_refused() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        std::fs::write(root.path().join("Spanish/verbs.md"), "something else\n")?;
        let err = restore_from_trash(&data_dir, &root, &id).unwrap_err();
        assert!(err.message().contains("already"), "{}", err.message());
        assert_eq!(
            std::fs::read_to_string(root.path().join("Spanish/verbs.md"))?,
            "something else\n"
        );
        assert_eq!(list_trash(&data_dir, "default")?.len(), 1);
        Ok(())
    }

    /// The parent may have gone too -- restoring a deck into a collection
    /// that was itself deleted has to recreate the folder.
    #[test]
    fn restoring_recreates_a_missing_parent() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        std::fs::remove_dir_all(root.path().join("Spanish"))?;
        restore_from_trash(&data_dir, &root, &id)?;
        assert!(root.path().join("Spanish/verbs.md").is_file());
        Ok(())
    }

    #[test]
    fn restoring_something_that_is_not_there_is_refused() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = TrashId::parse("20260909T120000-nothing")?;
        assert!(restore_from_trash(&data_dir, &root, &id).is_err());
        Ok(())
    }

    #[test]
    fn purging_removes_the_bytes_and_names_the_collection() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Collection,
            Some(CollectionId::new("abc12345")?),
            Timestamp::now(),
        )?;
        let erased = purge_entry(&data_dir, "default", &id)?;
        assert_eq!(erased.as_ref().map(|c| c.as_str()), Some("abc12345"));
        assert!(!entry_dir(&data_dir, "default", &id).exists());
        assert!(list_trash(&data_dir, "default")?.is_empty());
        Ok(())
    }

    /// A trashed deck has no rows of its own to erase: its cards belong to
    /// the collection, which is still there.
    #[test]
    fn purging_a_deck_names_no_collection() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        assert_eq!(purge_entry(&data_dir, "default", &id)?, None);
        Ok(())
    }

    #[test]
    fn emptying_the_trash_names_every_collection_it_held() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        std::fs::create_dir_all(root.path().join("German"))?;
        move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Collection,
            Some(CollectionId::new("abc12345")?),
            Timestamp::now(),
        )?;
        move_to_trash(
            &data_dir,
            &root,
            "German",
            TrashKind::Collection,
            Some(CollectionId::new("def67890")?),
            Timestamp::now(),
        )?;
        let mut erased: Vec<String> = purge_all(&data_dir, "default")?
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        erased.sort();
        assert_eq!(erased, vec!["abc12345", "def67890"]);
        assert!(list_trash(&data_dir, "default")?.is_empty());
        Ok(())
    }
}
