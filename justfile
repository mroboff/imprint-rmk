default:
  @just --list

fmt:
  cargo fmt -p moergo-config -p moergo-config-wasm -p moergo-control -p xtask
  cargo fmt --manifest-path crates/glove80-rmk/Cargo.toml
  cargo fmt --manifest-path crates/go60-rmk/Cargo.toml
  cargo fmt --manifest-path crates/imprint-rmk/Cargo.toml
  cargo fmt --manifest-path crates/split-lighting-tests/Cargo.toml

check:
  cargo run --quiet -p xtask -- check

host-test:
  cargo test --workspace

board-check:
  cd crates/glove80-rmk && cargo check --bins
  cd crates/go60-rmk && cargo check --bins
  cd crates/imprint-rmk && cargo check --bins

parity-check: check board-check

firmware: dist

go60-firmware: go60-dist

imprint-firmware: imprint-dist

firmware-all: firmware go60-firmware imprint-firmware

go60-dist:
  cargo run --quiet -p xtask -- dist-go60

imprint-dist:
  cargo run --quiet -p xtask -- dist-imprint

dist:
  cargo run --quiet -p xtask -- dist

inspect-uf2 file:
  cargo run --quiet -p xtask -- inspect-uf2 "{{file}}"
