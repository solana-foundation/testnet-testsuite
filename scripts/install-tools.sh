#!/usr/bin/env bash
set -euo pipefail

rustup component add rustfmt clippy

cargo install cargo-nextest --locked
cargo install cargo-deny --locked

echo "done. verify: cargo nextest --version && cargo deny --version"
