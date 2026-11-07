//! MCP-owned guarded document envelopes; native revisions remain u64.

use serde_json::{Value, json};
use solidcraft_engine::DocumentStamp;

#[derive(Debug)]
pub struct GuardedReply {
    pub result: Value,
    pub document_stamp: DocumentStamp,
}

#[derive(Debug)]
pub enum GuardedCallError {
    Rejected { message: String, error_details: Value },
    CommandFailed(String),
    Pending(String),
    OutcomeUnknown(String),
    NotSent(String),
}

pub(crate) fn unsupported(capability: &str) -> GuardedCallError {
    let message = "This execution path does not support guarded requests.";
    GuardedCallError::Rejected {
        message: message.into(),
        error_details: json!({
            "code":"UNSUPPORTED_CAPABILITY","message":message,"retry":"never","details":{"capability":capability}
        }),
    }
}

pub(crate) fn invalid(field: &str, message: &str) -> GuardedCallError {
    GuardedCallError::Rejected {
        message: message.into(),
        error_details: json!({
            "code":"INVALID_REQUEST","message":message,"retry":"never","details":{"field":field}
        }),
    }
}

pub(crate) fn parse_expected_document(envelope: &Value) -> Result<Option<DocumentStamp>, GuardedCallError> {
    envelope.get("expectedDocument").map(parse_stamp).transpose()
}

fn parse_stamp(value: &Value) -> Result<DocumentStamp, GuardedCallError> {
    let object = value.as_object().ok_or_else(|| invalid("expectedDocument", "expectedDocument must be an object"))?;
    if object.len() != 2 || !object.contains_key("documentId") || !object.contains_key("revision") {
        return Err(invalid("expectedDocument", "expectedDocument requires only documentId and revision"));
    }
    let id = value
        .get("documentId")
        .and_then(Value::as_str)
        .filter(|id| {
            (1..=128).contains(&id.len())
                && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        })
        .ok_or_else(|| invalid("expectedDocument.documentId", "documentId must be a valid ASCII Id of at most 128 bytes"))?;
    let revision = value
        .get("revision")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 20 && (s.len() == 1 || !s.starts_with('0')) && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| invalid("expectedDocument.revision", "revision must be a canonical decimal u64 string"))?;
    Ok(DocumentStamp { document_id: id.into(), revision })
}

pub(crate) fn stamp_json(stamp: &DocumentStamp) -> Value {
    json!({"documentId":stamp.document_id,"revision":stamp.revision.to_string()})
}

/// Decode the actual control reply, not the legacy `outcome` (which discards its siblings).
pub(crate) fn decode_guarded_reply(reply: &Value) -> Result<GuardedReply, GuardedCallError> {
    match reply.get("ok").and_then(Value::as_bool) {
        Some(true) => {
            let unknown = || GuardedCallError::OutcomeUnknown("The app's successful guarded reply lacks a valid result or document stamp.".into());
            let result = reply.get("result").ok_or_else(unknown)?.clone();
            let document_stamp = parse_stamp(reply.get("documentStamp").ok_or_else(unknown)?).map_err(|_| unknown())?;
            Ok(GuardedReply { result, document_stamp })
        }
        Some(false) => {
            let message = reply.get("error").and_then(Value::as_str).unwrap_or("command failed").to_owned();
            match reply.get("errorDetails") {
                Some(details) => Err(GuardedCallError::Rejected { message, error_details: details.clone() }),
                None => Err(GuardedCallError::CommandFailed(message)),
            }
        }
        None => Err(GuardedCallError::OutcomeUnknown("The app did not return a recognizable guarded outcome.".into())),
    }
}

fn execute_command(envelope: &Value) -> Option<&str> {
    envelope.get("command").and_then(Value::as_str).or_else(|| envelope.get("id").and_then(Value::as_str))
}

fn batch_command(step: &Value) -> Option<&str> {
    // Batch intentionally keeps first-present-Value semantics, unlike execute's string fallback.
    step.get("command").or_else(|| step.get("id")).and_then(Value::as_str)
}

fn check_step(step: &Value, raw: Option<&str>) -> Result<(), GuardedCallError> {
    let Some(raw) = raw else { return Ok(()) };
    // Resolve only for policy. The original spelling/params go to the existing command path.
    let Some(spec) = solidcraft_engine::find_command(raw) else {
        return if raw.eq_ignore_ascii_case("document.snapshot.capture") { Err(unsupported("document.snapshot.capture")) } else { Ok(()) };
    };
    let id = spec.id;
    let excluded =
        matches!(id, "component.edit_in_place" | "component.finish_edit" | "component.update_links" | "timeline.play" | "appearance.favorite")
            || ["export", "import", "save", "library"].iter().any(|word| id.contains(word));
    let supported = matches!(
        id.split('.').next().unwrap_or_default(),
        "appearance"
            | "assembly"
            | "body"
            | "component"
            | "config"
            | "construct"
            | "contact"
            | "explode"
            | "expr"
            | "face"
            | "joint"
            | "material"
            | "mesh"
            | "model"
            | "occurrence"
            | "parameters"
            | "plastic"
            | "sheet"
            | "sketch"
            | "solid"
            | "surface"
            | "timeline"
    ) || matches!(
        id,
        "document.inspect"
            | "document.revision"
            | "engine.commands"
            | "engine.describe"
            | "inspect.measure"
            | "inspect.clearance"
            | "inspect.interference"
            | "browser.visibility"
            | "edit.undo"
            | "edit.redo"
    );
    let file_params = step
        .get("params")
        .and_then(Value::as_object)
        .is_some_and(|params| ["path", "paths", "file", "files", "dir", "folder", "url", "out"].iter().any(|key| params.contains_key(*key)));
    if excluded || !supported || file_params { Err(unsupported(id)) } else { Ok(()) }
}

pub(crate) fn check_supported(method: &str, envelope: &Value) -> Result<(), GuardedCallError> {
    check_nested_stamps(method, envelope)?;
    match method {
        "engine.execute" => check_step(envelope, execute_command(envelope)),
        "engine.batch" => {
            // Native batch shape/length/preflight diagnostics stay native, including the 10,000 cap.
            if let Some(steps) = envelope.get("commands").and_then(Value::as_array).filter(|s| s.len() <= 10_000) {
                for step in steps {
                    check_step(step, batch_command(step))?;
                }
            }
            Ok(())
        }
        _ => Err(unsupported("guarded control method")),
    }
}

/// A step may not supply its own document condition, including through tool wrappers.
pub(crate) fn check_nested_stamps(method: &str, envelope: &Value) -> Result<(), GuardedCallError> {
    let mut todo = vec![(method, envelope, 0)];
    while let Some((method, envelope, depth)) = todo.pop() {
        if depth > 32 {
            return Err(invalid("commands", "batch wrappers are nested too deeply"));
        }
        let refuse = || invalid("expectedDocument", "expectedDocument is only allowed on the outer execute or batch request, not a step");
        if method == "engine.batch"
            && let Some(steps) = envelope.get("commands").and_then(Value::as_array)
        {
            for step in steps {
                if step.get("expectedDocument").is_some() || step.get("params").is_some_and(|params| params.get("expectedDocument").is_some()) {
                    return Err(refuse());
                }
                if batch_command(step) == Some("execute") {
                    let params = step.get("params").unwrap_or(&Value::Null);
                    if params.get("expectedDocument").is_some() {
                        return Err(refuse());
                    }
                    todo.push(("engine.execute", params, depth + 1));
                }
            }
        }
        if method == "engine.execute" {
            let params = envelope.get("params").unwrap_or(&Value::Null);
            if params.get("expectedDocument").is_some() {
                return Err(refuse());
            }
            if matches!(execute_command(envelope), Some("engine.batch" | "batch")) {
                todo.push(("engine.batch", params, depth + 1));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_request(method: &str, params: &Value) -> Result<(), GuardedCallError> {
    if parse_expected_document(params)?.is_none() {
        return Err(invalid("expectedDocument", "call_guarded requires expectedDocument"));
    }
    check_supported(method, params)
}
