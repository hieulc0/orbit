#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
scope=${1:-all}
case "$scope" in all|rust|python|ui|docs) ;; *) echo 'usage: bash scripts/check.sh [all|rust|python|ui|docs]' >&2; exit 2;; esac
if [[ $scope == all || $scope == rust ]]; then
  cargo fmt --all -- --check
  cargo test --locked
  cargo clippy --locked --all-targets --all-features -- -D warnings
fi
if [[ $scope == all || $scope == python ]]; then
  python3 -m unittest discover -s sdk/python -p 'test_*.py'
  python3 -m unittest discover -s scripts/tests -p 'test_*.py'
fi
if [[ $scope == all || $scope == ui ]]; then
  npm --prefix ui run build
  if [[ $scope == ui ]]; then
    export PLAYWRIGHT_BROWSERS_PATH=${PLAYWRIGHT_BROWSERS_PATH:-"$PWD/target/playwright"}
    npm --prefix ui test
  fi
fi
if [[ $scope == all || $scope == docs ]]; then
  node scripts/check-docs.mjs
fi
git diff --check
