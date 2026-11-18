# Build Sekisho, run every test, build the images and run the end-to-end
# suite. `make check` runs it all, in order; docs/testing-on-linux.md says
# what each target proves. Needs GNU make, bash, cargo (through rustup),
# git 2.37 or later, and Docker with Compose v2.

SHELL := bash
.SHELLFLAGS := -euo pipefail -c
.ONESHELL:
.NOTPARALLEL:
.DEFAULT_GOAL := help
MAKEFLAGS += --no-print-directory

COMPOSE := docker compose -f e2e/docker-compose.yml
# Each target's test output, kept for reading after a failure.
LOGS := target/check
# KEEP=1 leaves the end-to-end stack running after `make e2e`.
KEEP ?=

# Shell helpers, at the top of each recipe.
define LIB
step() { printf '\n==> %s\n' "$$*"; }
ok() { printf 'ok    %s\n' "$$*"; }
fail() { printf 'FAIL  %s\n' "$$*" >&2; exit 1; }
# Whether version $$1 is $$2 or later.
ver_ge() { [ "$$(printf '%s\n' "$$2" "$$1" | sort -V | head -n 1)" = "$$2" ]; }
# Passed and failed tests in a cargo test log.
count() { awk '/^test result:/ { p += $$4; f += $$6 } END { print p + 0, f + 0 }' "$$1"; }
# Runs cargo test into a log, and fails unless every test passed (exactly
# $$2 of them, when given, so a filter that matches nothing can't pass).
tests() {
  local name=$$1 want=$$2 status=0 p f
  shift 2
  mkdir -p $(LOGS)
  "$$@" > $(LOGS)/$$name.log 2>&1 || status=$$?
  read -r p f < <(count $(LOGS)/$$name.log)
  [ "$$status" = 0 ] && [ "$$f" = 0 ] && [ "$$p" -gt 0 ] \
    || fail "$$name: $$p passed, $$f failed; see $(LOGS)/$$name.log"
  [ -z "$$want" ] || [ "$$p" = "$$want" ] \
    || fail "$$name: $$p passed, expected $$want; see $(LOGS)/$$name.log"
  ok "$$name: $$p passed, 0 failed"
}
endef

.PHONY: help check prereqs build lint test unix images e2e e2e-down

help:
	@echo 'make check      everything below, in order (about an hour the first time)'
	echo 'make prereqs    cargo, git 2.37 or later, Docker and Compose v2'
	echo 'make build      the workspace, the release binaries, sekisho --help'
	echo 'make lint       cargo fmt --check and clippy -D warnings'
	echo 'make test       every workspace test (Docker must be running)'
	echo 'make unix       what only Linux and other Unix systems exercise'
	echo 'make images     the release image and the end-to-end image'
	echo 'make e2e        the end-to-end suite twice, then SIGTERM (KEEP=1 keeps the stack)'
	echo 'make e2e-down   remove the end-to-end stack and its volumes'
	echo "Logs: $(LOGS)/"

check: prereqs build lint test unix images e2e
	@echo
	echo 'All checks passed.'

prereqs:
	@$(LIB)
	step 'Prerequisites'
	command -v cargo > /dev/null || fail 'cargo not found: install rustup'
	ok "$$(cargo --version)"
	v=$$(git --version | awk '{ print $$3 }')
	ver_ge "$$v" 2.37 || fail "git $$v is older than 2.37: catalog syncs need 2.37 or later"
	ok "git $$v"
	docker info > /dev/null 2>&1 \
	  || fail "Docker isn't reachable: start it, or add yourself to the docker group"
	ok "Docker $$(docker version --format '{{.Server.Version}}')"
	docker compose version > /dev/null 2>&1 || fail 'docker compose (v2) not found'
	ok "$$(docker compose version)"

build:
	@$(LIB)
	step 'Build'
	cargo build --workspace --locked --all-targets
	cargo build --release --locked -p sekisho-server -p sekisho-testkit --bins
	help=$$(./target/release/sekisho --help)
	for command in serve migrate admin-link gateway-agent; do
	  grep -qw -- "$$command" <<< "$$help" || fail "sekisho --help doesn't list $$command"
	done
	ok 'build; sekisho --help lists serve, migrate, admin-link and gateway-agent'

lint:
	@$(LIB)
	step 'Format and lint'
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	ok 'cargo fmt and clippy are clean'

test:
	@$(LIB)
	step "Workspace tests (log: $(LOGS)/workspace.log)"
	docker image inspect postgres:18 > /dev/null 2>&1 || docker pull postgres:18
	tests workspace '' cargo test --workspace --locked --no-fail-fast

unix:
	@$(LIB)
	case "$$(uname -s)" in
	  Linux | Darwin | *BSD) ;;
	  *) fail "make unix needs Linux or another Unix, not $$(uname -s)" ;;
	esac
	step 'What only Unix exercises'
	tests unix-symlinks 1 cargo test --locked -p sekisho-catalog --test catalog_source \
	  symlinks_and_escaping_paths_are_refused -- --nocapture
	if grep -q 'skipping the symlink case' $(LOGS)/unix-symlinks.log; then
	  fail 'the symlink case was skipped'
	fi
	tests unix-git-links 1 cargo test --locked -p sekisho-server --test catalog_sync \
	  symlinks_and_submodules_in_git_are_refused
	tests unix-executable-bit 1 cargo test --locked -p sekisho-catalog --test content_sources \
	  a_skills_executable_bit_is_kept
	tests unix-config-0600 1 cargo test --locked -p sekisho-gateway --test agent \
	  write_atomic_makes_the_file_private
	tests unix-git-hardened 2 cargo test --locked -p sekisho-server --test catalog_sync -- \
	  git_pins_the_checked_address the_token_never_appears_in_errors_or_argv
	tests unix-git-version 2 cargo test --locked -p sekisho-server --lib -- \
	  only_git_that_honours_the_pin_is_used every_git_call_is_hardened

images:
	@$(LIB)
	step 'Release image'
	docker build -t sekisho:check .
	v=$$(docker run --rm sekisho:check --version)
	[[ "$$v" == 'sekisho '[0-9]* ]] || fail "sekisho --version in the image printed: $$v"
	ok "release image: $$v"
	step 'End-to-end image'
	$(COMPOSE) build
	g=$$(docker run --rm --entrypoint git sekisho-e2e:dev --version | awk '{ print $$3 }')
	ver_ge "$$g" 2.37 || fail "git $$g in the image is older than 2.37"
	u=$$(docker run --rm --entrypoint id -u sekisho-e2e:dev)
	[ "$$u" = 65532 ] || fail "the image runs as uid $$u, not 65532"
	ok "end-to-end image: git $$g, uid $$u"

e2e:
	@$(LIB)
	step 'End-to-end stack'
	$(COMPOSE) up -d --wait
	if [ -z '$(KEEP)' ]; then trap '$(COMPOSE) down -v > /dev/null 2>&1 || true' EXIT; fi
	for run in 1 2; do
	  step "End-to-end run $$run (log: $(LOGS)/e2e-$$run.log)"
	  tests e2e-$$run '' cargo test -p sekisho-e2e --locked --no-fail-fast -- \
	    --ignored --test-threads=1
	done
	step 'SIGTERM'
	start=$$SECONDS
	$(COMPOSE) stop -t 10 sekisho
	took=$$((SECONDS - start))
	code=$$(docker inspect --format '{{.State.ExitCode}}' "$$($(COMPOSE) ps -aq sekisho)")
	[ "$$code" = 0 ] || fail "sekisho exited with $$code on SIGTERM, not 0"
	[ "$$took" -lt 10 ] || fail "sekisho took $${took}s to stop: SIGTERM wasn't handled"
	logs=$$($(COMPOSE) logs sekisho)
	grep -q 'shutting down' <<< "$$logs" || fail "sekisho didn't log 'shutting down'"
	ok "SIGTERM: sekisho exited 0 in $${took}s and logged 'shutting down'"
	if [ -n '$(KEEP)' ]; then echo 'The stack is still running: make e2e-down removes it.'; fi

e2e-down:
	@$(COMPOSE) down -v
