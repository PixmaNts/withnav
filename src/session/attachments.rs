//! Images attached to a conversation: validation of an upload and the SQLite table that holds the
//! bytes. A message carries only an [`ImageRef`]; the bytes are read back by `(session, id)` when a
//! request to a model needs them.
//!
//! The store owns its own connection to the history database, so megabytes of blob never wait in
//! the history writer's queue (which sequences every event of every session). Every method is
//! synchronous and may wait up to the busy timeout on another writer: async callers wrap it in
//! `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use ulid::Ulid;
use withnav_client_api::attachment::ImageRef;

/// Largest accepted image, in raw bytes: 3.75 MiB, which is exactly 5 MiB once base64-encoded.
///
/// Provider per-image limits apply to the base64 payload, not to the file: 5 MB on Bedrock and
/// Google Cloud, 10 MB on Anthropic's own API (checked 2026-10-19), and a gateway may route one
/// model to any of them. An image stored above the lowest limit would be refused by the provider
/// on every later turn of the session, because it is resent with each request; refusing it at
/// upload keeps the conversation usable.
pub const MAX_IMAGE_BYTES: usize = 3_932_160;

/// Largest side, in pixels. Providers downscale or refuse beyond about this.
pub const MAX_IMAGE_SIDE: u32 = 8000;

/// Attachments one session may hold. Uploaded and never sent images live until the session is
/// deleted, so the quota is what bounds the database growth of an idle session.
pub const MAX_ATTACHMENTS_PER_SESSION: usize = 64;

const MAX_NAME_BYTES: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error(
        "unsupported image type ({0}): attach a PNG, JPEG, GIF or WebP file (convert or export SVG and other formats first)"
    )]
    Unsupported(String),
    #[error(
        "image is {size} bytes, over the {max} byte limit (3.75 MiB): downscale or compress it and attach it again"
    )]
    TooLarge { size: usize, max: usize },
    #[error(
        "image is {width}x{height} px, over the {max} px limit on a side: downscale it and attach it again"
    )]
    TooWide { width: u32, height: u32, max: u32 },
    #[error("invalid image: {0}")]
    Invalid(String),
    #[error("unknown attachment: {0}")]
    Unknown(String),
    #[error("unknown session")]
    UnknownSession,
    #[error("this conversation already holds {0} attachments, the most it keeps")]
    Quota(usize),
    #[error("attachment store error: {0}")]
    Db(String),
}

impl From<rusqlite::Error> for AttachmentError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(error.to_string())
    }
}

/// What the header of an upload says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detected {
    pub media_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Identifies the image from its magic bytes and a bounded header parse. The file name and any
/// declared content type are never consulted: they are the client's claim, the bytes are the
/// evidence. No pixel is decoded and nothing is allocated from the declared dimensions, so a file
/// that passes can still fail to decode at the provider.
pub fn inspect(bytes: &[u8]) -> Result<Detected, AttachmentError> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(AttachmentError::TooLarge {
            size: bytes.len(),
            max: MAX_IMAGE_BYTES,
        });
    }
    let kind = match imagesize::image_type(bytes) {
        Ok(kind) => kind,
        // The type probe wants 12 bytes: a shorter file that starts like one of the four formats
        // is a truncated image, not an unknown type.
        Err(_) if starts_like_supported(bytes) => {
            return Err(AttachmentError::Invalid(
                "the file ends inside the image header".into(),
            ));
        }
        Err(_) => return Err(AttachmentError::Unsupported(describe_unknown(bytes))),
    };
    let media_type = match kind {
        imagesize::ImageType::Png => "image/png",
        imagesize::ImageType::Jpeg => "image/jpeg",
        imagesize::ImageType::Gif => "image/gif",
        imagesize::ImageType::Webp => "image/webp",
        other => {
            return Err(AttachmentError::Unsupported(format!(
                "{} image",
                format!("{other:?}").to_lowercase()
            )));
        }
    };
    let size = imagesize::blob_size(bytes).map_err(|error| {
        AttachmentError::Invalid(format!("unreadable {media_type} header ({error})"))
    })?;
    if size.width == 0 || size.height == 0 {
        return Err(AttachmentError::Invalid(
            "the image has no pixels (zero width or height)".into(),
        ));
    }
    let side = MAX_IMAGE_SIDE as usize;
    if size.width > side || size.height > side {
        return Err(AttachmentError::TooWide {
            width: u32::try_from(size.width).unwrap_or(u32::MAX),
            height: u32::try_from(size.height).unwrap_or(u32::MAX),
            max: MAX_IMAGE_SIDE,
        });
    }
    Ok(Detected {
        media_type,
        width: size.width as u32,
        height: size.height as u32,
    })
}

fn starts_like_supported(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG")
        || bytes.starts_with(b"\xFF\xD8\xFF")
        || bytes.starts_with(b"GIF8")
        || bytes.starts_with(b"RIFF")
}

/// Names what an unrecognised upload looks like, for the error: SVG is the one worth naming
/// because it is the likeliest "image" a Captain tries and is refused on purpose (it can carry
/// script and no provider reads it).
fn describe_unknown(bytes: &[u8]) -> String {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_lowercase();
    if head.contains("<svg") {
        "SVG".into()
    } else if head.trim_start().starts_with('<') {
        "XML or HTML text".into()
    } else if bytes.is_empty() {
        "empty file".into()
    } else {
        "unrecognised file type".into()
    }
}

/// The file's base name, control characters removed, at most 128 bytes, `image` when nothing
/// usable is left. A client-chosen name ends up in prompts and placeholders, so it must not
/// carry a path, a newline or an escape sequence.
pub fn sanitize_name(raw: Option<&str>) -> String {
    let base = raw
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    let mut end = cleaned.len().min(MAX_NAME_BYTES);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let name = cleaned[..end].trim_end();
    if name.is_empty() {
        "image".into()
    } else {
        name.to_string()
    }
}

/// The attachments table, behind its own connection. Cheap to clone.
#[derive(Clone, Debug)]
pub struct AttachmentStore {
    connection: Arc<Mutex<Connection>>,
}

impl AttachmentStore {
    /// Opens the history database at `path` (the file that holds the `sessions` table).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AttachmentError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AttachmentError::Db(e.to_string()))?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(connection)
    }

    /// A private database, for tests: it has no `sessions` table until the test creates one.
    pub fn in_memory() -> Result<Self, AttachmentError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(connection: Connection) -> Result<Self, AttachmentError> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS attachments (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL, media_type TEXT NOT NULL,
                name TEXT NOT NULL, size INTEGER NOT NULL, width INTEGER NOT NULL,
                height INTEGER NOT NULL, bytes BLOB NOT NULL, created_at TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS attachments_session ON attachments(session_id);",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Validates and stores an upload. The session check, the quota check and the insert share one
    /// write transaction, so two concurrent uploads cannot both slip under the quota and an
    /// upload cannot land on a session deleted a moment before.
    pub fn insert(
        &self,
        session_id: &str,
        name: Option<&str>,
        bytes: &[u8],
    ) -> Result<ImageRef, AttachmentError> {
        let detected = inspect(bytes)?;
        let name = sanitize_name(name);
        let mut connection = self.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_sessions_table = tx
            .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions'")?
            .exists([])?;
        let session_exists = has_sessions_table
            && tx
                .prepare("SELECT 1 FROM sessions WHERE id = ?1")?
                .exists([session_id])?;
        if !session_exists {
            return Err(AttachmentError::UnknownSession);
        }
        let held: i64 = tx.query_row(
            "SELECT COUNT(*) FROM attachments WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        if held as usize >= MAX_ATTACHMENTS_PER_SESSION {
            return Err(AttachmentError::Quota(MAX_ATTACHMENTS_PER_SESSION));
        }
        let image = ImageRef {
            attachment_id: Ulid::generate().to_string(),
            media_type: detected.media_type.into(),
            name,
            size: bytes.len() as u64,
            width: detected.width,
            height: detected.height,
        };
        tx.execute(
            "INSERT INTO attachments
                (id, session_id, media_type, name, size, width, height, bytes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                image.attachment_id,
                session_id,
                image.media_type,
                image.name,
                image.size as i64,
                image.width,
                image.height,
                bytes,
                crate::bus::event::now_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(image)
    }

    /// The metadata of each id, looked up by `(session, id)`, in input order. An id that does
    /// not exist, or belongs to another session, is [`AttachmentError::Unknown`]: a message may
    /// never reference another conversation's image.
    pub fn meta(&self, session_id: &str, ids: &[String]) -> Result<Vec<ImageRef>, AttachmentError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT id, media_type, name, size, width, height
             FROM attachments WHERE session_id = ?1 AND id = ?2",
        )?;
        ids.iter()
            .map(|id| {
                statement
                    .query_row(params![session_id, id], row_to_ref)
                    .optional()?
                    .ok_or_else(|| AttachmentError::Unknown(id.clone()))
            })
            .collect()
    }

    /// The metadata and bytes of one image, by `(session, id)`.
    pub fn load(&self, session_id: &str, id: &str) -> Result<(ImageRef, Vec<u8>), AttachmentError> {
        let connection = self.lock();
        connection
            .query_row(
                "SELECT id, media_type, name, size, width, height, bytes
                 FROM attachments WHERE session_id = ?1 AND id = ?2",
                params![session_id, id],
                |row| Ok((row_to_ref(row)?, row.get::<_, Vec<u8>>(6)?)),
            )
            .optional()?
            .ok_or_else(|| AttachmentError::Unknown(id.to_string()))
    }

    /// Removes every attachment of a session. Session deletion does this in its own transaction
    /// (`HistoryStore::delete`); this is for callers that hold only the store.
    pub fn delete_session(&self, session_id: &str) -> Result<(), AttachmentError> {
        self.lock().execute(
            "DELETE FROM attachments WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    }
}

fn row_to_ref(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImageRef> {
    Ok(ImageRef {
        attachment_id: row.get(0)?,
        media_type: row.get(1)?,
        name: row.get(2)?,
        size: row.get::<_, i64>(3)? as u64,
        width: row.get(4)?,
        height: row.get(5)?,
    })
}

/// The loader a session installs on its root actor: it reads only rows of that session, on the
/// blocking pool, so a request can never carry another conversation's image.
pub struct SessionLoader {
    pub store: Arc<AttachmentStore>,
    pub session_id: String,
}

impl crate::agent::images::AttachmentLoader for SessionLoader {
    fn load(
        &self,
        ids: Vec<String>,
    ) -> futures::future::BoxFuture<
        'static,
        std::collections::HashMap<String, Result<withnav_provider::openai::ImagePayload, String>>,
    > {
        let store = self.store.clone();
        let session = self.session_id.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                ids.into_iter()
                    .map(|id| {
                        let payload = store
                            .load(&session, &id)
                            .map(|(image, bytes)| {
                                withnav_provider::openai::ImagePayload::from_bytes(
                                    image.media_type,
                                    &bytes,
                                )
                            })
                            .map_err(|error| error.to_string());
                        (id, payload)
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
        })
    }
}

/// Most images one message may reference.
pub const MAX_IMAGES_PER_MESSAGE: usize = 8;

/// Replaces the client's image references with the stored rows of this session: a client sends
/// only an id, so it can neither misstate the type or size nor point at another conversation's
/// image. Any unknown or foreign id refuses the whole message.
pub fn resolve_command_images(
    store: &AttachmentStore,
    session_id: &str,
    content: &mut [withnav_client_api::command::ContentBlock],
) -> Result<(), AttachmentError> {
    use withnav_client_api::command::ContentBlock;
    let ids: Vec<String> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image.attachment_id.clone()),
            _ => None,
        })
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    if ids.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(AttachmentError::Invalid(format!(
            "a message may carry at most {MAX_IMAGES_PER_MESSAGE} images"
        )));
    }
    let mut stored = store.meta(session_id, &ids)?.into_iter();
    for block in content.iter_mut() {
        if let ContentBlock::Image(image) = block
            && let Some(row) = stored.next()
        {
            *image = row;
        }
    }
    Ok(())
}

/// Hand-built image headers for tests: just enough bytes for the header parse.
#[cfg(test)]
pub(crate) mod fixtures {
    pub fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        out.extend(width.to_be_bytes());
        out.extend(height.to_be_bytes());
        out.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        out
    }

    pub fn gif(width: u16, height: u16) -> Vec<u8> {
        let mut out = b"GIF89a".to_vec();
        out.extend(width.to_le_bytes());
        out.extend(height.to_le_bytes());
        out.extend([0, 0, 0, 0x3b]);
        out
    }

    pub fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        out.extend([0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        out.extend(height.to_be_bytes());
        out.extend(width.to_be_bytes());
        out.extend([1, 1, 0x11, 0]);
        out.extend([0xFF, 0xD9]);
        out
    }

    /// A lossless WebP header (VP8L): 14 bits each for width-1 and height-1.
    pub fn webp(width: u32, height: u32) -> Vec<u8> {
        let mut out = b"RIFF\x00\x00\x00\x00WEBPVP8L\x00\x00\x00\x00\x2f".to_vec();
        let dims = (width - 1) | ((height - 1) << 14);
        out.extend(dims.to_le_bytes());
        out.extend([0; 4]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn store_with_session(id: &str) -> AttachmentStore {
        let store = AttachmentStore::in_memory().unwrap();
        {
            let connection = store.lock();
            connection
                .execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY)")
                .unwrap();
            connection
                .execute("INSERT INTO sessions (id) VALUES (?1)", [id])
                .unwrap();
        }
        store
    }

    fn add_session(store: &AttachmentStore, id: &str) {
        store
            .lock()
            .execute("INSERT INTO sessions (id) VALUES (?1)", [id])
            .unwrap();
    }

    #[test]
    fn each_format_is_identified_by_its_bytes() {
        let cases = [
            (png(12, 34), "image/png", 12, 34),
            (jpeg(640, 480), "image/jpeg", 640, 480),
            (gif(5, 6), "image/gif", 5, 6),
            (webp(100, 200), "image/webp", 100, 200),
        ];
        for (bytes, media_type, width, height) in cases {
            let detected = inspect(&bytes).unwrap();
            assert_eq!(
                detected,
                Detected {
                    media_type,
                    width,
                    height
                }
            );
        }
    }

    #[test]
    fn the_file_name_never_decides_the_type() {
        let store = store_with_session("s1");
        let image = store.insert("s1", Some("x.jpg"), &png(2, 2)).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.name, "x.jpg");
    }

    #[test]
    fn svg_and_other_text_are_refused_with_what_they_look_like() {
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#;
        let error = inspect(svg).unwrap_err();
        assert!(
            matches!(&error, AttachmentError::Unsupported(kind) if kind == "SVG"),
            "{error}"
        );
        assert!(matches!(
            inspect(b"hello there, not an image"),
            Err(AttachmentError::Unsupported(_))
        ));
        assert!(matches!(inspect(b""), Err(AttachmentError::Unsupported(_))));
    }

    #[test]
    fn another_image_format_is_refused_by_name() {
        let error = inspect(b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00\x28\x00\x00\x00\x02\x00\x00\x00\x02\x00\x00\x00")
            .unwrap_err();
        assert!(
            matches!(&error, AttachmentError::Unsupported(kind) if kind.contains("bmp")),
            "{error}"
        );
    }

    #[test]
    fn a_truncated_header_is_invalid() {
        let mut bytes = png(10, 10);
        bytes.truncate(14);
        assert!(matches!(inspect(&bytes), Err(AttachmentError::Invalid(_))));
        let mut jpeg = jpeg(10, 10);
        jpeg.truncate(10);
        assert!(matches!(inspect(&jpeg), Err(AttachmentError::Invalid(_))));
    }

    #[test]
    fn zero_dimensions_are_invalid() {
        assert!(matches!(
            inspect(&png(0, 10)),
            Err(AttachmentError::Invalid(_))
        ));
        assert!(matches!(
            inspect(&gif(10, 0)),
            Err(AttachmentError::Invalid(_))
        ));
    }

    #[test]
    fn a_side_over_8000_px_is_refused() {
        assert!(inspect(&png(8000, 8000)).is_ok());
        let error = inspect(&png(9000, 10)).unwrap_err();
        assert!(matches!(
            error,
            AttachmentError::TooWide {
                width: 9000,
                height: 10,
                max: 8000
            }
        ));
        assert!(matches!(
            inspect(&png(u32::MAX, 10)),
            Err(AttachmentError::TooWide { .. })
        ));
    }

    #[test]
    fn the_size_cap_is_inclusive() {
        let mut bytes = png(4, 4);
        bytes.resize(MAX_IMAGE_BYTES, 0);
        assert!(inspect(&bytes).is_ok());
        bytes.push(0);
        let error = inspect(&bytes).unwrap_err();
        assert!(
            matches!(error, AttachmentError::TooLarge { size, max } if size == MAX_IMAGE_BYTES + 1 && max == MAX_IMAGE_BYTES)
        );
    }

    #[test]
    fn names_lose_paths_controls_and_excess_length() {
        assert_eq!(sanitize_name(Some("/etc/passwd")), "passwd");
        assert_eq!(sanitize_name(Some(r"C:\Users\me\shot.png")), "shot.png");
        assert_eq!(sanitize_name(Some("a\nb\x1b[31m.png")), "ab[31m.png");
        assert_eq!(sanitize_name(Some("dir/")), "image");
        assert_eq!(sanitize_name(Some("  ")), "image");
        assert_eq!(sanitize_name(None), "image");
        let long = "é".repeat(100);
        let name = sanitize_name(Some(&long));
        assert!(name.len() <= 128);
        assert!(name.chars().all(|c| c == 'é'));
        assert_eq!(name.len(), 128);
    }

    #[test]
    fn an_upload_round_trips_for_its_own_session_only() {
        let store = store_with_session("s1");
        add_session(&store, "s2");
        let bytes = png(3, 4);
        let image = store.insert("s1", Some("shot.png"), &bytes).unwrap();
        assert_eq!(image.size, bytes.len() as u64);
        assert_eq!((image.width, image.height), (3, 4));

        let (loaded, data) = store.load("s1", &image.attachment_id).unwrap();
        assert_eq!(loaded, image);
        assert_eq!(data, bytes);
        let listed = store
            .meta("s1", std::slice::from_ref(&image.attachment_id))
            .unwrap();
        assert_eq!(listed, vec![image.clone()]);

        assert!(matches!(
            store.load("s2", &image.attachment_id),
            Err(AttachmentError::Unknown(id)) if id == image.attachment_id
        ));
        assert!(matches!(
            store.meta("s2", std::slice::from_ref(&image.attachment_id)),
            Err(AttachmentError::Unknown(_))
        ));
    }

    #[test]
    fn meta_keeps_input_order_and_fails_on_any_missing_id() {
        let store = store_with_session("s1");
        let first = store.insert("s1", None, &png(1, 1)).unwrap();
        let second = store.insert("s1", None, &gif(1, 1)).unwrap();
        let ids = vec![second.attachment_id.clone(), first.attachment_id.clone()];
        let found = store.meta("s1", &ids).unwrap();
        assert_eq!(found[0].attachment_id, second.attachment_id);
        assert_eq!(found[1].attachment_id, first.attachment_id);
        let mut ids = ids;
        ids.push("nope".into());
        assert!(
            matches!(store.meta("s1", &ids), Err(AttachmentError::Unknown(id)) if id == "nope")
        );
    }

    #[test]
    fn an_unknown_session_is_refused() {
        let store = store_with_session("s1");
        assert!(matches!(
            store.insert("other", None, &png(1, 1)),
            Err(AttachmentError::UnknownSession)
        ));
        // A database that has no sessions table at all.
        let bare = AttachmentStore::in_memory().unwrap();
        assert!(matches!(
            bare.insert("s1", None, &png(1, 1)),
            Err(AttachmentError::UnknownSession)
        ));
    }

    #[test]
    fn a_session_holds_at_most_64_attachments() {
        let store = store_with_session("s1");
        add_session(&store, "s2");
        let bytes = png(1, 1);
        for _ in 0..MAX_ATTACHMENTS_PER_SESSION {
            store.insert("s1", None, &bytes).unwrap();
        }
        assert!(matches!(
            store.insert("s1", None, &bytes),
            Err(AttachmentError::Quota(64))
        ));
        // The quota is per session.
        store.insert("s2", None, &bytes).unwrap();
        store.delete_session("s1").unwrap();
        store.insert("s1", None, &bytes).unwrap();
    }

    #[test]
    fn a_refused_upload_stores_nothing() {
        let store = store_with_session("s1");
        assert!(store.insert("s1", None, b"<svg/>").is_err());
        let count: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[cfg(test)]
mod admission_tests {
    use super::fixtures::png;
    use super::*;
    use withnav_client_api::command::ContentBlock;

    fn store() -> AttachmentStore {
        let store = AttachmentStore::in_memory().unwrap();
        store
            .lock()
            .execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY);
                 INSERT INTO sessions (id) VALUES ('s1'), ('s2');",
            )
            .unwrap();
        store
    }

    fn by_id(id: &str) -> ContentBlock {
        ContentBlock::Image(ImageRef {
            attachment_id: id.into(),
            media_type: "image/gif".into(),
            name: "lie.gif".into(),
            size: 1,
            width: 1,
            height: 1,
        })
    }

    #[test]
    fn admission_takes_the_metadata_from_the_stored_row_not_the_client() {
        let store = store();
        let stored = store.insert("s1", Some("shot.png"), &png(4, 3)).unwrap();
        let mut content = vec![
            ContentBlock::Text { text: "see".into() },
            by_id(&stored.attachment_id),
        ];
        resolve_command_images(&store, "s1", &mut content).unwrap();
        assert_eq!(content[1], ContentBlock::Image(stored));
    }

    #[test]
    fn an_image_of_another_session_or_an_unknown_id_refuses_the_message() {
        let store = store();
        let other = store.insert("s2", Some("other.png"), &png(4, 3)).unwrap();
        let mut content = vec![by_id(&other.attachment_id)];
        assert!(matches!(
            resolve_command_images(&store, "s1", &mut content),
            Err(AttachmentError::Unknown(_))
        ));
        let mut content = vec![by_id("01NOPE")];
        assert!(resolve_command_images(&store, "s1", &mut content).is_err());
    }

    #[test]
    fn a_message_carries_at_most_eight_images() {
        let store = store();
        let ids: Vec<_> = (0..=MAX_IMAGES_PER_MESSAGE)
            .map(|_| store.insert("s1", None, &png(1, 1)).unwrap().attachment_id)
            .collect();
        let mut content: Vec<_> = ids.iter().map(|id| by_id(id)).collect();
        assert!(matches!(
            resolve_command_images(&store, "s1", &mut content),
            Err(AttachmentError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn the_session_loader_reads_only_its_own_rows() {
        use crate::agent::images::AttachmentLoader as _;
        let store = Arc::new(store());
        let mine = store.insert("s1", None, &png(2, 2)).unwrap().attachment_id;
        let theirs = store.insert("s2", None, &png(2, 2)).unwrap().attachment_id;
        let loader = SessionLoader {
            store,
            session_id: "s1".into(),
        };
        let loaded = loader.load(vec![mine.clone(), theirs.clone()]).await;
        assert!(loaded[&mine].is_ok());
        assert!(loaded[&theirs].is_err());
    }
}
