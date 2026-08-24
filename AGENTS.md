# cephlens Agent Instructions

## Project scope

- cephlens is a Rust TUI for investigating Ceph clusters over SSH.
- It builds as a single binary.
- The crate is not published to crates.io because `publish = false`.
- Releases use cargo-dist. Pushing a version tag starts the release workflow.
- Keep changes focused on the requested task. Avoid unrelated refactoring.

## Repository structure

- `src/main.rs` provides the CLI.
- `src/app.rs` and `src/ui.rs` provide the TUI.
- `src/ssh.rs` and `src/stream.rs` handle SSH streams.
- `src/collect.rs` and `src/model.rs` handle cluster data.
- `src/trace.rs`, `src/runner.rs`, `src/kfstrace.rs`, and `src/radostrace.rs` handle tracers.
- `src/doctor.rs`, `src/lab.rs`, `src/session.rs`, `src/report.rs`, and `src/config.rs` contain supporting features.
- `docs/` contains GitHub Pages assets such as the installer script and CNAME. Do not treat it as general development documentation.
- `.github/workflows/ci/cephtrace-fetch.yml` provides a step for the cargo-dist build. It is not a standalone workflow.

## Build and validation

Use this command for a normal development build.

~~~bash
cargo build
~~~

Before reporting a change as ready, run all of the following commands.

~~~bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked
~~~

- Do not report the change as fully validated if any required command was skipped or failed.
- The MSRV is Rust 1.88.0.
- The project uses Rust edition 2024 and let chains.
- CI builds with the MSRV and `--locked`.
- Clippy warnings fail CI.
- CI also runs a rustsec dependency audit.
- After dependency changes, run `cargo audit` when it is already available. Do not install additional tools without approval.

## Tests

- Add tests for parser changes and behavior changes.
- Parser tests in `src/kfstrace.rs` and `src/radostrace.rs` are useful examples.
- Prefer focused tests that cover the changed behavior and relevant failure cases.
- Do not weaken or remove an existing test only to make a change pass.

## Local execution

For an approved manual smoke test, run the commands in this order.

~~~bash
cargo run -- doctor
cargo run -- tui
~~~

- `cargo run -- tui` is interactive. Do not run it in a noninteractive environment.
- Do not connect to a real Ceph cluster unless the user has explicitly approved the target.
- Do not expose host names, credentials, keys, or connection details in command output, commits, issues, or pull requests.

## Configuration and generated files

- `cephlens.toml`, `.cephlens/`, and `third_party/cephtrace/bin/` are ignored.
- Do not commit ignored configuration or generated files.
- `cephlens.toml` may contain site-specific host names.
- Under `third_party/cephtrace/`, track only the required GPL-2.0 notices and license files.
- Never add files from `third_party/cephtrace/bin/`.
- Source builds do not contain the cephtrace tracer binaries.
- Release CI downloads the tracer binaries and verifies their SHA256 checksums before adding them to release archives.

## Cluster safety

- `bench` and `lab` create pools named `cephlens-test-*` on a real Ceph cluster.
- Never run `bench` or `lab` without explicit user approval.
- Before running either command, confirm that the target is a disposable nonproduction cluster.
- Do not run destructive Ceph commands that are not required by the requested task.

## Commits

- Use `git commit -s`.
- Use a short imperative English subject.
- The `-s` option must add a `Signed-off-by` trailer.
- Do not include credentials, tokens, private keys, site-specific connection details, or generated binaries in a commit.

## Release safety

Use the following command for a release.

~~~bash
scripts/release.sh [patch|minor|major|X.Y.Z]
~~~

The script updates the version, creates a signed-off commit, creates a tag, and pushes the commit and tag.

- Never run the release script without explicit user approval.
- Never push a release commit or version tag without explicit user approval.
- Do not create a release tag manually unless the user specifically requests it.
- If release CI remains queued, check runner availability before investigating the job itself.
