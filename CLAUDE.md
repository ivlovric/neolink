# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Neolink is a Rust proxy that bridges Reolink IP cameras (using the proprietary "Baichuan" protocol on port 9000) to standard RTSP clients. It enables NVR software like Shinobi or Blue Iris to receive video from cameras that don't natively support ONVIF or RTSP.

Key features:
- RTSP proxy server for Reolink cameras
- MQTT integration for camera control and status updates
- Motion detection and stream pausing
- Battery-powered camera support with idle disconnect
- PTZ, LED, IR, floodlight, PIR, and siren control
- Multiple camera discovery methods (local, remote, map, relay)

## Build Commands

### Prerequisites
- Rust 1.82.0 (as specified in CI)
- GStreamer 1.20+ with development libraries
- OpenSSL development libraries

### Building
```bash
# Standard build
cargo build

# Release build
cargo build --release

# Build without default features (no GStreamer)
cargo build --no-default-features

# Build with specific features
cargo build --no-default-features --features=gstreamer
cargo build --no-default-features --features=pushnoti
```

### Testing
```bash
# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p neolink_core

# Run a specific test
cargo test test_name
```

### Linting and Formatting
```bash
# Run clippy with all features
cargo +nightly clippy --workspace --all-targets --all-features

# Run clippy with no features
cargo +nightly clippy --workspace --all-targets --no-default-features

# Format code
cargo +nightly fmt --all

# Check formatting without modifying
cargo +nightly fmt --all -- --check
```

### Running
```bash
# RTSP mode (default)
cargo run -- rtsp --config=neolink.toml

# MQTT mode
cargo run -- mqtt --config=neolink.toml

# Combined MQTT + RTSP
cargo run -- mqtt-rtsp --config=neolink.toml

# Other subcommands
cargo run -- battery --config=neolink.toml CameraName
cargo run -- image --config=neolink.toml --file-path=image.jpg CameraName
cargo run -- ptz --config=neolink.toml CameraName control 32 left
cargo run -- reboot --config=neolink.toml CameraName
cargo run -- status-light --config=neolink.toml CameraName on
```

## Architecture

### Project Structure

The project uses a Cargo workspace with the main binary and several crates:

- **Main binary (`src/`)**: CLI interface and high-level orchestration
  - `main.rs`: Entry point, command dispatch
  - `config.rs`: Configuration parsing and validation
  - `common/`: Shared components (reactor, camera threads, instances)
  - Subcommand modules: `rtsp/`, `mqtt/`, `battery/`, `image/`, `pir/`, `ptz/`, `reboot/`, `statusled/`, `talk/`, `users/`

- **`crates/core`**: Core Baichuan protocol implementation
  - `bc/`: High-level camera interface (BcCamera, streams, media)
  - `bc_protocol/`: Low-level protocol messages (login, stream, PTZ, battery, etc.)
  - `bcmedia/`: Media stream handling
  - `bcudp/`: UDP-based discovery and communication

- **`crates/decoder`**: Media decoding utilities
- **`crates/mailnoti`**: Email notification support
- **`crates/pushnoti`**: Push notification handling (deprecated due to Google API removal)

### Key Architectural Components

**NeoReactor (`common/reactor.rs`)**
- Central orchestrator that manages camera instances
- Created from config and shared across all subcommands
- Handles camera lifecycle and coordination

**NeoCam (`common/neocam.rs`)**
- Wrapper around BcCamera from core crate
- Provides higher-level camera operations
- Manages camera connection state

**BcCamera (`crates/core/src/bc/`)**
- Core camera interface to Baichuan protocol
- Handles login, keepalive, streams, and commands
- Abstracts protocol details from application layer

**Camera Discovery (`crates/core/src/bc_protocol/connection/discovery.rs`)**
- Multiple discovery methods: local (UDP broadcast), remote (via Reolink servers), map (camera-initiated), relay (proxied through Reolink)
- Controlled by config `discovery` setting

**Stream Management**
- Cameras typically support mainStream and subStream
- GStreamer pipelines handle media processing and RTSP serving
- Optional stream pausing based on client connections and motion

**MQTT Integration (`src/mqtt/`)**
- Control topics: `/control/led`, `/control/ir`, `/control/ptz`, `/control/floodlight`, etc.
- Status topics: `/status/battery_level`, `/status/motion`, `/status/preview`, etc.
- Query topics: `/query/battery`, `/query/pir`, `/query/ptz/preset`
- MQTT Discovery support for Home Assistant integration

### Feature Flags

- **`gstreamer`** (default): Enables RTSP server, image capture, and talk functionality
- **`pushnoti`**: Enables push notification support (deprecated)

### Configuration

Configuration is TOML-based (see `sample_config.toml`). Key sections:
- Global: `bind`, `bind_port`, `certificate` (for TLS), `[[users]]`
- `[mqtt]`: MQTT broker settings
- `[[cameras]]`: Per-camera settings (name, username, password, uid/address, discovery, stream, pause, idle_disconnect, mqtt settings)

### Connection and Authentication

1. Camera discovery (based on `discovery` setting)
2. TCP connection establishment
3. Login with MD5-based authentication
4. Keep-alive maintenance
5. Stream negotiation and media transport

### Testing Strategy

- Unit tests within each module using `#[cfg(test)]`
- Integration tests in `crates/core/src/bc_protocol/` for protocol handling
- CI runs tests across Linux, Windows, and macOS

## Development Notes

- Use `jemalloc` allocator on non-MSVC targets for better performance
- Async runtime: Tokio with multi-threaded executor
- Logging: `env_logger` with RUST_LOG environment variable
- XML handling: `quick-xml` with serde for protocol messages
- Encryption: AES-CFB mode for protocol encryption
- Media: FFmpeg for transcoding, MediaMTX for RTSP server (migrated from GStreamer 2025-10-31)

## RTSP Architecture (FFmpeg + MediaMTX - Current)

As of 2025-10-31, Neolink has migrated from GStreamer to FFmpeg + MediaMTX for RTSP streaming.

### New Architecture

**Data Flow:**
```
Camera → BcMedia frames → FFmpeg stdin → FFmpeg process → RTSP push to MediaMTX → Clients
```

**Key Components:**
- **MediaMTX**: External RTSP server (handles client connections, authentication, serving)
- **FFmpeg**: Subprocess for transcoding/remuxing camera streams
- **Neolink**: Bridges camera protocol to FFmpeg, manages FFmpeg processes

### Key Files

- `src/rtsp/ffmpeg.rs` - FFmpeg process wrapper (~300 lines)
  - Spawns FFmpeg with stdin pipe
  - Manages process lifecycle (spawn, monitor, restart, kill)
  - Handles H.264/H.265 passthrough and transcoding

- `src/rtsp/mediamtx.rs` - MediaMTX HTTP API client (~200 lines)
  - Health checking via MediaMTX API
  - Stream path validation
  - RTSP URL generation for publishing

- `src/rtsp/stream.rs` - Stream coordination (~148 lines)
  - Detects stream codec (H.264 vs H.265)
  - Configures transcode mode
  - Pipes BcMedia frames to FFmpeg

- `src/rtsp/mod.rs` - RTSP service entry point (~210 lines, simplified from 453)
  - MediaMTX health check on startup
  - Camera lifecycle management
  - FFmpeg process coordination

### Configuration

New config fields (all optional with defaults):
```toml
# MediaMTX HTTP API URL
mediamtx_api_url = "http://localhost:9997"  # default

# MediaMTX RTSP URL for publishing
mediamtx_rtsp_url = "rtsp://localhost:8554"  # default

# FFmpeg binary path
ffmpeg_path = "ffmpeg"  # default: use PATH
```

### Benefits vs GStreamer

1. **Simplicity**: -2850 lines of code (-81%)
   - No complex pipeline management
   - No buffer pools or state machines
   - No VAAPI context management

2. **Stability**: Process isolation
   - Clean kill semantics (SIGTERM works)
   - No thread leaks
   - FFmpeg handles codec complexities

3. **Observability**: External processes
   - Monitor: `ps aux | grep ffmpeg`
   - Debug: `strace -p <pid>`
   - MediaMTX API: `curl http://localhost:9997/v3/paths/list`

4. **Maintainability**: Simpler architecture
   - FFmpeg CLI is stable and well-documented
   - MediaMTX handles RTSP serving
   - Easier to debug and modify

### Prerequisites

- **FFmpeg**: Install via package manager (`apt install ffmpeg`, `brew install ffmpeg`, etc.)
- **MediaMTX**: Download from https://github.com/bluenviron/mediamtx/releases
  - Start before Neolink: `./mediamtx &`
  - Or use Docker: `docker run -p 8554:8554 -p 9997:9997 bluenviron/mediamtx`

### Common FFmpeg Issues

**Issue:** FFmpeg not found
```
Failed to spawn FFmpeg: No such file or directory
```
**Solution:** Install FFmpeg or set `ffmpeg_path` in config

**Issue:** MediaMTX not running
```
MediaMTX health check failed: Connection refused
```
**Solution:** Start MediaMTX: `./mediamtx &`

**Issue:** Stream not appearing
**Solution:**
1. Check Neolink logs: `journalctl -u neolink -f`
2. Verify FFmpeg running: `ps aux | grep ffmpeg`
3. Check MediaMTX paths: `curl http://localhost:9997/v3/paths/list`

See `FFMPEG_MIGRATION_GUIDE.md` for complete migration documentation.

### Legacy GStreamer Support

The old GStreamer code is kept behind the `gstreamer` feature flag for backwards compatibility:
- `src/rtsp/factory.rs` (2043 lines) - GStreamer pipeline factory
- `src/rtsp/gst.rs` - GStreamer RTSP server wrapper

To use legacy GStreamer:
```bash
cargo build --features=gstreamer
```

**Note:** GStreamer support will be removed in a future release once FFmpeg is proven stable.

## Common Patterns

- Camera commands follow a request-response pattern with XML payloads
- Most operations are async (tokio)
- Error handling uses `anyhow::Result` at application level, `thiserror` at library level
- Config validation uses the `validator` crate
- Channel-based communication between threads (crossbeam-channel, tokio channels)

## RTSP Stability - GStreamer (Historical / Legacy)

**Note:** This section documents the old GStreamer implementation (pre-2025-10-31). For the current FFmpeg-based implementation, see "RTSP Architecture (FFmpeg + MediaMTX - Current)" above.

The GStreamer RTSP implementation underwent significant stability improvements in 2025-10-29 to address resource leaks, deadlocks, and panics:

### Critical Fixes Applied

1. **Error Handling:** All `.unwrap()` calls in hot paths replaced with proper error handling
   - Buffer pool operations now gracefully handle failures
   - State transitions log warnings instead of panicking
   - Full error context in all failure paths

2. **Threading Model:** Replaced detached threads with managed tokio tasks
   - `std::thread::spawn()` → `tokio::task::spawn_blocking()`
   - Proper cleanup on camera disconnect
   - Error propagation from streaming tasks

3. **Deadlock Prevention:** Blocking operations replaced with timeouts
   - `blocking_send` → `try_send` with retry loop (5s timeout)
   - `blocking_recv` → `blocking_recv_timeout` (10s timeout)
   - Prevents GStreamer thread pool starvation

4. **Backpressure Handling:** Increased channel buffers
   - Media channels: 100 → 500 frames (~4s → ~20s at 25fps)
   - Client channels: 100 → 200 connections
   - Better handling of temporary slowdowns

### Testing Framework

**Unit Tests:** `src/rtsp/factory.rs::tests`
- Buffer pool creation and lifecycle
- Configuration updates and validation
- Factory creation scenarios

**Integration Tests:** `tests/rtsp_stability.rs`
- Camera disconnect/reconnect scenarios
- Multiple concurrent clients
- Resource cleanup verification
- Stress tests (run with `--ignored`)

**Stress Tests (Manual):**
```bash
# 24-hour stability test
cargo test --test rtsp_stability stress_test_24_hour_streaming -- --ignored --nocapture

# High concurrent load test (50-100 clients)
cargo test --test rtsp_stability stress_test_high_concurrent_load -- --ignored --nocapture
```

### Key Files for RTSP Work

- `src/rtsp/factory.rs` - Pipeline factory, buffer management (lines 302-482 are critical)
- `src/rtsp/mod.rs` - Camera lifecycle, connection management (lines 153-195)
- `src/rtsp/stream.rs` - Stream coordination between camera and factory
- `src/common/instance/gst.rs` - Media channel setup (lines 21, 183)

### Common RTSP Issues and Solutions

**Issue:** Streaming stops after a few seconds
- Check GStreamer pipeline state transitions
- Verify buffer pool is not exhausted
- Check logs for state change failures

**Issue:** Resource leaks / growing memory
- Verify `spawn_blocking` tasks complete when camera disconnects
- Check buffer pool cleanup in HashMap
- Monitor with: `ps aux | grep neolink` and `htop`

**Issue:** Deadlocks / hangs
- Check for blocking operations in async contexts
- Verify all timeouts are configured
- Review factory callback execution time

**Issue:** Panics in streaming
- Check for any remaining `.unwrap()` calls
- Verify error handling in `send_to_appsrc`
- Review GStreamer element creation

See `RTSP_STABILITY_FIXES.md` for detailed fix documentation.

## Push Notifications and SSL Certificate Issues

### Known Issue: SSL Certificate Verification

Push notifications require connecting to Google's FCM (Firebase Cloud Messaging) service. In some environments, particularly Docker containers or minimal Linux installations, SSL certificate verification may fail with errors like:

```
unable to get local issuer certificate
certificate verify failed
```

**Why This Happens:**
- OpenSSL cannot find the system's CA certificate store
- The certificate store path (`/usr/lib/ssl/certs`) doesn't exist or is misconfigured
- The `fcm-push-listener` crate uses the system's native TLS implementation

**Impact:**
- Push notifications will be unavailable
- **Core camera functionality (RTSP streaming, MQTT) is NOT affected**
- The system will log an INFO message and continue operating normally

**Solutions:**

1. **Install CA Certificates (Recommended):**
   ```bash
   # Debian/Ubuntu
   sudo apt-get install ca-certificates
   sudo update-ca-certificates

   # Alpine Linux
   apk add ca-certificates

   # Fedora/RHEL
   sudo yum install ca-certificates
   ```

2. **Configure OpenSSL Certificate Path:**
   ```bash
   export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
   export SSL_CERT_DIR=/etc/ssl/certs
   ```

3. **Disable Push Notifications:**
   Push notifications are optional. If you don't need them, the warnings can be safely ignored. Camera streaming and MQTT will work normally.

**Related Files:**
- `src/common/pushnoti.rs` - Push notification implementation
- Lines 133-156: Registration error handling
- Lines 227-240: Connection error handling
