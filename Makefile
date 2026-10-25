.DEFAULT_GOAL := help

CARGO := cargo

##@ Build

.PHONY: bundle-desktop
bundle-desktop: ## Bundle the desktop app with alchemyd and alchemy-helper inside
	scripts/bundle-desktop.sh

.PHONY: clean
clean: clean-rs clean-worktrees ## Clean up all build artifacts and worktrees

.PHONY: clean-rs
clean-rs: ## Clean up Rust build artifacts
	$(CARGO) clean

.PHONY: clean-worktrees
clean-worktrees: ## Remove linked Git worktrees (skips dirty or locked ones)
	@git worktree list --porcelain \
		| awk 'NR > 1 && /^worktree / { sub(/^worktree /, ""); print }' \
		| while IFS= read -r wt; do git worktree remove "$$wt" && echo "removed $$wt" || true; done
	@git worktree prune
	@git for-each-ref --format='%(refname:short)' 'refs/heads/worktree-agent-*' \
		| while IFS= read -r br; do git branch -d "$$br" 2>/dev/null || echo "kept $$br (unmerged or checked out)"; done

.PHONY: help
help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"} /^##@/ {printf "\n\033[1m%s\033[0m\n", substr($$0, 5)} /^[a-zA-Z0-9_-]+:.*##/ {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
