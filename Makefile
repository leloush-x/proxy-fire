.PHONY: all build build-release test lint fmt vet clean size help

CARGO ?= cargo
BINDIR = bin

all: build

build: ## Build the all-in-one binary (debug)
	@mkdir -p $(BINDIR)
	$(CARGO) build --workspace
	cp target/debug/proxy-fire $(BINDIR)/
	@echo "Built: $(BINDIR)/proxy-fire"

build-release: ## Build the all-in-one binary (release, thin LTO, stripped)
	@mkdir -p $(BINDIR)
	$(CARGO) build --release --locked
	cp target/release/proxy-fire $(BINDIR)/
	@echo "Built: $(BINDIR)/proxy-fire"

test: ## Run unit + integration tests
	$(CARGO) test --workspace

vet: ## Run clippy
	$(CARGO) clippy --workspace --all-targets -- -D warnings

lint: vet

fmt: ## Format the code
	$(CARGO) fmt --all

fmt-check: ## Check formatting (CI)
	$(CARGO) fmt --all -- --check

clean: ## Remove build artifacts
	rm -rf $(BINDIR) target

size: build-release ## Show binary size
	@ls -lh $(BINDIR)/proxy-fire

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'
