//! Provider-safe definitions and the bounded, cancellable tool-call boundary.
use super::Server;
use crate::{
    error::ToolError,
    tools::{DEFAULT_MAX_OUTPUT_BYTES, Tool, ToolContext, ToolErrorObj, ToolResult, ToolStatus},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const MAX_MEDIA_ITEMS: usize = 4;
const MAX_MEDIA_BASE64_BYTES: usize = 4 * 1024 * 1024;
const MAX_UI_HTML_BYTES: usize = 512 * 1024;

#[derive(Clone)]
pub struct McpTool {
    pub(super) wire: String,
    pub(super) original: String,
    server: Arc<Server>,
    schema: Value,
    pub(super) ui_resource_uri: Option<String>,
    pub(super) ui_url: Option<String>,
}

pub(super) fn wire_name(server: &str, original: &str) -> String {
    let safe: String = original
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut name = format!("mcp__{server}__{safe}");
    if name.len() > 64 {
        name.truncate(57);
        name.push('_');
        name.push_str(&hex::encode(Sha256::digest(original.as_bytes()))[..6]);
    }
    name
}

/// Keywords whose meaning depends on sibling keywords of the same schema object: merging a
/// `$ref` target with its siblings would change what `additionalProperties: false` or
/// `additionalItems` applies to, so the merged schema would not be the one the server
/// checks.
fn scope_conflict(target: &Value, siblings: &serde_json::Map<String, Value>) -> Result<(), String> {
    const GROUPS: [(&[&str], &[&str]); 2] = [
        (
            &["additionalProperties", "unevaluatedProperties"],
            &["properties", "patternProperties"],
        ),
        (
            &["additionalItems", "unevaluatedItems", "items"],
            &["items", "prefixItems"],
        ),
    ];
    let Some(target) = target.as_object() else {
        return Ok(());
    };
    for (closing, scoped) in GROUPS {
        for (a, b) in [(target, siblings), (siblings, target)] {
            for close in closing {
                for scope in scoped {
                    if close != scope && a.contains_key(*close) && b.contains_key(*scope) {
                        return Err(format!(
                            "$ref merge would change the scope of `{close}` (with `{scope}` on the other side)"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn normalize(schema: Value) -> Result<Value, String> {
    fn visit(
        value: &Value,
        root: &Value,
        refs: &mut Vec<String>,
        depth: usize,
    ) -> Result<Value, String> {
        if depth > 64 {
            return Err("schema nesting exceeds 64 levels".into());
        }
        match value {
            Value::Bool(_) => Ok(value.clone()),
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref") {
                    let reference = reference.as_str().ok_or("non-string schema reference")?;
                    if !reference.starts_with("#/") || refs.iter().any(|r| r == reference) {
                        return Err("external or recursive schema reference".into());
                    }
                    let target = root
                        .pointer(&reference[1..])
                        .ok_or("unresolved local schema reference")?;
                    scope_conflict(target, object)?;
                    refs.push(reference.into());
                    let mut resolved = visit(target, root, refs, depth + 1)?;
                    refs.pop();
                    let siblings: serde_json::Map<_, _> = object
                        .iter()
                        .filter(|(k, _)| k.as_str() != "$ref")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let normalized = visit(&Value::Object(siblings), root, refs, depth + 1)?;
                    // The target may itself be a `$ref` chain: check the keywords it
                    // resolves to, not only the ones written at the first hop.
                    scope_conflict(
                        &resolved,
                        normalized.as_object().ok_or("invalid reference siblings")?,
                    )?;
                    let output = resolved
                        .as_object_mut()
                        .ok_or("reference target is not an object schema")?;
                    // A sibling keyword the target also sets would replace it: the provider
                    // would see fewer constraints than the server checks. Annotations may
                    // replace; constraints only merge when they are new or identical.
                    for (key, value) in
                        normalized.as_object().ok_or("invalid reference siblings")?
                    {
                        let annotation = matches!(
                            key.as_str(),
                            "description" | "title" | "default" | "examples"
                        );
                        match output.get(key) {
                            Some(existing) if !annotation && existing != value => {
                                return Err(format!(
                                    "$ref sibling `{key}` conflicts with the referenced schema"
                                ));
                            }
                            _ => {
                                output.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    return Ok(resolved);
                }
                let mut output = serde_json::Map::new();
                for (key, value) in object {
                    if key == "$schema" {
                        continue;
                    }
                    if matches!(
                        key.as_str(),
                        "properties"
                            | "patternProperties"
                            | "dependentSchemas"
                            | "$defs"
                            | "definitions"
                    ) {
                        let entries = value
                            .as_object()
                            .ok_or("schema properties must be an object")?;
                        let mut properties = serde_json::Map::new();
                        for (name, schema) in entries {
                            properties.insert(name.clone(), visit(schema, root, refs, depth + 1)?);
                        }
                        if !matches!(key.as_str(), "$defs" | "definitions") {
                            output.insert(key.clone(), Value::Object(properties));
                        }
                        continue;
                    }
                    let normalized = match key.as_str() {
                        "items"
                        | "additionalProperties"
                        | "not"
                        | "contains"
                        | "propertyNames"
                        | "if"
                        | "then"
                        | "else" => visit(value, root, refs, depth + 1)?,
                        "prefixItems" | "allOf" | "anyOf" | "oneOf" => {
                            let entries = value
                                .as_array()
                                .ok_or("schema composition must be an array")?;
                            Value::Array(
                                entries
                                    .iter()
                                    .map(|entry| visit(entry, root, refs, depth + 1))
                                    .collect::<Result<_, _>>()?,
                            )
                        }
                        "required" => {
                            if !value
                                .as_array()
                                .is_some_and(|entries| entries.iter().all(Value::is_string))
                            {
                                return Err("required must be an array of strings".into());
                            }
                            value.clone()
                        }
                        "type" => {
                            let valid = |v: &Value| {
                                v.as_str().is_some_and(|t| {
                                    matches!(
                                        t,
                                        "object"
                                            | "array"
                                            | "string"
                                            | "number"
                                            | "integer"
                                            | "boolean"
                                            | "null"
                                    )
                                })
                            };
                            if !valid(value)
                                && !value.as_array().is_some_and(|entries| {
                                    !entries.is_empty() && entries.iter().all(valid)
                                })
                            {
                                return Err("invalid schema type".into());
                            }
                            value.clone()
                        }
                        // These are instance data, never schemas; preserve even a literal $ref.
                        "default" | "const" | "examples" => value.clone(),
                        "enum" => {
                            if !value.as_array().is_some_and(|entries| !entries.is_empty()) {
                                return Err("enum must be a nonempty array".into());
                            }
                            value.clone()
                        }
                        "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
                        | "maxProperties" => {
                            if value.as_u64().is_none() {
                                return Err(format!("{key} must be a non-negative integer"));
                            }
                            value.clone()
                        }
                        "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
                        | "multipleOf" => {
                            if !value.is_number() {
                                return Err(format!("{key} must be a number"));
                            }
                            value.clone()
                        }
                        "pattern" | "format" | "description" | "title" => {
                            if !value.is_string() {
                                return Err(format!("{key} must be a string"));
                            }
                            // Format values are dialect-specific; validate but omit for providers.
                            if key == "format" {
                                continue;
                            }
                            value.clone()
                        }
                        "uniqueItems" => {
                            if !value.is_boolean() {
                                return Err("uniqueItems must be a boolean".into());
                            }
                            value.clone()
                        }
                        _ => continue,
                    };
                    output.insert(key.clone(), normalized);
                }
                Ok(Value::Object(output))
            }
            _ => Err("schema must be an object or boolean".into()),
        }
    }
    let mut output = visit(&schema, &schema, &mut vec![], 0)?;
    let object = output
        .as_object_mut()
        .ok_or("input schema must be an object")?;
    if object.contains_key("oneOf") || object.contains_key("anyOf") || object.contains_key("allOf")
    {
        return Err("root schema composition cannot be normalized safely".into());
    }
    if object.get("type").is_some_and(|t| t != "object") {
        return Err("input schema describes non-object arguments".into());
    }
    object.insert("type".into(), json!("object"));
    Ok(output)
}

/// Largest redacted `structuredContent` kept, serialised. It is copied into the result metadata
/// that every client receives and every event log stores.
const MAX_STRUCTURED_BYTES: usize = 4 * 1024 * 1024;

/// Bytes one result may carry beyond its text, all kinds together: serialised structured
/// content, preview media (base64 or SVG text) and decoded model images. The per-item and
/// per-kind caps alone allow four 4 MiB previews plus four 9 MiB images plus unbounded
/// structured content. Structured content is charged first (an MCP App reads it), then each
/// block in order, its model image before its preview. `mcp_ui.html` and the text output have
/// their own caps and are not counted.
const MAX_RESULT_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Most omission notes written into one output, the rest counted in one line. They go last,
/// into the tail that output truncation keeps (about 32 KiB): a server sending thousands of bad
/// blocks must not push the first notes, structured content's included, out of it.
const MAX_OMISSION_NOTES: usize = 8;

/// Why a value was not serialised within a limit.
#[derive(Debug, PartialEq)]
pub(super) enum JsonLimit {
    TooLarge,
    Invalid,
}

/// Serialises `value` as JSON, stopping as soon as it passes `limit` bytes so an oversized value
/// is never copied whole. With `keep` false nothing is buffered: only the length is returned.
pub(super) fn json_within(
    value: &impl serde::Serialize,
    limit: usize,
    keep: bool,
) -> Result<(usize, Vec<u8>), JsonLimit> {
    struct Capped {
        len: usize,
        limit: usize,
        keep: bool,
        bytes: Vec<u8>,
    }
    impl std::io::Write for Capped {
        fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
            self.len = self.len.saturating_add(chunk.len());
            if self.len > self.limit {
                return Err(std::io::Error::other("over the limit"));
            }
            if self.keep {
                self.bytes.extend_from_slice(chunk);
            }
            Ok(chunk.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Capped {
        len: 0,
        limit,
        keep,
        bytes: vec![],
    };
    match serde_json::to_writer(&mut out, value) {
        Ok(()) => Ok((out.len, out.bytes)),
        Err(_) if out.len > limit => Err(JsonLimit::TooLarge),
        Err(_) => Err(JsonLimit::Invalid),
    }
}

fn mib(bytes: usize) -> usize {
    bytes / (1024 * 1024)
}

/// The type a client should render these bytes as, decided from the bytes alone: a server's
/// declared `mimeType` is a claim, and a preview typed from it fails or misrenders when it is
/// wrong. `None` for anything that is not one of the image types clients draw.
fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    use imagesize::ImageType;
    match imagesize::image_type(bytes) {
        Ok(ImageType::Png) => Some("image/png"),
        Ok(ImageType::Jpeg) => Some("image/jpeg"),
        Ok(ImageType::Gif) => Some("image/gif"),
        Ok(ImageType::Webp) => Some("image/webp"),
        Ok(_) => None,
        Err(_) => std::str::from_utf8(bytes)
            .is_ok_and(crate::session::attachments::svg::is_svg_document)
            .then_some("image/svg+xml"),
    }
}

/// The previews, model images and omission notes of one result, under one byte budget.
struct Payload {
    media: Vec<Value>,
    images: Vec<Vec<u8>>,
    omitted: Vec<String>,
    left: usize,
}

impl Payload {
    fn charge(&mut self, bytes: usize) -> bool {
        let fits = bytes <= self.left;
        if fits {
            self.left -= bytes;
        }
        fits
    }

    fn over_budget(&mut self, what: &str) {
        self.omitted.push(format!(
            "[{what} omitted: over the {} MiB budget for one result]",
            mib(MAX_RESULT_PAYLOAD_BYTES)
        ));
    }

    fn add_svg(&mut self, svg: String) {
        if self.media.len() >= MAX_MEDIA_ITEMS {
            return;
        }
        if self.charge(svg.len()) {
            self.media
                .push(json!({"mime_type": "image/svg+xml", "svg": svg}));
        } else {
            self.over_budget("SVG preview");
        }
    }

    /// Decodes one base64 image once, in the standard alphabet (RFC 4648 §4, the one MCP
    /// specifies and the only one a `data:` URL decodes), for the model and for a preview typed
    /// from its bytes. The model image is typed later from its bytes too, never from the
    /// declared `mimeType`. The image count and the byte budgets are checked on the decoded
    /// size computed from the base64 length, so a block past them is never decoded for the
    /// model. The error says why the model did not get it.
    fn add_image(
        &mut self,
        data: Option<&str>,
        model: bool,
        preview: bool,
        redact: &dyn Fn(&str) -> String,
    ) -> Result<(), String> {
        use base64::Engine as _;
        let data = data.ok_or("no data")?.trim();
        if data.len() > MAX_IMAGE_BLOCK_BASE64 {
            return Err(format!("{} bytes, too large to keep", data.len() / 4 * 3));
        }
        // Padded base64 comes in groups of four with at most two `=`: anything else is refused
        // before the size is estimated, so a block of padding cannot pass the budget as zero
        // bytes and still make the decoder allocate its full output buffer.
        if data.len() % 4 != 0 {
            return Err("not valid base64".into());
        }
        let padding = data.bytes().rev().take_while(|b| *b == b'=').count().min(2);
        let decoded_len = (data.len() / 4 * 3).saturating_sub(padding);
        let kept_bytes = self.images.iter().map(Vec::len).sum();
        let mut refused = None;
        if model {
            refused = crate::tools::tool_image_refusal(self.images.len(), kept_bytes, decoded_len)
                .map(|reason| format!("not kept, {reason}"))
                .or_else(|| {
                    (decoded_len > self.left).then(|| {
                        format!(
                            "not kept, over the {} MiB budget for one result",
                            mib(MAX_RESULT_PAYLOAD_BYTES)
                        )
                    })
                });
        }
        let to_model = model && refused.is_none();
        let to_preview = preview
            && self.media.len() < MAX_MEDIA_ITEMS
            && !data.is_empty()
            && data.len() <= MAX_MEDIA_BASE64_BYTES
            && redact(data) == data;
        if !to_model && !to_preview {
            return refused.map_or(Ok(()), Err);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| "not valid base64".to_string())?;
        let media_type = to_preview.then(|| sniff_media_type(&bytes)).flatten();
        if to_model {
            // Canonical padding makes the decoded length the estimate checked above.
            let charged = self.charge(bytes.len());
            debug_assert!(charged, "the budget was checked on the decoded length");
            self.images.push(bytes);
        }
        if let Some(media_type) = media_type {
            if self.charge(data.len()) {
                self.media
                    .push(json!({"mime_type": media_type, "data": data}));
            } else {
                self.over_budget("image preview");
            }
        }
        refused.map_or(Ok(()), Err)
    }
}

/// Redacts `structuredContent` and measures it, before and after redaction (a replacement can
/// be longer than the secret it hides). `Err` is the note naming why it was dropped.
fn bounded_structured(structured: &Value, secrets: &[String]) -> Result<(Value, usize), String> {
    let too_large = || {
        format!(
            "[structured content omitted: larger than {} MiB]",
            mib(MAX_STRUCTURED_BYTES)
        )
    };
    let invalid = |error| match error {
        JsonLimit::TooLarge => too_large(),
        JsonLimit::Invalid => "[structured content omitted: not serialisable]".to_string(),
    };
    json_within(structured, MAX_STRUCTURED_BYTES, false).map_err(invalid)?;
    let redacted = super::redaction::redact_value(structured, secrets);
    let (len, _) = json_within(&redacted, MAX_STRUCTURED_BYTES, false).map_err(invalid)?;
    Ok((redacted, len))
}

pub(super) fn extract_ui_resource_uri(meta: &Value) -> Option<String> {
    meta["ui"]["resourceUri"]
        .as_str()
        .or_else(|| meta["ui/resourceUri"].as_str())
        .filter(|uri| uri.starts_with("ui://") && uri.len() <= 512)
        .map(ToOwned::to_owned)
}

pub(super) fn extract_ui_url(meta: &Value) -> Option<String> {
    meta["ui"]["url"]
        .as_str()
        .or_else(|| meta["ui/url"].as_str())
        .filter(|url| {
            (url.starts_with("http://") || url.starts_with("https://")) && url.len() <= 512
        })
        .map(ToOwned::to_owned)
}

/// Converts a `tools/call` result to a bounded tool result. Structured content is redacted
/// as a value before it is serialised: serialisation escapes newlines, quotes and
/// backslashes, so a raw-value text replacement afterwards would miss such a secret.
pub(super) fn convert(
    value: &Value,
    server: &str,
    original: &str,
    secrets: &[String],
) -> ToolResult {
    let redact = |text: &str| super::redaction::redact(text, secrets);
    let content = value["content"].as_array();
    let has_text = content.is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "text"));
    let mut parts = vec![];
    let mut payload = Payload {
        media: vec![],
        images: vec![],
        omitted: vec![],
        left: MAX_RESULT_PAYLOAD_BYTES,
    };
    // Charged before any block: an MCP App reads its state from it.
    let redacted_structured = match value.get("structuredContent").filter(|v| !v.is_null()) {
        None => None,
        Some(structured) => match bounded_structured(structured, secrets) {
            Ok((redacted, len)) if payload.charge(len) => Some(redacted),
            Ok(_) => {
                payload.over_budget("structured content");
                None
            }
            Err(note) => {
                payload.omitted.push(note);
                None
            }
        },
    };
    let mut resource_uri =
        extract_ui_resource_uri(&value["_meta"]).filter(|uri| redact(uri) == *uri);
    let mut ui_url = extract_ui_url(&value["_meta"]).filter(|url| redact(url) == *url);
    let mut ui_html: Option<String> = None;
    if let Some(blocks) = content {
        for block in blocks {
            match block["type"].as_str().unwrap_or("") {
                "text" => {
                    let text = block["text"].as_str().unwrap_or("");
                    parts.push(text.to_string());
                    let trimmed = text.trim();
                    if payload.media.len() < MAX_MEDIA_ITEMS
                        && trimmed.len() <= MAX_UI_HTML_BYTES
                        && crate::session::attachments::svg::is_svg_document(trimmed)
                    {
                        payload.add_svg(redact(trimmed));
                    }
                }
                "resource_link" => parts.push(format!(
                    "{} ({})",
                    block["uri"].as_str().unwrap_or(""),
                    block["name"].as_str().unwrap_or("")
                )),
                "image" => {
                    let mime = block["mimeType"].as_str().unwrap_or("unknown");
                    // Decoded for the model; the note stays only when it could not be.
                    if let Err(note) =
                        payload.add_image(block["data"].as_str(), true, true, &redact)
                    {
                        payload.omitted.push(format!("[image: {mime}, {note}]"));
                    }
                }
                "audio" => {
                    let bytes = block["data"].as_str().map_or(0, |data| {
                        (data.len().saturating_mul(3) / 4)
                            .saturating_sub(data.chars().rev().take_while(|c| *c == '=').count())
                    });
                    parts.push(format!(
                        "[audio: {}, {bytes} bytes]",
                        block["mimeType"].as_str().unwrap_or("unknown")
                    ));
                }
                "resource" => {
                    let res = &block["resource"];
                    let uri = res["uri"].as_str().unwrap_or("");
                    let mime = res["mimeType"].as_str().unwrap_or("");
                    // A screenshot can also come as an embedded resource with an image type.
                    if mime.starts_with("image/") && res["blob"].is_string() {
                        let preview = !uri.starts_with("ui://");
                        if let Err(note) =
                            payload.add_image(res["blob"].as_str(), true, preview, &redact)
                        {
                            payload
                                .omitted
                                .push(format!("[embedded image omitted: {note}]"));
                        }
                    } else {
                        parts.push("[embedded resource omitted]".into());
                    }
                    if uri.starts_with("ui://") || mime.starts_with("text/html") {
                        if resource_uri.is_none()
                            && uri.starts_with("ui://")
                            && uri.len() <= 512
                            && redact(uri) == uri
                        {
                            resource_uri = Some(uri.to_owned());
                        }
                        if ui_html.is_none()
                            && let Some(html) = res["text"].as_str()
                            && html.len() <= MAX_UI_HTML_BYTES
                        {
                            ui_html = Some(redact(html));
                        }
                    } else if mime == "image/svg+xml"
                        && !res["blob"].is_string()
                        && payload.media.len() < MAX_MEDIA_ITEMS
                        && let Some(svg) = res["text"].as_str()
                        && svg.len() <= MAX_UI_HTML_BYTES
                        && crate::session::attachments::svg::is_svg_document(svg)
                    {
                        payload.add_svg(redact(svg));
                    }
                }
                _ => parts.push("[unsupported MCP content]".into()),
            }
        }
    }
    if !has_text && let Some(structured) = &redacted_structured {
        parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }
    if let Some(structured) = &redacted_structured {
        let board = &structured["excalidashBoard"];
        if let Some(data_url) = board["png"].as_str().or_else(|| structured["png"].as_str())
            && let Some(b64) = data_url.strip_prefix("data:image/png;base64,")
        {
            // A preview only: the board image is not a model image, a bad one is just skipped.
            let _ = payload.add_image(Some(b64), false, true, &redact);
        }
        if ui_url.is_none()
            && let Some(url) = board["url"].as_str().or_else(|| structured["url"].as_str())
            && (url.starts_with("http://") || url.starts_with("https://"))
            && url.len() <= 512
        {
            ui_url = Some(url.to_owned());
        }
    }
    // Last and few, so the bounded output's kept tail always names what was dropped.
    if payload.omitted.len() > MAX_OMISSION_NOTES {
        let more = payload.omitted.len() - MAX_OMISSION_NOTES;
        payload.omitted.truncate(MAX_OMISSION_NOTES);
        payload.omitted.push(format!("[{more} more items omitted]"));
    }
    parts.append(&mut payload.omitted);
    let Payload { media, images, .. } = payload;
    let mut result = ToolResult::bounded_ok(&redact(&parts.join("\n")), DEFAULT_MAX_OUTPUT_BYTES);
    if value["isError"] == true {
        result.status = ToolStatus::Error;
        result.error = Some(ToolErrorObj {
            code: "mcp_tool_error".into(),
            plugin_id: Some(server.into()),
            message: if result.output.is_empty() {
                "MCP tool failed".into()
            } else {
                result.output.clone()
            },
        });
    }
    let redacted_tool = redact(original);
    let mut metadata = json!({"mcp_server": server, "mcp_tool": redacted_tool});
    if let Some(structured) = redacted_structured {
        metadata["mcp_structured"] = structured;
    }
    if !media.is_empty() {
        metadata["mcp_media"] = Value::Array(media);
    }
    if resource_uri.is_some() || ui_html.is_some() || ui_url.is_some() {
        let mut ui = json!({"server": server, "tool": redacted_tool});
        if let Some(uri) = resource_uri.take() {
            ui["resource_uri"] = Value::String(uri);
        }
        if let Some(html) = ui_html.take() {
            ui["html"] = Value::String(html);
        }
        if let Some(url) = ui_url.take() {
            ui["url"] = Value::String(url);
        }
        metadata["mcp_ui"] = ui;
    }
    result.metadata = metadata;
    for (at, bytes) in images.into_iter().enumerate() {
        result.images.push(crate::tools::ToolImage {
            name: format!("{}-{}.img", redacted_tool, at + 1),
            bytes,
        });
    }
    result
}

/// Largest base64 image block decoded, in characters: 12 MiB, about 9 MiB of image. Bigger
/// blocks are not decoded at all, so a server cannot make the agent allocate unbounded memory.
const MAX_IMAGE_BLOCK_BASE64: usize = 12 * 1024 * 1024;

impl McpTool {
    pub(super) fn server_id(&self) -> &str {
        &self.server.id
    }
    /// Builds a provider-safe tool. Errors name the tool redacted. A tool whose name carries
    /// a configured secret is refused rather than renamed: its name would be sent back to
    /// the server on every call, and any rename would still derive from the secret.
    pub(super) fn new(server: Arc<Server>, definition: rmcp::model::Tool) -> Result<Self, String> {
        let original = definition.name.to_string();
        let secrets = server.secrets();
        if server.redact(&original) != original {
            return Err(format!(
                "{}: tool name contains a configured secret",
                server.redact(&original)
            ));
        }
        let raw_def = serde_json::to_value(&definition).unwrap_or_default();
        let ui_resource_uri =
            extract_ui_resource_uri(&raw_def["_meta"]).filter(|uri| server.redact(uri) == *uri);
        let ui_url = extract_ui_url(&raw_def["_meta"])
            .or_else(|| {
                let is_excalidraw = server.id == "excalidraw"
                    || server
                        .config
                        .command
                        .iter()
                        .any(|c| c.contains("excalidraw"));
                is_excalidraw.then(|| {
                    server
                        .config
                        .env
                        .get("EXPRESS_SERVER_URL")
                        .cloned()
                        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
                })
            })
            .filter(|url| {
                (url.starts_with("http://") || url.starts_with("https://"))
                    && url.len() <= 512
                    && server.redact(url) == *url
            });
        let parameters = normalize(Value::Object((*definition.input_schema).clone()))
            .map_err(|e| format!("{original}: {e}"))?;
        let parameters = super::redaction::redact_value(&parameters, &secrets);
        let wire = wire_name(&server.id, &original);
        let description =
            super::redaction::redact(definition.description.as_deref().unwrap_or(""), &secrets);
        let schema = json!({"description": format!("[mcp:{}] {description}", server.id), "parameters": parameters});
        Ok(Self {
            wire,
            original,
            server,
            schema,
            ui_resource_uri,
            ui_url,
        })
    }

    fn error(&self, code: &str, message: &str) -> ToolResult {
        let mut result =
            ToolResult::bounded_ok(&self.server.redact(message), DEFAULT_MAX_OUTPUT_BYTES);
        result.status = ToolStatus::Error;
        result.error = Some(ToolErrorObj {
            code: code.into(),
            plugin_id: Some(self.server.id.clone()),
            message: result.output.clone(),
        });
        result.with_metadata(self.metadata())
    }

    /// Published with every result; the name is redacted like any other server text.
    fn metadata(&self) -> Value {
        json!({"mcp_server": self.server.id, "mcp_tool": self.server.redact(&self.original)})
    }

    fn attach_ui(&self, mut result: ToolResult) -> ToolResult {
        if self.ui_resource_uri.is_none() && self.ui_url.is_none() {
            return result;
        }
        let ui = result.metadata["mcp_ui"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut merged = serde_json::Map::from_iter([
            ("server".into(), json!(self.server.id)),
            ("tool".into(), json!(self.server.redact(&self.original))),
        ]);
        merged.extend(ui);
        if !merged.contains_key("resource_uri")
            && let Some(uri) = &self.ui_resource_uri
        {
            merged.insert("resource_uri".into(), json!(uri));
        }
        if !merged.contains_key("url")
            && let Some(url) = &self.ui_url
        {
            merged.insert("url".into(), json!(url));
        }
        result.metadata["mcp_ui"] = Value::Object(merged);
        result
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.wire
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
        _cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(arguments) = args.as_object() else {
                return Ok(self.error("invalid_args", "MCP arguments must be an object"));
            };
            let ready = self
                .server
                .ready
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|r| (r.peer.clone(), r.disconnected.clone()));
            let Some((peer, disconnected)) = ready else {
                return Ok(self.error("mcp_disconnected", "MCP server is disconnected"));
            };
            let params = rmcp::model::CallToolRequestParams::new(self.original.clone())
                .with_arguments(arguments.clone());
            let request = rmcp::model::ClientRequest::CallToolRequest(
                rmcp::model::CallToolRequest::new(params),
            );
            let deadline =
                tokio::time::sleep(Duration::from_millis(self.server.config.call_timeout_ms));
            tokio::pin!(deadline);
            let mut handle = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(ToolResult::interrupted()),
                _ = disconnected.cancelled() => return Ok(self.error("mcp_disconnected", "MCP server is disconnected")),
                _ = &mut deadline => return Ok(self.error("mcp_timeout", "MCP tool call timed out")),
                result = peer.send_cancellable_request(request, rmcp::service::PeerRequestOptions::no_options()) => match result {
                    Ok(handle) => handle,
                    Err(_) => return Ok(self.error("mcp_disconnected", "MCP server is disconnected")),
                }
            };
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => ToolResult::interrupted(),
                _ = disconnected.cancelled() => self.error("mcp_disconnected", "MCP server is disconnected"),
                _ = &mut deadline => self.error("mcp_timeout", "MCP tool call timed out"),
                response = &mut handle.rx => {
                    return Ok(match response {
                        Ok(Ok(response)) => match serde_json::to_value(response) {
                            Ok(value) => self.attach_ui(
                                convert(&value, &self.server.id, &self.original, &self.server.secrets())
                            ),
                            Err(_) => self.error("mcp_tool_error", "invalid MCP tool result"),
                        },
                        Ok(Err(rmcp::ServiceError::McpError(_))) => self.error("mcp_tool_error", "MCP server rejected the call"),
                        _ => self.error("mcp_disconnected", "MCP server is disconnected"),
                    });
                }
            };
            // rmcp 3.5 RequestHandle has no cancel-on-drop: send the notification explicitly.
            let _ = tokio::time::timeout(
                Duration::from_millis(100),
                handle.cancel(Some("WithNav call cancelled".into())),
            )
            .await;
            Ok(result.with_metadata(self.metadata()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::attachments::fixtures;

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    const MIB: usize = 1024 * 1024;

    #[test]
    fn a_self_closing_svg_is_a_preview_and_text_merely_mentioning_svg_is_not() {
        for svg in [
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"/>",
            "<svg aria-label=\"a>b\"/>",
            "<?xml version=\"1.0\"?><svg viewBox=\"0 0 1 1\"><rect/></svg>",
        ] {
            let result = convert(
                &json!({"content":[{"type":"text","text":format!("  {svg}\n")}]}),
                "s",
                "t",
                &[],
            );
            assert_eq!(
                result.metadata["mcp_media"],
                json!([{"mime_type": "image/svg+xml", "svg": svg}]),
                "{svg}"
            );
        }
        for text in [
            "<svgfoo/>",
            "<svg width=\"1\">",
            "<html><svg/></html>",
            "see <svg/> here",
        ] {
            let result = convert(
                &json!({"content":[{"type":"text","text":text}]}),
                "s",
                "t",
                &[],
            );
            assert!(result.metadata.get("mcp_media").is_none(), "{text}");
        }
    }

    // Given image payloads whose declared type disagrees with their bytes
    // Then every preview is typed from the bytes, and bytes of no image type are not previewed
    #[test]
    fn a_preview_is_typed_from_its_bytes_not_its_declared_type() {
        let png = b64(&fixtures::png(2, 2));
        let gif = b64(&fixtures::gif(2, 2));
        let jpeg = b64(&fixtures::jpeg(2, 2));
        let svg = b64(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>");
        let value = json!({
            "content": [
                {"type": "image", "mimeType": "image/jpeg", "data": png},
                {"type": "resource", "resource": {"uri": "x://a", "mimeType": "image/png", "blob": gif}},
                {"type": "image", "mimeType": "image/png", "data": svg},
                {"type": "image", "mimeType": "image/png", "data": "AAAA"},
            ],
            "structuredContent": {"png": format!("data:image/png;base64,{jpeg}")},
        });
        let result = convert(&value, "s", "t", &[]);
        let types: Vec<_> = result.metadata["mcp_media"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["mime_type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            ["image/png", "image/gif", "image/svg+xml", "image/jpeg"]
        );
        // The model still gets every decodable block; its type is decided from the bytes later.
        assert_eq!(result.images.len(), 4);
        // A byte-order mark before the root element is still an SVG document.
        assert_eq!(
            sniff_media_type("\u{feff}<svg xmlns=\"http://www.w3.org/2000/svg\"/>".as_bytes()),
            Some("image/svg+xml")
        );
    }

    // Given the same image in the URL-safe and in the standard base64 alphabet
    // Then the URL-safe one is neither previewed nor given to the model, and says why
    #[test]
    fn only_the_standard_base64_alphabet_is_accepted_for_previews_and_the_model() {
        use base64::Engine as _;
        let mut bytes = fixtures::png(1, 1);
        bytes.extend([0xfb, 0xff, 0xbf, 0xfe]);
        let url_safe = base64::engine::general_purpose::URL_SAFE.encode(&bytes);
        assert!(url_safe.contains(['-', '_']));
        let rejected = convert(
            &json!({"content":[{"type":"image","mimeType":"image/png","data":url_safe}]}),
            "s",
            "t",
            &[],
        );
        assert!(rejected.metadata.get("mcp_media").is_none());
        assert!(rejected.images.is_empty());
        assert!(rejected.output.contains("not valid base64"));
        let accepted = convert(
            &json!({"content":[{"type":"image","mimeType":"image/png","data":b64(&bytes)}]}),
            "s",
            "t",
            &[],
        );
        assert_eq!(accepted.metadata["mcp_media"][0]["mime_type"], "image/png");
        assert_eq!(accepted.images.len(), 1);
    }

    // Given structured content over its cap, after a text block long enough to be truncated
    // Then it is not copied into the metadata and the kept tail of the output says so
    #[test]
    fn oversized_structured_content_is_dropped_with_a_note() {
        let big = json!({"blob": "x".repeat(MAX_STRUCTURED_BYTES)});
        let result = convert(
            &json!({"content":[{"type":"text","text":"y".repeat(200_000)}], "structuredContent": big}),
            "s",
            "t",
            &[],
        );
        assert!(result.metadata.get("mcp_structured").is_none());
        assert!(
            result
                .output
                .ends_with("[structured content omitted: larger than 4 MiB]"),
            "{}",
            &result.output[result.output.len() - 100..]
        );
        // Under the cap before redaction, over it after: a replacement is longer than a
        // one-character secret.
        let grows = json!({"blob": "s".repeat(MAX_STRUCTURED_BYTES / 2)});
        let result = convert(
            &json!({"content":[], "structuredContent": grows}),
            "s",
            "t",
            &["s".to_string()],
        );
        assert!(result.metadata.get("mcp_structured").is_none());
        assert!(result.output.contains("larger than 4 MiB"));
    }

    // Given oversized structured content and twenty broken images between long texts
    // Then the notes survive truncation: the first ones in full, the rest counted
    #[test]
    fn omission_notes_are_few_and_kept_in_the_output_tail() {
        let long = json!({"type": "text", "text": "y".repeat(100_000)});
        let broken = json!({"type": "image", "mimeType": "image/png", "data": "!!"});
        let mut content = vec![long.clone()];
        for _ in 0..20 {
            content.push(broken.clone());
            content.push(long.clone());
        }
        let result = convert(
            &json!({"content": content, "structuredContent": {"blob": "x".repeat(MAX_STRUCTURED_BYTES)}}),
            "s",
            "t",
            &[],
        );
        assert!(result.truncated.is_some());
        let tail = &result.output[result.output.len() - 2000..];
        assert!(tail.contains("[structured content omitted: larger than 4 MiB]"));
        assert_eq!(
            tail.matches("[image: image/png, not valid base64]").count(),
            7
        );
        assert!(tail.ends_with("[13 more items omitted]"), "{tail}");
    }

    // Given just under 3 MiB of structured content and three images of 1.5 MiB (2 MiB of base64 each)
    // Then the 8 MiB result budget keeps the first image and its preview, the second image
    //      without its preview, and refuses the third, naming each drop
    #[test]
    fn previews_images_and_structured_content_share_one_budget() {
        let mut bytes = fixtures::png(4, 4);
        bytes.resize(3 * MIB / 2, 0);
        let data = b64(&bytes);
        let block = json!({"type": "image", "mimeType": "image/png", "data": data});
        let structured = json!({"state": "z".repeat(3 * MIB - 64)});
        let result = convert(
            &json!({"content": [block, block, block], "structuredContent": structured}),
            "s",
            "t",
            &[],
        );
        assert!(result.metadata["mcp_structured"]["state"].is_string());
        assert_eq!(result.images.len(), 2);
        assert_eq!(result.metadata["mcp_media"].as_array().unwrap().len(), 1);
        let note = "over the 8 MiB budget for one result";
        assert!(
            result
                .output
                .contains(&format!("[image: image/png, not kept, {note}]"))
        );
        assert!(
            result
                .output
                .contains(&format!("[image preview omitted: {note}]"))
        );
    }

    #[test]
    fn json_is_measured_up_to_its_limit_and_no_further() {
        let value = json!("abc");
        assert_eq!(json_within(&value, 5, true), Ok((5, b"\"abc\"".to_vec())));
        assert_eq!(json_within(&value, 5, false), Ok((5, vec![])));
        assert_eq!(json_within(&value, 4, true), Err(JsonLimit::TooLarge));
        // Escapes count as written: a newline is two bytes, a multibyte character its UTF-8 length.
        assert_eq!(json_within(&json!("\né"), 6, false).map(|(n, _)| n), Ok(6));
        assert_eq!(
            json_within(&json!("\né"), 5, false),
            Err(JsonLimit::TooLarge)
        );
    }
    #[test]
    fn provider_safe_names_and_hashes() {
        assert_eq!(wire_name("github", "a.b 🦀"), "mcp__github__a_b__");
        let name = wire_name("github", &"a".repeat(100));
        assert_eq!(name.len(), 64);
        assert_ne!(name, wire_name("github", &"a".repeat(101)));
    }
    #[test]
    fn local_references_and_unsupported_schemas() {
        assert_eq!(
            normalize(json!({"properties":{"format":{"type":"string"}, "$ref":{"type":"number"}}}))
                .unwrap()["properties"]["format"]["type"],
            "string"
        );
        let result = normalize(json!({"$schema":"x", "$defs":{"s":{"type":"string"}}, "properties":{"s":{"$ref":"#/$defs/s"}}})).unwrap();
        assert_eq!(
            result,
            json!({"type":"object", "properties":{"s":{"type":"string"}}})
        );
        assert!(normalize(json!({"$ref":"https://bad"})).is_err());
        assert!(
            normalize(
                json!({"$defs":{"s":{"$ref":"#/$defs/s"}},"properties":{"s":{"$ref":"#/$defs/s"}}})
            )
            .is_err()
        );
        assert!(normalize(json!({"oneOf":[]})).is_err());
    }
    // Given an MCP result with a screenshot, an embedded image resource, broken and oversized blocks
    // Then the decodable images become tool images, the rest are named in the output, and the
    //      output never carries base64
    #[test]
    fn image_blocks_become_tool_images_and_bad_ones_are_named() {
        use base64::Engine as _;
        let png = crate::session::attachments::fixtures::png(3, 3);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let huge = "A".repeat(MAX_IMAGE_BLOCK_BASE64 + 4);
        let value = json!({"content": [
            {"type": "text", "text": "canvas"},
            {"type": "image", "mimeType": "image/png", "data": b64},
            {"type": "resource", "resource": {"uri": "x://shot", "mimeType": "image/png", "blob": b64}},
            {"type": "image", "mimeType": "image/png", "data": "!!not base64!!"},
            {"type": "image", "mimeType": "image/png", "data": huge},
        ]});
        let result = convert(&value, "excalidraw", "get_canvas_screenshot", &[]);
        assert_eq!(result.images.len(), 2);
        assert_eq!(result.images[0].bytes, png);
        assert!(result.output.contains("not valid base64"));
        assert!(result.output.contains("too large to keep"));
        assert!(!result.output.contains(&b64[..16]));
        assert_eq!(result.images[0].name, "get_canvas_screenshot-1.img");
    }

    #[test]
    fn at_most_four_image_blocks_are_decoded() {
        let png = crate::session::attachments::fixtures::png(1, 1);
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let blocks: Vec<Value> = (0..6)
            .map(|_| json!({"type": "image", "mimeType": "image/png", "data": b64}))
            .collect();
        let result = convert(&json!({"content": blocks}), "s", "t", &[]);
        assert_eq!(result.images.len(), 4);
        assert_eq!(result.output.matches("at most 4 images").count(), 2);
    }

    // Given image blocks (inline and embedded resource) whose decoded sizes reach the result
    //       budget exactly, then one more byte
    // When the result is converted
    // Then the blocks up to the budget are kept, the next one is refused before decoding and
    //      named in the output
    #[test]
    fn image_blocks_past_the_result_budget_are_refused_before_decoding() {
        use base64::Engine as _;
        // Base64 longer than a preview takes: only the model images are charged.
        let half =
            base64::engine::general_purpose::STANDARD
                .encode(vec![0u8; MAX_RESULT_PAYLOAD_BYTES / 2]);
        let one = base64::engine::general_purpose::STANDARD.encode([0u8]);
        let note = "over the 8 MiB budget for one result";
        let value = json!({"content": [
            {"type": "image", "mimeType": "image/png", "data": half},
            {"type": "resource", "resource": {"uri": "x://shot", "mimeType": "image/png", "blob": half}},
            {"type": "image", "mimeType": "image/png", "data": one},
        ]});
        let result = convert(&value, "s", "t", &[]);
        assert_eq!(result.images.len(), 2);
        let kept: usize = result.images.iter().map(|i| i.bytes.len()).sum();
        assert_eq!(kept, MAX_RESULT_PAYLOAD_BYTES);
        assert_eq!(result.output.matches(note).count(), 1);

        // A block made of padding counts at its full size, not as zero bytes.
        let padding = "=".repeat(MAX_IMAGE_BLOCK_BASE64);
        let value = json!({"content": [
            {"type": "image", "mimeType": "image/png", "data": half},
            {"type": "image", "mimeType": "image/png", "data": padding},
        ]});
        let result = convert(&value, "s", "t", &[]);
        assert_eq!(result.images.len(), 1);
        assert!(result.output.contains(note), "{}", result.output);
    }

    #[test]
    fn reference_siblings_that_would_replace_constraints_skip_the_tool() {
        let schema = json!({
            "$defs": {"args": {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]}},
            "properties": {"x": {"$ref": "#/$defs/args", "properties": {"b": {"type": "string"}}, "required": ["b"]}}
        });
        let error = normalize(schema).unwrap_err();
        assert!(
            error.contains("required") || error.contains("properties"),
            "{error}"
        );
        let merged = normalize(json!({
            "$defs": {"s": {"type": "string"}},
            "properties": {"x": {"$ref": "#/$defs/s", "minLength": 1, "description": "x"}}
        }))
        .unwrap();
        assert_eq!(
            merged["properties"]["x"],
            json!({"type":"string","minLength":1,"description":"x"})
        );
    }

    #[test]
    fn reference_merges_that_change_keyword_scope_skip_the_tool() {
        let reference = |target: Value, sibling: Value| {
            let mut x = sibling;
            x["$ref"] = json!("#/$defs/t");
            normalize(json!({"$defs": {"t": target}, "properties": {"x": x}}))
        };
        let closed = json!({"type":"object","additionalProperties":false});
        let error = reference(
            closed.clone(),
            json!({"properties":{"b":{"type":"string"}}}),
        )
        .unwrap_err();
        assert!(error.contains("additionalProperties"), "{error}");
        for (target, sibling) in [
            (
                json!({"properties":{"a":{}}}),
                json!({"additionalProperties":false}),
            ),
            (
                json!({"unevaluatedProperties":false}),
                json!({"patternProperties":{"^a":{}}}),
            ),
            (
                json!({"patternProperties":{"^a":{}}}),
                json!({"unevaluatedProperties":false}),
            ),
            (json!({"prefixItems":[{}]}), json!({"items":false})),
            (json!({"items":[{}]}), json!({"additionalItems":false})),
            (
                json!({"additionalItems":false}),
                json!({"prefixItems":[{}]}),
            ),
        ] {
            assert!(
                reference(target.clone(), sibling.clone()).is_err(),
                "{target} {sibling}"
            );
        }
        // A chained reference reaches `additionalProperties` only after resolution.
        let chained = normalize(json!({
            "$defs": {
                "closed": {"type":"object","additionalProperties":false},
                "alias": {"$ref":"#/$defs/closed"}
            },
            "properties": {"x": {"$ref":"#/$defs/alias","properties":{"b":{}}}}
        }))
        .unwrap_err();
        assert!(chained.contains("additionalProperties"), "{chained}");
        assert!(reference(closed, json!({"description":"d"})).is_ok());
        assert!(reference(json!({"items":{"type":"string"}}), json!({"maxItems":2})).is_ok());
    }

    #[test]
    fn results_are_bounded_including_error_messages() {
        let png = b64(&crate::session::attachments::fixtures::png(1, 1));
        let result = convert(
            &json!({"isError":true, "content":[{"type":"text", "text":"a".repeat(100000)}]}),
            "test",
            "big",
            &[],
        );
        assert_eq!(result.status, ToolStatus::Error);
        assert_eq!(result.truncated.unwrap().original_bytes, 100000);
        assert!(result.error.unwrap().message.len() <= DEFAULT_MAX_OUTPUT_BYTES);
        let structured = convert(
            &json!({"content":[], "structuredContent":{"a":1}}),
            "test",
            "echo",
            &[],
        );
        assert_eq!(structured.output, "{\n  \"a\": 1\n}");
        let mixed = convert(
            &json!({"content":[{"type":"resource_link","uri":"file:///x","name":"x"},{"type":"image","mimeType":"image/png","data":png}]}),
            "test",
            "echo",
            &[],
        );
        assert_eq!(mixed.output, "file:///x (x)");
        assert_eq!(mixed.images.len(), 1);
        assert_eq!(
            mixed.metadata["mcp_media"],
            json!([{"mime_type": "image/png", "data": png}])
        );
        let ui = convert(
            &json!({
                "_meta": {"ui": {"resourceUri": "ui://excalidraw/app.html"}},
                "content": [
                    {"type": "text", "text": "<svg viewBox=\"0 0 10 10\"></svg>"},
                    {"type": "resource", "resource": {"uri": "ui://excalidraw/app.html", "mimeType": "text/html;profile=mcp-app", "text": "<html>ok</html>"}}
                ]
            }),
            "excalidraw",
            "draw",
            &[],
        );
        assert_eq!(
            ui.metadata["mcp_ui"],
            json!({
                "server": "excalidraw",
                "tool": "draw",
                "resource_uri": "ui://excalidraw/app.html",
                "html": "<html>ok</html>"
            })
        );
        assert_eq!(
            ui.metadata["mcp_media"],
            json!([{"mime_type": "image/svg+xml", "svg": "<svg viewBox=\"0 0 10 10\"></svg>"}])
        );
        let dash = convert(
            &json!({
                "content": [{"type": "text", "text": "\"Board\" is shown to the user"}],
                "structuredContent": {
                    "excalidashBoard": {
                        "id": "b1",
                        "name": "Board",
                        "url": "https://excalidash.example/b/b1",
                        "png": format!("data:image/png;base64,{png}")
                    }
                }
            }),
            "excalidash",
            "display_board",
            &[],
        );
        assert_eq!(
            dash.metadata["mcp_structured"]["excalidashBoard"]["name"],
            "Board"
        );
        assert_eq!(
            dash.metadata["mcp_media"],
            json!([{"mime_type": "image/png", "data": png}])
        );
        assert_eq!(
            dash.metadata["mcp_ui"]["url"],
            "https://excalidash.example/b/b1"
        );
    }

    #[test]
    fn redact_before_bounding_and_derive_one_consistent_error_result() {
        let secret = "TOKEN";
        let text = format!("{}{} retained tail", "x".repeat(65534), secret);
        let result = convert(
            &json!({"isError":true,"content":[{"type":"text","text":text}]}),
            "fake",
            "echo",
            &[secret.to_string()],
        );
        assert!(!result.output.contains("TOK"));
        assert_eq!(result.error.as_ref().unwrap().message, result.output);
        assert_eq!(result.truncated.unwrap().original_bytes, 65534 + 10 + 14);
        let expanded = convert(
            &json!({"isError":true,"content":[{"type":"text","text":"s".repeat(8000)}]}),
            "fake",
            "echo",
            &["s".to_string()],
        );
        assert_eq!(expanded.truncated.unwrap().original_bytes, 80000);
        assert_eq!(expanded.error.unwrap().message, expanded.output);
        assert!(expanded.output.len() <= 65536);
    }

    #[test]
    fn structured_content_secrets_are_redacted_before_serialisation() {
        for secret in [
            "multi\nline-sentinel",
            "quote\"sentinel",
            "back\\slash-sentinel",
        ] {
            let result = convert(
                &json!({"content":[], "structuredContent":{"token":secret,"list":[secret]}}),
                "fake",
                secret,
                &[secret.to_string()],
            );
            assert!(!result.output.contains("sentinel"), "{}", result.output);
            assert!(!result.metadata.to_string().contains("sentinel"));
            let text = serde_json::to_string(&json!({"token": secret})).unwrap();
            let result = convert(
                &json!({"content":[{"type":"text","text":text}]}),
                "fake",
                "echo",
                &[secret.to_string()],
            );
            assert!(!result.output.contains("sentinel"), "{}", result.output);
        }
    }

    #[test]
    fn schema_instance_data_is_not_rewritten_and_malformed_shapes_fail() {
        let data = json!({"$ref":"literal", "$schema":"literal", "format":"literal"});
        let schema = normalize(json!({"properties":{"x":{"type":"object","default":data,"const":data,"enum":[data],"examples":[data]}}})).unwrap();
        assert_eq!(schema["properties"]["x"]["default"], data);
        assert_eq!(schema["properties"]["x"]["enum"][0], data);
        for schema in [
            json!({"properties":[]}),
            json!({"properties":{"x":42}}),
            json!({"required":[42]}),
            json!({"type":"string"}),
        ] {
            assert!(normalize(schema).is_err());
        }
        assert_eq!(
            normalize(json!({"unknownKeyword":{}})).unwrap(),
            json!({"type":"object"})
        );
    }

    #[test]
    fn schema_constraints_are_type_checked_before_provider_projection() {
        for key in [
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
            "minProperties",
            "maxProperties",
        ] {
            for value in [json!("bad"), json!(-1), json!(1.5), json!(true)] {
                assert!(
                    normalize(json!({"properties":{"x":{key:value}}})).is_err(),
                    "{key}"
                );
            }
        }
        for key in [
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ] {
            assert!(normalize(json!({"properties":{"x":{key:"bad"}}})).is_err());
            assert!(normalize(json!({"properties":{"x":{key:1.5}}})).is_ok());
        }
        for key in ["pattern", "format", "description", "title"] {
            assert!(normalize(json!({"properties":{"x":{key:42}}})).is_err());
        }
        for schema in [
            json!({"enum":42}),
            json!({"enum":[]}),
            json!({"uniqueItems":42}),
            json!({"additionalProperties":42}),
            json!({"type":["string","bad"]}),
        ] {
            assert!(normalize(schema).is_err());
        }
        let valid = normalize(json!({"properties":{"x":{"type":["string","null"],"minLength":0,"maxLength":10,"enum":[null,"x"],"pattern":"x","title":"x","description":"x","default":42,"const":42,"examples":42}},"additionalProperties":false,"uniqueItems":true})).unwrap();
        assert_eq!(valid["properties"]["x"]["examples"], 42);
    }
}
