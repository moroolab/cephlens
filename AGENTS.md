# cephlens — SSH 기반 Ceph 조사 TUI (Rust 단일 바이너리 크레이트)

## 리포 성격
- 본인 프로젝트. remote는 github.com/xtrusia/cephlens. crates.io 게시는 하지 않는다(`publish = false`).
- 릴리스는 cargo-dist. 버전 태그 push가 release 워크플로를 트리거한다.

## 구조
- `src/` 평면 모듈: `main.rs`(CLI), `app.rs`/`ui.rs`(TUI), `ssh.rs`/`stream.rs`(SSH 스트림),
  `collect.rs`/`model.rs`(클러스터 데이터), `trace.rs`/`runner.rs`/`kfstrace.rs`/`radostrace.rs`(트레이서),
  `doctor.rs`/`lab.rs`/`session.rs`/`report.rs`/`config.rs` 등.
- `docs/` = GitHub Pages 배포 자산(installer 스크립트, CNAME). 개발 문서가 아니다.
- `third_party/cephtrace/` = GPL-2.0 고지·라이선스만 커밋. `bin/`은 릴리스 CI가 fetch한다(gitignore).
- `.github/workflows/ci/cephtrace-fetch.yml`은 독립 워크플로가 아니라 dist 빌드에 주입되는 스텝이다.

## Canonical 명령
- 빌드: `cargo build`
- 테스트: `cargo test`
- 포맷: `cargo fmt --all --check`
- 린트: `cargo clippy --all-targets -- -D warnings`
- PR 전에 위 4개를 모두 실행한다. CI와 동일 검사다.
- 로컬 실행: `cargo run -- doctor` 후 `cargo run -- tui`
- 릴리스: `scripts/release.sh [patch|minor|major|X.Y.Z]` (버전 bump→sign-off 커밋→태그→push)

## 함정
- MSRV 1.88.0 (edition 2024, let chains). CI에 MSRV 전용 `cargo build --locked` 잡이 있다. 1.88 미만에서 깨지는 회귀를 만들지 않는다.
- clippy 경고는 CI에서 에러다. rustsec 의존성 audit도 CI에 있다.
- `cephlens.toml`, `.cephlens/`, `third_party/cephtrace/bin/`은 gitignore 대상이다. 커밋하지 않는다. `cephlens.toml`은 사이트별 호스트명을 담을 수 있다.
- 커밋은 `git commit -s`(Signed-off-by)로 만들고 제목은 짧은 명령형 영어로 쓴다.
- 파서·동작 변경에는 테스트를 붙인다. 선례는 `src/kfstrace.rs`, `src/radostrace.rs`의 파서 테스트다.
- `scripts/release.sh`는 origin push까지 수행하고 태그 push가 릴리스 빌드를 시작한다. 실행 전 사용자 승인을 받는다.
- 소스 빌드에는 cephtrace 트레이서 바이너리가 포함되지 않는다. 릴리스 아카이브만 CI가 SHA256 검증 후 번들한다.
- `bench`/`lab`은 실제 클러스터에 `cephlens-test-*` 풀을 만든다. 프로덕션 클러스터 대상 실행 금지.
- CI가 org self-hosted 러너에서 돌 수 있다. CI가 큐에 멈춰 있으면 러너 상태부터 확인한다.

## 글로벌 규칙 참조
- push·릴리스 승인, 홈 네트워크 호스트 작업은 `~/ai_conf/AGENTS.md`와 `~/ai_conf/reference/servers.md`의 강제 규칙을 따른다.
