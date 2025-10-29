# RTSP Stability Fixes

This document summarizes the critical stability fixes applied to the RTSP implementation.

## Date: 2025-10-29

## Issues Fixed

### 1. Critical: Unwrap/Panic in Hot Path ✅

**Files:** `src/rtsp/factory.rs`

**Problem:** Every video frame triggered buffer pool operations using `.unwrap()` that would panic on any GStreamer failure, crashing the entire streaming thread.

**Solution:**
- Created `create_buffer_pool()` helper function with proper error handling
- Replaced all `.unwrap()` calls with `?` operator and contextual error messages
- Added graceful error handling for:
  - Buffer pool configuration (`set_config`)
  - Buffer pool activation (`set_active`)
  - Buffer acquisition (`acquire_buffer`)
  - Buffer mapping (`get_mut`, `map_writable`)
  - State transitions (`set_state`)

**Lines Modified:** 302-314, 387-418, 455-482

---

### 2. Critical: Blocking Operations on Async Boundaries ✅

**Files:** `src/rtsp/factory.rs`

**Problem:** The GStreamer factory callback used `blocking_send()` and `blocking_recv()` which could deadlock the RTSP server thread pool.

**Solution:**
- Replaced `blocking_send` with `try_send` in a retry loop (max 5 seconds)
- Replaced `blocking_recv` with `blocking_recv_timeout` (10 second timeout)
- Added proper error handling for channel full and closed states
- Prevents indefinite hangs in GStreamer's thread pool

**Lines Modified:** 290-331

---

### 3. Critical: Camera Disconnect Race Condition ✅

**Files:** `src/rtsp/factory.rs`

**Problem:** Streaming threads spawned via `std::thread::spawn()` continued running after camera disconnected, causing orphaned threads and resource leaks.

**Solution:**
- Replaced `std::thread::spawn()` with `tokio::task::spawn_blocking()`
- Added `.await` on streaming task handle to ensure cleanup
- Implemented proper error propagation from streaming task
- Streaming automatically stops when `media_rx` channel closes (camera disconnect)

**Lines Modified:** 243-302

---

### 4. High: No Error Propagation ✅

**Files:** `src/rtsp/factory.rs`

**Problem:** Errors in the streaming thread were silently dropped with no visibility to the system.

**Solution:**
- Streaming task now properly returns `AnyResult<()>`
- Errors are awaited and propagated up the call stack
- Added structured logging for all error paths
- Includes camera name and stream type in all error messages

**Lines Modified:** 245-302

---

### 5. High: Buffer State Desynchronization ✅

**Files:** `src/rtsp/factory.rs`

**Problem:** Race conditions in pause/resume logic caused by checking state and setting state separately.

**Solution:**
- Added explicit state variables to reduce race window
- Replaced `.unwrap()` on state changes with error logging
- State change failures now log warnings instead of crashing
- Maintains streaming even if state transitions fail

**Lines Modified:** 455-482

---

### 6. Medium: Insufficient Backpressure ✅

**Files:** `src/rtsp/factory.rs`, `src/common/instance/gst.rs`

**Problem:** Media channel buffer of only 100 frames (4 seconds at 25 fps) caused stalls under load.

**Solution:**
- Increased media channel buffers from 100 to 500 frames
- Provides ~16-20 seconds of buffer at 25-30 fps
- Increased client connection channel from 100 to 200
- Better handles temporary slowdowns and backpressure

**Lines Modified:**
- `src/rtsp/factory.rs:158`
- `src/common/instance/gst.rs:23`
- `src/common/instance/gst.rs:183`

---

## Testing

### Unit Tests Added

**File:** `src/rtsp/factory.rs` (lines 1054-1166)

Tests added:
- `test_create_buffer_pool_success` - Verifies pool creation
- `test_create_buffer_pool_various_sizes` - Tests multiple buffer sizes
- `test_buffer_size_calculation` - Validates bitrate calculations
- `test_audio_type_variants` - Type safety checks
- `test_stream_config_fps_update` - FPS table handling
- `test_stream_config_bitrate_update` - Bitrate table handling
- `test_make_dummy_factory` - Factory creation
- `test_make_dummy_factory_with_splash` - Splash screen factory

### Integration Tests Added

**File:** `tests/rtsp_stability.rs`

Test framework for:
- Graceful shutdown on camera disconnect
- Multiple concurrent client connections
- Channel overflow handling
- Rapid reconnect cycles
- Buffer pool reuse and memory leaks
- GStreamer state transition errors
- Client timeout cleanup
- Config changes during streaming
- 24-hour stress testing (ignored by default)
- High concurrent load testing (ignored by default)

---

## Impact Assessment

### Before Fixes
- **Panics:** Any GStreamer error would crash the streaming thread
- **Deadlocks:** Possible under high load or client disconnections
- **Resource Leaks:** Orphaned threads accumulating over time
- **Silent Failures:** Errors dropped without visibility
- **Poor Backpressure:** 4-second buffer caused frequent stalls

### After Fixes
- **Graceful Errors:** All failures logged and handled appropriately
- **No Deadlocks:** Timeouts prevent indefinite hangs
- **Clean Shutdown:** All resources properly cleaned up
- **Full Visibility:** Comprehensive error logging with context
- **Better Performance:** 20-second buffer handles temporary slowdowns

---

## Verification Steps

To verify these fixes:

1. **Run Unit Tests:**
   ```bash
   cargo test --lib rtsp::factory::tests
   ```

2. **Run Integration Tests:**
   ```bash
   cargo test --test rtsp_stability
   ```

3. **Run Stress Tests (Manual):**
   ```bash
   cargo test --test rtsp_stability -- --ignored --nocapture
   ```

4. **Manual Testing:**
   - Connect multiple RTSP clients simultaneously
   - Disconnect camera while streaming
   - Rapidly reconnect camera multiple times
   - Monitor logs for errors
   - Check for orphaned threads: `ps aux | grep neolink`
   - Monitor memory usage over 24 hours

---

## Breaking Changes

**None** - All changes are internal implementation improvements that maintain the existing API.

---

## Future Improvements

Potential additional enhancements:

1. **Metrics/Telemetry:**
   - Track active streaming threads
   - Monitor buffer pool memory usage
   - Export metrics to Prometheus/StatsD

2. **Advanced Backpressure:**
   - Dynamic buffer sizing based on frame rate
   - Adaptive quality reduction under load

3. **Health Checks:**
   - Endpoint to verify streaming health
   - Detect and report degraded states

4. **Resource Limits:**
   - Maximum concurrent clients per camera
   - Maximum buffer pool size limits
   - Automatic cleanup of stale resources

---

## Related Files

- `src/rtsp/factory.rs` - Main factory and streaming logic
- `src/rtsp/mod.rs` - Camera lifecycle management
- `src/rtsp/stream.rs` - Stream coordination
- `src/common/instance/gst.rs` - Media channel management
- `tests/rtsp_stability.rs` - Integration tests
- `CLAUDE.md` - Project documentation
