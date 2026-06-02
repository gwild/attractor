# attractor-config

One shared place to read configuration and secrets, so **no Attractor crate
hardcodes** a path, port, endpoint, or token. The crate owns the *mechanism*;
each crate owns its *schema* — config stays modular, not a god-struct.

Three jobs, all **fail-loud** (no silent defaults — RULES.md NO FALLBACKS):

| Function | Job |
|---|---|
| `load::<T>(config_path, section)` | Deserialize a named `config.yaml` section into the caller's own typed struct. |
| `load_dotenv(dir)` + `secret(&env, key)` | Required secrets from `.env` *or* the process env — never a tracked file. No `std::env::set_var`. |
| `resolve_path(base, value)` | Resolve a config-relative path against a base, so configs carry relative paths, not machine-specific absolutes. |

## Usage — each crate reads its own slice of one shared file

```rust
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct ApiConfig {
    socket_path: String,
    tcp_addr: String,
}

fn boot(config_path: &Path) -> Result<(), attractor_config::ConfigError> {
    let base = config_path.parent().unwrap_or(Path::new("."));

    // 1. Typed section — this crate owns ApiConfig; attractor-config owns loading.
    let cfg: ApiConfig = attractor_config::load(config_path, "api")?;

    // 2. Relative path → absolute, against the config file's directory.
    let sock = attractor_config::resolve_path(base, &cfg.socket_path);

    // 3. Secret from .env-or-env, fail-loud if absent.
    let env = attractor_config::load_dotenv(base)?;
    let token = attractor_config::secret(&env, "ATTRACTOR_API_TOKEN")?;

    let _ = (sock, cfg.tcp_addr, token);
    Ok(())
}
```

See `config.example.yaml` and `.env.example` at the workspace root for the full
shared contract. `attractor-store`, `attractor-io`, and `attractor-core`'s
ingest each read *their own* section the same way — one file, no duplication.

## Layering rule

Defaults live in the **pure** crates (`attractor-proto`); config **overrides**
them at the **consumer** (`-api`/`-store`/`-io`). This crate is the consumer-side
mechanism — pure (`serde` + `serde_yaml`, no OS/async deps), so it attests on
both 1.92 cross-platform legs.

> `serde_yaml` is maintenance-only; if cargo-deny flags it, the drop-in swap is
> `serde_yml`.
