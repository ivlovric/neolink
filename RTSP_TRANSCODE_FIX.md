# RTSP Transcoding Stability Fix - VAAPI Hardware Encoding

## Issue
RTSP transcoding from H.265 to H.264 was failing with "App source is closed" error immediately after VAAPI encoder initialization. The error message indicated:

```
[INFO] Using VAAPI hardware encoder for transcoding
[ERROR] check_live: App source has no bus (element may have been destroyed or removed from bin)
[ERROR] check_live: App source state: Null
[ERROR] This often indicates a GStreamer element initialization failure
[INFO] Failed to send to source: App source is closed
[ERROR] Streaming error: App source is closed
```

## Root Cause
The GStreamer pipeline was being constructed successfully, but VAAPI encoder initialization was failing **silently during state transitions**. By the time we checked the appsrc (500ms later), GStreamer had already torn down the pipeline. The issues were:

1. **Silent VAAPI failures**: VAAPI encoder failed during pipeline state change to PLAYING but errors weren't captured
2. **No pipeline validation**: We only checked if appsrc existed, not if the pipeline could actually initialize
3. **No fallback mechanism**: Once VAAPI was selected, there was no way to fall back to software encoding
4. **Poor error reporting**: Generic "app source closed" with no indication of WHY it closed

## Changes Made (factory.rs)

### 1. GStreamer Bus Message Monitoring (Lines 596-662)
Added `test_pipeline_initialization()` function that:
- Monitors GStreamer bus for ERROR and WARNING messages during initialization
- Returns detailed error context (element name, error message, debug info)
- Detects VAAPI-specific warnings
- Provides timeout-based monitoring (default 300ms)

**Why**: Captures the actual reason for pipeline failures instead of discovering them after the fact.

### 2. VAAPI Capability Testing (Lines 664-734)
Added `test_vaapi_encoder()` function that:
- Creates a simple test pipeline: `videotestsrc → videoconvert → vaapih264enc → fakesink`
- Tests state transitions (NULL → READY → PAUSED)
- Monitors bus for initialization errors
- Returns boolean indicating if VAAPI is functional
- Runs only once per application lifetime (cached with `std::sync::Once`)

**Why**: Tests VAAPI before committing to using it, avoiding failures during actual streaming.

### 3. Automatic Fallback System (Lines 902-953)
Refactored transcoding pipeline into:
- `build_h265_to_h264_transcode()`: Top-level function that manages encoder selection
- `build_h265_to_h264_transcode_with_encoder()`: Builds pipeline with specific encoder type
- Enum `H264EncoderType` to specify VAAPI or Software encoding

**Fallback logic**:
1. Test VAAPI availability on first use (cached)
2. If VAAPI available, try building pipeline with VAAPI
3. If VAAPI pipeline fails, automatically rebuild with x264enc
4. If software encoding fails, return detailed error for both attempts

**Why**: Ensures video will stream even if hardware encoding fails, with clear logging at each step.

### 4. Enhanced Pipeline Validation (Lines 1116-1122)
Added call to `test_pipeline_initialization()` after linking:
- Monitors bus for 300ms after pipeline construction
- Catches initialization errors before returning the appsrc
- Returns detailed error instead of generic "app source closed"

**Why**: Validates that the encoder actually initialized before marking the pipeline as ready.

### 5. Improved Logging Throughout
- Added encoder type to all log messages
- Log parameter values when configuring encoders
- Clear distinction between "trying VAAPI" and "using VAAPI successfully"
- Detailed error messages with troubleshooting hints

**Why**: Makes it clear what encoder is being used and where failures occur.

## Testing

### Test 1: VAAPI Hardware Encoding (Success Case)
```bash
# Run in Docker container with GPU access
docker run -d \
  --name neolink \
  --privileged \
  --device /dev/dri:/dev/dri \
  -p 8554:8554 \
  -v $PWD/neolink.toml:/etc/neolink.toml \
  ivlovric/neolink

# Expected logs:
[DEBUG] Testing VAAPI H.264 encoder availability...
[INFO] VAAPI H.264 encoder is available and functional
[INFO] Building H.265 to H.264 transcoding pipeline
[INFO] Attempting to build pipeline with VAAPI hardware encoder
[DEBUG] Building H.265 to H.264 transcoding pipeline with VAAPI encoder
[DEBUG] Creating VAAPI hardware encoder
[DEBUG] VAAPI encoder configured: bitrate=2000 kbps, keyframe-period=50
[DEBUG] Testing pipeline initialization with VAAPI encoder...
[DEBUG] Pipeline initialization monitoring completed - no errors detected
[INFO] Transcoding pipeline built successfully with VAAPI encoder
```

### Test 2: VAAPI Fallback to Software (Failure → Success Case)
```bash
# If VAAPI initialization fails during streaming:

[INFO] Attempting to build pipeline with VAAPI hardware encoder
[DEBUG] Creating VAAPI hardware encoder
[DEBUG] Testing pipeline initialization with VAAPI encoder...
[ERROR] GStreamer ERROR from vaapiencodeh264-0: Failed to initialize encoder
[ERROR] Debug info: gstvaapiencoder.c(123): Failed to create encoder context
[ERROR] Pipeline initialization test failed: Pipeline initialization failed...
[ERROR] VAAPI transcoding pipeline failed to initialize: Pipeline failed initialization test
[INFO] Falling back to software encoder (x264enc)
[DEBUG] Building H.265 to H.264 transcoding pipeline with Software encoder
[DEBUG] Creating x264enc software encoder
[DEBUG] x264enc configured: bitrate=2000 kbps, key-int-max=50
[DEBUG] Testing pipeline initialization with Software encoder...
[DEBUG] Pipeline initialization monitoring completed - no errors detected
[INFO] Successfully fell back to software encoding
[INFO] Transcoding pipeline built successfully with Software encoder
```

### Test 3: Complete Failure (Both Encoders Fail)
```bash
# If both encoders fail (rare):

[ERROR] VAAPI transcoding pipeline failed to initialize: ...
[INFO] Falling back to software encoder (x264enc)
[ERROR] Software encoding also failed: ...
[ERROR] Both VAAPI and software transcoding failed.
        VAAPI: Pipeline initialization failed: ...
        Software: Failed to create x264enc encoder: ...
```

## Docker Configuration for Hardware Transcoding

The container needs proper GPU access. Use one of these configurations:

### Option 1: Privileged Mode (Recommended for Testing)
```bash
docker run -d \
  --name neolink \
  --privileged \
  --device /dev/dri:/dev/dri \
  -p 8554:8554 \
  -v $PWD/neolink.toml:/etc/neolink.toml \
  ivlovric/neolink
```

### Option 2: Specific Capabilities (More Secure)
```bash
docker run -d \
  --name neolink \
  --device /dev/dri:/dev/dri \
  --group-add 44 \
  --group-add 993 \
  --cap-add SYS_ADMIN \
  -p 8554:8554 \
  -v $PWD/neolink.toml:/etc/neolink.toml \
  ivlovric/neolink
```

**Note**: Group IDs 44 (video) and 993 (render) are standard on Linux but may vary. Check with:
```bash
ls -la /dev/dri/
```

## Transcode Device Configuration

You can now control which encoder to use for transcoding via the `transcode_device` configuration option in `neolink.toml`:

```toml
[[cameras]]
name = "MyCamera"
transcode_to = "h264"
transcode_device = "auto"  # Options: "auto", "vaapi", "x264", or device path
```

### Configuration Options

| Value | Behavior | Use Case |
|-------|----------|----------|
| `"auto"` (default) | Test VAAPI, use if available, fall back to software | Recommended for most users |
| `"vaapi"` | Force hardware encoding, fail if unavailable | When you know GPU is available and want to ensure it's used |
| `"x264"` or `"software"` | Force software encoding, skip VAAPI test | When VAAPI is buggy or you prefer CPU encoding |
| `"/dev/dri/renderD128"` | Use specific GPU device | Multi-GPU systems or specific device selection |

### Examples

**Auto-detect (recommended):**
```toml
transcode_to = "h264"
transcode_device = "auto"  # Tests VAAPI, falls back to x264 if needed
```

**Force hardware encoding:**
```toml
transcode_to = "h264"
transcode_device = "vaapi"  # Fails if VAAPI unavailable (no fallback)
```

**Force software encoding:**
```toml
transcode_to = "h264"
transcode_device = "x264"  # Always use software, skip VAAPI test
```

**Use specific GPU (multi-GPU systems):**
```toml
transcode_to = "h264"
transcode_device = "/dev/dri/renderD129"  # Second GPU
```

### Logging Output by Mode

**Auto mode (VAAPI works):**
```
[DEBUG] Transcode device config: auto
[DEBUG] Auto-detecting encoder (VAAPI preferred, will fallback to software)
[INFO] VAAPI H.264 encoder is available and functional
[INFO] Attempting to build pipeline with VAAPI encoder (fallback enabled)
[INFO] Transcoding pipeline built successfully with VAAPI encoder
```

**Auto mode (VAAPI fails, fallback to software):**
```
[DEBUG] Transcode device config: auto
[INFO] Attempting to build pipeline with VAAPI encoder (fallback enabled)
[ERROR] VAAPI transcoding pipeline failed to initialize: ...
[INFO] Falling back to software encoder (x264enc)
[INFO] Successfully fell back to software encoding
```

**Force VAAPI mode (fails with no fallback):**
```
[DEBUG] Transcode device config: vaapi
[INFO] Forced to use VAAPI encoder (no fallback to software)
[INFO] Attempting to build pipeline with VAAPI encoder (fallback disabled)
[ERROR] Transcoding pipeline failed: ...
[ERROR] VAAPI was forced (no fallback). Consider using transcode_device = "auto" to enable fallback.
```

**Force software mode:**
```
[DEBUG] Transcode device config: x264
[INFO] Forced to use software encoder (x264enc)
[INFO] Attempting to build pipeline with Software encoder (fallback disabled)
[INFO] Transcoding pipeline built successfully with Software encoder
```

## Expected Behavior After Fix

1. **VAAPI Available & Works**: Stream uses hardware encoding, ~5-10% CPU
2. **VAAPI Available but Fails**: Automatically falls back to x264enc, ~30-50% CPU
3. **VAAPI Not Available**: Uses x264enc from the start
4. **Both Fail**: Clear error message explaining both failure reasons

## Verifying VAAPI in Container

```bash
# Check if VAAPI is detected
docker exec -it neolink vainfo

# Should show:
# vainfo: VA-API version: 1.x.x
# vainfo: Driver version: ...
# VAEntrypointEncSlice for H264

# Test GStreamer VAAPI
docker exec -it neolink gst-launch-1.0 videotestsrc num-buffers=10 ! vaapih264enc ! fakesink

# Should complete without errors
```

## Troubleshooting

### Issue: "amdgpu: os_same_file_description couldn't determine..."
**Solution**: Container needs `--privileged` or `--cap-add SYS_ADMIN` to allow `kcmp()` syscall

### Issue: VAAPI still failing after fix
**Debug steps**:
1. Check logs for specific error message from GStreamer
2. Verify `/dev/dri/renderD128` is accessible in container
3. Test VAAPI standalone: `docker exec -it neolink vainfo`
4. Check if software fallback is working (should see x264enc in logs)

### Issue: Software encoding is slow
**Expected**: x264enc uses 30-50% CPU for 1080p. This is normal.
**Alternatives**:
- Fix VAAPI issues to use hardware encoding
- Reduce bitrate in neolink.toml
- Use subStream instead of mainStream

## Performance Notes

| Encoder | CPU Usage (1080p) | Latency | Quality |
|---------|------------------|---------|---------|
| VAAPI   | 5-10%            | ~50ms   | Good    |
| x264enc (medium) | 30-50%  | ~100ms  | Excellent |
| x264enc (ultrafast) | 15-25% | ~50ms  | Good |

## Files Modified
- `src/rtsp/factory.rs` - All transcoding logic (added ~250 lines)

## Rollback
If these changes cause issues:
```bash
git checkout src/rtsp/factory.rs
cargo build --release
```

## Future Improvements
1. Add configuration option to force software encoding
2. Cache encoder test results to avoid re-testing on container restart
3. Add support for other hardware encoders (NVENC, QSV)
4. Expose encoder statistics (frames encoded, bitrate achieved)
5. Add environment variable to set GST_DEBUG level from container
