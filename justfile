tests := "buy_sell_instructions trade_tx_and_quotes async_client_fetchers v2_custom_quote_mint v2_creator_fees v3_trades multi_hop_swap"

examples := "create_v2 create_v2_and_buy buy_v2 sell_v2 buy_amm sell_amm claim_cashback_v2 collect_creator_fee_v2 distribute_creator_fees_v2 collect_coin_creator_fee transfer_creator_fees_to_pump_v2 buy_v3 sweep_creator_fee multi_hop_swap"

# <so-file-base-name>:<program-id> pairs for programs the local validator needs.
# These follow the `cluster` arg passed to `dump-programs`.
program_ids := "pump:6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P pump_amm:pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA pump_fees:pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ pump_agent_payments:AgenTMiC2hvxGebTsgmsD4HHBa8WEcqGFf87iwRRxLo7 mayhem_program:MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e mpl_token_metadata:metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s"

# Programs that must ALWAYS be fetched from mainnet regardless of the
# `cluster` arg: devnet doesn't have them. `pump_stables_router` is *not*
# in this list because it isn't deployed on mainnet either — you'll need to
# build it from `programs/pump-stables-router` and drop the resulting .so
# into `artifacts/pump_stables_router.so` yourself.
mainnet_only_program_ids := "serum_dex:srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX raydium_amm_v4:675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"

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
        cargo test --features local-validator --test "$t" -- --ignored --nocapture --test-threads=1
    done

test-one name filter='':
    cargo test --features local-validator --test {{name}} -- --ignored --nocapture --test-threads=1 {{filter}}

examples:
    #!/usr/bin/env bash
    set -euo pipefail
    for ex in {{examples}}; do
        echo "=== $ex ==="
        cargo run --features local-validator --example "$ex"
    done

example-one name:
    cargo run --features local-validator --example {{name}}

# Mint test USDC to a wallet on the running local validator.
# Amount is in human USDC (6 decimals); e.g. `just airdropusdc <wallet> 100`.
airdropusdc wallet amount:
    cargo run --features local-validator --bin airdropusdc -- {{wallet}} {{amount}}

# Copy the pump / pump_amm / pump_fees IDLs and the program builds the local validator loads from a
# pump-programs-monorepo checkout (`monorepo` = its root, `ref` = a git ref;
# defaults to the working tree). The SDK's builders are generated from these
# IDLs, so refresh them whenever the programs change.
idls monorepo="../../programs/pump-programs-monorepo" ref="":
    #!/usr/bin/env bash
    set -euo pipefail
    show() { if [ -n "{{ref}}" ]; then git -C "{{monorepo}}" show "{{ref}}:$1"; else cat "{{monorepo}}/$1"; fi; }
    show idls-top-level/pump.json > idls/pump.json
    show idls-top-level/pump_amm.json > idls/pump_amm.json
    show idls-top-level/pump_fees.json > idls/pump_fees.json
    show litesvm-testing-common/programs-new/pump.so > artifacts/pump.so
    show litesvm-testing-common/programs-new/pump_amm.so > artifacts/pump_amm.so
    show litesvm-testing-common/programs-new/pump_fees.so > artifacts/pump_fees.so
    show litesvm-testing-common/programs-new/mayhem_program.so > artifacts/mayhem_program.so
    show litesvm-testing-common/programs-new/pump_agent_payments.so > artifacts/pump_agent_payments.so
    ls -la idls/pump.json idls/pump_amm.json idls/pump_fees.json artifacts/pump.so artifacts/pump_amm.so artifacts/pump_fees.so artifacts/mayhem_program.so artifacts/pump_agent_payments.so

# Dump deployed programs into artifacts/ for the local validator.
# `cluster` controls only the entries in `program_ids`; everything in
# `mainnet_only_program_ids` is always fetched from mainnet.
# Override cluster with e.g. `just dump-programs m` for mainnet.
#
# `solana program dump` only handles Upgradeable BPF Loader (Loader v3).
# Pre-upgradeable programs like raydium_amm_v4 / serum_dex are BPF Loader 2
# and fail that path with "is not an SBF program"; for those we fall back
# to `solana account -o <FILE>` which writes the raw account data — and for
# Loader 2 programs the account data *is* the .so payload.
dump-programs cluster="d":
    #!/usr/bin/env bash
    set -uo pipefail
    mkdir -p artifacts
    failed=0
    dump_one() {
        local name="$1" id="$2" cl="$3"
        local dst="artifacts/${name}.so"
        echo "Dumping $name ($id) from -u $cl -> $dst"
        if solana program dump -u "$cl" "$id" "$dst" 2>/dev/null; then
            return 0
        fi
        if solana account -u "$cl" -o "$dst" "$id" >/dev/null 2>&1 \
            && [ -s "$dst" ]; then
            echo "  (fetched via solana account; BPF Loader 2 program)"
            return 0
        fi
        echo "  FAILED: $name ($id)"
        return 1
    }
    for entry in {{program_ids}}; do
        name="${entry%%:*}"
        id="${entry##*:}"
        dump_one "$name" "$id" "{{cluster}}" || failed=$((failed + 1))
    done
    for entry in {{mainnet_only_program_ids}}; do
        name="${entry%%:*}"
        id="${entry##*:}"
        dump_one "$name" "$id" "m" || failed=$((failed + 1))
    done
    if [ "$failed" -gt 0 ]; then
        echo "$failed program(s) failed to dump."
        exit 1
    fi

build-and-test: build
    #!/usr/bin/env bash
    set -euo pipefail
    just clone-accounts
    cargo run --features local-validator --bin local-validator &
    validator_pid=$!
    trap "kill $validator_pid 2>/dev/null || true" EXIT
    sleep 10
    just test
