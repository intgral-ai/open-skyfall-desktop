---
name: solidcraft-ground-rules
description: "Use before adding any dependency, icon, image, font, test reference data or product wording to SolidCraft, or when unsure whether something may be copied, read or named. Covers independence from other CAD products, GPL code, asset attribution and the product name."
---

# Independence, licences, assets and the name

Hard rules; reasons: CONTRIBUTING.md, "Rules that aren't negotiable".

## Independent
- Design from public documentation, CAD conventions, user feedback and public format specs. Tests use analytic values (`tools/oracle-analytic/`) or open-source tools.
- No other-product code, icons, artwork, text, templates or materials. Only the scoped code grant below permits selected reuse; no CAD/UI assets.
- Never drive, script, automate or extract commercial CAD data (Fusion, SolidWorks, Onshape, ...): no test values, measurements, command lists, ids or generated files.
- Our command ids: dotted, lower-case `<area>.<thing>[.<variant>]`, never another product's.

## desktop-only Mastra distribution exception
- Rights gate: `docs/mastra-distribution.md` and `packaging/mastra/source-records.json`. At `acb5048120b53ae1b4502724b70d1283e8c9daf4`, the intgral-ai grant covers only selected owned Vector Studio orchestration/agent-loop/model-runtime code for public redistribution as `MIT OR Apache-2.0`, after per-file ownership, bytes, license, notices, dependencies and independent third-party review. Third-party content retains its own licenses/notices; `ee/` and excluded roles remain forbidden.
- Five source records remain `unverified`; real approvals are empty. Record source/destination paths/digests, adaptation and nonempty audit evidence before copying. Add real pending attribution when admitted; attribution is not permission.
- Only opt-in desktop `packages/mastra-runtime/` permits the TS companion, workflows/clients, pinned Node and necessary production dependencies. Host/core/application stay Rust/wasm; browser runtime unavailable, no Node.

## No GPL, LGPL or AGPL code
- Never read, copy or link GPL/LGPL/AGPL code: OpenCASCADE, FreeCAD (including planegcs), SolveSpace, CadQuery/OCP, LibreCAD or QCAD.
- `tools/freecad-oracle/` computes reference values through FreeCAD's public scripting API, separately; nothing linked, vendored or shipped. Solver/blends are ours.
- Explain permissive dependencies in the PR. `cargo xtask licences --check` inventories Cargo, not npm/Node or file rights. Never edit generated `THIRD-PARTY-LICENSES.txt` in a PR. [storytold](https://github.com/storytold) projects may guide conventions.

## Assets: the most serious rule
- No commercial CAD imagery, fonts or artwork; never commit its screenshots. Every asset is original/openly licensed with an ATTRIBUTION.md row (`cargo xtask assets`).
- Original SVG masters: `assets/icons/ui/design/svg/`; compilation: `crates/ui-egui/src/icon_svg.rs`.
- Before icon, assignment or media edits, read `assets/AGENTS.md`, `assets/icons/ui/AGENTS.md` and `assets/icons/ui/STYLE_GUIDE.md`. Use canonical bases and named finished references, not a parallel style; `cargo xtask icons` validates.

## The name
- Use SolidCraft: `solidcraft-*` crates, `solidcraft`/`solidcraft-cli`, `.sc` (`.solidcraft` still opens), `solidcraft/N`, `SOLIDCRAFT_*`, settings/recovery, MCP, docs, CI and packaging.
- Old-name shims only: `docs/compatibility.md`. Public docs/web/Flatpak Pages stay `/solidcraft/`; no old-name aliases.
- Terms: `docs/book/src/reference/glossary.md`, one per idea; add new terms there. Internal units: millimetres/radians; Z up.
