//! `fetch`: one HTTP(S) GET, returned as markdown, text or HTML.
//!
//! The client is its own: no provider headers, no credentials, redirects
//! handled here (same-origin only, so a hop never carries a request to
//! another site). The body is streamed and capped after decompression; the
//! declared length is never trusted.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::{Client, Url};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{Tool, ToolContext, ToolResult, DEFAULT_MAX_OUTPUT_BYTES};

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
    /// The program that converts HTML in a child process; see `converter`.
    converter: Option<PathBuf>,
}

/// Environment override of the converter program.
pub const HTML_CONVERTER_ENV: &str = "WITHNAV_HTML_CONVERTER";

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
                    .user_agent(concat!("withnav/", env!("CARGO_PKG_VERSION")))
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(FETCH_MAX_TIMEOUT_SECS))
                    .build()
                    .expect("the fetch client configuration is valid")
            })
            .clone();
        Self {
            client,
            converter: None,
        }
    }

    /// Converts HTML with this program (the `withnav` binary or a copy of it)
    /// instead of the running executable.
    pub fn with_converter(mut self, program: impl Into<PathBuf>) -> Self {
        self.converter = Some(program.into());
        self
    }

    /// The field, else `WITHNAV_HTML_CONVERTER`, else the running executable.
    fn converter(&self) -> Option<PathBuf> {
        self.converter
            .clone()
            .or_else(|| crate::paths::env_var_os(HTML_CONVERTER_ENV).map(PathBuf::from))
            .or_else(|| std::env::current_exe().ok())
    }
}

/// Longest URL or header text echoed in an output, error or metadata.
const ECHO_MAX_BYTES: usize = 2048;

/// `s` cut to `ECHO_MAX_BYTES` on a char boundary, with a marker.
fn clip(s: &str) -> String {
    if s.len() <= ECHO_MAX_BYTES {
        return s.to_string();
    }
    let mut end = ECHO_MAX_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [{} bytes cut]", &s[..end], s.len() - end)
}

/// A URL (or URL-looking header value) fit to print: userinfo replaced by
/// `***@`, length bounded.
fn shown(raw: &str) -> String {
    let redacted = match raw.find("://") {
        Some(i) => {
            let auth_start = i + 3;
            let auth_end = raw[auth_start..]
                .find(['/', '?', '#'])
                .map_or(raw.len(), |e| auth_start + e);
            match raw[auth_start..auth_end].rfind('@') {
                Some(at) => format!("{}***@{}", &raw[..auth_start], &raw[auth_start + at + 1..]),
                None => raw.to_string(),
            }
        }
        None => raw.to_string(),
    };
    clip(&redacted)
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
            "description": "Fetch one http(s) URL with GET and return its text. HTML is converted to markdown (default) or plain text, or returned raw with format `html`; other textual types (JSON, XML, plain text, JavaScript) are returned as is. Images and other binary content are refused. The body is limited to 5 MiB and the output to 64 KiB. Only same-origin redirects are followed: a redirect to another origin is returned as the target URL, call fetch again with it. Redirect locations use standard URL resolution: a new path without a query drops the original query; query parameters are not automatically forwarded. Any address is allowed, including localhost and private networks, so the call needs the user's approval. URLs with credentials are refused.",
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

            let deadline = Instant::now() + Duration::from_secs(timeout_secs);
            let work = fetch(&self.client, url, format, self.converter(), deadline);
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

async fn fetch(
    client: &Client,
    start: Url,
    format: Format,
    converter: Option<PathBuf>,
    deadline: Instant,
) -> ToolResult {
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
            // The raw value may carry credentials: it is not echoed.
            Err(_) => return fail("network", "invalid redirect target"),
        };
        if let Err(message) = check_url(&target) {
            return fail(
                "network",
                format!("redirect to {} refused: {message}", shown(target.as_str())),
            );
        }
        if !same_origin(&start, &target) {
            return ToolResult::ok(format!(
                "Redirected (HTTP {}) to another origin: {}\nNothing was fetched from it; call fetch again with that URL if you want it.",
                status.as_u16(),
                shown(target.as_str())
            ))
            .with_metadata(json!({
                "code": "redirect_cross_origin",
                "status": status.as_u16(),
                "url": shown(url.as_str()),
                "target": shown(target.as_str()),
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
        return fail(
            "http_status",
            format!("{} answered HTTP {status}", shown(url.as_str())),
        );
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mime = content_type
        .as_deref()
        .map(|ct| clip(essence(ct)))
        .unwrap_or_default();
    if !mime.is_empty() && !is_textual(&mime) {
        return fail(
            "unsupported_content",
            format!(
                "{} is {mime}, not a text type; only text is fetched",
                shown(url.as_str())
            ),
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
                            "{} body exceeds {} MiB; the download was stopped",
                            shown(url.as_str()),
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
            format!(
                "{} has no content type and looks binary",
                shown(url.as_str())
            ),
        );
    }

    let charset = content_type.as_deref().and_then(charset_of);
    let text = decode(&body, charset.as_deref(), is_html(&mime) || mime.is_empty());
    let original_len = body.len();

    let text = if is_html(&mime) || (mime.is_empty() && looks_like_html(&text)) {
        match format {
            Format::Html => text,
            Format::Markdown | Format::Text => {
                let budget = deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_sub(Duration::from_millis(500))
                    .min(CONVERT_MAX_DURATION);
                convert_html(text, format, converter.as_deref(), budget).await
            }
        }
    } else {
        text
    };

    ToolResult::bounded_ok(&text, DEFAULT_MAX_OUTPUT_BYTES).with_metadata(json!({
        "url": shown(url.as_str()),
        "status": status.as_u16(),
        "content_type": mime,
        "format": match format { Format::Markdown => "markdown", Format::Text => "text", Format::Html => "html" },
        "body_bytes": original_len,
        "redirects": hops,
    }))
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
    fail("network", clip(&message))
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
/// Deepest tag nesting and most elements the fast path hands to a converter.
const CONVERT_MAX_DEPTH: usize = 256;
const CONVERT_MAX_ELEMENTS: usize = 200_000;
/// Longest a conversion child may run, whatever time the call has left.
const CONVERT_MAX_DURATION: Duration = Duration::from_secs(10);
/// Address-space and CPU limits of the conversion child.
#[cfg(unix)]
const CONVERT_MAX_ADDRESS_SPACE: u64 = 512 * 1024 * 1024;
#[cfg(unix)]
const CONVERT_MAX_CPU_SECS: u64 = 10;
/// Output read from the child: the tool output bound plus a margin for the
/// note and for the cut.
const CONVERT_MAX_OUTPUT_BYTES: usize = DEFAULT_MAX_OUTPUT_BYTES + 4096;
/// The subcommand of the `withnav` binary that runs the converter.
pub const CONVERT_SUBCOMMAND: &str = "internal-html-convert";

const UNSAFE_NOTE: &str = "the page could not be converted safely; plain text shown";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ConvertFormat {
    Markdown,
    Text,
}

impl ConvertFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Text => "text",
        }
    }
}

/// Runs the converter when the process was started as a conversion child
/// (`<binary> internal-html-convert --format <f>`), and returns its exit
/// code; `None` for any other invocation. Both binaries call this first,
/// before building their async runtime: the child runs under the parent's
/// memory limit, and a multi-threaded runtime (one worker per core) would
/// spend that budget before the page is even read.
pub fn conversion_child(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some(CONVERT_SUBCOMMAND) {
        return None;
    }
    if args.len() != 4 {
        return Some(2);
    }
    let format = match (
        args.get(2).map(String::as_str),
        args.get(3).map(String::as_str),
    ) {
        (Some("--format"), Some("markdown")) => ConvertFormat::Markdown,
        (Some("--format"), Some("text")) => ConvertFormat::Text,
        _ => return Some(2),
    };
    Some(convert_stdio(format))
}

/// The converter itself, as the `internal-html-convert` child runs it: HTML
/// on stdin, converted text on stdout. It builds a DOM, so it only ever runs
/// in a child whose memory, CPU and time are limited by the parent.
pub fn convert_stdio(format: ConvertFormat) -> i32 {
    use std::io::{Read, Write};
    let mut raw = Vec::new();
    if std::io::stdin().read_to_end(&mut raw).is_err() {
        return 1;
    }
    let html = String::from_utf8_lossy(&raw);
    let converted = match format {
        ConvertFormat::Markdown => htmd::HtmlToMarkdown::builder()
            .skip_tags(vec!["script", "style", "noscript", "template", "head"])
            .build()
            .convert(&html)
            .map_err(|e| e.to_string()),
        ConvertFormat::Text => html2text::config::plain_no_decorate()
            .string_from_read(html.as_bytes(), TEXT_WIDTH)
            .map_err(|e| e.to_string()),
    };
    match converted {
        Ok(text) => {
            let mut out = std::io::stdout().lock();
            if out
                .write_all(text.as_bytes())
                .and_then(|_| out.flush())
                .is_ok()
            {
                0
            } else {
                1
            }
        }
        Err(_) => 1,
    }
}

/// Converts HTML to markdown or text; never fails and never builds a DOM in
/// this process. Input past the cap is cut, a page the fast path finds too
/// deep or too large is flattened, the rest goes to a limited child process
/// and falls back to the flattened text if the child fails.
async fn convert_html(
    html: String,
    format: Format,
    converter: Option<&Path>,
    budget: Duration,
) -> String {
    let mut notes = Vec::new();
    let mut input = html.as_str();
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
    let child_format = match format {
        Format::Markdown => ConvertFormat::Markdown,
        _ => ConvertFormat::Text,
    };
    let body = match measure(input) {
        Err(why) => {
            notes.push(format!(
                "{why}; the page was reduced to plain text without structure"
            ));
            flatten(input)
        }
        Ok(()) => {
            let converted = match converter {
                Some(program) => convert_in_child(program, input, child_format, budget).await,
                None => Err("no converter program".to_string()),
            };
            match converted {
                Ok(text) => text,
                Err(_) => {
                    notes.push(UNSAFE_NOTE.to_string());
                    flatten(input)
                }
            }
        }
    };
    if notes.is_empty() {
        return body;
    }
    format!("[note: {}]\n\n{body}", notes.join("; "))
}

/// Runs the converter child on `html`. Any failure (spawn, exit status,
/// signal, limit, timeout) is an `Err`; the child is killed on every path,
/// including when this future is dropped by a cancellation or the tool
/// timeout.
async fn convert_in_child(
    program: &Path,
    html: &str,
    format: ConvertFormat,
    budget: Duration,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::process::Command;

    let mut cmd = Command::new(program);
    cmd.arg(CONVERT_SUBCOMMAND)
        .arg("--format")
        .arg(format.name())
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use nix::sys::resource::{setrlimit, Resource};
        // SAFETY: the closure only calls setrlimit(2), which is
        // async-signal-safe, between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                setrlimit(
                    Resource::RLIMIT_AS,
                    CONVERT_MAX_ADDRESS_SPACE,
                    CONVERT_MAX_ADDRESS_SPACE,
                )
                .map_err(std::io::Error::from)?;
                setrlimit(
                    Resource::RLIMIT_CPU,
                    CONVERT_MAX_CPU_SECS,
                    CONVERT_MAX_CPU_SECS,
                )
                .map_err(std::io::Error::from)?;
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill().await;
        return Err("no pipes".into());
    };
    let input = html.as_bytes();
    let deadline = tokio::time::Instant::now() + budget;

    let io = async {
        let write = async move {
            // A child that stops reading early is judged by its exit status.
            let _ = stdin.write_all(input).await;
            let _ = stdin.shutdown().await;
        };
        let read = async {
            let mut buf = Vec::new();
            (&mut stdout)
                .take(CONVERT_MAX_OUTPUT_BYTES as u64 + 1)
                .read_to_end(&mut buf)
                .await
                .map(|_| buf)
        };
        tokio::join!(write, read).1
    };
    let buf = match tokio::time::timeout_at(deadline, io).await {
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => {
            let _ = child.kill().await;
            return Err(format!("read: {e}"));
        }
        Err(_) => {
            let _ = child.kill().await;
            return Err("the conversion timed out".into());
        }
    };
    let overflow = buf.len() > CONVERT_MAX_OUTPUT_BYTES;
    if overflow {
        // More than the tool will ever show: stop the child, keep the head.
        let _ = child.kill().await;
    } else {
        match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => return Err(format!("the converter failed: {status}")),
            Ok(Err(e)) => return Err(format!("wait: {e}")),
            Err(_) => {
                let _ = child.kill().await;
                return Err("the conversion timed out".into());
            }
        }
    }
    Ok(decode_converter_output(&buf))
}

fn decode_converter_output(buf: &[u8]) -> String {
    let kept = &buf[..buf.len().min(CONVERT_MAX_OUTPUT_BYTES)];
    let kept = if buf.len() > CONVERT_MAX_OUTPUT_BYTES {
        match std::str::from_utf8(kept) {
            Err(error) if error.error_len().is_none() => &kept[..error.valid_up_to()],
            _ => kept,
        }
    } else {
        kept
    };
    String::from_utf8_lossy(kept).into_owned()
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
                // `<!-->` and `<!--->` are complete, empty comments.
                i = if b[lt + 4..].starts_with(b">") {
                    lt + 5
                } else if b[lt + 4..].starts_with(b"->") {
                    lt + 6
                } else {
                    html[lt + 4..].find("-->").map_or(len, |e| lt + 4 + e + 3)
                };
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
                i = if closed { j + 1 } else { len };
                if !f(Tok::Open(name)) {
                    return;
                }
                // HTML ignores a trailing `/` on any element that is not
                // void: `<div/>` opens a div.
                if is_one_of(name, VOID_TAGS) {
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
/// Err says which limit is exceeded. This is only a fast path to the
/// flattened text: the tokenizer is not a full HTML parser, so a page can
/// pass it and still be hostile. Safety comes from the limited child process
/// that does the real conversion, not from this check.
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
    const ENTITIES: &[(&str, char)] = &[
        ("&nbsp;", ' '),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&#39;", '\''),
        ("&amp;", '&'),
    ];
    let mut normalized = String::with_capacity(out.len());
    let mut remaining = out.as_str();
    let mut space = false;
    while let Some(ch) = remaining.chars().next() {
        let (ch, len) = if ch == '&' {
            ENTITIES
                .iter()
                .find_map(|(entity, decoded)| {
                    remaining
                        .starts_with(entity)
                        .then_some((*decoded, entity.len()))
                })
                .unwrap_or((ch, ch.len_utf8()))
        } else {
            (ch, ch.len_utf8())
        };
        remaining = &remaining[len..];
        if ch.is_whitespace() {
            space = !normalized.is_empty();
        } else {
            if space {
                normalized.push(' ');
                space = false;
            }
            normalized.push(ch);
        }
    }
    normalized
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
            binding: None,
            workspace: PathBuf::from("."),
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
        }
    }

    /// Unit tests have no `withnav` binary to convert with; HTML comes back as
    /// flattened text. The conversion child is covered by `tests/fetch_convert.rs`.
    async fn call(args: Value) -> ToolResult {
        FetchTool::new()
            .with_converter("/nonexistent/withnav")
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

    #[test]
    fn flatten_decodes_entities_and_normalizes_visible_text() {
        for (html, expected) in [
            ("a&nbsp;b", "a b"),
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&quot;", "\""),
            ("&#39;", "'"),
            ("&amp;", "&"),
            ("&unknown; &#160;", "&unknown; &#160;"),
            ("&amp;lt;", "&lt;"),
            ("\u{a0}\talpha\u{2003}\u{202f}beta\n\u{3000}", "alpha beta"),
            (
                "a<!-- ignored -->b &am<!-- split entity -->p;lt;",
                "ab &lt;",
            ),
            ("a<b>b</b>c", "a b c"),
            ("a<script>&lt;hidden</script><style>hidden</style>b", "a b"),
            ("", ""),
            (" \t\n&nbsp;\u{2003}", ""),
        ] {
            assert_eq!(flatten(html), expected, "{html:?}");
        }
    }

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
    // When fetched as html
    // Then it is returned raw (markdown and text go through the converter
    // child, covered in tests/fetch_convert.rs)
    #[tokio::test]
    async fn html_format_is_returned_raw() {
        let server = html_server().await;
        let url = format!("{}/page", server.uri());

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

    // Given the WithNav user agent requirement
    // When fetched
    // Then the request carries `withnav/<version>` and no authorization
    #[tokio::test]
    async fn sends_the_withnav_user_agent_and_no_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header(
                "user-agent",
                concat!("withnav/", env!("CARGO_PKG_VERSION")),
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
        let res = call(json!({"url": format!("{}/old?token=private", server.uri())})).await;
        assert_eq!(res.output, "arrived", "{:?}", res.error);
        assert_eq!(res.metadata["redirects"], 1);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests[0].url.query(), Some("token=private"));
        assert_eq!(requests[1].url.query(), None);
        assert!(FetchTool::new().schema()["description"]
            .as_str()
            .unwrap()
            .contains("drops the original query"));
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

    // Given a redirect to a target with credentials, and a 100 KiB Location
    // When fetched
    // Then credentials never appear, and every echo is bounded
    #[tokio::test]
    async fn redirect_echoes_are_redacted_and_bounded() {
        let server = MockServer::start().await;
        let to = format!("http://user:s3cret@{}/x", server.address());
        Mock::given(method("GET"))
            .and(path("/cred"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", to.as_str()))
            .mount(&server)
            .await;
        let res = call(json!({"url": format!("{}/cred", server.uri())})).await;
        let dump = format!("{res:?}");
        assert_eq!(code(&res), "network");
        assert!(
            !dump.contains("s3cret") && !dump.contains("user:"),
            "{dump}"
        );
        assert!(dump.contains("***@"), "{dump}");

        let other = MockServer::start().await;
        let long = format!("{}/{}", other.uri(), "a".repeat(100 * 1024));
        Mock::given(method("GET"))
            .and(path("/long"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", long.as_str()))
            .mount(&server)
            .await;
        let res = call(json!({"url": format!("{}/long", server.uri())})).await;
        assert_eq!(res.status, ToolStatus::Ok);
        assert!(res.output.len() < 4096, "{}", res.output.len());
        assert!(res.output.contains("bytes cut"));
        assert!(res.metadata.to_string().len() < 4096);
    }

    #[test]
    fn shown_redacts_userinfo_and_clips() {
        assert_eq!(shown("http://u:p@h.test/p@q"), "http://***@h.test/p@q");
        assert_eq!(shown("https://h.test/a@b"), "https://h.test/a@b");
        assert_eq!(shown("http://a@b@h.test"), "http://***@h.test");
        assert!(shown(&"é".repeat(5000)).len() < 2200);
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

    // Given hostile markup the tokenizer must read as a browser does
    // When measured
    // Then `<div/>` counts as nesting and `<!-->` closes its comment
    #[test]
    fn the_fast_path_reads_slashes_and_short_comments_like_html() {
        assert!(measure(&"<div/>".repeat(100_000)).is_err());
        let page = format!("<!-->{}", "<div>".repeat(1000));
        assert!(measure(&page).is_err());
        let page = format!("<!--->{}", "<div>".repeat(1000));
        assert!(measure(&page).is_err());
        assert!(measure("<p>a</p><!-- <div> --><br/><img src=x/>").is_ok());
    }

    // Given HTML past the converter input cap and no converter
    // When converted
    // Then it is cut on a char boundary, and the plain text carries both notes
    #[tokio::test]
    async fn oversized_html_is_cut_with_a_note() {
        let page = format!("<p>{}</p>", "é".repeat(CONVERT_MAX_INPUT_BYTES));
        let out = convert_html(page, Format::Markdown, None, Duration::from_secs(5)).await;
        assert!(out.starts_with("[note: the HTML was cut"), "{}", &out[..80]);
        assert!(out.contains(UNSAFE_NOTE));
        for character in ["é", "€", "🦀"] {
            let prefix = "x".repeat(CONVERT_MAX_OUTPUT_BYTES - 1);
            let bytes = format!("{prefix}{character}");
            assert_eq!(decode_converter_output(bytes.as_bytes()), prefix);
        }
        assert_eq!(decode_converter_output(b"ok\xff"), "ok\u{fffd}");
    }

    // Given a converter that cannot start
    // When a page is converted
    // Then the flattened text is returned with the unsafe-conversion note
    #[tokio::test]
    async fn a_missing_converter_falls_back_to_plain_text() {
        let out = convert_html(
            PAGE.to_string(),
            Format::Markdown,
            Some(Path::new("/nonexistent/withnav")),
            Duration::from_secs(5),
        )
        .await;
        assert!(out.contains(UNSAFE_NOTE), "{out}");
        assert!(out.contains("Heading"), "{out}");
        assert!(!out.contains("SCRIPTBODY"), "{out}");
    }

    // Given a Location that does not parse and carries credentials
    // When fetched
    // Then nothing of it is echoed
    #[tokio::test]
    async fn an_unparsable_location_is_never_echoed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "//user:secret@[bad]/"),
            )
            .mount(&server)
            .await;
        let res = call(json!({"url": server.uri()})).await;
        let dump = format!("{res:?}");
        assert_eq!(code(&res), "network", "{dump}");
        assert!(dump.contains("invalid redirect target"), "{dump}");
        assert!(!dump.contains("secret") && !dump.contains("user"), "{dump}");
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
