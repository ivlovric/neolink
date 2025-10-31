# GStreamer Stability Patches - Implementation Summary

**Date**: 2025-10-31
**Status**: Phase 1 Complete (Band-Aid Fixes)
**Next Step**: Plan FFmpeg Migration

## Overview

This document describes the immediate stability patches applied to address critical GStreamer issues causing unkillable Docker containers, zombie pipelines, and resource exhaustion.

## Critical Issues Addressed

1. **Unkillable Docker Container** - Container wouldn't respond to SIGTERM/SIGKILL
2. **Zombie Pipelines** - Pipeline counter stuck at 2/2, rejecting new connections
3. **Resource Leaks** - Threads and VAAPI contexts not properly released
4. **Indefinite Blocking** - Streaming tasks stuck forever in GStreamer C code

## Changes Implemented

### 1. Streaming Task Timeout (`src/rtsp/factory.rs:458-477`)

**Problem**: `spawn_blocking` thread could hang forever if stuck in GStreamer C code.

**Solution**: Added 35-second timeout to `stream_handle.await`:

```rust
match tokio::time::timeout(Duration::from_secs(35), stream_handle).await {
    Ok(Ok(Ok(()))) => { /* success */ }
    Ok(Ok(Err(e))) => { /* error */ }
    Ok(Err(e)) => { /* panic */ }
    Err(_) => {
        // TIMEOUT - thread is stuck, log and proceed to cleanup
        log::error!("Streaming task timed out - may be blocked in GStreamer");
    }
}
```

**Effect**: Prevents indefinite waits. Timeout triggers cleanup even if thread is stuck.

**Limitation**: The stuck thread will be leaked (can't kill it), but new pipelines can still be created.

---

### 2. Pipeline Cleanup Timeout (`src/rtsp/factory.rs:483-515`)

**Problem**: `bin.set_state(Null)` could block forever during cleanup.

**Solution**: Wrapped state change in separate thread with 5-second timeout:

```rust
let (cleanup_tx, cleanup_rx) = std::sync::mpsc::channel();
std::thread::spawn(move || {
    let result = bin.set_state(gstreamer::State::Null);
    let _ = cleanup_tx.send(result);
});

match cleanup_rx.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok(_)) => { /* cleanup succeeded */ }
    Ok(Err(e)) => { /* cleanup failed */ }
    Err(Timeout) => {
        // Thread stuck, log and abandon
        log::error!("Pipeline cleanup timed out - resources may be leaked");
    }
    ...
}
```

**Effect**: Ensures pipeline counter always gets decremented, even if cleanup fails.

**Limitation**: Resources may leak (bin, elements, VAAPI contexts), but counter stays accurate.

---

### 3. Pipeline Counter Drop Guard (`src/rtsp/factory.rs:157-208`)

**Problem**: Manual `fetch_sub` at end of function wasn't reached if timeout/panic occurred.

**Solution**: Created RAII guard that auto-decrements counter on drop:

```rust
struct PipelineCounterGuard {
    counter: Arc<AtomicUsize>,
    created_at: Instant,
    // ...
}

impl Drop for PipelineCounterGuard {
    fn drop(&mut self) {
        let remaining = self.counter.fetch_sub(1, Ordering::Relaxed) - 1;
        let lifetime = self.created_at.elapsed();

        log::info!("Pipeline TERMINATED after {:?}. Remaining: {}/2",
                   lifetime, remaining);

        // Warn about zombie pipelines (active > 5 minutes)
        if lifetime > Duration::from_secs(300) {
            log::warn!("Pipeline had unusually long lifetime - possible zombie");
        }
    }
}
```

**Usage**:
```rust
// At start of pipeline task
let _pipeline_guard = PipelineCounterGuard::new(...);

// Counter automatically decremented when guard drops, even on:
// - Normal completion
// - Errors
// - Panics
// - Timeouts
```

**Effect**:
- Prevents counter getting stuck at 2/2
- Provides zombie pipeline detection (lifetime warnings)
- Ensures accurate counter even on abnormal exits

---

### 4. GStreamer MainLoop Watchdog (`src/rtsp/gst/server.rs:134-182`)

**Problem**: MainLoop could block forever, making container unkillable.

**Solution**: Heartbeat-based watchdog that aborts process if MainLoop is stuck:

```rust
// Cleanup thread sends heartbeat every 5 seconds
let (heartbeat_tx, heartbeat_rx) = std::sync::mpsc::channel();
// ... in cleanup loop:
let _ = heartbeat_tx.send(());
std::thread::sleep(Duration::from_secs(5));

// Watchdog thread monitors heartbeats
loop {
    match heartbeat_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(()) => { /* healthy */ }
        Err(Timeout) => {
            log::error!("CRITICAL: MainLoop stuck for 60s!");
            log::error!("Aborting to allow container restart...");
            std::thread::sleep(Duration::from_millis(500)); // flush logs
            std::process::abort();
        }
        ...
    }
}
```

**Effect**:
- If MainLoop is stuck for >60 seconds → process aborts
- Docker/systemd can restart the container
- Container becomes "killable" (by force-aborting)

**Trade-off**:
- This is a controlled crash, not a graceful shutdown
- Better than unkillable container requiring manual intervention

---

## Testing Plan

### Local Testing (before deployment)

```bash
# Build with Docker
docker build -t neolink-patched .

# Run with test config
docker run --rm -v $(pwd)/test_config.toml:/config.toml neolink-patched

# Monitor logs
docker logs -f <container_id>
```

### What to Look For

**Success Indicators**:
```
[INFO] Starting GStreamer MainLoop watchdog thread
[INFO] Pipeline ACTIVE (1/2)
[INFO] Pipeline TERMINATED after 23s. Remaining active: 0/2
[DEBUG] GStreamer MainLoop watchdog: heartbeat received, system healthy
```

**Timeout Behavior** (simulated by disconnecting camera):
```
[WARN] No frames received for 30s - camera may have disconnected
[INFO] Pipeline TERMINATED after 31s. Remaining active: 0/2
```

**Watchdog Abort** (only happens if severely stuck):
```
[ERROR] ╔════════════════════════════════════════╗
[ERROR] ║  CRITICAL: GStreamer MainLoop watchdog ║
[ERROR] ╚════════════════════════════════════════╝
[ERROR] Aborting to allow container restart...
```

### Stress Testing

1. **Resource Exhaustion Test**:
   ```bash
   # Connect many RTSP clients simultaneously
   for i in {1..10}; do
       ffplay rtsp://localhost:8554/Camera/mainStream &
   done

   # Watch for:
   # - Pipeline limit messages
   # - Proper rejection of excess connections
   # - Counter accuracy
   ```

2. **Camera Disconnect Test**:
   ```bash
   # Unplug camera or block network while streaming
   # Should see:
   # - 30s timeout warnings
   # - Clean pipeline termination
   # - Counter back to 0/2
   ```

3. **Long-Running Test**:
   ```bash
   # Run for 24 hours
   docker run -d --name neolink-test neolink-patched

   # Check periodically:
   docker stats neolink-test  # Memory should be stable
   docker logs neolink-test | grep WARN  # Look for zombie warnings
   ```

4. **Kill Test**:
   ```bash
   # Try to kill container
   docker stop neolink-test  # Should stop within 65 seconds (60s watchdog + 5s grace)

   # If it takes longer, check logs:
   docker logs neolink-test | tail -50
   ```

---

## Expected Behavior Changes

### Before Patches

❌ Container unkillable (requires `docker kill -9` or host restart)
❌ Pipeline counter stuck at 2/2 indefinitely
❌ Errors: `g_object_force_floating: assertion 'G_IS_OBJECT (object)' failed`
❌ New clients rejected forever even after cameras reconnect
❌ Resource leaks accumulate until OOM

### After Patches

✅ Container responds to SIGTERM within 65 seconds (watchdog timeout)
✅ Pipeline counter always accurate (Drop guard)
✅ Timeouts prevent indefinite blocking (35s stream, 5s cleanup)
✅ Zombie pipelines detected and logged (>5 min warnings)
✅ Process aborts cleanly if MainLoop is stuck (watchdog)

### Still Possible (Limitations)

⚠️ **Thread Leaks**: Stuck spawn_blocking threads can't be killed (OS limitation)
⚠️ **Resource Leaks**: VAAPI contexts may leak between watchdog restarts
⚠️ **g_object errors**: Still occur when resources are exhausted
⚠️ **Watchdog Aborts**: Controlled crashes instead of graceful shutdown

---

## Deployment

### Build

```bash
# In Docker environment
docker build -t neolink:stability-patch .

# Or locally (requires GStreamer 1.20+)
cargo build --release --features=gstreamer
```

### Deploy

```bash
# Update your docker-compose.yml or k8s manifest
image: neolink:stability-patch

# Ensure restart policy is set
restart: unless-stopped

# Monitor first 24 hours closely
docker logs -f neolink --tail 100
```

### Monitoring

Key log patterns to watch:

```bash
# Healthy operation
grep "Pipeline TERMINATED after" logs.txt
# Should show reasonable lifetimes (< 5 minutes unless intentionally streaming)

# Zombie detection
grep "unusually long lifetime" logs.txt
# Should be rare; frequent warnings indicate ongoing problems

# Watchdog activations
grep "CRITICAL.*watchdog" logs.txt
# Should be VERY rare; each one is a controlled crash

# Timeout occurrences
grep "timed out after" logs.txt
# Some are normal (camera disconnects); frequent means deeper issue
```

---

## Known Issues & Workarounds

### Issue 1: Threads Still Leak

**Symptom**: `ps aux | grep neolink` shows thread count slowly increasing
**Cause**: Stuck spawn_blocking threads can't be killed from Rust
**Workaround**: Watchdog will abort process after 60s, container restarts
**Long-term fix**: FFmpeg migration (no blocking C threads)

### Issue 2: VAAPI Resources May Not Release

**Symptom**: vainfo shows increasing context count, new pipelines fail with VAAPI errors
**Cause**: Cleanup timeout abandons bin without releasing VAAPI contexts
**Workaround**: Watchdog restart clears all resources
**Long-term fix**: FFmpeg subprocess isolation

### Issue 3: Watchdog False Positives

**Symptom**: Watchdog aborts even though system seems okay
**Cause**: Cleanup thread blocked for >60s (rare)
**Workaround**: Check logs before abort - if pipelines are working, may need to increase watchdog timeout
**Config** (future): Add `watchdog_timeout_sec` config option

---

## Metrics

Track these metrics to measure improvement:

1. **Container Restarts**: Should be rare (< 1 per week)
2. **Pipeline Counter Accuracy**: Should match actual active streams
3. **Average Pipeline Lifetime**: Should be < 2 minutes for typical use
4. **Watchdog Aborts**: Should be < 1 per month in stable deployment
5. **Memory Growth**: Should be flat over 24 hours

---

## Next Steps: FFmpeg Migration Plan

These patches are **band-aids**, not cures. They make the system survivable but don't fix root causes.

**Phase 2 (Recommended timeline: Next 2-4 weeks)**:

1. **Week 1**: Set up `mediamtx` RTSP server alongside neolink
2. **Week 2**: Replace GStreamer pipelines with ffmpeg subprocess spawning
3. **Week 3**: Remove GStreamer dependencies, extensive testing
4. **Week 4**: Production deployment and monitoring

**Why FFmpeg**:
- Clean process isolation (no thread leaks)
- Simple kill semantics (SIGTERM → clean exit)
- Better observability (strace, external monitoring)
- Simpler codebase (remove 3000+ lines of GStreamer code)

See `docs/ffmpeg_migration_plan.md` (to be created) for detailed migration steps.

---

## Questions?

If you encounter issues during testing or deployment:

1. Check logs for specific error patterns (see Monitoring section)
2. Verify GStreamer version (must be 1.20+)
3. Check VAAPI availability: `vainfo`
4. Review Docker logs around the time of failure
5. Check resource limits: `ulimit -a`, memory, /dev/dri permissions

## Files Modified

- `src/rtsp/factory.rs` - Added timeouts, Drop guard, zombie detection
- `src/rtsp/gst/server.rs` - Added MainLoop watchdog thread

## Rollback

If patches cause issues:

```bash
git revert HEAD  # or specific commit
docker build -t neolink:rollback .
```

The patches are additive and shouldn't break existing functionality, but rollback is straightforward if needed.
