# S12 — Split `moa-cli/src/main.rs` (3,132 LOC) and `commands/privacy.rs` (1,923 LOC)

## Scope

Cut the largest file in the workspace (`main.rs`) and the second-largest CLI command (`privacy.rs`) into manageable modules. **Standard clap-app-split pattern.** No subcommand renames, no flag changes, no behavior changes.

## Preconditions

- S01–S11 complete and merged.
- `cargo check --workspace` is green.

## Why this prompt

`main.rs` at 3,132 LOC is the single largest file in the workspace. It almost certainly bundles: the `clap` argument tree (every subcommand and flag), `main()` entry, runtime init, config loading, and inline implementations of multiple subcommands. `commands/privacy.rs` at 1,923 LOC has multiple privacy sub-subcommands (audit, export, redact, etc.) bundled together. Both follow well-established `clap` derive patterns when split — this is mechanical.

## Files in scope

- `crates/moa-cli/src/main.rs` → trimmed to ~150 LOC of dispatch + entry
- `crates/moa-cli/src/cli.rs` → new file containing the top-level `Cli` clap struct
- `crates/moa-cli/src/commands/<command>/mod.rs` and friends → expanded structure
- `crates/moa-cli/src/commands/privacy.rs` → split to `commands/privacy/`

## Files explicitly out of scope

- `crates/moa-cli/src/daemon.rs` (~1,113 LOC) — borderline; address only if it's tangled, otherwise skip and document
- `crates/moa-cli/tests/` — TEST pack handles (and creates first; CLI has no integration tests today)
- Any other crate

## Step-by-step instructions

### Part A — `main.rs` split

1. Read `main.rs` end-to-end. Expected sections:
   - `fn main()` entry (parses args, sets up runtime, dispatches)
   - Top-level `Cli` struct with `#[derive(Parser)]`
   - Top-level `Commands` enum with `#[derive(Subcommand)]`
   - Per-subcommand `Args` structs (or they may already be in `commands/<name>.rs`)
   - Per-subcommand `run` functions (the actual implementations)
   - Helper functions (config loading, runtime init, error printing)

2. Target structure for `moa-cli/src/`:
   ```
   src/
   ├── main.rs           — fn main + dispatch (target: <150 LOC)
   ├── cli.rs            — top-level Cli + Commands enum (Parser + Subcommand)
   ├── runtime_init.rs   — config loading, tracing init, runtime construction
   ├── error.rs          — top-level error reporting (was inline in main.rs)
   └── commands/
       ├── mod.rs
       ├── chat/
       │   ├── mod.rs    — Args + run
       │   └── ...
       ├── exec/
       ├── status/
       ├── sessions/
       ├── attach/
       ├── resume/
       ├── memory/
       ├── config/
       ├── init/
       ├── doctor/
       ├── daemon/
       ├── privacy/      — Part B handles
       └── version/
   ```
   Adjust to actual subcommands; this is the expected shape.

3. **Move `Cli` and `Commands` enum** to `cli.rs`:
   ```rust
   // crates/moa-cli/src/cli.rs
   use clap::{Parser, Subcommand};
   
   #[derive(Parser)]
   #[command(name = "moa", version, about = "...")]
   pub struct Cli {
       #[command(subcommand)]
       pub command: Commands,
       
       // global flags
       #[arg(long, global = true)]
       pub verbose: bool,
       // ...
   }
   
   #[derive(Subcommand)]
   pub enum Commands {
       Chat(crate::commands::chat::Args),
       Exec(crate::commands::exec::Args),
       // etc.
   }
   ```

4. **Each subcommand becomes a folder** with `mod.rs`:
   ```rust
   // crates/moa-cli/src/commands/chat/mod.rs
   use clap::Args as ClapArgs;
   
   #[derive(ClapArgs)]
   pub struct Args {
       #[arg(short, long)]
       pub model: Option<String>,
       // ...
   }
   
   pub async fn run(args: Args, ctx: crate::Context) -> anyhow::Result<()> {
       // implementation
   }
   ```

5. **`main.rs` after the split**:
   ```rust
   //! moa CLI entry point.
   
   mod cli;
   mod commands;
   mod error;
   mod runtime_init;
   
   use clap::Parser;
   use cli::{Cli, Commands};
   
   fn main() -> anyhow::Result<()> {
       let cli = Cli::parse();
       let runtime = runtime_init::tokio_runtime()?;
       runtime.block_on(async {
           let ctx = runtime_init::build_context(&cli).await?;
           dispatch(cli.command, ctx).await
       })
   }
   
   async fn dispatch(cmd: Commands, ctx: Context) -> anyhow::Result<()> {
       match cmd {
           Commands::Chat(a) => commands::chat::run(a, ctx).await,
           Commands::Exec(a) => commands::exec::run(a, ctx).await,
           Commands::Status(a) => commands::status::run(a, ctx).await,
           // etc.
       }
   }
   ```

### Part B — `commands/privacy.rs` split

6. Read `commands/privacy.rs` end-to-end. The expected pattern: a `Privacy` parent command with sub-subcommands (e.g. `audit`, `export`, `redact`, `consent`).

7. Target structure:
   ```
   commands/privacy/
   ├── mod.rs              — top-level Args + run (dispatches to subcommands)
   ├── audit.rs
   ├── export.rs
   ├── redact.rs
   ├── consent.rs
   └── (etc.)
   ```

8. The `mod.rs` looks like:
   ```rust
   use clap::{Args as ClapArgs, Subcommand};
   
   #[derive(ClapArgs)]
   pub struct Args {
       #[command(subcommand)]
       pub command: PrivacyCommand,
   }
   
   #[derive(Subcommand)]
   pub enum PrivacyCommand {
       Audit(audit::Args),
       Export(export::Args),
       Redact(redact::Args),
       Consent(consent::Args),
   }
   
   pub mod audit;
   pub mod export;
   pub mod redact;
   pub mod consent;
   
   pub async fn run(args: Args, ctx: crate::Context) -> anyhow::Result<()> {
       match args.command {
           PrivacyCommand::Audit(a) => audit::run(a, ctx).await,
           PrivacyCommand::Export(a) => export::run(a, ctx).await,
           PrivacyCommand::Redact(a) => redact::run(a, ctx).await,
           PrivacyCommand::Consent(a) => consent::run(a, ctx).await,
       }
   }
   ```

### Part C — `daemon.rs` (conditional)

9. If `daemon.rs` is genuinely tangled (multiple lifecycle phases mixed together): split into `commands/daemon/{mod.rs, start.rs, stop.rs, status.rs, logs.rs}`.

10. If it's mostly one coherent process-lifecycle loop, leave it alone.

11. Default: skip Part C, document the size in `REFACTOR_NOTES.md` for follow-up.

### All parts

12. Run verification.

## Verification

```bash
cargo check -p moa-cli --all-targets
cargo clippy -p moa-cli --all-targets -- -D warnings
cargo test -p moa-cli --no-run
cargo build -p moa-cli --release   # binary still builds

# Verify the CLI argument tree is unchanged
./target/release/moa --help > /tmp/help-after.txt
# Compare against a snapshot taken before the prompt (if you saved one):
# diff /tmp/help-before.txt /tmp/help-after.txt   # should be empty

./target/release/moa exec --help        # no panic
./target/release/moa privacy --help     # no panic
./target/release/moa privacy audit --help

# File sizes
find crates/moa-cli/src -name '*.rs' -exec wc -l {} + | awk '$1 > 700 {print "TOO BIG:", $0}'
```

## Acceptance criteria

- [ ] `crates/moa-cli/src/main.rs` is under 200 LOC.
- [ ] `crates/moa-cli/src/commands/privacy.rs` no longer exists; replaced by folder.
- [ ] Every subcommand's args + run function are co-located in their own module.
- [ ] No file in `crates/moa-cli/src/` exceeds 700 LOC after the prompt.
- [ ] `moa --help` output is identical to before the prompt.
- [ ] `cargo build -p moa-cli --release` produces a working binary.
- [ ] `cargo check --workspace --all-targets` passes.

## Rollback plan

`git checkout -- crates/moa-cli/`. Self-contained.

## Notes for the agent

- **The CLI argument tree is contractually stable.** Users have scripts that depend on flag names, subcommand names, and `--help` output formatting. No renames, no flag reorders, no help-text rewording.
- **`clap` derive macros are flexible enough** that the split shouldn't require changing the Args structs at all — only their location.
- **Capture `moa --help` output before the prompt** for diff comparison after.
- **Global flags** (those with `global = true`) belong in `Cli` (the top-level parser struct). Subcommand-specific flags belong in each `Args` struct. Don't re-arrange.
- **`Context` type is the dependency-injection holder** — wires up `MoaConfig`, the orchestrator, etc. Construct in `runtime_init::build_context`, pass to every `run()`.
- **If `Context` doesn't currently exist as a type** (everything is constructed inline in each subcommand), introduce it. This is the one structural change worth making, and it's necessary for the split to be clean.
- **`--json` and `--bare` output flags**: preserve exact behavior. If they were inline in `main.rs`, move to a small `output.rs` module.
- **Time budget**: 2 sessions. `main.rs` is 3k LOC and the privacy command is 2k; that's a lot of mechanical movement.
- **Anti-pattern**: don't introduce `dyn Command` trait for subcommands. The clap derive + match-dispatch pattern is fine and idiomatic.
