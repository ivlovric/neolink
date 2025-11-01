#!/bin/sh

# Entrypoint for Neolink with FFmpeg + MediaMTX
# This script starts MediaMTX in the background before starting Neolink

# Allows Ctrl-C, by letting this sh process act as PID 1
cleanup() {
    echo "Shutting down..."
    # Kill MediaMTX if it's running
    if [ -n "$MEDIAMTX_PID" ]; then
        echo "Stopping MediaMTX (PID: $MEDIAMTX_PID)"
        kill -TERM "$MEDIAMTX_PID" 2>/dev/null || true
        wait "$MEDIAMTX_PID" 2>/dev/null || true
    fi
    # Kill Neolink if it's running
    if [ -n "$NEOLINK_PID" ]; then
        echo "Stopping Neolink (PID: $NEOLINK_PID)"
        kill -TERM "$NEOLINK_PID" 2>/dev/null || true
        wait "$NEOLINK_PID" 2>/dev/null || true
    fi
    exit 0
}
trap cleanup TERM INT

ulimit -n 65535

# Start MediaMTX in the background with configuration
echo "Starting MediaMTX RTSP server with config..."
/usr/local/bin/mediamtx /etc/mediamtx/mediamtx.yml &
MEDIAMTX_PID=$!
echo "MediaMTX started (PID: $MEDIAMTX_PID)"

# Wait for MediaMTX to be ready (check API endpoint)
echo "Waiting for MediaMTX to be ready..."
max_wait=30
waited=0
while [ $waited -lt $max_wait ]; do
    if wget -q -O- --timeout=1 http://localhost:9997/v3/config/get >/dev/null 2>&1; then
        echo "MediaMTX is ready!"
        break
    fi
    sleep 1
    waited=$((waited + 1))
done

if [ $waited -ge $max_wait ]; then
    echo "WARNING: MediaMTX did not become ready within ${max_wait}s, proceeding anyway..."
else
    echo "MediaMTX ready after ${waited}s"
fi

# Show MediaMTX info
echo "MediaMTX RTSP URL: rtsp://localhost:8554"
echo "MediaMTX API URL: http://localhost:9997"

# Start Neolink in the foreground
echo "Starting Neolink: ${*}"
"$@" &
NEOLINK_PID=$!
echo "Neolink started (PID: $NEOLINK_PID)"

# Wait for Neolink to exit
wait "$NEOLINK_PID"
NEOLINK_EXIT=$?

echo "Neolink exited with code: $NEOLINK_EXIT"

# Cleanup
cleanup
