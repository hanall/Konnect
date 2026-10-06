# Testing And Release

The pull-request baseline is defined in `CONTRIBUTING.md`. Run the same commands
locally when the required platform dependencies are available:

```bash
cargo test --workspace --locked --lib --tests
cargo test --workspace --locked --doc
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo fmt --all -- --check
```

`protoc` and its well-known type includes are required by
`crates/konnect-ipc/build.rs`.

## Coverage Map

| Area | Source of tests |
|---|---|
| MCP protocol, CLI, and asset contracts | `crates/konnect/tests` and `crates/konnect/src` unit tests |
| Router and toolset invariants | `crates/konnect-core/src/router` tests |
| Dispatch and required-argument behavior | `crates/konnect-core/src/mcp/handler.rs` tests |
| Domain handlers and evidence rules | tests beside modules in `crates/konnect-core/src/tools` |
| S-expression parsing and atomic writes | `crates/konnect-sexp` tests |
| Typed schematic model | `crates/konnect-schematic-editor` tests |
| IPC builders, transport, and client behavior | `crates/konnect-ipc` tests |
| Real KiCad behavior | ignored live tests and `.github/workflows/e2e-kicad.yml` |
| Viewer | `crates/schematic-viewer` tests and its CI job |
| Python plugin | `plugin/tests` and the plugin CI job |
| PCM package | `packaging/validate-pcm.py` and packaging CI jobs |

viewer는 Cargo workspace 밖에 있으며 자체 lockfile을 사용합니다. Workspace
명령은 viewer를 검사하지 않으므로 다음 gate를 별도로 실행합니다.

```bash
cargo test --manifest-path crates/schematic-viewer/Cargo.toml --locked
cargo clippy --manifest-path crates/schematic-viewer/Cargo.toml --all-targets --locked -- -D warnings
cargo build --manifest-path crates/schematic-viewer/Cargo.toml --release --locked
```

release 검증에서는 viewer binary를 `konnect` binary와 같은 directory에 두고
`open_schematic_viewer`를 실제 browser 환경에서 호출합니다. 성공 근거는 process
spawn이 아니라 `ready: true`, 일치하는 viewer PID, `127.0.0.1` capability URL,
`browser_connected`, `root_rendered`, `auto_refresh_watcher`,
`source: isolated_snapshot`입니다. Browser가 SVG를 실제 load하지 못하면 readiness
timeout이어야 하며 성공으로 기록하면 안 됩니다.

## Evidence-Focused Regression Tests

When a tool returns a count, success state, or verdict, test the evidence behind
that field. The v0.7 reference cases include:

- complete DRC category parsing in `tools/cli.rs`;
- DRC-backed review/readiness decisions in `tools/design_review.rs` and
  `tools/manufacturing.rs`;
- footprint-type discrimination and post-commit read-back in
  `tools/pcb_sync.rs` and `konnect-ipc/src/builders.rs`;
- closed-board placement and flip refusal cases in `tools/pcb_components.rs`.

Use real KiCad-generated fixtures for formats KiCad owns. For an IPC path, unit
tests should prove request construction and failure classification; an ignored
live test or the end-to-end workflow should prove behavior that depends on a
running editor.

IPC doubles in `konnect-core` must use the shared `test_support::MockIpcServer`.
The returned guard owns an already-listening, process-unique `inproc://`
endpoint and joins its worker when dropped. Retain that guard for the whole
test. Do not discover a free TCP port by binding it, dropping the listener, and
then asking NNG to bind the same port: another process can claim the port in
between, making otherwise deterministic tests fail with `AddressInUse`.

The Specctra import undo boundary has a manual live gate because KiCad IPC can
create a named commit but cannot invoke the editor's Undo action. Open a
disposable copy of
`crates/konnect-core/tests/fixtures/specctra_two_resistors_locked.kicad_pcb`,
set `KONNECT_LIVE_SPECCTRA_BOARD`, `KICAD_API_SOCKET`, and (when it is not on
`PATH`) `KICAD_CLI_PATH`, set `FREEROUTING_JAR` to the local Freerouting 2.3.0
JAR, then run:

```text
cargo test -p konnect-core --locked one_undo_restores_the_exact_pre_import_board_snapshot -- --ignored --nocapture
```

When the test prints `LIVE_UNDO_READY`, press Ctrl+Z once in PCB Editor. The
test passes only when the exact pre-import IPC snapshot returns.

## CI And Live Validation

`.github/workflows/ci.yml` covers the Rust workspace, formatting, clippy,
documentation tests, viewer, plugin, Nix, and PCM validation. The real-KiCad
workflow remains separate because it installs KiCad and is not an ordinary
per-PR gate. Apply the `run:e2e-kicad` label to run that same workflow for a
pull request when KiCad-facing behavior needs hosted acceptance evidence.

Before creating a release tag, dispatch `.github/workflows/release.yml` against
the exact candidate ref. The manual entry point runs the release-profile target
matrix, all three PCM package builds, and the reusable real-KiCad acceptance
workflow with a synthetic `0.0.0` package version. Its publication job is
tag-only, so this pre-tag smoke run cannot create a GitHub release.

On a release tag, `.github/workflows/release.yml` calls
`.github/workflows/e2e-kicad.yml` as a reusable workflow. The called workflow
checks out the caller's exact tag commit. The `Create Release` job depends on
that acceptance job and both artifact-building jobs, so failed real-KiCad
acceptance leaves the diagnostic build artifacts in the workflow run but cannot
create a GitHub release or upload a partial release asset set. Weekly and manual
real-KiCad runs continue to use the standalone workflow entry points.

In the PR description, list every command run and explicitly name checks skipped
because they require KiCad, another operating system, credentials, or release
infrastructure.

## Documentation

For a public behavior change, inspect README, `DEV.md`, `tool-directory.md`,
these developer maps, and bundled guidance under
`crates/konnect/assets/skills` and `assets/agents`. Tool-count locations and
their enforcement are defined by `CONTRIBUTING.md` and
`crates/konnect/tests/doc_tool_counts.rs`; link to the authoritative catalogue
instead of copying totals into new documents.

Every behavioral claim in these maps should name its source module. When the
module changes, a contributor can find the claim by searching for the old path.
Describe unimplemented future behavior explicitly as design intent rather than
current capability.

## Packaging And Release

Build the server with `cargo build --release -p konnect`. Build the viewer
separately when it is in scope. Changes to plugin files, binary layout, metadata,
icons, or release scripts require PCM assembly and
`packaging/validate-pcm.py` validation.

`packaging/build-pcm.ps1` and `build-pcm.sh` enumerate the files staged into the
PCM archive. Developer documentation stays in the repository and is not added
to release zips.
