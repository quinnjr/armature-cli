# armature-cli

CLI tool for the Armature framework.

## Features

- **Project Generation** - Create new projects
- **Code Generation** - Generate controllers, models, etc.
- **Development Server** - Hot reload development
- **Build Tools** - Production builds and optimization

## Installation

```bash
cargo install armature-cli --locked
```

## Commands

### Create Project

```bash
armature new my-app
cd my-app
```

### Generate Code

`armature generate` (`armature g`) follows the NestJS CLI: the name carries
the path, each element gets its own directory, tests go next to it, and the
element is declared and registered in the nearest module.

```bash
armature g resource modules/products --crud   # module + controller + service + tests
armature g controller modules/products/admin  # registered in ProductsModule
armature g service users                      # src/users/users_service.rs
armature g guard common/auth --dry-run        # show what would change
```

```text
src/modules/products/
├── mod.rs                       # ProductsModule (controllers, providers), tests
├── products_controller.rs       # ProductsController → /products
├── products_service.rs          # ProductsService, injected into the controller
└── tests/
    ├── products_controller_test.rs
    └── products_service_test.rs
```

Rules:

- **Path and name**: `modules/products` → directory `src/modules/products/`,
  files `products_<kind>.rs`. Every directory becomes a Rust module: the
  generator adds `pub mod …;` to each parent and `mod modules;` to `main.rs`.
- **`--flat`**: no directory for the element (`src/modules/products_service.rs`).
- **Tests**: `tests/<file>_test.rs`, declared in a `#[cfg(test)] mod tests`
  block of the directory's module, so `cargo test` runs them. `--no-spec`
  (`--skip-tests`) skips them.
- **Registration**: controllers go to `controllers: [...]` and services /
  providers / repositories to `providers: [...]` of the nearest
  `#[module(...)]` (the element's directory, then each parent up to
  `AppModule`); a new module goes to the `imports` of its parent module.
  `--skip-import` disables it.
- **`--dry-run` (`-d`)**: print the `CREATE`/`UPDATE` list, write nothing.
  Nothing is written either when one of the files already exists.
- **Dependencies**: when the generated code needs an armature feature or a
  crate (`auth` for guards, `validator` for pipes and DTOs, `async-graphql`,
  `armature-queue`, …), it is added to `Cargo.toml`.

| NestJS | `armature g …` | Generated |
|---|---|---|
| `application` | `application` | new project (`armature new`) |
| `class` (`cl`) | `class` | `<name>.rs` — plain struct |
| `configuration` (`config`) | `config` | `<name>_config.rs` |
| `controller` (`co`) | `controller` (`co`, `c`) | `<name>_controller.rs` |
| `decorator` (`d`) | `decorator` | `<name>_decorator.rs` — request extractor (`FromRequest`) |
| `filter` (`f`) | `filter` | `<name>_filter.rs` — `ExceptionFilter` |
| `gateway` (`ga`) | `gateway` | `<name>_gateway.rs` — WebSocket handler |
| `guard` (`gu`) | `guard` | `<name>_guard.rs` (`--guard-type auth\|role\|permission\|api-key\|rate-limit`) |
| `interceptor` (`itc`) | `interceptor` | `<name>_interceptor.rs` |
| `interface` (`itf`) | `interface` | `<name>_interface.rs` — trait |
| `library` (`lib`) | `library` | `libs/<name>/` crate, workspace member + dependency |
| `middleware` (`mi`) | `middleware` | `<name>_middleware.rs` |
| `module` (`mo`) | `module` | `<dir>/mod.rs` with `#[module(...)]` |
| `pipe` (`pi`) | `pipe` | `<name>_pipe.rs` — parse + validate a body |
| `provider` (`pr`) | `provider` | `<name>_provider.rs` — `#[injectable]` |
| `resolver` (`r`) | `resolver` | `<name>_resolver.rs` — GraphQL |
| `resource` (`res`) | `resource` | module + controller + service + tests |
| `service` (`s`) | `service` | `<name>_service.rs` |
| `sub-app` (`app`) | `sub-app` | `apps/<name>/` application, workspace member |

Armature also has `model`, `dto`, `entity`, `repository`, `scaffold`, `job`,
`event`, `scheduler`, `cache-service`, `api-client`, `health` and the
`prax-*` generators, with the same path rules.

### Development Server

```bash
# Start with hot reload
armature dev

# Specify port
armature dev --port 8080

# Extra `cargo build` arguments, then arguments for the application
armature dev -- --release --bin api -- --config dev.toml

# Explain every event, build and restart
armature --verbose dev
```

`armature dev` builds the project with `cargo build`, runs the produced
binary directly (with `PORT` and `HOST` set) and rebuilds when a build input
changes. If `cargo-watch` is installed it is used instead; pass
`--no-cargo-watch` to force the built-in watcher.

How the built-in watcher behaves:

- **One save, one rebuild.** Events are filtered by kind: reads/opens (which
  the compiler itself produces while building) and metadata-only changes
  (`touch`, permissions) are ignored. Events close in time are grouped over a
  stability window (400 ms by default) and a file whose content did not
  actually change is ignored.
- **One build at a time.** Changes made while a build runs are queued and
  built once, right after it — never lost, never in parallel.
- **A failed build is not retried** until something changes again; the
  previous server keeps running meanwhile.
- **Clean restarts.** The old server and every process it started are stopped
  (SIGTERM, then SIGKILL after the grace period; a Job Object on Windows)
  before the new one starts, so the port is free. `Ctrl+C`, `SIGTERM` and
  closing the terminal stop everything as well.

Watched (build inputs, derived from `cargo metadata`):

| Path | |
|---|---|
| `src/**` of the package and of its workspace path dependencies | rebuild when content changes |
| `Cargo.toml`, `Cargo.lock`, `build.rs`, `.cargo/config.toml`, `rust-toolchain(.toml)` | rebuild |
| `armature.toml` | rebuild (no-op) and restart |
| `dev.watcher.watch_paths` | rebuild and restart |

Ignored: the Cargo target directory, `.git/` (and other VCS/IDE/cache
directories such as `.idea/`, `.vscode/`, `node_modules/`), editor swap and
backup files (`*.swp`, `*~`, `.#*`, `4913`, `*___jb_tmp___`…) and
`dev.watcher.exclude_paths`. Other hidden files are *not* ignored.

### Build

```bash
# Development build
armature build

# Production build
armature build --release
```

## Configuration

Create `armature.toml` in your project root:

```toml
[project]
name = "my-app"

[server]
port = 3000
host = "127.0.0.1"

[database]
url = "postgres://localhost/mydb"

# `armature dev` (all optional; these are the defaults)
[dev.watcher]
enabled = true          # false: build and run once, without watching
debounce_ms = 400       # stability window; also `armature dev --debounce-ms`
watch_paths = []        # extra files/directories, relative to the project root
exclude_paths = []      # paths that never trigger a rebuild
verbose = false         # same as `armature --verbose dev`

[dev.server]
graceful_shutdown_timeout_ms = 3000   # SIGTERM / CTRL_BREAK → wait
force_kill_timeout_ms = 2000          # then forced kill → wait
```

## License

MIT OR Apache-2.0

