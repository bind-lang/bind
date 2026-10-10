.PHONY: build test check certify certify-aws certify-vercel clean

DATABASE_URL ?= postgresql://bind:bind@localhost/bind
export DATABASE_URL

build:
	cargo build

test:
	cargo test

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
