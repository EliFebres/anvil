#!/usr/bin/env bash
# No quoted `list_dir` is left; the tool, the factory profile and the LS
# mapping use the new name; anvil-core and anvil-compat pass their tests.
set -eu

if grep -rn --include=*.rs --include=*.snap --include=*.md --include=*.toml '"list_dir"' crates deploy; then
  echo "\"list_dir\" is still referenced"
  exit 1
fi
grep -q '"list_directory"' crates/anvil-core/src/tools/list_dir.rs
grep -q '"list_directory"' deploy/nuc/anvil-factory.toml
grep -q '("LS", "list_directory")' crates/anvil-compat/src/names.rs
cargo test -q -p anvil-core -p anvil-compat
