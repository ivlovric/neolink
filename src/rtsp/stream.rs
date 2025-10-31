use anyhow::{anyhow, Result};
use log::*;
use neolink_core::bc_protocol::StreamKind;
use neolink_core::bcmedia::model::{BcMedia, VideoType};
use std::collections::HashSet;

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
    let mut buffer = Vec::new();
    let mut video_codec = None;

    // Examine up to 20 frames to detect codec
    for _ in 0..20 {
        if let Some(media) = media_rx.recv().await {
            // Check if this is a video frame with codec info
            match &media {
                BcMedia::Iframe(iframe) => {
                    video_codec = Some(VideoCodec::from_video_type(iframe.video_type));
                    buffer.push(media);
                    break; // Found codec, stop buffering
                }
                BcMedia::Pframe(pframe) => {
                    video_codec = Some(VideoCodec::from_video_type(pframe.video_type));
                    buffer.push(media);
                    break; // Found codec, stop buffering
                }
                _ => {
                    // Audio or info frame, keep buffering
                    buffer.push(media);
                }
            }
        } else {
            return Err(anyhow!("Stream ended before detecting codec"));
        }
    }

    let video_codec = video_codec
        .ok_or_else(|| anyhow!("Failed to detect video codec from stream"))?;

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
