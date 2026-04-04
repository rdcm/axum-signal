build:
	cargo build --release

format:
	cargo sort --workspace
	cargo fmt --all

lint:
	cargo clippy --release --all-targets

run:
	ulimit -n 65535 && ./target/release/broadcast

benchmark:
	ulimit -n 65535 && k6 run benchmarks/10k_connections.js