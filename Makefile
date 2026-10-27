.PHONY: build test test-runtime test-generated test-js-runtime fmt-check lint check certify certify-cloudflare update-locks ci clean

build:
	cargo build

test:
	cargo test
	cargo test --manifest-path runtime/bind-runtime/Cargo.toml

# The Rust runtime's own tests.
test-runtime:
	cargo test --manifest-path runtime/bind-runtime/Cargo.toml

# Builds and runs generated Rust crates and JS modules (snapshots, servers, the server
# differential test and the three-way evaluator x Rust x JS one, which needs node). Tests that
# need PostgreSQL run against BIND_TEST_DATABASE_URL and skip without it.
test-generated:
	cargo test -- --ignored

# The JS runtime's tests (runtime/bind-runtime-js/tests/run.mjs) under node; no npm packages.
test-js-runtime:
	node runtime/bind-runtime-js/tests/run.mjs

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --locked --all-targets -- -D warnings

# The four targets of examples/commerce: aws-lambda and vercel, each in the ssr and split modes.
TARGETS := aws-lambda-ssr aws-lambda-split vercel-ssr vercel-split
CLOUDFLARE := --manifest examples/cloudflare-pure/bind.toml --target cloudflare

check:
	for target in $(TARGETS); do cargo run -- check --target $$target || exit 1; done
	cargo run -- check $(CLOUDFLARE)

# Certification only accepts the committed locks (bind.lock, .bind/locks/).
certify: certify-cloudflare
	for target in $(TARGETS); do cargo run -- certify --target $$target || exit 1; done

certify-cloudflare:
	cargo run -- certify $(CLOUDFLARE)

# Accepts a new dependency resolution for every target; review and commit the lock changes.
update-locks:
	for target in $(TARGETS); do cargo run -- certify --target $$target --update-lock || exit 1; done
	cargo run -- certify $(CLOUDFLARE) --update-lock

# What CI runs.
ci: fmt-check lint test test-js-runtime certify

clean:
	cargo clean
