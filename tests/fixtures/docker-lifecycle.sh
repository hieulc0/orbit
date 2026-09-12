#!/bin/sh
# Process-lifecycle fixture only; it is not an OCI runtime or storage emulator.
case "$1" in
  run)
    if [ -n "$ORBIT_TEST_CONTAINER_MARKERS" ]; then
      : > "$ORBIT_TEST_CONTAINER_MARKERS/running"
      exec sleep 60
    fi
    for argument in "$@"; do
      case "$argument" in
        type=bind,src=*,dst=/orbit/outputs)
          output=${argument#type=bind,src=}
          output=${output%,dst=/orbit/outputs}
          printf 'fixture result\n' > "$output/result"
          printf 'fixture log\n'
          exit 0
          ;;
      esac
    done
    exit 1
    ;;
  rm)
    if [ -n "$ORBIT_TEST_CONTAINER_MARKERS" ]; then
      : > "$ORBIT_TEST_CONTAINER_MARKERS/removed"
    fi
    exit 0
    ;;
  *) exit 1 ;;
esac
