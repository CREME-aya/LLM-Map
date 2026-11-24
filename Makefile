SHELL := /usr/bin/env bash
.PHONY: help check-env check-env-json test-scripts verify check-16k

help:
	@echo "SLMorLLM on the map — Integration & Environment Targets"
	@echo "  make check-env          Audit local Gate 0 build environment (human-readable)"
	@echo "  make check-env-json     Audit local Gate 0 build environment (JSON)"
	@echo "  make test-scripts       Run unit tests for integration & verification scripts"
	@echo "  make verify             Run monorepo integration verification suite"
	@echo "  make check-16k TARGET=path  Verify 16 KB ELF LOAD alignment on .so/.apk/.aab/dir"

check-env:
	@bash scripts/check-env.sh

check-env-json:
	@bash scripts/check-env.sh --json

test-scripts:
	@python3 -m unittest discover -s scripts/tests -v

verify:
	@bash scripts/verify-integration.sh

check-16k:
	@test -n "$(TARGET)" || (echo "Usage: make check-16k TARGET=<path-to-so-or-apk-or-dir>" >&2; exit 2)
	@bash scripts/check-16k-elf.sh "$(TARGET)"
