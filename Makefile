# The whole gate: what CI runs, runnable locally as `make ci`. The hook tests drive a
# real nushell and need `nu` on PATH, so they live in their own target and CI job.
#
# Release:
#   make release V=0.5.0 P_NEW=0.3.1 P_OLD=0.3.0
# V is the crate, P_NEW the plugin on $(MASTER), P_OLD the plugin on $(NU_OLD).

REMOTE ?= gh
MASTER ?= master
NU_OLD ?= 0.114/nu

.PHONY: ci hook set-version tag push-release release

ci:
	cargo fmt --all --check
	cargo test --workspace
	cargo check --workspace --all-targets
	cargo bench --benches -- --test
	cd bench && cargo check --all-targets

hook:
	cargo build -p nu_plugin_zstdsep
	nu nu_plugin_zstdsep/tests/run-hook.nu

set-version:
	@test -n "$(V)" -a -n "$(P)" || { echo 'usage: make set-version V=<crate> P=<plugin>' >&2; exit 1; }
	sed -i '0,/^version = /s/^version = .*/version = "$(V)"/' Cargo.toml
	sed -i '0,/^version = /s/^version = .*/version = "$(P)"/' nu_plugin_zstdsep/Cargo.toml
	sed -i 's|^seekzstdsep = { path = "\.\.", version = "[^"]*" }|seekzstdsep = { path = "..", version = "$(basename $(V))" }|' nu_plugin_zstdsep/Cargo.toml
	cargo check --workspace
	cd bench && cargo check

# Numbers come from Cargo.toml, never from an argument: `release.yml` refuses a tag that
# disagrees with it.
tag:
	@v=$$(cargo metadata --format-version 1 --no-deps | jq -r '.packages[]|select(.name=="seekzstdsep").version'); \
	p=$$(cargo metadata --format-version 1 --no-deps | jq -r '.packages[]|select(.name=="nu_plugin_zstdsep").version'); \
	if git rev-parse -q --verify "refs/tags/v$$v" >/dev/null; then \
	  echo "v$$v is already tagged, leaving it"; \
	else \
	  git tag "v$$v"; echo "tagged v$$v"; \
	fi; \
	git tag "nu_plugin_zstdsep-v$$p"; echo "tagged nu_plugin_zstdsep-v$$p"

# The crate's tag goes first: the plugin's publish resolves seekzstdsep from crates.io.
# The run is found by the tag it was triggered from, since pushing the branch starts a
# rust.yml run that a bare `gh run watch` would attach to instead.
push-release:
	@git push $(REMOTE) "$$(git rev-parse --abbrev-ref HEAD)"
	@for t in $$(git tag --points-at HEAD | grep '^v') $$(git tag --points-at HEAD | grep -v '^v'); do \
	  git push $(REMOTE) "$$t" || exit 1; \
	  id=; \
	  for i in $$(seq 30); do \
	    id=$$(gh run list --workflow=release.yml --branch "$$t" --limit 1 --json databaseId -q '.[0].databaseId'); \
	    [ -n "$$id" ] && break; \
	    sleep 2; \
	  done; \
	  test -n "$$id" || { echo "no release.yml run for $$t" >&2; exit 1; }; \
	  gh run watch "$$id" --exit-status || exit 1; \
	done

# Nothing is pushed until both branches are tagged.
release:
	@test -n "$(V)" -a -n "$(P_NEW)" -a -n "$(P_OLD)" || \
	  { echo 'usage: make release V=<crate> P_NEW=<plugin on $(MASTER)> P_OLD=<plugin on $(NU_OLD)>' >&2; exit 1; }
	git checkout $(MASTER)
	$(MAKE) set-version V=$(V) P=$(P_NEW)
	$(MAKE) ci
	git commit -am "seekzstdsep: zstdsep: release $(V) and $(P_NEW)"
	$(MAKE) tag
	git checkout $(NU_OLD)
	$(MAKE) set-version V=$(V) P=$(P_OLD)
	$(MAKE) ci
	git commit -am "seekzstdsep: zstdsep: release $(V) and $(P_OLD)"
	$(MAKE) tag
	git checkout $(MASTER)
	$(MAKE) push-release
	git checkout $(NU_OLD)
	$(MAKE) push-release
	git checkout $(MASTER)
