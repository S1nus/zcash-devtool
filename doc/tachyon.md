# Experimental Tachyon wallets

The optional `tachyon` command group supports direct Zakura RPC synchronization,
shielding a transparent UTXO, sending Tachyon payments with change, and
unshielding. Each wallet has its own persistent state and machine-readable JSON
output. A later script or sidecar can operate several wallets; there is no
random workload loop, miner, or automatic aggregation in devtool.

**Private development networks and test funds only.** Ragu proofs are mocked.
Wallet seeds and note receipts are **unencrypted**, unlike ordinary devtool
wallets. On Unix, wallet directories must have mode 0700 and newly created
database/receipt files have mode 0600. On other platforms, restrict access with
OS ACLs yourself. Back up the private wallet directory, not just the seed: note
plaintexts cannot currently be recovered from the chain. Never commit it.

## Build and node requirements

Use Rust 1.97.1 (pinned in `rust-toolchain.toml`):

```sh
ROCKSDB_LIB_DIR="/opt/homebrew/lib" CARGO_INCREMENTAL=0 \
  RUSTFLAGS='--cfg zcash_unstable="nutachyon"' \
  cargo build --release --locked --features tachyon
```

This feature pins Tachyon `a4abfbdd`, Ragu `0881656d`, and Zakura common
`a9081dc0`, matching the experimental node integration. It uses common's V7
serialization, txid and sighash implementation, including a zero ZIP-233
amount. The ordinary wallet dependencies remain separate and unchanged.

Connect to a trusted, Tachyon-enabled archive Zakura node with
`gettachyonblock` (the proof-sync RPC) and `sendrawtransaction`. NuTachyon must
already be active. The node and wallet must use the same epoch length: the
default build uses 4096 blocks, not Tachyon's short `test-epoch-size` setting.
The supplied activation height is checked against the feed. Genesis and the
activation block are pinned so accidental network changes/regenesis fail closed.

For the internal miner, set `tachyon_workload = false`: the old output-only
generator replaces normal mempool selection. Keep `internal_miner = true` and
set `miner_address` to a wallet's `funding_address` if you want its block rewards
to fund the simulation. Wait for 100 confirmations before shielding coinbase
outputs. The RPC also needs retained previous transaction bodies for shielding.
No datastore reset is required.

Keep RPC on localhost, an SSH tunnel, or an authenticated HTTPS connection.
Use `--cookie-file /path/to/.cookie` during initialization if cookie auth is
enabled. The cookie is reread on each request. An authenticated HTTP connection
does not encrypt credentials; do not expose it on an untrusted network.

## Two-wallet example

Commands below use the compiled binary; no Cargo rebuild is needed per payment.
Replace height `8` with your node's NuTachyon activation height.

```sh
DEVTOOL=./target/release/zcash-devtool

$DEVTOOL tachyon --wallet-dir ./alice init \
  --rpc-url http://127.0.0.1:18232 --activation-height 8 --accept-mock-proofs
$DEVTOOL tachyon --wallet-dir ./bob init \
  --rpc-url http://127.0.0.1:18232 --activation-height 8 --accept-mock-proofs

$DEVTOOL tachyon --wallet-dir ./alice address
$DEVTOOL tachyon --wallet-dir ./alice sync
$DEVTOOL tachyon --wallet-dir ./alice balance
```

`sync` ingests at most 1000 blocks per invocation (adjustable with
`--max-blocks`, maximum 10000). Repeat until `caught_up` is true. Balances are
offline snapshots of the last successful sync, with note states `unconfirmed`,
`available`, `reserved`, and `spent`. `available_zat` is before fees and is not a
promise that multiple notes can be combined.

Fund Alice's reported transparent `funding_address`, then shield one confirmed
output. This consumes the entire UTXO less a fixed 50,000-zatoshi fee:

```sh
$DEVTOOL tachyon --wallet-dir ./alice shield --txid PREVIOUS_TXID --index 0
# Wait until mined, then:
$DEVTOOL tachyon --wallet-dir ./alice sync
$DEVTOOL tachyon --wallet-dir ./alice balance
```

Pay Bob using his `tachyon_address`. The current `tachyon-test:<payment-key>`
format is a devtool-specific test encoding, not a standardized Zcash address.

```sh
$DEVTOOL tachyon --wallet-dir ./alice send \
  --to tachyon-test:BOBS_PAYMENT_KEY --amount-zat 1000000
# Use the returned transaction ID:
$DEVTOOL tachyon --wallet-dir ./alice export-receipt \
  --txid PAYMENT_TXID --out ./payment.json
$DEVTOOL tachyon --wallet-dir ./bob receive --receipt ./payment.json
# After mining:
$DEVTOOL tachyon --wallet-dir ./bob sync
$DEVTOOL tachyon --wallet-dir ./bob balance

$DEVTOOL tachyon --wallet-dir ./bob unshield \
  --to TESTNET_TRANSPARENT_ADDRESS --amount-zat 100000
```

**Deliver receipts confidentially out of band.** They contain the recipient's
note plaintext, not a spending key, but expose the amount and commitment
opening. Tachyon does not currently provide an on-chain encrypted-note scanning
format here. Import checks ownership and network identity, and does not credit
funds until the note commitment appears in the validated feed. Own outputs and
change are recorded automatically. Export never overwrites an existing file.

## Recovery and proof updates

Payments consume one sufficiently large note, create recipient/change output
actions, and pay 50,000 zatoshis. Transparent funding uses one owned testnet
P2PKH UTXO; unshielding supports testnet P2PKH or P2SH recipients. Multi-note
selection/consolidation, encrypted seed storage, memo transport and ordinary
`wallet`/`inspect` integration are not implemented.

Input reservation, raw transaction and output receipts are committed to SQLite
**before** broadcast. If a request fails, the input remains reserved because
the node may have accepted the transaction. Recover using:

```sh
$DEVTOOL tachyon --wallet-dir ./alice transactions
$DEVTOOL tachyon --wallet-dir ./alice submit --txid PAYMENT_TXID
# Export still works even if the original broadcast failed.
$DEVTOOL tachyon --wallet-dir ./alice export-receipt \
  --txid PAYMENT_TXID --out ./recovered-payment.json
```

Retry resends identical bytes. Reservations expire only after the synced tip
passes the transaction expiry height. Payments expire within 20 blocks and
never beyond the current epoch. At an epoch's last block, wait for its successor
before sending. A reorganization rolls back stored public inputs; note and
transaction states are recalculated, including mined transactions returning to
pending. Concurrent commands against one wallet fail with a busy error rather
than racing input selection; parallelize across wallets instead.

The wallet validates ordered stamp commitments, anchor continuity and epoch
crossings from `gettachyonblock`. It reconstructs mock note-secret, nullifier,
closed-epoch absence and spendability proofs locally, then lifts the spending
stamp to its synced tip within the active epoch. It supports empty epochs and
retains historical public inputs past node tachygram pruning. It does **not**
independently validate consensus or the authenticity of a malicious RPC node.

This implementation rebuilds proofs and loads its stored feed on demand. It is
for small development chains, not a scalable proof service. A closed epoch is
limited to 65,536 tachygrams; larger epochs fail explicitly and will require
partitioned evidence. A later sidecar can cache evidence, schedule wallets,
exchange receipts, and observe mining; node-side aggregation remains Zakura's job.

## Validation

```sh
ROCKSDB_LIB_DIR="/opt/homebrew/lib" CARGO_INCREMENTAL=0 \
  RUSTFLAGS='--cfg zcash_unstable="nutachyon"' \
  RUSTDOCFLAGS='--cfg zcash_unstable="nutachyon"' \
  cargo test --locked -p tachyon-wallet
ROCKSDB_LIB_DIR="/opt/homebrew/lib" CARGO_INCREMENTAL=0 \
  RUSTFLAGS='--cfg zcash_unstable="nutachyon"' \
  cargo clippy --locked --workspace --all-targets --features tachyon -- -D warnings
```
