#!/bin/sh
set -eu
test "$(sh calc.sh 2 3)" = 5
test "$(sh calc.sh 7 1)" = 8
