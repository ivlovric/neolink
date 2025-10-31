# Neolink Docker Documentation

This directory contains Docker-related files for running Neolink in containers.

## Quick Start

### Using Docker Compose (Recommended)

1. **Create a config file:**
   ```bash
   cp sample_config.toml neolink.toml
   # Edit neolink.toml with your camera settings
   ```

2. **Start Neolink (FFmpeg mode - default):**
   ```bash
   docker-compose up -d neolink
   ```

3. **Access your camera streams:**
   ```
   rtsp://localhost:8554/CameraName
   ```

4. **View logs:**
   ```bash
   docker-compose logs -f neolink
   ```

### Using Docker CLI

**FFmpeg mode (recommended):**
```bash
docker run -d \
  --name neolink \
  -p 8554:8554 \
  -p 9997:9997 \
  -v $(pwd)/neolink.toml:/etc/neolink.toml \
  ivlovric/neolink:latest
```

**GStreamer mode (legacy):**
```bash
docker run -d \
  --name neolink-legacy \
  --privileged \
  --device /dev/dri:/dev/dri \
  -p 8555:8554 \
  -v $(pwd)/neolink.toml:/etc/neolink.toml \
  ivlovric/neolink:gstreamer
```

## Architecture Modes

### FFmpeg Mode (Default, Recommended)

**Advantages:**
- Simpler architecture (no complex GStreamer pipelines)
- Better stability (process isolation)
- Easier debugging (external FFmpeg processes)
- No hardware dependencies (pure software)
- Smaller image size

**Docker Image Size:**
- Base: ~200MB (vs ~400MB for GStreamer)

**Ports:**
- `8554`: RTSP (MediaMTX)
- `9997`: MediaMTX HTTP API (optional, for monitoring)

**Requirements:**
- No special privileges
- No GPU access needed
- Standard Docker setup

### GStreamer Mode (Legacy)

**When to use:**
- You need the old GStreamer implementation
- You have existing GStreamer configurations
- Migration testing

**Ports:**
- `8554`: RTSP (GStreamer)

**Requirements:**
- `privileged: true` (for hardware acceleration)
- `/dev/dri` device access (for GPU)

## Building Images

### Build FFmpeg Image

```bash
# Default build (FFmpeg mode)
docker build -t neolink:ffmpeg .

# Or explicitly specify
docker build \
  --build-arg USE_GSTREAMER=false \
  -t neolink:ffmpeg \
  .
```

### Build GStreamer Image

```bash
# Build legacy GStreamer image
docker build \
  --target runtime-gstreamer \
  --build-arg USE_GSTREAMER=true \
  -t neolink:gstreamer \
  .
```

### Multi-Architecture Builds

```bash
# Build for multiple architectures (requires buildx)
docker buildx build \
  --platform linux/amd64,linux/arm64,linux/arm/v7 \
  -t myrepo/neolink:latest \
  --push \
  .
```

## Configuration

### Basic Configuration

Create `neolink.toml`:

```toml
# FFmpeg mode configuration (optional, uses defaults)
mediamtx_api_url = "http://localhost:9997"
mediamtx_rtsp_url = "rtsp://localhost:8554"
ffmpeg_path = "ffmpeg"

[[cameras]]
name = "FrontDoor"
username = "admin"
password = "password"
address = "192.168.1.100:9000"

# Optional: Transcode H.265 to H.264
transcode_to = "h264"
```

### Advanced Configuration

**Environment Variables:**

- `RUST_LOG`: Set logging level (default: `info`)
  ```yaml
  environment:
    - RUST_LOG=neolink=debug
  ```

- `NEO_LINK_MODE`: Operation mode (default: `rtsp`)
  ```yaml
  environment:
    - NEO_LINK_MODE=mqtt-rtsp
  ```

**Volume Mounts:**

```yaml
volumes:
  - ./neolink.toml:/etc/neolink.toml:ro  # Config (read-only)
  - ./recordings:/recordings              # Recordings (optional)
```

**Network Mode:**

For host network access (advanced):
```yaml
network_mode: host
```

## Monitoring

### FFmpeg Mode

**Check MediaMTX Status:**
```bash
curl http://localhost:9997/v3/config/get
```

**List Active Streams:**
```bash
curl http://localhost:9997/v3/paths/list | jq
```

**Check FFmpeg Processes:**
```bash
docker exec neolink ps aux | grep ffmpeg
```

### GStreamer Mode

**Check GStreamer Pipelines:**
```bash
docker exec neolink-legacy gst-inspect-1.0
```

## Troubleshooting

### FFmpeg Mode

**Problem: MediaMTX not starting**

Check logs:
```bash
docker logs neolink | grep MediaMTX
```

Verify MediaMTX is running:
```bash
docker exec neolink ps aux | grep mediamtx
```

**Problem: Stream not appearing**

1. Check Neolink logs:
   ```bash
   docker logs -f neolink
   ```

2. Check MediaMTX paths:
   ```bash
   curl http://localhost:9997/v3/paths/list
   ```

3. Test FFmpeg → MediaMTX:
   ```bash
   docker exec neolink ffmpeg -f lavfi -i testsrc -f rtsp rtsp://localhost:8554/test
   ```

**Problem: FFmpeg not found**

This should not happen in the Docker image. If it does:
```bash
docker exec neolink which ffmpeg
docker exec neolink ffmpeg -version
```

### GStreamer Mode

**Problem: Hardware acceleration not working**

1. Check GPU devices:
   ```bash
   docker exec neolink-legacy ls -la /dev/dri
   ```

2. Check VAAPI:
   ```bash
   docker exec neolink-legacy vainfo
   ```

**Problem: Permission denied on /dev/dri**

Add privileged mode:
```yaml
privileged: true
devices:
  - /dev/dri:/dev/dri
```

## Performance

### Resource Usage

**FFmpeg Mode:**
- Memory: ~50-100MB per camera
- CPU: 5-15% (passthrough), 20-40% (transcoding)
- No GPU required

**GStreamer Mode:**
- Memory: ~150-250MB per camera
- CPU: 10-20% (with VAAPI), 30-50% (without)
- GPU recommended for transcoding

### Optimization

**Limit Resources:**
```yaml
deploy:
  resources:
    limits:
      memory: 512M
      cpus: '1.0'
    reservations:
      memory: 256M
```

**Multiple Cameras:**
- FFmpeg mode: Scale horizontally (one container per camera)
- GStreamer mode: One container can handle multiple cameras

## Security

### Best Practices

1. **Read-only config:**
   ```yaml
   volumes:
     - ./neolink.toml:/etc/neolink.toml:ro
   ```

2. **Non-privileged mode (FFmpeg):**
   - FFmpeg mode doesn't need `privileged: true`
   - Better security posture

3. **Network isolation:**
   ```yaml
   networks:
     - camera-network
   ```

4. **Secrets management:**
   - Use Docker secrets for passwords
   - Don't commit `neolink.toml` with passwords

### MediaMTX Authentication

Configure MediaMTX auth in the entrypoint:
```yaml
environment:
  - MEDIAMTX_PROTOCOLS=tcp
  - MEDIAMTX_READUSER=viewer
  - MEDIAMTX_READPASS=password
```

## Migration from GStreamer

### Step-by-Step

1. **Stop current container:**
   ```bash
   docker-compose down
   ```

2. **Pull new image:**
   ```bash
   docker pull ivlovric/neolink:latest
   ```

3. **Update docker-compose.yml:**
   - Add port `9997:9997` (MediaMTX API)
   - Remove `privileged: true`
   - Remove `devices: /dev/dri`

4. **Start new container:**
   ```bash
   docker-compose up -d neolink
   ```

5. **Verify:**
   ```bash
   docker logs -f neolink
   curl http://localhost:9997/v3/paths/list
   ```

### Rollback

If issues arise:
```bash
docker-compose down
docker-compose --profile legacy up -d neolink-legacy
```

## Examples

### Home Assistant

```yaml
# configuration.yaml
camera:
  - platform: generic
    name: Front Door
    stream_source: rtsp://neolink-host:8554/FrontDoor
```

### Frigate

```yaml
# config.yml
cameras:
  front_door:
    ffmpeg:
      inputs:
        - path: rtsp://neolink-host:8554/FrontDoor
          roles:
            - detect
            - record
```

### Blue Iris

Add camera:
- Type: RTSP/TCP
- IP: neolink-host
- Port: 8554
- Path: /FrontDoor

## Support

- Documentation: `../FFMPEG_MIGRATION_GUIDE.md`
- Issues: https://github.com/QuantumEntangledAndy/neolink/issues
- Discussions: https://github.com/QuantumEntangledAndy/neolink/discussions
