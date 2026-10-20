# Optional proof execution trace

Add `--proof-progress auto` to a Tachyon command that reconstructs spendability:

```sh
zcash-devtool tachyon --wallet-dir <test-wallet> --proof-progress auto <command and arguments>
```

The global option also works after the command. The default is `off`. `plain`
writes JSON lines to stderr. `auto` and `tty` use append-only readable lines when
stderr is a terminal and JSON lines when redirected. Successful stdout JSON and
existing command names remain unchanged. Offline inspection such as `balance`
does not prove and emits no proof events. Do not repeat a payment just to compare
output modes; retain normal operation IDs and recovery procedures.

Events carry a process-local `step_id`, operation name, `dependencies`, and state
(`started`, `completed`, `failed`, or `interrupted`). Terminal events also show
active IDs. Final events include measured `elapsed_ms`; start events omit it.
Dependencies identify instrumented proof outputs consumed by the next seed/fuse
call. Empty lists mean no earlier instrumented output is consumed.

Coverage includes NoteSeed, NullifierDerive, SpendableInit, QR intake/routing/
merging/sealing, QrUnspentInit, UnspentBind, QrSpendableInit, and SpendableLift.
ProofStamp::lift reports the aggregate call; dependency-library internals are not
inferred. No witnesses, note/nullifier/wallet/key identifiers, percentages, or
estimated parallelism are emitted. Current epoch loops and merge rounds remain
sequential. Timing and operation shape are diagnostic metadata and should remain
separate from public explorer data.

Dropped spans report `interrupted` during Rust unwinding. Abrupt termination cannot
emit a final event; unmatched starts remain unknown. Stderr write failures do not
change the payment result. No animation or cursor manipulation is used.

Illustrative JSON (timings are not a performance claim):

```json
{"type":"proof_progress","step_id":1,"operation":"NoteSeed","dependencies":[],"state":"started"}
{"type":"proof_progress","step_id":1,"operation":"NoteSeed","dependencies":[],"state":"completed","elapsed_ms":2}
{"type":"proof_progress","step_id":2,"operation":"NullifierDerive","dependencies":[1],"state":"started"}
```

## Renderer choice for review

The binary already installs a `tracing-subscriber` layer on stderr with
`EnvFilter`. This change deliberately retains a separate, flag-controlled proof
renderer: the explicit proof mode must not depend on `RUST_LOG`, dependencies are
JSON arrays, and redirected proof events have no diagnostic timestamps or ANSI
formatting. The existing subscriber is not configured for this per-target output
contract. Reusing it while preserving that contract would require a dedicated
layer/filter and binary-to-wallet configuration, rather than simply replacing
writes with spans. This is a review tradeoff, not a claim that tracing cannot
support the feature. If maintainers prefer standard tracing records over this
contract, spans and a subscriber layer are the alternative.

One mutex serializes ID allocation, active-set updates, and event writes. The
start event and ID allocation use the same lock hold. This favors coherent,
append-only output for a diagnostic option over nonblocking logging.

## Validation

The repository toolchain file selects Rust. Run:

```sh
RUSTFLAGS='--cfg zcash_unstable="nutachyon"' \
RUSTDOCFLAGS='--cfg zcash_unstable="nutachyon"' \
cargo test --locked -p tachyon-wallet
```

One child-process regression runs the wallet's mock-proof reconstruction path in
off/plain/auto/tty modes and checks stdout separation, dependency completion order,
paired lifecycle events, failed/dropped steps, and redirected terminal fallback.
Other tests cover JSON escaping, terminal formatting, overlapping IDs, and CLI mode
parsing. The fixture uses in-memory test data; it is not a production proof benchmark.
No ignored interactive demo is shipped. See also [wallet validation](tachyon.md#validation).
