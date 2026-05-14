tests := "buy_sell_instructions trade_tx_and_quotes async_client_fetchers create_v2_devnet_clone"

default:
    @just --list

build:
    cargo build --features local-validator

clone-accounts:
    cargo run --features local-validator --bin clone_devnet_accounts

local-validator: clone-accounts
    cargo run --features local-validator --bin local-validator

test:
    #!/usr/bin/env bash
    set -euo pipefail
    for t in {{tests}}; do
        echo "=== $t ==="
        cargo test --features local-validator --test "$t" -- --ignored --nocapture
    done

test-one name:
    cargo test --features local-validator --test {{name}} -- --ignored --nocapture

build-and-test: build
    #!/usr/bin/env bash
    set -euo pipefail
    just clone-accounts
    cargo run --features local-validator --bin local-validator &
    validator_pid=$!
    trap "kill $validator_pid 2>/dev/null || true" EXIT
    sleep 10
    just test
