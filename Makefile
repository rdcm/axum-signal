build:
	cargo build --release

format:
	cargo sort --workspace
	cargo fmt --all

lint:
	cargo clippy --release --all-targets

tests:
	cargo nextest run --workspace --no-fail-fast

run-server:
	ulimit -n 65535 && ./target/release/server

benchmark:
	ulimit -n 65535 && k6 run benchmarks/10k_connections.js

publish:
	cargo publish --dry-run