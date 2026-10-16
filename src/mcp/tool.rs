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

#[derive(Clone)]
pub struct McpTool {
    pub(super) wire: String,
    pub(super) original: String,
    server: Arc<Server>,
    schema: Value,
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
    if let Some(blocks) = content {
        for block in blocks {
            match block["type"].as_str().unwrap_or("") {
                "text" => parts.push(block["text"].as_str().unwrap_or("").to_string()),
                "resource_link" => parts.push(format!(
                    "{} ({})",
                    block["uri"].as_str().unwrap_or(""),
                    block["name"].as_str().unwrap_or("")
                )),
                kind @ ("image" | "audio") => {
                    let bytes = block["data"].as_str().map_or(0, |data| {
                        (data.len().saturating_mul(3) / 4)
                            .saturating_sub(data.chars().rev().take_while(|c| *c == '=').count())
                    });
                    parts.push(format!(
                        "[{kind}: {}, {bytes} bytes]",
                        block["mimeType"].as_str().unwrap_or("unknown")
                    ));
                }
                "resource" => parts.push("[embedded resource omitted]".into()),
                _ => parts.push("[unsupported MCP content]".into()),
            }
        }
    }
    if !has_text && let Some(structured) = value.get("structuredContent") {
        let structured = super::redaction::redact_value(structured, secrets);
        parts.push(serde_json::to_string_pretty(&structured).unwrap_or_default());
    }
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
    result.metadata = json!({"mcp_server": server, "mcp_tool": redact(original)});
    result
}

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
                            Ok(value) => {
                                convert(&value, &self.server.id, &self.original, &self.server.secrets())
                            },
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
            &json!({"content":[{"type":"resource_link","uri":"file:///x","name":"x"},{"type":"image","mimeType":"image/png","data":"AAAA"}]}),
            "test",
            "echo",
            &[],
        );
        assert_eq!(mixed.output, "file:///x (x)\n[image: image/png, 3 bytes]");
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
