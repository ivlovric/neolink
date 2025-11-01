use anyhow::{anyhow, Result};
use log::*;
use neolink_core::bc_protocol::StreamKind;
use neolink_core::bcmedia::model::BcMedia;

use crate::common::NeoInstance;
use super::ffmpeg::{FfmpegConfig, TranscodeMode, VideoCodec, stream_to_ffmpeg};
use super::mediamtx::MediaMtxClient;

type AnyResult<T> = anyhow::Result<T, anyhow::Error>;

/// This handles the stream itself by creating an FFmpeg process and streaming to MediaMTX
pub(crate) async fn stream_main(
    camera: NeoInstance,
    stream: StreamKind,
    mediamtx: &MediaMtxClient,
    ffmpeg_path: &str,
    paths: &[String],
) -> AnyResult<()> {
    let name = camera.config().await?.borrow().name.clone();
    let camera_config = camera.config().await?.borrow().clone();

    info!("{}: Starting stream: {:?}", name, stream);

    // Use the first path as the primary stream path
    let stream_path = paths.first()
        .ok_or_else(|| anyhow!("No paths provided for stream"))?
        .trim_start_matches('/')
        .to_string();

    info!("{}: Stream will be available at {}", name, paths.join(", "));

    // Validate the path is available in MediaMTX
    if let Err(e) = mediamtx.validate_path(&stream_path).await {
        warn!("{}: Path validation warning: {}", name, e);
    }

    // Get the RTSP publish URL for FFmpeg
    let rtsp_url = mediamtx.get_publish_url(&stream_path);
    info!("{}: FFmpeg will publish to: {}", name, rtsp_url);

    // Start the camera stream
    let media_rx = camera.stream_while_live(stream).await?;

    // Learn the stream codec by examining the first few frames
    let (video_codec, transcode_mode, media_rx) =
        detect_stream_format(media_rx, &camera_config.transcode_to).await?;

    info!(
        "{}: Stream format: {:?}, transcode: {:?}",
        name, video_codec, transcode_mode
    );

    // Build FFmpeg configuration
    let ffmpeg_config = FfmpegConfig {
        ffmpeg_path: ffmpeg_path.to_string(),
        input_codec: video_codec,
        transcode: transcode_mode,
        rtsp_url,
        camera_name: name.clone(),
        stream_name: stream.to_string(),
    };

    // Start streaming to FFmpeg
    stream_to_ffmpeg(media_rx, ffmpeg_config).await?;

    info!("{}: Stream ended", name);
    Ok(())
}

/// Detect stream format by examining the first few frames
///
/// Returns (VideoCodec, TranscodeMode, updated receiver with buffered frames)
async fn detect_stream_format(
    mut media_rx: tokio::sync::mpsc::Receiver<BcMedia>,
    transcode_to: &Option<String>,
) -> Result<(VideoCodec, TranscodeMode, tokio::sync::mpsc::Receiver<BcMedia>)> {
    // Wrap codec detection in a timeout to handle battery cameras that may not send frames immediately
    // Battery cameras often disconnect before sending frames, so we need a timeout with fallback
    // We also need to wait for an I-frame (keyframe) before starting the stream
    let detection_timeout = tokio::time::Duration::from_secs(10);

    let detection_result = tokio::time::timeout(detection_timeout, async {
        let mut buffer = Vec::new();
        let mut detected_codec: Option<VideoCodec> = None;
        let mut has_iframe = false;

        // Examine frames until we have both codec and I-frame
        // Limit to 50 frames to prevent excessive buffering
        for frame_count in 0..50 {
            if let Some(media) = media_rx.recv().await {
                // Check if this is a video frame with codec info
                match &media {
                    BcMedia::Iframe(iframe) => {
                        let codec = VideoCodec::from_video_type(iframe.video_type);
                        buffer.push(media);
                        has_iframe = true;

                        if detected_codec.is_none() {
                            // First video frame is an I-frame - perfect!
                            debug!("Detected {:?} codec from I-frame", codec);
                            detected_codec = Some(codec);
                        } else {
                            // We already knew the codec from a P-frame, now we have the I-frame
                            debug!("Found I-frame after {} frames", frame_count + 1);
                        }

                        // We have both codec and I-frame, we're ready to stream
                        return Ok::<_, anyhow::Error>((detected_codec, buffer, has_iframe));
                    }
                    BcMedia::Pframe(pframe) => {
                        let codec = VideoCodec::from_video_type(pframe.video_type);
                        buffer.push(media);

                        if detected_codec.is_none() {
                            // Detected codec from P-frame, but we need an I-frame to start
                            info!("Detected {:?} codec from P-frame, waiting for I-frame...", codec);
                            detected_codec = Some(codec);
                        }
                        // Continue looping - we need an I-frame before streaming
                    }
                    _ => {
                        // Audio or info frame, keep buffering
                        buffer.push(media);
                    }
                }
            } else {
                // Stream ended before we got what we needed
                return Ok((detected_codec, buffer, has_iframe));
            }
        }

        // Examined 50 frames without finding I-frame (unlikely but possible)
        warn!("Examined 50 frames without I-frame, codec: {:?}", detected_codec);
        Ok((detected_codec, buffer, has_iframe))
    }).await;

    // Handle timeout or detection result
    let (detected_codec, buffer, has_iframe) = match detection_result {
        Ok(Ok((codec, buf, iframe))) => (codec, buf, iframe),
        Ok(Err(e)) => return Err(e),
        Err(_timeout) => {
            warn!("Codec detection timed out after 10 seconds, falling back to H.264");
            warn!("This is common with battery cameras that wake slowly");
            (None, Vec::new(), false) // Empty buffer on timeout
        }
    };

    // Warn if we're starting without an I-frame (stream may not decode properly)
    if !has_iframe && !buffer.is_empty() {
        warn!("Starting stream without I-frame - video may not display initially");
        warn!("Clients will need to wait for the next I-frame to start decoding");
    }

    let video_codec = detected_codec.unwrap_or_else(|| {
        warn!("Failed to detect video codec from stream, assuming H.264");
        warn!("If your camera uses H.265, the stream may not work correctly");
        VideoCodec::H264 // Default fallback to H.264 (most common)
    });

    // Determine transcode mode based on codec and config
    let transcode_mode = match (video_codec, transcode_to) {
        (VideoCodec::H265, Some(target)) if target.to_lowercase() == "h264" => {
            info!("Will transcode H.265 -> H.264");
            TranscodeMode::TranscodeToH264
        }
        (VideoCodec::H265, None) => {
            // Default: passthrough H.265 (most clients support it now)
            TranscodeMode::Passthrough
        }
        _ => {
            // H.264 or any other codec: passthrough
            TranscodeMode::Passthrough
        }
    };

    // Create a new channel and send buffered frames
    let (tx, rx) = tokio::sync::mpsc::channel(500);

    // Spawn a task to forward buffered frames and then the rest of the stream
    tokio::spawn(async move {
        // Send buffered frames
        for frame in buffer {
            if tx.send(frame).await.is_err() {
                return;
            }
        }

        // Forward remaining frames
        while let Some(media) = media_rx.recv().await {
            if tx.send(media).await.is_err() {
                return;
            }
        }
    });

    Ok((video_codec, transcode_mode, rx))
}
