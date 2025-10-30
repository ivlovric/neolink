# VAAPI Resource Leak Fixes

## Summary

Fixed critical resource leaks in the RTSP/VAAPI transcoding pipeline that caused "could not create element" errors after container uptime. The issue manifested as VAAPI element creation failures that required container restarts to resolve.

## Root Cause

GStreamer pipeline elements (particularly VAAPI encoders/decoders) were not being properly transitioned to NULL state when streams ended or clients disconnected. This caused:
- GPU memory pool accumulation
- VAAPI device handle leaks (/dev/dri/ file descriptors)
- Eventually exhausting VAAPI driver resources
- "could not create element" errors from GStreamer

## Fixes Applied

### 1. Pipeline Cleanup on Stream End (CRITICAL)
**File:** `src/rtsp/factory.rs` (lines 288-361)

**Change:** Added explicit cleanup of appsrc elements when streaming tasks complete:
- Created `cleanup_sources()` helper function within spawn_blocking task
- Sends EOS (end-of-stream) signal to appsrc elements
- Sets video and audio appsrc elements to NULL state
- Cleanup runs in BOTH success and error paths
- Happens before spawn_blocking task returns

**Impact:** Prevents accumulation of VAAPI resources from orphaned appsrc elements

### 2. Bin Cleanup After Streaming (CRITICAL)
**File:** `src/rtsp/factory.rs` (lines 257-407)

**Change:** Added pipeline bin cleanup after streaming task completes:
- Clone element bin before sending to RTSP server (line 259)
- After stream_handle completes, set bin to NULL state (lines 387-406)
- Wait for state change to complete (1000ms timeout)
- Comprehensive error logging if cleanup fails

**Impact:** Ensures entire GStreamer bin and all contained VAAPI elements release resources

### 3. VAAPI Test Pipeline Cleanup (HIGH)
**File:** `src/rtsp/factory.rs` (lines 724-809)

**Change:** Improved test pipeline cleanup in `test_specific_vaapi_encoder()`:
- Created `cleanup_pipeline()` helper function
- Properly checks result of `set_state(NULL)` instead of ignoring with `let _`
- Waits for NULL state to be reached (500ms timeout)
- Cleanup called in ALL code paths (success and all error paths)
- Logs warnings if cleanup fails

**Impact:** Prevents VAAPI resource leaks from encoder availability tests

### 4. Consolidated VAAPI Static Cache (MEDIUM)
**File:** `src/rtsp/factory.rs` (lines 1006-1016)

**Change:** Consolidated duplicate VAAPI caching statics:
- Created single `is_vaapi_available()` function using `std::sync::OnceLock`
- Replaced two separate unsafe static mut implementations
- Thread-safe without unsafe code
- Prevents duplicate VAAPI testing

**Impact:** Code quality improvement, prevents potential race conditions

### 5. Diagnostic Logging (LOW)
**File:** `src/rtsp/factory.rs` (multiple locations)

**Change:** Added comprehensive pipeline lifecycle logging:
- Line 170: "Pipeline lifecycle: CREATED"
- Line 290: "Cleaning up pipeline sources"
- Line 387: "Pipeline lifecycle: ENDING"
- Line 395: "Pipeline lifecycle: DESTROYED - all resources released"
- Line 734: Test pipeline cleanup logging

**Impact:** Enables monitoring of pipeline lifecycle and early detection of leaks

## Expected Behavior After Fixes

### Before Fixes:
```
(neolink:8): GStreamer-RTSP-Server-CRITICAL **: could not create element
(repeating every 10-15 seconds after container uptime)
```

### After Fixes:
```
INFO New RTSP client connection for CameraName::MainStream - creating pipeline
DEBUG CameraName::MainStream: Pipeline lifecycle: CREATED
DEBUG CameraName::MainStream: Cleaning up pipeline sources
TRACE CameraName::MainStream: Video appsrc cleaned up
TRACE CameraName::MainStream: Audio appsrc cleaned up
DEBUG CameraName::MainStream: Pipeline lifecycle: ENDING - cleaning up resources
INFO CameraName::MainStream: Pipeline lifecycle: DESTROYED - all resources released
```

## Testing Recommendations

### 1. Verify No Resource Leaks

Monitor file descriptors over time:
```bash
watch -n 5 'lsof -p $(pgrep neolink) | grep /dev/dri | wc -l'
```

Expected: Count should remain stable or decrease over time, not grow continuously

### 2. Monitor Memory Usage

Check GPU memory (if available):
```bash
watch -n 5 'cat /sys/kernel/debug/dri/0/i915_gem_objects'
```

Expected: GPU memory should not grow unbounded

### 3. Stress Test

Connect and disconnect multiple RTSP clients:
```bash
for i in {1..100}; do
  ffplay rtsp://localhost:8554/camera/mainStream &
  sleep 2
  pkill ffplay
  sleep 1
done
```

Expected: No "could not create element" errors, container continues functioning

### 4. Long-Running Test

Run container for 24+ hours with periodic client connections.

Expected: No need to restart container, VAAPI elements continue to create successfully

## Verification in Logs

To verify the fixes are working, look for these log patterns:

**Good Signs:**
- "Pipeline lifecycle: DESTROYED - all resources released" after each stream ends
- "Video appsrc cleaned up" and "Audio appsrc cleaned up" messages
- No "could not create element" errors
- Stable file descriptor count for /dev/dri/

**Bad Signs (indicate issues remain):**
- "Pipeline lifecycle: CLEANUP_FAILED" or "CLEANUP_INCOMPLETE"
- "could not create element" errors returning
- Growing /dev/dri/ file descriptor count

## Docker Configuration

Your current Docker configuration is good:
```bash
docker run -d --privileged \
  --name neolink --cap-add SYS_ADMIN \
  --group-add 993 --group-add video \
  -e XDG_RUNTIME_DIR=/tmp \
  --network host \
  --device /dev/dri:/dev/dri \
  -v /configs/neolink/neolink.toml:/etc/neolink.toml \
  --restart unless-stopped \
  neolink:transcode9 \
  /usr/local/bin/neolink mqtt-rtsp --config=/etc/neolink.toml
```

No changes needed to Docker configuration.

## Configuration Recommendation

Consider changing from forced VAAPI to auto mode for robustness:

**Current (in neolink.toml):**
```toml
transcode_device = "vaapi"  # Forced, no fallback
```

**Recommended:**
```toml
transcode_device = "auto"   # VAAPI with software fallback
```

This provides automatic fallback to software encoding if VAAPI has issues, preventing stream failures.

## Technical Details

### GStreamer State Transitions

GStreamer elements have states: NULL → READY → PAUSED → PLAYING

**Critical for resource cleanup:**
- PLAYING/PAUSED → NULL transition releases all resources
- Without explicit NULL transition, resources may persist until process dies
- VAAPI elements hold GPU memory, device handles, and driver sessions

**The fixes ensure:**
1. appsrc elements are set to NULL when stream ends
2. Entire bin is set to NULL after streaming task completes
3. Test pipelines are set to NULL after VAAPI testing
4. State transitions are waited on (not fire-and-forget)

### VAAPI Resource Types

VAAPI elements can hold:
- **VA Surface Pools:** GPU memory for video frames
- **VA Context:** Encoder/decoder session
- **DRM Device Handles:** File descriptors to /dev/dri/renderD128
- **VA Display:** Connection to VAAPI driver

All of these are leaked if elements aren't properly cleaned up.

## Files Modified

- `src/rtsp/factory.rs` - All resource leak fixes and diagnostic logging

## Next Steps

1. **Build in Docker:** Rebuild the Docker image with these fixes
2. **Deploy:** Replace the running container
3. **Monitor:** Watch logs for "Pipeline lifecycle" messages
4. **Verify:** Check that /dev/dri/ FD count stays stable
5. **Test:** Run stress tests with multiple client connections
6. **Report:** Confirm "could not create element" errors are gone

## Support

If issues persist after these fixes:
1. Check logs for "CLEANUP_FAILED" or "CLEANUP_INCOMPLETE" messages
2. Verify VAAPI test passes: Look for "VAAPI H.264 encoder is available and functional"
3. Monitor /dev/dri/ permissions: `ls -l /dev/dri/`
4. Check GStreamer plugin availability: `gst-inspect-1.0 vaapih264enc`

---

**Date:** 2025-10-30
**Branch:** master
**Commits:** To be created after testing
