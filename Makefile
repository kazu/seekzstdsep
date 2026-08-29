# The whole gate: what CI runs, runnable locally as `make ci`. The hook tests drive a
# real nushell and need `nu` on PATH, so they live in their own target and CI job.

.PHONY: ci hook

ci:
	cargo fmt --all --check
	cargo test --workspace
	cargo check --workspace --all-targets
	cargo bench --benches -- --test
	cd bench && cargo check --all-targets

hook:
	cargo build -p nu_plugin_zstdsep
	nu nu_plugin_zstdsep/tests/run-hook.nu
