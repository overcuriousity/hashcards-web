use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::config::SchedulingOverrides;
use crate::cmd::serve::config::slugify;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::collection_id::CollectionId;
use crate::types::performance::DesiredRetention;
use crate::types::performance::MaxInterval;
use crate::utils::ensure_dir;

/// One user's markdown tree, at `{data_dir}/cards/{user}`.
///
/// Every collection lives in one of these: a collection is a top-level
/// folder here, discovered by reading the directory rather than declared in
/// the config file, and owned by whoever the tree belongs to.
pub struct CardRoot {
    root: PathBuf,
}

impl CardRoot {
    /// The tree belonging to `owner`, creating it if absent. `None` is the
    /// shared `default` tree used when `[oidc]` is not configured.
    pub fn for_user(data_dir: &Path, owner: Option<&str>) -> Fallible<Self> {
        let root = root_path(data_dir, owner)?;
        ensure_dir(&root, "local card directory")?;
        Ok(Self { root })
    }

    /// The tree belonging to `owner`, without creating it. Read paths use
    /// this: serving a page must not materialize directories.
    pub fn open(data_dir: &Path, owner: Option<&str>) -> Fallible<Self> {
        Ok(Self {
            root: root_path(data_dir, owner)?,
        })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Resolve a client-supplied relative path inside this tree.
    ///
    /// Unlike `MediaLoader::validate`, the final component need not exist —
    /// the file manager calls this to create files and folders. The deepest
    /// ancestor that *does* exist must canonicalize to somewhere inside the
    /// root, so a symlinked directory cannot be used to escape.
    pub fn resolve(&self, rel: &str) -> Fallible<PathBuf> {
        Ok(self.resolve_entry(rel)?.path)
    }

    /// The same, keeping the normalized relative path alongside it.
    ///
    /// Callers that both touch the file and reason about *where* it sits —
    /// which collection owns it, whether it is a collection root — must ask
    /// both questions of the same string. Splitting the raw request path
    /// instead answers them differently: `./Spanish` resolves to the
    /// collection folder while a raw split calls it nested, so the folder
    /// would be deleted without its review database going with it.
    pub fn resolve_entry(&self, rel: &str) -> Fallible<ResolvedEntry> {
        let trimmed = rel.trim();
        if trimmed.is_empty() {
            return fail("No file path was given.");
        }
        let rel_path = PathBuf::from(trimmed);
        if rel_path.is_absolute() || rel_path.has_root() {
            return fail(format!(
                "Path must be relative to your card folder: `{trimmed}`"
            ));
        }
        // `.` components are dropped rather than refused: they name the same
        // place, and normalizing here is what keeps every later answer about
        // this path consistent.
        let mut parts: Vec<String> = Vec::new();
        for component in rel_path.components() {
            match component {
                Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
                Component::CurDir => continue,
                Component::ParentDir => {
                    return fail(format!(
                        "Path must not contain `..` components: `{trimmed}`"
                    ));
                }
                _ => {
                    return fail(format!(
                        "Path must be relative to your card folder: `{trimmed}`"
                    ));
                }
            }
        }
        // Nothing addresses the root by path — every caller wants a file or
        // folder inside it — and one that did would let the file manager
        // delete the whole tree.
        if parts.is_empty() {
            return fail(format!(
                "Path must name something inside your card folder: `{trimmed}`"
            ));
        }
        let normalized = parts.join("/");
        let joined = self.root.join(&normalized);

        // Walk up to the deepest ancestor that exists and canonicalize that:
        // the leaf may legitimately be missing.
        let mut existing = joined.as_path();
        while !existing.exists() {
            existing = match existing.parent() {
                Some(p) => p,
                None => return fail(format!("Path is outside your card folder: `{trimmed}`")),
            };
        }
        if existing.is_symlink() {
            return fail(format!("Path goes through a symbolic link: `{trimmed}`"));
        }
        let canonical_root = self.root.canonicalize()?;
        let canonical = existing.canonicalize()?;
        if !canonical.starts_with(&canonical_root) {
            return fail(format!("Path is outside your card folder: `{trimmed}`"));
        }
        Ok(ResolvedEntry {
            rel: normalized,
            path: joined,
        })
    }
}

/// A checked client-supplied path: where it is on disk, and its relative
/// path reduced to plain `/`-separated components.
pub struct ResolvedEntry {
    /// Normalized, so `./Spanish/verbs.md` and `Spanish/verbs.md` are the
    /// same string. Never empty, and never starts or ends with `/`.
    pub rel: String,
    pub path: PathBuf,
}

/// Where `owner`'s tree lives, whether or not it exists yet.
///
/// The folder is named for the email, and then disambiguated by eight hex
/// characters of its hash. The slug alone is not injective — `slugify` maps
/// every character outside `[A-Za-z0-9._-]` to `-`, so `me+work@example.com`
/// and `me-work@example.com` name the same folder — and which tree a
/// collection sits in *is* who owns it: two identities sharing a tree would
/// share every collection in it, every file in the file manager, and every
/// review database. The slug is kept in front of the hash so that the
/// directory is still recognisable to whoever administers the server.
fn root_path(data_dir: &Path, owner: Option<&str>) -> Fallible<PathBuf> {
    let who = match owner {
        Some(email) => {
            let email = email.trim().to_lowercase();
            if email.is_empty() {
                return fail("Cannot open a local card folder: the owner name is empty.");
            }
            let digest = blake3::hash(email.as_bytes()).to_hex();
            format!("{}-{}", slugify(&email), &digest[..8])
        }
        None => "default".to_string(),
    };
    Ok(data_dir.join("cards").join(who))
}

/// Per-collection metadata file. Skipped by the parser (it is not `.md`)
/// and hidden from the file manager.
pub const COLLECTION_META_FILE: &str = ".hashcards.toml";

#[derive(Deserialize, Serialize)]
struct CollectionMeta {
    id: String,
    /// Scheduling this collection asks for. Optional, and skipped when
    /// absent so that the file the server writes for itself stays one line.
    ///
    /// Deliberately untyped: a `f64` here makes `desired_retention = "0.95"`
    /// a *parse* error, and the parse is what `existing_collection_id` reads
    /// the id out of. One mistyped preference would then take the whole
    /// collection off the landing page along with its review history — the
    /// exact outcome the leniency below exists to avoid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    desired_retention: Option<toml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_interval_days: Option<toml::Value>,
}

/// The scheduling a collection asks for in its own `.hashcards.toml`.
///
/// A file that cannot be read, a value of the wrong type, or a value out of
/// range all yield no override rather than an error: the number is a
/// preference, and losing a whole collection from the landing page over one
/// is a worse answer than scheduling it the way every other collection is
/// scheduled.
pub fn collection_overrides(folder: &Path) -> SchedulingOverrides {
    let meta_path = folder.join(COLLECTION_META_FILE);
    let Ok(text) = read_to_string(&meta_path) else {
        return SchedulingOverrides::default();
    };
    let Ok(meta) = toml::from_str::<CollectionMeta>(&text) else {
        return SchedulingOverrides::default();
    };
    let complain = |what: &str, e: &ErrorReport| {
        log::warn!("Ignoring `{what}` in {}: {e}", meta_path.display());
    };
    SchedulingOverrides {
        retention: meta
            .desired_retention
            .and_then(|v| override_number(&v, "desired_retention", &meta_path))
            .and_then(|v| {
                DesiredRetention::new(v)
                    .inspect_err(|e| complain("desired_retention", e))
                    .ok()
            }),
        max_interval: meta
            .max_interval_days
            .and_then(|v| override_number(&v, "max_interval_days", &meta_path))
            .and_then(|v| {
                MaxInterval::new(v)
                    .inspect_err(|e| complain("max_interval_days", e))
                    .ok()
            }),
    }
}

/// The number an override was written as, or `None` with a warning when it
/// was not written as a number at all. TOML tells integers and floats apart
/// and `max_interval_days = 256` is the natural way to write a whole number
/// of days, so both are numbers here.
fn override_number(value: &toml::Value, what: &str, meta_path: &Path) -> Option<f64> {
    match value {
        toml::Value::Float(f) => Some(*f),
        toml::Value::Integer(i) => Some(*i as f64),
        other => {
            log::warn!(
                "Ignoring `{what}` in {}: expected a number, found {}.",
                meta_path.display(),
                other.type_str()
            );
            None
        }
    }
}

/// The stable id of a local collection folder, creating it on first sight.
///
/// Review databases are named from this id rather than from the folder's
/// slug, so renaming a folder keeps its review history. Ids are derived from
/// the clock and the process id rather than from the name, precisely so that
/// a rename cannot change them.
pub fn collection_id(folder: &Path) -> Fallible<CollectionId> {
    if let Some(id) = existing_collection_id(folder)? {
        return Ok(id);
    }
    let meta_path = folder.join(COLLECTION_META_FILE);
    let id = fresh_id(folder)?;
    let meta = CollectionMeta {
        id: id.clone(),
        desired_retention: None,
        max_interval_days: None,
    };
    write(&meta_path, toml::to_string(&meta)?)?;
    CollectionId::new(id)
}

/// Eight hex characters derived from the clock, the process id and the
/// folder path. Not a hash of the name alone: renaming must not change it.
fn fresh_id(folder: &Path) -> Fallible<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| ErrorReport::new(format!("System clock is before 1970: {e}")))?
        .as_nanos();
    let seed = format!("{nanos}:{}:{}", std::process::id(), folder.display());
    Ok(blake3::hash(seed.as_bytes()).to_hex()[..8].to_string())
}

/// Whether discovery may give a folder that has none a stable id.
///
/// Read paths pass `ExistingOnly`: serving a page must not write into the
/// user's tree. Pages that exist to show the tree (`/files`, the landing
/// page) pass `CreateMissing`, off the async executor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IdPolicy {
    CreateMissing,
    ExistingOnly,
}

/// The id already recorded for `folder`, if any. Never writes.
///
/// A file TOML itself cannot parse is not given up on until the id has been
/// looked for by eye: the id names the review database, so failing here
/// costs the collection every review it has ever recorded, and the file is
/// one a user is invited to hand-edit. Only a file with no recognisable id
/// at all is an error — and it has to be, because the alternative is minting
/// a fresh id and writing over whatever the user was in the middle of.
pub fn existing_collection_id(folder: &Path) -> Fallible<Option<CollectionId>> {
    let meta_path = folder.join(COLLECTION_META_FILE);
    if !meta_path.exists() {
        return Ok(None);
    }
    let text = read_to_string(&meta_path)?;
    let meta: CollectionMeta = match toml::from_str(&text) {
        Ok(meta) => meta,
        Err(e) => {
            let Some(id) = salvage_id(&text) else {
                return fail(format!(
                    "{} is not valid TOML ({e}), and no `id` line could be found in it. \
                     Fix the file, or delete it to start the collection's history over.",
                    meta_path.display()
                ));
            };
            log::warn!(
                "{} is not valid TOML ({e}). Reading its id as `{id}` and ignoring \
                 everything else in the file.",
                meta_path.display()
            );
            return Ok(Some(id));
        }
    };
    if meta.id.is_empty() {
        return Ok(None);
    }
    Ok(Some(CollectionId::new(meta.id)?))
}

/// The `id` of a metadata file the TOML parser has rejected.
///
/// The id is always written as one plain `id = "..."` line, so it stays
/// legible however badly the rest of the file has been mangled. Nothing else
/// is recovered this way: the overrides are preferences and can wait for the
/// file to be fixed, whereas the id cannot be guessed again.
fn salvage_id(text: &str) -> Option<CollectionId> {
    text.lines().find_map(|line| {
        let value = line.trim_start().strip_prefix("id")?.trim_start();
        let value = value.strip_prefix('=')?.trim();
        let quote = value.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let id = value[quote.len_utf8()..].split(quote).next()?;
        CollectionId::new(id).ok()
    })
}

/// Every top-level folder in `root`, as a collection.
///
/// Loose files directly under the root are ignored: a collection is a
/// folder, so that a file always belongs to exactly one.
///
/// A folder hashcards cannot make sense of is skipped with a warning rather
/// than failing the whole listing: one malformed `.hashcards.toml` must not
/// make every other collection disappear from the landing page. Folders are
/// visited in name order, so which of two names that slugify alike wins does
/// not depend on the order the filesystem happened to list them in.
pub fn discover_local_collections(
    root: &CardRoot,
    db_dir: &Path,
    owner: Option<&str>,
    policy: IdPolicy,
) -> Fallible<Vec<ResolvedCollection>> {
    let mut collections: Vec<ResolvedCollection> = Vec::new();
    if !root.path().exists() {
        return Ok(collections);
    }
    let db_path = user_db_path(root, db_dir)?;
    let mut names = Vec::new();
    for entry in read_dir(root.path())? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() || path.is_symlink() {
            continue;
        }
        match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if !n.starts_with('.') => names.push(n.to_string()),
            _ => continue,
        }
    }
    names.sort();

    for name in names {
        let path = root.path().join(&name);
        let id = match folder_id(&path, policy) {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("Skipping local collection `{name}`: {e}");
                continue;
            }
        };
        let slug = slugify(&name);
        if let Some(first) = collections.iter().find(|c| c.slug == slug) {
            log::warn!(
                "Skipping local collection `{name}`: it maps to the same URL slug `{slug}` as `{}`.",
                first.name
            );
            continue;
        }
        collections.push(ResolvedCollection {
            slug,
            name,
            overrides: collection_overrides(&path),
            coll_dir: path,
            db_path: db_path.clone(),
            collection_id: id,
            owner: owner.map(|o| o.to_lowercase()),
        });
    }
    Ok(collections)
}

/// The review database of the user whose tree this is.
///
/// One file per tree, named for the tree's own directory — which is
/// `default` or `{email-slug}-{8 hex}`, and so can never collide with a
/// collection id, which is eight hex characters.
pub fn user_db_path(root: &CardRoot, db_dir: &Path) -> Fallible<PathBuf> {
    let name = root
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ErrorReport::new("the card folder has no readable name"))?;
    Ok(db_dir.join(format!("{name}.db")))
}

/// Every collection in every user's tree under `{data_dir}/cards`.
///
/// For startup-time work that must touch each review database once — the
/// dangling-session sweep — and nothing else. `owner` is always `None`: a
/// tree's directory name is a *slug* of an email, which no request can be
/// matched against, so a collection from here must never be routed to.
/// Every read path goes through `find_collection` instead.
///
/// `ExistingOnly`, so startup never writes an id into a user's tree: a
/// folder with no id has no database either, and nothing to sweep.
pub fn discover_all_collections(data_dir: &Path) -> Vec<ResolvedCollection> {
    let trees_dir = data_dir.join("cards");
    let db_dir = data_dir.join("db");
    let entries = match read_dir(&trees_dir) {
        Ok(entries) => entries,
        // No tree yet is the ordinary state of a fresh install.
        Err(_) => return Vec::new(),
    };
    let mut all = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || path.is_symlink() {
            continue;
        }
        let root = CardRoot { root: path };
        match discover_local_collections(&root, &db_dir, None, IdPolicy::ExistingOnly) {
            Ok(found) => all.extend(found),
            Err(e) => log::warn!("Skipping the card tree at {}: {e}", root.path().display()),
        }
    }
    all
}

/// The id of one folder under `policy`. `None` means "no id yet, and this
/// caller may not create one".
fn folder_id(path: &Path, policy: IdPolicy) -> Fallible<Option<CollectionId>> {
    match policy {
        IdPolicy::CreateMissing => Ok(Some(collection_id(path)?)),
        IdPolicy::ExistingOnly => existing_collection_id(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::create_tmp_directory;
    use crate::types::performance::Scheduling;

    /// `create_tmp_directory` returns a `PathBuf`, not a `TempDir`.
    fn fixture() -> Fallible<(PathBuf, CardRoot)> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, Some("Me@Example.com"))?;
        Ok((dir, root))
    }

    /// The normalized path is what every later question about the file is
    /// asked of, so it has to be free of the `.` components a client can
    /// put in: `./Spanish` and `Spanish` name one collection, and answers
    /// derived from splitting the raw string disagreed about that.
    #[test]
    fn resolving_normalizes_away_dot_components() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let plain = root.resolve_entry("Spanish/verbs.md")?;
        for path in [
            "./Spanish/verbs.md",
            "Spanish/./verbs.md",
            "Spanish//verbs.md",
        ] {
            let entry = root.resolve_entry(path)?;
            assert_eq!(entry.rel, plain.rel, "for `{path}`");
            assert_eq!(entry.path, plain.path, "for `{path}`");
        }
        // The root itself still names nothing inside the tree.
        assert!(root.resolve_entry(".").is_err());
        assert!(root.resolve_entry("./").is_err());
        assert!(root.resolve_entry("./..").is_err());
        Ok(())
    }

    #[test]
    fn user_dir_is_slugified_and_lowercased() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let expected = dir
            .join("cards")
            .join(format!("me-example.com-{}", digest_of("me@example.com")));
        assert_eq!(root.path(), expected);
        Ok(())
    }

    /// Two emails that slugify alike must not share a tree. The tree a
    /// collection sits in is who owns it, so a shared one would hand both
    /// identities the same collections, the same files and the same review
    /// databases.
    #[test]
    fn users_whose_emails_slugify_alike_get_separate_trees() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let plus = CardRoot::for_user(&dir, Some("me+work@example.com"))?;
        let dash = CardRoot::for_user(&dir, Some("me-work@example.com"))?;
        assert_ne!(plus.path(), dash.path());
        // Case and surrounding space are still the same identity.
        let upper = CardRoot::for_user(&dir, Some("  Me+Work@Example.com "))?;
        assert_eq!(plus.path(), upper.path());
        Ok(())
    }

    /// The eight hex characters `root_path` appends, for tests that spell
    /// out the expected folder name.
    fn digest_of(email: &str) -> String {
        blake3::hash(email.as_bytes()).to_hex()[..8].to_string()
    }

    #[test]
    fn anonymous_user_gets_the_default_tree() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        assert_eq!(root.path(), dir.join("cards").join("default"));
        Ok(())
    }

    #[test]
    fn resolves_a_path_that_does_not_exist_yet() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let resolved = root.resolve("Spanish/verbs.md")?;
        assert_eq!(resolved, root.path().join("Spanish").join("verbs.md"));
        Ok(())
    }

    #[test]
    fn rejects_parent_components() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        assert!(root.resolve("../escape.md").is_err());
        assert!(root.resolve("Spanish/../../escape.md").is_err());
        Ok(())
    }

    /// The root is not addressable. `delete_entry` resolves the path it is
    /// given and removes it: `.` would have named the whole card folder,
    /// and a `media` collection at the top level does not even count
    /// towards "is it empty", so the tree would be judged empty and wiped.
    #[test]
    fn rejects_the_root_itself() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        assert!(root.resolve(".").is_err());
        assert!(root.resolve("./").is_err());
        assert!(root.resolve("/").is_err());
        assert!(root.resolve("  ").is_err());
        Ok(())
    }

    #[test]
    fn rejects_absolute_paths() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        assert!(root.resolve("/etc/passwd").is_err());
        Ok(())
    }

    #[test]
    fn rejects_empty_paths() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        assert!(root.resolve("").is_err());
        assert!(root.resolve("   ").is_err());
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn rejects_escape_through_a_symlinked_directory() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside)?;
        std::os::unix::fs::symlink(&outside, root.path().join("link"))?;
        assert!(root.resolve("link/evil.md").is_err());
        Ok(())
    }

    #[test]
    fn collection_id_is_created_once_and_then_reused() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;

        let first = collection_id(&folder)?;
        let second = collection_id(&folder)?;
        assert_eq!(first, second);
        assert_eq!(first.as_str().len(), 8);
        assert!(folder.join(COLLECTION_META_FILE).exists());
        Ok(())
    }

    #[test]
    fn collection_id_survives_a_rename() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let before = root.path().join("Spanish");
        std::fs::create_dir_all(&before)?;
        let id = collection_id(&before)?;

        let after = root.path().join("Espanol");
        std::fs::rename(&before, &after)?;

        assert_eq!(collection_id(&after)?, id);
        Ok(())
    }

    #[test]
    fn two_folders_get_different_ids() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let a = root.path().join("A");
        let b = root.path().join("B");
        std::fs::create_dir_all(&a)?;
        std::fs::create_dir_all(&b)?;
        assert_ne!(collection_id(&a)?, collection_id(&b)?);
        Ok(())
    }

    #[test]
    fn discovery_returns_one_collection_per_top_level_folder() -> Fallible<()> {
        let (dir, root) = fixture()?;
        std::fs::create_dir_all(root.path().join("Spanish").join("nested"))?;
        std::fs::create_dir_all(root.path().join("Medicine"))?;
        std::fs::write(root.path().join("loose.md"), "Q: a\nA: b\n")?;

        let db_dir = dir.join("db");
        let mut found = discover_local_collections(
            &root,
            &db_dir,
            Some("me@example.com"),
            IdPolicy::CreateMissing,
        )?;
        found.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "Medicine");
        assert_eq!(found[1].name, "Spanish");
        assert_eq!(found[1].owner, Some("me@example.com".to_string()));
        Ok(())
    }

    /// The database is the *user's*, named for their card tree; which rows
    /// in it belong to this collection is its stable id, not its slug. A
    /// rename therefore changes neither.
    #[test]
    fn discovered_db_path_is_the_users_file_not_the_collections() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        let id = collection_id(&folder)?;

        let db_dir = dir.join("db");
        let found = discover_local_collections(&root, &db_dir, None, IdPolicy::CreateMissing)?;
        assert_eq!(found[0].db_path, user_db_path(&root, &db_dir)?);
        assert_ne!(found[0].db_path, db_dir.join(format!("{id}.db")));
        assert_eq!(found[0].collection_id, id);
        Ok(())
    }

    /// A collection may name its own retention and ceiling; anything it does
    /// not name falls through to the instance's.
    #[test]
    fn discovery_reads_a_collections_scheduling_overrides() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        let id = collection_id(&folder)?;
        std::fs::write(
            folder.join(COLLECTION_META_FILE),
            format!("id = \"{id}\"\ndesired_retention = 0.95\n"),
        )?;

        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        let scheduling = found[0].scheduling(Scheduling::default());
        assert_eq!(scheduling.retention.into_inner(), 0.95);
        assert_eq!(
            scheduling.max_interval.into_inner(),
            MaxInterval::DEFAULT,
            "an unnamed value must fall through to the instance default"
        );
        Ok(())
    }

    /// An override out of range is not worth losing the collection over: the
    /// landing page still lists it and it schedules by the instance default.
    #[test]
    fn an_out_of_range_override_falls_back_to_the_default() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        let id = collection_id(&folder)?;
        std::fs::write(
            folder.join(COLLECTION_META_FILE),
            format!("id = \"{id}\"\ndesired_retention = 2.0\n"),
        )?;

        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        assert_eq!(found.len(), 1, "the collection must still be listed");
        assert_eq!(
            found[0]
                .scheduling(Scheduling::default())
                .retention
                .into_inner(),
            DesiredRetention::DEFAULT
        );
        Ok(())
    }

    /// An override of the wrong *type* is no different from one out of
    /// range: `desired_retention = "0.95"` is a slip a hand-editor makes,
    /// and it used to fail the strict parse in `existing_collection_id` and
    /// take the whole collection — review history and all — off the landing
    /// page.
    #[test]
    fn a_mistyped_override_falls_back_to_the_default() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        let id = collection_id(&folder)?;
        std::fs::write(
            folder.join(COLLECTION_META_FILE),
            format!("id = \"{id}\"\ndesired_retention = \"0.95\"\nmax_interval_days = true\n"),
        )?;

        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        assert_eq!(found.len(), 1, "the collection must still be listed");
        assert_eq!(found[0].collection_id, id);
        let scheduling = found[0].scheduling(Scheduling::default());
        assert_eq!(scheduling.retention.into_inner(), DesiredRetention::DEFAULT);
        assert_eq!(scheduling.max_interval.into_inner(), MaxInterval::DEFAULT);
        Ok(())
    }

    /// An integer is a number: `max_interval_days = 256` must not be read as
    /// a type error now that the value arrives as a `toml::Value`.
    #[test]
    fn an_integer_override_is_read_as_a_number() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        let id = collection_id(&folder)?;
        std::fs::write(
            folder.join(COLLECTION_META_FILE),
            format!("id = \"{id}\"\nmax_interval_days = 256\n"),
        )?;

        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        assert_eq!(
            found[0]
                .scheduling(Scheduling::default())
                .max_interval
                .into_inner(),
            256.0
        );
        Ok(())
    }

    /// A syntax slip anywhere in the file still leaves the id readable by
    /// eye, and the id is what names the review database. Losing it would
    /// lose every review the collection has ever recorded, so it is worth
    /// scanning for by hand once the parser has given up.
    #[test]
    fn a_syntax_error_keeps_the_collection_by_salvaging_its_id() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        let id = collection_id(&folder)?;
        let broken = format!("id = \"{id}\"\ndesired retention = 0.95\n");
        std::fs::write(folder.join(COLLECTION_META_FILE), &broken)?;

        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        assert_eq!(found.len(), 1, "the collection must still be listed");
        assert_eq!(
            found[0].collection_id, id,
            "the salvaged id must still scope the same review rows"
        );
        assert_eq!(
            std::fs::read_to_string(folder.join(COLLECTION_META_FILE))?,
            broken,
            "the user's file must not be rewritten under them"
        );
        Ok(())
    }

    /// The metadata file the server writes for itself holds the id and
    /// nothing else: an override it did not put there must not appear as a
    /// value someone has to reason about.
    #[test]
    fn a_fresh_metadata_file_holds_only_the_id() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        collection_id(&folder)?;
        let written = std::fs::read_to_string(folder.join(COLLECTION_META_FILE))?;
        assert!(!written.contains("retention"), "wrote: {written}");
        assert!(!written.contains("interval"), "wrote: {written}");
        Ok(())
    }

    #[test]
    fn discovery_skips_a_folder_whose_metadata_is_broken() -> Fallible<()> {
        // One unreadable `.hashcards.toml` must not take every other local
        // collection down with it.
        let (dir, root) = fixture()?;
        let broken = root.path().join("Spanish");
        std::fs::create_dir_all(&broken)?;
        std::fs::write(broken.join(COLLECTION_META_FILE), "this is not toml {{{")?;
        std::fs::create_dir_all(root.path().join("Medicine"))?;

        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Medicine");
        Ok(())
    }

    #[test]
    fn read_only_discovery_writes_no_metadata_file() -> Fallible<()> {
        let (dir, root) = fixture()?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;

        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::ExistingOnly)?;
        assert!(found.is_empty(), "a folder with no id yet must be skipped");
        assert!(
            !folder.join(COLLECTION_META_FILE).exists(),
            "read path wrote"
        );
        Ok(())
    }

    #[test]
    fn discovery_keeps_one_of_two_folders_that_slugify_alike() -> Fallible<()> {
        // `slugify` maps both names to `Verbs-1`; serving both would make
        // `/collection/Verbs-1` mean whichever `read_dir` yielded first.
        let (dir, root) = fixture()?;
        std::fs::create_dir_all(root.path().join("Verbs 1"))?;
        std::fs::create_dir_all(root.path().join("Verbs-1"))?;

        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].name, "Verbs 1",
            "the first name by sort order wins"
        );
        Ok(())
    }

    #[test]
    fn opening_a_tree_does_not_create_it() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::open(&dir, Some("me@example.com"))?;
        assert!(!root.path().exists());
        Ok(())
    }
}
