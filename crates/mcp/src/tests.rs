//! An agent builds parts through MCP only and verifies them through MCP only.

use std::f64::consts::PI;

use serde_json::{Value, json};

use crate::{Headless, PROTOCOL_VERSION, Server, tool_definitions};

fn guarded_headless_stamp(h: &mut Headless) -> Value {
    let stamp = h.session.document_stamp();
    json!({"documentId":stamp.document_id,"revision":stamp.revision.to_string()})
}

// These adapters observe the real tools/Headless seam; they do not implement guard behavior.
#[derive(Default)]
struct GuardToolBackend {
    headless: Headless,
    calls: Vec<(bool, String, Value)>,
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl crate::Backend for GuardToolBackend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.calls.push((false, method.into(), params.clone()));
        crate::Backend::call(&mut self.headless, method, params)
    }
    fn call_guarded(&mut self, method: &str, params: Value) -> Result<crate::GuardedReply, crate::GuardedCallError> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.calls.push((true, method.into(), params.clone()));
        crate::Backend::call_guarded(&mut self.headless, method, params)
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "observed Headless".into()
    }
}

fn guarded_tool_body(tool: &Value) -> Value {
    let body: Value = serde_json::from_str(tool["content"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap()).unwrap();
    assert_eq!(tool["structuredContent"], body, "readable and structured facts must agree: {tool}");
    body
}

// Documentation literals must match the real tool/native paths, not another copy of prose.
#[test]
fn guarded_documented_examples_match_actual_shapes() {
    use crate::Backend;
    use crate::document_guard::{parse_expected_document, stamp_json};
    use solidcraft_engine::schema::ParamCheck;

    fn examples(text: &str) -> Vec<Value> {
        let (_, rest) = text.split_once("<!-- guarded-examples -->").expect("missing guarded-examples opening marker");
        let (section, _) = rest.split_once("<!-- /guarded-examples -->").expect("missing guarded-examples closing marker");
        let json = section.trim().strip_prefix("```json\n").unwrap().strip_suffix("```").unwrap();
        serde_json::from_str(json).unwrap()
    }
    fn normalize(value: &mut Value, before: &Value, after: &Value) {
        if value == before {
            *value = json!({"documentId":"doc-A","revision":"9007199254740993"});
        } else if value == after {
            *value = json!({"documentId":"doc-A","revision":"9007199254740994"});
        } else {
            match value {
                Value::Object(object) => object.values_mut().for_each(|v| normalize(v, before, after)),
                Value::Array(array) => array.iter_mut().for_each(|v| normalize(v, before, after)),
                _ => {}
            }
        }
    }
    let control = examples(include_str!("../../../docs/control-protocol.md"));
    let mcp = examples(include_str!("../../../docs/mcp.md"));
    for label in ["matching-execute", "matching-batch", "stale", "closed", "unsupported", "primitive-wrapper", "pending", "expectation-failed"] {
        for (surface, entries) in [("control", &control), ("mcp", &mcp)] {
            let entry = entries.iter().find(|e| e["label"] == label).unwrap_or_else(|| panic!("missing {surface} label {label}"));
            let mut args = entry["arguments"].clone();
            let tool = entry["tool"].as_str().unwrap();
            let method = if tool == "batch" { "engine.batch" } else { "engine.execute" };
            let mut headless = Headless::default();
            let discovery = headless.call("engine.execute", json!({"command":"document.revision"})).unwrap();
            let native = headless.session.document_stamp();
            assert_eq!(discovery["document_id"], native.document_id);
            assert_eq!(discovery["revision"].as_u64(), Some(native.revision));
            let template = parse_expected_document(&args).unwrap().unwrap();
            assert_eq!(template.document_id, "doc-A");
            assert_eq!(template.revision, 9_007_199_254_740_993u64);
            let before = guarded_headless_stamp(&mut headless);
            args["expectedDocument"] = before.clone();
            if label == "stale" {
                headless.session.execute("parameters.change", &json!({"name":"width","expression":"30 mm"})).unwrap();
            }
            let actual = if matches!(label, "closed" | "primitive-wrapper" | "pending") {
                // Fixed protocol fixtures from Task7's decode/wrapper/pending tests. This tests
                // real Remote/tools decoding, not a claim that Headless can close a GUI tab.
                let original = if label == "closed" {
                    json!({"ok":false,"error":"stale","errorDetails":{"code":"STALE_DOCUMENT","message":"stale","retry":"never",
                        "details":{"expected":{"documentId":"doc-A","revision":"9007199254740993"},"current":null}}})
                } else {
                    json!({"ok":true,"result":7,"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}})
                };
                let mut polls = 0;
                let reply = original.clone();
                let peer = GuardPeer::new(move |_, req| {
                    GuardAnswer::Reply(match req["method"].as_str() {
                        Some("control.capabilities") => guard_capabilities(),
                        Some("control.pending") => {
                            polls += 1;
                            if polls == 1 {
                                json!({"ok":true,"result":{"pending":1,"done":false}})
                            } else {
                                json!({"ok":true,"result":{"pending":1,"done":true,"reply":reply}})
                            }
                        }
                        _ if label == "pending" => running(),
                        _ => reply.clone(),
                    })
                });
                let mut remote = peer.remote(ParamCheck::Off);
                let initial = crate::call_tool(&mut remote, tool, &entry["arguments"]);
                let result = if label == "pending" {
                    assert_eq!(initial["isError"], true);
                    let body = guarded_tool_body(&initial);
                    assert_eq!(body["pending"], true);
                    assert_eq!(body["expectation"], Value::Null);
                    crate::call_tool(&mut remote, "pending", &json!({}))
                } else {
                    initial
                };
                let body = guarded_tool_body(&result);
                assert_eq!(result["isError"] == true, entry["isError"].as_bool().unwrap());
                assert_eq!(peer.finish(remote).iter().filter(|(_, r)| r["method"] == method).count(), 1);
                if surface == "control" && label != "pending" { original } else { body }
            } else if surface == "control" && label != "expectation-failed" {
                // Same native/guard-to-control carrier construction as Task7's actual peer.
                match headless.call_guarded(method, args) {
                    Ok(r) => json!({"ok":true,"result":r.result,"documentStamp":stamp_json(&r.document_stamp)}),
                    Err(crate::GuardedCallError::Rejected { message, error_details }) => {
                        json!({"ok":false,"error":message,"errorDetails":error_details})
                    }
                    other => panic!("{other:?}"),
                }
            } else {
                let reply = crate::call_tool(&mut headless, tool, &args);
                assert_eq!(reply["isError"] == true, entry["isError"].as_bool().unwrap());
                guarded_tool_body(&reply)
            };
            let after = guarded_headless_stamp(&mut headless);
            if label == "expectation-failed" {
                assert_ne!(before, after, "expectation failure must not erase the successful write");
                assert_eq!(headless.session.doc.param("width").unwrap().expr, "20 mm");
            }
            let mut actual = actual;
            if !matches!(label, "closed" | "primitive-wrapper" | "pending") {
                normalize(&mut actual, &before, &after);
            }
            assert_eq!(actual, entry["reply"], "{surface}/{label}");
        }
    }
    let vectors = |entries: &[Value]| entries.iter().find(|e| e["label"] == "envelope-vectors").unwrap()["vectors"].clone();
    // The existing UI control test reads these same UTF-8 strings from the same document.
    for vector in vectors(&control).as_array().unwrap() {
        let input = vector["utf8"].as_str().unwrap();
        let envelope: Value = serde_json::from_slice(input.as_bytes()).unwrap();
        let parsed = parse_expected_document(&envelope);
        if vector["accept"] == true {
            let native = parsed.unwrap().unwrap();
            assert_eq!(native.revision.to_string(), vector["revision"]);
            assert_eq!(stamp_json(&native), envelope["expectedDocument"]);
        } else {
            assert!(
                matches!(parsed, Err(crate::GuardedCallError::Rejected { error_details, .. }) if error_details["code"] == "INVALID_REQUEST"),
                "{input}"
            );
        }
    }
}

// Actual scoped transport, not just envelope arithmetic: the peer counts even a partial batch.
#[test]
fn guarded_mcp_batch_exact_wire_budget_includes_scope_and_framing() {
    use crate::Backend;
    use solidcraft_engine::io::vfs::{self, PathScope};
    use solidcraft_engine::schema::ParamCheck;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::channel,
    };
    use std::time::Duration;

    let base = vfs::temp_dir().join(format!("solidcraft-wire-r1-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let roots: Vec<_> = (0..400)
        .map(|i| {
            let p = base.join(format!("{i:03}-{}", "r".repeat(180)));
            std::fs::create_dir(&p).unwrap();
            p
        })
        .collect();
    let scope = PathScope::new(roots.clone()).with_design_folder(&roots[0]);
    assert_eq!(scope.roots().len(), 400);
    assert_eq!(scope.design_roots().len(), 1);
    let root_json = json!(scope.roots().iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>());
    assert!(root_json.to_string().len() > 64 * 1024);
    let mut failures = Vec::new();
    // MAX_LINE counts the JSON text; the existing transport appends one LF after that text.
    for (target, blocked, level, steps) in [
        (0, false, ParamCheck::Strict, 1),
        (0, false, ParamCheck::Strict, 10_000),
        (solidcraft_engine::MAX_LINE - 1, false, ParamCheck::Off, 1),
        (solidcraft_engine::MAX_LINE, false, ParamCheck::Off, 1),
        (solidcraft_engine::MAX_LINE + 1, true, ParamCheck::Off, 1),
        (solidcraft_engine::MAX_LINE + 16 * 1024, true, ParamCheck::Warn, 1),
    ] {
        let mut h = Headless::default();
        let stamp = guarded_headless_stamp(&mut h);
        let checkpoints = Arc::new(AtomicUsize::new(0));
        let cp = checkpoints.clone();
        h.session.set_command_check(Some(Arc::new(move |id, _, _| {
            if id == "edit.checkpoint" {
                cp.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })));
        let token = "fixture\"\\\n\ttoken";
        let mut args = json!({"commands":[{"command":"parameters.change","params":{"name":"width","expression":"10 mm","padding":""}}],"expectedDocument":stamp});
        if target == 0 {
            args["commands"] = json!(vec![json!({"command":"parameters.list"}); steps]);
        }
        let expected_envelope = |args: &Value| {
            let mut params = args.clone();
            params["param_check"] = json!(level.name());
            params["allow_paths"] = root_json.clone();
            params["design_folders"] = json!(scope.design_roots().iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>());
            json!({"id":u64::MAX-1,"method":"engine.batch","params":params,"token":token})
        };
        let overhead = expected_envelope(&args).to_string().len();
        if target > 0 {
            args["commands"][0]["params"]["padding"] = json!("x".repeat(target - overhead));
        }
        let line_len = expected_envelope(&args).to_string().len();
        assert_eq!(line_len, if target == 0 { overhead } else { target });
        assert!(args.to_string().len() + 64 * 1024 <= solidcraft_engine::MAX_LINE, "exercise beyond the old margin, not the early refusal");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (done_tx, done_rx) = channel();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut cap = String::new();
            reader.read_line(&mut cap).unwrap();
            let cap: Value = serde_json::from_str(&cap).unwrap();
            assert_eq!(cap["method"], "control.capabilities");
            assert_eq!(cap["id"], u64::MAX - 2);
            let mut reply = guard_capabilities();
            reply["id"] = cap["id"].clone();
            writeln!(stream, "{reply}").unwrap();
            let mut buf = [0; 8192];
            let n = reader.read(&mut buf).unwrap();
            if n == 0 || blocked {
                // Report the first bytes even if no full JSON line ever arrived. Closing is
                // also the deterministic unblock for the old implementation's partial write.
                stream.shutdown(std::net::Shutdown::Both).unwrap();
                done_tx.send((n, 0, checkpoints.load(Ordering::SeqCst), Value::Null)).unwrap();
                return;
            }
            let mut bytes = buf[..n].to_vec();
            if !bytes.ends_with(b"\n") {
                reader.read_until(b'\n', &mut bytes).unwrap();
            }
            assert_eq!(bytes.last(), Some(&b'\n'));
            let request: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
            assert_eq!(request["method"], "engine.batch");
            assert_eq!(request["id"], u64::MAX - 1);
            assert_eq!(request["token"], token);
            assert_eq!(request["params"]["param_check"], level.name());
            let result = h.call_guarded("engine.batch", request["params"].clone()).unwrap();
            assert!(result.result.get("failed").is_none(), "{}", result.result);
            if request["params"]["commands"][0]["command"] == "parameters.change" {
                assert_eq!(h.session.doc.param("width").unwrap().expr, "10 mm");
            }
            let reply = json!({"ok":true,"result":result.result,"documentStamp":{"documentId":result.document_stamp.document_id,"revision":result.document_stamp.revision.to_string()},"param_check":level.name()});
            writeln!(stream, "{reply}").unwrap();
            done_tx.send((bytes.len(), 1, checkpoints.load(Ordering::SeqCst), request)).unwrap();
        });
        let mut remote = crate::Remote::new(&addr, Some(token)).with_param_check(level);
        remote.set_next_id(u64::MAX - 2);
        let result = vfs::scoped(Some(scope.clone()), || crate::call_tool(&mut remote, "batch", &args));
        // NotSent must not publish Outstanding or latch unknown. This query is local and must
        // not manufacture another original batch or consume a different request.
        let pending = remote.call("control.pending", json!({})).unwrap();
        drop(remote);
        let (bytes, batches, cp, request) = done_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        peer.join().unwrap();
        println!(
            "WIRE_BUDGET_R1 {}",
            json!({"line_bytes":line_len,"params_bytes":args.to_string().len(),"scope_roots_bytes":root_json.to_string().len(),"steps":steps,
            "raw_batch_bytes":bytes,"native_batches":batches,"checkpoints":cp,"not_sent":result["structuredContent"]["notSent"]==true})
        );
        if blocked {
            if result["structuredContent"]["notSent"] != true || bytes != 0 || batches != 0 || cp != 0 || pending != json!({"pending":null}) {
                failures.push(format!("target={target}: bytes={bytes}, batches={batches}, checkpoints={cp}, pending={pending}, result={result}"));
            }
        } else {
            assert_ne!(result["isError"], true, "{result}");
            assert_eq!(bytes, line_len + 1);
            assert_eq!(batches, 1);
            assert_eq!(cp, 1);
            assert_eq!(request["params"]["allow_paths"], root_json);
            assert_eq!(request["params"]["design_folders"], expected_envelope(&args)["params"]["design_folders"]);
        }
    }
    for p in roots {
        std::fs::remove_dir(p).unwrap();
    }
    std::fs::remove_dir(base).unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn guarded_mcp_published_revision_schema_contract() {
    let defs = tool_definitions();
    let schemas: Value = ["execute", "batch"]
        .into_iter()
        .map(|name| {
            (
                name.to_string(),
                defs.iter().find(|d| d["name"] == name).unwrap()["inputSchema"]["properties"]["expectedDocument"]["properties"]["revision"].clone(),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    // The adjunct evaluates these actual published schemas, not the runtime stamp parser.
    println!("REVISION_SCHEMA_R1 {schemas}");
    let expected_pattern = concat!(
        "^(?:0|[1-9][0-9]{0,18}|",
        "1[0-7][0-9]{18}|18[0-3][0-9]{17}|184[0-3][0-9]{16}|1844[0-5][0-9]{15}|",
        "18446[0-6][0-9]{14}|184467[0-3][0-9]{13}|1844674[0-3][0-9]{12}|",
        "184467440[0-6][0-9]{10}|1844674407[0-2][0-9]{9}|18446744073[0-6][0-9]{8}|",
        "1844674407370[0-8][0-9]{6}|18446744073709[0-4][0-9]{5}|184467440737095[0-4][0-9]{4}|",
        "18446744073709550[0-9]{3}|18446744073709551[0-5][0-9]{2}|1844674407370955160[0-9]|",
        "1844674407370955161[0-5])(?![\\s\\S])"
    );
    for name in ["execute", "batch"] {
        assert_eq!(schemas[name]["type"], "string");
        assert_eq!(schemas[name]["maxLength"], 20);
        assert_eq!(schemas[name]["pattern"], expected_pattern, "published string range contract");
    }
}

#[test]
fn guarded_mcp_published_document_id_schema_contract() {
    let defs = tool_definitions();
    let schemas: Value = ["execute", "batch"]
        .into_iter()
        .map(|name| {
            (
                name.to_string(),
                defs.iter().find(|d| d["name"] == name).unwrap()["inputSchema"]["properties"]["expectedDocument"]["properties"]["documentId"].clone(),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    println!("DOCUMENT_ID_SCHEMA_WAVE {schemas}");
    let cases = [
        (json!("a"), true),
        (json!("0"), true),
        (json!("A0._:-z"), true),
        (json!("x".repeat(128)), true),
        (json!("x".repeat(129)), false),
        (json!(""), false),
        (json!("_bad"), false),
        (json!("-bad"), false),
        (json!("a/b"), false),
        (json!("中文"), false),
        (json!("é"), false),
        (json!("a\n"), false),
        (json!("a\r"), false),
        (json!("a\r\n"), false),
        (json!("a\nb"), false),
        (json!(null), false),
        (json!(7), false),
        (json!(true), false),
        (json!([]), false),
        (json!({}), false),
    ];
    let mut outcomes = Vec::new();
    for (id, expected) in cases {
        let accepted = crate::document_guard::parse_expected_document(&json!({"expectedDocument":{"documentId":id,"revision":"0"}})).is_ok();
        assert_eq!(accepted, expected, "{id}");
        outcomes.push(json!({"value":id,"accepted":accepted}));
    }
    println!("DOCUMENT_ID_MCP_WAVE {}", json!(outcomes));
}

// Missing validation would send a mutation, parse expect, or allocate a checkpoint before refusal.
#[test]
fn guarded_mcp_schema_and_direct_tool_validate_before_effects() {
    let defs = tool_definitions();
    for name in ["execute", "batch"] {
        let schema = &defs.iter().find(|d| d["name"] == name).unwrap()["inputSchema"];
        let stamp = &schema["properties"]["expectedDocument"];
        assert_eq!(stamp["type"], "object");
        assert_eq!(stamp["additionalProperties"], false);
        assert_eq!(stamp["required"], json!(["documentId", "revision"]));
        assert_eq!(stamp["properties"]["revision"]["type"], "string");
        assert!(!schema["required"].as_array().unwrap().contains(&json!("expectedDocument")));
    }
    let mut bad = vec![Value::Null, json!(7), json!({}), json!({"documentId":"doc-A","revision":"0","extra":0})];
    for revision in [json!(0), json!(null), json!("01"), json!("18446744073709551616"), json!("-1"), json!("1e3"), json!(" 1")] {
        bad.push(json!({"documentId":"doc-A","revision":revision}));
    }
    for id in ["", "_bad", "x/y", "中文"] {
        bad.push(json!({"documentId":id,"revision":"0"}));
    }
    bad.push(json!({"documentId":"x".repeat(129),"revision":"0"}));
    for name in ["execute", "batch"] {
        for expected in &bad {
            let mut b = GuardToolBackend::default();
            let mut args = if name == "execute" { json!({"command":"parameters.list"}) } else { json!({"commands":[{"command":"parameters.list"}]}) };
            args["expectedDocument"] = expected.clone();
            args["expect"] = json!({"bogus":true});
            let reply = crate::call_tool(&mut b, name, &args);
            assert_eq!(reply["isError"], true, "{args}: {reply}");
            assert_eq!(reply["structuredContent"]["error"]["code"], "INVALID_REQUEST", "guard must precede expect: {reply}");
            assert!(b.calls.is_empty());
            let count = b.count.clone();
            let mut s = Server::new(Box::new(b));
            assert_eq!(raw_tool(&mut s, name, args)["error"]["code"], -32602);
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
    for step in [
        json!({"command":"parameters.list","expectedDocument":null}),
        json!({"command":"parameters.list","params":{"expectedDocument":null}}),
        json!({"command":"execute","params":{"command":"parameters.list","expectedDocument":{"documentId":"doc-A","revision":"0"}}}),
        json!({"command":"execute","params":{"command":"engine.batch","params":{"commands":[{"command":"parameters.list","expectedDocument":null}]}}}),
    ] {
        for guarded in [false, true] {
            let mut b = GuardToolBackend::default();
            let mut args = json!({"commands":[{"command":"parameters.list"},step]});
            if guarded {
                args["expectedDocument"] = guarded_headless_stamp(&mut b.headless);
            }
            let reply = crate::call_tool(&mut b, "batch", &args);
            assert_eq!(reply["structuredContent"]["error"]["code"], "INVALID_REQUEST", "{reply}");
            assert!(b.calls.is_empty());
            let count = b.count.clone();
            assert_eq!(raw_tool(&mut Server::new(Box::new(b)), "batch", args)["error"]["code"], -32602);
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
    let mut b = GuardToolBackend::default();
    let base = guarded_headless_stamp(&mut b.headless);
    let reply = crate::call_tool(&mut b, "execute", &json!({"command":null,"id":"PaRaMeTeRs.LiSt","expectedDocument":base}));
    assert_ne!(reply["isError"], true, "{reply}");
    assert_eq!(b.calls[0].2["command"], Value::Null);
    assert_eq!(b.calls[0].2["id"], "PaRaMeTeRs.LiSt");
    let reply = crate::call_tool(&mut b, "batch", &json!({"commands":[{"command":null,"id":"file.new"}],"expectedDocument":base}));
    let body = guarded_tool_body(&reply);
    assert_eq!(body["completed"], 0);
    assert!(body.get("failed").is_some());
    assert_eq!(body["failed"]["command"], Value::Null);
    assert_eq!(b.calls.len(), 2);
    assert!(b.calls[1].0);
    assert_eq!(b.calls[1].1, "engine.batch");
    assert_eq!(b.calls[1].2["commands"], json!([{"command":null,"id":"file.new"}]));
}

// A primitive must not lose its identity or its real stamp; compact only touches native results.
#[test]
fn guarded_mcp_wrapper_keeps_native_json_and_compact_metadata() {
    use solidcraft_engine::schema::ParamCheck;
    let stamp = json!({"documentId":"doc-A","revision":"9007199254740994"});
    for native in [json!({"value":7}), json!([1, "x"]), json!("text"), json!(42), Value::Null] {
        for verbose in [true, false] {
            let payload = native.clone();
            let peer = GuardPeer::new(move |_, req| {
                GuardAnswer::Reply(if req["method"] == "control.capabilities" {
                    guard_capabilities()
                } else {
                    json!({"ok":true,"result":payload,"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}})
                })
            });
            let mut b = peer.remote(ParamCheck::Off);
            let mut args = guard_request("engine.execute");
            args["verbose"] = json!(verbose);
            let reply = crate::call_tool(&mut b, "execute", &args);
            assert_ne!(reply["isError"], true, "{reply}");
            assert_eq!(guarded_tool_body(&reply), json!({"result":native,"documentStamp":stamp}));
            let wire = peer.finish(b);
            assert_eq!(wire.iter().filter(|(_, r)| r["method"] == "engine.execute").count(), 1);
        }
    }
    let peer = GuardPeer::new(|_, req| {
        GuardAnswer::Reply(if req["method"] == "control.capabilities" {
            guard_capabilities()
        } else {
            json!({"ok":true,"result":{"created_bodies":["new"],"bodies":["old","new"],"changed_bodies":["old"],"removed_bodies":[],"error":{"facts":"keep"}},"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}})
        })
    });
    let mut remote = peer.remote(ParamCheck::Off);
    let mut args = guard_request("engine.execute");
    args["verbose"] = json!(false);
    let body = guarded_tool_body(&crate::call_tool(&mut remote, "execute", &args));
    assert_eq!(
        body,
        json!({"result":{"created_bodies":["new"],"changed_bodies":["old"],"removed_bodies":[],"error":{"facts":"keep"}},"documentStamp":stamp})
    );
    assert_eq!(peer.finish(remote).len(), 2);
    let mut b = GuardToolBackend::default();
    let base = guarded_headless_stamp(&mut b.headless);
    let reply = crate::call_tool(&mut b, "batch", &json!({"expectedDocument":base,"commands":[{"command":"parameters.list"}],"verbose":false}));
    let body = guarded_tool_body(&reply);
    assert_eq!(body["completed"], 1);
    assert_eq!(body["documentStamp"], base);
    assert_eq!(b.calls.len(), 1);
    assert!(b.calls[0].0);
    b.calls.clear();
    let legacy = crate::call_tool(&mut b, "execute", &json!({"command":"parameters.list"}));
    let legacy: Value = serde_json::from_str(legacy["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(legacy.get("documentStamp").is_none());
    assert!(legacy.get("result").is_none());
    assert_eq!(b.calls.len(), 1);
    assert!(!b.calls[0].0);
    let base = guarded_headless_stamp(&mut b.headless);
    b.calls.clear();
    let reply = crate::call_tool(
        &mut b,
        "batch",
        &json!({"expectedDocument":base,"commands":[
        {"command":"parameters.change","params":{"name":"width","expression":"20 mm"}}],"expect":{"bodies":0}}),
    );
    let body = guarded_tool_body(&reply);
    assert_ne!(reply["isError"], true);
    assert_eq!(body["expectation"]["ok"], true);
    assert_eq!(body["completed"], 1);
    assert_eq!(b.calls.len(), 2);
    assert!(b.calls.iter().all(|(guarded, _, _)| *guarded));
    assert_eq!(b.calls[1].2["command"], "inspect.measure");
    assert_eq!(b.calls[1].2["expectedDocument"], body["documentStamp"]);
}

// Transport refusals cannot authorize a second send, single steps, or Undo::begin.
#[test]
fn guarded_mcp_batch_unavailable_or_oversized_never_falls_back() {
    use solidcraft_engine::schema::ParamCheck;
    for message in ["unknown method `engine.batch`", "engine.batch unavailable", "busy: not run"] {
        let peer = GuardPeer::new(move |_, req| {
            GuardAnswer::Reply(if req["method"] == "control.capabilities" { guard_capabilities() } else { json!({"ok":false,"error":message}) })
        });
        let mut b = peer.remote(ParamCheck::Off);
        let r = crate::call_tool(&mut b, "batch", &guard_request("engine.batch"));
        assert_eq!(r["isError"], true);
        assert_eq!(r["structuredContent"]["error"], message);
        let wire = peer.finish(b);
        assert_eq!(wire.iter().map(|(_, r)| r["method"].as_str().unwrap()).collect::<Vec<_>>(), ["control.capabilities", "engine.batch"]);
    }
    let mut b = GuardToolBackend::default();
    let base = guarded_headless_stamp(&mut b.headless);
    for commands in [
        json!([{"command":"parameters.list"},{"command":"file.new"}]),
        json!([{"command":"parameters.list","params":{"padding":"x".repeat(solidcraft_engine::MAX_LINE)}}]),
        json!(vec![json!({"command":"parameters.list"}); 10_001]),
    ] {
        let r = crate::call_tool(&mut b, "batch", &json!({"expectedDocument":base,"commands":commands}));
        assert_eq!(r["isError"], true);
        assert!(b.calls.is_empty(), "no backend/checkpoint calls");
    }
    let mut legacy = LayoutFallbackBackend::new();
    let r = crate::call_tool(&mut legacy, "batch", &guard_request("engine.batch"));
    assert_eq!(r["structuredContent"]["error"]["code"], "UNSUPPORTED_CAPABILITY");
    assert!(legacy.calls.is_empty());
}

// Raw stale and actual checkpoint/restore failures must survive pending and compact unchanged.
#[test]
fn guarded_mcp_pending_and_expectation_preserve_effect_facts() {
    use solidcraft_engine::schema::ParamCheck;
    let stale = json!({"code":"STALE_DOCUMENT","message":"stale","retry":"never","details":{"expected":{"documentId":"doc-A","revision":"9007199254740993"},"current":null}});
    for (method, original) in [
        ("engine.execute", json!({"ok":false,"error":"stale","errorDetails":stale})),
        ("engine.execute", json!({"ok":true,"result":7,"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}})),
        (
            "engine.execute",
            json!({"ok":true,"result":{"completed":1,"results":[null],"undone":0},"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}}),
        ),
        (
            "engine.batch",
            json!({"ok":true,"result":{"completed":0,"results":[],"undone":0,"rolled_back":false,"not_run":[0,1,2],"failed":{"index":null,"command":"edit.checkpoint","stage":"checkpoint","error":"blocked"}},"documentStamp":{"documentId":"doc-A","revision":"9007199254740993"}}),
        ),
        (
            "engine.batch",
            json!({"ok":true,"result":{"completed":1,"results":[{"value":20}],"undone":1,"rolled_back":true,"not_run":[2],"failed":{"index":1,"command":"parameters.change","error":"bad"}},"documentStamp":{"documentId":"doc-A","revision":"9007199254740995"}}),
        ),
        (
            "engine.batch",
            json!({"ok":true,"result":{"completed":1,"results":[{"value":20}],"undone":0,"rolled_back":false,"not_run":[2],"failed":{"index":1,"command":"parameters.change","error":"restore also failed"}},"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}}),
        ),
        (
            "engine.batch",
            json!({"ok":true,"result":{"completed":1,"results":[{"value":20}],"undone":0,"rolled_back":false,"failed":{"index":1,"command":"parameters.change","error":"bad"}},"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}}),
        ),
    ] {
        for deferred in [false, true] {
            let response = original.clone();
            let mut polls = 0;
            let peer = GuardPeer::new(move |_, req| {
                GuardAnswer::Reply(match req["method"].as_str() {
                    Some("control.capabilities") => guard_capabilities(),
                    Some("control.pending") => {
                        polls += 1;
                        if polls == 1 {
                            json!({"ok":true,"result":{"pending":1,"done":false}})
                        } else {
                            json!({"ok":true,"result":{"pending":1,"done":true,"reply":response}})
                        }
                    }
                    _ if deferred => running(),
                    _ => response.clone(),
                })
            });
            let mut b = peer.remote(ParamCheck::Off);
            let mut args = guard_request(method);
            args["verbose"] = json!(false);
            if method == "engine.batch" {
                args["commands"] = json!([{"command":"parameters.list"},{"command":"parameters.list"},{"command":"parameters.list"}]);
            }
            let initial = crate::call_tool(&mut b, if method == "engine.batch" { "batch" } else { "execute" }, &args);
            let result = if deferred {
                assert_eq!(initial["isError"], true);
                crate::call_tool(&mut b, "pending", &json!({}))
            } else {
                initial
            };
            let body = guarded_tool_body(&result);
            let body = if deferred { &body["result"] } else { &body };
            if original["ok"] == false {
                assert_eq!(result["isError"], true);
                assert_eq!(body["error"], stale);
            } else {
                assert_eq!(body["documentStamp"], original["documentStamp"]);
                if method == "engine.execute" {
                    assert_eq!(body["result"], original["result"], "execute stays wrapped even when its native object resembles a batch");
                } else {
                    for k in ["completed", "results", "rolled_back", "undone", "not_run", "failed"] {
                        if k == "not_run" && original["result"].get(k).is_none() {
                            assert_eq!(body[k], json!([2]), "use the original three steps, not the one returned result");
                        } else {
                            assert_eq!(body[k], original["result"][k], "{k}: {body}");
                        }
                    }
                    assert_eq!(result["isError"], true);
                }
            }
            let wire = peer.finish(b);
            assert_eq!(wire.iter().filter(|(_, r)| r["method"] == method).count(), 1);
            assert!(wire.iter().all(|(_, r)| r["method"] != "engine.execute" || method == "engine.execute"));
        }
    }
    let mut b = GuardToolBackend::default();
    b.headless.session.execute("parameters.change", &json!({"name":"width","expression":"10 mm"})).unwrap();
    let base = guarded_headless_stamp(&mut b.headless);
    let doc = b.headless.session.doc.clone();
    let undo = b.headless.session.undo.len();
    let trace = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = trace.clone();
    b.headless.session.set_command_check(Some(std::sync::Arc::new(move |id, _, _| {
        seen.lock().unwrap().push(id.to_string());
        if id == "edit.checkpoint" { Err("checkpoint blocked".into()) } else { Ok(()) }
    })));
    let r = crate::call_tool(
        &mut b,
        "batch",
        &json!({"expectedDocument":base,"verbose":false,"expect":{"bodies":0},"commands":[
        {"command":"parameters.change","params":{"name":"width","expression":"20 mm"}},
        {"command":"parameters.list"}]}),
    );
    let body = guarded_tool_body(&r);
    assert_eq!(r["isError"], true);
    assert_eq!(body["failed"]["index"], Value::Null);
    assert_eq!(body["failed"]["stage"], "checkpoint");
    assert_eq!(body["completed"], 0);
    assert_eq!(body["not_run"], json!([0, 1]));
    assert_eq!(body["rolled_back"], false);
    assert_eq!(body["undone"], 0);
    assert_eq!(body["documentStamp"], base);
    assert!(body.get("expectation").is_none());
    assert!(std::sync::Arc::ptr_eq(&doc, &b.headless.session.doc));
    assert_eq!(b.headless.session.undo.len(), undo);
    assert_eq!(*trace.lock().unwrap(), ["edit.checkpoint"]);
    assert_eq!(b.calls.len(), 1);
    assert!(b.calls[0].0);
}

// Changing the source before measurement must not certify another state or erase the write.
#[test]
fn guarded_expectation_uses_applied_stamp_and_never_certifies_other_state() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    use std::sync::{Arc, Mutex, mpsc::channel};
    for (race, expected_bodies, expected_ok) in [(false, 0, true), (false, 1, false), (true, 0, false)] {
        let mut headless = Headless::default();
        let base = guarded_headless_stamp(&mut headless);
        let state = Arc::new(Mutex::new(headless));
        let owner = state.clone();
        let (written_tx, written_rx) = channel();
        let (release_tx, release_rx) = channel();
        let peer = GuardPeer::new(move |_, req| {
            if req["method"] == "control.capabilities" {
                return GuardAnswer::Reply(guard_capabilities());
            }
            let is_read = req["params"]["command"] == "inspect.measure";
            if is_read {
                written_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            // Let the legacy route really execute too: RED must show the missing guarded
            // measurement/stamp, not a peer panic or a barrier timeout.
            if req["params"].get("expectedDocument").is_none() {
                let reply = owner.lock().unwrap().call("engine.execute", req["params"].clone());
                return GuardAnswer::Reply(match reply {
                    Ok(result) => json!({"ok":true,"result":result}),
                    Err(error) => json!({"ok":false,"error":error}),
                });
            }
            let reply = owner.lock().unwrap().call_guarded("engine.execute", req["params"].clone());
            GuardAnswer::Reply(match reply {
                Ok(r) => {
                    json!({"ok":true,"result":r.result,"documentStamp":{"documentId":r.document_stamp.document_id,"revision":r.document_stamp.revision.to_string()}})
                }
                Err(crate::GuardedCallError::Rejected { message, error_details }) => json!({"ok":false,"error":message,"errorDetails":error_details}),
                other => panic!("{other:?}"),
            })
        });
        let b = peer.remote(ParamCheck::Off);
        let worker = std::thread::spawn(move || {
            let mut b = b;
            let r = crate::call_tool(
                &mut b,
                "execute",
                &json!({"command":"parameters.change","params":{"name":"width","expression":"20 mm"},"expectedDocument":base,"expect":{"bodies":expected_bodies}}),
            );
            (b, r)
        });
        written_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        let applied = guarded_headless_stamp(&mut state.lock().unwrap());
        if race {
            state.lock().unwrap().session.execute("parameters.change", &json!({"name":"width","expression":"30 mm"})).unwrap();
        }
        release_tx.send(()).unwrap();
        let (b, r) = worker.join().unwrap();
        let wire = peer.finish(b);
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[2].1["params"]["expectedDocument"], applied, "measurement must be guarded by the actual write stamp");
        let body = guarded_tool_body(&r);
        assert_eq!(r["isError"] == true, !expected_ok, "{r}");
        assert_eq!(body["documentStamp"], applied);
        assert_eq!(body["result"]["value"], 20.0);
        assert_eq!(body["expectation"]["ok"], expected_ok);
        assert!(body.get("error").is_none());
        if race {
            assert_eq!(body["expectation"]["error"]["code"], "STALE_DOCUMENT");
            assert_eq!(state.lock().unwrap().session.doc.param("width").unwrap().expr, "30 mm");
        } else {
            assert_eq!(state.lock().unwrap().session.doc.param("width").unwrap().expr, "20 mm");
        }
    }
    // A protocol peer returning a different read stamp cannot certify even matching numbers.
    let peer = GuardPeer::new(|_, req| {
        GuardAnswer::Reply(if req["method"] == "control.capabilities" {
            guard_capabilities()
        } else if req["params"]["command"] == "inspect.measure" {
            json!({"ok":true,"result":{"body_count":0,"bodies":[],"total":{"volume_mm3":0}},"documentStamp":{"documentId":"doc-A","revision":"9007199254740995"}})
        } else {
            json!({"ok":true,"result":{"value":20},"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}})
        })
    });
    let mut b = peer.remote(ParamCheck::Off);
    let mut args = guard_request("engine.execute");
    args["expect"] = json!({"bodies":0});
    let r = crate::call_tool(&mut b, "execute", &args);
    let body = guarded_tool_body(&r);
    assert_eq!(r["isError"], true);
    assert_eq!(body["result"]["value"], 20);
    assert_eq!(body["documentStamp"]["revision"], "9007199254740994");
    assert_eq!(body["expectation"]["ok"], false);
    assert_eq!(body["expectation"]["verified"], false);
    assert!(body["expectation"].get("measured").is_none());
    assert_eq!(peer.finish(b).len(), 3);
    // A delayed write is returned as written, never measured against whatever is current later.
    let mut polls = 0;
    let peer = GuardPeer::new(move |_, req| {
        GuardAnswer::Reply(match req["method"].as_str() {
            Some("control.capabilities") => guard_capabilities(),
            Some("control.pending") => {
                polls += 1;
                if polls == 1 {
                    json!({"ok":true,"result":{"pending":1,"done":false}})
                } else {
                    json!({"ok":true,"result":{"pending":1,"done":true,"reply":{"ok":true,"result":5,"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}}}})
                }
            }
            _ => running(),
        })
    });
    let mut b = peer.remote(ParamCheck::Off);
    let mut args = guard_request("engine.execute");
    args["expect"] = json!({"bodies":0});
    let initial = crate::call_tool(&mut b, "execute", &args);
    assert_eq!(initial["isError"], true);
    let initial = guarded_tool_body(&initial);
    assert_eq!(initial["expectation"], Value::Null);
    assert!(initial["note"].as_str().unwrap().contains("not executed"));
    let r = crate::call_tool(&mut b, "pending", &json!({}));
    let body = guarded_tool_body(&r);
    assert_eq!(body["result"]["result"], 5);
    assert_eq!(body["expectation"], Value::Null);
    assert!(body["note"].as_str().unwrap().contains("not executed"));
    let wire = peer.finish(b);
    assert_eq!(wire.iter().filter(|(_, r)| r["method"] == "engine.execute").count(), 1);
}

// Catches legacy fallback, per-step stale checks and invented result stamps: all writes run
// through the real Headless Backend entry and real Session commands, without a protocol peer.
#[test]
fn guarded_headless_uses_same_native_core_without_fallback() {
    use crate::Backend;
    let mut h = Headless::default();
    h.session.execute("parameters.add", &json!({"name":"width","expression":"10 mm"})).unwrap();
    let base = guarded_headless_stamp(&mut h);
    let trace = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = trace.clone();
    h.session.set_command_check(Some(std::sync::Arc::new(move |id, p, _| {
        observed.lock().unwrap().push((id.to_string(), p.clone()));
        Ok(())
    })));
    let reply = h
        .call_guarded(
            "engine.execute",
            json!({"command":null,"id":"PARAMETERS.CHANGE","params":{"name":"width","expression":"20 mm"},"expectedDocument":base}),
        )
        .unwrap();
    assert_eq!(reply.result["value"], 20.0);
    assert_eq!(reply.document_stamp, h.session.document_stamp());
    assert_ne!(guarded_headless_stamp(&mut h), base);
    let current = guarded_headless_stamp(&mut h);
    trace.lock().unwrap().clear();
    let stale = h.call_guarded("engine.batch", json!({"commands":[{"command":"parameters.list"}],"expectedDocument":base})).unwrap_err();
    let crate::GuardedCallError::Rejected { error_details, .. } = stale else { panic!("not a stale refusal: {stale:?}") };
    assert_eq!(error_details["code"], "STALE_DOCUMENT");
    assert_eq!(error_details["retry"], "never");
    assert_eq!(error_details["details"], json!({"expected":base,"current":current}));
    assert!(trace.lock().unwrap().is_empty());
    let steps = vec![
        json!({"command":"parameters.change","params":{"name":"width","expression":"30 mm"}}),
        json!({"command":"parameters.change","params":{"name":"width","expression":"40 mm"}}),
    ];
    let mut native = h.session.scratch();
    let native_stamp = native.document_stamp();
    let expected = native.batch_if_current(&steps, &json!({}), &native_stamp).unwrap().0;
    let reply = h.call_guarded("engine.batch", json!({"commands":steps,"expectedDocument":current})).unwrap();
    assert_eq!(reply.result, expected);
    assert_eq!(reply.result["completed"], 2);
    assert_eq!(reply.document_stamp, h.session.document_stamp());
    assert_eq!(h.session.doc.param("width").unwrap().expr, "40 mm");
    let trace = trace.lock().unwrap();
    // scratch() carries the host check too: first native, then Headless, each one checkpoint.
    assert_eq!(trace.iter().filter(|(id, _)| id == "edit.checkpoint").count(), 2);
    assert_eq!(trace.iter().filter(|(id, _)| id == "parameters.change").count(), 4);
    assert_eq!(trace.last().unwrap(), &("edit.restore_checkpoint".into(), json!({"checkpoint":1,"forget":true})));
}

#[test]
fn guarded_headless_refuses_all_unsupported_steps_before_history_and_restores_param_check() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    let mut h = Headless::default();
    h.session.param_check = ParamCheck::Warn;
    let base = guarded_headless_stamp(&mut h);
    let doc = h.session.doc.clone();
    let trace = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = trace.clone();
    h.session.set_command_check(Some(std::sync::Arc::new(move |id, _, _| {
        observed.lock().unwrap().push(id.to_string());
        Ok(())
    })));
    for last in [
        json!({"command":"DOC.OPEN","params":{"path":"never-opened.sc"}}),
        json!({"command":"panel.list"}),
        json!({"command":"parameters.change","params":{"name":"width","expression":"20","path":"forbidden"}}),
    ] {
        let request = json!({"expectedDocument":base,"param_check":"strict","commands":[
            {"command":"parameters.add","params":{"name":"width","expression":"10 mm"}},last]});
        assert!(
            matches!(h.call_guarded("engine.batch",request),Err(crate::GuardedCallError::Rejected{error_details,..}) if error_details["code"]=="UNSUPPORTED_CAPABILITY")
        );
        assert!(trace.lock().unwrap().is_empty());
        assert!(std::sync::Arc::ptr_eq(&doc, &h.session.doc));
        assert_eq!(h.session.undo.len(), 0);
        assert_eq!(h.session.param_check, ParamCheck::Warn);
    }
    for method in ["engine.execute", "engine.batch"] {
        let step = json!({"command":"solid.box","params":{"lenght":10,"width":10,"height":10}});
        let mut request = if method == "engine.execute" { step.clone() } else { json!({"commands":[{"command":"parameters.list"},step]}) };
        request["expectedDocument"] = base.clone();
        request["param_check"] = json!("strict");
        match h.call_guarded(method, request) {
            Err(crate::GuardedCallError::CommandFailed(message)) if method == "engine.execute" => assert!(message.contains("lenght")),
            Ok(reply) if method == "engine.batch" => {
                assert_eq!(reply.result["completed"], 0);
                assert_eq!(reply.result["failed"]["index"], 1);
                assert_eq!(reply.result["not_run"], json!([0, 1]));
            }
            other => panic!("{method}: {other:?}"),
        }
        assert_eq!(h.session.param_check, ParamCheck::Warn);
        assert!(std::sync::Arc::ptr_eq(&doc, &h.session.doc));
    }
    trace.lock().unwrap().clear();
    for (method, request) in [
        ("engine.execute", json!({"command":"file.new","expectedDocument":base})),
        ("engine.execute", json!({"command":"parameters.list","expectedDocument":null})),
        ("engine.execute", json!({"command":"parameters.list","expectedDocument":base,"param_check":"loud"})),
        ("engine.batch", json!({"commands":vec![json!({"command":"parameters.list"});10_001],"expectedDocument":base})),
        ("engine.batch", json!({"commands":null,"expectedDocument":base})),
    ] {
        assert!(h.call_guarded(method, request).is_err());
        assert_eq!(h.session.param_check, ParamCheck::Warn);
        assert!(trace.lock().unwrap().is_empty());
    }
    // Batch keeps first-Value semantics; execute's string fallback above must not leak here.
    let reply = h.call_guarded("engine.batch", json!({"expectedDocument":base,"commands":[{"command":null,"id":"parameters.list"}]})).unwrap();
    assert_eq!(reply.result["completed"], 0);
    assert_eq!(reply.result["failed"]["command"], Value::Null);
    assert!(trace.lock().unwrap().is_empty());
    assert_eq!(h.session.execute("edit.checkpoint", &json!({})).unwrap()["checkpoint"], 1, "all refusals consumed zero checkpoint ids");
}

#[test]
fn guarded_headless_checkpoint_policy_and_native_failure_bodies() {
    use crate::Backend;
    for rollback in [None, Some(true), Some(false)] {
        let mut h = Headless::default();
        h.session.execute("parameters.add", &json!({"name":"width","expression":"10 mm"})).unwrap();
        let base = guarded_headless_stamp(&mut h);
        let doc = h.session.doc.clone();
        let undo = h.session.undo.len();
        let trace = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = trace.clone();
        h.session.set_command_check(Some(std::sync::Arc::new(move |id, _, _| {
            observed.lock().unwrap().push(id.to_string());
            if id == "edit.checkpoint" { Err("checkpoint refused".into()) } else { Ok(()) }
        })));
        let mut request = json!({"expectedDocument":base,"coalesce_parameters":true,"commands":[
            {"command":"parameters.change","params":{"name":"width","expression":"20 mm"}},
            {"command":"parameters.change","params":{"name":"width","expression":"1 +"}}]});
        if let Some(rollback) = rollback {
            request["rollback"] = json!(rollback);
        }
        let reply = h.call_guarded("engine.batch", request).unwrap();
        assert_eq!(reply.result["rolled_back"], false);
        assert_eq!(reply.result["undone"], 0);
        if rollback != Some(false) {
            assert_eq!(reply.result["completed"], 0);
            assert_eq!(reply.result["not_run"], json!([0, 1]));
            assert_eq!(reply.result["failed"]["stage"], "checkpoint");
            assert_eq!(*trace.lock().unwrap(), ["edit.checkpoint"]);
            assert!(std::sync::Arc::ptr_eq(&doc, &h.session.doc));
            assert_eq!(h.session.undo.len(), undo);
        } else {
            assert_eq!(reply.result["completed"], 1);
            assert_eq!(h.session.doc.param("width").unwrap().expr, "20 mm");
            assert!(trace.lock().unwrap().iter().all(|id| id != "edit.checkpoint"));
        }
        assert_eq!(reply.document_stamp, h.session.document_stamp());
    }
}

fn server() -> Server {
    Server::new(Box::new(Headless::default()))
}

fn panel_headless() -> Headless {
    Headless::new(solidcraft_engine::Session::default()).with_panels(solidcraft_engine::panels::fixture_command_backend().unwrap())
}
fn panel_mcp_steps() -> Vec<Value> {
    vec![
        json!({"command":"panel.open","params":{"kind":"fixture.log","instance":"log","destination":{"type":"new_section","section":"logs","index":0,
        "location":{"type":"dock","zone":"left"},"size":{"width":280,"height":240}}}}),
        json!({"command":"panel.collapse","params":{"target":{"type":"instance","id":"log"},"collapsed":true}}),
    ]
}
#[test]
fn headless_panel_single_batch_and_script_keep_one_backend_and_never_change_the_session() {
    use crate::Backend;
    let mut one = panel_headless();
    let mut batch = panel_headless();
    let mut script = panel_headless();
    let doc = one.session.doc.clone();
    let model = one.session.model.state();
    let before = (one.session.document_id.clone(), one.session.revision, one.session.undo.len(), one.session.is_dirty(), one.session.camera);
    for step in panel_mcp_steps() {
        one.call("engine.execute", step).unwrap();
    }
    batch.call("engine.batch", json!({"commands":panel_mcp_steps()})).unwrap();
    script.call("engine.script", json!({"commands":panel_mcp_steps()})).unwrap();
    let state = one.call("engine.execute", json!({"command":"layout.get"})).unwrap();
    assert_eq!(batch.call("engine.execute", json!({"command":"layout.get"})).unwrap()["state"]["layout"], state["state"]["layout"]);
    assert_eq!(script.call("engine.execute", json!({"command":"layout.get"})).unwrap()["state"]["layout"], state["state"]["layout"]);
    assert_eq!(state["state"]["capabilities"]["native_windows"], false);
    assert_eq!((one.session.document_id.clone(), one.session.revision, one.session.undo.len(), one.session.is_dirty(), one.session.camera), before);
    assert!(std::sync::Arc::ptr_eq(&doc, &one.session.doc));
    assert!(std::sync::Arc::ptr_eq(&model, &one.session.model.state()));
    one.call("engine.script", json!({"commands":[{"command":"layout.save","params":{"name":"MCP saved"}},{"command":"layout.reset"}]})).unwrap();
    one.call("engine.execute", json!({"command":"layout.apply","params":{"name":"MCP saved"}})).unwrap();
    assert_eq!(one.call("engine.execute", json!({"command":"layout.get"})).unwrap()["state"]["layout"], state["state"]["layout"]);
}
#[test]
fn mcp_panel_tools_publish_structured_errors_and_committed_state_through_shared_backend() {
    let mut s = Server::new(Box::new(panel_headless()));
    let input = panel_mcp_steps()[0].clone();
    let result = tool(&mut s, "execute", input);
    assert_eq!(result["layout"]["instances"][0]["id"], "log");
    let result = tool(
        &mut s,
        "batch",
        json!({"commands":[
        {"command":"panel.collapse","params":{"target":{"type":"instance","id":"log"},"collapsed":true}},
        {"command":"panel.peek","params":{"target":{"type":"instance","id":"log"}}}]}),
    );
    assert_eq!(result["completed"], 2);
    let result = raw_tool(&mut s, "execute", json!({"command":"panel.focus","params":{"id":"fixture.log"}}));
    assert_eq!(result["result"]["isError"], true);
    assert_eq!(result["result"]["structuredContent"]["panel_error"]["rule"], "exactly_once");
    let result = raw_tool(&mut s, "execute", json!({"command":"layout.get","params":{"unknown":true}}));
    assert_eq!(result["result"]["structuredContent"]["panel_error"]["rule"], "schema");
}
#[test]
fn headless_catalogue_atomic_batch_failure_keeps_presets_and_layout_and_session_only_is_truthful() {
    use crate::Backend;
    use solidcraft_engine::panels::{ActionSink, commands::LayoutCatalogue};
    let mut h = panel_headless();
    let state = h.panels.as_ref().unwrap().service.inspect();
    let catalogue = h.panels.as_ref().unwrap().catalogue.state().clone();
    let error = h
        .call(
            "engine.batch",
            json!({"commands":[
        {"command":"layout.save","params":{"name":"Not committed"}},
        {"command":"layout.workspace.select","params":{"workspace":"not-registered"}}]}),
        )
        .unwrap_err();
    assert_eq!(solidcraft_engine::panels::commands::structured_error(&error).unwrap()["rule"], "missing_target");
    assert_eq!(h.panels.as_ref().unwrap().service.inspect(), state);
    assert_eq!(h.panels.as_ref().unwrap().catalogue.state(), &catalogue);
    let mut plain = Headless::default();
    let got = plain.call("engine.execute", json!({"command":"panel.list"})).unwrap();
    assert!(got["kinds"].as_array().unwrap().is_empty());
    assert_eq!(got["state"]["capabilities"]["native_windows"], false);
    assert!(plain.call("engine.execute", json!({"command":"layout.reset"})).is_err());
    assert!(solidcraft_engine::Session::default().run_script(&json!({"commands":[{"command":"layout.get"}]})).is_err());
}

#[test]
fn auto_backend_keeps_headless_layout_work_even_without_document_features() {
    use crate::Backend;
    use std::cell::Cell;
    use std::rc::Rc;
    let mut headless = panel_headless();
    headless.call("engine.execute", json!({"command":"layout.save","params":{"name":"Owned headless layout"}})).unwrap();
    let discoveries = Rc::new(Cell::new(0));
    let seen = discoveries.clone();
    let mut auto = crate::Auto::new(
        headless,
        Box::new(move || {
            seen.set(seen.get() + 1);
            None
        }),
        None,
    );
    assert_eq!(auto.call("engine.execute", json!({"command":"layout.get"})).unwrap()["state"]["layout"]["workspace"], "fixture.workspace");
    assert_eq!(discoveries.get(), 0, "owned layout must not be discarded while probing for an app");
}

#[test]
fn remote_panel_single_batch_and_script_forward_to_one_authoritative_backend() {
    use crate::Backend;
    let authoritative = std::sync::Arc::new(std::sync::Mutex::new(solidcraft_engine::panels::fixture_command_backend().unwrap()));
    let state = authoritative.clone();
    // This is an authored local transport fixture, never run during implementation.
    let (addr, seen) = scripted_app(move |request| {
        let mut backend = state.lock().unwrap();
        let params = &request["params"];
        let result = match request["method"].as_str() {
            Some("engine.execute") => backend.execute(params["command"].as_str().unwrap(), params.get("params").unwrap_or(&json!({}))),
            Some("engine.batch") => backend.batch(params["commands"].as_array().unwrap()),
            Some("engine.script") => {
                let mut replies = Vec::new();
                for step in params["commands"].as_array().unwrap() {
                    replies.push(backend.execute(step["command"].as_str().unwrap(), step.get("params").unwrap_or(&json!({}))).unwrap());
                }
                Ok(json!(replies))
            }
            _ => return json!({"ok":false,"error":"unknown method"}),
        };
        match result {
            Ok(result) => json!({"ok":true,"result":result}),
            Err(reason) => json!({"ok":false,"error":solidcraft_engine::panels::commands::error_text(reason)}),
        }
    });
    let mut remote = crate::Remote::connect_with_token(&addr, Some("0123456789abcdef0123456789abcdef")).unwrap();
    remote.call("engine.execute", panel_mcp_steps()[0].clone()).unwrap();
    remote
        .call(
            "engine.batch",
            json!({"commands":[panel_mcp_steps()[1].clone(),{"command":"panel.peek","params":{"target":{"type":"instance","id":"log"}}}]}),
        )
        .unwrap();
    authoritative.lock().unwrap().execute("panel.collapse", &json!({"target":{"type":"instance","id":"log"},"collapsed":false})).unwrap();
    assert_eq!(
        remote.call("engine.execute", json!({"command":"layout.get"})).unwrap()["state"]["layout"]["sections"][0]["collapsed"],
        false,
        "remote reads external live changes"
    );
    remote
        .call("engine.script", json!({"commands":[{"command":"layout.save","params":{"name":"Remote saved"}},{"command":"layout.reset"}]}))
        .unwrap();
    assert!(remote.call("engine.execute", json!({"command":"layout.get"})).unwrap()["state"]["layout"]["instances"].as_array().unwrap().is_empty());
    let seen = seen.lock().unwrap();
    assert!(seen.1.iter().any(|method| method == "engine.batch"));
    assert!(seen.1.iter().any(|method| method == "engine.script"));
}

#[test]
fn headless_null_params_match_all_panel_routes_and_normal_document_sequences() {
    use crate::Backend;
    let mut headless = panel_headless();
    let expected = headless.call("engine.execute", json!({"command":"panel.list","params":null})).unwrap();
    for (method, rollback) in [("engine.batch", true), ("engine.batch", false), ("engine.script", false)] {
        let reply = headless
            .call(
                method,
                json!({"rollback":rollback,"commands":[
                    {"command":"panel.list","params":null},{"command":"layout.get","params":null},
                ]}),
            )
            .unwrap();
        let results = if method == "engine.script" { &reply } else { &reply["results"] };
        assert_eq!(results[0], expected);
        assert_eq!(results[1]["state"], expected["state"]);
    }
    for method in ["engine.batch", "engine.script"] {
        let previous = headless.session.document_id.clone();
        let reply = headless
            .call(
                method,
                json!({"rollback":false,"commands":[
                    {"command":"file.new","params":null},{"command":"panel.list","params":null},{"command":"parameters.list","params":null},
                ]}),
            )
            .unwrap();
        assert!(reply.get("failed").is_none(), "{reply}");
        assert_ne!(headless.session.document_id, previous);
    }
}

struct LayoutFallbackBackend {
    headless: Headless,
    calls: Vec<(String, String)>,
}
impl LayoutFallbackBackend {
    fn new() -> Self {
        Self { headless: panel_headless(), calls: Vec::new() }
    }
}
impl crate::Backend for LayoutFallbackBackend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.calls.push((method.into(), params.get("command").and_then(Value::as_str).unwrap_or_default().into()));
        if method == "engine.batch" {
            return Err("engine.batch unavailable: fixture has no atomic endpoint".into());
        }
        crate::Backend::call(&mut self.headless, method, params)
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn in_process(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "synthetic layout fallback boundary".into()
    }
}
fn fallback_tool_reply(backend: &mut LayoutFallbackBackend, args: Value) -> (Value, Value) {
    let tool = crate::tools::call_tool(backend, "batch", &args);
    let body = serde_json::from_str(tool["content"][0]["text"].as_str().unwrap()).unwrap();
    (tool, body)
}

#[test]
fn atomic_tool_layout_batches_never_emulate_an_unavailable_backend_with_document_undo() {
    use solidcraft_engine::panels::{ActionSink, commands::LayoutCatalogue};
    let open = panel_mcp_steps()[0].clone();
    for steps in [
        json!([open.clone()]),
        json!([open.clone(),{"command":"parameters.change","params":{"name":"broken","expression":"("}}]),
        json!([{"command":"execute","params":{"command":"panel.open","params":open["params"]}}]),
    ] {
        let mut backend = LayoutFallbackBackend::new();
        let before = backend.headless.panels.as_ref().unwrap().service.inspect();
        let catalogue = backend.headless.panels.as_ref().unwrap().catalogue.state().clone();
        let document = backend.headless.session.doc.clone();
        let model = backend.headless.session.model.state();
        let session = (backend.headless.session.revision, backend.headless.session.undo.len(), backend.headless.session.is_dirty());
        let (tool, body) = fallback_tool_reply(&mut backend, json!({"commands":steps}));
        assert_eq!(tool["isError"], true);
        assert_eq!(tool["structuredContent"]["panel_error"]["rule"], "capability");
        assert_eq!(body["completed"], 0);
        assert_eq!(body["applied"], json!([]));
        assert_eq!(body["rolled_back"], false);
        assert_eq!(backend.calls, vec![("engine.batch".into(), String::new())], "no checkpoint, inspection or single layout call");
        assert_eq!(backend.headless.panels.as_ref().unwrap().service.inspect(), before);
        assert_eq!(backend.headless.panels.as_ref().unwrap().catalogue.state(), &catalogue);
        assert!(std::sync::Arc::ptr_eq(&document, &backend.headless.session.doc));
        assert!(std::sync::Arc::ptr_eq(&model, &backend.headless.session.model.state()));
        assert_eq!((backend.headless.session.revision, backend.headless.session.undo.len(), backend.headless.session.is_dirty()), session);
    }
}

#[test]
fn oversized_atomic_tool_layout_batch_refuses_before_any_backend_or_checkpoint_call() {
    use solidcraft_engine::panels::{ActionSink, commands::LayoutCatalogue};
    let mut backend = LayoutFallbackBackend::new();
    let before = backend.headless.panels.as_ref().unwrap().service.inspect();
    let catalogue = backend.headless.panels.as_ref().unwrap().catalogue.state().clone();
    let document = backend.headless.session.doc.clone();
    let model = backend.headless.session.model.state();
    let args = json!({"commands":[panel_mcp_steps()[0],{"command":"parameters.change","params":{"name":"broken","expression":"x".repeat(solidcraft_engine::MAX_LINE-32768)}}]});
    assert!(args.to_string().len() < solidcraft_engine::MAX_LINE, "fixture fits the incoming line but exceeds the outgoing margin");
    let (tool, body) = fallback_tool_reply(&mut backend, args);
    assert_eq!(tool["isError"], true);
    assert_eq!(tool["structuredContent"]["panel_error"]["rule"], "collection_limit");
    assert_eq!(body["completed"], 0);
    assert_eq!(body["applied"], json!([]));
    assert!(backend.calls.is_empty());
    assert_eq!(backend.headless.panels.as_ref().unwrap().service.inspect(), before);
    assert_eq!(backend.headless.panels.as_ref().unwrap().catalogue.state(), &catalogue);
    assert!(std::sync::Arc::ptr_eq(&document, &backend.headless.session.doc));
    assert!(std::sync::Arc::ptr_eq(&model, &backend.headless.session.model.state()));
}

#[test]
fn sequential_tool_layout_fallback_truthfully_retains_completed_changes_without_checkpointing() {
    use solidcraft_engine::panels::{ActionSink, InstanceId, commands::LayoutCatalogue};
    let mut backend = LayoutFallbackBackend::new();
    let (_, body) = fallback_tool_reply(
        &mut backend,
        json!({"rollback":false,"commands":[
            panel_mcp_steps()[0],{"command":"parameters.change","params":{"name":"broken","expression":"("}},
        ]}),
    );
    assert_eq!(body["ok"], false);
    assert_eq!(body["completed"], 1);
    assert_eq!(body["applied"], json!([0]));
    assert_eq!(body["rolled_back"], false);
    assert_eq!(body["undone"], 0);
    let panels = backend.headless.panels.as_ref().unwrap();
    assert!(panels.service.layout().instance(&InstanceId::new("log").unwrap()).unwrap().requested_open);
    assert_eq!(panels.service.inspect().revision, 1);
    assert!(panels.catalogue.state().remembered(&panels.service.layout().workspace).is_some());
    assert!(!backend.calls.iter().any(|(_, command)| command == "edit.checkpoint" || command == "edit.restore_checkpoint"));
    assert_eq!(backend.calls.iter().filter(|(_, command)| command == "panel.open").count(), 1);
}

#[test]
fn document_only_atomic_tool_fallback_keeps_existing_checkpoint_rollback_behavior() {
    let mut backend = LayoutFallbackBackend::new();
    let (_, body) = fallback_tool_reply(
        &mut backend,
        json!({"commands":[
            {"command":"parameters.change","params":{"name":"temporary","expression":1}},
            {"command":"parameters.change","params":{"name":"broken","expression":"("}},
        ]}),
    );
    assert_eq!(body["rolled_back"], true, "{body}");
    assert_eq!(body["applied"], json!([]));
    assert!(backend.headless.session.doc.param("temporary").is_none());
    assert!(backend.calls.iter().any(|(_, command)| command == "edit.checkpoint"));
    assert!(backend.calls.iter().any(|(_, command)| command == "edit.restore_checkpoint"));
}

/// Screenshots are drawn on the GPU by the renderer the host installs (`solidcraft-cli mcp`); the
/// tests install the same one. `false`, with the reason printed, on a machine without a GPU
/// adapter: the screenshots are then left out of the test.
fn renders() -> bool {
    match solidcraft_gpu::capture::shared() {
        Ok(c) => {
            solidcraft_engine::capture::install(std::sync::Arc::new(move |scene, cam, w, h| {
                c.rgba(scene, cam, w, h).map(|rgba| solidcraft_engine::capture::Picture { rgba, renderer: c.renderer() }).map_err(|e| e.to_string())
            }));
            true
        }
        Err(e @ solidcraft_gpu::GpuError::NoAdapter(_)) if std::env::var_os("SOLIDCRAFT_REQUIRE_GPU").is_none() => {
            eprintln!("screenshots left out: {e}");
            false
        }
        Err(e) => panic!("{e}"),
    }
}

fn call(s: &mut Server, id: u64, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let r = s.handle_line(&line).expect("reply");
    serde_json::from_str(&r).unwrap()
}

fn raw_tool(s: &mut Server, name: &str, args: Value) -> Value {
    call(s, 99, "tools/call", json!({"name": name, "arguments": args}))
}

/// Call a tool that must succeed; returns its JSON text (or the first content block).
fn tool(s: &mut Server, name: &str, args: Value) -> Value {
    let r = raw_tool(s, name, args);
    let c = &r["result"]["content"][0];
    assert_ne!(r["result"]["isError"], json!(true), "{name} failed: {c}");
    assert!(r.get("error").is_none(), "{name}: {r}");
    if c["type"] == "text" { serde_json::from_str(c["text"].as_str().unwrap()).unwrap_or(Value::Null) } else { c.clone() }
}

/// A large whitespace prefix must not evade the raw 16 MiB JSON-RPC line limit.
#[test]
fn whitespace_does_not_bypass_mcp_request_limit() {
    let mut s = server();
    let request = format!("{}{}", " ".repeat(solidcraft_engine::MAX_LINE), r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    let reply: Value = serde_json::from_str(&s.handle_line(&request).expect("size error")).unwrap();
    assert_eq!(reply["error"]["code"], -32600, "{reply}");
    assert!(reply["result"].is_null(), "oversized request must not execute: {reply}");
}

/// The transport must discard an oversized line in bounded space and resume on the next one.
#[test]
fn oversized_mcp_stdio_line_does_not_poison_the_next_request() {
    let mut bytes = vec![b'X'; solidcraft_engine::MAX_LINE + 1];
    bytes.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");
    let input = std::io::BufReader::with_capacity(64, std::io::Cursor::new(bytes));
    let mut output = Vec::new();
    server().serve(input, &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    let replies: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(replies.len(), 2, "{text}");
    assert_eq!(replies[0]["error"]["code"], -32600, "{}", replies[0]);
    assert_eq!(replies[1]["id"], 2, "{}", replies[1]);
    assert_eq!(replies[1]["result"], json!({}), "{}", replies[1]);
}

/// Preserve parse-error framing and accept an unterminated final line.
#[test]
fn mcp_stdio_recovers_after_invalid_utf8_and_accepts_eof() {
    let input = b"\xff\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}";
    let mut output = Vec::new();
    server().serve(std::io::Cursor::new(input), &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    let replies: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(replies.len(), 2, "{text}");
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[1]["id"], 3);
    assert_eq!(replies[1]["result"], json!({}));
}

#[test]
fn crash_reports_are_listed_and_read_without_changing_the_design() {
    let mut s = server();
    let list = tool(&mut s, "crash_reports", json!({"limit": 3}));
    assert!(list["reports"].as_array().is_some_and(|r| r.len() <= 3), "{list}");
    assert!(list["dir"].is_string(), "{list}");
    let r = raw_tool(&mut s, "crash_reports", json!({"id": "../../etc"}));
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    let r = raw_tool(&mut s, "crash_reports", json!({"id": 5}));
    assert!(r.get("error").is_some() || r["result"]["isError"] == json!(true), "{r}");
}

fn volume(s: &mut Server) -> f64 {
    tool(s, "measure", json!({}))["total"]["volume_mm3"].as_f64().unwrap()
}

fn rel(a: f64, b: f64) -> f64 {
    (a - b).abs() / b.abs().max(1e-12)
}

#[test]
fn handshake_and_tools_list() {
    let mut s = server();
    let init =
        call(&mut s, 1, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}));
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "solidcraft");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["instructions"].as_str().unwrap().contains("headless"));
    // An unknown version gets ours.
    let init = call(&mut s, 2, "initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
    assert!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    assert!(s.is_initialized());
    assert_eq!(call(&mut s, 3, "ping", json!({}))["result"], json!({}));

    let tools = call(&mut s, 4, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in [
        "list_commands",
        "describe_command",
        "execute",
        "batch",
        "inspect_design",
        "measure",
        "body_topology",
        "set_parameter",
        "check_parameters",
        "screenshot",
        "compare_picture",
        "export",
        "undo",
        "redo",
        "crash_reports",
        "skill_reference",
        "running",
        "new_design",
        "open",
        "save",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    for t in tools["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert!(t["description"].as_str().unwrap().len() > 20, "{t}");
    }

    let cmds = tool(&mut s, "list_commands", json!({"filter": "fillet"}));
    assert!(cmds.as_array().unwrap().iter().any(|c| c["id"] == "solid.fillet"), "{cmds}");
    assert!(cmds.as_array().unwrap().len() < 5);
    // Headless, calls run one at a time: none is running while `running` is asked (#3654).
    assert_eq!(tool(&mut s, "running", json!({})), json!({"running": []}));

    let res = call(&mut s, 5, "resources/list", json!({}));
    assert_eq!(res["result"]["resources"].as_array().unwrap().len(), 2);
    let d = call(&mut s, 6, "resources/read", json!({"uri": "solidcraft://design"}));
    assert!(d["result"]["contents"][0]["text"].as_str().unwrap().contains("timeline"));
    assert_eq!(call(&mut s, 7, "resources/read", json!({"uri": "solidcraft://nope"}))["error"]["code"], -32002);
    assert_eq!(call(&mut s, 8, "nope/nope", json!({}))["error"]["code"], -32601);
}

#[test]
fn skill_reference_returns_bundled_text_without_backend_calls() {
    let mut b = Recorder { calls: Vec::new(), batch: Err("unused".into()) };
    let docs = [
        ("solidcraft-modeling", "reference.md", include_str!("../../../skills/solidcraft-modeling/reference.md")),
        ("solidcraft-mechanisms", "reference.md", include_str!("../../../skills/solidcraft-mechanisms/reference.md")),
        ("solidcraft-checking", "reference/porting.md", include_str!("../../../skills/solidcraft-checking/reference/porting.md")),
        ("solidcraft-modeling", "SKILL.md", include_str!("../../../skills/solidcraft-modeling/SKILL.md")),
        ("solidcraft-mechanisms", "SKILL.md", include_str!("../../../skills/solidcraft-mechanisms/SKILL.md")),
        ("solidcraft-checking", "SKILL.md", include_str!("../../../skills/solidcraft-checking/SKILL.md")),
        ("solidcraft-editing", "SKILL.md", include_str!("../../../skills/solidcraft-editing/SKILL.md")),
    ];
    for (skill, file, expected) in docs {
        let args = json!({"skill": skill, "file": file});
        let r = crate::call_tool(&mut b, "skill_reference", &args);
        assert_ne!(r["isError"], true, "{r}");
        assert_eq!(r["content"][0]["text"], expected);
        if file == "reference.md" {
            assert_eq!(crate::call_tool(&mut b, "skill_reference", &json!({"skill": skill})), r);
        }
    }
    assert!(b.calls.is_empty(), "{:?}", b.calls);
}

#[test]
fn skill_reference_refuses_unknown_names_and_paths_with_available_choices() {
    let mut s = server();
    let skills = ["solidcraft-checking", "solidcraft-editing", "solidcraft-mechanisms", "solidcraft-modeling"];
    for skill in ["unknown", "../solidcraft-modeling", "/tmp/solidcraft-modeling"] {
        let r = raw_tool(&mut s, "skill_reference", json!({"skill": skill}));
        assert_eq!(r["result"]["isError"], true, "{r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        for available in skills {
            assert!(text.contains(available), "{text}");
        }
    }
    for file in ["missing.md", "../reference.md", "/etc/passwd", "reference/../reference.md"] {
        let r = raw_tool(&mut s, "skill_reference", json!({"skill": "solidcraft-modeling", "file": file}));
        assert_eq!(r["result"]["isError"], true, "{r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Available files: SKILL.md, reference.md"), "{text}");
    }
    for (skill, files) in [("solidcraft-checking", "SKILL.md, reference/porting.md"), ("solidcraft-editing", "SKILL.md")] {
        let r = raw_tool(&mut s, "skill_reference", json!({"skill": skill}));
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains(files), "{r}");
    }
}

#[test]
fn skill_reference_is_advertised_as_read_only() {
    let mut s = server();
    let r = call(&mut s, 1, "tools/list", json!({}));
    let t = r["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "skill_reference").expect("skill_reference tool");
    assert_eq!(t["annotations"]["readOnlyHint"], true);
    assert_eq!(t["annotations"]["destructiveHint"], false);
    assert_eq!(t["annotations"]["openWorldHint"], false);
    assert_eq!(t["inputSchema"]["required"], json!(["skill"]));
    assert_eq!(t["inputSchema"]["properties"]["file"]["default"], "reference.md");
}

/// A URDF goes through `export` with its own options (#719).
#[test]
fn export_tool_writes_a_urdf_package() {
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    let dir = std::env::temp_dir().join(format!("solidcraft-mcp-urdf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let urdf = dir.join("robot.urdf");
    let r = tool(&mut s, "export", json!({"path": urdf.to_string_lossy(), "mesh_prefix": "package://robot/meshes/"}));
    assert_eq!(r["format"], "Urdf", "{r}");
    assert!(std::fs::read_to_string(&urdf).unwrap().contains(r#"filename="package://robot/meshes/robot-base.stl""#));
    assert!(dir.join("robot-base.stl").exists());
    // effort and velocity reach file.export, which refuses a bad one.
    let r = raw_tool(&mut s, "export", json!({"path": urdf.to_string_lossy(), "effort": -1}));
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Box → fillet → hole in one batch, then verify with measure / inspect_design and look at it.
#[test]
fn agent_builds_box_fillet_hole() {
    let mut s = server();
    let r = tool(
        &mut s,
        "batch",
        json!({"commands": [
            {"command": "sketch.create", "params": {"plane": "XY"}},
            {"command": "sketch.rectangle.two_point", "params": {"p0": [0, 0], "p1": [40, 30]}},
            {"command": "sketch.finish"},
            {"command": "solid.extrude", "params": {"distance": 20}},
            {"command": "solid.fillet", "params": {"edges": [[0, 0, 10]], "radius": 3}},
            {"command": "sketch.create", "params": {"plane": "XY"}},
            {"command": "sketch.circle.center", "params": {"center": [20, 15], "radius": 5}},
            {"command": "solid.extrude", "params": {"distance": 20, "operation": "cut"}},
        ]}),
    );
    assert_eq!(r["completed"], 8, "{r}");
    let expect = 24000.0 - (9.0 - PI * 9.0 / 4.0) * 20.0 - PI * 25.0 * 20.0;
    let v = volume(&mut s);
    assert!(rel(v, expect) < 5e-4, "{v} vs {expect}");
    let m = tool(&mut s, "measure", json!({"body": "Body1"}));
    // 6 box faces + fillet + hole wall (the kernel may keep the wall as two halves).
    assert!((8..=9).contains(&m["bodies"][0]["faces"].as_u64().unwrap()), "{m}");
    assert!(rel(m["bodies"][0]["bbox"]["max"][0].as_f64().unwrap(), 40.0) < 1e-6, "{m}");

    let d = tool(&mut s, "inspect_design", json!({}));
    assert_eq!(d["timeline"].as_array().unwrap().len(), 5, "{d}");
    assert!(d["timeline"].as_array().unwrap().iter().all(|f| f["error"].is_null()), "{d}");
    assert_eq!(d["sketches"][0]["curves"].as_array().unwrap().len(), 4, "{d}");
    assert!(d["sketches"][0]["status"].is_string(), "{d}");
    assert!(d["bodies"][0]["volume_mm3"].as_f64().unwrap() > 0.0);

    let topo = tool(&mut s, "body_topology", json!({"body": "Body1"}));
    assert!(topo["edges"].as_array().unwrap().len() >= 12, "{topo}");
    assert!(topo["faces"].as_array().unwrap().len() >= 8, "{topo}");

    // Undo the hole, redo it.
    tool(&mut s, "undo", json!({}));
    assert!(rel(volume(&mut s), 24000.0 - (9.0 - PI * 9.0 / 4.0) * 20.0) < 5e-4);
    tool(&mut s, "redo", json!({}));
    assert!(rel(volume(&mut s), expect) < 5e-4);

    // Look at it from the top: a real PNG comes back as an image block.
    if renders() {
        let img = tool(&mut s, "screenshot", json!({"view": "top", "width": 320, "height": 200}));
        assert_eq!(img["type"], "image");
        assert_eq!(img["mimeType"], "image/png");
        assert!(img["data"].as_str().unwrap().starts_with("iVBORw0KGgo"), "not a PNG");
    }

    // Export, save, start over, open.
    let dir = std::env::temp_dir().join(format!("solidcraft-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let step = dir.join("part.step");
    tool(&mut s, "export", json!({"path": step.to_string_lossy()}));
    assert!(std::fs::read_to_string(&step).unwrap().contains("ISO-10303-21"));
    let stl = dir.join("part.stl");
    tool(&mut s, "export", json!({"path": stl.to_string_lossy(), "format": "stl"}));
    assert!(std::fs::metadata(&stl).unwrap().len() > 1000);
    let shot = dir.join("shot.png");
    if renders() {
        tool(&mut s, "screenshot", json!({"path": shot.to_string_lossy(), "width": 64, "height": 64}));
        assert!(std::fs::metadata(&shot).unwrap().len() > 100);
    }
    let design = dir.join("part.solidcraft");
    tool(&mut s, "save", json!({"path": design.to_string_lossy()}));
    tool(&mut s, "new_design", json!({"name": "Empty"}));
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 0);
    tool(&mut s, "open", json!({"path": design.to_string_lossy()}));
    assert!(rel(volume(&mut s), expect) < 5e-4);
    // A STEP file opens as a new design with the same body.
    let r = tool(&mut s, "open", json!({"path": step.to_string_lossy()}));
    assert_eq!(r["features"], 1, "{r}");
    assert!(rel(volume(&mut s), expect) < 5e-4);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A 60 × 40 × 10 plate with two Ø6 holes at x 15 and 45 and a Ø10 × 8 boss in the middle, all
/// on y 20 and along Z: three cylinders, the halves the kernel splits them into merged.
fn plate_with_holes_and_boss(s: &mut Server) {
    let r = tool(
        s,
        "batch",
        json!({"commands": [
            {"command": "sketch.create", "params": {"plane": "XY"}},
            {"command": "sketch.rectangle.two_point", "params": {"p0": [0, 0], "p1": [60, 40]}},
            {"command": "sketch.finish"},
            {"command": "solid.extrude", "params": {"distance": 10}},
            {"command": "sketch.create", "params": {"plane": "XY"}},
            {"command": "sketch.circle.center", "params": {"center": [15, 20], "radius": 3}},
            {"command": "sketch.circle.center", "params": {"center": [45, 20], "radius": 3}},
            {"command": "sketch.finish"},
            {"command": "solid.extrude", "params": {"distance": 10, "operation": "cut"}},
            {"command": "solid.cylinder", "params": {"radius": 5, "height": 8, "base": [30, 20, 10], "operation": "join"}},
        ]}),
    );
    assert_eq!(r["completed"], 10, "{r}");
}

/// `body_topology {faces: "cylinders"}` answers where the holes and bosses are in a few lines:
/// one entry per cylinder with its radius, axis line and extent, and no edges (#3380).
#[test]
fn body_topology_lists_merged_cylinders_with_their_extent() {
    let mut s = server();
    plate_with_holes_and_boss(&mut s);
    let full = tool(&mut s, "body_topology", json!({"body": "Body1"}));
    let halves = full["faces"].as_array().unwrap().iter().filter(|f| f["cylinder"].is_object()).count();
    assert!(halves >= 3, "{full}");

    let topo = tool(&mut s, "body_topology", json!({"body": "Body1", "faces": "cylinders"}));
    assert_eq!(topo["body"], "Body1", "{topo}");
    assert!(topo.get("edges").is_none() && topo.get("faces").is_none(), "{topo}");
    let found = topo["cylinders"].as_array().unwrap();
    assert_eq!(found.len(), 3, "{topo}");
    assert_eq!(found.iter().map(|c| c["names"].as_array().unwrap().len()).sum::<usize>(), halves, "{topo}");
    assert!(topo.to_string().len() * 4 < full.to_string().len(), "{} vs {} bytes", topo.to_string().len(), full.to_string().len());
    // Each cylinder: (x of its axis, radius, the z its extent spans), its axis along Z.
    let mut seen: Vec<(i64, i64, i64, i64)> = found
        .iter()
        .map(|c| {
            let axis: Vec<f64> = c["axis"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let origin: Vec<f64> = c["origin"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            assert!(axis[2].abs() > 1.0 - 1e-6, "{c}");
            assert!((origin[1] - 20.0).abs() < 1e-3, "{c}");
            let extent: Vec<f64> = c["extent"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let mut z: Vec<f64> = extent.iter().map(|t| origin[2] + t * axis[2]).collect();
            z.sort_by(|a, b| a.total_cmp(b));
            let mm = |v: f64| (v * 1000.0).round() as i64;
            (mm(origin[0]), mm(c["radius"].as_f64().unwrap()), mm(z[0]), mm(z[1]))
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, vec![(15000, 3000, 0, 10000), (30000, 5000, 10000, 18000), (45000, 3000, 0, 10000)], "{topo}");
    // Every merged half is a real face name, so the entry picks the face too.
    let names: Vec<&str> = found.iter().flat_map(|c| c["names"].as_array().unwrap()).filter_map(Value::as_str).collect();
    let real: Vec<&str> = full["faces"].as_array().unwrap().iter().filter_map(|f| f["name"].as_str()).collect();
    assert!(!names.is_empty() && names.iter().all(|n| real.contains(n)), "{names:?} in {real:?}");

    // The default is unchanged, and a value outside the enum is refused by argument checking.
    let same = tool(&mut s, "body_topology", json!({"body": "Body1", "faces": "all"}));
    assert_eq!(same, full);
    let r = raw_tool(&mut s, "body_topology", json!({"body": "Body1", "faces": "planes"}));
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("cylinders"), "{r}");
}

/// `measure {summary: true}` keeps what a stage check needs, as a tool and as a batch step (#3380).
#[test]
fn measure_summary_keeps_only_the_check_fields() {
    let mut s = server();
    plate_with_holes_and_boss(&mut s);
    let full = tool(&mut s, "measure", json!({}));
    let m = tool(&mut s, "measure", json!({"summary": true}));
    let keys = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(keys(&m), ["bodies", "body_count", "total"], "{m}");
    assert_eq!(keys(&m["total"]), ["volume_mm3"], "{m}");
    assert_eq!(m["body_count"], 1, "{m}");
    let b = &m["bodies"][0];
    assert_eq!(keys(b), ["bbox", "closed", "faces", "name", "volume_mm3"], "{m}");
    assert_eq!(b["name"], "Body1");
    assert_eq!(b["closed"], true);
    assert_eq!(b["faces"], full["bodies"][0]["faces"]);
    assert_eq!(b["bbox"], full["bodies"][0]["bbox"]);
    let expect = 60.0 * 40.0 * 10.0 - 2.0 * PI * 9.0 * 10.0 + PI * 25.0 * 8.0;
    let v = m["total"]["volume_mm3"].as_f64().unwrap();
    assert_eq!(v, full["total"]["volume_mm3"].as_f64().unwrap(), "{m}");
    assert_eq!(b["volume_mm3"].as_f64().unwrap(), full["bodies"][0]["volume_mm3"].as_f64().unwrap(), "{m}");
    assert!(rel(v, expect) < 1e-3, "{v} vs {expect}");
    assert!(m.to_string().len() * 2 < full.to_string().len(), "{} vs {} bytes", m.to_string().len(), full.to_string().len());
    // One body by name, and the default unchanged.
    let one = tool(&mut s, "measure", json!({"body": "Body1", "summary": true}));
    assert_eq!(one, m);
    assert_eq!(tool(&mut s, "measure", json!({"summary": false})), full);

    // As a batch step, the same reply.
    let r = tool(
        &mut s,
        "batch",
        json!({"commands": [{"command": "measure", "params": {"summary": true}}, {"command": "measure", "params": {"body": "Body1"}}]}),
    );
    assert_eq!(r["completed"], 2, "{r}");
    assert_eq!(r["results"][0], m, "{r}");
    assert_eq!(r["results"][1], full, "{r}");
    assert_eq!(raw_tool(&mut s, "measure", json!({"summary": "yes"}))["error"]["code"], -32602);
}

/// `open` hands `up` and `names` to the STEP import, and the tool's schema refuses other values.
#[test]
fn open_stands_a_y_up_step_file_up() {
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 40, "height": 5}}));
    let dir = std::env::temp_dir().join(format!("solidcraft-mcp-up-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let step = dir.join("tall.step").to_string_lossy().to_string();
    tool(&mut s, "export", json!({"path": step}));
    let height = |s: &mut Server| {
        let bbox = tool(s, "measure", json!({}))["bodies"][0]["bbox"].clone();
        bbox["max"][2].as_f64().unwrap() - bbox["min"][2].as_f64().unwrap()
    };

    let r = tool(&mut s, "open", json!({"path": step}));
    assert_eq!(r["features"], 1, "{r}");
    assert!((height(&mut s) - 5.0).abs() < 1e-6, "as written");
    let r = tool(&mut s, "open", json!({"path": step, "up": "y", "names": "product"}));
    assert_eq!(r["features"], 1, "the import, its component turned: {r}");
    assert!((height(&mut s) - 40.0).abs() < 1e-6, "Y became Z");

    for args in [json!({"path": step, "up": "x"}), json!({"path": step, "names": "label"}), json!({"path": step, "up": 1})] {
        let r = raw_tool(&mut s, "open", args.clone());
        assert_eq!(r["error"]["code"], -32602, "{args}: {r}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_selects_container_or_json_and_reopens_by_contents() {
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 20, "height": 30}}));
    let dir = std::env::temp_dir().join(format!("solidcraft-mcp-encodings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, format, is_json) in [
        ("default.solidcraft", None, false),
        ("explicit.solidcraft", Some("json"), true),
        ("legacy.solvecraft", None, false),
        ("inferred.json", None, true),
        ("container.json", Some("container"), false),
    ] {
        let path = dir.join(name);
        let mut args = json!({"path": path.to_string_lossy()});
        if let Some(format) = format {
            args["format"] = json!(format);
        }
        tool(&mut s, "save", args);
        let bytes = std::fs::read(&path).unwrap();
        if is_json {
            assert!(serde_json::from_slice::<Value>(&bytes).unwrap().is_object());
        } else {
            assert!(bytes.starts_with(b"PK\x03\x04"));
        }
        tool(&mut s, "new_design", json!({}));
        tool(&mut s, "open", json!({"path": path.to_string_lossy()}));
        assert!(rel(volume(&mut s), 6000.0) < 1e-9);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn parameters_drive_the_design() {
    let mut s = server();
    tool(&mut s, "set_parameter", json!({"name": "len", "value": "40 mm"}));
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": "len", "width": 30, "height": "len / 2", "corner": [0, 0, 0]}}));
    assert!(rel(volume(&mut s), 40.0 * 30.0 * 20.0) < 1e-9);
    let r = tool(&mut s, "set_parameter", json!({"name": "len", "value": 50}));
    assert_eq!(r["value"], 50.0, "{r}");
    assert_eq!(r["errors"].as_array().unwrap().len(), 0, "{r}");
    assert!(rel(volume(&mut s), 50.0 * 30.0 * 25.0) < 1e-9);
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert!(d["params"].as_array().unwrap().iter().any(|p| p["name"] == "len" && p["value"] == 50.0), "{d}");
}

/// `inspect_design` gives each joint's definition (as `joint.inspect`) when the design has
/// joints, leaves the key out when it has none, and `joints: false` leaves them out.
#[test]
fn inspect_design_lists_joint_definitions() {
    let mut s = server();
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert!(d.get("joints").is_none(), "{d}");
    for (name, corner) in [("A", [0, 0, 0]), ("B", [20, 0, 0])] {
        tool(&mut s, "execute", json!({"command": "component.activate", "params": {"component": "root"}}));
        tool(&mut s, "execute", json!({"command": "component.create", "params": {"name": name}}));
        tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "corner": corner}}));
    }
    tool(
        &mut s,
        "execute",
        json!({"command": "joint.create", "params": {"type": "rigid", "name": "Glue", "a": {"occurrence": "A:1", "face": [5, 5, 10]}, "b": {"occurrence": "B:1", "face": [25, 5, 0]}}}),
    );
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert_eq!(d["joints"][0]["name"], "Glue", "{d}");
    assert_eq!(d["joints"][0]["a"]["snap"]["type"], "face_center", "{d}");
    assert_eq!(d["joints"][0]["b"]["resolves"], true, "{d}");
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false, "joints": false}));
    assert!(d.get("joints").is_none(), "{d}");
}

/// `set_parameter` takes the new value as `value` or as `expression` (the name parameters.change
/// uses), which it used to refuse instead of forwarding.
#[test]
fn set_parameter_takes_value_or_expression() {
    let mut s = server();
    let def = tool_definitions().into_iter().find(|d| d["name"] == "set_parameter").unwrap();
    assert_eq!(def["inputSchema"]["required"], json!(["name"]), "{def}");
    assert!(def["inputSchema"]["properties"]["expression"].is_object(), "{def}");
    tool(&mut s, "set_parameter", json!({"name": "len", "expression": "40 mm"}));
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": "len", "width": 10, "height": 10, "corner": [0, 0, 0]}}));
    assert!(rel(volume(&mut s), 4000.0) < 1e-9);
    let r = tool(&mut s, "set_parameter", json!({"name": "len", "expression": 50}));
    assert_eq!(r["value"], 50.0, "{r}");
    assert!(rel(volume(&mut s), 5000.0) < 1e-9);
    // Both, when they agree.
    let r = tool(&mut s, "set_parameter", json!({"name": "len", "value": 60, "expression": "60"}));
    assert_eq!(r["value"], 60.0, "{r}");
    // Both disagreeing, or neither, is an error that changes nothing.
    for args in [json!({"name": "len", "value": 70, "expression": "80"}), json!({"name": "len"}), json!({"name": "len", "value": null})] {
        let r = raw_tool(&mut s, "set_parameter", args.clone());
        assert_eq!(r["result"]["isError"], json!(true), "{args}: {r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("`value`") && text.contains("`expression`"), "{args}: {text}");
    }
    assert!(rel(volume(&mut s), 6000.0) < 1e-9);
}

/// A box whose length is a parameter and a cylinder standing on it at `len / 2`, so a change
/// of `len` moves the cylinder's axis.
fn parametric_box_with_cylinder(s: &mut Server) {
    tool(&mut *s, "set_parameter", json!({"name": "len", "value": "40 mm"}));
    tool(
        &mut *s,
        "batch",
        json!({"commands": [
            {"command": "solid.box", "params": {"length": "len", "width": 30, "height": 20, "corner": [0, 0, 0]}},
            {"command": "solid.cylinder", "params": {"radius": 5, "height": 10, "base": ["len / 2", 15, 20], "operation": "join"}},
        ]}),
    );
}

fn box_with_cylinder_volume(len: f64) -> f64 {
    len * 30.0 * 20.0 + PI * 25.0 * 10.0
}

/// `check_parameters` applies the values with one rebuild, reports the new volume, the
/// features' state and the moved cylinder axis (its two kernel halves as one cylinder), and
/// puts the design back: the parameter, the volume and the undo history are as before.
#[test]
fn check_parameters_tries_values_and_puts_the_design_back() {
    let mut s = server();
    parametric_box_with_cylinder(&mut s);
    assert!(rel(volume(&mut s), box_with_cylinder_volume(40.0)) < 5e-4);
    let before = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    let r = tool(&mut s, "check_parameters", json!({"set": {"len": "60 mm"}}));
    assert_eq!(r["restored"], true, "{r}");
    assert_eq!(r["values"]["len"], 60.0, "{r}");
    assert!(r["recomputed"].as_u64().unwrap() >= 1, "{r}");
    let v = r["measure"]["total"]["volume_mm3"].as_f64().unwrap();
    assert!(rel(v, box_with_cylinder_volume(60.0)) < 5e-4, "{v}");
    assert_eq!(r["measure"]["body_count"], 1, "{r}");
    assert!(r["measure"]["bodies"][0]["faces"].as_u64().unwrap() >= 8, "{r}");
    assert_eq!((&r["errors"], &r["warnings"], &r["parameter_errors"]), (&json!([]), &json!([]), &json!([])), "{r}");
    let cyl = r["cylinders"].as_array().unwrap();
    assert_eq!(cyl.len(), 1, "one cylinder, halves merged: {r}");
    assert_eq!(cyl[0]["body"], "Body1", "{r}");
    assert!((cyl[0]["radius"].as_f64().unwrap() - 5.0).abs() < 1e-6, "{r}");
    let origin: Vec<f64> = cyl[0]["origin"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
    let axis: Vec<f64> = cyl[0]["axis"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
    assert!((origin[0] - 30.0).abs() < 1e-6 && (origin[1] - 15.0).abs() < 1e-6 && origin[2].abs() < 1e-6, "axis moved to len / 2 = 30: {r}");
    assert!(axis[0].abs() < 1e-9 && axis[1].abs() < 1e-9 && (axis[2] - 1.0).abs() < 1e-9, "{r}");
    assert!(!cyl[0]["faces"].as_array().unwrap().is_empty(), "{r}");
    // The halves' areas add up to the cylinder's (measured on its mesh).
    assert!(rel(cyl[0]["area"].as_f64().unwrap(), 2.0 * PI * 5.0 * 10.0) < 1e-2, "{r}");
    // Back as it was: the value, the volume and the undo history (a checkpoint restores it).
    assert!(rel(volume(&mut s), box_with_cylinder_volume(40.0)) < 5e-4);
    let after = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert!(after["params"].as_array().unwrap().iter().any(|p| p["name"] == "len" && p["value"] == 40.0 && p["expression"] == "40 mm"), "{after}");
    assert_eq!(after["undo"], before["undo"], "undo depth: {after}");
    assert_eq!(after["redo"], before["redo"], "{after}");
    // Only what was asked for.
    let r = tool(&mut s, "check_parameters", json!({"set": {"len": 50}, "report": ["measure"]}));
    assert!(rel(r["measure"]["total"]["volume_mm3"].as_f64().unwrap(), box_with_cylinder_volume(50.0)) < 5e-4, "{r}");
    assert!(r.get("cylinders").is_none() && r.get("errors").is_none(), "{r}");
    assert_eq!(r["restored"], true, "{r}");
    assert!(rel(volume(&mut s), box_with_cylinder_volume(40.0)) < 5e-4);
    // A value that breaks a feature is reported, not kept.
    let r = tool(&mut s, "check_parameters", json!({"set": {"len": 0}, "report": ["errors"]}));
    assert!(!r["errors"].as_array().unwrap().is_empty(), "{r}");
    assert_eq!(r["restored"], true, "{r}");
    assert!(rel(volume(&mut s), box_with_cylinder_volume(40.0)) < 5e-4);
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert!(d["timeline"].as_array().unwrap().iter().all(|f| f["error"].is_null()), "{d}");
}

/// An unknown parameter name, a bad expression or a bad report list is an error that leaves the
/// design untouched; `set` that is not an object is refused by argument checking; the tool is
/// annotated as an edit (it changes the design and puts it back) and is not a batch step.
#[test]
fn check_parameters_refuses_bad_input_and_changes_nothing() {
    let mut s = server();
    parametric_box_with_cylinder(&mut s);
    let before = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    let r = raw_tool(&mut s, "check_parameters", json!({"set": {"nope": 5}}));
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("`nope`") && text.contains("`len`"), "{text}");
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert_eq!(d["undo"], before["undo"], "{d}");
    assert!(d["params"].as_array().unwrap().iter().all(|p| p["name"] != "nope"), "{d}");
    for args in [json!({"set": "len"}), json!({"set": [["len", 5]]}), json!({"set": 5}), json!({"set": {"len": 5}, "report": "measure"}), json!({})] {
        let r = raw_tool(&mut s, "check_parameters", args.clone());
        assert_eq!(r["error"]["code"], -32602, "{args}: {r}");
    }
    for (args, hint) in [
        (json!({"set": {}}), "empty"),
        (json!({"set": {"len": true}}), "`set.len`"),
        (json!({"set": {"len": null}}), "`set.len`"),
        (json!({"set": {"": 5}}), "empty"),
        (json!({"set": {"len": 5}, "report": ["volume"]}), "measure, cylinders, errors"),
        (json!({"set": {"len": "1 +"}}), "len"),
        (json!({"set": {"len": "a = 5"}}), "len"),
    ] {
        let r = raw_tool(&mut s, "check_parameters", args.clone());
        assert_eq!(r["result"]["isError"], json!(true), "{args}: {r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains(hint), "{args}: {text}");
    }
    assert!(rel(volume(&mut s), box_with_cylinder_volume(40.0)) < 5e-4);
    // Untouched: the undo depth and the parameter (the engine's `revision` moves even for a
    // refused change, so it is not what tells).
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert_eq!(d["undo"], before["undo"], "{d}");
    assert!(d["params"].as_array().unwrap().iter().any(|p| p["name"] == "len" && p["expression"] == "40 mm"), "{d}");
    // Not a batch step: refused before anything runs.
    let r = raw_tool(&mut s, "batch", json!({"commands": [{"command": "check_parameters", "params": {"set": {"len": 5}}}]}));
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["completed"], 0, "{body}");
    assert!(body["failed"]["error"].as_str().unwrap().contains("check_parameters"), "{body}");
    let def = tool_definitions().into_iter().find(|d| d["name"] == "check_parameters").unwrap();
    assert_eq!(def["inputSchema"]["required"], json!(["set"]), "{def}");
    assert_eq!(def["annotations"]["readOnlyHint"], false, "{def}");
    assert_eq!(def["annotations"]["destructiveHint"], false, "{def}");
}

/// A backend answering from a closure: what `check_parameters` sends, and what it does when an
/// app has no `parameters.change {changes}` or cannot restore the checkpoint.
struct Answers {
    calls: Vec<(String, Value)>,
    answer: Box<dyn Fn(&str, &Value) -> Result<Value, String>>,
}

impl crate::backend::Backend for Answers {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.calls.push((method.to_string(), params.clone()));
        (self.answer)(method, &params)
    }
    fn has_ui(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "answers".into()
    }
}

/// An app without `parameters.change {changes}` gets one change per name; one that cannot
/// restore the checkpoint makes the call an error that says the values are still in.
#[test]
fn check_parameters_falls_back_per_name_and_says_when_the_restore_failed() {
    let app = |restore_ok: bool| Answers {
        calls: Vec::new(),
        answer: Box::new(move |method, p| {
            let command = p["command"].as_str().unwrap_or_default();
            match (method, command) {
                ("engine.execute", "parameters.list") => Err("unknown command `parameters.list`".into()),
                ("engine.execute", "edit.checkpoint") => Ok(json!({"checkpoint": 3})),
                ("engine.execute", "edit.restore_checkpoint") if restore_ok => Ok(json!({"checkpoint": 3, "restored": true})),
                ("engine.execute", "edit.restore_checkpoint") => Err("busy: request 9 is still running".into()),
                ("engine.execute", "parameters.change") if p["params"].get("changes").is_some() => {
                    Err("parameters.change: `name` is required".into())
                }
                ("engine.execute", "parameters.change") => Ok(json!({"name": p["params"]["name"], "value": 7.0, "recomputed": 1})),
                ("engine.execute", "inspect.measure") => Ok(json!({"body_count": 1, "bodies": [], "total": {"volume_mm3": 12.5}})),
                ("document.inspect", _) => {
                    Ok(json!({"timeline": [{"id": 1, "name": "Box", "error": null, "warning": "thin"}], "params": [], "bodies": [{"name": "B"}]}))
                }
                ("engine.execute", "model.faces") => Ok(json!({"faces": [
                    {"index": 0, "name": "side", "area": 1.0, "cylinder": {"origin": [0, 0, 5], "axis": [0, 0, -1], "radius": 2}},
                    {"index": 1, "name": "side", "area": 1.0, "cylinder": {"origin": [0, 0, -3], "axis": [0, 0, 1], "radius": 2}},
                    {"index": 2, "name": "hole", "area": 1.0, "cylinder": {"origin": [9, 0, 0], "axis": [0, 0, 1], "radius": 2}},
                    {"index": 3, "name": "top", "area": 4.0, "plane_normal": [0, 0, 1]},
                ]})),
                (m, c) => Err(format!("unknown method `{m}` ({c})")),
            }
        }),
    };
    let mut b = app(true);
    let r = crate::tools::call_tool(&mut b, "check_parameters", &json!({"set": {"a": "1 mm", "b": 2}}));
    assert_ne!(r["isError"], json!(true), "{r}");
    let body: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["restored"], true, "{body}");
    assert_eq!(body["values"], json!({"a": 7.0, "b": 7.0}), "{body}");
    assert_eq!(body["recomputed"], 2, "{body}");
    assert_eq!(body["measure"]["total"]["volume_mm3"], 12.5, "{body}");
    assert_eq!(body["warnings"], json!([{"feature": 1, "name": "Box", "warning": "thin"}]), "{body}");
    assert_eq!(body["errors"], json!([]), "{body}");
    // Two halves on one axis, reported with opposite directions and different axis points, are
    // one cylinder; the hole next to it is another.
    let cyl = body["cylinders"].as_array().unwrap();
    assert_eq!(cyl.len(), 2, "{body}");
    assert_eq!((&cyl[0]["faces"], &cyl[0]["origin"], &cyl[0]["axis"]), (&json!([0, 1]), &json!([0.0, 0.0, 0.0]), &json!([0.0, 0.0, 1.0])), "{body}");
    assert_eq!((&cyl[1]["faces"], &cyl[1]["origin"], &cyl[1]["name"]), (&json!([2]), &json!([9.0, 0.0, 0.0]), &json!("hole")), "{body}");
    let changes: Vec<&Value> = b.calls.iter().filter(|(_, p)| p["command"] == "parameters.change").map(|(_, p)| &p["params"]).collect();
    assert_eq!(changes.len(), 3, "one combined change, then one per name: {changes:?}");
    assert_eq!(changes[0], &json!({"changes": {"a": "1 mm", "b": 2}}), "{changes:?}");
    assert_eq!((changes[1]["name"].as_str(), changes[2]["name"].as_str()), (Some("a"), Some("b")), "{changes:?}");
    let last = b.calls.last().unwrap();
    assert_eq!(last.1["command"], "edit.restore_checkpoint", "the restore is the last call: {last:?}");
    assert_eq!(last.1["params"]["checkpoint"], 3, "{last:?}");

    // The restore fails: an error, loudly, with everything that was read.
    let mut b = app(false);
    let r = crate::tools::call_tool(&mut b, "check_parameters", &json!({"set": {"a": "1 mm"}, "report": ["measure"]}));
    assert_eq!(r["isError"], json!(true), "{r}");
    let body: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["restored"], false, "{body}");
    assert!(body["restore_error"].as_str().unwrap().contains("still running"), "{body}");
    assert!(body["note"].as_str().unwrap().contains("STILL HOLDS"), "{body}");
    assert_eq!(body["measure"]["total"]["volume_mm3"], 12.5, "{body}");
}

/// Design properties (#1015) are set through `execute` and come back in `inspect_design`.
#[test]
fn design_properties_show_in_inspect_design() {
    let mut s = server();
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert_eq!(d["metadata"], json!({}), "{d}");
    let params = json!({"values": {"author": "Agent", "com.example.agent": {"session": 7}}});
    tool(&mut s, "execute", json!({"command": "document.metadata.set", "params": params}));
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert_eq!(d["metadata"], json!({"author": "Agent", "com.example.agent": {"session": 7}}), "{d}");
}

#[test]
fn batch_stops_at_first_error_and_rolls_back() {
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    let r = raw_tool(
        &mut s,
        "batch",
        json!({"commands": [
            {"command": "solid.sphere", "params": {"radius": 3, "center": [50, 0, 0]}},
            {"command": "solid.extrude", "params": {"distance": "bogus +"}},
            {"command": "solid.sphere", "params": {"radius": 3, "center": [80, 0, 0]}},
        ]}),
    );
    assert_eq!(r["result"]["isError"], true, "{r}");
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["failed"]["index"], 1, "{body}");
    assert_eq!(body["failed"]["command"], "solid.extrude");
    assert_eq!(body["undone"], 1, "{body}");
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    // Only the first box is left, and its undo step is intact.
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 1);
    assert_eq!(tool(&mut s, "inspect_design", json!({"measure": false}))["undo"], 1);

    // Without rollback the completed steps stay.
    let r = raw_tool(
        &mut s,
        "batch",
        json!({"rollback": false, "commands": [{"command": "solid.sphere", "params": {"radius": 3, "center": [50, 0, 0]}}, {"command": "solid.extrude", "params": {"distance": "bogus +"}}]}),
    );
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("solid.extrude"));
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 2);
    // `names_only` passes through, directly and as a batch step.
    let n = tool(&mut s, "measure", json!({"names_only": true}));
    assert_eq!(n["body_count"], 2, "{n}");
    assert!(n["bodies"][0]["name"].is_string() && n["bodies"][0].get("volume_mm3").is_none(), "{n}");
    assert_eq!(tool(&mut s, "measure", json!({"names_only": true, "summary": true})), n, "summary has nothing to narrow in a names list");
    let r = tool(&mut s, "batch", json!({"commands": [{"command": "measure", "params": {"names_only": true, "summary": true}}]}));
    assert!(r.to_string().contains("\"definition\""), "{r}");
}

/// #5853: a misspelled command, however late in the batch, refuses the batch before its first
/// step: nothing runs, nothing is undone, and the error names the closest command.
#[test]
fn batch_refuses_an_unknown_command_before_the_first_step() {
    let mut s = server();
    let mut steps: Vec<Value> = (0..20).map(|k| json!({"command": "solid.sphere", "params": {"radius": 1, "center": [k * 5, 0, 0]}})).collect();
    steps.push(json!({"command": "solid.extrud", "params": {"distance": 5}}));
    let r = raw_tool(&mut s, "batch", json!({"commands": steps}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((body["completed"].clone(), body["failed"]["index"].clone(), body["undone"].clone()), (json!(0), json!(20), json!(0)), "{body}");
    assert!(body["failed"]["error"].as_str().is_some_and(|e| e.contains("closest: `solid.extrude`")), "{body}");
    assert_eq!((body["applied"].clone(), body["not_run"].as_array().map(Vec::len)), (json!([]), Some(21)), "{body}");
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 0, "no step ran");
    assert_eq!(tool(&mut s, "inspect_design", json!({"measure": false}))["undo"], 0);
}

/// A step with its parameters under another key (`args`) refuses the batch before its first
/// step, naming the key, instead of running the command without them.
#[test]
fn batch_refuses_a_step_with_an_unknown_key() {
    let mut s = server();
    let steps = json!([
        {"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}},
        {"command": "sketch.create", "args": {"plane": "XY"}},
    ]);
    let r = raw_tool(&mut s, "batch", json!({"commands": steps}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((body["completed"].clone(), body["failed"]["index"].clone()), (json!(0), json!(1)), "{body}");
    assert!(body["failed"]["error"].as_str().is_some_and(|e| e.contains("unknown key `args`") && e.contains("`params`")), "{body}");
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 0, "no step ran");
}

/// #5822: with `coalesce_parameters` a batch of parameter edits, one per step, rebuilds the
/// design once for the run, and every step still has its own result. A run with an edit
/// `changes` can't make (a new parameter) goes step by step, and a refused edit is still named
/// by its own step.
#[test]
fn batch_coalesces_parameter_edits() {
    let mut s = server();
    let mut build = vec![json!({"command": "solid.box", "params": {"length": 100, "width": 100, "height": 5, "body_name": "Plate"}})];
    for i in 0..5 {
        build.push(json!({"command": "parameters.add", "params": {"name": format!("h{i}"), "expression": "5 mm"}}));
        build.push(json!({"command": "solid.cylinder", "params": {"radius": 3, "height": format!("h{i}"), "base": [i * 15 - 30, 0, 5], "operation": "join"}}));
    }
    tool(&mut s, "batch", json!({"commands": build}));
    let edits: Vec<Value> = (0..5)
        .map(|i| json!({"command": "parameters.change", "params": {"name": format!("h{i}"), "expression": format!("{} mm", 10 + i)}}))
        .collect();
    let r = tool(&mut s, "batch", json!({"commands": edits, "coalesce_parameters": true}));
    let results = r["results"].as_array().unwrap();
    assert_eq!(results.len(), 5, "{r}");
    for (i, row) in results.iter().enumerate() {
        assert_eq!(row["name"], format!("h{i}"), "{row}");
        assert_eq!(row["coalesced"], 5, "{row}");
        assert_eq!(row["value"], json!(10.0 + i as f64), "{row}");
    }
    // The plate and five pins, as tall as the new parameters make them.
    let pins: f64 = (0..5).map(|i| std::f64::consts::PI * 9.0 * (10.0 + f64::from(i))).sum();
    let v = tool(&mut s, "measure", json!({}))["total"]["volume_mm3"].as_f64().unwrap();
    assert!((v - (100.0 * 100.0 * 5.0 + pins)).abs() < 1e-3 * v, "{v}");
    // The same edits step by step on a second design end in the same design and values.
    let mut t = server();
    tool(&mut t, "batch", json!({"commands": build}));
    let one_by_one = tool(&mut t, "batch", json!({"commands": edits}));
    for (a, b) in results.iter().zip(one_by_one["results"].as_array().unwrap()) {
        assert_eq!((&a["name"], &a["value"]), (&b["name"], &b["value"]), "{a} vs {b}");
    }
    let w = tool(&mut t, "measure", json!({}))["total"]["volume_mm3"].as_f64().unwrap();
    assert!((v - w).abs() < 1e-9 * w, "coalesced {v}, step by step {w}");
    let list = |s: &mut Server| tool(s, "execute", json!({"command": "parameters.list", "params": {"filter": "user"}}));
    assert_eq!(list(&mut s), list(&mut t));
    // A new parameter in the run: the run goes step by step and still sets everything.
    let r = tool(
        &mut s,
        "batch",
        json!({"coalesce_parameters": true, "commands": [
            {"command": "parameters.change", "params": {"name": "h0", "expression": "6 mm"}},
            {"command": "parameters.change", "params": {"name": "spare", "expression": "2 mm"}},
        ]}),
    );
    assert_eq!(r["results"].as_array().map(Vec::len), Some(2), "{r}");
    assert!(r["results"][0].get("coalesced").is_none(), "{r}");
    // A refused edit is named by its own step, and the batch goes back.
    let r = raw_tool(
        &mut s,
        "batch",
        json!({"coalesce_parameters": true, "commands": [
            {"command": "parameters.change", "params": {"name": "h1", "expression": "7 mm"}},
            {"command": "parameters.change", "params": {"name": "h2", "expression": "1 +"}},
        ]}),
    );
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["failed"]["index"], 1, "{body}");
    assert_eq!(body["rolled_back"], true, "{body}");
    let rows = tool(&mut s, "execute", json!({"command": "parameters.list", "params": {"filter": "user"}}));
    let h1 = rows.to_string();
    assert!(h1.contains("11 mm"), "h1 kept its value after the refused batch: {h1}");
}

/// Regression: with a full undo history a failed batch reported `undone: 0` and left its
/// sketches behind (an empty "Sketch1", two sketches named "TapHole").
#[test]
fn batch_rolls_back_with_a_full_undo_history() {
    let mut s = server();
    let fill: Vec<Value> =
        (0..230).map(|i| json!({"command": "parameters.add", "params": {"name": format!("p{i}"), "expression": "1 mm"}})).collect();
    tool(&mut s, "batch", json!({"commands": fill}));
    let undo = tool(&mut s, "inspect_design", json!({"measure": false}))["undo"].clone();
    for _ in 0..2 {
        let r = raw_tool(
            &mut s,
            "batch",
            json!({"commands": [
                {"command": "sketch.create", "params": {"plane": "XY", "name": "TapHole"}},
                {"command": "sketch.circle.center", "params": {"center": [0, 0], "radius": 2}},
                {"command": "sketch.dimension", "params": {"entities": ["nope"], "value": 3}},
            ]}),
        );
        let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["failed"]["index"], 2, "{body}");
        assert_eq!(body["undone"], 2, "{body}");
    }
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert_eq!(d["sketches"].as_array().map(Vec::len), Some(0), "{d}");
    assert!(d["active_sketch"].is_null(), "{d}");
    assert_eq!(d["undo"], undo);
}

/// Regression (#3242): a failed batch containing `file.new` reported `rolled_back: true` and
/// kept the replacement design. The checkpoint survives the new document, so the design the
/// batch started in — the 1 mm cube — is back when the batch fails.
#[test]
fn a_failed_batch_with_file_new_restores_the_original_design() {
    let mut s = server();
    tool(&mut s, "batch", json!({"commands": [{"command": "solid.box", "params": {"length": 1, "width": 1, "height": 1, "body_name": "Original"}}]}));
    let name = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}))["name"].clone();
    let r = raw_tool(
        &mut s,
        "batch",
        json!({"rollback": true, "commands": [
            {"command": "file.new", "params": {"name": "Replacement"}},
            {"command": "solid.box", "params": {"length": 2, "width": 2, "height": 2}},
            // Fails at its turn (an unknown command would be refused before the first step, #5853).
            {"command": "solid.extrude", "params": {"distance": "bogus +"}},
        ]}),
    );
    assert_eq!(r["result"]["isError"], true, "{r}");
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["failed"]["index"], 2, "{body}");
    assert_eq!(body["rolled_back"], true, "{body}");
    assert_eq!(body["undone"], 2, "{body}");
    // The original 1 mm cube is back; the replacement design is gone.
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 1);
    assert!(rel(volume(&mut s), 1.0) < 1e-9, "{}", volume(&mut s));
    let d = tool(&mut s, "inspect_design", json!({"measure": false, "sketches": false}));
    assert_eq!(d["name"], name, "{d}");
}

/// `expect` on a batch: the volume, body count and bounding box the agent computed beforehand
/// are measured and compared by the server once the batch went through, so no separate
/// `measure` call and no comparison by hand follow every stage.
#[test]
fn a_batch_with_a_matching_expect_reports_the_expectation_met() {
    let mut s = server();
    let r = tool(
        &mut s,
        "batch",
        json!({
            "commands": [
                {"command": "solid.box", "params": {"length": 40, "width": 30, "height": 20, "corner": [0, 0, 0]}},
                {"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "corner": [15, 10, 5], "operation": "cut"}},
            ],
            "expect": {"volume_mm3": 23000, "bodies": 1, "bbox": {"min": [0, 0, 0], "max": [40, 30, 20]}},
        }),
    );
    assert_eq!(r["completed"], 2, "{r}");
    let e = &r["expectation"];
    assert_eq!(e["ok"], true, "{r}");
    assert_eq!(e["mismatch"], json!([]), "{r}");
    assert_eq!(e["measured"]["bodies"], 1, "{r}");
    assert!(rel(e["measured"]["volume_mm3"].as_f64().unwrap(), 23000.0) < 1e-6, "{r}");
    assert!(rel(e["measured"]["bbox"]["max"][0].as_f64().unwrap(), 40.0) < 1e-6, "{r}");
    assert_eq!(
        e["expected"],
        json!({"volume_mm3": 23000.0, "tol": 0.002, "bodies": 1, "bbox": {"min": [0.0, 0.0, 0.0], "max": [40.0, 30.0, 20.0]}}),
        "{r}"
    );
    // Without `expect` the reply is as before.
    tool(&mut s, "new_design", json!({}));
    let box40 = json!({"command": "solid.box", "params": {"length": 40, "width": 30, "height": 20, "corner": [0, 0, 0]}});
    let r = tool(&mut s, "batch", json!({"commands": [box40]}));
    assert!(r.get("expectation").is_none(), "{r}");
    // `expect` is the tool's argument, not a step key: a step carrying one is refused, as any step
    // with a stray key is, saying where `expect` goes, instead of running with nothing checked.
    let sphere = json!({"command": "solid.sphere", "params": {"radius": 5, "center": [100, 0, 0]}, "expect": {"bodies": 7}});
    let r = raw_tool(&mut s, "batch", json!({"commands": [sphere]}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((body["completed"].clone(), body["failed"]["index"].clone()), (json!(0), json!(0)), "{body}");
    assert!(body["failed"]["error"].as_str().is_some_and(|e| e.contains("`expect` is the batch's argument")), "{body}");
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 1, "the sphere did not run");
}

/// A volume the design does not match flags the reply as an error that names the mismatch
/// first, and keeps the design as built: the agent decides what to do with it.
#[test]
fn a_batch_with_a_wrong_expected_volume_is_flagged_and_the_design_kept() {
    let mut s = server();
    let box40 = json!({"command": "solid.box", "params": {"length": 40, "width": 30, "height": 20, "corner": [0, 0, 0]}});
    let r = raw_tool(&mut s, "batch", json!({"commands": [box40], "expect": {"volume_mm3": 20000}}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    let content = r["result"]["content"].as_array().unwrap();
    let first = content[0]["text"].as_str().unwrap();
    assert!(first.starts_with("expectation not met: volume: measured 24000, expected 20000 (+20.00 %)."), "{first}");
    assert!(first.contains("kept as built"), "{first}");
    let body: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["ok"], true, "the batch itself went through: {body}");
    assert_eq!(body["completed"], 1, "{body}");
    assert_eq!(body["results"][0]["created_bodies"], json!(["Body1"]), "{body}");
    assert_eq!(body["expectation"]["ok"], false, "{body}");
    assert_eq!(body["expectation"]["mismatch"], json!(["volume: measured 24000, expected 20000 (+20.00 %)"]), "{body}");
    assert_eq!(body["expectation"]["expected"], json!({"volume_mm3": 20000.0, "tol": 0.002}), "{body}");
    // The design is kept as built, undoably.
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 1);
    assert!(rel(volume(&mut s), 24000.0) < 1e-9);
    assert_eq!(tool(&mut s, "inspect_design", json!({"measure": false}))["undo"], 1);
    // `tol` is relative to the expected volume: 0.5 % off passes at 1 %, not at the default 0.2 %.
    let cube = json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "corner": [100, 0, 0]}});
    let r = tool(&mut s, "batch", json!({"commands": [cube], "expect": {"volume_mm3": 25000.0 * 1.005, "tol": 0.01}}));
    assert_eq!(r["expectation"]["ok"], true, "{r}");
    let r = raw_tool(&mut s, "batch", json!({"commands": [{"command": "undo"}], "expect": {"volume_mm3": 24000.0 * 1.005}}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("volume: measured 24000, expected 24120 (-0.50 %)"), "{r}");
}

/// A wrong body count and a wrong bounding box are each named, with the coordinates that are
/// off; a bounding box within 0.05 mm matches.
#[test]
fn expect_names_body_count_and_bounding_box_mismatches() {
    let mut s = server();
    let box40 = json!({"command": "solid.box", "params": {"length": 40, "width": 30, "height": 20, "corner": [0, 0, 0]}});
    let cube = json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "corner": [100, -5, -5]}});
    let r = raw_tool(
        &mut s,
        "batch",
        json!({"commands": [box40, cube], "expect": {"volume_mm3": 25000, "bodies": 1, "bbox": {"min": [0, 0, 0], "max": [40, 30, 20]}}}),
    );
    assert_eq!(r["result"]["isError"], true, "{r}");
    let body: Value = serde_json::from_str(r["result"]["content"][1]["text"].as_str().unwrap()).unwrap();
    let mismatch = body["expectation"]["mismatch"].as_array().unwrap();
    assert_eq!(mismatch.len(), 2, "the volume matches and is not named: {body}");
    assert_eq!(mismatch[0], "bodies: measured 2, expected 1", "{body}");
    let bbox = mismatch[1].as_str().unwrap();
    assert!(bbox.starts_with("bbox: measured min [0, -5, -5] max [110, 30, 20], expected min [0, 0, 0] max [40, 30, 20] ("), "{bbox}");
    assert!(bbox.contains("min y off by -5 mm") && bbox.contains("min z off by -5 mm") && bbox.contains("max x off by 70 mm"), "{bbox}");
    assert!(!bbox.contains("max y") && !bbox.contains("max z") && !bbox.contains("min x"), "{bbox}");
    assert_eq!(body["expectation"]["measured"]["bodies"], 2, "{body}");
    assert!(rel(body["expectation"]["measured"]["bbox"]["max"][0].as_f64().unwrap(), 110.0) < 1e-6, "{body}");
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 2, "the design is kept");
    // Within 0.05 mm the box matches; past it the coordinate is named.
    tool(&mut s, "undo", json!({}));
    let r = tool(&mut s, "batch", json!({"commands": [{"command": "redo"}], "expect": {"bbox": {"min": [0, -5, -5.04], "max": [110.04, 30, 20]}}}));
    assert_eq!(r["expectation"]["ok"], true, "{r}");
    let r = raw_tool(&mut s, "batch", json!({"commands": [{"command": "undo"}], "expect": {"bbox": {"min": [0, 0, 0], "max": [40.1, 30, 20]}}}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("(max x off by -0.1 mm)"), "{r}");
    // No body at all: the box can't be measured, and the count says so.
    tool(&mut s, "new_design", json!({}));
    let r = raw_tool(
        &mut s,
        "batch",
        json!({"commands": [{"command": "parameters.add", "params": {"name": "w", "expression": "1 mm"}}], "expect": {"bodies": 1, "bbox": {"min": [0, 0, 0], "max": [1, 1, 1]}}}),
    );
    assert_eq!(r["result"]["isError"], true, "{r}");
    let body: Value = serde_json::from_str(r["result"]["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        body["expectation"]["mismatch"],
        json!(["bodies: measured 0, expected 1", "bbox: no body to measure, expected min [0, 0, 0] max [1, 1, 1]"]),
        "{body}"
    );
    assert!(body["expectation"]["measured"]["bbox"].is_null(), "{body}");
}

/// An `expect` that is not what the schema says is refused before anything runs: a typo never
/// builds a design that then goes unchecked.
#[test]
fn a_malformed_expect_is_refused_before_anything_runs() {
    let mut s = server();
    let step = json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}});
    for name in ["execute", "batch"] {
        let args = |expect: Value| {
            let mut a = if name == "batch" { json!({"commands": [step]}) } else { step.clone() };
            a["expect"] = expect;
            a
        };
        // Not an object: the argument check, as for every tool argument.
        for bad in [json!(5), json!("24000"), json!([24000]), json!(true)] {
            let r = raw_tool(&mut s, name, args(bad.clone()));
            assert_eq!(r["error"]["code"], -32602, "{name} {bad}: {r}");
            assert!(r["error"]["message"].as_str().unwrap().contains("`expect` must be of type"), "{r}");
        }
        // An object the tool can't use: a tool error naming the key, and nothing was run.
        for (bad, says) in [
            (json!({}), "nothing to check"),
            (json!({"volume": 24000}), "unknown key `volume`"),
            (json!({"volume_mm3": "24000"}), "`volume_mm3` must be a number"),
            (json!({"volume_mm3": -1}), "`volume_mm3` must not be negative"),
            (json!({"volume_mm3": 1, "tol": -0.1}), "`tol` must not be negative"),
            (json!({"bodies": 1.5}), "`bodies` must be a whole number"),
            (json!({"bodies": -1}), "`bodies` must be a whole number"),
            (json!({"bbox": [0, 0, 0]}), "`bbox` must be"),
            (json!({"bbox": {"min": [0, 0], "max": [1, 1, 1]}}), "`bbox.min` must be [x, y, z]"),
            (json!({"bbox": {"min": [0, 0, 0], "max": [1, "1", 1]}}), "`bbox.max` must be [x, y, z]"),
            (json!({"bbox": {"min": [2, 0, 0], "max": [1, 1, 1]}}), "`bbox.min` must not exceed"),
            (json!({"bbox": {"min": [0, 0, 0], "max": [1, 1, 1], "mid": [0, 0, 0]}}), "unknown key `bbox.mid`"),
        ] {
            let r = raw_tool(&mut s, name, args(bad.clone()));
            assert_eq!(r["result"]["isError"], true, "{name} {bad}: {r}");
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.starts_with("expect: ") && text.contains(says), "{name} {bad}: {text}");
        }
        assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 0, "{name}: nothing ran");
        assert_eq!(tool(&mut s, "inspect_design", json!({"measure": false}))["undo"], 0, "{name}: nothing ran");
    }
}

/// `execute` takes `expect` as `batch` does: met, the reply carries the expectation; not met,
/// the reply is an error naming it, and the command's result stays, as the design does. A
/// command that fails is the error it always was, with nothing measured.
#[test]
fn execute_checks_its_expect_like_batch() {
    let mut s = server();
    let r = tool(
        &mut s,
        "execute",
        json!({
            "command": "solid.box", "params": {"length": 40, "width": 30, "height": 20, "corner": [0, 0, 0]},
            "expect": {"volume_mm3": 24000, "bodies": 1, "bbox": {"min": [0, 0, 0], "max": [40, 30, 20]}},
        }),
    );
    assert_eq!(r["created_bodies"], json!(["Body1"]), "{r}");
    assert_eq!(r["expectation"]["ok"], true, "{r}");
    assert_eq!(r["expectation"]["mismatch"], json!([]), "{r}");
    let r = raw_tool(
        &mut s,
        "execute",
        json!({
            "command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "corner": [15, 10, 5], "operation": "cut"},
            "verbose": false,
            "expect": {"volume_mm3": 24000, "bodies": 2},
        }),
    );
    assert_eq!(r["result"]["isError"], true, "{r}");
    let content = r["result"]["content"].as_array().unwrap();
    let first = content[0]["text"].as_str().unwrap();
    assert!(first.starts_with("expectation not met: volume: measured 23000, expected 24000 (-4.17 %); bodies: measured 1, expected 2."), "{first}");
    let body: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["changed_bodies"], json!(["Body1"]), "the command's result stays: {body}");
    assert!(body.get("bodies").is_none(), "compact, as asked: {body}");
    assert_eq!(body["expectation"]["measured"]["bodies"], 1, "{body}");
    assert!(rel(volume(&mut s), 23000.0) < 1e-9, "the cut is kept");
    let r = raw_tool(&mut s, "execute", json!({"command": "solid.extrude", "params": {"distance": 10}, "expect": {"bodies": 1}}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(r["result"]["content"].as_array().map(Vec::len), Some(1), "{r}");
    assert!(!r["result"]["content"][0]["text"].as_str().unwrap().contains("expectation"), "{r}");
    assert!(rel(volume(&mut s), 23000.0) < 1e-9);
}

#[test]
fn helper_sketches_can_be_hidden() {
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 40, "width": 40, "height": 2}}));
    let sk = tool(&mut s, "execute", json!({"command": "sketch.create", "params": {"plane": "XY"}}))["sketch"].clone();
    tool(&mut s, "execute", json!({"command": "sketch.line", "params": {"points": [[0, 0], [40, 0]], "construction": true}}));
    tool(&mut s, "execute", json!({"command": "sketch.finish"}));
    let visible = |s: &mut Server| tool(s, "inspect_design", json!({"measure": false}))["sketches"][0]["visible"].clone();
    assert_eq!(visible(&mut s), true);
    tool(&mut s, "execute", json!({"command": "browser.visibility", "params": {"items": [sk], "visible": false}}));
    assert_eq!(visible(&mut s), false);
    let r = raw_tool(&mut s, "execute", json!({"command": "browser.visibility", "params": {"items": ["Nope"], "visible": false}}));
    assert_eq!(r["result"]["isError"], true);
    tool(&mut s, "execute", json!({"command": "browser.visibility", "params": {"folder": "sketches", "visible": true}}));
    assert_eq!(visible(&mut s), true);
}

#[test]
fn unknown_tools_and_bad_arguments_are_clean_errors() {
    let mut s = server();
    let r = raw_tool(&mut s, "make_coffee", json!({}));
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("make_coffee"));
    assert_eq!(raw_tool(&mut s, "execute", json!({}))["error"]["code"], -32602);
    assert_eq!(raw_tool(&mut s, "execute", json!({"command": 5}))["error"]["code"], -32602);
    assert_eq!(raw_tool(&mut s, "measure", json!({"bogus": 1}))["error"]["code"], -32602);
    assert_eq!(raw_tool(&mut s, "screenshot", json!({"source": "sideways"}))["error"]["code"], -32602);
    // `view` also takes saved named views, so an unknown name is a tool error naming it.
    let r = raw_tool(&mut s, "screenshot", json!({"view": "sideways"}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("sideways"), "{r}");
    assert_eq!(call(&mut s, 1, "tools/call", json!({"arguments": {}}))["error"]["code"], -32602);
    // Engine errors are tool errors the model can read.
    let r = raw_tool(&mut s, "execute", json!({"command": "solid.extrude", "params": {"distance": 10}}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    let r = raw_tool(&mut s, "execute", json!({"command": "NoSuchCommand"}));
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("unknown command"));
    // Framing errors.
    assert!(s.handle_line("{not json").unwrap().contains("-32700"));
    assert!(s.handle_line("[]").unwrap().contains("-32600"));
    assert!(s.handle_line("42").unwrap().contains("-32600"));
    assert!(s.handle_line("   ").is_none());
    assert!(s.handle_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).is_none());
    let both = s.handle_line(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/x"}]"#).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&both).unwrap().as_array().unwrap().len(), 1);
}

#[test]
fn tool_unknown_command_errors_suggest_close_ids() {
    let mut s = server();
    for id in ["solid.bxo", "SOLID_BOX", "solid-box", "solid box", "madeup.box"] {
        for name in ["execute", "batch"] {
            let step = json!({"command": id});
            let args = if name == "batch" { json!({"commands": [step]}) } else { step };
            let r = raw_tool(&mut s, name, args);
            assert_eq!(r["result"]["isError"], true, "{r}");
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("closest: `solid.box`"), "{text}");
        }
    }
    let r = raw_tool(&mut s, "execute", json!({"command": "solid.box"}));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("solid.box") && text.contains("length"), "{text}");
}

#[test]
fn tool_unknown_command_errors_explain_packed_params() {
    let mut s = server();
    for id in ["solid.cube {side: 20}", "solid.box(20)"] {
        for name in ["execute", "batch"] {
            let step = json!({"command": id});
            let args = if name == "batch" { json!({"commands": [step]}) } else { step };
            let r = raw_tool(&mut s, name, args);
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("command ids have no parameters; pass them in `params`"), "{r}");
        }
    }
    let reply = json!({"completed": 0, "results": [], "failed": {"index": 0, "command": "solid.cube {side: 20}", "error": "unknown command `solid.cube {side: 20}`"}, "undone": 0});
    let (_, body, error) = recorded_batch(Ok(reply), json!({"commands": [{"command": "solid.cube {side: 20}"}]}));
    assert!(error);
    assert!(body["failed"]["error"].as_str().unwrap().contains("command ids have no parameters; pass them in `params`"), "{body}");
}

/// #10678: through direct `execute`, an unknown command id that is exactly a registered
/// MCP tool name gets the namespace hint instead of unrelated closest-command suggestions.
#[test]
fn tool_name_command_ids_get_namespace_hint_via_execute() {
    let mut s = server();
    for id in ["execute", "list_commands", "describe_command", "screenshot"] {
        let r = raw_tool(&mut s, "execute", json!({"command": id, "params": {}}));
        assert_eq!(r["result"]["isError"], true, "{r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with(&format!("unknown command `{id}`")), "{text}");
        assert!(text.contains(&format!("`{id}` is an MCP tool, not an engine command id")), "{text}");
        assert!(text.contains("tools/list") && text.contains("list_commands"), "{text}");
        assert!(!text.contains("closest:"), "{text}");
    }
}

/// #10678: the direct `describe_command` hint inspects the requested `id`, not the outer
/// `engine.describe` command `exec` sees.
#[test]
fn tool_name_command_ids_get_namespace_hint_via_describe_command() {
    let mut s = server();
    for id in ["execute", "list_commands"] {
        let r = raw_tool(&mut s, "describe_command", json!({"id": id}));
        assert_eq!(r["result"]["isError"], true, "{r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with(&format!("unknown command `{id}`")), "{text}");
        assert!(text.contains(&format!("`{id}` is an MCP tool, not an engine command id")), "{text}");
        assert!(!text.contains("engine.describe"), "{text}");
    }
    // Control: a valid engine command id still describes itself.
    let r = raw_tool(&mut s, "describe_command", json!({"id": "solid.box"}));
    assert_ne!(r["result"]["isError"], true, "{r}");
}

/// #10678: an engine-id typo keeps its closest-command suggestions; the tool-name hint is
/// only for exact registered tool names.
#[test]
fn engine_command_id_typo_still_gets_closest_suggestions() {
    let mut s = server();
    let r = raw_tool(&mut s, "execute", json!({"command": "solid.bxo"}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("closest: `solid.box`"), "{text}");
    assert!(!text.contains("MCP tool"), "{text}");
}

/// #10678: a genuinely unknown id keeps the ordinary unknown-command error on both paths.
#[test]
fn genuinely_unknown_command_id_keeps_the_ordinary_error() {
    let mut s = server();
    for (name, args) in [("execute", json!({"command": "madeup.thing"})), ("describe_command", json!({"id": "madeup.thing"}))] {
        let r = raw_tool(&mut s, name, args);
        assert_eq!(r["result"]["isError"], true, "{r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("unknown command `madeup.thing`"), "{text}");
        assert!(!text.contains("MCP tool"), "{text}");
    }
}

/// #10678: a valid engine command id that overlaps a tool name resolves as a command —
/// the engine accepts it before any tool-name hint could apply.
#[test]
fn valid_engine_command_id_overlapping_a_tool_name_resolves_as_command() {
    let mut b = Recorder { calls: Vec::new(), batch: Ok(json!({})) };
    let r = crate::tools::call_tool(&mut b, "execute", &json!({"command": "measure"}));
    assert_ne!(r["isError"], json!(true), "{r}");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("measure"), "{r}");
    let r = crate::tools::call_tool(&mut b, "describe_command", &json!({"id": "measure"}));
    assert_ne!(r["isError"], json!(true), "{r}");
}

fn hostile_values() -> Vec<Value> {
    vec![
        Value::Null,
        json!(true),
        json!(-1),
        json!(0),
        json!(1e308),
        json!(-1e308),
        json!(""),
        json!("nope"),
        json!("../../../../../../nonexistent/dir/x.step"),
        json!("1/0"),
        json!([]),
        json!([1e308, -1e308]),
        json!([{"command": "solid.extrude", "params": {"distance": 1e308}}]),
        json!([{"command": 7}, null, "x"]),
        json!({}),
        json!({"x": 1}),
    ]
}

/// Every tool, every argument it documents, every hostile value: replies are well-formed
/// JSON-RPC and nothing panics.
#[test]
fn hostile_arguments_never_panic() {
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    let defs = tool_definitions();
    let mut n = 0;
    for def in &defs {
        let name = def["name"].as_str().unwrap();
        // Hostile strings are relative paths: don't let the sweep write files into the crate.
        let writes = matches!(name, "save" | "screenshot" | "export");
        let keys: Vec<String> = def["inputSchema"]["properties"].as_object().unwrap().keys().filter(|k| !(writes && *k == "path")).cloned().collect();
        for k in &keys {
            for v in hostile_values() {
                let mut args = json!({});
                // Fill the other required arguments with something plausible.
                for r in def["inputSchema"]["required"].as_array().into_iter().flatten() {
                    args[r.as_str().unwrap()] = json!("Body1");
                }
                args[k.as_str()] = v;
                if name == "screenshot" && (k == "width" || k == "height") {
                    args["view"] = json!("front");
                }
                let r = raw_tool(&mut s, name, args.clone());
                assert!(r.get("result").is_some() || r["error"]["code"].is_i64(), "{name} {args}: {r}");
                n += 1;
            }
        }
    }
    assert!(n > 100);
    // Execute every command with hostile params through MCP too.
    let cmds = tool(&mut s, "list_commands", json!({}));
    for c in cmds.as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        if matches!(id, "doc.open" | "file.save" | "file.save_as" | "file.export" | "file.save_mesh") {
            continue;
        }
        for v in hostile_values() {
            let r = raw_tool(&mut s, "execute", json!({"command": id, "params": {"distance": v.clone(), "edges": v.clone(), "plane": v}}));
            assert!(r.get("result").is_some(), "{id}: {r}");
        }
    }
    // Arguments that are not an object.
    for v in hostile_values().into_iter().filter(|v| !v.is_object() && !v.is_null()) {
        let r = call(&mut s, 1, "tools/call", json!({"name": "measure", "arguments": v}));
        assert_eq!(r["error"]["code"], -32602, "{r}");
    }
}

#[test]
fn base64_matches_rfc4648() {
    use crate::tools::base64;
    for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
        assert_eq!(base64(i.as_bytes()), o);
    }
}

/// Filtering by a command id returns that command, not every command whose docs mention it (#269).
#[test]
fn list_commands_filter_by_a_command_id_matches_ids() {
    let mut s = server();
    let cmds = tool(&mut s, "list_commands", json!({"filter": "sketch.delete"}));
    let ids: Vec<&str> = cmds.as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).collect();
    assert!(ids.contains(&"sketch.delete"), "{ids:?}");
    assert!(ids.iter().all(|id| id.contains("sketch.delete")), "{} commands matched: {ids:?}", ids.len());
}

/// #11726: `list_commands` narrows by id prefix and by panel, shrinks each entry to id and label
/// or to chosen keys, and pages; with none of these the reply is the whole catalog as before. A
/// batch step narrows the same way, and a bad argument is a tool error that names it.
#[test]
fn list_commands_narrows_by_prefix_panel_fields_and_page() {
    let mut s = server();
    let all = tool(&mut s, "list_commands", json!({}));
    let all = all.as_array().unwrap();
    assert!(all.len() > 100 && all[0].get("params").is_some(), "the whole catalog, every key");
    let ids = |v: &Value| v.as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let sketch = tool(&mut s, "list_commands", json!({"prefix": "SKETCH."}));
    assert!(!ids(&sketch).is_empty() && ids(&sketch).iter().all(|id| id.starts_with("sketch.")), "{:?}", ids(&sketch));
    let want = all.iter().filter(|c| c["id"].as_str().unwrap().starts_with("sketch.")).count();
    assert_eq!(ids(&sketch).len(), want, "every sketch command, not only those on a toolbar");
    let modify = tool(&mut s, "list_commands", json!({"panel": "modify"}));
    assert!(!ids(&modify).is_empty(), "{modify}");
    assert!(
        modify
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["panel"].as_str().unwrap().eq_ignore_ascii_case("modify") || c["tab"].as_str().unwrap().eq_ignore_ascii_case("modify")),
        "only the whole panel name matches"
    );
    let compact = tool(&mut s, "list_commands", json!({"compact": true}));
    assert_eq!(ids(&compact), ids(&Value::Array(all.clone())), "compact keeps every command");
    assert!(compact.as_array().unwrap().iter().all(|c| c.as_object().unwrap().keys().eq(["id", "label"].iter())), "{}", compact[0]);
    assert!(compact.to_string().len() * 5 < Value::Array(all.clone()).to_string().len(), "compact is far smaller");
    let picked = tool(&mut s, "list_commands", json!({"prefix": "solid.fillet", "fields": ["id", "enabled"], "compact": true}));
    assert_eq!(picked[0], json!({"id": "solid.fillet", "enabled": picked[0]["enabled"]}), "fields wins over compact");
    // Pages: together they are the whole match list, in order.
    let mut paged = Vec::new();
    let mut offset = json!(0);
    while let Some(at) = offset.as_u64() {
        let page = tool(&mut s, "list_commands", json!({"prefix": "sketch.", "compact": true, "limit": 7, "offset": at}));
        assert_eq!(page["total"], want, "{page}");
        assert_eq!(page["offset"], at);
        assert!(page["commands"].as_array().unwrap().len() <= 7);
        paged.extend(ids(&page["commands"]));
        offset = page["next_offset"].clone();
    }
    assert_eq!(paged, ids(&sketch));
    let past = tool(&mut s, "list_commands", json!({"limit": 5, "offset": 1_000_000}));
    assert_eq!((past["commands"].as_array().unwrap().len(), &past["next_offset"]), (0, &Value::Null), "{past}");
    // A batch step narrows as the tool does.
    let r = tool(&mut s, "batch", json!({"commands": [{"command": "list_commands", "params": {"prefix": "sketch.", "compact": true, "limit": 3}}]}));
    let step = r["results"][0].clone();
    assert_eq!(step["commands"].as_array().map(Vec::len), Some(3), "{r}");
    assert_eq!(step["total"], want, "{r}");
    for bad in [
        json!({"limit": 0}),
        json!({"limit": -1}),
        json!({"offset": "x"}),
        json!({"fields": ["nope"]}),
        json!({"fields": "id"}),
        json!({"compact": 1}),
        json!({"prefix": 3}),
    ] {
        // The argument schema refuses a wrong type before the tool runs; the tool refuses the rest.
        let r = raw_tool(&mut s, "list_commands", bad.clone());
        let text = r["result"]["content"][0]["text"].as_str().or(r["error"]["message"].as_str()).unwrap_or_default().to_string();
        assert!(r["result"]["isError"] == json!(true) || r["error"]["code"].is_i64(), "{bad}: {r}");
        let key = bad.as_object().unwrap().keys().next().unwrap().clone();
        assert!(text.contains(&format!("`{key}`")), "{bad}: the error names the argument: {text}");
        let r = raw_tool(&mut s, "batch", json!({"commands": [{"command": "list_commands", "params": bad.clone()}]}));
        assert!(r["result"]["isError"] == json!(true) || r.to_string().contains(&format!("`{key}`")), "{bad} as a step: {r}");
    }
}

/// Compatibility shims (docs/compatibility.md): a design saved before the rename (legacy
/// extension and format tag) opens through MCP, and the legacy resource URIs still read.
#[test]
fn legacy_design_files_and_resource_uris() {
    let mut s = server();
    let saved = String::from_utf8(solidcraft_engine::io::write_design(&solidcraft_engine::doc::Document::default())).unwrap();
    assert!(saved.contains("\"solidcraft/"), "{saved}");
    let legacy = saved.replace("\"solidcraft/", "\"solvecraft/");
    let path = std::env::temp_dir().join(format!("solidcraft-mcp-legacy-{}.solvecraft", std::process::id()));
    std::fs::write(&path, legacy).unwrap();
    tool(&mut s, "open", json!({"path": path.to_string_lossy()}));
    let _ = std::fs::remove_file(&path);
    let d = call(&mut s, 1, "resources/read", json!({"uri": "solvecraft://design"}));
    assert!(d["result"]["contents"][0]["text"].as_str().unwrap().contains("timeline"));

    // `.sc` is what new designs are saved as; a `.solidcraft` file opens and `save {}` writes it
    // back under its own name.
    let dir = std::env::temp_dir().join(format!("solidcraft-mcp-ext-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (new, old) = (dir.join("part.sc"), dir.join("part.solidcraft"));
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    tool(&mut s, "save", json!({"path": new.to_string_lossy()}));
    assert!(new.is_file());
    std::fs::rename(&new, &old).unwrap();
    tool(&mut s, "open", json!({"path": old.to_string_lossy()}));
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 5, "width": 5, "height": 5}}));
    tool(&mut s, "save", json!({}));
    assert!(old.is_file() && !new.exists(), "save keeps the opened file's name");
    assert_eq!(solidcraft_engine::io::read_design(&std::fs::read(&old).unwrap()).unwrap().features.len(), 2);
    tool(&mut s, "open", json!({"path": old.to_string_lossy()}));
    tool(&mut s, "save", json!({"path": new.to_string_lossy()}));
    tool(&mut s, "open", json!({"path": new.to_string_lossy()}));
    assert!(old.is_file() && new.is_file(), "save as leaves the old file");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `compare_picture` (#1486): the overlay comes back as an image block and the numbers as text.
#[test]
fn compare_picture_returns_the_overlay_as_an_image() {
    use image::ImageEncoder as _;
    use std::io::Cursor;
    // A scoped server, like `solidcraft-cli mcp`: the reference must be inside the folders.
    let base = solidcraft_engine::io::vfs::temp_dir().join(format!("solidcraft-mcp-compare-{}", std::process::id()));
    let recovery = base.join("recovery");
    std::fs::create_dir_all(&recovery).unwrap();
    let scope = crate::agent_scope_with(Some(recovery.clone()), []);
    let mut s = Server::new(Box::new(Headless::default())).with_path_scope(Some(scope));
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 20, "width": 10, "height": 30}}));
    // A white rectangle on grey with the box's front-view proportions (20 x 30 mm).
    let (w, h) = (200u32, 300u32);
    let mut img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 200, 200, 255]));
    for y in 20..280 {
        for x in 20..180 {
            img.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
        }
    }
    let mut png = Cursor::new(Vec::new());
    image::codecs::png::PngEncoder::new(&mut png).write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8).unwrap();
    let path = recovery.join("target.png");
    std::fs::write(&path, png.into_inner()).unwrap();

    let r = raw_tool(&mut s, "compare_picture", json!({"reference": path.to_string_lossy(), "view": "front"}));
    assert_ne!(r["result"]["isError"], json!(true), "{r}");
    let content = r["result"]["content"].as_array().unwrap().clone();
    assert_eq!(content[0]["type"], "image", "{content:?}");
    assert_eq!(content[0]["mimeType"], "image/png");
    assert!(content[0]["data"].as_str().unwrap().starts_with("iVBORw0KGgo"), "not a PNG");
    let info: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    assert!(info["overlap"].as_f64().unwrap() > 0.9, "{info}");
    assert!(info.get("overlay_png").is_none(), "the base64 blob belongs in the image block only");
    // A reference outside the server's folders is refused through the existing path rules.
    let outside = raw_tool(&mut s, "compare_picture", json!({"reference": "/etc/hostname"}));
    assert_eq!(outside["result"]["isError"], json!(true), "{outside}");
    let _ = std::fs::remove_dir_all(&base);
}

/// The default folders (#205): the recovery folder works out of the box, `screenshot` without a
/// `path` uses the server's own scratch folder, and the open design's folder counts while the
/// design is open there, without its hidden files. The working folder doesn't count at all.
#[test]
fn the_default_folders_are_the_designs_the_recovery_folder_and_the_scratch_folder() {
    let base = solidcraft_engine::io::vfs::temp_dir().join(format!("solidcraft-mcp-defaults-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (proj, other, recovery) = (base.join("proj"), base.join("other"), base.join("recovery"));
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(proj.join(".env"), "TOKEN=secret").unwrap();
    let at = |d: &std::path::Path, f: &str| d.join(f).to_string_lossy().to_string();
    let scope = crate::agent_scope_with(Some(recovery.clone()), []);
    let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    assert!(!scope.roots().contains(&cwd), "the working folder is not a default folder: {scope:?}");
    assert!(scope.roots().contains(&std::fs::canonicalize(&recovery).unwrap()), "{scope:?}");
    assert!(scope.roots().contains(&std::fs::canonicalize(crate::scratch_dir().unwrap()).unwrap()), "{scope:?}");

    let mut session = solidcraft_engine::Session::default();
    session.execute("solid.box", &json!({"length": 10, "width": 10, "height": 10})).unwrap();
    session.execute("file.save_as", &json!({"path": at(&proj, "part.solidcraft")})).unwrap();
    let mut s = Server::new(Box::new(Headless::new(session))).with_path_scope(Some(scope));
    // Legitimate work in the design's folder: export, save, open, screenshots, the recovery folder.
    tool(&mut s, "export", json!({"path": at(&proj, "part.step")}));
    tool(&mut s, "execute", json!({"command": "parameters.export", "params": {"path": at(&proj, "params.csv")}}));
    tool(&mut s, "save", json!({"path": at(&proj, "copy.solidcraft")}));
    tool(&mut s, "open", json!({"path": at(&proj, "part.solidcraft")}));
    if renders() {
        tool(&mut s, "screenshot", json!({"width": 32, "height": 32}));
    }
    tool(&mut s, "execute", json!({"command": "doc.recovery_list", "params": {"dir": recovery.to_string_lossy()}}));
    assert!(proj.join("part.step").exists() && proj.join("copy.solidcraft").exists());
    // Hidden files in it, and folders beside it, stay out of reach.
    for (name, args) in [
        ("export", json!({"path": at(&proj, ".env")})),
        ("execute", json!({"command": "parameters.import", "params": {"path": at(&proj, ".env")}})),
        ("export", json!({"path": at(&proj, ".git/x.step")})),
        ("export", json!({"path": at(&other, "x.step")})),
    ] {
        let r = raw_tool(&mut s, name, args);
        assert_eq!(r["result"]["isError"], json!(true), "{name}: {r}");
    }
    assert_eq!(std::fs::read_to_string(proj.join(".env")).unwrap(), "TOKEN=secret");
    assert_eq!(std::fs::read_dir(&other).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&base);
}

/// #7181: the folder the agent opened or saved a design in stays one of its folders when the
/// design moves on (a new design, a STEP file opened from there, a save-as into a subfolder), so
/// a later `save` beside it works. Its hidden files and the folders beside it stay out of reach,
/// and a refused call adds nothing.
#[test]
fn a_folder_the_agent_saved_a_design_in_stays_its_folder() {
    use solidcraft_engine::io::vfs::PathScope;
    let base = solidcraft_engine::io::vfs::temp_dir().join(format!("solidcraft-mcp-remembered-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (proj, other) = (base.join("proj"), base.join("other"));
    std::fs::create_dir_all(proj.join("sub")).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let at = |d: &std::path::Path, f: &str| d.join(f).to_string_lossy().to_string();
    let mut session = solidcraft_engine::Session::default();
    session.execute("solid.box", &json!({"length": 10, "width": 10, "height": 10})).unwrap();
    session.execute("file.save_as", &json!({"path": at(&proj, "part.solidcraft")})).unwrap();
    let mut s = Server::new(Box::new(Headless::new(session))).with_path_scope(Some(PathScope::new([])));
    tool(&mut s, "export", json!({"path": at(&proj, "part.step")}));
    tool(&mut s, "new_design", json!({}));
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 5, "width": 5, "height": 5}}));
    tool(&mut s, "save", json!({"path": at(&proj, "part-2.solidcraft")}));
    // A STEP file opens as a new, unsaved design; it saves beside its file.
    tool(&mut s, "open", json!({"path": at(&proj, "part.step")}));
    tool(&mut s, "save", json!({"path": at(&proj, "part-3.solidcraft")}));
    // A save-as into a subfolder moves the design there; the folder above goes on counting.
    tool(&mut s, "execute", json!({"command": "file.save_as", "params": {"path": at(&proj.join("sub"), "part-4.solidcraft")}}));
    tool(&mut s, "save", json!({"path": at(&proj, "part-5.solidcraft")}));
    for f in ["part-2", "part-3", "sub/part-4", "part-5"] {
        assert!(proj.join(format!("{f}.solidcraft")).exists(), "{f}");
    }
    // Twice: the first refusal must not have added anything.
    for _ in 0..2 {
        let r = raw_tool(&mut s, "export", json!({"path": at(&proj, ".env")}));
        assert_eq!(r["result"]["isError"], json!(true), "{r}");
        let r = raw_tool(&mut s, "save", json!({"path": at(&other, "x.solidcraft")}));
        assert!(r.to_string().contains("outside the folders this agent may use"), "{r}");
    }
    assert!(!proj.join(".env").exists());
    assert_eq!(std::fs::read_dir(&other).unwrap().count(), 0);
    // A link beside it to a design here counts where it leads, never where it lies.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(proj.join("part.step"), other.join("link.step")).unwrap();
        std::os::unix::fs::symlink(proj.join("part.solidcraft"), other.join("link.solidcraft")).unwrap();
        tool(&mut s, "open", json!({"path": at(&other, "link.step")}));
        tool(&mut s, "open", json!({"path": at(&other, "link.solidcraft")}));
        let r = raw_tool(&mut s, "save", json!({"path": at(&other, "x.solidcraft")}));
        assert!(r.to_string().contains("outside the folders this agent may use"), "{r}");
        assert!(!other.join("x.solidcraft").exists());
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Tool calls reach files only inside the server's folders (#205): the usual workflow (save,
/// export, open, screenshots) works there; a path outside, directly, through `..` or through a
/// symlink that leads out, is refused with a clear error and nothing is written.
#[test]
fn tool_calls_stay_inside_the_agents_folders() {
    use solidcraft_engine::io::vfs::PathScope;
    let base = solidcraft_engine::io::vfs::temp_dir().join(format!("solidcraft-mcp-scope-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (root, out) = (base.join("root"), base.join("out"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let at = |d: &std::path::Path, f: &str| d.join(f).to_string_lossy().to_string();
    let defaults = crate::agent_scope_with(None, [root.clone()]);
    assert!(defaults.roots().contains(&std::fs::canonicalize(&root).unwrap()), "{defaults:?}");
    assert!(!defaults.roots().contains(&std::fs::canonicalize(std::env::temp_dir()).unwrap()), "{defaults:?}");
    // Only `root` here, to tell inside from outside within the temporary folder.
    let mut s = Server::new(Box::new(Headless::default())).with_path_scope(Some(PathScope::new([root.clone()])));
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    tool(&mut s, "save", json!({"path": at(&root, "part.solidcraft")}));
    tool(&mut s, "export", json!({"path": at(&root, "part.step")}));
    tool(&mut s, "open", json!({"path": at(&root, "part.solidcraft")}));
    if renders() {
        tool(&mut s, "screenshot", json!({"path": at(&root, "shot.png"), "width": 64, "height": 64}));
    }
    let escapes = vec![at(&out, "x.step"), at(&root, "../out/x.step")];
    #[cfg(unix)]
    let mut escapes = escapes;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&out, root.join("link")).unwrap();
        escapes.push(at(&root, "link/x.step"));
    }
    for p in escapes {
        for (name, args) in [
            ("export", json!({"path": p})),
            ("save", json!({"path": p})),
            ("open", json!({"path": p})),
            ("screenshot", json!({"path": p, "width": 64, "height": 64})),
            ("execute", json!({"command": "file.save_as", "params": {"path": p}})),
            ("batch", json!({"commands": [{"command": "file.export", "params": {"path": p}}]})),
        ] {
            let r = raw_tool(&mut s, name, args);
            assert_eq!(r["result"]["isError"], json!(true), "{name} {p}: {r}");
            assert!(r.to_string().contains("outside the folders this agent may use"), "{name} {p}: {r}");
        }
    }
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "nothing written outside");
    let _ = std::fs::remove_dir_all(&base);
}

/// The short (8.3) spelling of an existing `path` as `cmd` gives it (`NAME~1.XYZ`), or `None`
/// when it has no short name (the volume makes none).
#[cfg(windows)]
fn short_spelling(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("cmd").raw_arg(format!("/d /c for %I in (\"{}\") do @echo %~sI", path.display())).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| solidcraft_engine::io::vfs::windows_refusal(s).is_some_and(|why| why.contains("8.3")))
        .map(Into::into)
}

/// On Windows the temporary folder's path often has a short (8.3) name (the user folder's
/// `NAME~1.XYZ` when its name is long). The server's own folders are spelled with long names
/// there, so `screenshot` without a `path` (the scratch folder) and `doc.recovery_list` (the
/// recovery folder) work; on such a machine both were refused as short names. Run in a child
/// process whose `TMP`, `TEMP` and `SOLIDCRAFT_RECOVERY_DIR` are short spellings.
#[cfg(windows)]
#[test]
fn the_servers_own_folders_work_when_the_temporary_folder_has_a_short_name() {
    let base = solidcraft_engine::io::vfs::temp_dir().join(format!("solidcraft-mcp-short-names-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (temp, recovery) = (base.join("temporary"), base.join("recovery folder"));
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::create_dir_all(&recovery).unwrap();
    let (Some(short_temp), Some(short_recovery)) = (short_spelling(&temp), short_spelling(&recovery)) else {
        eprintln!("skipped: {} has no short (8.3) name on this volume", base.display());
        let _ = std::fs::remove_dir_all(&base);
        return;
    };
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::short_names_child", "--test-threads=1", "--nocapture"])
        .env("TMP", &short_temp)
        .env("TEMP", &short_temp)
        .env("SOLIDCRAFT_RECOVERY_DIR", &short_recovery)
        .env("SOLIDCRAFT_MCP_SHORT_NAMES_CHILD", "1")
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("short names: ok"), "{text}");
    let _ = std::fs::remove_dir_all(&base);
}

/// The child process of the test above; does nothing on its own.
#[cfg(windows)]
#[test]
fn short_names_child() {
    if std::env::var_os("SOLIDCRAFT_MCP_SHORT_NAMES_CHILD").is_none() {
        return;
    }
    use solidcraft_engine::io::vfs::windows_refusal;
    let temp = std::env::temp_dir();
    assert!(windows_refusal(&temp.to_string_lossy()).is_some_and(|why| why.contains("8.3")), "{}", temp.display());
    let mut s = Server::new(Box::new(Headless::default())).with_path_scope(Some(crate::agent_scope([])));
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    if renders() {
        tool(&mut s, "screenshot", json!({"width": 32, "height": 32}));
    }
    tool(&mut s, "execute", json!({"command": "doc.recovery_list", "params": {}}));
    // An agent's own short spelling of the scratch folder stays refused.
    let scratch = crate::scratch_dir().unwrap();
    let short = std::env::temp_dir().join(scratch.file_name().unwrap()).join("shot.png");
    let r = raw_tool(&mut s, "screenshot", json!({"path": short.to_string_lossy(), "width": 32, "height": 32}));
    assert!(r.to_string().contains("short (8.3) names"), "{r}");
    println!("short names: ok");
}

/// A bridged server sends the agent's folders with every request, so the app confines the
/// request's files the same way (`allow_paths`, crates/ui-egui/src/control.rs).
#[test]
fn a_bridged_request_carries_the_agents_folders() {
    use std::io::{BufRead, BufReader, Write};

    use solidcraft_engine::io::vfs::{PathScope, scoped};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let echo = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        let mut result = req["params"].clone();
        result["sent_token"] = req["token"].clone();
        writeln!(&stream, "{}", json!({"id": req["id"], "ok": true, "result": result})).unwrap();
    });
    // The app's control token goes with the request too (#205).
    let mut b = crate::Remote::connect_with_token(&addr, Some("0123456789abcdef0123456789abcdef")).unwrap();
    let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let got = scoped(Some(PathScope::new([dir.clone()])), || crate::Backend::call(&mut b, "engine.execute", json!({"command": "file.new"}))).unwrap();
    assert_eq!(got["allow_paths"], json!([dir.to_string_lossy()]), "{got}");
    assert_eq!(got["command"], "file.new");
    assert_eq!(got["sent_token"], "0123456789abcdef0123456789abcdef");
    echo.join().unwrap();
}

/// A server started before its first tool call keeps no connection waiting (the app closes one
/// that shows no token in time), and a call on a connection the app has closed meanwhile goes out
/// on a new one without an error.
#[test]
fn a_bridge_holds_no_idle_connection_and_survives_one_closed_before_its_first_call() {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = std::thread::spawn(move || {
        let (mut probe, _) = listener.accept().unwrap();
        probe.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        assert_eq!(probe.read(&mut [0u8; 1]).unwrap(), 0, "the probe closes at once and sends nothing");
        // The connection the second bridge makes first: closed unanswered, as a timed-out one is.
        drop(listener.accept().unwrap());
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            writeln!(&stream, "{}", json!({"id": req["id"], "ok": true, "result": {"method": req["method"]}})).unwrap();
        }
    });
    let token = "0123456789abcdef0123456789abcdef";
    let mut early = crate::Remote::new(&addr, Some(token));
    early.try_connect().unwrap();
    let mut idle = crate::Remote::connect_with_token(&addr, Some(token)).unwrap();
    let got = crate::Backend::call(&mut early, "ui.view", json!({})).unwrap();
    assert_eq!(got["method"], "ui.view", "{got}");
    let got = crate::Backend::call(&mut idle, "ui.view", json!({})).unwrap();
    assert_eq!(got["method"], "ui.view", "{got}");
    app.join().unwrap();
}

/// #7181, bridged: once the agent has saved a design through the app, that file's folder goes
/// with every later request as `design_folders`, so the app counts it after the design moves on.
/// Not before the save succeeded, not for one the app refused, not for a relative path (the app
/// resolves those against its own working folder).
#[test]
fn a_bridged_server_sends_the_folders_the_agent_saved_designs_in() {
    use solidcraft_engine::io::vfs::PathScope;
    let base = solidcraft_engine::io::vfs::temp_dir().join(format!("solidcraft-mcp-bridged-folders-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (proj, other) = (base.join("proj"), base.join("other"));
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let at = |d: &std::path::Path, f: &str| d.join(f).to_string_lossy().to_string();
    let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let (log, refused) = (sent.clone(), at(&other, "x.solidcraft"));
    let (addr, _) = scripted_app(move |req| {
        if req["method"] == "engine.execute" {
            log.lock().unwrap().push(req["params"]["design_folders"].clone());
        }
        if req["params"]["params"]["path"] == refused.as_str() {
            json!({"ok": false, "error": "outside the folders this agent may use"})
        } else {
            json!({"ok": true, "result": {}})
        }
    });
    let remote = crate::Remote::connect_with_token(&addr, Some("0123456789abcdef0123456789abcdef")).unwrap();
    let mut s = Server::new(Box::new(remote)).with_path_scope(Some(PathScope::new([])));
    let r = raw_tool(&mut s, "save", json!({"path": at(&other, "x.solidcraft")}));
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    tool(&mut s, "save", json!({"path": "relative/y.solidcraft"}));
    tool(&mut s, "save", json!({"path": at(&proj, "a.solidcraft")}));
    tool(&mut s, "new_design", json!({}));
    let proj = std::fs::canonicalize(&proj).unwrap();
    assert_eq!(*sent.lock().unwrap(), [Value::Null, Value::Null, Value::Null, json!([proj.to_string_lossy()])]);
    let _ = std::fs::remove_dir_all(&base);
}

/// #528: an agent saves the angle of a product photo as a named view once, then renders the
/// model from that view after each change. The render must use the saved camera, not a
/// standard view.
#[test]
fn screenshot_renders_a_saved_named_view() {
    if !renders() {
        return;
    }
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 40, "width": 20, "height": 10}}));
    let camera = json!({"target": [20, 10, 5], "yaw": 0.5, "pitch": -0.6, "distance": 150});
    tool(&mut s, "execute", json!({"command": "view.save", "params": {"name": "Photo", "camera": camera.clone()}}));
    let shot = |s: &mut Server, view: &str| {
        let img = tool(s, "screenshot", json!({"view": view, "width": 96, "height": 64}));
        img["data"].as_str().unwrap_or_default().to_string()
    };
    let iso = shot(&mut s, "iso");
    let photo = shot(&mut s, "Photo");
    assert!(photo.starts_with("iVBORw0KGgo"), "not a PNG");
    assert_ne!(photo, iso, "the named view rendered like the standard iso view");
    // The same camera given directly renders the same picture; a standard view turns back.
    let direct = tool(&mut s, "screenshot", json!({"camera": camera, "width": 96, "height": 64}));
    assert_eq!(direct["data"].as_str().unwrap_or_default(), photo);
    assert_eq!(shot(&mut s, "iso"), iso);
}

/// The ground grid lies on Z 0 and cuts through a part that straddles it: `grid: false` and
/// `axes: false` leave it and the origin's axes out of the render. An axis hidden with
/// `browser.visibility` stays out whatever `axes` says.
#[test]
fn screenshot_can_leave_the_grid_and_the_axes_out() {
    if !renders() {
        return;
    }
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 40, "width": 30, "height": 20, "corner": [10, -15, -10]}}));
    let shot = |s: &mut Server, extra: Value| {
        let mut args = json!({"view": "front", "width": 96, "height": 64});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        tool(s, "screenshot", args)["data"].as_str().unwrap_or_default().to_string()
    };
    let all = shot(&mut s, json!({}));
    let no_grid = shot(&mut s, json!({"grid": false}));
    let no_axes = shot(&mut s, json!({"axes": false}));
    let bare = shot(&mut s, json!({"grid": false, "axes": false}));
    assert!(all.starts_with("iVBORw0KGgo"), "not a PNG");
    let pictures = [&all, &no_grid, &no_axes, &bare];
    for (i, a) in pictures.iter().enumerate() {
        for b in &pictures[i + 1..] {
            assert_ne!(a, b, "two of the pictures are the same");
        }
    }
    // Asking for them is the default.
    assert_eq!(shot(&mut s, json!({"grid": true, "axes": true})), all);
    // Hiding the origin leaves the axes out, with or without `axes`; the grid is not the origin.
    tool(&mut s, "execute", json!({"command": "browser.visibility", "params": {"folder": "origin", "visible": false}}));
    assert_eq!(shot(&mut s, json!({})), no_axes);
    assert_eq!(shot(&mut s, json!({"axes": true})), no_axes);
    assert_eq!(shot(&mut s, json!({"grid": false})), bare);
    // Other than booleans are schema errors.
    for bad in [json!({"grid": "no"}), json!({"axes": 0})] {
        assert_eq!(raw_tool(&mut s, "screenshot", bad)["error"]["code"], -32602);
    }
}

/// Bridged to an app, `grid` and `axes` ask for a model render and travel with it; without them
/// the window is captured as before; the window cannot be asked to leave things out.
#[test]
fn bridged_screenshots_with_grid_or_axes_are_model_renders() {
    struct App {
        calls: Vec<(String, Value)>,
    }
    impl crate::backend::Backend for App {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.push((method.to_string(), params.clone()));
            match method {
                "ui.render" | "ui.screenshot" => {
                    let path = params["path"].as_str().ok_or("missing `path`")?;
                    std::fs::write(path, b"\x89PNG").map_err(|e| e.to_string())?;
                    Ok(json!({"path": path, "width": 64, "height": 48}))
                }
                other => Err(format!("unknown method `{other}`")),
            }
        }
        fn has_ui(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "app".into()
        }
    }
    let dir = std::env::temp_dir().join(format!("solidcraft-mcp-guides-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shot.png").to_string_lossy().to_string();
    let shoot = |extra: Value| {
        let mut args = json!({"path": path, "width": 64, "height": 48});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let mut app = App { calls: Vec::new() };
        let r = crate::tools::call_tool(&mut app, "screenshot", &args);
        (app.calls, r)
    };
    let (calls, r) = shoot(json!({}));
    assert!(r["isError"] != json!(true), "{r}");
    assert_eq!(calls.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(), ["ui.screenshot"]);
    let (calls, r) = shoot(json!({"grid": false, "axes": false}));
    assert!(r["isError"] != json!(true), "{r}");
    assert_eq!(calls, [("ui.render".to_string(), json!({"path": path, "width": 64, "height": 48, "grid": false, "axes": false}))]);
    let (calls, _) = shoot(json!({"axes": true}));
    assert_eq!(calls, [("ui.render".to_string(), json!({"path": path, "width": 64, "height": 48, "axes": true}))]);
    let (calls, r) = shoot(json!({"source": "model", "grid": false}));
    assert!(r["isError"] != json!(true), "{r}");
    assert_eq!(calls, [("ui.render".to_string(), json!({"path": path, "width": 64, "height": 48, "grid": false}))]);
    // The window shows what it shows: nothing is called, and the error says what to ask for.
    let (calls, r) = shoot(json!({"source": "window", "grid": false, "view": "top"}));
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("source"), "{r}");
    assert!(calls.is_empty(), "{calls:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// #1677: a hidden window gives no picture. With no `source`, a model render comes back and says
/// it is one (`fallback`); asked for the window, the tool says why there is none, at once.
#[test]
fn a_hidden_window_screenshot_says_so() {
    struct Hidden {
        calls: Vec<String>,
    }
    impl crate::backend::Backend for Hidden {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.push(method.to_string());
            match method {
                "ui.screenshot" => Err("no frame was presented (window hidden?); use ui.render".into()),
                "ui.render" => {
                    let path = params["path"].as_str().ok_or("missing `path`")?;
                    std::fs::write(path, b"\x89PNG").map_err(|e| e.to_string())?;
                    Ok(json!({"path": path, "width": 64, "height": 48}))
                }
                other => Err(format!("unknown method `{other}`")),
            }
        }
        fn has_ui(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "app".into()
        }
    }
    let dir = std::env::temp_dir().join(format!("solidcraft-mcp-hidden-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shot.png").to_string_lossy().to_string();
    let shoot = |extra: Value| {
        let mut args = json!({"path": path, "width": 64, "height": 48});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let mut app = Hidden { calls: Vec::new() };
        let r = crate::tools::call_tool(&mut app, "screenshot", &args);
        (app.calls, r)
    };
    // The default: the window first, then a model render that says why.
    let (calls, r) = shoot(json!({}));
    assert!(r["isError"] != json!(true), "{r}");
    assert_eq!(calls, ["ui.screenshot", "ui.render"]);
    let info: Value = serde_json::from_str(r["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(info["source"], "model", "{info}");
    assert!(info["fallback"].as_str().is_some_and(|f| f.contains("window hidden")), "{info}");
    // Asked for the window: an error naming the cause and the way out, and no render.
    let (calls, r) = shoot(json!({"source": "window"}));
    assert_eq!(r["isError"], json!(true), "{r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("window hidden") && text.contains("source \"model\""), "{text}");
    assert_eq!(calls, ["ui.screenshot"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #530: in a design with many bodies every feature result lists all of them, so a batch reply
/// is mostly the same body list once per step. A compact batch reply names only what each step
/// added or changed.
#[test]
fn compact_batch_reply_does_not_repeat_the_body_list() {
    let mut s = server();
    let cube =
        |x: i64, y: i64| json!({"command": "solid.box", "params": {"length": 1, "width": 1, "height": 1, "corner": [x, y, 0], "operation": "new"}});
    let existing: Vec<Value> = (0..50).map(|i| cube(2 * i, 0)).collect();
    tool(&mut s, "batch", json!({"commands": existing}));
    let more: Vec<Value> = (0..3).map(|i| cube(2 * i, 5)).collect();
    let r = raw_tool(&mut s, "batch", json!({"commands": more, "verbose": false}));
    assert!(r.get("error").is_none() && r["result"]["isError"] != json!(true), "{r}");
    let reply = r["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert_eq!(tool(&mut s, "measure", json!({}))["body_count"], 53);
    // The new bodies are named; the 50 untouched ones are not repeated.
    for new in ["Body51", "Body52", "Body53"] {
        assert!(reply.contains(&format!("\"{new}\"")), "{new} missing from the compact reply: {reply}");
    }
    for old in ["Body1", "Body25", "Body50"] {
        assert!(!reply.contains(&format!("\"{old}\"")), "untouched {old} repeated in the compact reply ({} bytes): {reply}", reply.len());
    }
}

/// A compact batch reply keeps what each step did: the body a cut changed, and a sketch
/// command's new ids once (under `created`); the default reply is unchanged.
#[test]
fn compact_batch_reply_keeps_changed_bodies_and_created_ids() {
    let mut s = server();
    let steps = json!([
        {"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "body_name": "Base"}},
        {"command": "solid.box", "params": {"length": 4, "width": 4, "height": 10, "corner": [3, 3, 5], "operation": "cut", "targets": ["Base"]}},
        {"command": "sketch.create", "params": {"plane": "XY"}},
        {"command": "sketch.rectangle.two_point", "params": {"p0": [20, 0], "p1": [30, 10]}},
    ]);
    let full = tool(&mut s, "batch", json!({"commands": steps}));
    assert_eq!(full["results"][1]["bodies"], json!(["Base"]), "{full}");
    assert_eq!(full["results"][3]["curves"], full["results"][3]["created"]["curves"], "{full}");
    tool(&mut s, "new_design", json!({}));
    let r = tool(&mut s, "batch", json!({"commands": steps, "verbose": false}));
    let cut = &r["results"][1];
    assert!(cut.get("bodies").is_none(), "{r}");
    assert_eq!(cut["changed_bodies"], json!(["Base"]), "{r}");
    let rect = &r["results"][3];
    assert!(rect.get("curves").is_none() && rect.get("constraints").is_none(), "{r}");
    assert_eq!(rect["created"]["curves"].as_array().map(Vec::len), Some(4), "{r}");
    assert!(rect["sides"].is_object(), "results that are not repeated stay: {r}");
}

/// #531: a product model keeps an optional variant hidden in its default presentation. Hiding
/// it must survive saving and reopening the design (suppressing it would change the design).
#[test]
fn hidden_body_stays_hidden_after_save_and_reopen() {
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "body_name": "Base"}}));
    tool(
        &mut s,
        "execute",
        json!({"command": "solid.box", "params": {"length": 5, "width": 5, "height": 5, "corner": [20, 0, 0], "operation": "new", "body_name": "Variant"}}),
    );
    tool(&mut s, "execute", json!({"command": "browser.visibility", "params": {"items": ["Variant"], "visible": false}}));
    let visible = |s: &mut Server, body: &str| {
        let d = tool(s, "inspect_design", json!({"measure": false}));
        d["bodies"].as_array().into_iter().flatten().find(|b| b["name"] == body).map(|b| b["visible"].clone()).unwrap_or(Value::Null)
    };
    assert_eq!(visible(&mut s, "Variant"), false);

    let dir = std::env::temp_dir().join(format!("solidcraft-mcp-visibility-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let design = dir.join("variants.solidcraft");
    tool(&mut s, "save", json!({"path": design.to_string_lossy()}));
    tool(&mut s, "new_design", json!({"name": "Empty"}));
    tool(&mut s, "open", json!({"path": design.to_string_lossy()}));
    let (base, variant) = (visible(&mut s, "Base"), visible(&mut s, "Variant"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(base, true);
    assert_eq!(variant, false, "the hidden body is visible again after reopening");
}

/// Single execute calls share batch compaction without changing geometry or inspection.
#[test]
fn compact_execute_keeps_changes_ids_and_default_replies() {
    let mut s = server();
    let commands = json!([
        {"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "body_name": "Base"}},
        {"command": "solid.box", "params": {"length": 4, "width": 4, "height": 10, "corner": [3, 3, 5], "operation": "cut", "targets": ["Base"]}},
        {"command": "sketch.create", "params": {"plane": "XY"}},
        {"command": "sketch.rectangle.two_point", "params": {"p0": [20, 0], "p1": [30, 10]}}
    ]);
    let full: Vec<Value> = commands.as_array().unwrap().iter().map(|a| tool(&mut s, "execute", a.clone())).collect();
    assert_eq!(full[0]["bodies"], json!(["Base"]));
    assert_eq!(full[3]["curves"], full[3]["created"]["curves"]);
    let expected_volume = volume(&mut s);
    for verbose in [true, false] {
        tool(&mut s, "new_design", json!({}));
        for (a, expected) in commands.as_array().unwrap().iter().zip(&full) {
            let mut a = a.clone();
            a["verbose"] = json!(verbose);
            let mut actual = tool(&mut s, "execute", a);
            let mut expected = expected.clone();
            // Independent executions have different timings; the field must still be present.
            if expected.get("ms").is_some() {
                assert!(actual["ms"].is_number());
                actual.as_object_mut().unwrap().remove("ms");
                expected.as_object_mut().unwrap().remove("ms");
            }
            if !verbose {
                // Compare against the full result with only the documented repeated fields removed.
                let o = expected.as_object_mut().unwrap();
                if o.contains_key("created_bodies") {
                    o.remove("bodies");
                }
                for key in ["points", "curves", "constraints", "dimensions"] {
                    if o.get(key).is_some_and(|v| o.get("created").and_then(|c| c.get(key)) == Some(v)) {
                        o.remove(key);
                    }
                }
            }
            assert_eq!(actual, expected);
        }
        assert!((volume(&mut s) - expected_volume).abs() < 1e-9);
        let inspect = tool(&mut s, "execute", json!({"command": "sketch.inspect", "verbose": false}));
        assert_eq!(inspect["curves"].as_array().map(Vec::len), Some(4));
        let invalid = raw_tool(&mut s, "execute", json!({"command": "solid.box", "verbose": "false"}));
        assert_eq!(invalid["error"]["code"], -32602);
        assert!((volume(&mut s) - expected_volume).abs() < 1e-9);
        let error = raw_tool(&mut s, "execute", json!({"command": "not.a.command", "verbose": false}));
        assert_eq!(error["result"]["isError"], true);
    }
}

/// A connected app sees exactly one engine call; no inventory inspection is needed.
#[test]
fn compact_execute_uses_one_backend_call_and_keeps_removed_bodies() {
    struct ReplyOnly {
        calls: usize,
    }
    impl crate::backend::Backend for ReplyOnly {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            assert_eq!(method, "engine.execute");
            assert_eq!(params, json!({"command": "test.feature", "params": {}}));
            self.calls += 1;
            Ok(
                json!({"feature": 7, "name": "Combine1", "bodies": ["Base"], "created_bodies": [], "changed_bodies": ["Base"], "removed_bodies": ["Tool"]}),
            )
        }
        fn has_ui(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "test".into()
        }
    }
    let mut b = ReplyOnly { calls: 0 };
    let r = crate::tools::call_tool(&mut b, "execute", &json!({"command": "test.feature", "verbose": false}));
    let v: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(b.calls, 1);
    assert_eq!(v, json!({"feature": 7, "name": "Combine1", "created_bodies": [], "changed_bodies": ["Base"], "removed_bodies": ["Tool"]}));
}

/// The worked example in `skills/solidcraft-modeling/reference.md` is what agents copy, so it has
/// to keep building with the volume the file documents, and follow a parameter change.
#[test]
fn skill_reference_example_builds_as_documented() {
    let doc = include_str!("../../../skills/solidcraft-modeling/reference.md");
    let start = doc.find("{\"commands\":[").expect("the reference has a worked example");
    let end = start + doc[start..].find("```").expect("the example's code block ends");
    let example: Value = serde_json::from_str(&doc[start..end]).expect("the example is JSON");
    let mut s = server();
    tool(&mut s, "batch", example);
    // Plate with four r5 corners, minus four counterbored holes (Ø3.4 through, Ø6.5 × 3.3).
    let expected = |l: f64| {
        let (w, t) = (50.0, 6.0);
        let plate = l * w * t - 4.0 * 25.0 * (1.0 - PI / 4.0) * t;
        let hole = PI * 1.7 * 1.7 * (t - 3.3) + PI * 3.25 * 3.25 * 3.3;
        plate - 4.0 * hole
    };
    assert!((expected(80.0) - 23335.2).abs() < 0.05, "the reference states {}", expected(80.0));
    // Volume can't see where the holes are: each counterbore floor (z = T - 3.3) is centred on its
    // hole, at (±(L/2 - inset), ±(W/2 - inset)).
    let holes_at = |s: &mut Server, x: f64| {
        let topo = tool(s, "body_topology", json!({"body": "Body1"}));
        let mut floors: Vec<(i64, i64)> = topo["faces"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["centroid"].as_array())
            .filter(|c| (c[2].as_f64().unwrap() - 2.7).abs() < 1e-6)
            .map(|c| ((c[0].as_f64().unwrap() * 1000.0).round() as i64, (c[1].as_f64().unwrap() * 1000.0).round() as i64))
            .collect();
        floors.sort_unstable();
        let x = (x * 1000.0).round() as i64;
        assert_eq!(floors, vec![(-x, -17000), (-x, 17000), (x, -17000), (x, 17000)], "{topo}");
    };
    let m = tool(&mut s, "measure", json!({}));
    assert!(rel(m["total"]["volume_mm3"].as_f64().unwrap(), expected(80.0)) < 1e-5, "{m}");
    assert_eq!(m["bodies"][0]["faces"], json!(22), "{m}");
    holes_at(&mut s, 32.0);
    let r = tool(&mut s, "set_parameter", json!({"name": "L", "value": "120 mm"}));
    assert_eq!(r["errors"].as_array().map(Vec::len), Some(0), "{r}");
    assert!(rel(volume(&mut s), expected(120.0)) < 1e-5);
    holes_at(&mut s, 52.0);
}

/// The server instructions' recipe (primitives with expression fields, fillet before the shell,
/// inside features after it, a blind hole with its depth, picks by world point, parameters
/// checked in one coalesced batch and set back) builds the part it describes, with the volume
/// worked out by hand, and follows its parameters, so the instructions and the engine stay in
/// step whatever the kernel does.
#[test]
fn instructions_recipe_builds_a_parametric_shelled_box() {
    // A box L × W × H with its four vertical edges rounded r, shelled from the top with wall t
    // (the inner corners are r - t), a boss of radius R from the floor to the rim, and a blind
    // hole Ø d, depth hd, down the boss.
    let (fr, t, big_r, d, hd) = (5.0, 2.0, 6.0, 5.0, 10.0);
    let expected = |l: f64, w: f64, h: f64| {
        let corner = 4.0 * (1.0 - PI / 4.0);
        let outer = l * w * h - corner * fr * fr * h;
        let cavity = (l - 2.0 * t) * (w - 2.0 * t) * (h - t) - corner * (fr - t) * (fr - t) * (h - t);
        let boss = PI * big_r * big_r * (h - t);
        let hole = PI * (d / 2.0) * (d / 2.0) * hd;
        outer - cavity + boss - hole
    };
    let mut s = server();
    let r = tool(
        &mut s,
        "batch",
        json!({"verbose": false, "commands": [
            {"command": "parameters.add", "params": {"name": "L", "expression": "60 mm"}},
            {"command": "parameters.add", "params": {"name": "W", "expression": "40 mm"}},
            {"command": "parameters.add", "params": {"name": "H", "expression": "30 mm"}},
            {"command": "parameters.add", "params": {"name": "wall", "expression": "2 mm"}},
            {"command": "solid.box", "params": {"length": "L", "width": "W", "height": "H", "corner": [0, 0, 0], "name": "Block", "body_name": "Case"}},
            {"command": "solid.fillet", "params": {"edges": [[0, 0, 15], [60, 0, 15], [60, 40, 15], [0, 40, 15]], "radius": fr, "name": "Corners"}},
            {"command": "solid.shell", "params": {"faces": [[30, 20, 30]], "thickness": "wall", "name": "Hollow"}},
            {"command": "solid.cylinder", "params": {"radius": big_r, "height": "H - wall", "base": ["L/2", "W/2", "wall"], "operation": "join", "name": "Boss"}},
            {"command": "solid.hole", "params": {"position": ["L/2", "W/2", "H"], "diameter": d, "depth": hd, "name": "Pilot"}},
            {"command": "measure", "params": {}},
        ]}),
    );
    let results = r["results"].as_array().unwrap();
    assert_eq!(results.len(), 10, "{r}");
    let m = &results[9];
    assert_eq!(m["bodies"].as_array().map(Vec::len), Some(1), "{m}");
    assert!(rel(m["total"]["volume_mm3"].as_f64().unwrap(), expected(60.0, 40.0, 30.0)) < 1e-3, "{m}");
    let no_errors = |s: &mut Server| {
        let d = tool(s, "inspect_design", json!({"measure": false, "sketches": false}));
        let failed: Vec<&Value> = d["timeline"].as_array().unwrap().iter().filter(|f| !f["error"].is_null()).collect();
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(d["timeline"].as_array().map(Vec::len), Some(5), "{d}");
    };
    no_errors(&mut s);
    // The boss and the pilot hole are where the expressions put them.
    let axes = |s: &mut Server, x: f64, y: f64| {
        let topo = tool(s, "body_topology", json!({"body": "Case"}));
        let cylinders: Vec<(f64, f64, f64)> = topo["faces"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f.get("cylinder"))
            .map(|c| (c["origin"][0].as_f64().unwrap(), c["origin"][1].as_f64().unwrap(), c["radius"].as_f64().unwrap()))
            .collect();
        for radius in [big_r, d / 2.0] {
            assert!(
                cylinders.iter().any(|&(cx, cy, cr)| (cx - x).abs() < 1e-6 && (cy - y).abs() < 1e-6 && (cr - radius).abs() < 1e-6),
                "no cylinder r {radius} on ({x}, {y}): {cylinders:?}"
            );
        }
    };
    axes(&mut s, 30.0, 20.0);
    // Two parameters changed in one coalesced batch (one rebuild per run), measured, set back.
    let r = tool(
        &mut s,
        "batch",
        json!({"verbose": false, "coalesce_parameters": true, "commands": [
            {"command": "parameters.change", "params": {"name": "L", "expression": "80 mm"}},
            {"command": "parameters.change", "params": {"name": "W", "expression": "50 mm"}},
            {"command": "measure", "params": {}},
            {"command": "parameters.change", "params": {"name": "L", "expression": "60 mm"}},
            {"command": "parameters.change", "params": {"name": "W", "expression": "40 mm"}},
        ]}),
    );
    let results = r["results"].as_array().unwrap();
    assert_eq!(results.len(), 5, "{r}");
    assert_eq!(results[0]["coalesced"], 2, "{r}");
    assert_eq!(results[0]["errors"].as_array().map(Vec::len), Some(0), "{r}");
    let m = &results[2];
    assert_eq!(m["bodies"].as_array().map(Vec::len), Some(1), "{m}");
    assert!(rel(m["total"]["volume_mm3"].as_f64().unwrap(), expected(80.0, 50.0, 30.0)) < 1e-3, "{m}");
    assert_eq!(results[3]["coalesced"], 2, "{r}");
    assert_eq!(results[4]["errors"].as_array().map(Vec::len), Some(0), "{r}");
    let m = tool(&mut s, "measure", json!({}));
    assert_eq!(m["bodies"].as_array().map(Vec::len), Some(1), "{m}");
    assert!(rel(m["total"]["volume_mm3"].as_f64().unwrap(), expected(60.0, 40.0, 30.0)) < 1e-3, "{m}");
    no_errors(&mut s);
    axes(&mut s, 30.0, 20.0);
}

/// The skill reference's sketch-plane table: where sketch (10, 20) lands and which way +distance goes.
#[test]
fn sketch_plane_axes_match_the_skill_reference() {
    let mut s = server();
    for plane in ["XY", "XZ", "YZ"] {
        tool(
            &mut s,
            "batch",
            json!({"commands": [
                {"command": "sketch.create", "params": {"plane": plane}},
                {"command": "sketch.circle.center", "params": {"center": [10, 20], "radius": 1}},
                {"command": "sketch.finish"},
                {"command": "solid.extrude", "params": {"distance": 3, "operation": "new"}},
            ]}),
        );
    }
    let m = tool(&mut s, "measure", json!({}));
    let centres: Vec<Vec<f64>> = m["bodies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["center_of_mass"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect())
        .collect();
    let expected = [[10.0, 20.0, 1.5], [10.0, 1.5, -20.0], [1.5, 10.0, 20.0]];
    assert_eq!(centres.len(), 3, "{m}");
    for (got, want) in centres.iter().zip(expected) {
        assert!(got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-6), "{got:?} != {want:?}");
    }
}

/// The worked example in `skills/solidcraft-mechanisms/reference.md` is what agents copy: the
/// actuator has to follow the hinge with the stroke the file computes, the mirrored panel its link,
/// and the drive has to stop where the rod reaches its limit.
#[test]
fn mechanisms_skill_example_moves_as_documented() {
    let doc = include_str!("../../../skills/solidcraft-mechanisms/reference.md");
    let start = doc.find("{\"commands\":[").expect("the reference has a worked example");
    let end = start + doc[start..].find("```").expect("the example's code block ends");
    let example: Value = serde_json::from_str(&doc[start..end]).expect("the example is JSON");
    let mut s = server();
    tool(&mut s, "batch", example);
    let exec = |s: &mut Server, command: &str, params: Value| tool(s, "execute", json!({"command": command, "params": params}));
    let value = |list: &Value, joint: &str| {
        let j = list["joints"].as_array().unwrap().iter().find(|j| j["name"] == joint).unwrap_or_else(|| panic!("no joint {joint}: {list}"));
        j["values"][0]["value"].as_f64().unwrap()
    };
    let com = |s: &mut Server, occurrence: &str| -> Vec<f64> {
        let p = exec(s, "occurrence.properties", json!({"occurrence": occurrence}));
        p["total"]["center_of_mass"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect()
    };
    // Frame 0, the panels and the actuator one motion between them; nothing open.
    let list = exec(&mut s, "joint.list", json!({}));
    assert_eq!(list["conflicts"], json!([]), "{list}");
    let mut dof: Vec<u64> = list["dof"].as_array().unwrap().iter().map(|d| d["dof"].as_u64().unwrap()).collect();
    dof.sort_unstable();
    assert_eq!(dof, [0, 0, 1, 1, 1, 1], "root, Frame, then Wing, Wing 2, barrel and rod: {list}");
    let near = |got: &[f64], want: [f64; 3]| got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-3);
    let wing = com(&mut s, "Wing:1");
    assert!(near(&wing, [0.0, 20.0, 1.5]), "joined where it was modelled: {wing:?}");

    // The tip at (0, 30 cos θ, 30 sin θ), the root at (0, 0, -40): L² = 2500 + 2400 sin θ.
    let stroke = |deg: f64| (2500.0 + 2400.0 * deg.to_radians().sin()).sqrt() - 50.0;
    assert!((stroke(20.0) - 7.6268).abs() < 5e-5, "the reference states {}", stroke(20.0));
    exec(&mut s, "joint.drive", json!({"joint": "Hinge", "value": 20}));
    let list = exec(&mut s, "joint.list", json!({}));
    assert!((value(&list, "Stroke") - stroke(20.0)).abs() < 1e-6, "{list}");
    assert!((value(&list, "Hinge 2") + 20.0).abs() < 1e-9, "{list}");
    let (wing, wing2) = (com(&mut s, "Wing:1"), com(&mut s, "Wing 2:1"));
    assert!(near(&wing, [0.0, 18.281, 8.250]), "{wing:?}");
    assert!(near(&wing2, [0.0, -18.281, 8.250]), "{wing2:?}");

    // The rod reaches +10 mm where L = 60: sin θ = 11/24.
    let stop = (11.0f64 / 24.0).asin().to_degrees();
    assert!((stop - 27.280).abs() < 5e-4, "the reference states {stop}");
    let r = exec(&mut s, "joint.drive", json!({"joint": "Hinge", "value": 45}));
    assert_eq!(r["stopped_by_loop"]["joint"], "Stroke", "{r}");
    let list = exec(&mut s, "joint.list", json!({}));
    let hinge = value(&list, "Hinge");
    assert!((hinge - stop).abs() < 0.01, "stopped at {hinge}, the rod's limit is at {stop}");
    assert!(value(&list, "Stroke") <= 10.0 + 1e-6 && (value(&list, "Stroke") - stroke(hinge)).abs() < 1e-6, "{list}");
}

/// Every tool and command the agent skills call exists, so a rename or removal can't leave the
/// skills sending agents to names the server no longer has.
#[test]
fn skills_name_only_real_tools_and_commands() {
    let docs = [
        include_str!("../../../skills/solidcraft-modeling/SKILL.md"),
        include_str!("../../../skills/solidcraft-modeling/reference.md"),
        include_str!("../../../skills/solidcraft-editing/SKILL.md"),
        include_str!("../../../skills/solidcraft-checking/SKILL.md"),
        include_str!("../../../skills/solidcraft-checking/reference/porting.md"),
        include_str!("../../../skills/solidcraft-mechanisms/SKILL.md"),
        include_str!("../../../skills/solidcraft-mechanisms/reference.md"),
        include_str!("../../../skills/solidcraft-reporting/SKILL.md"),
        include_str!("../../../skills/solidcraft-reporting/reference/report-template.md"),
    ];
    let mut s = server();
    let tools: Vec<String> = tool_definitions().iter().filter_map(|t| t["name"].as_str()).map(str::to_owned).collect();
    let commands: Vec<String> =
        tool(&mut s, "list_commands", json!({})).as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).map(str::to_owned).collect();
    let mut checked = 0;
    for doc in docs {
        // A call in backticks is `name {}`, `name {"json":…}` or `name {placeholder}`: a tool when the
        // name is snake_case, a command when it is a dotted id. Field lists such as
        // `sides {bottom,right,top,left}` and `created: {…}` aren't calls. JSON examples name
        // commands as "command":"id".
        let ident = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
        let calls = doc.split('`').skip(1).step_by(2).filter_map(|span| {
            let (head, args) = span.split_once(" {")?;
            let name = head.rsplit(' ').next()?;
            let args = args.strip_suffix('}')?;
            let call_like = args.is_empty() || args.starts_with('"') || args.chars().all(|c| c.is_ascii_lowercase() || c == '_');
            (ident(name) && call_like).then_some(name)
        });
        let in_json = doc.split("\"command\":\"").skip(1).filter_map(|rest| rest.split('"').next());
        for name in calls.chain(in_json) {
            let known = if name.contains('.') { commands.iter().any(|c| c == name) } else { tools.iter().any(|t| t == name) };
            assert!(known, "a skill calls `{name}`, which is neither a tool nor a command");
            checked += 1;
        }
    }
    assert!(checked > 30, "only {checked} tool and command names found in the skills");
}

/// Every tool and command the server instructions name is real: a backticked `snake_case` word
/// (bare or as a call, `measure {}`) is a tool, a dotted id (`solid.box`) a command in the
/// engine's registry. A rename or removal can't leave the instructions sending agents to names
/// the server no longer has.
#[test]
fn instructions_name_only_real_tools() {
    let mut s = server();
    let init = call(&mut s, 1, "initialize", json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}}));
    let text = init["result"]["instructions"].as_str().unwrap().to_owned();
    let tools: Vec<String> = tool_definitions().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
    let commands: Vec<String> =
        tool(&mut s, "list_commands", json!({})).as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).map(str::to_owned).collect();
    let ident = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
    // A span is `name`, `name {}` or `name {"json":…}`; `edges: [[x,y,z]]`, `verbose: false` and
    // `"L/2 - 10"` are fields and values, not calls.
    let named: Vec<&str> =
        text.split('`').skip(1).step_by(2).map(|span| span.split_once(" {").map_or(span, |(head, _)| head)).filter(|w| ident(w)).collect();
    for must in ["skill_reference", "batch", "measure", "body_topology", "pending", "solid.box", "parameters.change", "edit.restore_checkpoint"] {
        assert!(named.contains(&must), "{named:?}");
    }
    for name in named {
        let known = if name.contains('.') { commands.iter().any(|c| c == name) } else { tools.iter().any(|t| t == name) };
        assert!(known, "the instructions name `{name}`, which is neither a tool nor a command");
    }
}

/// A fake app that records what it is asked and answers `engine.batch` as configured.
struct Recorder {
    calls: Vec<(String, Value)>,
    batch: Result<Value, String>,
}

impl crate::backend::Backend for Recorder {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.calls.push((method.to_string(), params.clone()));
        match method {
            "engine.batch" => self.batch.clone(),
            "engine.execute" => match params["command"].as_str() {
                Some("edit.checkpoint") => Ok(json!({"checkpoint": 3})),
                Some(c) => Ok(json!({"ran": c})),
                None => Err("missing `command`".into()),
            },
            "document.inspect" => Ok(json!({"undo": 0})),
            other => Err(format!("unknown method `{other}`")),
        }
    }
    fn has_ui(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "recorder".into()
    }
}

fn recorded_batch(batch: Result<Value, String>, args: Value) -> (Recorder, Value, bool) {
    let mut b = Recorder { calls: Vec::new(), batch };
    let r = crate::tools::call_tool(&mut b, "batch", &args);
    let body = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap_or_else(|_| r["content"][0]["text"].clone());
    let is_error = r["isError"] == json!(true);
    (b, body, is_error)
}

fn methods(b: &Recorder) -> Vec<&str> {
    b.calls.iter().map(|(m, _)| m.as_str()).collect()
}

/// #3654: bridged, `running` asks the app's `control.running`; an older app's refusal is the
/// tool's error.
#[test]
fn running_asks_the_app() {
    let mut b = Recorder { calls: Vec::new(), batch: Ok(json!({})) };
    let r = crate::tools::call_tool(&mut b, "running", &json!({}));
    assert_eq!(methods(&b), ["control.running"]);
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(r["content"][0]["text"].as_str().is_some_and(|t| t.starts_with("unknown method")), "{r}");
}

/// #743: bridged to an app that has `engine.batch`, a batch is one control request.
#[test]
fn batch_is_one_engine_batch_call() {
    let reply = json!({"completed": 2, "results": [{"feature": 1, "bodies": ["B"], "created_bodies": ["B"]}, {"x": 2}], "undone": 0});
    let steps = json!([{"command": "a.one", "params": {"v": 1}}, {"command": "a.two"}]);
    let (b, body, is_error) = recorded_batch(Ok(reply.clone()), json!({"commands": steps}));
    assert_eq!(methods(&b), ["engine.batch"]);
    assert_eq!(b.calls[0].1, json!({"commands": steps, "rollback": true}));
    assert!(!is_error);
    assert_eq!(body, json!({"ok": true, "completed": 2, "results": reply["results"]}));
    // `verbose: false` compacts each step's result as the per-step path does.
    let (_, body, _) = recorded_batch(Ok(reply), json!({"commands": steps, "verbose": false}));
    assert_eq!(body["results"][0], json!({"feature": 1, "created_bodies": ["B"]}), "{body}");
}

#[test]
fn batch_reports_a_failed_engine_batch_like_before() {
    let reply = json!({"completed": 1, "results": [{"x": 1}], "failed": {"index": 1, "command": "a.two", "error": "boom"}, "undone": 1});
    let (b, body, is_error) = recorded_batch(Ok(reply), json!({"commands": [{"command": "a.one"}, {"command": "a.two"}]}));
    assert_eq!(methods(&b), ["engine.batch"]);
    assert!(is_error);
    assert_eq!(
        body,
        json!({"ok": false, "completed": 1, "failed": {"index": 1, "command": "a.two", "error": "boom"}, "rolled_back": true, "undone": 1, "applied": [], "not_run": [], "results": [{"x": 1}]})
    );
}

/// Bridged, `expect` measures the app's design with one `inspect.measure` after the one
/// `engine.batch` request, and a batch that failed is not measured.
#[test]
fn a_bridged_batch_measures_its_expect_after_the_one_batch_call() {
    let steps = json!([{"command": "a.one"}]);
    let reply = json!({"completed": 1, "results": [{"x": 1}], "undone": 0});
    let mut b = Recorder { calls: Vec::new(), batch: Ok(reply.clone()) };
    let r = crate::tools::call_tool(&mut b, "batch", &json!({"commands": steps, "expect": {"bodies": 1}}));
    assert_eq!(methods(&b), ["engine.batch", "engine.execute"]);
    assert_eq!(b.calls[1].1, json!({"command": "inspect.measure", "params": {}}));
    // The recorder's measure reply has no bodies: the mismatch comes first, the batch's reply after it.
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(r["content"][0]["text"].as_str().unwrap().starts_with("expectation not met: bodies: measured 0, expected 1."), "{r}");
    let body: Value = serde_json::from_str(r["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!((body["ok"].clone(), body["completed"].clone(), body["expectation"]["ok"].clone()), (json!(true), json!(1), json!(false)), "{body}");
    let failed = json!({"completed": 0, "results": [], "failed": {"index": 0, "command": "a.one", "error": "boom"}, "undone": 0});
    let (b, body, is_error) = recorded_batch(Ok(failed), json!({"commands": steps, "expect": {"bodies": 1}}));
    assert!(is_error);
    assert_eq!(methods(&b), ["engine.batch"], "a failed batch is not measured");
    assert!(body.get("expectation").is_none(), "{body}");
}

/// `rolled_back` tells whether the design went back, not whether a rollback was asked for
/// (#3242): the app's `rolled_back`, or, for an app that predates that field, none of the
/// completed steps undone means the design did not go back.
#[test]
fn a_batch_the_app_could_not_roll_back_reports_it() {
    let failed = |reply: Value| {
        let (_, body, is_error) = recorded_batch(Ok(reply), json!({"commands": [{"command": "a.one"}, {"command": "a.two"}]}));
        assert!(is_error);
        body
    };
    let body = failed(
        json!({"completed": 1, "results": [{"x": 1}], "failed": {"index": 1, "command": "a.two", "error": "boom"}, "rolled_back": false, "undone": 0}),
    );
    assert_eq!(body["rolled_back"], false, "{body}");
    let body = failed(json!({"completed": 1, "results": [{"x": 1}], "failed": {"index": 1, "command": "a.two", "error": "boom"}, "undone": 0}));
    assert_eq!(body["rolled_back"], false, "{body}");
    // A first step that failed left nothing to take back.
    let body = failed(json!({"completed": 0, "results": [], "failed": {"index": 0, "command": "a.one", "error": "boom"}, "undone": 0}));
    assert_eq!(body["rolled_back"], true, "{body}");
}

#[test]
fn batch_falls_back_when_the_app_lacks_engine_batch() {
    let (b, body, is_error) =
        recorded_batch(Err("unknown method `engine.batch`".into()), json!({"commands": [{"command": "a.one"}, {"command": "a.two"}]}));
    assert!(!is_error, "{body}");
    let ran: Vec<&str> = b.calls.iter().filter(|(m, _)| m == "engine.execute").map(|(_, p)| p["command"].as_str().unwrap_or("")).collect();
    assert!(ran.contains(&"a.one") && ran.contains(&"a.two"), "{ran:?}");
    assert_eq!(body["results"], json!([{"ran": "a.one"}, {"ran": "a.two"}]));
}

#[test]
fn batch_falls_back_while_the_agent_cursor_is_shown() {
    let (b, body, is_error) =
        recorded_batch(Err("engine.batch unavailable: the agent cursor is shown".into()), json!({"commands": [{"command": "a.one"}]}));
    assert!(!is_error, "{body}");
    assert!(b.calls.iter().any(|(m, p)| m == "engine.execute" && p["command"] == "a.one"), "{:?}", b.calls);
}

/// The app may have run the batch: sending the steps again would apply them twice.
#[test]
fn batch_does_not_resend_after_a_transport_error() {
    let (b, body, is_error) =
        recorded_batch(Err("SolidCraft at 127.0.0.1:1: timed out".into()), json!({"commands": [{"command": "a.one"}, {"command": "a.two"}]}));
    assert_eq!(methods(&b), ["engine.batch"]);
    assert!(is_error);
    assert!(body.as_str().is_some_and(|t| t.contains("may have run") && t.contains("timed out")), "{body}");
}

/// A batch longer than one control line goes step by step instead of being refused unrun.
#[test]
fn batch_too_large_for_one_control_line_goes_per_step() {
    let big = "x".repeat(solidcraft_engine::MAX_LINE);
    let (b, body, is_error) = recorded_batch(Ok(json!({})), json!({"commands": [{"command": "a.one", "params": {"text": big}}]}));
    assert!(!is_error, "{body}");
    assert!(!methods(&b).contains(&"engine.batch"), "{:?}", methods(&b));
    assert!(b.calls.iter().any(|(m, p)| m == "engine.execute" && p["command"] == "a.one"));
}

/// A fake app on a loopback port: counts connections and `engine.batch` lines, never replies to
/// them (`close_first`: close a connection as soon as it is accepted).
fn silent_app(close_first: bool) -> (String, std::sync::Arc<std::sync::Mutex<(usize, usize)>>) {
    use std::io::BufRead;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let counts = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            counts.lock().unwrap().0 += 1;
            let counts = counts.clone();
            std::thread::spawn(move || {
                if close_first && counts.lock().unwrap().0 > 1 {
                    return;
                }
                let mut r = std::io::BufReader::new(stream);
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                    if line.contains("engine.batch") {
                        counts.lock().unwrap().1 += 1;
                        return;
                    }
                    line.clear();
                }
            });
        }
    });
    (addr, seen)
}

#[test]
fn remote_engine_batch_is_not_resent_after_a_lost_reply() {
    use crate::backend::Backend;
    for close_first in [false, true] {
        let (addr, seen) = silent_app(close_first);
        let mut remote = crate::Remote::connect(&addr).unwrap();
        let r = remote.call("engine.batch", json!({"commands": [{"command": "solid.box"}]}));
        assert!(r.is_err(), "{r:?}");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (connections, batches) = *seen.lock().unwrap();
        // The connection from `connect`, and one fresh one for the batch: no resend.
        assert_eq!(connections, 2, "close_first {close_first}: {connections} connections");
        assert!(batches <= 1, "close_first {close_first}: the batch arrived {batches} times");
        if !close_first {
            assert_eq!(batches, 1);
        }
    }
}

// Catches legacy entry points silently discarding outer guards, including the script route.
#[test]
fn guarded_backend_legacy_headless_refuses_before_execute() {
    use crate::Backend;
    let mut baseline = Headless::default();
    baseline.call("engine.execute", json!({"command":"parameters.change","params":{"name":"width","value":"10 mm"}})).unwrap();
    assert!(!baseline.session.undo.is_empty(), "the unguarded control really edits the document");
    for method in ["engine.execute", "command", "engine.batch", "engine.script"] {
        for expected in [Value::Null, json!({"documentId":"doc-A","revision":"0","extra":true})] {
            let mut backend = Headless::default();
            let before = backend.session.doc.clone();
            let request = json!({"command":"parameters.change","params":{"name":"width","value":"10 mm"},
                "commands":[{"command":"parameters.change","params":{"name":"width","value":"10 mm"}}],
                "expectedDocument":expected});
            let result = backend.call(method, request);
            assert!(result.is_err_and(|e| e.contains("call_guarded")), "{method}: must refuse before executing");
            assert!(std::sync::Arc::ptr_eq(&before, &backend.session.doc));
            assert!(backend.session.undo.is_empty());
        }
    }
}

// These are the same inline wire bytes/cases as the frozen UI parser tests, pending #5's corpus.
// Catches lossy revisions, null-as-absent and unknown stamp fields before a backend can run.
#[test]
fn guarded_mcp_native_conversion_roundtrips_and_rejects_bad_envelopes() {
    use crate::document_guard::{parse_expected_document, stamp_json};
    assert_eq!(parse_expected_document(&json!({})).unwrap(), None);
    for (revision, number) in [("0", 0u64), ("9007199254740993", 9_007_199_254_740_993), ("18446744073709551615", u64::MAX)] {
        for id in ["a".to_string(), "A0._:-z".to_string(), "x".repeat(128)] {
            let bytes = json!({"expectedDocument":{"documentId":id,"revision":revision}}).to_string();
            let envelope: Value = serde_json::from_slice(bytes.as_bytes()).unwrap();
            let native = parse_expected_document(&envelope).unwrap().unwrap();
            let actual_revision: u64 = native.revision;
            assert_eq!(actual_revision, number);
            assert_eq!(native.document_id, id);
            assert_eq!(stamp_json(&native), json!({"documentId":id,"revision":revision}));
        }
    }
    let id = "doc-A";
    let mut invalid =
        vec![Value::Null, json!({}), json!({"documentId":id}), json!({"revision":"0"}), json!({"documentId":id,"revision":"0","extra":true})];
    for revision in
        [json!(0), Value::Null, json!("01"), json!("-1"), json!("1e3"), json!("18446744073709551616"), json!(" 1"), json!("1 "), json!("")]
    {
        invalid.push(json!({"documentId":id,"revision":revision}));
    }
    for id in [json!(""), json!("-bad"), json!("a/b"), json!("é"), json!("x".repeat(129)), Value::Null, json!(1)] {
        invalid.push(json!({"documentId":id,"revision":"0"}));
    }
    for expected in invalid {
        let bytes = json!({"expectedDocument":expected}).to_string();
        let envelope: Value = serde_json::from_slice(bytes.as_bytes()).unwrap();
        assert!(
            matches!(parse_expected_document(&envelope), Err(crate::GuardedCallError::Rejected { error_details, .. })
            if error_details["code"] == "INVALID_REQUEST" && error_details["retry"] == "never"),
            "{bytes}"
        );
    }
}

enum GuardAnswer {
    Reply(Value),
    Close,
    ReplyAndClose(Value),
}

// Catches successful primitive/null payloads being rewritten, rejection metadata loss, and
// a malformed/missing success stamp being mistaken for a definite command failure/success.
#[test]
fn guarded_mcp_native_reply_decode_preserves_results_and_refusals() {
    use crate::backend::decode_guarded_reply;
    for native in [json!({"x":1}), json!([1, 2]), json!("native"), json!(7), Value::Null] {
        let reply = decode_guarded_reply(&json!({"ok":true,"result":native,
            "documentStamp":{"documentId":"doc-A","revision":"18446744073709551615"}}))
        .unwrap();
        assert_eq!(reply.result, native);
        assert_eq!(reply.document_stamp.revision, u64::MAX);
    }
    for stamp in [
        Value::Null,
        json!({"documentId":"doc-A","revision":1}),
        json!({"documentId":"doc-A","revision":"01"}),
        json!({"documentId":"doc-A","revision":"0","extra":true}),
    ] {
        assert!(matches!(
            decode_guarded_reply(&json!({"ok":true,"result":null,"documentStamp":stamp})),
            Err(crate::GuardedCallError::OutcomeUnknown(_))
        ));
    }
    for reply in [json!({"ok":true,"result":null}), json!({"ok":true,"documentStamp":{"documentId":"doc-A","revision":"0"}}), json!({})] {
        assert!(matches!(decode_guarded_reply(&reply), Err(crate::GuardedCallError::OutcomeUnknown(_))));
    }
    let details = json!({"code":"STALE_DOCUMENT","message":"stale","retry":"never","details":{
        "expected":{"documentId":"doc-A","revision":"9007199254740993"},"current":null}});
    assert!(matches!(decode_guarded_reply(&json!({"ok":false,"error":"stale","errorDetails":details})),
        Err(crate::GuardedCallError::Rejected { message, error_details }) if message == "stale" && error_details == details));
    assert!(matches!(decode_guarded_reply(&json!({"ok":false,"error":"native command failed"})),
        Err(crate::GuardedCallError::CommandFailed(message)) if message == "native command failed"));
}

// Catches thin validation being bypassed in the actual network backend. Invalid envelopes and
// unsupported routes must not even open a socket or touch legacy script/step dispatch.
#[test]
fn guarded_remote_validation_refuses_before_connection() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    let peer = GuardPeer::new(|_, _| GuardAnswer::Reply(guard_success()));
    let mut remote = peer.remote(ParamCheck::Off);
    for expected in [Value::Null, json!({"documentId":"doc-A","revision":"0","extra":true}), json!({"documentId":"doc-A","revision":0})] {
        let mut request = guard_request("engine.execute");
        request["expectedDocument"] = expected;
        assert!(
            matches!(remote.call_guarded("engine.execute", request), Err(crate::GuardedCallError::Rejected { error_details, .. }) if error_details["code"] == "INVALID_REQUEST")
        );
    }
    assert!(matches!(remote.call_guarded("engine.execute", json!({"command":"parameters.change"})), Err(crate::GuardedCallError::Rejected { .. })));
    for method in ["engine.script", "ui.view", "document.inspect"] {
        assert!(
            matches!(remote.call_guarded(method, guard_request(method)), Err(crate::GuardedCallError::Rejected { error_details, .. }) if error_details["code"] == "UNSUPPORTED_CAPABILITY")
        );
    }
    for command in ["file.new", "component.edit_in_place", "timeline.play", "document.snapshot.capture", "panel.open"] {
        let mut request = guard_request("engine.execute");
        request["command"] = json!(command);
        assert!(matches!(remote.call_guarded("engine.execute", request), Err(crate::GuardedCallError::Rejected { .. })), "{command}");
    }
    let mut request = guard_request("engine.batch");
    request["commands"] = json!([{"command":"parameters.change"},{"command":"file.new"}]);
    assert!(matches!(remote.call_guarded("engine.batch", request), Err(crate::GuardedCallError::Rejected { .. })));
    let (connections, wire) = peer.finish_counted(remote);
    assert_eq!(connections, 0);
    assert!(wire.is_empty());

    // Execute resolves the first string, whereas batch resolves the first present Value.
    // Collect every refusal mismatch before asserting so RED observes the whole bad-type/file
    // table, not just command:null. A capable peer would otherwise receive these requests.
    let bad_commands = [Value::Null, json!(7), json!(false), json!([]), json!({})];
    let mut refused = Vec::new();
    for command in &bad_commands {
        for id in ["file.new", "doc.open", "DOC.OPEN"] {
            let mut request = guard_request("engine.execute");
            request["command"] = command.clone();
            request["id"] = json!(id);
            refused.push(request);
        }
        for key in ["path", "paths", "file", "files", "dir", "folder", "url", "out"] {
            let mut request = guard_request("engine.execute");
            request["command"] = command.clone();
            request["id"] = json!("PaRaMeTeRs.ChAnGe");
            request["params"][key] = Value::Null; // Presence, not the value's type, forbids it.
            refused.push(request);
        }
    }
    for id in [json!("parameters.change"), Value::Null, json!(false)] {
        let mut request = guard_request("engine.execute");
        request["command"] = json!("file.new");
        request["id"] = id;
        refused.push(request);
    }
    for id in ["file.new", "doc.open"] {
        let mut request = guard_request("engine.execute");
        request.as_object_mut().unwrap().remove("command");
        request["id"] = json!(id);
        refused.push(request);
    }
    let mut mismatches = Vec::new();
    for request in refused {
        let peer =
            GuardPeer::new(|_, req| GuardAnswer::Reply(if req["method"] == "control.capabilities" { guard_capabilities() } else { guard_success() }));
        let mut remote = peer.remote(ParamCheck::Off);
        let result = remote.call_guarded("engine.execute", request.clone());
        if !matches!(result, Err(crate::GuardedCallError::Rejected { ref error_details, .. }) if error_details["code"] == "UNSUPPORTED_CAPABILITY") {
            mismatches.push(format!("not locally unsupported: {request}: {result:?}"));
        }
        let (connections, wire) = peer.finish_counted(remote);
        if connections != 0 || !wire.is_empty() {
            mismatches.push(format!("refusal did I/O: {request}: connections={connections}, wire={}", wire.len()));
        }
    }

    // Native diagnostics must still own malformed/unknown commands. Neither a bad id nor an
    // unused forbidden id overrides a valid string command, and no raw spelling is rewritten.
    let mut allowed = Vec::new();
    for command in &bad_commands {
        let mut request = guard_request("engine.execute");
        request["command"] = command.clone();
        request["id"] = json!("PaRaMeTeRs.ChAnGe");
        allowed.push((request, None));
    }
    let mut id_only = guard_request("engine.execute");
    id_only.as_object_mut().unwrap().remove("command");
    id_only["id"] = json!("PaRaMeTeRs.ChAnGe");
    allowed.push((id_only, None));
    for id in [json!("file.new"), Value::Null, json!(7), json!(false), json!([]), json!({})] {
        let mut request = guard_request("engine.execute");
        request["command"] = json!("PaRaMeTeRs.ChAnGe");
        request["id"] = id;
        allowed.push((request, None));
    }
    for (command, id, error) in [
        (Value::Null, json!(false), "missing `command`"),
        (json!([]), json!({}), "missing `command`"),
        (json!(""), json!("file.new"), "unknown command ``"),
        (json!("not.a.command"), json!("file.new"), "unknown command `not.a.command`"),
    ] {
        let mut request = guard_request("engine.execute");
        request["command"] = command;
        request["id"] = id;
        allowed.push((request, Some(error)));
    }
    for (request, error) in allowed {
        let peer = GuardPeer::new(move |_, req| {
            GuardAnswer::Reply(if req["method"] == "control.capabilities" {
                guard_capabilities()
            } else if let Some(error) = error {
                json!({"ok":false,"error":error})
            } else {
                guard_success()
            })
        });
        let mut remote = peer.remote(ParamCheck::Off);
        let result = remote.call_guarded("engine.execute", request.clone());
        if let Some(error) = error {
            assert!(matches!(result, Err(crate::GuardedCallError::CommandFailed(ref message)) if message == error), "{request}: {result:?}");
        } else {
            assert!(result.is_ok(), "{request}: {result:?}");
        }
        let (connections, wire) = peer.finish_counted(remote);
        assert_eq!(connections, 1);
        assert_eq!(wire.iter().map(|(_, r)| r["method"].as_str().unwrap()).collect::<Vec<_>>(), ["control.capabilities", "engine.execute"]);
        let mut expected = request;
        expected["param_check"] = json!("off");
        assert_eq!(wire[1].1["params"], expected, "the backend preserves the original envelope");
    }

    // Batch's existing Value-first resolution must NOT acquire execute's string fallback.
    for command in &bad_commands {
        let mut request = guard_request("engine.batch");
        request["commands"] = json!([{"command":command,"id":"file.new"}]);
        let native = json!({"completed":0,"results":[],"undone":0,"failed":{"index":0,"error":"missing command"}});
        let reply = native.clone();
        let peer = GuardPeer::new(move |_, req| {
            GuardAnswer::Reply(if req["method"] == "control.capabilities" {
                guard_capabilities()
            } else {
                json!({"ok":true,"result":reply,"documentStamp":{"documentId":"doc-A","revision":"9007199254740993"}})
            })
        });
        let mut remote = peer.remote(ParamCheck::Off);
        assert_eq!(remote.call_guarded("engine.batch", request.clone()).unwrap().result, native);
        let (connections, wire) = peer.finish_counted(remote);
        assert_eq!(connections, 1);
        assert_eq!(wire.len(), 2);
        request["param_check"] = json!("off");
        assert_eq!(wire[1].1["params"], request);
    }
    for step in [json!({"id":"file.new"}), json!({"command":"file.new","id":"parameters.change"})] {
        let peer = GuardPeer::new(|_, _| GuardAnswer::Reply(guard_success()));
        let mut remote = peer.remote(ParamCheck::Off);
        let mut request = guard_request("engine.batch");
        request["commands"] = json!([step]);
        assert!(matches!(remote.call_guarded("engine.batch", request), Err(crate::GuardedCallError::Rejected { .. })));
        let (connections, wire) = peer.finish_counted(remote);
        assert_eq!(connections, 0);
        assert!(wire.is_empty());
    }
    assert!(mismatches.is_empty(), "execute policy/dispatch mismatch ({} failures):\n{}", mismatches.len(), mismatches.join("\n"));
}

// The real Remote speaks JSON lines to this socket, including its own auth/id/capability/pending
// machinery. A stop connection joins the peer deterministically; no sleeps prove event ordering.
struct GuardPeer {
    addr: String,
    stop: std::sync::Arc<std::sync::Mutex<Option<std::net::SocketAddr>>>,
    connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<Vec<(usize, Value)>>>,
}

impl GuardPeer {
    fn new(mut answer: impl FnMut(usize, &Value) -> GuardAnswer + Send + 'static) -> Self {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let stop = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stopped = stop.clone();
        let connections = std::sync::Arc::new(AtomicUsize::new(0));
        let accepted = connections.clone();
        let thread = std::thread::spawn(move || {
            let mut log = Vec::new();
            for (connection, stream) in listener.incoming().enumerate() {
                let mut stream = stream.unwrap();
                if *stopped.lock().unwrap() == Some(stream.peer_addr().unwrap()) {
                    break;
                }
                accepted.fetch_add(1, Ordering::SeqCst);
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    log.push((connection, request.clone()));
                    let (mut reply, close) = match answer(connection, &request) {
                        GuardAnswer::Reply(reply) => (reply, false),
                        GuardAnswer::ReplyAndClose(reply) => (reply, true),
                        GuardAnswer::Close => break,
                    };
                    reply["id"] = request["id"].clone();
                    if writeln!(stream, "{reply}").is_err() || close {
                        break;
                    }
                    line.clear();
                }
            }
            log
        });
        Self { addr, stop, connections, thread: Some(thread) }
    }

    fn remote(&self, level: solidcraft_engine::schema::ParamCheck) -> crate::Remote {
        let mut remote = crate::Remote::new(&self.addr, Some("0123456789abcdef0123456789abcdef")).with_param_check(level);
        remote.set_waits(quick_waits(0, 0));
        remote
    }

    fn finish(self, backend: impl Sized) -> Vec<(usize, Value)> {
        self.finish_counted(backend).1
    }

    fn finish_counted(mut self, backend: impl Sized) -> (usize, Vec<(usize, Value)>) {
        drop(backend);
        self.wake();
        let wire = self.thread.take().unwrap().join().unwrap();
        (self.connections.load(std::sync::atomic::Ordering::SeqCst), wire)
    }

    fn wake(&self) {
        // Publish the wake socket's identity under the same lock the accept loop checks.
        // Queued backend connections are counted even if accept happens only during finish.
        let mut stop = self.stop.lock().unwrap();
        if let Ok(wake) = std::net::TcpStream::connect(&self.addr) {
            *stop = wake.local_addr().ok();
        }
    }
}

impl Drop for GuardPeer {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.wake();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn guard_capabilities() -> Value {
    json!({"ok":true,"result":{"guardedExecution":{"version":"1.0","execute":true,"batch":true},"param_check":["off","warn","strict"]}})
}

fn guard_request(method: &str) -> Value {
    let mut request = if method == "engine.batch" {
        json!({"commands":[{"command":"parameters.change","params":{"name":"width","value":"10 mm"}}]})
    } else {
        json!({"command":"parameters.change","params":{"name":"width","value":"10 mm"}})
    };
    request["expectedDocument"] = json!({"documentId":"doc-A","revision":"9007199254740993"});
    request
}

fn guard_success() -> Value {
    json!({"ok":true,"result":{"completed":1,"results":[null],"undone":0,"timings":[{"index":0,"ms":1.25}]},
        "documentStamp":{"documentId":"doc-A","revision":"9007199254740994"},"param_check":"strict"})
}

// Catches negotiation on a discarded connection, unsupported downgrades and validation-level
// fallback. Both execute and batch must remain on the negotiated socket, even at off.
#[test]
fn guarded_remote_capability_is_connection_scoped_and_precedes_send() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for level in [ParamCheck::Off, ParamCheck::Warn, ParamCheck::Strict] {
        let peer = GuardPeer::new(|_, req| {
            GuardAnswer::Reply(if req["method"] == "control.capabilities" {
                guard_capabilities()
            } else {
                let mut reply = guard_success();
                reply["param_check"] = req["params"]["param_check"].clone();
                reply
            })
        });
        let mut remote = peer.remote(level);
        for method in ["engine.execute", "engine.batch"] {
            let reply = remote.call_guarded(method, guard_request(method)).unwrap();
            assert_eq!(reply.document_stamp.revision, 9_007_199_254_740_994);
            assert_eq!(reply.result["timings"][0]["ms"], 1.25);
        }
        let log = peer.finish(remote);
        assert_eq!(
            log.iter().map(|(_, r)| r["method"].as_str().unwrap()).collect::<Vec<_>>(),
            ["control.capabilities", "engine.execute", "engine.batch"]
        );
        assert!(log.iter().all(|(connection, _)| *connection == 0));
        for (_, req) in &log[1..] {
            assert_eq!(req["params"]["expectedDocument"], json!({"documentId":"doc-A","revision":"9007199254740993"}));
            assert_eq!(req["params"]["param_check"], level.name());
        }
        for capability in [
            json!({"ok":true,"result":{}}),
            json!({"ok":true,"result":{"guardedExecution":false}}),
            json!({"ok":true,"result":{"guardedExecution":{"version":"2.0","execute":true,"batch":true}}}),
            json!({"ok":false,"error":"unknown method `control.capabilities`"}),
        ] {
            let peer = GuardPeer::new(move |_, _| GuardAnswer::Reply(capability.clone()));
            let mut remote = peer.remote(level);
            assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")),
                Err(crate::GuardedCallError::Rejected { error_details, .. }) if error_details["code"] == "UNSUPPORTED_CAPABILITY"));
            assert_eq!(peer.finish(remote).len(), 1, "no mutation at any level");
        }
    }
    for method in ["engine.execute", "engine.batch"] {
        let field = if method == "engine.execute" { "execute" } else { "batch" };
        let peer = GuardPeer::new(move |_, _| {
            let mut caps = guard_capabilities();
            caps["result"]["guardedExecution"][field] = json!(false);
            GuardAnswer::Reply(caps)
        });
        let mut remote = peer.remote(ParamCheck::Off);
        assert!(matches!(remote.call_guarded(method, guard_request(method)), Err(crate::GuardedCallError::Rejected { .. })));
        assert_eq!(peer.finish(remote).len(), 1);
    }
    for level in [ParamCheck::Warn, ParamCheck::Strict] {
        let peer = GuardPeer::new(|_, _| {
            let mut caps = guard_capabilities();
            caps["result"]["param_check"] = json!(["off"]);
            GuardAnswer::Reply(caps)
        });
        let mut remote = peer.remote(level);
        assert!(matches!(remote.call_guarded("engine.batch", guard_request("engine.batch")), Err(crate::GuardedCallError::Rejected { .. })));
        assert_eq!(peer.finish(remote).len(), 1);
    }
}

// Catches connected helpers silently reconnecting after negotiation and stale cached capability.
#[test]
fn guarded_remote_connected_helpers_never_reconnect() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    let peer =
        GuardPeer::new(
            |_, req| if req["method"] == "control.capabilities" { GuardAnswer::ReplyAndClose(guard_capabilities()) } else { GuardAnswer::Close },
        );
    let mut remote = peer.remote(ParamCheck::Off);
    remote.ensure_guarded_capability("engine.execute").unwrap();
    assert!(matches!(
        remote.call_guarded_connected("engine.execute", guard_request("engine.execute")),
        Err(crate::GuardedCallError::OutcomeUnknown(_))
    ));
    assert!(remote.roundtrip_connected("{}").is_err());
    assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::OutcomeUnknown(_))));
    assert_eq!(peer.finish(remote).iter().filter(|(_, r)| r["method"] == "control.capabilities").count(), 1);

    let peer = GuardPeer::new(|connection, _| {
        GuardAnswer::ReplyAndClose(if connection == 0 { guard_capabilities() } else { json!({"ok":true,"result":{}}) })
    });
    let mut remote = peer.remote(ParamCheck::Off);
    remote.ensure_guarded_capability("engine.execute").unwrap();
    assert!(remote.roundtrip_connected("{}").is_err()); // observes EOF and invalidates the cache
    assert!(matches!(remote.call_guarded("engine.batch", guard_request("engine.batch")), Err(crate::GuardedCallError::Rejected { .. })));
    let log = peer.finish(remote);
    assert_eq!(log.iter().filter(|(_, r)| r["method"] == "control.capabilities").map(|(c, _)| *c).collect::<Vec<_>>(), [0, 1]);
    assert!(!log.iter().any(|(_, r)| r["method"] == "engine.batch"));
}

// Catches connected methods creating a connection at all, and classifying a never-connected
// request as pending/unknown rather than provably unsent.
#[test]
fn guarded_remote_no_connection_is_not_sent_and_does_not_create_pending() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    let peer = GuardPeer::new(|_, _| GuardAnswer::Reply(guard_success()));
    let mut remote = peer.remote(ParamCheck::Off);
    assert!(remote.roundtrip_connected("{}").is_err());
    assert!(matches!(remote.call_guarded_connected("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::NotSent(_))));
    assert_eq!(remote.call("control.pending", json!({})).unwrap(), json!({"pending":null}));
    assert!(peer.finish(remote).is_empty());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let mut remote = crate::Remote::new(&addr, Some("0123456789abcdef0123456789abcdef"));
    assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::NotSent(_))));
    assert_eq!(remote.call("control.pending", json!({})).unwrap(), json!({"pending":null}));
}

// Catches either old execute retry or old send_batch routing; one received write is already an
// uncertain effect. Ordinary reconnect/empty pending must not clear that uncertainty.
#[test]
fn guarded_remote_never_resends_after_write_or_reply_loss() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for method in ["engine.execute", "engine.batch"] {
        let peer = GuardPeer::new(|_, req| match req["method"].as_str() {
            Some("control.capabilities") => GuardAnswer::Reply(guard_capabilities()),
            Some("control.running") => GuardAnswer::Reply(json!({"ok":true,"result":{"running":[]}})),
            _ => GuardAnswer::Close,
        });
        let mut remote = peer.remote(ParamCheck::Off);
        assert!(matches!(remote.call_guarded(method, guard_request(method)), Err(crate::GuardedCallError::OutcomeUnknown(_))));
        remote.call("control.running", json!({})).unwrap(); // ordinary reconnect is permitted
        let _ = remote.call("control.pending", json!({}));
        assert!(matches!(remote.call_guarded(method, guard_request(method)), Err(crate::GuardedCallError::OutcomeUnknown(_))));
        let log = peer.finish(remote);
        assert_eq!(log.iter().filter(|(_, r)| r["method"] == method).count(), 1);
        assert_eq!(log.iter().filter(|(_, r)| r["method"] == "control.capabilities").count(), 1);
    }
}

// Catches old recursive retry_busy and claiming an unrun request is itself pending.
#[test]
fn guarded_remote_busy_is_refused_without_retry_or_false_pending() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    let peer = GuardPeer::new(|_, req| {
        GuardAnswer::Reply(if req["method"] == "control.capabilities" {
            guard_capabilities()
        } else {
            json!({"ok":false,"busy":true,"unrun":true,"error":"an earlier request is running"})
        })
    });
    let mut remote = peer.remote(ParamCheck::Off);
    assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::CommandFailed(_))));
    assert_eq!(remote.call("control.pending", json!({})).unwrap(), json!({"pending":null}));
    let log = peer.finish(remote);
    assert_eq!(log.len(), 2);
}

// Catches metadata stripping through outcome(), pending-slot inference and loss of the latch.
#[test]
fn guarded_remote_pending_keeps_original_reply_and_unknown() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for original in [
        guard_success(),
        json!({"ok":false,"error":"stale","errorDetails":{"code":"STALE_DOCUMENT","retry":"never",
        "message":"stale","details":{"expected":{"documentId":"doc-A","revision":"9007199254740993"},"current":null}}}),
    ] {
        let reply = original.clone();
        let mut polls = 0;
        let peer = GuardPeer::new(move |_, req| {
            GuardAnswer::Reply(match req["method"].as_str() {
                Some("control.capabilities") => guard_capabilities(),
                Some("engine.batch") => running(),
                Some("control.pending") => {
                    polls += 1;
                    if polls == 1 {
                        json!({"ok":true,"result":{"pending":2,"done":false}})
                    } else {
                        json!({"ok":true,"result":{"pending":2,"done":true,"reply":reply}})
                    }
                }
                _ => json!({"ok":true,"result":null}),
            })
        });
        let mut remote = peer.remote(ParamCheck::Strict);
        assert!(matches!(remote.call_guarded("engine.batch", guard_request("engine.batch")), Err(crate::GuardedCallError::Pending(_))));
        let pending = remote.call("control.pending", json!({})).unwrap();
        // The peer adds the wire request id; every other original sibling stays byte-value equal.
        let mut actual = pending["reply"].clone();
        actual.as_object_mut().unwrap().remove("id");
        assert_eq!(actual, original);
        assert_eq!(pending["method"], "engine.batch");
        assert_eq!(pending["guarded"], true);
        let log = peer.finish(remote);
        assert_eq!(log.iter().filter(|(_, r)| r["method"] == "engine.batch").count(), 1);
        assert!(log.iter().all(|(c, _)| *c == 0));
    }
    for empty in [false, true] {
        let peer = GuardPeer::new(move |_, req| match req["method"].as_str() {
            Some("control.capabilities") => GuardAnswer::Reply(guard_capabilities()),
            Some("engine.execute") => GuardAnswer::Reply(running()),
            Some("control.pending") if empty => GuardAnswer::Reply(json!({"ok":true,"result":{"pending":null}})),
            _ => GuardAnswer::Close,
        });
        let mut remote = peer.remote(ParamCheck::Off);
        assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::OutcomeUnknown(_))));
        let _ = remote.call("control.pending", json!({}));
        assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::OutcomeUnknown(_))));
        assert_eq!(peer.finish(remote).iter().filter(|(_, r)| r["method"] == "engine.execute").count(), 1);
    }
}

// Catches the immediate running->done branch stripping the stamp/stale details, and the later
// pending tool dropping its disconnect uncertainty before the next guarded request.
#[test]
fn guarded_remote_immediate_and_later_pending_preserve_effect_facts() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for stale in [false, true] {
        let peer = GuardPeer::new(move |_, req| {
            GuardAnswer::Reply(match req["method"].as_str() {
                Some("control.capabilities") => guard_capabilities(),
                Some("engine.execute") => running(),
                _ => json!({"ok":true,"result":{"pending":1,"done":true,"reply": if stale {
                json!({"ok":false,"error":"stale","errorDetails":{"code":"STALE_DOCUMENT","details":{"current":null}}})
            } else { guard_success() }}}),
            })
        });
        let mut remote = peer.remote(ParamCheck::Strict);
        let reply = remote.call_guarded("engine.execute", guard_request("engine.execute"));
        if stale {
            assert!(matches!(reply, Err(crate::GuardedCallError::Rejected { error_details, .. })
            if error_details == json!({"code":"STALE_DOCUMENT","details":{"current":null}})));
        } else {
            let reply = reply.unwrap();
            assert_eq!(reply.document_stamp.revision, 9_007_199_254_740_994);
            assert_eq!(reply.result["completed"], 1);
        }
        assert_eq!(peer.finish(remote).len(), 3);
    }
    let mut polls = 0;
    let peer = GuardPeer::new(move |_, req| match req["method"].as_str() {
        Some("control.capabilities") => GuardAnswer::Reply(guard_capabilities()),
        Some("engine.execute") => GuardAnswer::Reply(running()),
        Some("control.pending") => {
            polls += 1;
            if polls == 1 { GuardAnswer::Reply(json!({"ok":true,"result":{"pending":1,"done":false}})) } else { GuardAnswer::Close }
        }
        _ => GuardAnswer::Reply(json!({"ok":true,"result":{"running":[]}})),
    });
    let mut remote = peer.remote(ParamCheck::Off);
    assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::Pending(_))));
    let pending = remote.call("control.pending", json!({})).unwrap();
    assert_eq!(pending["lost"], true);
    assert_eq!(pending["guarded"], true);
    assert_eq!(pending["method"], "engine.execute");
    remote.call("control.running", json!({})).unwrap();
    let _ = remote.call("control.pending", json!({}));
    assert!(matches!(remote.call_guarded("engine.batch", guard_request("engine.batch")), Err(crate::GuardedCallError::OutcomeUnknown(_))));
    assert_eq!(peer.finish(remote).iter().filter(|(_, r)| r["method"] == "engine.execute").count(), 1);
}

// Catches a new guarded request inheriting the old request's Pending status and losing the
// original settled stamp/error. Completion of the earlier call is not a retry of the new one.
#[test]
fn guarded_remote_settles_original_before_new_write() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    let mut polls = 0;
    let peer = GuardPeer::new(move |_, req| {
        GuardAnswer::Reply(match req["method"].as_str() {
            Some("control.capabilities") => guard_capabilities(),
            Some("control.pending") => {
                polls += 1;
                if polls <= 2 {
                    json!({"ok":true,"result":{"pending":1,"done":false}})
                } else {
                    json!({"ok":true,"result":{"pending":1,"done":true,"reply":guard_success()}})
                }
            }
            Some("engine.batch") => running(),
            _ => guard_success(),
        })
    });
    let mut remote = peer.remote(ParamCheck::Strict);
    assert!(matches!(remote.call_guarded("engine.batch", guard_request("engine.batch")), Err(crate::GuardedCallError::Pending(_))));
    assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::NotSent(_))));
    remote.call_guarded("engine.execute", guard_request("engine.execute")).unwrap();
    let pending = remote.call("control.pending", json!({})).unwrap();
    assert_eq!(pending["method"], "engine.batch");
    assert_eq!(pending["reply"]["documentStamp"], json!({"documentId":"doc-A","revision":"9007199254740994"}));
    assert_eq!(peer.finish(remote).iter().filter(|(_, r)| r["method"] == "engine.execute").count(), 1);
}

// Catches apparently successful replies being certified without their stamp or requested echo.
#[test]
fn guarded_remote_missing_metadata_is_unknown_and_latched() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for level in [ParamCheck::Off, ParamCheck::Warn, ParamCheck::Strict] {
        for missing_stamp in [true, false] {
            if level == ParamCheck::Off && !missing_stamp {
                continue;
            }
            let peer = GuardPeer::new(move |_, req| {
                let mut reply = guard_success();
                if req["method"] == "control.capabilities" {
                    return GuardAnswer::Reply(guard_capabilities());
                }
                reply["param_check"] = json!(level.name());
                reply.as_object_mut().unwrap().remove(if missing_stamp { "documentStamp" } else { "param_check" });
                GuardAnswer::Reply(reply)
            });
            let mut remote = peer.remote(level);
            for _ in 0..2 {
                assert!(matches!(
                    remote.call_guarded("engine.execute", guard_request("engine.execute")),
                    Err(crate::GuardedCallError::OutcomeUnknown(_))
                ));
            }
            assert_eq!(peer.finish(remote).len(), 2);
        }
    }
}

// Catches a terminal pending reply first classified unknown being resurrected as done/success
// by the settled cache. Its raw facts remain available, but never certify a guarded success.
#[test]
fn guarded_remote_pending_missing_metadata_remains_unknown() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for (missing_stamp, deferred) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut original = guard_success();
        original.as_object_mut().unwrap().remove(if missing_stamp { "documentStamp" } else { "param_check" });
        let raw = original.clone();
        let mut polls = 0;
        let peer = GuardPeer::new(move |_, req| {
            GuardAnswer::Reply(match req["method"].as_str() {
                Some("control.capabilities") => guard_capabilities(),
                Some("engine.batch") => running(),
                _ => {
                    polls += 1;
                    if deferred && polls == 1 {
                        json!({"ok":true,"result":{"pending":1,"done":false}})
                    } else {
                        json!({"ok":true,"result":{"pending":1,"done":true,"reply":raw}})
                    }
                }
            })
        });
        let mut remote = peer.remote(ParamCheck::Strict);
        let reply = remote.call_guarded("engine.batch", guard_request("engine.batch"));
        if deferred {
            assert!(matches!(reply, Err(crate::GuardedCallError::Pending(_))));
        } else {
            assert!(matches!(reply, Err(crate::GuardedCallError::OutcomeUnknown(_))));
        }
        let pending = remote.call("control.pending", json!({})).unwrap();
        assert_eq!(pending["lost"], true, "unknown must not reappear as a certified done: {pending}");
        assert_ne!(pending["done"], true);
        assert_eq!(pending["reply"], original, "retain raw effect facts even when unconfirmed");
        assert_eq!(pending["method"], "engine.batch");
        assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::OutcomeUnknown(_))));
        assert_eq!(peer.finish(remote).len(), if deferred { 4 } else { 3 });
    }
}

// A tool's unknown classification must also latch the Remote that actually sent the batch.
#[test]
fn guarded_remote_nonobject_batch_immediate_latches_after_reconnect() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for level in [ParamCheck::Off, ParamCheck::Strict] {
        for native in [json!(7), Value::Null, json!([1]), json!("native")] {
            let peer = GuardPeer::new(move |_, req| match req["method"].as_str() {
                Some("control.capabilities") => GuardAnswer::Reply(guard_capabilities()),
                Some("control.running") => GuardAnswer::ReplyAndClose(json!({"ok":true,"result":{"running":[]}})),
                _ => {
                    let mut reply = guard_success();
                    reply["result"] = native.clone();
                    GuardAnswer::Reply(reply)
                }
            });
            let mut remote = peer.remote(level);
            let first = crate::call_tool(&mut remote, "batch", &guard_request("engine.batch"));
            assert_eq!(first["structuredContent"]["outcomeUnknown"], true, "{first}");
            for method in ["engine.execute", "engine.batch"] {
                assert!(matches!(remote.call_guarded(method, guard_request(method)), Err(crate::GuardedCallError::OutcomeUnknown(_))));
            }
            let _ = remote.call("control.pending", json!({}));
            remote.call("control.running", json!({})).unwrap();
            remote.call("control.running", json!({})).unwrap();
            for method in ["engine.execute", "engine.batch"] {
                assert!(matches!(remote.call_guarded(method, guard_request(method)), Err(crate::GuardedCallError::OutcomeUnknown(_))));
            }
            let (connections, wire) = peer.finish_counted(remote);
            assert!(connections >= 2, "ordinary call really reconnected");
            assert_eq!(wire.iter().filter(|(_, r)| matches!(r["method"].as_str(), Some("engine.execute" | "engine.batch"))).count(), 1);
        }
    }
}

// All terminal paths share the original method, retain raw effect facts as Lost, and never
// resurrect an unconfirmed batch when pending is consumed or an ordinary call reconnects.
#[test]
fn guarded_remote_nonobject_batch_pending_latches_all_finish_paths() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for route in ["within", "pending", "guarded", "ordinary"] {
        for native in [json!(7), Value::Null, json!([])] {
            let mut original = guard_success();
            original["result"] = native;
            let raw = original.clone();
            let mut polls = 0;
            let peer = GuardPeer::new(move |_, req| match req["method"].as_str() {
                Some("control.capabilities") => GuardAnswer::Reply(guard_capabilities()),
                Some("engine.batch") => GuardAnswer::Reply(running()),
                Some("control.pending") => {
                    polls += 1;
                    GuardAnswer::Reply(if route != "within" && polls == 1 {
                        json!({"ok":true,"result":{"pending":1,"done":false}})
                    } else {
                        json!({"ok":true,"result":{"pending":1,"done":true,"reply":raw}})
                    })
                }
                Some("control.running") => GuardAnswer::ReplyAndClose(json!({"ok":true,"result":{"running":[]}})),
                _ => GuardAnswer::Reply(guard_success()),
            });
            let mut remote = peer.remote(ParamCheck::Strict);
            let first = remote.call_guarded("engine.batch", guard_request("engine.batch"));
            if route == "within" {
                assert!(matches!(first, Err(crate::GuardedCallError::OutcomeUnknown(_))), "{route}");
            } else {
                assert!(matches!(first, Err(crate::GuardedCallError::Pending(_))), "{route}");
            }
            if route == "guarded" {
                assert!(matches!(
                    remote.call_guarded("engine.execute", guard_request("engine.execute")),
                    Err(crate::GuardedCallError::OutcomeUnknown(_))
                ));
            } else if route == "ordinary" {
                remote.call("engine.commands", json!({})).unwrap();
            }
            let pending = remote.call("control.pending", json!({})).unwrap();
            assert_eq!(pending["lost"], true, "{route}: {pending}");
            assert_ne!(pending["done"], true);
            assert_eq!(pending["reply"], original);
            assert_eq!(pending["method"], "engine.batch");
            assert_eq!(pending["guarded"], true);
            assert_eq!(pending["steps"], 1);
            assert_eq!(remote.call("control.pending", json!({})).unwrap()["pending"], Value::Null);
            remote.call("control.running", json!({})).unwrap();
            remote.call("control.running", json!({})).unwrap();
            for method in ["engine.execute", "engine.batch"] {
                assert!(matches!(remote.call_guarded(method, guard_request(method)), Err(crate::GuardedCallError::OutcomeUnknown(_))));
            }
            let (connections, wire) = peer.finish_counted(remote);
            assert!(connections >= 2);
            assert_eq!(wire.iter().filter(|(_, r)| matches!(r["method"].as_str(), Some("engine.execute" | "engine.batch"))).count(), 1);
        }
    }
}

// Catches overwriting a per-request validation level with the bridge default, accepting the
// wrong echo, or treating a partial write as a provable zero-send failure.
#[test]
fn guarded_remote_request_level_and_partial_write_fail_closed() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    for echo in ["strict", "off"] {
        let peer = GuardPeer::new(move |_, req| {
            GuardAnswer::Reply(if req["method"] == "control.capabilities" {
                guard_capabilities()
            } else {
                let mut reply = guard_success();
                reply["param_check"] = json!(echo);
                reply
            })
        });
        let mut remote = peer.remote(ParamCheck::Off);
        let mut request = guard_request("engine.execute");
        request["param_check"] = json!("strict");
        let reply = remote.call_guarded("engine.execute", request);
        if echo == "strict" {
            assert!(reply.is_ok(), "{reply:?}");
        } else {
            assert!(matches!(reply, Err(crate::GuardedCallError::OutcomeUnknown(_))));
        }
        let log = peer.finish(remote);
        assert_eq!(log[1].1["params"]["param_check"], "strict");
    }
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["method"], "control.capabilities");
        let mut caps = guard_capabilities();
        caps["id"] = req["id"].clone();
        writeln!(stream, "{caps}").unwrap();
        let mut first = [0];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(first[0], b'{');
        stream.shutdown(std::net::Shutdown::Both).unwrap();
        1 // one mutation began; it need not have reached the owner to make its effect uncertain
    });
    let mut remote = crate::Remote::new(&addr, Some("0123456789abcdef0123456789abcdef"));
    let mut request = guard_request("engine.execute");
    request["payload"] = json!("x".repeat(16 << 20));
    assert!(matches!(remote.call_guarded("engine.execute", request), Err(crate::GuardedCallError::OutcomeUnknown(_))));
    assert_eq!(peer.join().unwrap(), 1);
    assert!(matches!(remote.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::OutcomeUnknown(_))));
}

// Catches new Backend defaults calling old call(), Auto double-delegation/fallback, and built-in
// legacy network calls dropping a guard (even a null guard on engine.script).
#[test]
fn guarded_backend_default_auto_and_legacy_entry_do_not_downgrade() {
    use crate::Backend;
    use solidcraft_engine::schema::ParamCheck;
    struct Legacy(usize);
    impl Backend for Legacy {
        fn call(&mut self, _: &str, _: Value) -> Result<Value, String> {
            self.0 += 1;
            Ok(Value::Null)
        }
        fn has_ui(&self) -> bool {
            false
        }
        fn describe(&self) -> String {
            String::new()
        }
    }
    let mut legacy = Legacy(0);
    assert!(matches!(legacy.call_guarded("engine.execute", guard_request("engine.execute")), Err(crate::GuardedCallError::Rejected { .. })));
    assert_eq!(legacy.0, 0);
    let peer =
        GuardPeer::new(|_, req| GuardAnswer::Reply(if req["method"] == "control.capabilities" { guard_capabilities() } else { guard_success() }));
    let port = peer.addr.rsplit(':').next().unwrap().parse().unwrap();
    let mut auto = crate::Auto::new(Headless::default(), Box::new(move || Some(port)), Some("0123456789abcdef0123456789abcdef"));
    auto.call_guarded("engine.execute", guard_request("engine.execute")).unwrap();
    assert_eq!(peer.finish(auto).len(), 2, "one capability + exactly one actual backend write");
    let mut auto = crate::Auto::new(Headless::default(), Box::new(|| None), None);
    assert!(
        matches!(auto.call_guarded("engine.batch", guard_request("engine.batch")), Err(crate::GuardedCallError::Rejected { .. })),
        "Headless awaits Task6, not legacy fallback"
    );
    for method in ["engine.execute", "engine.batch", "engine.script"] {
        let peer = GuardPeer::new(|_, _| GuardAnswer::Reply(json!({"ok":true,"result":null})));
        let mut remote = peer.remote(ParamCheck::Strict);
        let mut request = guard_request(method);
        request["expectedDocument"] = Value::Null;
        assert!(remote.call(method, request).is_err_and(|e| e.contains("call_guarded")));
        assert!(peer.finish(remote).is_empty());
    }
}

/// A bridge made before the app runs keeps its tools (#270): calls say the app is not running and
/// how to start it (a batch was never sent, so it may not "have run"), and reach the app once it
/// listens.
#[test]
fn a_bridge_made_before_the_app_runs_connects_once_it_does() {
    use std::io::{BufRead, BufReader, Write};

    use crate::backend::Backend;
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");
    let mut remote = crate::Remote::new(&addr, Some("0123456789abcdef0123456789abcdef"));
    // Still "connected to the SolidCraft app", which the skills read as bridged.
    assert!(remote.describe().starts_with("connected to the SolidCraft app") && remote.describe().contains("not running"), "{}", remote.describe());
    let e = remote.call("engine.execute", json!({"command": "inspect.measure"})).unwrap_err();
    assert!(e.starts_with(crate::backend::NOT_RUNNING) && e.contains(&format!("solidcraft --control {port}")), "{e}");
    let batch = crate::call_tool(&mut remote, "batch", &json!({"commands": [{"command": "solid.box"}]}));
    let text = batch["content"][0]["text"].as_str().unwrap_or_default();
    assert_eq!(batch["isError"], true);
    assert!(text.starts_with(crate::backend::NOT_RUNNING) && !text.contains("may have run"), "{text}");

    let listener = std::net::TcpListener::bind(&addr).unwrap();
    let app = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        writeln!(&stream, "{}", json!({"id": req["id"], "ok": true, "result": {"answered": req["params"]["command"]}})).unwrap();
    });
    let got = remote.call("engine.execute", json!({"command": "inspect.measure"})).unwrap();
    assert_eq!(got["answered"], "inspect.measure");
    assert!(remote.describe().starts_with("connected") && !remote.describe().contains("not running"), "{}", remote.describe());
    app.join().unwrap();
}

/// An app that quits while it runs a request may have applied it: the retry then finds nothing
/// listening, and the error says the call may have run, not just that the app is not running.
#[test]
fn an_app_that_quits_during_a_call_may_have_run_it() {
    use std::io::{BufRead, BufReader};

    use crate::backend::Backend;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
        // Quit without answering: the port closes first, so the retry is refused.
        drop(listener);
        drop(stream);
    });
    let mut remote = crate::Remote::connect(&addr).unwrap();
    let e = remote.call("engine.execute", json!({"command": "solid.box"})).unwrap_err();
    app.join().unwrap();
    assert!(!e.starts_with(crate::backend::NOT_RUNNING) && e.contains("may have quit") && e.contains("Inspect the design"), "{e}");
}

/// Tool annotations never understate what a tool may do (#205): `screenshot` writes a file when
/// given a `path`, and `execute`, `batch`, `save`, `export` and `open` can overwrite or delete
/// files, so none of them is read-only and all are destructive and reach the file system.
#[test]
fn tool_annotations_tell_the_whole_truth() {
    let tools = crate::tool_definitions();
    let hints = |name: &str| {
        let t = tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("no tool {name}"));
        let a = &t["annotations"];
        (a["readOnlyHint"].as_bool().unwrap(), a["destructiveHint"].as_bool().unwrap(), a["openWorldHint"].as_bool().unwrap())
    };
    for name in ["execute", "batch", "save", "export", "open", "screenshot"] {
        assert_eq!(hints(name), (false, true, true), "{name}");
    }
    for name in
        ["list_commands", "describe_command", "inspect_design", "measure", "body_topology", "crash_reports", "skill_reference", "running", "pending"]
    {
        assert_eq!(hints(name), (true, false, false), "{name}");
    }
    assert_eq!(hints("new_design"), (false, true, false));
    for name in ["set_parameter", "check_parameters", "undo", "redo"] {
        assert_eq!(hints(name), (false, false, false), "{name}");
    }
    assert!(tools.iter().all(|t| t["annotations"]["readOnlyHint"].is_boolean()), "every tool is annotated");
}

/// `measure` with `items` (#1392): a sketch circle's diameter and radius, and two circles'
/// centre-to-centre with the closest and farthest distances.
#[test]
fn measure_items_gives_diameters_and_centre_distances() {
    let mut s = server();
    tool(&mut s, "execute", json!({"command": "sketch.create", "params": {"plane": "XY"}}));
    let a = tool(&mut s, "execute", json!({"command": "sketch.circle.center", "params": {"center": [0, 0], "radius": 3}}));
    let b = tool(&mut s, "execute", json!({"command": "sketch.circle.center", "params": {"center": [30, 40], "radius": 2}}));
    let id = |r: &Value| r["curves"][0].as_str().or(r["result"]["curves"][0].as_str()).unwrap().to_string();
    let (a, b) = (id(&a), id(&b));
    let one = tool(&mut s, "measure", json!({"items": [{"type": "sketch_curve", "id": a}]}));
    assert_eq!((one["diameter"].as_f64(), one["radius"].as_f64()), (Some(6.0), Some(3.0)), "{one}");
    let two = tool(&mut s, "measure", json!({"items": [{"type": "sketch_curve", "id": a}, {"type": "sketch_curve", "id": b}]}));
    assert!((two["center_distance"].as_f64().unwrap() - 50.0).abs() < 1e-9, "{two}");
    assert!((two["min"].as_f64().unwrap() - 45.0).abs() < 1e-9 && (two["max"].as_f64().unwrap() - 55.0).abs() < 1e-9, "{two}");
    assert!(two["distance_mm"].is_number() && two["from"].is_array(), "{two}");
}

/// #1833: `timings` / `profile` time every step run, the failing one included, headless with the
/// kernel's share; bridged, the app is asked to time them and its numbers are passed on.
#[test]
fn batch_reports_timings_and_profile() {
    let mut s = server();
    let steps = json!([
        {"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}},
        {"command": "solid.box", "params": {"length": 4, "width": 4, "height": 30, "operation": "join"}},
    ]);
    let r = raw_tool(&mut s, "batch", json!({"commands": steps, "profile": true}));
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let rows = body["timings"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{body}");
    assert_eq!((rows[1]["index"].as_u64(), rows[1]["command"].as_str()), (Some(1), Some("solid.box")));
    assert!(rows[1]["profile"]["boolean_ms"].as_f64().unwrap() > 0.0, "{body}");
    assert!(body["total_ms"].as_f64().is_some(), "{body}");

    let r = raw_tool(
        &mut s,
        "batch",
        json!({"timings": true, "commands": [{"command": "solid.box", "params": {"length": 1, "width": 1, "height": 1}}, {"command": "solid.extrude", "params": {"distance": "bogus +"}}]}),
    );
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["timings"].as_array().map(Vec::len), Some(2), "the failing step is timed too: {body}");
    assert!(body["timings"][0].get("profile").is_none());
    // Not asked: no timings.
    let d = tool(&mut s, "batch", json!({"commands": [{"command": "solid.box", "params": {"length": 1, "width": 1, "height": 1}}]}));
    assert!(d.get("timings").is_none(), "{d}");

    let reply =
        json!({"completed": 1, "results": [{"x": 1}], "undone": 0, "timings": [{"index": 0, "command": "a.one", "ms": 2.5}], "total_ms": 2.6});
    let (b, body, _) = recorded_batch(Ok(reply), json!({"commands": [{"command": "a.one"}], "profile": true}));
    assert_eq!(b.calls[0].1["timings"], true);
    assert_eq!(b.calls[0].1["profile"], true);
    assert_eq!((body["timings"][0]["ms"].as_f64(), body["total_ms"].as_f64()), (Some(2.5), Some(2.6)), "{body}");
    // An app without engine.batch is timed step by step here, without a kernel split it can't measure.
    let (_, body, _) = recorded_batch(Err("unknown method `engine.batch`".into()), json!({"commands": [{"command": "a.one"}], "profile": true}));
    assert_eq!(body["timings"][0]["command"], "a.one", "{body}");
    assert!(body["timings"][0].get("profile").is_none(), "{body}");
}

/// A fake app on a loopback port that answers every request line with `answer(request)`. The
/// shared state counts connections and records each request's method and command.
#[allow(clippy::type_complexity)]
fn scripted_app(answer: impl Fn(&Value) -> Value + Send + Sync + 'static) -> (String, std::sync::Arc<std::sync::Mutex<(usize, Vec<String>)>>) {
    use std::io::{BufRead, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = std::sync::Arc::new(std::sync::Mutex::new((0usize, Vec::<String>::new())));
    let log = seen.clone();
    let answer = std::sync::Arc::new(answer);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            log.lock().unwrap().0 += 1;
            let (log, answer) = (log.clone(), answer.clone());
            std::thread::spawn(move || {
                let mut out = stream.try_clone().unwrap();
                let mut r = std::io::BufReader::new(stream);
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let what = match req["params"]["command"].as_str() {
                        Some(c) => format!("{} {c}", req["method"].as_str().unwrap_or("")),
                        None => req["method"].as_str().unwrap_or("").to_string(),
                    };
                    log.lock().unwrap().1.push(what);
                    let mut reply = answer(&req);
                    reply["id"] = req["id"].clone();
                    if writeln!(out, "{reply}").is_err() {
                        return;
                    }
                    line.clear();
                }
            });
        }
    });
    (addr, seen)
}

fn quick_waits(pending_ms: u64, settle_ms: u64) -> crate::backend::Waits {
    use std::time::Duration;
    crate::backend::Waits { pending: Duration::from_millis(pending_ms), settle: Duration::from_millis(settle_ms), poll: Duration::from_millis(5) }
}

fn running() -> Value {
    json!({"ok": false, "running": true, "error": "timeout: still running in the app"})
}

/// #1618 over MCP: a request the app says is still running is waited for on the same
/// connection (asking `control.pending`), and the call returns its real result.
#[test]
fn a_timed_out_request_is_waited_for_and_returns_its_result() {
    use crate::backend::Backend;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let polls = AtomicUsize::new(0);
    let (addr, seen) = scripted_app(move |req| match req["method"].as_str() {
        Some("engine.execute") => running(),
        Some("control.pending") if polls.fetch_add(1, Ordering::SeqCst) < 2 => {
            json!({"ok": true, "result": {"pending": 1, "method": "engine.execute", "done": false}})
        }
        Some("control.pending") => {
            json!({"ok": true, "result": {"pending": 1, "method": "engine.execute", "done": true, "reply": {"ok": true, "result": {"filleted": true}}}})
        }
        _ => json!({"ok": true, "result": null}),
    });
    let mut remote = crate::Remote::connect(&addr).unwrap();
    remote.set_waits(quick_waits(5000, 1000));
    let r = remote.call("engine.execute", json!({"command": "solid.fillet"})).unwrap();
    assert_eq!(r, json!({"filleted": true}));
    let (connections, methods) = seen.lock().unwrap().clone();
    assert_eq!(connections, 1, "the reply waits on the connection: it is kept");
    assert_eq!(methods, ["engine.execute solid.fillet", "control.pending", "control.pending", "control.pending"]);
    // Nothing is outstanding any more.
    assert_eq!(remote.call("control.pending", json!({})).unwrap(), json!({"pending": null}));
}

/// #3654 with #1618: `control.running` is sent at once while a request still runs, not held
/// back behind it as other calls are, and asking it leaves that request outstanding.
#[test]
fn running_is_asked_at_once_behind_a_request_still_running() {
    use crate::backend::{Backend, STILL_RUNNING};
    let (addr, seen) = scripted_app(|req| match req["method"].as_str() {
        Some("engine.execute") => running(),
        Some("control.pending") => json!({"ok": true, "result": {"pending": 1, "method": "engine.execute", "done": false}}),
        Some("control.running") => json!({"ok": true, "result": {"running": [{"id": 1, "method": "engine.execute", "self": true}]}}),
        _ => json!({"ok": true, "result": "ran"}),
    });
    let mut remote = crate::Remote::connect(&addr).unwrap();
    remote.set_waits(quick_waits(20, 20));
    assert!(remote.call("engine.execute", json!({"command": "solid.fillet"})).unwrap_err().starts_with(STILL_RUNNING));
    std::mem::take(&mut seen.lock().unwrap().1);
    let r = crate::call_tool(&mut remote, "running", &json!({}));
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(seen.lock().unwrap().1, ["control.running"], "not behind the request still running");
    let e = remote.call("document.inspect", json!({})).unwrap_err();
    assert!(e.starts_with(STILL_RUNNING) && e.contains("earlier request"), "still outstanding: {e}");
}

/// A request still running after the wait is left to the `pending` tool: the error says so
/// (and that it will still apply, or only write its file), later calls are not sent behind it, and `pending` returns
/// its result once it lands, on the same connection.
#[test]
fn a_request_still_running_is_left_to_the_pending_tool() {
    use crate::backend::{Backend, STILL_RUNNING};
    use std::sync::atomic::{AtomicBool, Ordering};
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    let (addr, seen) = scripted_app(move |req| match req["method"].as_str() {
        Some("engine.execute") if req["params"]["command"] != "inspect.measure" => running(),
        Some("control.pending") if finished.load(Ordering::SeqCst) => {
            json!({"ok": true, "result": {"pending": 1, "method": "engine.execute", "done": true, "reply": {"ok": true, "result": {"r": 1}}}})
        }
        Some("control.pending") => json!({"ok": true, "result": {"pending": 1, "method": "engine.execute", "done": false}}),
        _ => json!({"ok": true, "result": "ran"}),
    });
    let mut remote = crate::Remote::connect(&addr).unwrap();
    remote.set_waits(quick_waits(60, 60));
    let e = remote.call("engine.execute", json!({"command": "solid.fillet"})).unwrap_err();
    assert!(e.starts_with(STILL_RUNNING) && e.contains("solid.fillet") && e.contains("will still apply") && e.contains("`pending`"), "{e}");
    // Not sent behind it.
    let e = remote.call("document.inspect", json!({})).unwrap_err();
    assert!(e.starts_with(STILL_RUNNING) && e.contains("earlier request") && e.contains("not sent"), "{e}");
    assert!(!seen.lock().unwrap().1.iter().any(|m| m == "document.inspect"), "{:?}", seen.lock().unwrap().1);
    // `pending` while it still runs.
    let r = crate::call_tool(&mut remote, "pending", &json!({}));
    assert_ne!(r["isError"], true, "{r}");
    let body: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["done"], false, "{body}");
    // It lands: `pending` returns its own result.
    done.store(true, Ordering::SeqCst);
    let r = crate::call_tool(&mut remote, "pending", &json!({}));
    let body: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((&body["done"], &body["result"]), (&json!(true), &json!({"r": 1})), "{body}");
    assert_eq!(remote.call("document.inspect", json!({})).unwrap(), "ran");

    // Waited out by a later call instead: that call runs, and `pending` still has the result.
    done.store(false, Ordering::SeqCst);
    assert!(remote.call("engine.execute", json!({"command": "solid.chamfer"})).is_err());
    done.store(true, Ordering::SeqCst);
    assert_eq!(remote.call("engine.execute", json!({"command": "inspect.measure"})).unwrap(), "ran");
    let r = crate::call_tool(&mut remote, "pending", &json!({}));
    let body: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((&body["done"], &body["result"]), (&json!(true), &json!({"r": 1})), "{body}");
    assert_eq!(seen.lock().unwrap().0, 1, "one connection throughout");

    // A request that only reads is not said to apply.
    done.store(false, Ordering::SeqCst);
    let e = remote.call("engine.execute", json!({"command": "sketch.inspect"})).unwrap_err();
    assert!(e.contains("only reads") && !e.contains("will still apply"), "{e}");

    // An export only writes its file (#1989): it is not said to apply either.
    let mut other = crate::Remote::connect(&addr).unwrap();
    other.set_waits(quick_waits(60, 60));
    let e = other.call("engine.execute", json!({"command": "file.export", "params": {"path": "/tmp/wall.step"}})).unwrap_err();
    assert!(e.starts_with(STILL_RUNNING) && e.contains("will still write the file") && !e.contains("apply"), "{e}");
    let e = other.call("document.inspect", json!({})).unwrap_err();
    assert!(e.contains("earlier request") && e.contains("will still write the file"), "{e}");
}

/// The headless session never leaves a request running.
#[test]
fn pending_is_empty_headless() {
    let mut s = server();
    assert_eq!(tool(&mut s, "pending", json!({}))["pending"], Value::Null);
}

/// A backend for the per-step batch path: `fail` maps commands to their errors, and
/// `edit.restore_checkpoint` works or not.
struct Stub {
    calls: Vec<(String, Value)>,
    fail: Vec<(&'static str, String)>,
    restore_ok: bool,
}

impl crate::backend::Backend for Stub {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.calls.push((method.to_string(), params.clone()));
        match (method, params["command"].as_str()) {
            ("engine.execute", Some("edit.checkpoint")) => Ok(json!({"checkpoint": 7})),
            ("engine.execute", Some("edit.restore_checkpoint")) if self.restore_ok => Ok(json!({})),
            ("engine.execute", Some("edit.restore_checkpoint")) => Err("busy: request 3 is still running".into()),
            ("engine.execute", Some(c)) => match self.fail.iter().find(|(f, _)| *f == c) {
                Some((_, e)) => Err(e.clone()),
                None => Ok(json!({"ran": c})),
            },
            (m, _) => Err(format!("unknown method `{m}`")),
        }
    }
    fn has_ui(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "stub".into()
    }
}

fn stub_batch(fail: Vec<(&'static str, String)>, restore_ok: bool, rollback: bool) -> (Stub, Value) {
    let mut b = Stub { calls: Vec::new(), fail, restore_ok };
    let args = json!({"commands": [{"command": "a.one"}, {"command": "a.two"}, {"command": "a.three"}], "rollback": rollback});
    let r = crate::tools::call_tool(&mut b, "batch", &args);
    assert_eq!(r["isError"], true, "{r}");
    let body = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    (b, body)
}

/// A failed batch says what really happened: `rolled_back` only when the checkpoint was restored,
/// which steps' changes are in the design (`applied`) and which were never sent (`not_run`). A
/// step still running in the app counts as applied unless the rollback took it back.
#[test]
fn a_failed_batch_reports_what_really_happened() {
    let still = format!("{} `engine.execute` (a.two) is still running in the app", crate::backend::STILL_RUNNING);
    // Still running, and the rollback could not restore the checkpoint.
    let (b, body) = stub_batch(vec![("a.two", still.clone())], false, true);
    assert_eq!((&body["rolled_back"], &body["undone"]), (&json!(false), &json!(0)), "{body}");
    assert_eq!((&body["applied"], &body["not_run"]), (&json!([0, 1]), &json!([2])), "{body}");
    assert_eq!(body["failed"]["still_running"], true, "{body}");
    assert!(!b.calls.iter().any(|(_, p)| p["command"] == "a.three"), "a step after the failure is never sent");
    // Still running, and the rollback (after the backend waited for it) restored the checkpoint.
    let (_, body) = stub_batch(vec![("a.two", still)], true, true);
    assert_eq!((&body["rolled_back"], &body["undone"]), (&json!(true), &json!(1)), "{body}");
    assert_eq!((&body["applied"], &body["not_run"]), (&json!([]), &json!([2])), "{body}");
    assert!(body["failed"].get("still_running").is_none(), "{body}");
    // An ordinary failure whose rollback failed.
    let (_, body) = stub_batch(vec![("a.two", "boom".into())], false, true);
    assert_eq!((&body["rolled_back"], &body["applied"]), (&json!(false), &json!([0])), "{body}");
    // No rollback asked for: nothing is restored, and the reply does not say it was.
    let (b, body) = stub_batch(vec![("a.two", "boom".into())], true, false);
    assert_eq!((&body["rolled_back"], &body["applied"], &body["not_run"]), (&json!(false), &json!([0]), &json!([2])), "{body}");
    assert!(!b.calls.iter().any(|(_, p)| p["command"] == "edit.restore_checkpoint"), "{:?}", b.calls);

    // The app's own engine.batch: its `rolled_back` is believed; an older app's is inferred.
    let reply = json!({"completed": 1, "results": [{"x": 1}], "failed": {"index": 1, "command": "a.two", "error": "boom"}, "undone": 0, "rolled_back": false});
    let (_, body, _) = recorded_batch(Ok(reply), json!({"commands": [{"command": "a.one"}, {"command": "a.two"}, {"command": "a.three"}]}));
    assert_eq!((&body["rolled_back"], &body["applied"], &body["not_run"]), (&json!(false), &json!([0]), &json!([2])), "{body}");
    let reply = json!({"completed": 1, "results": [{"x": 1}], "failed": {"index": 1, "command": "a.two", "error": "boom"}, "undone": 0});
    let (_, body, _) = recorded_batch(Ok(reply), json!({"commands": [{"command": "a.one"}, {"command": "a.two"}]}));
    assert_eq!(body["rolled_back"], false, "undone 0 of 1 completed: not rolled back: {body}");
    // A whole engine.batch still running in the app is not "may have run": `pending` gets it.
    let still = format!("{} `engine.batch` is still running in the app", crate::backend::STILL_RUNNING);
    let (b, body, is_error) = recorded_batch(Err(still), json!({"commands": [{"command": "a.one"}]}));
    assert!(is_error && body.as_str().is_some_and(|t| t.starts_with(crate::backend::STILL_RUNNING) && !t.contains("may have run")), "{body}");
    assert_eq!(methods(&b), ["engine.batch"]);
}

/// Batch steps may name the tools an agent knows: `list_commands` and `set_parameter` run as
/// engine.commands (filtered as the tool filters) and parameters.change; tools that can't be
/// steps fail the batch before anything is sent.
#[test]
fn batch_steps_may_name_tools() {
    let catalog = json!([{"id": "solid.box", "label": "Box", "tab": "SOLID", "panel": "CREATE"}, {"id": "sketch.line", "label": "Line", "tab": "SKETCH", "panel": "CREATE"}]);
    let reply = json!({"completed": 3, "results": [catalog, {"value": 40}, {"value": 20}], "undone": 0});
    let steps = json!([
        {"command": "list_commands", "params": {"filter": "box"}},
        {"command": "set_parameter", "params": {"name": "w", "value": 40}},
        {"command": "set_parameter", "params": {"name": "h", "expression": "w / 2", "unit": "mm"}},
    ]);
    let (b, body, is_error) = recorded_batch(Ok(reply), json!({"commands": steps}));
    assert!(!is_error, "{body}");
    assert_eq!(
        b.calls[0].1["commands"],
        json!([
            {"command": "engine.commands", "params": {}},
            {"command": "parameters.change", "params": {"name": "w", "expression": 40}},
            {"command": "parameters.change", "params": {"name": "h", "expression": "w / 2", "unit": "mm"}},
        ])
    );
    assert_eq!(body["results"][0], json!([{"id": "solid.box", "label": "Box", "tab": "SOLID", "panel": "CREATE"}]), "{body}");

    let (b, body, is_error) =
        recorded_batch(Ok(json!({})), json!({"commands": [{"command": "solid.box"}, {"command": "screenshot", "params": {"view": "iso"}}]}));
    assert!(is_error, "{body}");
    assert!(b.calls.is_empty(), "nothing is sent: {:?}", b.calls);
    assert_eq!((&body["failed"]["index"], &body["not_run"], &body["applied"]), (&json!(1), &json!([0, 1]), &json!([])), "{body}");
    assert!(body["failed"]["error"].as_str().is_some_and(|e| e.contains("screenshot")), "{body}");
    let (_, body, _) = recorded_batch(Ok(json!({})), json!({"commands": [{"command": "body_topology", "params": {"body": "Body1"}}]}));
    assert!(body["failed"]["error"].as_str().is_some_and(|e| e.contains("model.edges") && e.contains("model.faces")), "{body}");
    // Editing in place goes to another design's tab: never a step, not even one by one
    for step in [
        json!({"command": "component.edit_in_place"}),
        json!({"command": "Component.Finish_Edit"}),
        json!({"command": "execute", "params": {"command": "component.finish_edit"}}),
    ] {
        let command = step.to_string();
        let (b, body, is_error) = recorded_batch(Ok(json!({})), json!({"commands": [{"command": "solid.box"}, step]}));
        assert!(is_error && b.calls.is_empty(), "{command}: {body}");
        assert!(body["failed"]["error"].as_str().is_some_and(|e| e.contains("on its own")), "{body}");
    }

    // Headless, step by step.
    let mut s = server();
    let r = tool(
        &mut s,
        "batch",
        json!({"commands": [
            {"command": "set_parameter", "params": {"name": "w", "value": "40 mm"}},
            {"command": "solid.box", "params": {"length": "w", "width": 10, "height": 10}},
            {"command": "list_commands", "params": {"filter": "solid.box"}},
            {"command": "execute", "params": {"command": "solid.sphere", "params": {"radius": 2, "center": [100, 0, 0]}}},
            {"command": "describe_command", "params": {"id": "solid.box"}},
        ]}),
    );
    assert_eq!(r["completed"], 5, "{r}");
    assert!(r["results"][4]["input"]["properties"].get("operation").is_some(), "{r}");
    let ids: Vec<&str> = r["results"][2].as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).collect();
    assert!(!ids.is_empty() && ids.iter().all(|id| id.contains("solid.box")), "{ids:?}");
    assert!(volume(&mut s) > 4000.0);
}

/// An unknown argument is refused with the tool's real arguments listed.
#[test]
fn unknown_arguments_list_the_real_ones() {
    let mut s = server();
    for (tool_name, want) in [
        ("screenshot", &["view", "camera", "path", "width", "height", "source", "grid", "axes"][..]),
        ("inspect_design", &["measure", "sketches"][..]),
        ("body_topology", &["body"][..]),
    ] {
        let args = if tool_name == "body_topology" { json!({"body": "Body1", "nonsense": 3}) } else { json!({"nonsense": 3}) };
        let r = raw_tool(&mut s, tool_name, args);
        let msg = r["error"]["message"].as_str().unwrap_or_default();
        assert_eq!(r["error"]["code"], -32602, "{r}");
        assert!(msg.contains("unknown argument `nonsense`") && want.iter().all(|a| msg.contains(a)), "{tool_name}: {msg}");
    }
}

/// `describe_command` serves a converted command's schemas, and a headless session set to strict
/// refuses a misspelt key through `execute` without touching the design.
#[test]
fn describe_command_and_strict_parameter_checking() {
    let mut s = server();
    let d = tool(&mut s, "describe_command", json!({"id": "solid.box"}));
    assert!(d["input"]["properties"].get("operation").is_some(), "{d}");
    assert!(d["output"]["properties"].get("created_bodies").is_some(), "{d}");

    let mut headless = Headless::default();
    headless.session.param_check = solidcraft_engine::schema::ParamCheck::Strict;
    let mut s = Server::new(Box::new(headless));
    call(&mut s, 1, "initialize", json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    let r = raw_tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 5, "width": 5, "height": 5, "operaton": "cut"}}));
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("unknown key `operaton`; did you mean `operation`?"), "{text}");
    let m = tool(&mut s, "measure", json!({}));
    assert_eq!(m["body_count"], json!(0), "{m}");
}

/// Bridged to an app (#9072), the server checks each call against its command's contract before
/// sending it, as a headless session does: the app's own session checks nothing. `strict` sends
/// nothing that breaks a contract, `warn` adds `param_warnings` to the app's reply, and a command
/// this server has no contract for goes to the app unchecked.
#[test]
fn a_bridged_server_checks_calls_at_its_param_check_level() {
    use solidcraft_engine::schema::ParamCheck;
    let (addr, seen) = scripted_app(|req| match req["method"].as_str() {
        Some("engine.batch") => {
            let n = req["params"]["commands"].as_array().map_or(0, Vec::len);
            json!({"ok": true, "result": {"completed": n, "results": vec![json!({}); n], "undone": 0}})
        }
        _ => json!({"ok": true, "result": {"created_bodies": ["Box1"]}}),
    });
    let bridged = |level| {
        let remote = crate::Remote::connect_with_token(&addr, Some("0123456789abcdef0123456789abcdef")).unwrap().with_param_check(level);
        Server::new(Box::new(remote))
    };
    let sent = |what: &str| seen.lock().unwrap().1.iter().filter(|w| w.as_str() == what).count();
    let text = |r: &Value| r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    let misspelt = json!({"command": "solid.box", "params": {"length": 5, "width": 5, "height": 5, "operaton": "cut"}});
    let good = json!({"command": "solid.box", "params": {"length": 5, "width": 5, "height": 5}});
    let warning = "unknown key `operaton`; did you mean `operation`?";

    let mut s = bridged(ParamCheck::Strict);
    let r = raw_tool(&mut s, "execute", misspelt.clone());
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    assert!(text(&r).contains(&format!("invalid parameters for `solid.box`: {warning}")), "{r}");
    assert_eq!(sent("engine.execute solid.box"), 0, "refused unsent");
    let r = raw_tool(&mut s, "execute", json!({"command": "solid.chamfer", "params": {"nonsense": 1}}));
    assert_ne!(r["result"]["isError"], json!(true), "{r}");
    assert_eq!(sent("engine.execute solid.chamfer"), 1, "no contract here: the app decides");
    let r = raw_tool(&mut s, "batch", json!({"commands": [good, misspelt]}));
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    let reply: Value = serde_json::from_str(&text(&r)).unwrap();
    assert_eq!((&reply["failed"]["index"], &reply["completed"], &reply["not_run"]), (&json!(1), &json!(0), &json!([0, 1])), "{reply}");
    assert!(reply["failed"]["error"].as_str().unwrap_or_default().contains(warning), "{reply}");
    assert_eq!(sent("engine.batch"), 0, "refused unsent");

    let mut s = bridged(ParamCheck::Warn);
    let r = tool(&mut s, "execute", misspelt.clone());
    assert_eq!(r["param_warnings"], json!([warning]), "{r}");
    assert_eq!(sent("engine.execute solid.box"), 1);
    let r = tool(&mut s, "batch", json!({"commands": [good, misspelt]}));
    assert_eq!((r["results"][0].get("param_warnings"), &r["results"][1]["param_warnings"]), (None, &json!([warning])), "{r}");
    assert_eq!(sent("engine.batch"), 1);

    let mut s = bridged(ParamCheck::Off);
    let r = tool(&mut s, "execute", misspelt);
    assert_eq!(r.get("param_warnings"), None, "{r}");
}

/// Bridged to an app that checks a request's own `param_check` (#9184), the server asks it once
/// per connection (`control.capabilities`) and then sends every call with its level, unchecked
/// here: the app checks against its own commands, so a key only a newer app knows is not
/// refused, and its answer is the one the agent sees.
#[test]
fn a_bridged_server_leaves_the_check_to_an_app_that_makes_it() {
    use solidcraft_engine::schema::ParamCheck;
    let levels = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let log = levels.clone();
    let (addr, seen) = scripted_app(move |req| {
        let level = req["params"]["param_check"].clone();
        match req["method"].as_str() {
            Some("control.capabilities") => json!({"ok": true, "result": {"param_check": ["off", "warn", "strict"]}}),
            Some("engine.batch") => {
                log.lock().unwrap().push(level.clone());
                json!({"ok": true, "result": {"completed": 1, "results": [{"param_warnings": ["the app's own"]}], "undone": 0}, "param_check": level})
            }
            _ => {
                log.lock().unwrap().push(level.clone());
                json!({"ok": true, "result": {"created_bodies": ["Box1"], "param_warnings": ["the app's own"]}, "param_check": level})
            }
        }
    });
    let remote = crate::Remote::connect_with_token(&addr, Some("0123456789abcdef0123456789abcdef")).unwrap().with_param_check(ParamCheck::Strict);
    let mut s = Server::new(Box::new(remote));
    let sent = |what: &str| seen.lock().unwrap().1.iter().filter(|w| w.as_str() == what).count();
    let newer = json!({"command": "solid.box", "params": {"length": 5, "width": 5, "height": 5, "a_newer_key": 1}});

    for _ in 0..2 {
        let r = tool(&mut s, "execute", newer.clone());
        assert_eq!(r["param_warnings"], json!(["the app's own"]), "{r}");
    }
    assert_eq!((sent("engine.execute solid.box"), sent("control.capabilities")), (2, 1), "asked once, both sent");
    // A batch goes on a fresh connection, so the next call asks again.
    let r = tool(&mut s, "batch", json!({"commands": [newer.clone()]}));
    assert_eq!(r["results"][0]["param_warnings"], json!(["the app's own"]), "{r}");
    tool(&mut s, "execute", newer);
    assert_eq!(sent("control.capabilities"), 2);
    assert!(levels.lock().unwrap().iter().all(|l| l == "strict"), "{:?}", levels.lock().unwrap());
}

/// An app that says it checks `param_check` but answers a call without saying it checked it at
/// the bridge's level (#9184), with no echo or another level: at `strict` that is an error, as the
/// call may have run unchecked, and from then on the server checks the calls itself.
#[test]
fn a_bridged_server_at_strict_refuses_an_unconfirmed_reply() {
    use solidcraft_engine::schema::ParamCheck;
    for echo in [Value::Null, json!("off")] {
        let (addr, seen) = scripted_app(move |req| match req["method"].as_str() {
            Some("control.capabilities") => json!({"ok": true, "result": {"param_check": ["off", "warn", "strict"]}}),
            _ => json!({"ok": true, "result": {"created_bodies": ["Box1"]}, "param_check": echo}),
        });
        let remote = crate::Remote::connect_with_token(&addr, Some("0123456789abcdef0123456789abcdef")).unwrap().with_param_check(ParamCheck::Strict);
        let mut s = Server::new(Box::new(remote));
        let sent = |what: &str| seen.lock().unwrap().1.iter().filter(|w| w.as_str() == what).count();
        let r = raw_tool(&mut s, "execute", json!({"command": "solid.sphere", "params": {"radius": 2}}));
        assert_eq!(r["result"]["isError"], json!(true), "{r}");
        assert!(r.to_string().contains("did not say it checked this call"), "{r}");
        let r = raw_tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 5, "width": 5, "height": 5, "operaton": "cut"}}));
        assert!(r.to_string().contains("unknown key `operaton`"), "checked here now: {r}");
        assert_eq!(sent("engine.execute solid.box"), 0, "refused unsent");
    }
}

/// Plain `mcp` with no app running: the session is headless, says so on every result, and bridges
/// to the app once one runs, but only while it holds no work of its own.
#[test]
fn auto_session_bridges_to_a_late_app_only_while_empty() {
    use std::cell::Cell;
    use std::rc::Rc;
    // A port nothing listens on: the bridge exists, its calls say the app is not running.
    let port = Rc::new(Cell::new(None::<u16>));
    let seen = port.clone();
    let mut s = Server::new(Box::new(crate::Auto::new(Headless::default(), Box::new(move || seen.get()), None)));
    let text = |r: &Value| r["result"]["content"].as_array().unwrap().iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");

    let r = raw_tool(&mut s, "list_commands", json!({}));
    assert!(text(&r).contains("headless session because the SolidCraft app was not found"), "{r}");
    let init = call(&mut s, 1, "initialize", json!({}));
    assert!(init["result"]["instructions"].as_str().unwrap().contains("was not running when this server started"));

    // The app appears while the session is empty: the next call goes to it.
    port.set(Some(1));
    let r = raw_tool(&mut s, "inspect_design", json!({}));
    assert!(!text(&r).contains("headless session because"), "{r}");
    assert!(s.backend().has_ui());
    assert!(s.backend().describe().contains("connected to the SolidCraft app"));
    // The headless session's `--param-check` level goes on checking the bridged calls.
    let mut headless = Headless::default();
    headless.session.param_check = solidcraft_engine::schema::ParamCheck::Strict;
    let mut strict = Server::new(Box::new(crate::Auto::new(headless, Box::new(|| Some(1)), None)));
    raw_tool(&mut strict, "inspect_design", json!({}));
    assert!(strict.backend().has_ui());
    assert_eq!(strict.backend().param_check(), solidcraft_engine::schema::ParamCheck::Strict);

    // With work in the headless session, the app showing up changes nothing.
    port.set(None);
    let mut s = Server::new(Box::new(crate::Auto::new(
        Headless::default(),
        Box::new({
            let p = port.clone();
            move || p.get()
        }),
        None,
    )));
    let r = raw_tool(&mut s, "execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    assert_ne!(r["result"]["isError"], json!(true), "{r}");
    port.set(Some(1));
    let r = raw_tool(&mut s, "inspect_design", json!({}));
    assert!(text(&r).contains("headless session because the SolidCraft app was not found"), "{r}");
    assert!(!s.backend().has_ui());
}

/// A server with no `Auto` backend (`--headless`, `--in`) adds no notice.
#[test]
fn plain_headless_adds_no_notice() {
    let mut s = server();
    let r = raw_tool(&mut s, "list_commands", json!({}));
    assert_eq!(r["result"]["content"].as_array().unwrap().len(), 1);
}
