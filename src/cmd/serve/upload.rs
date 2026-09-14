use axum::body::Bytes;
use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::StatusCode;

use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::cards::CardRoot;
use crate::cmd::serve::files::collection_folder;
use crate::cmd::serve::files::user_root;
use crate::cmd::serve::state::AppState;
use crate::error::Fallible;
use crate::error::fail;
use crate::utils::ensure_dir;

/// The largest single paste hashcards will store. Stated in the editor
/// before it is hit, and enforced twice: once by the route's body limit,
/// once here, so the message the user gets is ours.
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

/// Where a collection keeps the images pasted into its decks.
pub const MEDIA_DIR: &str = "media";

/// The extension for the image `bytes` hold, read from its magic number
/// rather than from whatever the browser called the file.
///
/// SVG is deliberately absent: it is script-bearing markup, and
/// `collection_file_handler` serves it inline as `image/svg+xml` from the
/// same origin as the app.
pub fn sniff_image(bytes: &[u8]) -> Fallible<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(PNG) {
        return Ok("png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok("jpg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok("gif");
    }
    // A WebP is a RIFF container whose form type, four bytes after the
    // length, says WEBP — a WAV is the same container with another word
    // there, which is why the whole header has to be read.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Ok("webp");
    }
    fail("That is not an image hashcards can store. Paste a PNG, JPEG, GIF or WebP.")
}

/// Store a pasted image for the collection that holds the deck at `rel`,
/// returning the path to write into the markdown.
///
/// The name is the image's own content hash, so the same screenshot pasted
/// twice is stored once and no clipboard tool's idea of a filename ever
/// reaches the disk. The file goes in the *collection's* media folder and
/// the returned path is collection-relative (`@/`), so a deck nested in a
/// subfolder resolves it exactly as one at the top does.
pub fn store_pasted_image(root: &CardRoot, rel: &str, bytes: &[u8]) -> Fallible<String> {
    if bytes.len() > MAX_UPLOAD_BYTES {
        return fail(format!(
            "That image is {} — the limit is 10 MB.",
            megabytes(bytes.len())
        ));
    }
    let extension = sniff_image(bytes)?;
    // The path is the deck the image is being pasted into. A path that
    // names no card file is not an edit in progress, and storing for it
    // would create a collection's media folder on the strength of a name
    // nothing answers to.
    if !root.resolve(rel)?.is_file() {
        return fail(format!("`{rel}` is not a card file in your collection."));
    }
    let media = collection_folder(root, rel)?.join(MEDIA_DIR);
    ensure_dir(&media, "collection media directory")?;

    let name = format!("{}.{extension}", &blake3::hash(bytes).to_hex()[..16]);
    let path = media.join(&name);
    if !path.exists() {
        // Written beside the target and renamed into place: a paste
        // interrupted halfway must not leave a truncated file under a name
        // that claims to be the hash of the whole image.
        let partial = media.join(format!("{name}.part"));
        std::fs::write(&partial, bytes)?;
        std::fs::rename(&partial, &path)?;
    }
    Ok(format!("@/{MEDIA_DIR}/{name}"))
}

/// A byte count as the user's clipboard tool would report it.
fn megabytes(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// `POST /files/media/{deck path}`: the body is the image itself.
///
/// The response body is the markdown path to insert, or the reason it was
/// refused — the editor puts either straight into its status line.
pub async fn media_upload_handler(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    current_user: Option<CurrentUser>,
    body: Bytes,
) -> (StatusCode, String) {
    let outcome = user_root(&state, current_user.as_ref())
        .and_then(|root| store_pasted_image(&root, &rel, &body));
    match outcome {
        Ok(path) => (StatusCode::OK, path),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::create_tmp_directory;

    /// The smallest byte strings that carry each format's magic number.
    fn png() -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(b"body");
        v
    }
    fn jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend_from_slice(b"body");
        v
    }
    fn gif() -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(b"body");
        v
    }
    fn webp() -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"WEBPVP8 ");
        v
    }

    fn fixture() -> Fallible<(std::path::PathBuf, CardRoot)> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        std::fs::write(root.path().join("Spanish").join("verbs.md"), "Q: a\nA: b\n")?;
        Ok((dir, root))
    }

    #[test]
    fn sniffing_names_every_supported_format() -> Fallible<()> {
        assert_eq!(sniff_image(&png())?, "png");
        assert_eq!(sniff_image(&jpeg())?, "jpg");
        assert_eq!(sniff_image(&gif())?, "gif");
        assert_eq!(sniff_image(&webp())?, "webp");
        Ok(())
    }

    /// The filename is never consulted, so a PDF called `cat.png` is still a
    /// PDF. SVG is refused on purpose, not by omission.
    #[test]
    fn sniffing_refuses_anything_else() {
        assert!(sniff_image(b"%PDF-1.7\n").is_err());
        assert!(sniff_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\">").is_err());
        assert!(sniff_image(b"").is_err());
        assert!(sniff_image(b"RIFF\0\0\0\0WAVEfmt ").is_err());
    }

    #[test]
    fn an_image_is_stored_in_the_collections_media_folder() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let inserted = store_pasted_image(&root, "Spanish/verbs.md", &png())?;

        // Collection-relative (`@/`), not deck-relative: a deck in a
        // subfolder resolves the same path.
        let name = inserted
            .strip_prefix("@/media/")
            .unwrap_or_else(|| panic!("not a collection-relative media path: {inserted}"));
        assert!(name.ends_with(".png"), "got: {inserted}");
        assert!(
            root.path()
                .join("Spanish")
                .join("media")
                .join(name)
                .exists(),
            "not written: {inserted}"
        );
        Ok(())
    }

    #[test]
    fn the_name_is_the_content_so_one_image_is_stored_once() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let first = store_pasted_image(&root, "Spanish/verbs.md", &png())?;
        let second = store_pasted_image(&root, "Spanish/verbs.md", &png())?;
        assert_eq!(first, second);

        let media = root.path().join("Spanish").join("media");
        assert_eq!(std::fs::read_dir(&media)?.count(), 1);

        // Different bytes, different name.
        let other = store_pasted_image(&root, "Spanish/verbs.md", &gif())?;
        assert_ne!(other, first);
        assert_eq!(std::fs::read_dir(&media)?.count(), 2);
        Ok(())
    }

    /// The media folder belongs to the collection, not to the deck's own
    /// directory: `@/media/...` resolves from any depth, and one folder
    /// holds the collection's images however deeply its decks are nested.
    #[test]
    fn a_deck_in_a_subfolder_stores_at_the_collection_root() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let nested = root.path().join("Spanish").join("Unit 2");
        std::fs::create_dir_all(&nested)?;
        std::fs::write(nested.join("verbs.md"), "Q: a\nA: b\n")?;

        let inserted = store_pasted_image(&root, "Spanish/Unit 2/verbs.md", &png())?;
        assert!(inserted.starts_with("@/media/"), "got: {inserted}");
        assert!(root.path().join("Spanish").join("media").is_dir());
        assert!(!nested.join("media").exists(), "stored beside the deck");
        Ok(())
    }

    #[test]
    fn a_deck_outside_any_collection_folder_is_refused() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        std::fs::write(root.path().join("loose.md"), "Q: a\nA: b\n")?;
        assert!(store_pasted_image(&root, "loose.md", &png()).is_err());
        Ok(())
    }

    /// The path names the deck being edited, so a path naming no deck at
    /// all is a request that cannot have come from the editor. Storing for
    /// it anyway materializes a `media/` folder inside a collection for a
    /// file that does not exist -- and, for a top-level name that is not a
    /// collection either, invents the collection folder's media directory.
    #[test]
    fn a_paste_for_a_deck_that_does_not_exist_is_refused() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        assert!(store_pasted_image(&root, "Spanish/nope.md", &png()).is_err());
        assert!(
            !root.path().join("Spanish").join("media").exists(),
            "a refused paste must write nothing"
        );
        // A directory is not a deck either.
        std::fs::create_dir_all(root.path().join("Spanish").join("Unit 2"))?;
        assert!(store_pasted_image(&root, "Spanish/Unit 2", &png()).is_err());
        Ok(())
    }

    #[test]
    fn an_oversized_paste_is_refused_by_size_not_by_content() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        let mut huge = png();
        huge.resize(MAX_UPLOAD_BYTES + 1, 0);
        let error = match store_pasted_image(&root, "Spanish/verbs.md", &huge) {
            Ok(_) => return fail("expected an oversized paste to be refused"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("10 MB"), "the limit must be named: {error}");
        assert!(
            !root.path().join("Spanish").join("media").exists(),
            "a refused paste must write nothing"
        );
        Ok(())
    }

    #[test]
    fn an_empty_paste_is_refused() -> Fallible<()> {
        let (_dir, root) = fixture()?;
        assert!(store_pasted_image(&root, "Spanish/verbs.md", b"").is_err());
        Ok(())
    }
}
