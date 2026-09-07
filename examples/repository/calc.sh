#!/bin/sh
# Deliberately incorrect: the qualification task fixes subtraction to addition.
printf '%s\n' "$(( $1 - $2 ))"
