# The whole gate: what CI runs, runnable locally as `make ci`. The hook tests drive a
# real nushell and need `nu` on PATH, so they live in their own target and CI job.
#
# Release:
#   make release V=0.5.0 P=0.3.1
# V is the crate and P is the plugin, both released from $(MASTER).
# The plugin alone, with the crate left at the version it has:
#   make release-plugin P=0.3.1

REMOTE ?= gh
MASTER ?= master
RUSTUP_TOOLCHAIN ?= 1.96.1
export RUSTUP_TOOLCHAIN

.PHONY: ci hook changelog set-version tag push-release release release-plugin

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

# CHANGELOG.md is rendered from the commit log by git-cliff (`cargo install git-cliff`), never
# edited by hand. With V the commits that have no tag yet are named for the release being cut,
# which is why `release` runs this before the release commit: `commit -am` picks the file up.
#
# Only $(MASTER) carries releases, so the log divides into releases on that branch.
changelog:
	@if [ -n "$(V)" ]; then git-cliff --tag "v$(V)" -o CHANGELOG.md; else git-cliff -o CHANGELOG.md; fi

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

# Release only from $(MASTER); tag and publish both packages from the same commit.
release:
	@test -n "$(V)" -a -n "$(P)" || \
	  { echo 'usage: make release V=<crate> P=<plugin>' >&2; exit 1; }
	@test "$$(git branch --show-current)" = "$(MASTER)" || \
	  { echo 'release must run on $(MASTER)' >&2; exit 1; }
	@test -z "$$(git status --porcelain)" || \
	  { echo 'release requires a clean worktree' >&2; exit 1; }
	$(MAKE) set-version V=$(V) P=$(P)
	$(MAKE) changelog V=$(V)
	$(MAKE) ci
	git commit -am "seekzstdsep: release $(V) and $(P)"
	$(MAKE) tag
	$(MAKE) push-release

# The plugin alone. The crate keeps its number, so `tag` adds only the plugin's tag.
# No changelog: the file is divided by the crate's tags, and plugin commits land under the next one.
release-plugin:
	@test -n "$(P)" || { echo 'usage: make release-plugin P=<plugin>' >&2; exit 1; }
	@test "$$(git branch --show-current)" = "$(MASTER)" || \
	  { echo 'release-plugin must run on $(MASTER)' >&2; exit 1; }
	@test -z "$$(git status --porcelain)" || \
	  { echo 'release-plugin requires a clean worktree' >&2; exit 1; }
	$(MAKE) set-version V=$$(cargo metadata --format-version 1 --no-deps | jq -r '.packages[]|select(.name=="seekzstdsep").version') P=$(P)
	$(MAKE) ci
	git commit -am "nu_plugin_zstdsep: release $(P)"
	$(MAKE) tag
	$(MAKE) push-release
