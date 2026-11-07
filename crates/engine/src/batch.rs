//! Shared document batch application. Hosts keep their step and history adapters distinct.

use serde_json::{Value, json};

use crate::{ConditionedError, DocumentStamp, EngineError, Session, batch_params, batch_preflight, timing};

impl Session {
    /// Check the live base once, before history or steps; this batch's own edits advance it.
    pub fn batch_if_current(
        &mut self,
        steps: &[Value],
        options: &Value,
        expected: &DocumentStamp,
    ) -> std::result::Result<(Value, DocumentStamp), ConditionedError> {
        self.check_document_stamp(expected)?;
        if steps.len() > 10_000 {
            return Err(ConditionedError::Command(EngineError::Other("engine.batch: commands must be an array of at most 10000 steps".into())));
        }
        let result = match batch_preflight::check(steps, self.param_check, &[]) {
            Some(refusal) => refusal.reply(steps.len()),
            None => run_document_steps(self, steps, options, execute_session, execute_session, CheckpointPolicy::Required),
        };
        Ok((result, self.document_stamp()))
    }
}

fn execute_session(session: &mut Session, id: &str, params: Value) -> Result<Value, String> {
    session.execute(id, &params).map_err(|e| e.to_string())
}

/// Whether a batch may proceed when its requested checkpoint cannot be established.
pub enum CheckpointPolicy {
    BestEffort,
    Required,
}

/// Apply already checked document steps on one owner. Step timing includes the host adapter's
/// tail; total timing also includes checkpoint creation and restore/forget commands.
pub fn run_document_steps<C>(
    context: &mut C,
    steps: &[Value],
    options: &Value,
    execute: fn(&mut C, &str, Value) -> Result<Value, String>,
    history: fn(&mut C, &str, Value) -> Result<Value, String>,
    checkpoint_policy: CheckpointPolicy,
) -> Value {
    let p = options;
    let rollback = p.get("rollback").and_then(Value::as_bool).unwrap_or(true);
    let profile = p.get("profile").and_then(Value::as_bool).unwrap_or(false);
    let timed = profile || p.get("timings").and_then(Value::as_bool).unwrap_or(false);
    let mut rows = Vec::new();
    let clock = timing::Clock::start();
    let checkpoint = if rollback {
        match history(context, "edit.checkpoint", json!({}))
            .and_then(|v| v["checkpoint"].as_u64().ok_or_else(|| "edit.checkpoint did not return a u64 checkpoint id".to_string()))
        {
            Ok(id) => Some(id),
            Err(error) if matches!(checkpoint_policy, CheckpointPolicy::Required) => {
                let reply = json!({"completed":0,"results":[],"undone":0,"rolled_back":false,
                    "not_run":(0..steps.len()).collect::<Vec<_>>(),
                    "failed":{"index":null,"command":"edit.checkpoint","stage":"checkpoint","error":error}});
                return with_timings(reply, timed, rows, clock.ms());
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let mut results = Vec::with_capacity(steps.len());
    // With `coalesce_parameters`, each run of plain parameter edits rebuilds once (#5822); a run
    // whose combined edit is refused goes step by step, so a refused edit is named by its step.
    let coalesce = p.get("coalesce_parameters").and_then(Value::as_bool).unwrap_or(false);
    for run in batch_params::plan(steps, coalesce) {
        let singles = match run {
            batch_params::BatchRun::Step(i) => vec![i],
            batch_params::BatchRun::Params { steps: indices, changes } => {
                let (r, took) = timing::time(profile, || execute(context, "parameters.change", batch_params::combined_params(&changes)));
                match r {
                    Ok(v) => {
                        if timed {
                            rows.push(took.to_json(indices.first().copied().unwrap_or(0), "parameters.change"));
                        }
                        results.extend(batch_params::step_results(steps, &indices, &v));
                        continue;
                    }
                    Err(_) => indices,
                }
            }
        };
        for i in singles {
            let Some(step) = steps.get(i) else { continue };
            let (r, took) = timing::time(profile, || match command(step) {
                Some(id) => execute(context, id, step.get("params").cloned().filter(|v| !v.is_null()).unwrap_or(json!({}))),
                None => Err("missing `command`".to_string()),
            });
            if timed {
                rows.push(took.to_json(i, command(step).unwrap_or_default()));
            }
            match r {
                Ok(v) => results.push(v),
                Err(e) => {
                    let restored = checkpoint.is_some_and(|id| history(context, "edit.restore_checkpoint", json!({"checkpoint": id})).is_ok());
                    let failed = json!({"index": i, "command": command(step), "error": e});
                    // `rolled_back`: the design really is back at its checkpoint.
                    let reply = json!({"completed": i, "results": results, "failed": failed, "undone": if restored { i } else { 0 }, "rolled_back": restored});
                    return with_timings(reply, timed, rows, clock.ms());
                }
            }
        }
    }
    if let Some(id) = checkpoint {
        let _ = history(context, "edit.restore_checkpoint", json!({"checkpoint": id, "forget": true}));
    }
    with_timings(json!({"completed": results.len(), "results": results, "undone": 0}), timed, rows, clock.ms())
}

/// A batch step's command id (first present value, not execute's string fallback).
fn command(step: &Value) -> Option<&str> {
    step.get("command").or_else(|| step.get("id")).and_then(Value::as_str)
}

/// A batch reply with `timings` and `total_ms` when they were asked for.
fn with_timings(mut reply: Value, timed: bool, rows: Vec<Value>, total_ms: f64) -> Value {
    if timed && let Some(o) = reply.as_object_mut() {
        o.insert("timings".into(), Value::Array(rows));
        o.insert("total_ms".into(), json!(timing::round_ms(total_ms)));
    }
    reply
}
