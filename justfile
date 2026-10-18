# notelet recipes. Crate-targeted on purpose: never `--workspace`.

set shell := ["sh", "-eu", "-c"]
set dotenv-load := true

default:
    @just --list

# fmt + clippy on both crates
check:
    cargo fmt --all -- --check
    cargo clippy -p notelet-core -p notelet-mcp -p notelet --all-targets -- -D warnings

# unit tests (no database; NOTELET_TEST_DATABASE_URL is ignored, see test-pg)
test:
    env -u NOTELET_TEST_DATABASE_URL cargo test -p notelet-core
    cargo test -p notelet-mcp
    cargo test -p notelet

# regenerate the tool list in README.md and docs/guides/tools.md
gen-docs:
    NOTELET_UPDATE_DOCS=1 cargo test -p notelet-mcp tool_docs_match_router -- --nocapture

# start the throwaway Postgres for the integration test (127.0.0.1:55433, data in tmpfs)
pg-up:
    #!/bin/sh
    set -eu
    if [ -z "$(docker ps -q --filter name=^notelet-pgtest$)" ]; then
        # a stopped leftover of the same name would make `docker run` fail
        if [ -n "$(docker ps -aq --filter name=^notelet-pgtest$)" ]; then
            docker rm -f notelet-pgtest >/dev/null
        fi
        docker run -d --rm --name notelet-pgtest \
            -e POSTGRES_USER=notelet -e POSTGRES_PASSWORD=notelet -e POSTGRES_DB=notelet_test \
            -p 127.0.0.1:55433:5432 --tmpfs /var/lib/postgresql/data postgres:16 >/dev/null
    fi
    i=0
    until docker exec notelet-pgtest pg_isready -h 127.0.0.1 -p 5432 -U notelet -d notelet_test >/dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -ge 30 ]; then
            echo "pg-up: notelet-pgtest not ready after 30s" >&2
            exit 1
        fi
        sleep 1
    done

# remove the throwaway Postgres; succeeds when absent
pg-down:
    docker rm -f notelet-pgtest >/dev/null 2>&1 || true

# Postgres integration test; defaults to the `just pg-up` container, refuses the dev database
test-pg:
    #!/bin/sh
    set -eu
    if [ -z "${NOTELET_TEST_DATABASE_URL:-}" ]; then
        NOTELET_TEST_DATABASE_URL=postgres://notelet:notelet@127.0.0.1:55433/notelet_test
    elif [ "$NOTELET_TEST_DATABASE_URL" = "${NOTELET_DATABASE_URL:-${DATABASE_URL:-}}" ]; then
        echo "test-pg: NOTELET_TEST_DATABASE_URL equals NOTELET_DATABASE_URL (or DATABASE_URL); refusing to test against the dev database" >&2
        exit 1
    fi
    export NOTELET_TEST_DATABASE_URL
    cargo test -p notelet-core --test pg -- --nocapture

# throwaway Docker Postgres: pg-up, test-pg, pg-down (pg-down runs even when the test fails)
test-pg-local:
    #!/bin/sh
    set -eu
    trap '{{just_executable()}} pg-down' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM HUP
    {{just_executable()}} pg-up
    NOTELET_TEST_DATABASE_URL=postgres://notelet:notelet@127.0.0.1:55433/notelet_test {{just_executable()}} test-pg

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
