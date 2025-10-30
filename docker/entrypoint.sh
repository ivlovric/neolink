#!/bin/sh

# Allows Ctrl-C, by letting this sh process act as PID 1
exit_func() {
    exit 1
}
trap exit_func TERM INT

ulimit -n 65535

# Set up XDG_RUNTIME_DIR for GStreamer
# GStreamer needs this directory for runtime files (sockets, temporary data, etc.)
if [ -z "$XDG_RUNTIME_DIR" ]; then
    export XDG_RUNTIME_DIR=/tmp/runtime-root
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 0700 "$XDG_RUNTIME_DIR"
fi

echo "Running: ${*}"
"$@"
