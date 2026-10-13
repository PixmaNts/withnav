//! Bounded, no-follow script evidence. It never supplies authorization.
use std::io::Read;
use std::os::unix::fs::MetadataExt;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{bash_command, sensitive_read, shell};

const SCRIPT_BYTES: usize = 48 * 1024;

pub(super) fn attach(
    actor: &super::super::actor::AgentActor,
    call: &crate::provider::ToolCall,
    state: &mut Value,
) {
    let needed = super::super::permissions::Needed {
        scope: super::super::permissions::Scope::Folder(actor.tool_context.workspace.clone()),
        destination: None,
    };
    if call.name != "bash"
        || actor.protected_config_unresolved
        || super::sensitive_call(call, &needed, &actor.tool_context)
        || super::protected_config_call(actor, call, &needed)
    {
        return;
    }
    let Some(command) = bash_command(call) else {
        return;
    };
    if command.contains("'~") || command.contains("\"~") {
        return;
    }
    let Some(segments) = shell::segments(command) else {
        return;
    };
    // Multiple invocations and changed working directories need separate bindings.
    if segments.len() != 1 {
        return;
    }
    let Some((operand, shebang)) = shell::script_operand(&segments[0], &state["host_facts"]) else {
        return;
    };
    let Ok(spelling) = super::super::permissions::requested_path(
        std::path::Path::new(operand),
        &actor.tool_context,
    ) else {
        return;
    };
    let Ok(path) = super::super::permissions::resolve_read_target(
        std::path::Path::new(operand),
        &actor.tool_context,
    ) else {
        return;
    };
    if sensitive_read(&spelling)
        || sensitive_read(&path)
        || actor
            .protected_config_root
            .as_ref()
            .is_some_and(|root| path.starts_with(root))
        || actor
            .protected_config_spelling
            .as_ref()
            .is_some_and(|root| spelling.starts_with(root))
    {
        return;
    }
    // Open the original spelling too: canonicalization must not hide a link.
    let Ok(file) = crate::tools::nofollow::open_read(&spelling) else {
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    if !metadata.is_file() || metadata.nlink() != 1 || (shebang && metadata.mode() & 0o111 == 0) {
        return;
    }
    let mut bytes = Vec::new();
    if file
        .take((SCRIPT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return;
    }
    let interpreter = if shebang {
        let Some(interpreter) = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.lines().next())
            .and_then(|line| line.strip_prefix("#!"))
            .and_then(|line| line.split_whitespace().next())
            .filter(|word| std::path::Path::new(word).is_absolute())
            .and_then(|word| shell::trusted_program(word, &state["host_facts"]))
            .filter(|(name, _)| name != "env")
        else {
            return;
        };
        Some(interpreter.1)
    } else {
        None
    };
    let mut parents = Vec::new();
    for parent in path.ancestors().skip(1) {
        let Ok(parent_metadata) =
            crate::tools::nofollow::open_read(parent).and_then(|file| Ok(file.metadata()?))
        else {
            return;
        };
        parents
            .push(json!({"path":parent, "dev":parent_metadata.dev(), "ino":parent_metadata.ino()}));
    }
    let complete = bytes.len() <= SCRIPT_BYTES && metadata.len() == bytes.len() as u64;
    bytes.truncate(SCRIPT_BYTES);
    let hash = hex::encode(Sha256::digest(&bytes));
    let Ok(text) = String::from_utf8(bytes) else {
        return;
    };
    state["untrusted_evidence"]["script"] = json!({
        "path": path, "sha256": hash, "bytes": metadata.len(),
        "sha256_scope": if complete { "full_file" } else { "read_prefix" },
        "truncated": !complete, "text": text, "interpreter": interpreter,
    });
    state["host_facts"]["script_binding"] = json!({
        "path": path, "spelling": spelling, "operand": operand,
        "sha256": if complete { Some(hash) } else { None }, "bytes": metadata.len(), "complete": complete,
        "dev": metadata.dev(), "ino": metadata.ino(), "parents": parents,
    });
}

/// Retain script evidence before excerpts; JSON escaping counts against the budget.
pub(super) fn fit(state: &mut Value, instructions: usize, limit: usize) {
    if state.to_string().len() + instructions <= limit {
        return;
    }
    state["untrusted_evidence"]["assistant_text"] = Value::Null;
    state["untrusted_evidence"]["parent_agent_evidence"] = Value::Null;
    if state.to_string().len() + instructions <= limit
        || !state["untrusted_evidence"]["script"]["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    {
        return;
    }
    state["untrusted_evidence"]["script"]["truncated"] = json!(true);
    state["host_facts"]["script_binding"]["complete"] = json!(false);
    let mut bytes = state.to_string().len() + instructions;
    let text = state["untrusted_evidence"]["script"]["text"]
        .as_str()
        .unwrap();
    let mut end = text.len();
    loop {
        let previous = end;
        end = end.saturating_sub(1024);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        // Count each discarded suffix once, retaining the old UTF-8 cut points
        // and exact JSON escaping without reserializing the unchanged state.
        bytes -= serde_json::to_string(&text[end..previous])
            .expect("script text JSON")
            .len()
            - 2;
        if bytes <= limit || end == 0 {
            break;
        }
    }
    state["untrusted_evidence"]["script"]["text"] = json!(&text[..end]);
}

pub(crate) fn still_matches(note: Option<&Value>) -> bool {
    let Some(binding) = note.and_then(|note| {
        note.get("script_binding").or_else(|| {
            note.get("permission_gate")
                .and_then(|gate| gate.get("script_binding"))
        })
    }) else {
        return true;
    };
    crate::tools::bash::script_matches(binding)
}
