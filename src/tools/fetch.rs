//! `fetch`: one HTTP(S) GET, returned as markdown, text or HTML.
//!
//! The client is its own: no provider headers, no credentials, redirects
//! handled here (same-origin only, so a hop never carries a request to
//! another site). The body is streamed and capped after decompression; the
//! declared length is never trusted.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::{Client, Url};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{bound_output, Tool, ToolContext, ToolResult, DEFAULT_MAX_OUTPUT_BYTES};

pub const FETCH_DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const FETCH_MAX_TIMEOUT_SECS: u64 = 120;
pub const FETCH_MAX_REDIRECTS: usize = 10;
pub const FETCH_MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// Wrap width of the plain-text rendering.
const TEXT_WIDTH: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Markdown,
    Text,
    Html,
}

impl Format {
    fn parse(s: Option<&str>) -> Option<Self> {
        match s {
            None | Some("markdown") => Some(Self::Markdown),
            Some("text") => Some(Self::Text),
            Some("html") => Some(Self::Html),
            Some(_) => None,
        }
    }

    fn accept(self) -> &'static str {
        match self {
            Self::Markdown => "text/html, text/markdown;q=0.9, text/plain;q=0.8, */*;q=0.1",
            Self::Text => "text/plain, text/html;q=0.9, */*;q=0.1",
            Self::Html => "text/html, application/xhtml+xml;q=0.9, */*;q=0.1",
        }
    }
}

pub struct FetchTool {
    client: Client,
}

impl Default for FetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FetchTool {
    pub fn new() -> Self {
        static CLIENT: OnceLock<Client> = OnceLock::new();
        let client = CLIENT
            .get_or_init(|| {
                Client::builder()
                    .user_agent(concat!("navai/", env!("CARGO_PKG_VERSION")))
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(FETCH_MAX_TIMEOUT_SECS))
                    .build()
                    .expect("the fetch client configuration is valid")
            })
            .clone();
        Self { client }
    }
}

fn fail(code: &str, message: impl Into<String>) -> ToolResult {
    ToolResult::error(code, message)
}

impl Tool for FetchTool {
    fn name(&self) -> &str {
        "fetch"
    }

    fn schema(&self) -> Value {
        json!({
            "description": "Fetch one http(s) URL with GET and return its text. HTML is converted to markdown (default) or plain text, or returned raw with format `html`; other textual types (JSON, XML, plain text, JavaScript) are returned as is. Images and other binary content are refused. The body is limited to 5 MiB and the output to 64 KiB. Only same-origin redirects are followed: a redirect to another origin is returned as the target URL, call fetch again with it. Any address is allowed, including localhost and private networks, so the call needs the user's approval. URLs with credentials are refused.",
            "parameters": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "absolute http:// or https:// URL, without credentials"},
                    "format": {"type": "string", "enum": ["markdown", "text", "html"], "description": "markdown (default), text or html"},
                    "timeout_secs": {"type": "integer", "description": "request timeout (default 30, max 120)"}
                },
                "required": ["url"]
            }
        })
    }

    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
        _cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(raw_url) = args.get("url").and_then(|v| v.as_str()) else {
                return Err(ToolError::Denied {
                    code: "invalid_args".into(),
                    message: "missing string argument: url".into(),
                });
            };
            let Some(format) = Format::parse(args.get("format").and_then(|v| v.as_str())) else {
                return Ok(fail(
                    "invalid_args",
                    "format must be markdown, text or html",
                ));
            };
            let timeout_secs = args
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .map(|t| t.clamp(1, FETCH_MAX_TIMEOUT_SECS))
                .unwrap_or(FETCH_DEFAULT_TIMEOUT_SECS);
            let url = match parse_url(raw_url) {
                Ok(u) => u,
                Err(message) => return Ok(fail("invalid_url", message)),
            };

            let work = fetch(&self.client, url, format);
            tokio::select! {
                _ = cancel.cancelled() => Ok(ToolResult::interrupted()),
                res = tokio::time::timeout(Duration::from_secs(timeout_secs), work) => {
                    Ok(res.unwrap_or_else(|_| {
                        fail("timeout", format!("no complete response within {timeout_secs} s"))
                    }))
                }
            }
        })
    }
}

fn parse_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("not a valid URL: {e}"))?;
    check_url(&url)?;
    Ok(url)
}

fn check_url(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "only http and https URLs are fetched, not {}",
            url.scheme()
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("the URL has no host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with credentials are refused".into());
    }
    Ok(())
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

async fn fetch(client: &Client, start: Url, format: Format) -> ToolResult {
    let mut url = start.clone();
    let mut hops = 0usize;
    let response = loop {
        let resp = match client
            .get(url.clone())
            .header(ACCEPT, format.accept())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return net_error(e),
        };
        let status = resp.status();
        if !status.is_redirection() {
            break resp;
        }
        let Some(location) = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()) else {
            return fail(
                "http_status",
                format!("HTTP {status} without a Location header"),
            );
        };
        let target = match url.join(location) {
            Ok(t) => t,
            Err(e) => return fail("network", format!("bad redirect target {location:?}: {e}")),
        };
        if let Err(message) = check_url(&target) {
            return fail(
                "network",
                format!("redirect to {target} refused: {message}"),
            );
        }
        if !same_origin(&start, &target) {
            return ToolResult::ok(format!(
                "Redirected (HTTP {}) to another origin: {target}\nNothing was fetched from it; call fetch again with that URL if you want it.",
                status.as_u16()
            ))
            .with_metadata(json!({
                "code": "redirect_cross_origin",
                "status": status.as_u16(),
                "url": url.as_str(),
                "target": target.as_str(),
            }));
        }
        hops += 1;
        if hops > FETCH_MAX_REDIRECTS {
            return fail(
                "network",
                format!("more than {FETCH_MAX_REDIRECTS} redirects"),
            );
        }
        url = target;
    };

    let status = response.status();
    if !status.is_success() {
        return fail("http_status", format!("{url} answered HTTP {status}"));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mime = content_type
        .as_deref()
        .map(|ct| essence(ct).to_string())
        .unwrap_or_default();
    if !mime.is_empty() && !is_textual(&mime) {
        return fail(
            "unsupported_content",
            format!("{url} is {mime}, not a text type; only text is fetched"),
        );
    }

    let mut response = response;
    let mut body: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > FETCH_MAX_BODY_BYTES {
                    return fail(
                        "too_large",
                        format!(
                            "{url} body exceeds {} MiB; the download was stopped",
                            FETCH_MAX_BODY_BYTES / (1024 * 1024)
                        ),
                    );
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return net_error(e),
        }
    }
    if mime.is_empty() && body.contains(&0) {
        return fail(
            "unsupported_content",
            format!("{url} has no content type and looks binary"),
        );
    }

    let charset = content_type.as_deref().and_then(charset_of);
    let text = decode(&body, charset.as_deref(), is_html(&mime) || mime.is_empty());
    let original_len = body.len();

    let converted = if is_html(&mime) || (mime.is_empty() && looks_like_html(&text)) {
        match format {
            Format::Html => Ok(text),
            Format::Markdown | Format::Text => convert_isolated(text, format).await,
        }
    } else {
        Ok(text)
    };
    let text = match converted {
        Ok(t) => t,
        Err(e) => {
            return fail(
                "unsupported_content",
                format!("{url}: the HTML could not be converted: {e}"),
            )
        }
    };

    let (out, elided) = bound_output(&text, DEFAULT_MAX_OUTPUT_BYTES);
    let mut result = ToolResult::ok(out).with_metadata(json!({
        "url": url.as_str(),
        "status": status.as_u16(),
        "content_type": mime,
        "format": match format { Format::Markdown => "markdown", Format::Text => "text", Format::Html => "html" },
        "body_bytes": original_len,
        "redirects": hops,
    }));
    if let Some(original) = elided {
        result = result.with_truncation(original);
    }
    result
}

fn net_error(e: reqwest::Error) -> ToolResult {
    if e.is_timeout() {
        return fail("timeout", "the request timed out");
    }
    let e = e.without_url();
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        message.push_str(": ");
        message.push_str(&s.to_string());
        source = s.source();
    }
    fail("network", message)
}

/// `text/html; charset=x` to `text/html`, lowercased.
fn essence(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or("").trim()
}

fn is_html(mime: &str) -> bool {
    mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("application/xhtml+xml")
}

fn is_textual(mime: &str) -> bool {
    let m = mime.to_ascii_lowercase();
    m.starts_with("text/")
        || m.ends_with("+json")
        || m.ends_with("+xml")
        || m.ends_with("+yaml")
        || matches!(
            m.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-www-form-urlencoded"
                | "application/x-sh"
                | "application/x-ndjson"
                | "application/graphql"
        )
}

fn looks_like_html(text: &str) -> bool {
    let head: String = text
        .chars()
        .take(512)
        .collect::<String>()
        .to_ascii_lowercase();
    head.contains("<!doctype html") || head.contains("<html")
}

fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches('"').to_string())
    })
}

/// The charset of the header, else (for HTML) of a `<meta>` in the first
/// kilobyte, else UTF-8; a byte-order mark wins over all of them. Invalid
/// sequences become U+FFFD.
fn decode(body: &[u8], header_charset: Option<&str>, sniff_meta: bool) -> String {
    let label = header_charset
        .map(str::to_string)
        .or_else(|| sniff_meta.then(|| meta_charset(body)).flatten());
    let encoding = label
        .as_deref()
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(body);
    text.into_owned()
}

fn meta_charset(body: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&body[..body.len().min(1024)]).to_ascii_lowercase();
    let at = head.find("charset")? + "charset".len();
    let rest = head[at..].trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.trim_start_matches(['"', '\'']);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}

/// Largest HTML handed to a converter; the rest is dropped with a note.
const CONVERT_MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
/// Deepest tag nesting and most elements a converter is trusted with.
const CONVERT_MAX_DEPTH: usize = 256;
const CONVERT_MAX_ELEMENTS: usize = 200_000;
/// The converters recurse over the DOM; their thread gets room for it.
const CONVERT_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Runs the conversion on its own thread with a large stack, so a deep but
/// accepted document cannot overflow the worker stack of the runtime.
async fn convert_isolated(html: String, format: Format) -> Result<String, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("fetch-convert".into())
        .stack_size(CONVERT_STACK_BYTES)
        .spawn(move || {
            let _ = tx.send(convert_html(&html, format));
        })
        .map_err(|e| format!("no conversion thread: {e}"))?;
    rx.await
        .map_err(|_| "the conversion thread failed".to_string())?
}

/// Converts HTML, within limits: oversized input is cut, and a document too
/// deep or too large is flattened to plain text without building a DOM.
fn convert_html(html: &str, format: Format) -> Result<String, String> {
    let mut notes = Vec::new();
    let mut input = html;
    if input.len() > CONVERT_MAX_INPUT_BYTES {
        let mut end = CONVERT_MAX_INPUT_BYTES;
        while !input.is_char_boundary(end) {
            end -= 1;
        }
        input = &input[..end];
        notes.push(format!(
            "the HTML was cut to its first {} MiB before conversion",
            CONVERT_MAX_INPUT_BYTES / (1024 * 1024)
        ));
    }
    let body = match measure(input) {
        Ok(()) => match format {
            Format::Markdown => htmd::HtmlToMarkdown::builder()
                .skip_tags(vec!["script", "style", "noscript", "template", "head"])
                .build()
                .convert(input)
                .map_err(|e| e.to_string())?,
            Format::Text => html2text::config::plain_no_decorate()
                .string_from_read(input.as_bytes(), TEXT_WIDTH)
                .map_err(|e| e.to_string())?,
            Format::Html => input.to_string(),
        },
        Err(why) => {
            notes.push(format!(
                "{why}; the page was reduced to plain text without structure"
            ));
            flatten(input)
        }
    };
    if notes.is_empty() {
        return Ok(body);
    }
    Ok(format!("[note: {}]\n\n{body}", notes.join("; ")))
}

#[derive(Debug, Clone, Copy)]
enum Tok<'a> {
    Text(&'a str),
    Open(&'a str),
    Close(&'a str),
    /// The body of a script, style or similar element.
    Raw,
}

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr", "param",
];
const RAW_TAGS: &[&str] = &["script", "style", "textarea", "title", "xmp"];

fn is_one_of(name: &str, set: &[&str]) -> bool {
    set.iter().any(|t| name.eq_ignore_ascii_case(t))
}

/// Linear, tolerant HTML tokenizer: never recurses, never backtracks. `f`
/// returns false to stop.
fn scan<'a>(html: &'a str, mut f: impl FnMut(Tok<'a>) -> bool) {
    let b = html.as_bytes();
    let len = b.len();
    let name_end = |from: usize| {
        let mut j = from;
        while j < len && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'-' | b':' | b'_')) {
            j += 1;
        }
        j
    };
    let mut i = 0;
    while i < len {
        let Some(off) = b[i..].iter().position(|&c| c == b'<') else {
            f(Tok::Text(&html[i..]));
            return;
        };
        let lt = i + off;
        if lt > i && !f(Tok::Text(&html[i..lt])) {
            return;
        }
        let next = b.get(lt + 1).copied();
        match next {
            Some(b'!') if b[lt..].starts_with(b"<!--") => {
                i = html[lt + 4..].find("-->").map_or(len, |e| lt + 4 + e + 3);
            }
            Some(b'!' | b'?') => {
                i = b[lt..]
                    .iter()
                    .position(|&c| c == b'>')
                    .map_or(len, |e| lt + e + 1);
            }
            Some(b'/') if b.get(lt + 2).is_some_and(u8::is_ascii_alphabetic) => {
                let end = name_end(lt + 2);
                if !f(Tok::Close(&html[lt + 2..end])) {
                    return;
                }
                i = b[end..]
                    .iter()
                    .position(|&c| c == b'>')
                    .map_or(len, |e| end + e + 1);
            }
            Some(c) if c.is_ascii_alphabetic() => {
                let end = name_end(lt + 1);
                let name = &html[lt + 1..end];
                // Attributes, honouring quotes, up to the closing `>`.
                let mut j = end;
                let mut quote = None;
                let mut closed = false;
                while j < len {
                    match (quote, b[j]) {
                        (Some(q), c) if c == q => quote = None,
                        (Some(_), _) => {}
                        (None, q @ (b'"' | b'\'')) => quote = Some(q),
                        (None, b'>') => {
                            closed = true;
                            break;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                let self_closing = closed && j > end && b[j - 1] == b'/';
                i = if closed { j + 1 } else { len };
                if !f(Tok::Open(name)) {
                    return;
                }
                if self_closing || is_one_of(name, VOID_TAGS) {
                    if !f(Tok::Close(name)) {
                        return;
                    }
                } else if is_one_of(name, RAW_TAGS) {
                    // Everything up to the matching close tag is not markup.
                    let mut k = i;
                    let close_at = loop {
                        let Some(p) = b[k..].windows(2).position(|w| w == b"</") else {
                            break None;
                        };
                        let at = k + p;
                        let tail = &b[at + 2..];
                        if tail.len() >= name.len()
                            && tail[..name.len()].eq_ignore_ascii_case(name.as_bytes())
                        {
                            break Some(at);
                        }
                        k = at + 2;
                    };
                    let body_end = close_at.unwrap_or(len);
                    if body_end > i && !f(Tok::Raw) {
                        return;
                    }
                    if close_at.is_some() {
                        if !f(Tok::Close(name)) {
                            return;
                        }
                        i = b[body_end..]
                            .iter()
                            .position(|&c| c == b'>')
                            .map_or(len, |e| body_end + e + 1);
                    } else {
                        i = len;
                    }
                }
            }
            _ => {
                if !f(Tok::Text("<")) {
                    return;
                }
                i = lt + 1;
            }
        }
    }
}

/// Maximum nesting depth and element count of `html`, in one linear pass.
/// Err says which limit is exceeded.
fn measure(html: &str) -> Result<(), String> {
    const IMPLIED_END: &[&str] = &["p", "li", "dt", "dd", "tr", "td", "th", "option"];
    let mut stack: Vec<&str> = Vec::new();
    let mut elements = 0usize;
    let mut verdict = Ok(());
    scan(html, |tok| {
        match tok {
            Tok::Open(name) => {
                elements += 1;
                if elements > CONVERT_MAX_ELEMENTS {
                    verdict = Err(format!(
                        "the page has more than {CONVERT_MAX_ELEMENTS} elements"
                    ));
                    return false;
                }
                if stack.last().is_some_and(|top| {
                    top.eq_ignore_ascii_case(name) && is_one_of(name, IMPLIED_END)
                }) {
                    stack.pop();
                }
                stack.push(name);
                if stack.len() > CONVERT_MAX_DEPTH {
                    verdict = Err(format!(
                        "the page nests elements deeper than {CONVERT_MAX_DEPTH} levels"
                    ));
                    return false;
                }
            }
            Tok::Close(name) => {
                if let Some(at) = stack.iter().rposition(|t| t.eq_ignore_ascii_case(name)) {
                    stack.truncate(at);
                }
            }
            Tok::Text(_) | Tok::Raw => {}
        }
        true
    });
    verdict
}

/// Plain text of `html` by dropping every tag, comment and script/style
/// body; linear, no DOM.
fn flatten(html: &str) -> String {
    let mut out = String::new();
    let mut hidden = 0usize;
    scan(html, |tok| {
        match tok {
            Tok::Text(t) if hidden == 0 => out.push_str(t),
            Tok::Open(name) => {
                if is_one_of(name, &["script", "style"]) {
                    hidden += 1;
                }
                out.push(' ');
            }
            Tok::Close(name) => {
                if is_one_of(name, &["script", "style"]) {
                    hidden = hidden.saturating_sub(1);
                }
                out.push(' ');
            }
            _ => {}
        }
        true
    });
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolStatus;
    use std::path::PathBuf;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cx() -> ToolContext {
        ToolContext {
            workspace: PathBuf::from("."),
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
        }
    }

    async fn call(args: Value) -> ToolResult {
        FetchTool::new()
            .call(args, CancellationToken::new(), &cx())
            .await
            .unwrap()
    }

    fn code(res: &ToolResult) -> &str {
        res.error.as_ref().map(|e| e.code.as_str()).unwrap_or("")
    }

    const PAGE: &str = "<html><head><title>T</title><style>p{color:red}</style></head>\
        <body><h1>Heading</h1><p>Some <b>bold</b> text and a <a href=\"https://e.test/x\">link</a>.</p>\
        <script>alert('SCRIPTBODY')</script></body></html>";

    async fn html_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE, "text/html; charset=utf-8"))
            .mount(&server)
            .await;
        server
    }

    // Given an HTML page with a style and a script
    // When fetched as markdown (default), text and html
    // Then markdown and text drop script and style, html is raw
    #[tokio::test]
    async fn formats_markdown_text_html() {
        let server = html_server().await;
        let url = format!("{}/page", server.uri());

        let md = call(json!({"url": url})).await;
        assert_eq!(md.status, ToolStatus::Ok, "{:?}", md.error);
        assert!(md.output.contains("# Heading"), "{}", md.output);
        assert!(md.output.contains("**bold**"), "{}", md.output);
        assert!(
            md.output.contains("[link](https://e.test/x)"),
            "{}",
            md.output
        );
        assert!(!md.output.contains("SCRIPTBODY"), "{}", md.output);
        assert!(!md.output.contains("color:red"), "{}", md.output);
        assert_eq!(md.metadata["format"], "markdown");

        let text = call(json!({"url": url, "format": "text"})).await;
        assert_eq!(text.status, ToolStatus::Ok, "{:?}", text.error);
        assert!(text.output.contains("Heading"), "{}", text.output);
        assert!(text.output.contains("Some bold text"), "{}", text.output);
        assert!(!text.output.contains("SCRIPTBODY"), "{}", text.output);
        assert!(!text.output.contains("color:red"), "{}", text.output);
        assert!(!text.output.contains('<'), "{}", text.output);

        let html = call(json!({"url": url, "format": "html"})).await;
        assert_eq!(html.output, PAGE);
    }

    // Given a JSON endpoint and a bad format
    // When fetched
    // Then JSON comes back as is, the format is refused
    #[tokio::test]
    async fn non_html_text_is_returned_as_is_and_bad_format_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"a":1}"#, "application/json"))
            .mount(&server)
            .await;
        let res = call(json!({"url": format!("{}/data", server.uri())})).await;
        assert_eq!(res.output, r#"{"a":1}"#);
        let res = call(json!({"url": format!("{}/data", server.uri()), "format": "pdf"})).await;
        assert_eq!(code(&res), "invalid_args");
    }

    // Given the Navai user agent requirement
    // When fetched
    // Then the request carries `navai/<version>` and no authorization
    #[tokio::test]
    async fn sends_the_navai_user_agent_and_no_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header(
                "user-agent",
                concat!("navai/", env!("CARGO_PKG_VERSION")),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_raw("ok", "text/plain"))
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri()})).await;
        assert_eq!(res.output, "ok", "{:?}", res.error);
        let seen = &server.received_requests().await.unwrap()[0];
        assert!(seen.headers.get("authorization").is_none());
    }

    // Given a same-origin redirect chain
    // When fetched
    // Then it is followed to the final body
    #[tokio::test]
    async fn same_origin_redirect_is_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/old"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/new"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("arrived", "text/plain"))
            .mount(&server)
            .await;
        let res = call(json!({"url": format!("{}/old", server.uri())})).await;
        assert_eq!(res.output, "arrived", "{:?}", res.error);
        assert_eq!(res.metadata["redirects"], 1);
    }

    // Given a redirect to another origin
    // When fetched
    // Then the target is named in a non-error result and not requested
    #[tokio::test]
    async fn cross_origin_redirect_is_returned_not_followed() {
        let target = MockServer::start().await;
        let server = MockServer::start().await;
        let to = format!("{}/landing", target.uri());
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", to.as_str()))
            .mount(&server)
            .await;
        let res = call(json!({"url": format!("{}/go", server.uri())})).await;
        assert_eq!(res.status, ToolStatus::Ok);
        assert!(res.output.contains(&to), "{}", res.output);
        assert_eq!(res.metadata["code"], "redirect_cross_origin");
        assert_eq!(res.metadata["target"], to);
        assert!(target.received_requests().await.unwrap().is_empty());
    }

    // Given a redirect loop
    // When fetched
    // Then it stops after the redirect cap
    #[tokio::test]
    async fn redirect_loop_stops_at_the_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/again"))
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri()})).await;
        assert_eq!(code(&res), "network");
        assert!(res.error.unwrap().message.contains("redirects"));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            FETCH_MAX_REDIRECTS + 1
        );
    }

    // Given a body past the cap, with a truthful and a missing Content-Length
    // When fetched
    // Then it is refused as too_large
    #[tokio::test]
    async fn body_over_the_cap_is_refused() {
        let server = MockServer::start().await;
        let big = vec![b'a'; FETCH_MAX_BODY_BYTES + 1];
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(big, "text/plain"))
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri()})).await;
        assert_eq!(code(&res), "too_large", "{:?}", res.error);
    }

    // Given a body of exactly the cap
    // When fetched
    // Then it is accepted and the output is bounded with a marker
    #[tokio::test]
    async fn body_at_the_cap_is_accepted_and_output_bounded() {
        let server = MockServer::start().await;
        let body = vec![b'a'; FETCH_MAX_BODY_BYTES];
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/plain"))
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri()})).await;
        assert_eq!(res.status, ToolStatus::Ok, "{:?}", res.error);
        assert!(res.output.len() <= DEFAULT_MAX_OUTPUT_BYTES + 32);
        assert!(res.output.contains("elided"));
        assert_eq!(res.truncated.unwrap().original_bytes, FETCH_MAX_BODY_BYTES);
    }

    // Given a gzip body that inflates past the cap while small on the wire
    // When fetched
    // Then the decompressed size is what is capped
    #[tokio::test]
    async fn the_cap_applies_after_decompression() {
        use std::io::Write as _;
        let server = MockServer::start().await;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&vec![b'a'; FETCH_MAX_BODY_BYTES + 1024])
            .unwrap();
        let packed = enc.finish().unwrap();
        assert!(packed.len() < 100_000);
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .set_body_raw(packed, "text/plain"),
            )
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri()})).await;
        assert_eq!(code(&res), "too_large", "{:?}", res.error);
    }

    // Given image, octet-stream and untyped binary responses
    // When fetched
    // Then each is refused as unsupported_content
    #[tokio::test]
    async fn binary_content_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/img"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(vec![0x89, b'P', b'N', b'G'], "image/png"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(vec![1u8, 2, 3], "application/octet-stream"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/untyped"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8, 1, 2, 0]))
            .mount(&server)
            .await;
        for p in ["img", "bin", "untyped"] {
            let res = call(json!({"url": format!("{}/{p}", server.uri())})).await;
            assert_eq!(code(&res), "unsupported_content", "{p}: {:?}", res.error);
        }
    }

    // Given a server slower than the timeout
    // When fetched with a 1 s timeout
    // Then the result is a timeout error
    #[tokio::test]
    async fn slow_server_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("late", "text/plain")
                    .set_delay(Duration::from_secs(3)),
            )
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri(), "timeout_secs": 1})).await;
        assert_eq!(code(&res), "timeout", "{:?}", res.error);
    }

    // Given a cancelled token and a slow server
    // When fetched
    // Then the result is interrupted at once
    #[tokio::test]
    async fn cancellation_interrupts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("late", "text/plain")
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let res = FetchTool::new()
            .call(json!({"url": server.uri()}), cancel, &cx())
            .await
            .unwrap();
        assert_eq!(res.status, ToolStatus::Interrupted);
    }

    // Given a latin-1 body, by header and by meta tag
    // When fetched
    // Then it is decoded with that charset
    #[tokio::test]
    async fn charset_is_honoured() {
        let server = MockServer::start().await;
        let latin1 = b"caf\xe9 cr\xe8me".to_vec();
        Mock::given(method("GET"))
            .and(path("/header"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(latin1.clone(), "text/plain; charset=iso-8859-1"),
            )
            .mount(&server)
            .await;
        let mut meta = b"<html><head><meta charset=\"latin1\"></head><body><p>".to_vec();
        meta.extend_from_slice(&latin1);
        meta.extend_from_slice(b"</p></body></html>");
        Mock::given(method("GET"))
            .and(path("/meta"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(meta, "text/html"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/broken"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(vec![b'o', b'k', 0xff, 0xfe], "text/plain"),
            )
            .mount(&server)
            .await;
        let h = call(json!({"url": format!("{}/header", server.uri())})).await;
        assert_eq!(h.output, "café crème");
        let m = call(json!({"url": format!("{}/meta", server.uri())})).await;
        assert!(m.output.contains("café crème"), "{}", m.output);
        let b = call(json!({"url": format!("{}/broken", server.uri())})).await;
        assert!(b.output.starts_with("ok\u{fffd}"), "{}", b.output);
    }

    // Given URLs with credentials, other schemes, no host, garbage
    // When fetched
    // Then each is refused as invalid_url before any request
    #[tokio::test]
    async fn invalid_urls_are_refused() {
        let server = MockServer::start().await;
        let creds = format!("http://user:pw@{}/", server.address());
        let user_only = format!("http://user@{}/", server.address());
        for bad in [
            creds.as_str(),
            user_only.as_str(),
            "file:///etc/passwd",
            "ftp://example.test/x",
            "http://",
            "not a url",
            "",
        ] {
            let res = call(json!({"url": bad})).await;
            assert_eq!(code(&res), "invalid_url", "{bad}: {:?}", res.error);
        }
        assert!(server.received_requests().await.unwrap().is_empty());
        let err = FetchTool::new()
            .call(json!({}), CancellationToken::new(), &cx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    // Given a redirect whose target carries credentials
    // When fetched
    // Then it is refused rather than followed
    #[tokio::test]
    async fn redirect_to_credentials_is_refused() {
        let server = MockServer::start().await;
        let to = format!("http://u:p@{}/x", server.address());
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", to.as_str()))
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri()})).await;
        assert_eq!(code(&res), "network", "{:?}", res.error);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    // Given error statuses
    // When fetched
    // Then the result is an http_status error naming the status
    #[tokio::test]
    async fn http_error_status_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_raw("nope", "text/plain"))
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri()})).await;
        assert_eq!(code(&res), "http_status");
        assert!(res.error.unwrap().message.contains("404"));
    }

    // Given a closed port
    // When fetched
    // Then the result is a network error
    #[tokio::test]
    async fn unreachable_host_is_a_network_error() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let res = call(json!({"url": format!("http://127.0.0.1:{port}/")})).await;
        assert_eq!(code(&res), "network", "{:?}", res.error);
    }

    // Given 100,000 nested divs
    // When fetched as markdown and as text
    // Then the plain-text fallback is returned, without a crash
    #[tokio::test]
    async fn deeply_nested_html_gets_the_plain_text_fallback() {
        let server = MockServer::start().await;
        let page = format!(
            "<html><body>{}deep text{}</body></html>",
            "<div>".repeat(100_000),
            "</div>".repeat(100_000)
        );
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page, "text/html"))
            .mount(&server)
            .await;
        for format in ["markdown", "text"] {
            let res = call(json!({"url": server.uri(), "format": format})).await;
            assert_eq!(res.status, ToolStatus::Ok, "{:?}", res.error);
            assert!(res.output.contains("deep text"), "{format}");
            assert!(
                res.output.contains("plain text without structure"),
                "{format}"
            );
        }
    }

    // Given a normal page
    // When converted
    // Then markdown is produced, with no fallback note
    #[test]
    fn a_normal_page_converts_to_markdown() {
        let out = convert_html(PAGE, Format::Markdown).unwrap();
        assert!(out.contains("# Heading"), "{out}");
        assert!(!out.contains("[note:"), "{out}");
        assert!(!out.contains("SCRIPTBODY"), "{out}");
    }

    // Given 50,000 empty script elements and tag-like text in comments and attributes
    // When converted
    // Then it is quick and the surrounding content survives
    #[test]
    fn many_scripts_convert_quickly_and_tag_like_text_is_harmless() {
        let page = format!(
            "<p>start</p>{}<p title=\"<script>\">middle</p><!-- <script> --><p>end</p>",
            "<script></script>".repeat(50_000)
        );
        for format in [Format::Markdown, Format::Text] {
            let t = std::time::Instant::now();
            let out = convert_html(&page, format).unwrap();
            assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
            for word in ["start", "middle", "end"] {
                assert!(out.contains(word), "{word} missing: {out}");
            }
        }
    }

    // Given HTML past the converter input cap
    // When converted
    // Then it is cut on a char boundary and a note says so
    #[test]
    fn oversized_html_is_cut_with_a_note() {
        let page = format!("<p>{}</p>", "é".repeat(CONVERT_MAX_INPUT_BYTES));
        let out = convert_html(&page, Format::Markdown).unwrap();
        assert!(out.starts_with("[note: the HTML was cut"), "{}", &out[..80]);
    }

    #[test]
    fn textual_mimes() {
        for m in [
            "text/plain",
            "text/css",
            "application/json",
            "application/ld+json",
            "image/svg+xml",
            "application/atom+xml",
            "application/javascript",
        ] {
            assert!(is_textual(m), "{m}");
        }
        for m in [
            "image/png",
            "application/pdf",
            "application/octet-stream",
            "video/mp4",
        ] {
            assert!(!is_textual(m), "{m}");
        }
    }
}
