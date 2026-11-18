# Changelog — `armature-cli`

All notable changes to this crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Earlier changes are recorded in the workspace [`CHANGELOG.md`](../CHANGELOG.md).

## [Unreleased]

### Fixed

- `armature dev` (built-in watcher) no longer rebuilds in a loop. On Linux, `notify` reports the compiler opening source files as `Access(Open)` events; the watcher only looked at the `.rs` extension (the event kind was dropped by `notify-debouncer-mini`), so each build triggered the next one, killing it before it could finish. Events are now filtered by kind (access and metadata-only events are ignored) and by path, and a change is only acted on when the file content really changed.
- `armature dev` now builds with `cargo build` and runs the produced binary itself, in its own process group (Job Object on Windows). Restarts and `Ctrl+C`/`SIGTERM`/`SIGHUP` stop the server *and* its descendants, with a grace period before a forced kill; previously only `cargo run` was killed, which could leave the application running and holding its port.
- Only one build runs at a time; changes made during a build are queued and built once afterwards instead of being lost or starting concurrent builds.
- A failed build is reported once and not retried until a new change; the previous server keeps running.
- `Cargo.toml`, `Cargo.lock`, `build.rs`, `.cargo/config.toml`, `armature.toml` and workspace path dependencies are now watched; `target/`, `.git/` and editor temporary files are ignored. `Cargo.lock` writes made by Cargo itself during a build do not trigger another build.
- `armature generate` wrote files that Rust never compiled: they were not declared anywhere (e.g. `src/services/x.rs` without `mod services;`, modules declared in a `src/mod.rs`, tests in `tests/` directories never declared). Every generated file is now declared up to the crate root, and its test is compiled by `cargo test`.
- Templates that did not compile once actually included now do: CRUD controller (`no_content()` not wrapped in `Ok`), interceptor (real `Interceptor::intercept` API), exception filter (real `ExceptionFilter::catch` API), middleware (missing `Middleware`/`Next` imports), pipe (there is no `Pipe` trait: inherent `transform`), job (real armature-queue API), event (real armature-events API), cache service (real armature-cache API), API client (real armature-http-client API), repository (missing serde import; the `RepositoryImpl` is the provider), config (missing `name_upper`). `prax-module` no longer emits `#[module_impl]`/`#[provider]`, which do not exist; the Prax templates rely on the code produced by `prax generate` and are not compiled by the CLI's tests. The service template no longer emits an unused import.

### Added

- `armature --verbose dev` explains every filesystem event (accepted or ignored, and why), batch, build and server start/stop.
- `armature dev --debounce-ms <MS>` and `--no-cargo-watch`.
- `[dev.watcher]` / `[dev.server]` sections in `armature.toml` (debounce, extra watched/excluded paths, shutdown timeouts). Everything has a default.
- Arguments after a second `--` are passed to the application (`armature dev -- --release -- --flag`), like `cargo run`.
- `armature generate` follows the NestJS CLI: the name carries the path (`armature g resource modules/products --crud` → `src/modules/products/{mod.rs, products_controller.rs, products_service.rs, tests/…}`), each element gets its own directory (`--flat` to opt out), elements are registered in the nearest `#[module(...)]` (`--skip-import` to opt out), and `--dry-run`/`-d` and `--no-spec` are supported. Generation is all-or-nothing.
- New NestJS schematics: `class`, `interface` (trait), `provider`, `decorator` (request extractor), `library` (workspace crate in `libs/`), `sub-app` (application in `apps/`), `application`; NestJS aliases (`co`, `s`, `mo`, `res`, `pr`, `cl`, `itf`, `d`, `f`, `ga`, `gu`, `itc`, `mi`, `pi`, `r`, `lib`, `app`).
- Generators add what their code needs to `Cargo.toml` (armature features such as `auth`, crates such as `validator`, `async-graphql`, `armature-queue`).

### Changed

- **Breaking** for `armature generate`: files no longer go to per-kind directories (`src/controllers/`, `src/services/`, `src/models/`…) but to the path given in the name, NestJS style (`armature g service users` → `src/users/users_service.rs`). `r` is now the alias of `resolver` (NestJS); `resource` is `res`. `generate scheduler` produces a cron job (same as `job --job-type scheduled`). `generate filter` replaces `generate exception-filter` (kept as an alias).

### Removed

- Dependency on `notify-debouncer-mini`.

## [0.9.0] - 2026-10-29

### Changed

- Requires `armature-core` 0.12 (was `0.11`).
- Requires `armature-app` 0.7 (was `0.6`).

## [0.8.0] - 2026-10-17

### Security

- **Breaking (generated code):** every `armature generate guard` template now authorizes only from the verified `armature_auth::UserContext` attached by `JwtAuthMiddleware` and fails closed (401 without it). The permission/role guards no longer trust the client-controlled `x-user-permissions` / `x-user-role` headers, and the generic/auth guards no longer accept a bare `Authorization` header. Generated projects need the `auth` feature on `armature`.
- The API-key guard template has no `Default`, rejects keys shorter than 32 bytes and compares in constant time; migrate with `#[guard(XGuard::from_env().expect(..))]`.
- The rate-limit guard template keys on `HttpRequest::client_address` (socket peer unless proxies are explicitly trusted) instead of the spoofable `X-Forwarded-For`, and bounds its client table.
- Generated Dockerfile uses `rust:1.99.1-slim-trixie` / `debian:trixie-slim` (was EOL `rust:1.75`), builds `--locked`, drops OpenSSL, runs as numeric UID 10001 and explains digest pinning. The Lambda/Cloud Run Dockerfiles mirror the digest-pinned canonical `templates/`: cargo-lambda 1.9.2 with a hardcoded SHA-256 (no skip path) and a pinned cargo-chef.
- Generated GitHub Actions workflow pins every action to a commit SHA, sets top-level `permissions: contents: read`, checks out with `persist-credentials: false`, grants `packages: write` only to the image job and tags images with the commit SHA instead of `latest`.
- Generated docker-compose files ship no passwords (`${VAR:?}` from `.env`), password-protect Redis and publish ports on 127.0.0.1 only; `.env.example` no longer suggests a placeholder `JWT_SECRET`. The Cloud Run service manifest deploys by digest; the SAM template flags that its API is public.
- External `armature <name>` subcommands are no longer looked up in empty or relative `PATH` entries or the current directory, so a binary checked into an untrusted repository cannot be run by accident.
- Install hints use `cargo install ... --locked`.
- **Breaking (generated code):** `armature generate websocket` was rewritten against the `armature-websocket` API (it previously emitted a non-existent `WebSocket` type and a stub HTTP route). The generated module builds its server with an Origin allowlist and a handshake authorizer that requires `Authorization: Bearer <token>` and a caller-supplied `verify` function returning the connection's `Principal`; handlers receive that `Principal`. The `websockets`/`realtime` project feature now adds `armature-websocket` to the generated `Cargo.toml`.

### Changed

- Replaced the deprecated `serde_yaml` (unmaintained `unsafe-libyaml`) with `serde_norway`.
- Generated projects depend on `armature-framework` 0.7 and `armature-core` 0.10 (were 0.2 / 0.4), so the templates compile against the APIs they use.

## [0.7.0] - 2026-09-15

### Changed

- **Breaking:** requires `armature-app` 0.5 (was `0.4`); its types appear in this crate's API, so the requirement change is breaking here and the minor moves. Part of the `armature-core` 0.10 release train.
- **Breaking:** requires `armature-core` 0.10 (was `0.9`); its types appear in this crate's API, so the requirement change is breaking here and the minor moves. Part of the `armature-core` 0.10 release train.

### Security

- Require `webbrowser` 1.2.4 or later, which fixes RUSTSEC-2026-0257 (browser argument injection through the Unix `BROWSER` variable).

## [0.6.0] - 2026-08-05

### Changed

- **Requires `armature-core` 0.9 (breaking).** The requirement moved `0.8` →
  `0.9`. `armature-core 0.9.0` itself moves `armature-h1` across a breaking
  0.x boundary; because `armature-core` types appear in this crate's own
  public API, the requirement change is breaking here too and the minor moves
  with it. Under Cargo's 0.x caret rules the 0.8 and 0.9 types are distinct
  and do not unify, so a consumer holding an `armature-core 0.8` type cannot
  pass it to this crate. Part of the `armature-core 0.9.0` release train; see
  `armature-core`'s CHANGELOG for the publish order.
- Requires `armature-app` 0.4 (was `0.3`); it moved its minor in the same train for the same reason.

## [0.5.1] - 2026-08-04

### Fixed

- Requirements on sibling armature crates name a minor instead of `0`. Under
  Cargo's 0.x rules `version = "0"` matches any release ever made, and edition
  2024 selects the MSRV-aware resolver, so a consumer declaring an older
  `rust-version` was handed the oldest version satisfying it — resolving
  `armature-core = "0"` on Rust 1.89 produced `armature-core 0.2.3` while an
  explicit `armature-core = "0.8"` elsewhere in the same graph pulled 0.8.2.
  Two copies of core, and a build failing on symbols the older one lacks. Each
  0.x minor in this family is a breaking change, so the requirement now names
  one. No API change.
