//! Live check of `ModelClient::chat` against a real endpoint, two turns with one tool call.
//!
//! cargo run -p withnav-provider --example genai_live [--features genai-chat] -- <models.json key> [variant]
//!
//! Reads `models.json` and `auth.json` from the WithNav config directory, read-only. Prints the
//! deltas, the final message of each turn and the request outcome. With `WITHNAV_GENAI_WIRE=anthropic`
//! (genai transport only) the same provider entry is spoken to as Anthropic Messages.

use tokio_util::sync::CancellationToken;
use withnav_provider::client::ModelClient;
use withnav_provider::config::{ModelsConfig, Variant};
use withnav_provider::openai::{
    ChatMessage, ChatRequest, ClientCache, RequestFunction, RequestToolCall, StreamOptions, ToolDef,
};
use withnav_provider::{StreamDelta, auth};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let key = args
        .next()
        .ok_or("usage: genai_live <models.json key> [variant]")?;
    let variant_name = args.next();
    let config_dir = withnav_provider::dirs::config_dir().ok_or("no config dir")?;
    let models = ModelsConfig::load(&config_dir.join("models.json"))?;
    let resolved = models.resolve(&key)?;
    let api_key = auth::resolve_key(
        &config_dir,
        &resolved.provider_name,
        resolved.provider.api_key_env.as_deref(),
    )?;
    let variant: Option<Variant> = match variant_name.as_deref() {
        None => None,
        Some(name) => Some(
            match resolved.model.variants.as_ref().and_then(|v| v.get(name)) {
                Some(v) => v.clone(),
                None => serde_json::from_str(name)?,
            },
        ),
    };
    let client = ModelClient::new(
        &resolved.provider_name,
        &resolved.provider,
        api_key,
        &ClientCache::new(),
    )?;
    println!(
        "transport: {}  model: {}  variant: {}",
        if withnav_provider::client::GENAI_CHAT {
            "genai"
        } else {
            "built-in"
        },
        resolved.model.model,
        variant
            .as_ref()
            .map_or("none".into(), |v| serde_json::to_string(v).unwrap()),
    );

    let tool = ToolDef::function(
        "get_weather",
        "Current weather for a city.",
        serde_json::json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
    );
    let mut messages = vec![ChatMessage::User {
        content: "What is the weather in Paris? Call get_weather, then answer in one sentence."
            .into(),
        provenance: Default::default(),
    }];

    for turn in 1..=2 {
        let request = ChatRequest {
            model: resolved.model.model.clone(),
            messages: messages.clone(),
            tools: vec![tool.clone()],
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            max_tokens: Some(4096),
            temperature: None,
            extra: Default::default(),
        };
        let started = std::time::Instant::now();
        let (mut thinking, mut text, mut tool_events) = (0usize, 0usize, 0usize);
        let answer = client
            .chat(
                request,
                variant.as_ref(),
                &CancellationToken::new(),
                &mut |d| match d {
                    StreamDelta::Thinking { text } => thinking += text.len(),
                    StreamDelta::Text { text: t } => text += t.len(),
                    StreamDelta::ToolCallStarted { .. } | StreamDelta::ToolCallArgs { .. } => {
                        tool_events += 1
                    }
                },
            )
            .await;
        println!("--- turn {turn} ({} ms)", started.elapsed().as_millis());
        println!("deltas: thinking {thinking} bytes, text {text} bytes, tool events {tool_events}");
        let answer = match answer {
            Ok(answer) => answer,
            Err(e) => {
                println!("error: {e:?}");
                std::process::exit(1);
            }
        };
        println!(
            "text: {:?}\nthinking: {} chars\ntool_calls: {:?}\nfinish_reason: {:?}\nusage: {:?}",
            answer.text,
            answer.thinking.chars().count(),
            answer
                .tool_calls
                .iter()
                .map(|c| (&c.id, &c.name, &c.args, c.args_valid))
                .collect::<Vec<_>>(),
            answer.finish_reason,
            answer.usage,
        );
        if answer.tool_calls.is_empty() {
            break;
        }
        messages.push(ChatMessage::Assistant {
            content: (!answer.text.is_empty()).then(|| answer.text.clone()),
            reasoning_content: (!answer.thinking.is_empty()).then(|| answer.thinking.clone()),
            tool_calls: answer
                .tool_calls
                .iter()
                .map(|c| RequestToolCall {
                    id: c.id.clone(),
                    kind: "function".into(),
                    function: RequestFunction {
                        name: c.name.clone(),
                        arguments: c.args.to_string(),
                    },
                })
                .collect(),
        });
        for call in &answer.tool_calls {
            messages.push(ChatMessage::Tool {
                tool_call_id: call.id.clone(),
                content: "18 °C, sunny, wind 10 km/h".into(),
                answered_by: None,
            });
        }
    }
    Ok(())
}
