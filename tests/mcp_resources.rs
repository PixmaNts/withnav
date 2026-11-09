//! A server's resources reach the agent as two registry tools: one lists, one reads by URI.
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use withnav::{
    mcp::McpManager,
    settings::Settings,
    tools::{Registry, ToolContext, ToolResult, ToolStatus},
};
mod common;

const LIST: &str = "mcp__fixture__list_resources";
const READ: &str = "mcp__fixture__read_resource";
const SECRET: &str = "resource-secret-sentinel";

fn manager(env: Value, server: Value) -> std::sync::Arc<McpManager> {
    let mut config = json!({
        "command": [common::python(), concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py")],
        "env": env,
        "call_timeout_ms": 2000,
    });
    if let (Some(config), Some(extra)) = (config.as_object_mut(), server.as_object()) {
        config.extend(extra.clone());
    }
    let mut settings = Settings::default();
    settings.set_dotted("mcp", json!({ "fixture": config }));
    McpManager::start(&settings)
}

async fn ready(manager: &McpManager) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.statuses().iter().any(|s| s.state == "ready") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture must initialize: {:?}", manager.statuses()));
}

fn registry(manager: &McpManager) -> Registry {
    let mut registry = Registry::new();
    for tool in manager.snapshot() {
        registry.register(Box::new(tool));
    }
    registry
}

fn context() -> ToolContext {
    ToolContext {
        secrets: None,
        binding: None,
        workspace: std::env::temp_dir(),
        writable_roots: vec![],
        readable_roots: vec![],
    }
}

async fn call(registry: &Registry, name: &str, args: Value) -> ToolResult {
    registry
        .dispatch(name, args, CancellationToken::new(), &context())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_server_without_resources_gets_no_resource_tools() {
    let manager = manager(json!({}), json!({}));
    ready(&manager).await;
    let names = registry(&manager).names();
    assert!(names.contains(&"mcp__fixture__echo".to_string()));
    assert!(!names.iter().any(|n| n == LIST || n == READ), "{names:?}");
    manager.shutdown().await;
}

// Given a server that offers resources over two pages and a URI template
// When the agent lists them, then reads a resource made of several pieces
// Then every page is listed, the template is explained, and the pieces come back numbered
#[tokio::test]
async fn resources_are_listed_and_read_as_numbered_pieces() {
    let manager = manager(json!({"RESOURCES": "1", "SECRET": SECRET}), json!({}));
    ready(&manager).await;
    let registry = registry(&manager);
    assert!(registry.names().contains(&LIST.to_string()));
    assert!(registry.names().contains(&READ.to_string()));
    assert_eq!(manager.statuses()[0].tool_count, 13, "11 server tools + 2");

    let listing = call(&registry, LIST, json!({})).await;
    assert_eq!(listing.status, ToolStatus::Ok, "{}", listing.output);
    assert_eq!(listing.metadata["mcp_tool"], "list_resources");
    let text = &listing.output;
    assert!(
        text.contains("- file:///notes/a.md | a | text/markdown | First note"),
        "{text}"
    );
    assert!(
        text.contains("- x://multi | multi | Several pieces"),
        "{text}"
    );
    assert!(text.contains("read one with mcp__fixture__read_resource"));
    assert!(
        text.contains("- x://notes/{id} | note | One note by id"),
        "{text}"
    );
    assert!(text.contains("fill each {placeholder}"));
    // A URI holding a configured secret is withheld, not shown redacted.
    assert!(!text.contains("sentinel"), "{text}");
    assert!(text.contains("an entry whose URI contains a configured secret is not shown"));

    let read = call(&registry, READ, json!({"uri": "x://multi"})).await;
    assert_eq!(read.status, ToolStatus::Ok, "{}", read.output);
    assert_eq!(read.metadata["mcp_resource_pieces"], 3);
    assert_eq!(
        read.output,
        "[1/3] x://multi (text/plain)\nfirst piece\n\n\
         [2/3] x://multi#image (image/png)\n[binary content: image/png, 8 bytes, not shown]\n\n\
         [3/3] x://multi#notes (no type)\nsecond piece"
    );
    assert!(read.images.is_empty());

    let empty = call(&registry, READ, json!({"uri": "x://empty"})).await;
    assert_eq!(empty.output, "The resource is empty.");
    manager.shutdown().await;
}

#[tokio::test]
async fn resource_text_is_redacted_bounded_and_server_errors_stay_out() {
    let manager = manager(json!({"RESOURCES": "1", "SECRET": SECRET}), json!({}));
    ready(&manager).await;
    let registry = registry(&manager);
    let secret = call(&registry, READ, json!({"uri": "x://secret"})).await;
    assert!(secret.output.contains("token "), "{}", secret.output);
    assert!(!secret.output.contains("sentinel"), "{}", secret.output);

    let big = call(&registry, READ, json!({"uri": "x://big"})).await;
    assert!(big.output.len() <= 64 * 1024);
    assert!(big.truncated.unwrap().original_bytes > 100_000);

    // The server's error message carries the secret: only a fixed text comes back.
    let missing = call(&registry, READ, json!({"uri": "x://missing"})).await;
    assert_eq!(missing.status, ToolStatus::Error);
    assert_eq!(missing.error.as_ref().unwrap().code, "mcp_tool_error");
    assert!(!missing.output.contains("sentinel"));

    for args in [
        json!({}),
        json!({"uri": ""}),
        json!({"uri": 1}),
        json!({"uri": "x://multi", "more": true}),
        json!({"uri": "x".repeat(2049)}),
    ] {
        let refused = call(&registry, READ, args.clone()).await;
        assert_eq!(refused.error.unwrap().code, "invalid_args", "{args}");
    }
    let refused = call(&registry, LIST, json!({"cursor": "x"})).await;
    assert_eq!(refused.error.unwrap().code, "invalid_args");
    manager.shutdown().await;
}

#[tokio::test]
async fn a_cancelled_read_is_interrupted_and_the_server_told() {
    let manager = manager(json!({"RESOURCES": "1"}), json!({}));
    ready(&manager).await;
    let registry = registry(&manager);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        token.cancel();
    });
    let result = registry
        .dispatch(READ, json!({"uri": "x://slow"}), cancel, &context())
        .await
        .unwrap();
    canceller.await.unwrap();
    assert_eq!(result.status, ToolStatus::Interrupted);
    assert_eq!(
        call(&registry, "mcp__fixture__cancellations", json!({}))
            .await
            .output,
        "1"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn partial_listings_say_so() {
    // A server without templates still lists its resources, and says it has no templates.
    let manager_a = manager(json!({"RESOURCES": "1", "NO_TEMPLATES": "1"}), json!({}));
    // A server that repeats its cursor gets a listing marked incomplete.
    let manager_b = manager(json!({"RESOURCES": "1", "LOOP_CURSOR": "1"}), json!({}));
    ready(&manager_a).await;
    ready(&manager_b).await;
    let listing = call(&registry(&manager_a), LIST, json!({})).await;
    assert_eq!(listing.status, ToolStatus::Ok);
    assert!(
        listing
            .output
            .starts_with("Note: this server lists no resource templates")
    );
    assert!(listing.output.contains("x://multi"));
    assert!(!listing.output.contains("Resource templates"));
    let looping = call(&registry(&manager_b), LIST, json!({})).await;
    assert!(
        looping
            .output
            .starts_with("Note: resources/list incomplete: the server repeated a page cursor"),
        "{}",
        looping.output
    );
    manager_a.shutdown().await;
    manager_b.shutdown().await;
}

#[tokio::test]
async fn a_resources_only_server_is_not_asked_for_tools() {
    let manager = manager(json!({"RESOURCES_ONLY": "1"}), json!({}));
    ready(&manager).await;
    let mut names = registry(&manager).names();
    names.sort();
    assert_eq!(names, [LIST, READ]);
    manager.shutdown().await;
}

// Given a server tool whose wire name is `read_resource`, and a deny glob on `list_resources`
// Then the server's own tool keeps the name, the added one is reported skipped, and the
//      denied one is absent; the MCP Apps bridge never reaches an added tool
#[tokio::test]
async fn server_tools_keep_their_names_and_deny_globs_apply() {
    let manager = manager(
        json!({"RESOURCES": "1", "TOOL_NAMES": json!(["read.resource", "echo"]).to_string()}),
        json!({"tools": {"list_resources": false}}),
    );
    ready(&manager).await;
    let registry = registry(&manager);
    let mut names = registry.names();
    names.sort();
    assert_eq!(names, ["mcp__fixture__echo", READ]);
    // The server's own tool answers (the fixture echoes the arguments).
    let own = call(&registry, READ, json!({"text": "mine"})).await;
    assert_eq!(own.output, "{\"text\": \"mine\"}");
    assert_eq!(own.metadata["mcp_tool"], "read.resource");
    let status = &manager.statuses()[0];
    assert!(
        status
            .skipped_tools
            .iter()
            .any(|s| s == "read_resource: name taken by a server tool"),
        "{:?}",
        status.skipped_tools
    );

    // Only real server tools are callable from an MCP App.
    let other = plain_resource_server().await;
    assert!(
        other
            .call_ui_tool("fixture", "read_resource", Default::default())
            .await
            .is_err()
    );
    assert!(
        other
            .call_ui_tool("fixture", READ, Default::default())
            .await
            .is_err()
    );
    other.shutdown().await;
    manager.shutdown().await;
}

async fn plain_resource_server() -> std::sync::Arc<McpManager> {
    let manager = manager(json!({"RESOURCES": "1"}), json!({}));
    ready(&manager).await;
    manager
}

// Given a listing far over the 64 KiB limit, and a templates page slower than the deadline
// Then the first stops at the limit with a leading note and no templates request result, and
//      the second fails with a timeout instead of reporting "no templates"
#[tokio::test]
async fn listings_stop_at_the_output_limit_and_share_one_deadline() {
    let big = manager(json!({"RESOURCES": "1", "BIG_LISTING": "1"}), json!({}));
    let slow = manager(
        json!({"RESOURCES": "1", "SLOW_TEMPLATES": "1.5"}),
        json!({"call_timeout_ms": 300}),
    );
    ready(&big).await;
    ready(&slow).await;
    let listing = call(&registry(&big), LIST, json!({})).await;
    assert_eq!(listing.status, ToolStatus::Ok);
    assert!(
        listing
            .output
            .starts_with("Note: listing incomplete: it reached the 64 KiB output limit"),
        "{}",
        &listing.output[..200]
    );
    assert!(listing.output.len() <= 64 * 1024);
    assert!(
        !listing.output.contains("x://big/2/"),
        "paging stops once the limit is reached"
    );
    let timed_out = call(&registry(&slow), LIST, json!({})).await;
    assert_eq!(timed_out.status, ToolStatus::Error, "{}", timed_out.output);
    assert_eq!(timed_out.error.unwrap().code, "mcp_timeout");
    big.shutdown().await;
    slow.shutdown().await;
}

// A secret spread over two lines, or crossing the description limit, is redacted before the
// listing folds whitespace or cuts the description.
#[tokio::test]
async fn listing_fields_are_redacted_before_they_are_reshaped() {
    let manager = manager(
        json!({"RESOURCES": "1", "DESCRIBE_SECRET": "1", "SECRET": "multi\nline-resource-sentinel"}),
        json!({}),
    );
    ready(&manager).await;
    let listing = call(&registry(&manager), LIST, json!({})).await;
    assert!(
        listing.output.contains("x://described"),
        "{}",
        listing.output
    );
    assert!(listing.output.contains("x://notes/{id}"));
    for fragment in ["multi", "line-res", "sentinel"] {
        assert!(!listing.output.contains(fragment), "{}", listing.output);
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn a_listing_names_no_reader_when_a_server_tool_holds_the_name() {
    let manager = manager(
        json!({"RESOURCES": "1", "TOOL_NAMES": json!(["read.resource"]).to_string()}),
        json!({}),
    );
    ready(&manager).await;
    let listing = call(&registry(&manager), LIST, json!({})).await;
    assert!(
        listing
            .output
            .starts_with("Note: no resource read tool is available"),
        "{}",
        listing.output
    );
    assert!(!listing.output.contains(READ));
    manager.shutdown().await;
}
