///
/// # FFmpeg Process Wrapper
///
/// This module provides a wrapper for spawning and managing FFmpeg processes
/// to transcode/remux camera streams and push them to MediaMTX via RTSP.
///
/// Key features:
/// - Spawn FFmpeg with stdin pipe for feeding media data
/// - Process lifecycle management (spawn, monitor, kill)
/// - Automatic restart on failure
/// - Support for H.264 and H.265 passthrough and transcoding
///
use anyhow::{anyhow, Context, Result};
use log::*;
use neolink_core::bcmedia::model::{BcMedia, VideoType};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child as TokioChild, Command as TokioCommand};
use tokio::sync::mpsc::Receiver as MpscReceiver;

/// FFmpeg process configuration
#[derive(Debug, Clone)]
pub struct FfmpegConfig {
    /// Path to ffmpeg binary (default: "ffmpeg")
    pub ffmpeg_path: String,
    /// Input video codec (h264 or hevc)
    pub input_codec: VideoCodec,
    /// Whether to transcode or passthrough
    pub transcode: TranscodeMode,
    /// Transcode device/encoder selection (hardware vs software)
    pub transcode_device: TranscodeDevice,
    /// RTSP publish URL (e.g., "rtsp://localhost:8554/CameraName")
    pub rtsp_url: String,
    /// Camera name for logging
    pub camera_name: String,
    /// Stream name for logging (e.g., "mainStream")
    pub stream_name: String,
}

/// Video codec type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    /// Get the FFmpeg input format string
    pub fn input_format(&self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "hevc",
        }
    }

    /// Create from neolink VideoType
    pub fn from_video_type(vtype: VideoType) -> Self {
        match vtype {
            VideoType::H264 => VideoCodec::H264,
            VideoType::H265 => VideoCodec::H265,
        }
    }
}

/// Transcode mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscodeMode {
    /// Copy video codec without transcoding
    Passthrough,
    /// Transcode to H.264 using libx264
    TranscodeToH264,
}

/// Transcode device/encoder selection
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscodeDevice {
    /// Auto-detect: try hardware (VAAPI), fallback to software (libx264)
    Auto,
    /// Force VAAPI hardware encoding (Intel Quick Sync, AMD, etc.)
    VAAPI(String), // Device path like "/dev/dri/renderD128"
    /// Force software encoding (libx264)
    Software,
}

impl TranscodeDevice {
    /// Parse from config string
    pub fn from_config(s: &Option<String>) -> Self {
        match s.as_deref() {
            None | Some("auto") => TranscodeDevice::Auto,
            Some("vaapi") => TranscodeDevice::VAAPI("/dev/dri/renderD128".to_string()),
            Some("x264") | Some("software") => TranscodeDevice::Software,
            Some(path) if path.starts_with("/dev/dri/") => TranscodeDevice::VAAPI(path.to_string()),
            _ => {
                warn!("Unknown transcode_device value: {:?}, using auto", s);
                TranscodeDevice::Auto
            }
        }
    }
}

/// FFmpeg process wrapper
pub struct FfmpegProcess {
    /// Process handle
    process: TokioChild,
    /// Process stdin for writing media data
    stdin: tokio::process::ChildStdin,
    /// Configuration
    config: FfmpegConfig,
}

impl FfmpegProcess {
    /// Spawn a new FFmpeg process
    ///
    /// # Arguments
    /// * `config` - FFmpeg configuration
    ///
    /// # Returns
    /// FFmpeg process wrapper
    pub fn spawn(config: FfmpegConfig) -> Result<Self> {
        let camera = &config.camera_name;
        let stream = &config.stream_name;

        info!("{}::{}: Spawning FFmpeg process", camera, stream);
        debug!("{}::{}: FFmpeg config: {:?}", camera, stream, config);

        // Try hardware acceleration first if Auto or VAAPI
        let use_vaapi = match (&config.transcode, &config.transcode_device) {
            (TranscodeMode::TranscodeToH264, TranscodeDevice::Auto) => true,
            (TranscodeMode::TranscodeToH264, TranscodeDevice::VAAPI(_)) => true,
            _ => false,
        };

        if use_vaapi {
            // Try VAAPI first
            match Self::spawn_with_vaapi(config.clone()) {
                Ok(process) => return Ok(process),
                Err(e) => {
                    match &config.transcode_device {
                        TranscodeDevice::VAAPI(_) => {
                            // VAAPI was explicitly requested, don't fallback
                            return Err(e.context("VAAPI encoding failed (explicitly requested)"));
                        }
                        TranscodeDevice::Auto => {
                            // Auto mode: fallback to software
                            warn!("{}::{}: VAAPI encoding failed, falling back to software: {}", camera, stream, e);
                            info!("{}::{}: Using software encoding (libx264)", camera, stream);
                        }
                        _ => {}
                    }
                }
            }
        }

        // Software encoding or passthrough
        Self::spawn_with_software(config)
    }

    /// Spawn FFmpeg with VAAPI hardware acceleration
    fn spawn_with_vaapi(config: FfmpegConfig) -> Result<Self> {
        let camera = &config.camera_name;
        let stream = &config.stream_name;

        let vaapi_device = match &config.transcode_device {
            TranscodeDevice::VAAPI(path) => path.clone(),
            TranscodeDevice::Auto => "/dev/dri/renderD128".to_string(),
            _ => "/dev/dri/renderD128".to_string(),
        };

        info!("{}::{}: Attempting VAAPI hardware encoding (device: {})", camera, stream, vaapi_device);

        // Build FFmpeg command with VAAPI
        let mut cmd = TokioCommand::new(&config.ffmpeg_path);

        // Hardware acceleration setup (must come before input)
        cmd.arg("-hwaccel").arg("vaapi")
            .arg("-hwaccel_device").arg(&vaapi_device)
            .arg("-hwaccel_output_format").arg("vaapi");

        // Input configuration: read from stdin
        cmd.arg("-f")
            .arg(config.input_codec.input_format())
            .arg("-i")
            .arg("pipe:0");

        // Video codec configuration with VAAPI
        match config.transcode {
            TranscodeMode::Passthrough => {
                cmd.arg("-c:v").arg("copy");
            }
            TranscodeMode::TranscodeToH264 => {
                cmd.arg("-c:v").arg("h264_vaapi")
                    .arg("-qp").arg("23"); // Quality (lower = better, 23 is good balance)
            }
        }

        // Output configuration: RTSP push
        cmd.arg("-f")
            .arg("rtsp")
            .arg("-rtsp_transport")
            .arg("tcp")
            .arg(&config.rtsp_url);

        // FFmpeg options for better reliability
        cmd.arg("-loglevel")
            .arg("warning") // Only show warnings and errors
            .arg("-nostats") // Don't show encoding statistics
            .arg("-hide_banner"); // Hide FFmpeg banner

        // Stdio configuration
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Log the complete FFmpeg command for debugging
        let cmd_string = format!(
            "{} -hwaccel vaapi -hwaccel_device {} -hwaccel_output_format vaapi -f {} -i pipe:0 -c:v h264_vaapi -qp 23 -f rtsp -rtsp_transport tcp {} -loglevel warning",
            config.ffmpeg_path,
            vaapi_device,
            config.input_codec.input_format(),
            config.rtsp_url
        );
        info!("{}::{}: FFmpeg command (VAAPI): {}", camera, stream, cmd_string);

        // Spawn the process
        let mut process = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn FFmpeg with VAAPI: {:?}", cmd))?;

        // Get stdin handle for writing media data
        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open FFmpeg stdin"))?;

        info!("{}::{}: FFmpeg process spawned with VAAPI (PID: {:?})", camera, stream, process.id());

        Ok(Self {
            process,
            stdin,
            config,
        })
    }

    /// Spawn FFmpeg with software encoding (libx264)
    fn spawn_with_software(config: FfmpegConfig) -> Result<Self> {
        let camera = &config.camera_name;
        let stream = &config.stream_name;

        // Build FFmpeg command with software encoding
        let mut cmd = TokioCommand::new(&config.ffmpeg_path);

        // Input configuration: read from stdin
        cmd.arg("-f")
            .arg(config.input_codec.input_format())
            .arg("-i")
            .arg("pipe:0");

        // Video codec configuration
        match config.transcode {
            TranscodeMode::Passthrough => {
                cmd.arg("-c:v").arg("copy");
            }
            TranscodeMode::TranscodeToH264 => {
                cmd.arg("-c:v")
                    .arg("libx264")
                    .arg("-preset")
                    .arg("medium")
                    .arg("-tune")
                    .arg("zerolatency");
            }
        }

        // Output configuration: RTSP push
        cmd.arg("-f")
            .arg("rtsp")
            .arg("-rtsp_transport")
            .arg("tcp")
            .arg(&config.rtsp_url);

        // FFmpeg options for better reliability
        cmd.arg("-loglevel")
            .arg("warning") // Only show warnings and errors
            .arg("-nostats") // Don't show encoding statistics
            .arg("-hide_banner"); // Hide FFmpeg banner

        // Stdio configuration
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Log the complete FFmpeg command for debugging
        let codec_args = match config.transcode {
            TranscodeMode::Passthrough => "-c:v copy",
            TranscodeMode::TranscodeToH264 => "-c:v libx264 -preset medium -tune zerolatency",
        };
        let cmd_string = format!("{} -f {} -i pipe:0 {} -f rtsp -rtsp_transport tcp {} -loglevel warning",
            config.ffmpeg_path,
            config.input_codec.input_format(),
            codec_args,
            config.rtsp_url
        );
        info!("{}::{}: FFmpeg command (software): {}", camera, stream, cmd_string);

        // Spawn the process
        let mut process = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn FFmpeg process: {:?}", cmd))?;

        // Get stdin handle for writing media data
        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open FFmpeg stdin"))?;

        info!("{}::{}: FFmpeg process spawned (PID: {:?})", camera, stream, process.id());

        Ok(Self {
            process,
            stdin,
            config,
        })
    }

    /// Write media frame to FFmpeg stdin
    ///
    /// # Arguments
    /// * `data` - Raw media frame data (H.264 or H.265 NAL units)
    ///
    /// # Returns
    /// Ok(()) on success, Err if write failed
    pub async fn write_frame(&mut self, data: &[u8]) -> Result<()> {
        self.stdin
            .write_all(data)
            .await
            .context("Failed to write to FFmpeg stdin")?;

        Ok(())
    }

    /// Flush stdin buffer
    pub async fn flush(&mut self) -> Result<()> {
        self.stdin
            .flush()
            .await
            .context("Failed to flush FFmpeg stdin")?;
        Ok(())
    }

    /// Check if the process is still running
    pub fn is_running(&mut self) -> bool {
        match self.process.try_wait() {
            Ok(Some(status)) => {
                warn!(
                    "{}::{}: FFmpeg process exited with status: {}",
                    self.config.camera_name, self.config.stream_name, status
                );
                false
            }
            Ok(None) => true, // Still running
            Err(e) => {
                error!(
                    "{}::{}: Error checking FFmpeg process status: {}",
                    self.config.camera_name, self.config.stream_name, e
                );
                false
            }
        }
    }

    /// Kill the FFmpeg process
    ///
    /// Sends SIGTERM and waits for clean shutdown
    pub async fn kill(&mut self) -> Result<()> {
        let camera = &self.config.camera_name;
        let stream = &self.config.stream_name;

        info!("{}::{}: Killing FFmpeg process", camera, stream);

        // Try to kill gracefully
        if let Err(e) = self.process.kill().await {
            warn!("{}::{}: Failed to kill FFmpeg process: {}", camera, stream, e);
        }

        // Wait for process to exit (with timeout)
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.process.wait()
        ).await {
            Ok(Ok(status)) => {
                info!("{}::{}: FFmpeg process exited: {}", camera, stream, status);
                Ok(())
            }
            Ok(Err(e)) => {
                error!("{}::{}: Error waiting for FFmpeg process: {}", camera, stream, e);
                Err(e.into())
            }
            Err(_) => {
                warn!("{}::{}: FFmpeg process did not exit within 5s", camera, stream);
                // Process is stuck, it will be reaped by OS eventually
                Ok(())
            }
        }
    }

    /// Get the FFmpeg process ID
    pub fn pid(&self) -> Option<u32> {
        self.process.id()
    }
}

impl Drop for FfmpegProcess {
    fn drop(&mut self) {
        let camera = &self.config.camera_name;
        let stream = &self.config.stream_name;

        // Try to kill the process if it's still running
        // Note: We can't use async in Drop, so we use the sync kill
        if let Err(e) = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(format!("{}", self.process.id().unwrap_or(0)))
            .output()
        {
            warn!("{}::{}: Failed to kill FFmpeg process in Drop: {}", camera, stream, e);
        }
    }
}

/// Stream camera media to FFmpeg process
///
/// This function reads BcMedia frames from the receiver and writes them to FFmpeg stdin.
/// It handles process lifecycle and restarts on failure.
///
/// # Arguments
/// * `mut media_rx` - Receiver for camera media frames
/// * `config` - FFmpeg configuration
///
/// # Returns
/// Ok(()) when stream ends, Err on fatal error
pub async fn stream_to_ffmpeg(
    mut media_rx: MpscReceiver<BcMedia>,
    config: FfmpegConfig,
) -> Result<()> {
    let camera = &config.camera_name;
    let stream = &config.stream_name;

    info!("{}::{}: Starting FFmpeg streaming", camera, stream);

    // Spawn FFmpeg process
    let mut ffmpeg = FfmpegProcess::spawn(config.clone())?;

    let mut frame_count = 0usize;
    let mut error_count = 0usize;
    const MAX_ERRORS: usize = 10;

    // Stream media frames to FFmpeg
    while let Some(media) = media_rx.recv().await {
        // Check if process is still running
        if !ffmpeg.is_running() {
            error!("{}::{}: FFmpeg process died, attempting restart", camera, stream);

            // Attempt to restart
            match FfmpegProcess::spawn(config.clone()) {
                Ok(new_ffmpeg) => {
                    ffmpeg = new_ffmpeg;
                    error_count = 0; // Reset error count on successful restart
                    info!("{}::{}: FFmpeg process restarted successfully", camera, stream);
                }
                Err(e) => {
                    error!("{}::{}: Failed to restart FFmpeg: {}", camera, stream, e);
                    error_count += 1;
                    if error_count >= MAX_ERRORS {
                        return Err(anyhow!("Too many FFmpeg restart failures"));
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            }
        }

        // Extract video frame data
        let frame_data = match &media {
            BcMedia::Iframe(iframe) => Some(&iframe.data),
            BcMedia::Pframe(pframe) => Some(&pframe.data),
            _ => None, // Ignore audio frames for now
        };

        if let Some(data) = frame_data {
            // Write frame to FFmpeg stdin
            match ffmpeg.write_frame(data).await {
                Ok(_) => {
                    frame_count += 1;
                    if frame_count % 100 == 0 {
                        trace!("{}::{}: Sent {} frames to FFmpeg", camera, stream, frame_count);
                    }
                }
                Err(e) => {
                    error!("{}::{}: Failed to write frame to FFmpeg: {}", camera, stream, e);
                    error_count += 1;
                    if error_count >= MAX_ERRORS {
                        return Err(anyhow!("Too many FFmpeg write errors"));
                    }
                }
            }
        }
    }

    info!("{}::{}: Media stream ended, killing FFmpeg process", camera, stream);
    ffmpeg.kill().await?;

    info!("{}::{}: FFmpeg streaming completed ({} frames)", camera, stream, frame_count);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_codec_input_format() {
        assert_eq!(VideoCodec::H264.input_format(), "h264");
        assert_eq!(VideoCodec::H265.input_format(), "hevc");
    }

    #[test]
    fn test_video_codec_from_video_type() {
        assert_eq!(
            VideoCodec::from_video_type(VideoType::H264),
            VideoCodec::H264
        );
        assert_eq!(
            VideoCodec::from_video_type(VideoType::H265),
            VideoCodec::H265
        );
    }

    #[test]
    fn test_ffmpeg_config_creation() {
        let config = FfmpegConfig {
            ffmpeg_path: "ffmpeg".to_string(),
            input_codec: VideoCodec::H264,
            transcode: TranscodeMode::Passthrough,
            rtsp_url: "rtsp://localhost:8554/test".to_string(),
            camera_name: "TestCam".to_string(),
            stream_name: "mainStream".to_string(),
        };

        assert_eq!(config.ffmpeg_path, "ffmpeg");
        assert_eq!(config.input_codec, VideoCodec::H264);
        assert_eq!(config.transcode, TranscodeMode::Passthrough);
    }

    // Note: Process spawning tests require ffmpeg to be installed
    #[tokio::test]
    #[ignore]
    async fn test_spawn_ffmpeg_process() {
        let config = FfmpegConfig {
            ffmpeg_path: "ffmpeg".to_string(),
            input_codec: VideoCodec::H264,
            transcode: TranscodeMode::Passthrough,
            rtsp_url: "rtsp://localhost:8554/test".to_string(),
            camera_name: "TestCam".to_string(),
            stream_name: "mainStream".to_string(),
        };

        let mut process = FfmpegProcess::spawn(config).unwrap();
        assert!(process.is_running());

        // Kill the process
        process.kill().await.unwrap();
        assert!(!process.is_running());
    }
}
