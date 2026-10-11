.PHONY: build test test-generated test-js-runtime check certify clean

DATABASE_URL ?= postgresql://bind:bind@localhost/bind
export DATABASE_URL

build:
	cargo build

test:
	cargo test
	cargo test --manifest-path runtime/bind-runtime/Cargo.toml

# Builds and runs generated Rust crates and JS modules (snapshots, servers, the server
# differential test and the three-way evaluator x Rust x JS one, which needs node). Tests that
# need PostgreSQL run against BIND_TEST_DATABASE_URL and skip without it.
test-generated:
	cargo test -- --ignored

# The JS runtime's tests (runtime/bind-runtime-js/tests/run.mjs) under node; no npm packages.
test-js-runtime:
	node runtime/bind-runtime-js/tests/run.mjs

# The four targets of examples/commerce: aws-lambda and vercel, each in the ssr and split modes.
TARGETS := aws-lambda-ssr aws-lambda-split vercel-ssr vercel-split

check:
	for target in $(TARGETS); do cargo run -- check --target $$target || exit 1; done

certify:
	for target in $(TARGETS); do cargo run -- certify --target $$target || exit 1; done

clean:
	cargo clean
