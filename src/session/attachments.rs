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
use sha2::{Digest, Sha256};

pub mod svg;
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

/// Attachments tools may fill: the rest of the quota stays the Captain's, so a run of screenshots
/// never stops them attaching an image.
pub const MAX_TOOL_ATTACHMENTS_PER_SESSION: usize = 48;

/// Who stores an attachment: an upload is the Captain's; a tool's image is deduplicated and has
/// the smaller [`MAX_TOOL_ATTACHMENTS_PER_SESSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Upload,
    Tool,
}

const MAX_NAME_BYTES: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("unsupported image type ({0}): attach a PNG, JPEG, GIF, WebP or SVG file")]
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

/// Identifies the image from its magic bytes and a bounded header parse, then decodes it fully.
/// The file name and any declared content type are never consulted: they are the client's claim,
/// the bytes are the evidence. A file that passes decodes: a truncated or corrupt raster is
/// refused here instead of failing at the provider on every later turn of the session.
pub fn inspect(bytes: &[u8]) -> Result<Detected, AttachmentError> {
    let detected = inspect_header(bytes)?;
    decode_fully(bytes, detected)?;
    Ok(detected)
}

/// The cheap half of [`inspect`]: magic bytes and the header's dimensions. Nothing is decoded and
/// nothing is allocated from the declared dimensions.
fn inspect_header(bytes: &[u8]) -> Result<Detected, AttachmentError> {
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

/// Allocation ceiling asked of the decoders: an 8000 x 8000 RGBA image (the largest the header
/// check admits) is 256 MiB, and the decoder needs room beside its output. Not every codec honours
/// it (WebP and JPEG check dimensions only): for a still image the real bound is the header's side
/// limit, which keeps one decode at roughly 430 MiB at worst, and decodes run one at a time.
const MAX_DECODE_ALLOC: u64 = 384 * 1024 * 1024;

/// Pixels decoded across the frames of one animation (GIF, animated WebP) before the file is
/// refused: each frame is composited onto a full canvas, so a file of thousands of large frames
/// would cost minutes of CPU. A 3.75 MiB animation of ordinary size stays far below.
const MAX_ANIMATION_PIXELS: u64 = 512 * 1024 * 1024;

/// Largest canvas of an animation (GIF, animated WebP, APNG) that is checked, in pixels: 4096 x
/// 4096, about 64 MiB per RGBA buffer.
const MAX_ANIMATION_CANVAS_PIXELS: u64 = 4096 * 4096;

/// Decodes every pixel of `bytes`, whose header [`inspect_header`] accepted, and drops the result.
/// Decoding is bounded in memory ([`MAX_DECODE_ALLOC`], [`MAX_ANIMATION_CANVAS_PIXELS`]) and in
/// work for animations ([`MAX_ANIMATION_PIXELS`]), and runs one at a time: each holds a
/// full-size canvas. An animation is decoded frame by frame, a still image once.
fn decode_fully(bytes: &[u8], detected: Detected) -> Result<(), AttachmentError> {
    use image::{AnimationDecoder, ImageDecoder, ImageFormat, Limits};
    use std::io::Cursor;

    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());

    let corrupt = |error: image::ImageError| {
        AttachmentError::Invalid(format!(
            "corrupt or truncated {} ({error})",
            detected.media_type
        ))
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let corrupt_gif = |error: gif::DecodingError| {
        AttachmentError::Invalid(format!("corrupt or truncated image/gif ({error})"))
    };
    let reader = Cursor::new(bytes);
    let frames = |frames: image::Frames<'_>| -> Result<(), AttachmentError> {
        // Each frame is composited onto a canvas that is held beside the decoded frame and the
        // previous one: past this size the animation is refused instead of costing ~3 canvases.
        if u64::from(detected.width) * u64::from(detected.height) > MAX_ANIMATION_CANVAS_PIXELS {
            return Err(AttachmentError::Invalid(format!(
                "the {} animation is too large to check ({}x{} px)",
                detected.media_type, detected.width, detected.height
            )));
        }
        let mut pixels: u64 = 0;
        for frame in frames {
            let frame = frame.map_err(corrupt)?;
            pixels += u64::from(frame.buffer().width()) * u64::from(frame.buffer().height());
            if pixels > MAX_ANIMATION_PIXELS {
                return Err(AttachmentError::Invalid(format!(
                    "the {} animation has too many frames to check",
                    detected.media_type
                )));
            }
        }
        Ok(())
    };
    match detected.media_type {
        "image/gif" => {
            check_gif_frames(bytes).map_err(corrupt_gif)?;
            let mut decoder = image::codecs::gif::GifDecoder::new(reader).map_err(corrupt)?;
            decoder.set_limits(limits).map_err(corrupt)?;
            frames(decoder.into_frames())
        }
        "image/webp" => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(reader).map_err(corrupt)?;
            decoder.set_limits(limits).map_err(corrupt)?;
            if decoder.has_animation() {
                frames(decoder.into_frames())
            } else {
                image::DynamicImage::from_decoder(decoder)
                    .map(drop)
                    .map_err(corrupt)
            }
        }
        "image/png" => {
            check_png_chunks(bytes).map_err(AttachmentError::Invalid)?;
            let mut decoder = image::codecs::png::PngDecoder::new(reader).map_err(corrupt)?;
            decoder.set_limits(limits).map_err(corrupt)?;
            if decoder.is_apng().map_err(corrupt)? {
                frames(decoder.apng().map_err(corrupt)?.into_frames())
            } else {
                image::DynamicImage::from_decoder(decoder)
                    .map(drop)
                    .map_err(corrupt)
            }
        }
        _ => {
            let mut decoder = image::ImageReader::with_format(reader, ImageFormat::Jpeg);
            decoder.limits(limits);
            decoder.decode().map(drop).map_err(corrupt)
        }
    }
}

/// Reads every frame header of a GIF without decoding pixels, refusing a frame that lies outside
/// the logical screen. The pixel budget of [`decode_fully`] counts canvas pixels, so a GIF with a
/// 1 x 1 screen and large frames would otherwise cost far more than it is charged.
fn check_gif_frames(bytes: &[u8]) -> Result<(), gif::DecodingError> {
    let mut options = gif::DecodeOptions::new();
    options.check_frame_consistency(true);
    let mut decoder = options.read_info(std::io::Cursor::new(bytes))?;
    while decoder.next_frame_info()?.is_some() {}
    Ok(())
}

/// Walks the chunk structure of a PNG up to `IEND`: the pixel decoder stops once the image data
/// is complete and never notices a file cut inside the chunks after it (or the closing `IEND`,
/// and animation frames past the default image, which it does not read). Bytes after `IEND` are
/// ignored.
fn check_png_chunks(bytes: &[u8]) -> Result<(), String> {
    let mut at = 8usize;
    loop {
        let header = bytes
            .get(at..at + 8)
            .ok_or("corrupt or truncated image/png (the file ends before its IEND chunk)")?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        // length + data + 4-byte checksum
        let end = (at + 8)
            .checked_add(length)
            .and_then(|n| n.checked_add(4))
            .filter(|end| *end <= bytes.len())
            .ok_or("corrupt or truncated image/png (a chunk runs past the end of the file)")?;
        if &header[4..8] == b"IEND" {
            return Ok(());
        }
        at = end;
    }
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

/// Brings an `attachments` table written by an older build up to date: the `content_hash` column
/// (hex SHA-256 of the stored bytes, filled in for existing rows) and the index tool-image
/// deduplication looks it up by.
pub(crate) fn upgrade_schema(connection: &Connection) -> rusqlite::Result<()> {
    let upgraded = !connection
        .prepare("SELECT 1 FROM pragma_table_info('attachments') WHERE name='content_hash'")?
        .exists([])?;
    // One transaction: a failure after the column exists would otherwise leave the old rows
    // unhashed for good, since the backfill runs only when the column is added.
    let tx = connection.unchecked_transaction()?;
    crate::session::store::add_column_if_missing(&tx, "attachments", "content_hash", "TEXT")?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS attachments_session_hash
            ON attachments(session_id, content_hash);",
    )?;
    if upgraded {
        // Hash the rows an older build stored, once, so a session already holding tool images
        // still finds them: without it a session at its tool quota would refuse a repeated
        // screenshot it used to deduplicate. Only on the upgrade itself: finding the rows still
        // lacking a hash would read past every blob.
        let ids: Vec<String> = tx
            .prepare("SELECT id FROM attachments")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for id in ids {
            let bytes: Vec<u8> =
                tx.query_row("SELECT bytes FROM attachments WHERE id = ?1", [&id], |r| {
                    r.get(0)
                })?;
            tx.execute(
                "UPDATE attachments SET content_hash = ?2 WHERE id = ?1",
                params![id, hex::encode(Sha256::digest(&bytes))],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
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
                height INTEGER NOT NULL, bytes BLOB NOT NULL, created_at TEXT NOT NULL,
                content_hash TEXT);
            CREATE INDEX IF NOT EXISTS attachments_session ON attachments(session_id);",
        )?;
        upgrade_schema(&connection)?;
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
        self.insert_from(session_id, name, bytes, Origin::Upload)
    }

    /// [`Self::insert`] for `origin`. A tool image identical to one the session already holds
    /// returns that one: a tool polled for the same screenshot does not fill the quota.
    pub fn insert_from(
        &self,
        session_id: &str,
        name: Option<&str>,
        bytes: &[u8],
        origin: Origin,
    ) -> Result<ImageRef, AttachmentError> {
        // An SVG is stored as the PNG it renders to, under the same name with `.png`: providers
        // read raster images only, and a client never receives markup that could run.
        let rendered;
        let (bytes, name) = if imagesize::image_type(bytes).is_err() && svg::looks_like_svg(bytes) {
            rendered = svg::rasterize(bytes)?;
            let base = sanitize_name(name);
            let stem = match base.len().checked_sub(4) {
                Some(at)
                    if base.is_char_boundary(at) && base[at..].eq_ignore_ascii_case(".svg") =>
                {
                    &base[..at]
                }
                _ => base.as_str(),
            };
            (rendered.as_slice(), format!("{stem}.png"))
        } else {
            (bytes, sanitize_name(name))
        };
        let detected = inspect(bytes)?;
        let content_hash = hex::encode(Sha256::digest(bytes));
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
        if origin == Origin::Tool {
            let known = tx
                .query_row(
                    "SELECT id, media_type, name, size, width, height FROM attachments
                     WHERE session_id = ?1 AND content_hash = ?2 AND size = ?3 LIMIT 1",
                    params![session_id, content_hash, bytes.len() as i64],
                    |row| {
                        Ok(ImageRef {
                            attachment_id: row.get(0)?,
                            media_type: row.get(1)?,
                            name: row.get(2)?,
                            size: row.get::<_, i64>(3)? as u64,
                            width: row.get(4)?,
                            height: row.get(5)?,
                        })
                    },
                )
                .optional()?;
            if let Some(image) = known {
                return Ok(image);
            }
        }
        let limit = match origin {
            Origin::Upload => MAX_ATTACHMENTS_PER_SESSION,
            Origin::Tool => MAX_TOOL_ATTACHMENTS_PER_SESSION,
        };
        if held as usize >= limit {
            return Err(AttachmentError::Quota(limit));
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
                (id, session_id, media_type, name, size, width, height, bytes, created_at,
                 content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                content_hash,
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
    fn store(
        &self,
        name: String,
        bytes: Vec<u8>,
    ) -> futures::future::BoxFuture<'static, Result<ImageRef, String>> {
        let store = self.store.clone();
        let session = self.session_id.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                store.insert_from(&session, Some(&name), &bytes, Origin::Tool)
            })
            .await
            .map_err(|_| "the attachment store stopped".to_string())?
            .map_err(|error| error.to_string())
        })
    }

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

/// Image fixtures for tests. `png`, `jpeg`, `gif` and `webp` are real, fully decodable images (the
/// store decodes every upload); the `*_header` variants are header-only bytes, for the checks that
/// need dimensions no test should allocate (zero, or past the side limit).
#[cfg(test)]
pub(crate) mod fixtures {
    use image::{ImageFormat, RgbImage};
    use std::io::Cursor;

    fn encode(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbImage::new(width, height)
            .write_to(&mut out, format)
            .unwrap();
        out.into_inner()
    }

    pub fn png(width: u32, height: u32) -> Vec<u8> {
        encode(width, height, ImageFormat::Png)
    }

    pub fn gif(width: u16, height: u16) -> Vec<u8> {
        encode(width.into(), height.into(), ImageFormat::Gif)
    }

    pub fn jpeg(width: u16, height: u16) -> Vec<u8> {
        encode(width.into(), height.into(), ImageFormat::Jpeg)
    }

    pub fn webp(width: u32, height: u32) -> Vec<u8> {
        encode(width, height, ImageFormat::WebP)
    }

    pub fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        out.extend(width.to_be_bytes());
        out.extend(height.to_be_bytes());
        out.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        out
    }

    pub fn gif_header(width: u16, height: u16) -> Vec<u8> {
        let mut out = b"GIF89a".to_vec();
        out.extend(width.to_le_bytes());
        out.extend(height.to_le_bytes());
        out.extend([0, 0, 0, 0x3b]);
        out
    }

    pub fn jpeg_header(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        out.extend([0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        out.extend(height.to_be_bytes());
        out.extend(width.to_be_bytes());
        out.extend([1, 1, 0x11, 0]);
        out.extend([0xFF, 0xD9]);
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
            (jpeg(64, 48), "image/jpeg", 64, 48),
            (gif(5, 6), "image/gif", 5, 6),
            (webp(100, 20), "image/webp", 100, 20),
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

    // Given an SVG upload
    // Then the store keeps the PNG it renders to, under the same name with `.png`, never the markup
    #[test]
    fn an_svg_upload_is_stored_as_its_png_rendering() {
        let store = store_with_session("s1");
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="400"><rect width="800" height="400" fill="green"/></svg>"#;
        let image = store.insert("s1", Some("diagram.svg"), svg).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.name, "diagram.png");
        let upper = store.insert("s1", Some("SHOT.SVG"), svg).unwrap();
        assert_eq!(upper.name, "SHOT.png");
        assert_eq!((image.width, image.height), (800, 400));
        let (_, bytes) = store.load("s1", &image.attachment_id).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn raw_svg_and_other_text_are_not_images_to_the_header_check() {
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
        let mut jpeg = jpeg_header(10, 10);
        jpeg.truncate(10);
        assert!(matches!(inspect(&jpeg), Err(AttachmentError::Invalid(_))));
    }

    #[test]
    fn zero_dimensions_are_invalid() {
        assert!(matches!(
            inspect(&png_header(0, 10)),
            Err(AttachmentError::Invalid(_))
        ));
        assert!(matches!(
            inspect(&gif_header(10, 0)),
            Err(AttachmentError::Invalid(_))
        ));
    }

    #[test]
    fn a_side_over_8000_px_is_refused() {
        assert!(inspect_header(&png_header(8000, 8000)).is_ok());
        let error = inspect(&png_header(9000, 10)).unwrap_err();
        assert!(matches!(
            error,
            AttachmentError::TooWide {
                width: 9000,
                height: 10,
                max: 8000
            }
        ));
        assert!(matches!(
            inspect(&png_header(u32::MAX, 10)),
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
    fn a_repeated_tool_image_is_stored_once_and_tools_leave_room_for_the_captain() {
        let store = store_with_session("s1");
        let first = store
            .insert_from("s1", Some("a.png"), &png(1, 1), Origin::Tool)
            .unwrap();
        let again = store
            .insert_from("s1", Some("b.png"), &png(1, 1), Origin::Tool)
            .unwrap();
        assert_eq!(first, again);
        for side in 2..=MAX_TOOL_ATTACHMENTS_PER_SESSION as u32 {
            store
                .insert_from("s1", None, &png(side, 1), Origin::Tool)
                .unwrap();
        }
        assert!(matches!(
            store.insert_from("s1", None, &png(1, 2), Origin::Tool),
            Err(AttachmentError::Quota(48))
        ));
        // An identical image is still found once the tools' share is full.
        store
            .insert_from("s1", None, &png(1, 1), Origin::Tool)
            .unwrap();
        // The Captain's uploads are never deduplicated and keep the remaining slots.
        store.insert("s1", None, &png(1, 1)).unwrap();
    }

    // Given a raster whose header is intact but whose pixel data is cut short or damaged
    // Then the upload is refused before any row is written, for each format
    #[test]
    fn a_truncated_or_corrupt_raster_is_refused_before_storage() {
        let store = store_with_session("s1");
        for (name, bytes) in [
            ("png", png(40, 40)),
            ("jpeg", jpeg(64, 64)),
            ("gif", gif(40, 40)),
            ("webp", webp(40, 40)),
        ] {
            let mut cut = bytes.clone();
            cut.truncate(bytes.len() * 3 / 4);
            let error = store.insert("s1", None, &cut).unwrap_err();
            assert!(
                matches!(&error, AttachmentError::Invalid(why) if why.contains("corrupt or truncated")),
                "{name}: {error}"
            );
        }
        // A header with no image data behind it (the shape of the hand-built fixtures).
        assert!(matches!(
            store.insert("s1", None, &png_header(4, 4)),
            Err(AttachmentError::Invalid(_))
        ));
        let count: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // Given a PNG with bit flips inside its compressed data
    // Then the checksum or the inflate fails and nothing is stored
    #[test]
    fn a_png_with_damaged_data_is_refused() {
        let store = store_with_session("s1");
        let mut bytes = png(40, 40);
        let middle = bytes.len() / 2;
        for byte in &mut bytes[middle..middle + 4] {
            *byte ^= 0xFF;
        }
        assert!(matches!(
            store.insert("s1", None, &bytes),
            Err(AttachmentError::Invalid(_))
        ));
    }

    // Given the quota's worth of tool images
    // Then looking one up by content goes through the hash index and reads no blob
    #[test]
    fn tool_image_dedup_uses_the_hash_index() {
        let store = store_with_session("s1");
        for side in 1..=MAX_TOOL_ATTACHMENTS_PER_SESSION as u32 {
            store
                .insert_from("s1", None, &png(side, 1), Origin::Tool)
                .unwrap();
        }
        let plan: String = store
            .lock()
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT id, media_type, name, size, width, height FROM attachments
                 WHERE session_id = ?1 AND content_hash = ?2 AND size = ?3 LIMIT 1",
            )
            .unwrap()
            .query_map(params!["s1", "h", 1], |row| row.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join("; ");
        assert!(plan.contains("attachments_session_hash"), "{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
        // Same bytes under another name: found by hash, not stored again.
        let again = store
            .insert_from("s1", Some("again.png"), &png(7, 1), Origin::Tool)
            .unwrap();
        let held: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(held, MAX_TOOL_ATTACHMENTS_PER_SESSION as i64);
        assert_eq!((again.width, again.height), (7, 1));
    }

    // Given a table written before the hash column existed, holding a tool image
    // Then opening upgrades it in place, hashes the old row, and a repeat of that image is found
    #[test]
    fn a_table_without_the_hash_column_is_upgraded_and_old_rows_deduplicate() {
        let connection = Connection::open_in_memory().unwrap();
        let old = png(2, 2);
        connection
            .execute_batch(
                "CREATE TABLE attachments (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, media_type TEXT NOT NULL,
                    name TEXT NOT NULL, size INTEGER NOT NULL, width INTEGER NOT NULL,
                    height INTEGER NOT NULL, bytes BLOB NOT NULL, created_at TEXT NOT NULL);
                 CREATE TABLE sessions (id TEXT PRIMARY KEY);
                 INSERT INTO sessions VALUES ('s1');",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO attachments VALUES ('old','s1','image/png','a.png',?1,2,2,?2,'t')",
                params![old.len() as i64, old],
            )
            .unwrap();
        let store = AttachmentStore::init(connection).unwrap();
        assert!(store.load("s1", "old").is_ok());
        let again = store
            .insert_from("s1", None, &png(2, 2), Origin::Tool)
            .unwrap();
        assert_eq!(again.attachment_id, "old");
    }

    // Given a PNG cut inside the chunks after its image data
    // Then it is refused, though every pixel decodes
    #[test]
    fn a_png_missing_its_closing_chunk_is_refused() {
        let store = store_with_session("s1");
        let whole = png(8, 8);
        let mut cut = whole.clone();
        cut.truncate(whole.len() - 4);
        assert!(matches!(
            store.insert("s1", None, &cut),
            Err(AttachmentError::Invalid(why)) if why.contains("truncated")
        ));
        // Bytes after IEND are tolerated.
        let mut padded = whole;
        padded.extend([0; 16]);
        assert!(store.insert("s1", None, &padded).is_ok());
    }

    // Given an animation whose canvas is past the checked size
    // Then it is refused rather than decoded
    #[test]
    fn an_animation_with_a_huge_canvas_is_refused() {
        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, 6000, 6000, &[]).unwrap();
            encoder
                .write_frame(&gif::Frame::from_rgb(2, 2, &[0; 12]))
                .unwrap();
        }
        let store = store_with_session("s1");
        assert!(matches!(
            store.insert("s1", None, &bytes),
            Err(AttachmentError::Invalid(why)) if why.contains("too large to check")
        ));
    }

    // Given a GIF whose screen is 1x1 but whose frame is larger
    // Then it is refused before any frame is decoded
    #[test]
    fn a_gif_frame_outside_its_screen_is_refused() {
        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, 1, 1, &[]).unwrap();
            let frame = gif::Frame::from_rgb(64, 64, &[0; 64 * 64 * 3]);
            encoder.write_frame(&frame).unwrap();
        }
        let store = store_with_session("s1");
        assert!(matches!(
            store.insert("s1", None, &bytes),
            Err(AttachmentError::Invalid(_))
        ));
    }

    #[test]
    fn a_refused_upload_stores_nothing() {
        let store = store_with_session("s1");
        assert!(
            store
                .insert("s1", None, b"<html>not an image</html>")
                .is_err()
        );
        assert!(store.insert("s1", None, b"<svg><unclosed").is_err());
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
