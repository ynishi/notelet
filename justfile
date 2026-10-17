# notelet recipes. Crate-targeted on purpose: never `--workspace`.

set shell := ["sh", "-eu", "-c"]
set dotenv-load := true

default:
    @just --list

# fmt + clippy on both crates
check:
    cargo fmt --all -- --check
    cargo clippy -p notelet-core -p notelet-mcp -p notelet --all-targets -- -D warnings

# unit tests (no database)
test:
    cargo test -p notelet-core
    cargo test -p notelet-mcp
    cargo test -p notelet

# regenerate the tool list in README.md and docs/guides/tools.md
gen-docs:
    NOTELET_UPDATE_DOCS=1 cargo test -p notelet-mcp tool_docs_match_router -- --nocapture

# Postgres integration test; needs NOTELET_TEST_DATABASE_URL
test-pg:
    cargo test -p notelet-core --test pg -- --nocapture

# run the HTTP server against the database in the environment (dev)
dev:
    cargo run -p notelet-mcp -- serve

# apply migrations to the database in the environment
migrate:
    cargo run -p notelet-mcp -- migrate

# build docs for the core crate (the design document)
doc:
    cargo doc -p notelet-core --no-deps --open

# --- deploy: run by a human -------------------------------------------------

# deploy the dev app (fly.dev.toml); pass the app name when it differs from the toml
deploy-dev app="notelet-dev":
    fly deploy -c fly.dev.toml -a {{app}} --ha=false

# deploy the production app (fly.toml)
deploy:
    fly deploy -c fly.toml
