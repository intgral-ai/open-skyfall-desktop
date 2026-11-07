//! Host-owned wire validation, synchronous document pinning and guarded-route policy.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde_json::{Value, json};
use solidcraft_engine::DocumentStamp;

use crate::{ControlRequest, SolveApp, control};

pub(crate) enum GuardRefusal {
    InvalidRequest { field: String, message: String },
    Stale { expected: DocumentStamp, current: Option<DocumentStamp> },
    Unsupported { capability: String },
    Cancelled { operation: String },
}

#[derive(Clone)]
pub(crate) struct GuardedAdmission {
    pub(crate) expected: DocumentStamp,
    pub(crate) context: Arc<AtomicBool>,
}

fn invalid(field: &str, message: &str) -> GuardRefusal {
    GuardRefusal::InvalidRequest { field: field.into(), message: message.into() }
}

pub(crate) fn parse_expected_document(envelope: &Value) -> Result<Option<DocumentStamp>, GuardRefusal> {
    let Some(value) = envelope.get("expectedDocument") else { return Ok(None) };
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
    Ok(Some(DocumentStamp { document_id: id.into(), revision }))
}

pub(crate) fn stamp_json(stamp: &DocumentStamp) -> Value {
    json!({"documentId":stamp.document_id,"revision":stamp.revision.to_string()})
}

pub(crate) fn refusal_json(error: &GuardRefusal) -> Value {
    // Messages are bounded static diagnostics; never echo hostile paths, command ids or bodies.
    let (code, message, details) = match error {
        GuardRefusal::InvalidRequest { field, message } => ("INVALID_REQUEST", message.as_str(), json!({"field":field})),
        GuardRefusal::Stale { expected, current } => (
            "STALE_DOCUMENT",
            "The document changed before this write could apply.",
            json!({"expected":stamp_json(expected),"current":current.as_ref().map(stamp_json)}),
        ),
        GuardRefusal::Unsupported { capability } => {
            ("UNSUPPORTED_CAPABILITY", "This execution path does not support guarded requests.", json!({"capability":capability}))
        }
        GuardRefusal::Cancelled { operation } => {
            ("CANCELLED", "not run: the agent chat turn ended or its document tab was left", json!({"operation":operation}))
        }
    };
    json!({"ok":false,"error":message,"errorDetails":{"code":code,"message":message,"retry":"never","details":details}})
}

fn unsupported(capability: &str) -> GuardRefusal {
    GuardRefusal::Unsupported { capability: capability.into() }
}

pub(crate) fn cancelled(method: &str) -> GuardRefusal {
    // Never copy an arbitrary wire method (or any parameters/turn identity) into diagnostics.
    let operation = match method {
        "engine.execute" | "command" | "engine.batch" | "document.inspect" | "document.revision" | "engine.commands" => method,
        _ => "control request",
    };
    GuardRefusal::Cancelled { operation: operation.into() }
}

fn check_step(step: &Value, raw: Option<&str>) -> Result<(), GuardRefusal> {
    let Some(raw) = raw else { return Ok(()) };
    // Canonical ids are for capability decisions only; dispatch receives the original spelling.
    let Some(spec) = solidcraft_engine::find_command(raw) else {
        if raw.eq_ignore_ascii_case("document.snapshot.capture") {
            return Err(unsupported("document.snapshot.capture"));
        }
        return Ok(()); // Keep the engine's original unknown-command diagnostic.
    };
    let id = spec.id;
    let excluded =
        matches!(id, "component.edit_in_place" | "component.finish_edit" | "component.update_links" | "timeline.play" | "appearance.favorite")
            || ["export", "import", "save", "library"].iter().any(|word| id.contains(word));
    let area = id.split('.').next().unwrap_or_default();
    let supported = matches!(
        area,
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
    let names_file = step
        .get("params")
        .and_then(Value::as_object)
        .is_some_and(|params| ["path", "paths", "file", "files", "dir", "folder", "url", "out"].iter().any(|key| params.contains_key(*key)));
    if excluded || !supported || names_file { Err(unsupported(id)) } else { Ok(()) }
}

pub(crate) fn check_supported(method: &str, envelope: &Value) -> Result<(), GuardRefusal> {
    match method {
        "engine.execute" | "command" => check_step(envelope, control::execute_command(envelope)),
        "engine.batch" => {
            // The existing batch owns shape/length diagnostics and its 10,000-step limit.
            if let Some(steps) = envelope.get("commands").and_then(Value::as_array).filter(|s| s.len() <= 10_000) {
                for step in steps {
                    check_step(step, control::batch_step_command(step))?;
                }
            }
            Ok(())
        }
        "document.inspect" | "document.revision" | "engine.commands" => Ok(()),
        _ => Err(unsupported("guarded control method")),
    }
}

/// Only visit known control/tool envelopes, never arbitrary nested CAD business data.
fn check_nested_stamps(method: &str, envelope: &Value) -> Result<(), GuardRefusal> {
    let mut todo = vec![(method, envelope, 0)];
    while let Some((method, envelope, depth)) = todo.pop() {
        if depth > 32 {
            return Err(invalid("commands", "batch wrappers are nested too deeply"));
        }
        let refuse = || invalid("expectedDocument", "expectedDocument is only allowed on the outer execute or batch request, not a step");
        if method == "engine.batch"
            && let Some(steps) = envelope.get("commands").and_then(Value::as_array).filter(|s| s.len() <= 10_000)
        {
            // Oversized batches already refuse before any consumption; retain their native error.
            for step in steps {
                if step.get("expectedDocument").is_some() || step.get("params").is_some_and(|params| params.get("expectedDocument").is_some()) {
                    return Err(refuse());
                }
                if control::batch_step_command(step) == Some("execute") {
                    todo.push(("engine.execute", step.get("params").unwrap_or(&Value::Null), depth + 1));
                }
            }
        }
        if matches!(method, "engine.execute" | "command") {
            let params = envelope.get("params").unwrap_or(&Value::Null);
            if params.get("expectedDocument").is_some() {
                return Err(refuse());
            }
            if matches!(control::execute_command(envelope), Some("engine.batch" | "batch")) {
                todo.push(("engine.batch", params, depth + 1));
            }
        }
    }
    Ok(())
}

pub(crate) fn check_application(app: &mut SolveApp, req: &ControlRequest) -> Result<Option<DocumentStamp>, GuardRefusal> {
    if (req.guarded.is_some() || req.params.get("expectedDocument").is_some()) && control::chat_turn_cancelled(req.chat_turn.as_deref()) {
        return Err(cancelled(&req.method));
    }
    check_nested_stamps(&req.method, &req.params)?;
    let expected = match &req.guarded {
        Some(admission) => admission.expected.clone(),
        None => match parse_expected_document(&req.params)? {
            Some(expected) => expected,
            None => return Ok(None),
        },
    };
    check_supported(&req.method, &req.params)?;
    let target_open = app.session.document_id == expected.document_id || app.docs.background().any(|s| s.document_id == expected.document_id);
    let current = target_open.then(|| app.session.document_stamp());
    let context_live = req.guarded.as_ref().is_none_or(|a| a.context.load(Ordering::Relaxed));
    if !context_live || current.as_ref() != Some(&expected) {
        return Err(GuardRefusal::Stale { expected, current });
    }
    Ok(current)
}

pub(crate) fn admit(app: &mut SolveApp, req: &mut ControlRequest) -> Result<(), GuardRefusal> {
    if let Some(expected) = check_application(app, req)?
        && req.guarded.is_none()
    {
        req.guarded = Some(GuardedAdmission { expected, context: Arc::clone(&app.guarded_context) });
    }
    Ok(())
}

pub(crate) fn invalidate_context(app: &mut SolveApp) {
    app.guarded_context.store(false, Ordering::Relaxed);
    app.guarded_context = Arc::new(AtomicBool::new(true));
}

pub(crate) fn stamp_outcome(app: &mut SolveApp, mut out: control::Outcome) -> control::Outcome {
    if let control::Outcome::Done(reply) = &mut out
        && reply.get("ok") == Some(&Value::Bool(true))
        && let Some(object) = reply.as_object_mut()
    {
        object.insert("documentStamp".into(), stamp_json(&app.session.document_stamp()));
    }
    out
}
