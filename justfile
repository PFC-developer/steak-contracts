check:
  cargo check --target wasm32-unknown-unknown --lib

clippy:
  cargo +nightly clippy --tests

format:
    cargo +nightly fmt

test:
  cargo test

optimize:
  docker run --rm -v "$(pwd)":/code \
    --mount type=volume,source="$(basename "$(pwd)")_cache",target=/target \
    --mount type=volume,source=registry_cache,target=/usr/local/cargo/registry \
    --platform linux/amd64 \
    cosmwasm/optimizer:0.16.0
