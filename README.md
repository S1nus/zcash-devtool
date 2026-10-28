# zcash-devtool (Tachyon Fork💫)

The Tachyon fork of [zcash-devtool](https://github.com/zcash/zcash-devtool). See the upstream repo for docs and usage info.

Here you will find utilities for interacting with the Tachyon shielded protocol.

Run `zcash-devtool tachyon --help` to view available commands.

## Build

`RUSTFLAGS='--cfg zcash_unstable="nutachyon"' cargo build --release --locked --features tachyon`

the binary will be available in `./target/release/zcash-devtool`


## Security Warnings

**DO NOT USE THIS IN PRODUCTION!!!**

This app has not been written with security in mind. It does however have affordances
such as encryption of the mnemonic seed phrases that should make it viable for small
scale experimentation, at your own risk.

## Documentation

For experimental mock-proof Tachyon wallets connected directly to a private
Zakura network, see [the Tachyon guide](doc/tachyon.md). This is an optional,
separate wallet implementation, not an extension of the production wallet DB.

For a step-by-step guide for how to get started using these tools, see [this
walkthrough](doc/walkthrough.md).

## License

All code in this workspace is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.