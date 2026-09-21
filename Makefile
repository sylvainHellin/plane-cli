# plane-cli -- the checks a change has to pass before it is committed.
# `make check` is the whole gate; the three targets below are it, one at a time.

.PHONY: check fmt fmt-check clippy test

check: fmt-check clippy test  ## Everything below, in order: formatting, lints, tests

fmt:              ## Reformat the tree in place
	cargo fmt

fmt-check:        ## Fail if anything is unformatted (prints the offending hunks)
	cargo fmt --check

clippy:           ## Lint every target, warnings are failures
	cargo clippy --all-targets -- -D warnings

test:             ## Run the test suite
	cargo test
