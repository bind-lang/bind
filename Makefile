.PHONY: build test test-generated test-js-runtime check certify certify-aws certify-vercel clean

DATABASE_URL ?= postgresql://bind:bind@localhost/bind
export DATABASE_URL

build:
	cargo build

test:
	cargo test
	cargo test --manifest-path runtime/bind-runtime/Cargo.toml

# Builds and runs generated Rust crates (snapshots, servers, the differential test). Tests that
# need PostgreSQL run against BIND_TEST_DATABASE_URL and skip without it.
test-generated:
	cargo test -- --ignored

# The JS runtime's tests (runtime/bind-runtime-js/tests/run.mjs) under node; no npm packages.
test-js-runtime:
	node runtime/bind-runtime-js/tests/run.mjs

check:
	cargo run -- check --target aws-lambda
	cargo run -- check --target vercel

certify: certify-aws certify-vercel

certify-aws:
	cargo run -- certify --target aws-lambda

certify-vercel:
	cargo run -- certify --target vercel

clean:
	cargo clean
