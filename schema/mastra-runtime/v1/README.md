# SolidCraft Mastra protocol 1.0 — Rust conformance bundle

Semantic authority: `docs/mastra-runtime.md`, sections 3–9 and 11.
Normative documentation digest: `sha256:da553ed3c5e83362b20ae89e1a2ec20728d8fdc3325a435e61d2ee1f3a165dd7`.

`protocol.schema.json` is generated from the actual Rust DTOs using native-test-only
schemars metadata plus explicit primitive, strict envelope, error-details and scope
conditions. `$schema` is Draft 2020-12; `$id` is `urn:solidcraft:mastra-runtime:1.0`.
Every schema reference is local. Successful response results use `anyOf`, because
turn/request cancellation legitimately share `{state:"cancelling"}`. Responses have
no added method tag: the actual pending ledger correlates method, scope and ID.

## Validation layers

`cases.json` is `{schemaVersion,cases}`. Each sorted case has `id,path,kind,schemaRef,
phase,sender,shapeExpected,handlingExpected,errorCode`; schemaRef/errorCode may be null.
Kinds are frame/data/transcript/bytes. Shape is valid/invalid/not_applicable; handling
is accept/invalid_request/method_not_found/transport_error/transcript. A schema-invalid
known-method parameter gets recoverable INVALID_REQUEST, not necessarily transport loss.
Sender/direction, bootstrap version handling and operation legality are separate layers.

The pinned Draft 2020-12 evaluator actually checks shape offline. JSON Schema does
not enforce duplicate-key detection, UTF-8 byte limits, NDJSON LF framing, negotiated
cross-field resource arithmetic, JCS digests, tool-result history or ledger correlation.
The actual Rust decoder/DTO validators and RuntimeMachine cover those distinct layers.
Raw `.bin` and `.ndjson` fixtures are exact bytes; do not parse/reformat/bless them.
Decimal revisions (including unsafe-JS and max-u64) remain strings, never Number/f64.
Opaque geometry/schema objects retain their declared scope, not global integer caps.

The `guard-*` data cases adopt #8's copied proposal
`sha256:754778a123a1fe44f3a4e99e5905c38d0a715e376cb9f2b72d987d68d4db88ac`.
They preserve raw expectedDocument/control/MCP carriers and native result bytes;
component adapters validate actual DocumentStamp/WireError types separately. Their
catalog `shapeExpected:not_applicable` and `handlingExpected:accept` mean the
carrier/component assertions agree, NOT that a guarded write was authorized or
that an error/pending/expectation failure became a successful operation. Raw wrappers
never enter the IPC Frame evaluator/decoder. The seven `guard-decode-*` examples
preserve native decode inputs, not a replacement native guarded-result decoder.
Primitive `7` is only a synthetic carrier; the raw control pending nested reply has
no invented id, and its full timeout diagnostic and original outer identities remain.

Transcripts are `{schemaVersion,id,initialLimits,steps}`. Step tags are host_request,
runtime_frame, advance_ms, host_complete, expect_actions. Paths name persisted fixture
bytes. The runner drives the actual RuntimeMachine with virtual monotonic time and
real callback tokens, Written and NoticeConsumed ownership acknowledgements. Expected
actions are literal identities/results/errors/closures/discards, not a second machine.
`host_complete` models native service replies, EOF or generation replacement; it does
not assert that a real provider HTTP call stopped or a native document was guarded.

## Deterministic verification and recording

Run from this repository with its locked Rust toolchain and offline Cargo cache:

```sh
cargo test --locked --profile ci -p solidcraft-ui-egui --lib -- agent_chat::tests::mastra_bundle_
cargo test --locked --profile ci -p solidcraft-ui-egui --lib -- agent_chat::tests::mastra_
cargo test --locked --profile ci -p solidcraft-ui-egui --lib -- agent_chat::process::tests::
cargo test --locked --profile ci -p solidcraft-ui-egui --lib -- agent_chat::acp_client::tests::
```

Normal tests only compare; they never write the bundle. Review intended contract
changes and the generated-contract test before explicitly recording:

```sh
SOLIDCRAFT_MASTRA_RECORD=1 cargo test --locked --profile ci -p solidcraft-ui-egui --lib -- agent_chat::tests::mastra_bundle_record --ignored --exact
```

Only that ignored, environment-guarded test materializes deterministic fixture/schema/
catalog/manifest bytes. Re-run normal tests afterward. The RFC 8785 encoder is the
pinned serde_json_canonicalizer, tested against number formatting and UTF-16 key order.
This is not a native candidate/profile/snapshot hashing or reporting implementation.

`manifest.json` has schemaVersion/protocolVersion `1.0` and bytewise sorted unique
`{path,sizeBytes,digest}` files. Paths are safe relative POSIX paths; symlinks are
rejected. Every artifact except the manifest itself is included. Each lowercase
`sha256:` digest covers exact persisted bytes; the bundle digest is SHA-256 of the
manifest's exact bytes. No recursive self digest or source-commit field exists.
Later external release receipts must bind actual source revision and bundle digest.

## Acceptance boundary

This is independently executable **Rust conformance**, not TypeScript compatibility,
an actual Mastra Agent, native CAD/write authorization, host report verification,
snapshot persistence, installed platform resources or successful CI/Actions evidence.
Wire-only artifact references and runtime proposals are not trusted native snapshot
or report evidence. #7 must validate/serialize these same bytes with actual locked
TypeScript validators and Mastra; #14 must prove installed resources; #17 must run
the relevant same-source gates. Native peer tests require an actual Rust compiler.
Independent source review and the later unique full `cargo xtask ci` remain required.
