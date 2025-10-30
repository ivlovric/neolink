use gstreamer::ClockTime;
use std::{collections::HashMap, time::Duration};

use anyhow::{anyhow, Context, Result};
use gstreamer::{prelude::*, Bin, Caps, Element, ElementFactory, FlowError, GhostPad};
use gstreamer_app::{AppSrc, AppSrcCallbacks, AppStreamType};
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{
        BcMedia, BcMediaIframe, BcMediaInfoV1, BcMediaInfoV2, BcMediaPframe, VideoType,
    },
};
use tokio::{sync::mpsc::channel as mpsc, task::JoinHandle};

use crate::{common::NeoInstance, rtsp::gst::NeoMediaFactory, AnyResult};

#[derive(Clone, Debug)]
pub enum AudioType {
    Aac,
    Adpcm(u32),
}

#[derive(Clone, Debug)]
struct StreamConfig {
    #[allow(dead_code)]
    resolution: [u32; 2],
    bitrate: u32,
    fps: u32,
    bitrate_table: Vec<u32>,
    fps_table: Vec<u32>,
    vid_type: Option<VideoType>,
    aud_type: Option<AudioType>,
    transcode_to: Option<String>,
    transcode_device: Option<String>,
}
impl StreamConfig {
    async fn new(instance: &NeoInstance, name: StreamKind) -> AnyResult<Self> {
        let (resolution, bitrate, fps, fps_table, bitrate_table) = instance
            .run_passive_task(|cam| {
                Box::pin(async move {
                    let infos = cam
                        .get_stream_info()
                        .await?
                        .stream_infos
                        .iter()
                        .flat_map(|info| info.encode_tables.clone())
                        .collect::<Vec<_>>();
                    if let Some(encode) =
                        infos.iter().find(|encode| encode.name == name.to_string())
                    {
                        let bitrate_table = encode
                            .bitrate_table
                            .split(',')
                            .filter_map(|c| {
                                let i: Result<u32, _> = c.parse();
                                i.ok()
                            })
                            .collect::<Vec<u32>>();
                        let framerate_table = encode
                            .framerate_table
                            .split(',')
                            .filter_map(|c| {
                                let i: Result<u32, _> = c.parse();
                                i.ok()
                            })
                            .collect::<Vec<u32>>();

                        Ok((
                            [encode.resolution.width, encode.resolution.height],
                            bitrate_table
                                .get(encode.default_bitrate as usize)
                                .copied()
                                .unwrap_or(encode.default_bitrate)
                                * 1024,
                            framerate_table
                                .get(encode.default_framerate as usize)
                                .copied()
                                .unwrap_or(encode.default_framerate),
                            framerate_table.clone(),
                            bitrate_table.clone(),
                        ))
                    } else {
                        Ok(([0, 0], 0, 0, vec![], vec![]))
                    }
                })
            })
            .await?;

        Ok(StreamConfig {
            resolution,
            bitrate,
            fps,
            fps_table,
            bitrate_table,
            vid_type: None,
            aud_type: None,
            transcode_to: None,
            transcode_device: None,
        })
    }

    fn update_fps(&mut self, fps: u32) {
        let new_fps = self.fps_table.get(fps as usize).copied().unwrap_or(fps);
        self.fps = new_fps;
    }
    #[allow(dead_code)]
    fn update_bitrate(&mut self, bitrate: u32) {
        let new_bitrate = self
            .bitrate_table
            .get(bitrate as usize)
            .copied()
            .unwrap_or(bitrate);
        self.bitrate = new_bitrate;
    }

    fn update_from_media(&mut self, media: &BcMedia) {
        match media {
            BcMedia::InfoV1(BcMediaInfoV1 { fps, .. })
            | BcMedia::InfoV2(BcMediaInfoV2 { fps, .. }) => self.update_fps(*fps as u32),
            BcMedia::Aac(_) => {
                self.aud_type = Some(AudioType::Aac);
            }
            BcMedia::Adpcm(adpcm) => {
                self.aud_type = Some(AudioType::Adpcm(adpcm.block_size()));
            }
            BcMedia::Iframe(BcMediaIframe { video_type, .. })
            | BcMedia::Pframe(BcMediaPframe { video_type, .. }) => {
                self.vid_type = Some(*video_type);
            }
        }
    }
}

pub(super) async fn make_dummy_factory(
    use_splash: bool,
    pattern: String,
) -> AnyResult<NeoMediaFactory> {
    NeoMediaFactory::new_with_callback(move |element| {
        clear_bin(&element)?;
        if !use_splash {
            Ok(None)
        } else {
            build_unknown(&element, &pattern)?;
            Ok(Some(element))
        }
    })
    .await
}

enum ClientMsg {
    NewClient {
        element: Element,
        reply: tokio::sync::oneshot::Sender<Element>,
    },
}

pub(super) async fn make_factory(
    camera: NeoInstance,
    stream: StreamKind,
) -> AnyResult<(NeoMediaFactory, JoinHandle<AnyResult<()>>)> {
    // Increased from 100 to 200 to handle more concurrent client connections
    let (client_tx, mut client_rx) = mpsc(200);
    // Create the task that creates the pipelines
    let thread = tokio::task::spawn(async move {
        let name = camera.config().await?.borrow().name.clone();

        while let Some(msg) = client_rx.recv().await {
            match msg {
                ClientMsg::NewClient { element, reply } => {
                    log::info!("New RTSP client connection for {name}::{stream} - creating pipeline");
                    let camera = camera.clone();
                    let name = name.clone();
                    tokio::task::spawn(async move {
                        clear_bin(&element)?;
                        log::trace!("{name}::{stream}: Starting camera");
                        log::debug!("{name}::{stream}: Pipeline lifecycle: CREATED");

                        // Start the camera
                        let config = camera.config().await?.borrow().clone();
                        let mut media_rx = camera.stream_while_live(stream).await?;

                        log::trace!("{name}::{stream}: Learning camera stream type");
                        // Learn the camera data type
                        let mut buffer = vec![];
                        let mut frame_count = 0usize;

                        let mut stream_config = StreamConfig::new(&camera, stream).await?;
                        stream_config.transcode_to = config.transcode_to.clone();
                        stream_config.transcode_device = config.transcode_device.clone();
                        while let Some(media) = media_rx.recv().await {
                            stream_config.update_from_media(&media);
                            buffer.push(media);
                            if frame_count > 10
                                || (stream_config.vid_type.is_some()
                                    && stream_config.aud_type.is_some())
                            {
                                break;
                            }
                            frame_count += 1;
                        }

                        log::trace!("{name}::{stream}: Building the pipeline");
                        // Build the right video pipeline
                        let vid_src = match stream_config.vid_type.as_ref() {
                            Some(VideoType::H264) => {
                                let src = build_h264(&element, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            Some(VideoType::H265) => {
                                // Check if transcoding to H264 is requested
                                if stream_config.transcode_to.as_ref()
                                    .map(|s| s.to_lowercase() == "h264")
                                    .unwrap_or(false)
                                {
                                    log::info!("{name}::{stream}: Transcoding H.265 to H.264");
                                    let src = build_h265_to_h264_transcode(&element, &stream_config)?;
                                    AnyResult::Ok(Some(src))
                                } else {
                                    let src = build_h265(&element, &stream_config)?;
                                    AnyResult::Ok(Some(src))
                                }
                            }
                            None => {
                                build_unknown(&element, &config.splash_pattern.to_string())?;
                                AnyResult::Ok(None)
                            }
                        }?;

                        // Build the right audio pipeline
                        let aud_src = match stream_config.aud_type.as_ref() {
                            Some(AudioType::Aac) => {
                                let src = build_aac(&element, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            Some(AudioType::Adpcm(block_size)) => {
                                let src = build_adpcm(&element, *block_size, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            None => AnyResult::Ok(None),
                        }?;

                        if let Some(app) = vid_src.as_ref() {
                            app.set_callbacks(
                                AppSrcCallbacks::builder()
                                    .seek_data(move |_, _seek_pos| true)
                                    .build(),
                            );
                        }
                        if let Some(app) = aud_src.as_ref() {
                            app.set_callbacks(
                                AppSrcCallbacks::builder()
                                    .seek_data(move |_, _seek_pos| true)
                                    .build(),
                            );
                        }

                        log::trace!("{name}::{stream}: Sending pipeline to gstreamer");
                        // Clone the element bin so we can clean it up later
                        // The RTSP server will own the original, but we need a reference for cleanup
                        let element_for_cleanup = element.clone();

                        // Send the pipeline back to the factory so it can start
                        let _ = reply.send(element);

                        // Wait a brief moment to allow the RTSP server to initialize the pipeline
                        // This is critical for transcoding pipelines where encoders need time to initialize
                        tokio::time::sleep(Duration::from_millis(500)).await;

                        // Verify the pipeline is ready before sending frames
                        if let Some(vid_src) = vid_src.as_ref() {
                            // Check if the appsrc is still valid after pipeline initialization
                            if let Err(e) = check_live(vid_src) {
                                log::error!("{name}::{stream}: Pipeline validation failed: {e:?}");
                                log::error!("{name}::{stream}: This usually indicates that GStreamer elements failed to initialize.");
                                log::error!("{name}::{stream}: For transcoding issues, check VAAPI/hardware encoder availability.");
                                return Err(anyhow!("Pipeline not ready: {e:?}"));
                            }
                        }

                        log::trace!("{name}::{stream}: Pipeline validated, ready to send frames");

                        // Clone name and stream for use after the blocking task
                        let name_clone = name.clone();
                        let stream_clone = stream;

                        // Spawn a blocking task to handle the frame streaming
                        // This needs to be blocking because we interact with GStreamer's C API
                        let stream_handle = tokio::task::spawn_blocking(move || {
                            let mut aud_ts = 0u32;
                            let mut vid_ts = 0u32;
                            let mut pools = Default::default();

                            // Helper to cleanup appsrc elements and release VAAPI resources
                            let cleanup_sources = |vid_src: &Option<AppSrc>, aud_src: &Option<AppSrc>| {
                                log::debug!("{name}::{stream}: Cleaning up pipeline sources");

                                if let Some(vid) = vid_src.as_ref() {
                                    // Send EOS to signal end of stream
                                    let _ = vid.end_of_stream();
                                    // Set to NULL state to release GPU/VAAPI resources
                                    if let Err(e) = vid.set_state(gstreamer::State::Null) {
                                        log::warn!("{name}::{stream}: Failed to set video appsrc to NULL: {:?}", e);
                                    } else {
                                        log::trace!("{name}::{stream}: Video appsrc cleaned up");
                                    }
                                }

                                if let Some(aud) = aud_src.as_ref() {
                                    let _ = aud.end_of_stream();
                                    if let Err(e) = aud.set_state(gstreamer::State::Null) {
                                        log::warn!("{name}::{stream}: Failed to set audio appsrc to NULL: {:?}", e);
                                    } else {
                                        log::trace!("{name}::{stream}: Audio appsrc cleaned up");
                                    }
                                }
                            };

                            log::trace!("{name}::{stream}: Sending buffered frames");
                            for buffered in buffer.drain(..) {
                                if let Err(e) = send_to_sources(
                                    buffered,
                                    &mut pools,
                                    &vid_src,
                                    &aud_src,
                                    &mut vid_ts,
                                    &mut aud_ts,
                                    &stream_config,
                                ) {
                                    log::error!(
                                        "{name}::{stream}: Error sending buffered frame: {e:?}"
                                    );
                                    // Cleanup before returning error
                                    cleanup_sources(&vid_src, &aud_src);
                                    return Err(e);
                                }
                            }

                            log::trace!("{name}::{stream}: Sending new frames");
                            let streaming_result = (|| {
                                while let Some(data) = media_rx.blocking_recv() {
                                    match send_to_sources(
                                        data,
                                        &mut pools,
                                        &vid_src,
                                        &aud_src,
                                        &mut vid_ts,
                                        &mut aud_ts,
                                        &stream_config,
                                    ) {
                                        Ok(_) => {}
                                        Err(e) => {
                                            log::info!(
                                                "{name}::{stream}: Failed to send to source: {e:?}"
                                            );
                                            return Err(e);
                                        }
                                    }
                                }
                                log::trace!("{name}::{stream}: All media received, streaming ended");
                                AnyResult::Ok(())
                            })();

                            // Always cleanup sources, regardless of success or failure
                            cleanup_sources(&vid_src, &aud_src);

                            streaming_result
                        });

                        // Wait for the streaming task to complete and propagate errors
                        let stream_result = match stream_handle.await {
                            Ok(Ok(())) => {
                                log::debug!("{name_clone}::{stream_clone}: Streaming completed successfully");
                                AnyResult::Ok(())
                            }
                            Ok(Err(e)) => {
                                log::error!("{name_clone}::{stream_clone}: Streaming error: {e:?}");
                                Err(e)
                            }
                            Err(e) => {
                                log::error!("{name_clone}::{stream_clone}: Streaming task panicked: {e:?}");
                                Err(anyhow!("Streaming task panicked: {e:?}"))
                            }
                        };

                        // Cleanup the pipeline bin to release VAAPI/GPU resources
                        // This is critical to prevent resource leaks when streams end
                        log::debug!("{name_clone}::{stream_clone}: Pipeline lifecycle: ENDING - cleaning up resources");
                        if let Ok(bin) = element_for_cleanup.dynamic_cast::<Bin>() {
                            // Set bin to NULL state to release all elements and VAAPI resources
                            match bin.set_state(gstreamer::State::Null) {
                                Ok(_) => {
                                    // Wait for state change to complete
                                    let (result, current, _) = bin.state(gstreamer::ClockTime::from_mseconds(1000));
                                    if result.is_ok() && current == gstreamer::State::Null {
                                        log::info!("{name_clone}::{stream_clone}: Pipeline lifecycle: DESTROYED - all resources released");
                                    } else {
                                        log::warn!("{name_clone}::{stream_clone}: Pipeline lifecycle: CLEANUP_INCOMPLETE - state={:?}", current);
                                    }
                                }
                                Err(e) => {
                                    log::error!("{name_clone}::{stream_clone}: Pipeline lifecycle: CLEANUP_FAILED - {:?}", e);
                                }
                            }
                        } else {
                            log::error!("{name_clone}::{stream_clone}: Pipeline lifecycle: CLEANUP_ERROR - could not cast to Bin");
                        }

                        stream_result
                    });
                }
            }
        }
        AnyResult::Ok(())
    });

    // Now setup the factory
    let factory = NeoMediaFactory::new_with_callback(move |element| {
        let (reply, new_element) = tokio::sync::oneshot::channel();

        // Try to send with timeout to avoid blocking GStreamer thread pool indefinitely
        // Use try_send in a loop with short sleeps (max 5 seconds total)
        let mut msg = Some(ClientMsg::NewClient { element, reply });
        let send_result = (0..50).find_map(|_| {
            match client_tx.try_send(msg.take().unwrap()) {
                Ok(_) => Some(Ok(())),
                Err(tokio::sync::mpsc::error::TrySendError::Full(returned_msg)) => {
                    // Channel full, wait a bit and retry
                    msg = Some(returned_msg);
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    Some(Err(anyhow!("Client channel closed")))
                }
            }
        });

        match send_result {
            Some(Ok(())) => {
                // Successfully sent, now wait for reply with timeout (10 seconds)
                // Use a thread-based timeout since tokio::sync::oneshot doesn't have blocking_recv_timeout
                let (timeout_tx, timeout_rx) = std::sync::mpsc::channel();

                std::thread::spawn(move || {
                    let result = new_element.blocking_recv();
                    let _ = timeout_tx.send(result);
                });

                match timeout_rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(Ok(element)) => Ok(Some(element)),
                    Ok(Err(_)) => Err(anyhow!("Pipeline creation channel closed")),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        Err(anyhow!("Timeout waiting for pipeline creation (10s)"))
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        Err(anyhow!("Pipeline creation timeout thread disconnected"))
                    }
                }
            }
            Some(Err(e)) => Err(e),
            None => Err(anyhow!("Timeout trying to send client message (5s)")),
        }
    })
    .await?;
    Ok((factory, thread))
}

/// Helper function to create and configure a buffer pool
fn create_buffer_pool(size: usize) -> AnyResult<gstreamer::BufferPool> {
    let pool = gstreamer::BufferPool::new();
    let mut pool_config = pool.config();
    // Set a max buffers to ensure we don't grow in memory endlessly
    // min=8, max=32 buffers per pool
    pool_config.set_params(None, size as u32, 8, 32);
    pool.set_config(pool_config)
        .map_err(|e| anyhow!("Failed to set buffer pool config: {:?}", e))?;
    pool.set_active(true)
        .map_err(|e| anyhow!("Failed to activate buffer pool: {:?}", e))?;
    Ok(pool)
}

fn send_to_sources(
    data: BcMedia,
    pools: &mut HashMap<usize, gstreamer::BufferPool>,
    vid_src: &Option<AppSrc>,
    aud_src: &Option<AppSrc>,
    vid_ts: &mut u32,
    aud_ts: &mut u32,
    stream_config: &StreamConfig,
) -> AnyResult<()> {
    // Update TS
    match data {
        BcMedia::Aac(aac) => {
            let duration = aac.duration().expect("Could not calculate AAC duration");
            if let Some(aud_src) = aud_src.as_ref() {
                log::debug!("Sending AAC: {:?}", Duration::from_micros(*aud_ts as u64));
                send_to_appsrc(
                    aud_src,
                    aac.data,
                    Duration::from_micros(*aud_ts as u64),
                    pools,
                )?;
            }
            *aud_ts += duration;
        }
        BcMedia::Adpcm(adpcm) => {
            let duration = adpcm
                .duration()
                .expect("Could not calculate ADPCM duration");
            if let Some(aud_src) = aud_src.as_ref() {
                log::trace!("Sending ADPCM: {:?}", Duration::from_micros(*aud_ts as u64));
                send_to_appsrc(
                    aud_src,
                    adpcm.data,
                    Duration::from_micros(*aud_ts as u64),
                    pools,
                )?;
            }
            *aud_ts += duration;
        }
        BcMedia::Iframe(BcMediaIframe { data, .. })
        | BcMedia::Pframe(BcMediaPframe { data, .. }) => {
            if let Some(vid_src) = vid_src.as_ref() {
                log::trace!("Sending VID: {:?}", Duration::from_micros(*vid_ts as u64));
                send_to_appsrc(vid_src, data, Duration::from_micros(*vid_ts as u64), pools)?;
            }
            const MICROSECONDS: u32 = 1000000;
            *vid_ts += MICROSECONDS / stream_config.fps;
        }
        _ => {}
    }
    Ok(())
}

fn send_to_appsrc(
    appsrc: &AppSrc,
    data: Vec<u8>,
    mut ts: Duration,
    pools: &mut HashMap<usize, gstreamer::BufferPool>,
) -> AnyResult<()> {
    check_live(appsrc)?; // Stop if appsrc is dropped

    // In live mode we follow the advice in
    // https://gstreamer.freedesktop.org/documentation/additional/design/element-source.html?gi-language=c#live-sources
    // Only push buffers when in play state and have a clock
    // we also timestamp at the current time
    if appsrc.is_live() {
        if let Some(time) = appsrc
            .current_clock_time()
            .and_then(|t| appsrc.base_time().map(|bt| t - bt))
        {
            if matches!(appsrc.current_state(), gstreamer::State::Playing) {
                ts = Duration::from_micros(time.useconds());
            } else {
                // Not playing
                return Ok(());
            }
        } else {
            // Clock not up yet
            return Ok(());
        }
    }
    let buf = {
        let msg_size = data.len();

        // Get or create a pool of this len
        let pool = match pools.entry(msg_size) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let size = *e.key();
                let pool = create_buffer_pool(size)
                    .with_context(|| format!("Failed to create buffer pool for size {}", size))?;
                e.insert(pool)
            }
        };

        // Get a buffer from the pool and then copy in the data
        let gst_buf = {
            let mut new_buf = pool
                .acquire_buffer(None)
                .map_err(|e| anyhow!("Failed to acquire buffer from pool: {:?}", e))?;
            let gst_buf_mut = new_buf
                .get_mut()
                .ok_or_else(|| anyhow!("Failed to get mutable reference to buffer"))?;
            let time = ClockTime::from_useconds(ts.as_micros() as u64);
            gst_buf_mut.set_dts(time);
            gst_buf_mut.set_pts(time);
            let mut gst_buf_data = gst_buf_mut
                .map_writable()
                .map_err(|_| anyhow!("Failed to map buffer as writable"))?;
            gst_buf_data.copy_from_slice(data.as_slice());
            drop(gst_buf_data);
            new_buf
        };

        // Return the new buffer with the data
        gst_buf
    };

    // Push buffer into the appsrc
    match appsrc.push_buffer(buf) {
        Ok(_) => {
            // log::info!(
            //     "Send {}{} on {}",
            //     data.data.len(),
            //     if data.keyframe { " (keyframe)" } else { "" },
            //     appsrc.name()
            // );
            Ok(())
        }
        Err(FlowError::Flushing) => {
            // Buffer is full just skip
            log::info!(
                "Buffer full on {} pausing stream until client consumes frames",
                appsrc.name()
            );
            Ok(())
        }
        Err(e) => Err(anyhow!("Error in streaming: {e:?}")),
    }?;
    // Check if we need to pause/resume based on buffer levels
    // Use hysteresis to avoid rapid state changes
    let current_level = appsrc.current_level_bytes();
    let max_bytes = appsrc.max_bytes();
    let current_state = appsrc.current_state();

    if current_level >= max_bytes * 2 / 3 && matches!(current_state, gstreamer::State::Paused) {
        if let Err(e) = appsrc.set_state(gstreamer::State::Playing) {
            log::warn!("Failed to set {} to Playing state: {:?}", appsrc.name(), e);
        }
    } else if current_level <= max_bytes / 3 && matches!(current_state, gstreamer::State::Playing) {
        if let Err(e) = appsrc.set_state(gstreamer::State::Paused) {
            log::warn!("Failed to set {} to Paused state: {:?}", appsrc.name(), e);
        }
    }
    Ok(())
}
fn check_live(app: &AppSrc) -> Result<()> {
    // Check if bus exists - this indicates the element is part of a valid pipeline
    if app.bus().is_none() {
        log::error!("check_live: App source has no bus (element may have been destroyed or removed from bin)");
        log::error!("check_live: App source state: {:?}", app.current_state());
        log::error!("check_live: This often indicates a GStreamer element initialization failure");
        return Err(anyhow!("App source is closed"));
    }

    // Check if pads are linked
    let pads = app.pads();
    if pads.is_empty() {
        log::warn!("check_live: App source has no pads");
    } else {
        let linked_count = pads.iter().filter(|pad| pad.is_linked()).count();
        if linked_count == 0 {
            log::error!("check_live: App source has {} pads but none are linked", pads.len());
            return Err(anyhow!("App source is not linked"));
        }
        log::trace!("check_live: App source has {}/{} pads linked", linked_count, pads.len());
    }

    Ok(())
}

/// Tests if a bin's pipeline can initialize properly by monitoring bus messages
/// Returns Ok if pipeline initializes without errors, Err with details if it fails
fn test_pipeline_initialization(bin: &Bin, timeout_ms: u64) -> Result<()> {
    use gstreamer::MessageView;

    // Get the pipeline's bus to monitor for errors
    let bus = match bin.bus() {
        Some(bus) => bus,
        None => {
            log::error!("Pipeline has no bus - cannot monitor initialization");
            return Err(anyhow!("No bus available for pipeline monitoring"));
        }
    };

    log::debug!("Monitoring pipeline bus for {} ms", timeout_ms);

    // Check for error and warning messages during initialization
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        // Pop messages with timeout
        if let Some(msg) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(10)) {
            match msg.view() {
                MessageView::Error(err) => {
                    let error_msg = err.error().to_string();
                    let debug_info = err.debug().unwrap_or_else(|| "No debug info".into());
                    let src_name = msg.src().map(|s| s.name()).unwrap_or_else(|| "Unknown".into());

                    log::error!("GStreamer ERROR from {}: {}", src_name, error_msg);
                    log::error!("Debug info: {}", debug_info);

                    return Err(anyhow!(
                        "Pipeline initialization failed: {} (from {})\nDebug: {}",
                        error_msg, src_name, debug_info
                    ));
                }
                MessageView::Warning(warn) => {
                    let warning_msg = warn.error().to_string();
                    let debug_info = warn.debug().unwrap_or_else(|| "No debug info".into());
                    let src_name = msg.src().map(|s| s.name()).unwrap_or_else(|| "Unknown".into());

                    log::warn!("GStreamer WARNING from {}: {}", src_name, warning_msg);
                    log::debug!("Warning debug info: {}", debug_info);

                    // Check for critical VAAPI warnings that indicate failure
                    if warning_msg.contains("vaapi") || warning_msg.contains("VAAPI") {
                        log::warn!("VAAPI warning detected - encoder may not work properly");
                    }
                }
                MessageView::Info(info) => {
                    let debug_info = info.debug().unwrap_or_else(|| "No debug info".into());
                    log::trace!("GStreamer info: {}", debug_info);
                }
                MessageView::StateChanged(sc) => {
                    if let Some(src) = msg.src() {
                        log::trace!("Element {} state changed: {:?} -> {:?}",
                            src.name(), sc.old(), sc.current());
                    }
                }
                _ => {}
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    log::debug!("Pipeline initialization monitoring completed - no errors detected");
    Ok(())
}

/// Tests if VAAPI H.264 encoder is available and functional
/// Returns true if VAAPI works, false if it should be avoided
fn test_vaapi_encoder() -> bool {
    log::debug!("Testing VAAPI H.264 encoder availability...");

    // Try new VA-API 2.0 encoder first
    if let Ok(_) = test_specific_vaapi_encoder("vah264enc", "VA-API 2.0") {
        log::info!("VA-API 2.0 H.264 encoder (vah264enc) is available and functional");
        return true;
    }

    // Fall back to testing legacy VAAPI encoder
    if let Ok(_) = test_specific_vaapi_encoder("vaapih264enc", "legacy VAAPI") {
        log::info!("Legacy VAAPI H.264 encoder (vaapih264enc) is available and functional");
        return true;
    }

    log::warn!("No VAAPI H.264 encoders are available or functional");
    log::info!("Will use software encoding (x264enc) instead");
    false
}

/// Tests a specific VAAPI encoder by name
fn test_specific_vaapi_encoder(encoder_name: &str, encoder_desc: &str) -> Result<()> {
    log::debug!("Testing {} encoder...", encoder_desc);

    let pipeline = gstreamer::Pipeline::builder()
        .name("vaapi_test")
        .build();

    // Helper to ensure cleanup always happens
    let cleanup_pipeline = |pipeline: &gstreamer::Pipeline| {
        log::trace!("Cleaning up {} test pipeline", encoder_desc);

        // Set to NULL to release VAAPI resources
        match pipeline.set_state(gstreamer::State::Null) {
            Ok(_) => {
                // Wait for NULL state to be reached (important for VAAPI resource cleanup)
                let (result, current, _) = pipeline.state(gstreamer::ClockTime::from_mseconds(500));
                if result.is_err() || current != gstreamer::State::Null {
                    log::warn!("{} test pipeline cleanup incomplete: state={:?}, result={:?}",
                        encoder_desc, current, result);
                } else {
                    log::trace!("{} test pipeline cleaned up successfully", encoder_desc);
                }
            }
            Err(e) => {
                log::warn!("{} test pipeline failed to set NULL state: {:?}", encoder_desc, e);
            }
        }
    };

    // Create test elements
    let videotestsrc = make_element("videotestsrc", "test_src")?;
    let videoconvert = make_element("videoconvert", "test_convert")?;
    let vaapi_enc = make_element(encoder_name, "test_vaapi_enc")
        .map_err(|e| anyhow!("{} encoder element not found: {}", encoder_desc, e))?;
    let fakesink = make_element("fakesink", "test_sink")?;

    // Configure for quick test
    videotestsrc.set_property("num-buffers", 1i32);

    // Add to pipeline
    pipeline.add_many([&videotestsrc, &videoconvert, &vaapi_enc, &fakesink])?;
    Element::link_many([&videotestsrc, &videoconvert, &vaapi_enc, &fakesink])
        .map_err(|e| anyhow!("Failed to link test pipeline: {:?}", e))?;

    // Try to set to READY state (this will fail if VAAPI can't initialize)
    let state_change = pipeline.set_state(gstreamer::State::Ready);
    if state_change.is_err() {
        log::debug!("{} encoder failed to reach READY state", encoder_desc);
        cleanup_pipeline(&pipeline);
        return Err(anyhow!("State change to READY failed"));
    }

    // Monitor bus for errors
    if let Some(bin) = pipeline.dynamic_cast_ref::<Bin>() {
        if let Err(e) = test_pipeline_initialization(bin, 100) {
            cleanup_pipeline(&pipeline);
            return Err(e);
        }
    }

    // Try to transition to PAUSED (encoder will be fully initialized)
    let state_change = pipeline.set_state(gstreamer::State::Paused);
    if state_change.is_err() {
        log::debug!("{} encoder failed to reach PAUSED state", encoder_desc);
        cleanup_pipeline(&pipeline);
        return Err(anyhow!("State change to PAUSED failed"));
    }

    // Wait briefly for async state change
    std::thread::sleep(Duration::from_millis(200));

    // Check final state
    let (result, current, _pending) = pipeline.state(gstreamer::ClockTime::from_mseconds(100));
    if result.is_err() || current != gstreamer::State::Paused {
        log::debug!("{} encoder state check failed: {:?}, current: {:?}", encoder_desc, result, current);
        cleanup_pipeline(&pipeline);
        return Err(anyhow!("Failed to reach PAUSED state"));
    }

    // Clean up - this MUST happen to release VAAPI resources
    cleanup_pipeline(&pipeline);

    log::debug!("{} encoder test successful", encoder_desc);
    Ok(())
}

fn clear_bin(bin: &Element) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    // Clear the autogenerated ones
    for element in bin.iterate_elements().into_iter().flatten() {
        bin.remove(&element)?;
    }

    Ok(())
}

fn build_unknown(bin: &Element, pattern: &str) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Unknown Pipeline");
    let source = make_element("videotestsrc", "testvidsrc")?;
    source.set_property_from_str("pattern", pattern);
    source.set_property("num-buffers", 500i32); // Send buffers then EOS
    let queue = make_queue("queue0", 1024 * 1024 * 4)?;

    let overlay = make_element("textoverlay", "overlay")?;
    overlay.set_property("text", "Stream not Ready");
    overlay.set_property_from_str("valignment", "top");
    overlay.set_property_from_str("halignment", "left");
    overlay.set_property("font-desc", "Sans, 16");
    let encoder = make_element("jpegenc", "encoder")?;
    let payload = make_element("rtpjpegpay", "pay0")?;

    bin.add_many([&source, &queue, &overlay, &encoder, &payload])?;
    source.link_filtered(
        &queue,
        &Caps::builder("video/x-raw")
            .field("format", "YUY2")
            .field("width", 896i32)
            .field("height", 512i32)
            .field("framerate", gstreamer::Fraction::new(25, 1))
            .build(),
    )?;
    Element::link_many([&queue, &overlay, &encoder, &payload])?;

    Ok(())
}

struct Linked {
    appsrc: AppSrc,
    output: Element,
}

fn pipe_h264(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = buffer_size(stream_config.bitrate);
    log::debug!(
        "buffer_size: {buffer_size}, bitrate: {}",
        stream_config.bitrate
    );
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building H264 Pipeline");
    let source = make_element("appsrc", "vidsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(false);
    source.set_block(false);
    source.set_min_latency(1000 / (stream_config.fps as i64));
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(false);
    source.set_stream_type(AppStreamType::Stream);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;
    let queue = make_queue("source_queue", buffer_size)?;
    let parser = make_element("h264parse", "parser")?;
    // let stamper = make_element("h264timestamper", "stamper")?;

    bin.add_many([&source, &queue, &parser])?;
    Element::link_many([&source, &queue, &parser])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: parser,
    })
}

fn build_h264(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_h264(bin, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtph264pay", "pay0")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

fn pipe_h265(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = buffer_size(stream_config.bitrate);
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building H265 Pipeline");
    let source = make_element("appsrc", "vidsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;
    source.set_is_live(false);
    source.set_block(false);
    source.set_min_latency(1000 / (stream_config.fps as i64));
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(false);
    source.set_stream_type(AppStreamType::Stream);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;
    let queue = make_queue("source_queue", buffer_size)?;
    let parser = make_element("h265parse", "parser")?;
    // let stamper = make_element("h265timestamper", "stamper")?;

    bin.add_many([&source, &queue, &parser])?;
    Element::link_many([&source, &queue, &parser])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: parser,
    })
}

fn build_h265(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_h265(bin, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtph265pay", "pay0")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

/// Enum to specify which encoder to use for transcoding
#[derive(Debug, Clone, Copy)]
enum H264EncoderType {
    VAAPI,
    Software,
}

/// Cached VAAPI availability test result
/// Uses OnceLock for thread-safe lazy initialization without unsafe code
fn is_vaapi_available() -> bool {
    use std::sync::OnceLock;
    static VAAPI_AVAILABLE: OnceLock<bool> = OnceLock::new();

    *VAAPI_AVAILABLE.get_or_init(|| {
        log::debug!("Testing VAAPI availability (cached for future calls)");
        test_vaapi_encoder()
    })
}

fn build_h265_to_h264_transcode(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    log::info!("Building H.265 to H.264 transcoding pipeline");

    // Determine encoder selection based on transcode_device config
    let device_config = stream_config.transcode_device.as_ref()
        .map(|s| s.as_str())
        .unwrap_or("auto");

    log::debug!("Transcode device config: {}", device_config);

    // Parse the device configuration
    // Returns: (encoder_type, allow_fallback_to_software, optional_device_path)
    // Device path is used to specify a specific GPU device like /dev/dri/renderD128
    let (encoder_preference, allow_fallback, device_path) = match device_config {
        "auto" => {
            // Auto mode: Test VAAPI, use if available, fall back to software
            log::debug!("Auto-detecting encoder (VAAPI preferred, will fallback to software)");

            if is_vaapi_available() {
                (H264EncoderType::VAAPI, true, None)
            } else {
                (H264EncoderType::Software, false, None)
            }
        }
        "vaapi" => {
            // Force VAAPI mode: No fallback
            log::info!("Forced to use VAAPI encoder (no fallback to software)");
            (H264EncoderType::VAAPI, false, None)
        }
        "x264" | "software" => {
            // Force software mode: Skip VAAPI entirely
            log::info!("Forced to use software encoder (x264enc)");
            (H264EncoderType::Software, false, None)
        }
        path if path.starts_with("/dev/dri/") => {
            // Specific device path: Use VAAPI with custom device, no fallback
            log::info!("Using VAAPI with device path: {}", path);
            (H264EncoderType::VAAPI, false, Some(path.to_string()))
        }
        unknown => {
            // Unknown config: Warn and use auto mode
            log::warn!("Unknown transcode_device value '{}', using auto-detection", unknown);

            if is_vaapi_available() {
                (H264EncoderType::VAAPI, true, None)
            } else {
                (H264EncoderType::Software, false, None)
            }
        }
    };

    // Build the pipeline with the selected encoder
    log::info!("Attempting to build pipeline with {:?} encoder (fallback {})",
        encoder_preference, if allow_fallback { "enabled" } else { "disabled" });

    match build_h265_to_h264_transcode_with_encoder(bin, stream_config, encoder_preference, device_path.as_deref()) {
        Ok(source) => {
            log::info!("Transcoding pipeline built successfully with {:?} encoder", encoder_preference);
            Ok(source)
        }
        Err(e) if matches!(encoder_preference, H264EncoderType::VAAPI) && allow_fallback => {
            // VAAPI failed and fallback is allowed, try software encoding
            log::error!("VAAPI hardware transcoding failed: {}", e);
            log::error!("This usually means:");
            log::error!("  - VAAPI drivers are not properly installed");
            log::error!("  - The GPU device is not accessible (check /dev/dri/ permissions)");
            log::error!("  - GStreamer VAAPI plugins are missing (gstreamer-vaapi package)");
            log::info!("Falling back to software encoder (x264enc) - this will use more CPU");

            match build_h265_to_h264_transcode_with_encoder(bin, stream_config, H264EncoderType::Software, None) {
                Ok(source) => {
                    log::info!("Successfully fell back to software encoding");
                    Ok(source)
                }
                Err(e2) => {
                    log::error!("Software encoding (x264enc) also failed: {}", e2);
                    log::error!("This indicates a serious GStreamer configuration issue");
                    log::error!("Check that gst-plugins-ugly (x264enc) is installed");
                    Err(anyhow!("Both VAAPI and software transcoding failed. VAAPI: {}. Software: {}", e, e2))
                }
            }
        }
        Err(e) => {
            // Either not VAAPI, or VAAPI with no fallback allowed
            if matches!(encoder_preference, H264EncoderType::VAAPI) {
                log::error!("VAAPI hardware transcoding failed: {}", e);
                log::error!("VAAPI was forced (transcode_device = \"vaapi\" or specific device path)");
                log::error!("Troubleshooting steps:");
                log::error!("  1. Check VAAPI drivers: vainfo (should show supported profiles)");
                log::error!("  2. Check device permissions: ls -l /dev/dri/ (should be accessible)");
                log::error!("  3. Check GStreamer plugins: gst-inspect-1.0 vaapih264enc");
                log::error!("  4. Consider using transcode_device = \"auto\" to enable software fallback");
                if device_path.is_some() {
                    log::error!("  5. Verify the specified device path is correct");
                }
            } else {
                log::error!("Software transcoding pipeline failed: {}", e);
                log::error!("Check that gst-plugins-ugly (x264enc) is installed");
            }
            Err(e)
        }
    }
}

fn build_h265_to_h264_transcode_with_encoder(bin: &Element, stream_config: &StreamConfig, encoder_type: H264EncoderType, device_path: Option<&str>) -> Result<AppSrc> {
    let buffer_size = buffer_size(stream_config.bitrate);
    let bin_clone = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    log::debug!("Building H.265 to H.264 transcoding pipeline with {:?} encoder", encoder_type);

    // Create the appsrc for H.265 input
    let source = make_element("appsrc", "vidsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc"))?;

    source.set_is_live(false);
    source.set_block(false);
    source.set_min_latency(1000 / (stream_config.fps as i64));
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(false);
    source.set_stream_type(AppStreamType::Stream);

    // Set caps for H.265 bytestream input
    source.set_caps(Some(
        &Caps::builder("video/x-h265")
            .field("stream-format", "byte-stream")
            .build(),
    ));

    let source_element = source
        .clone()
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast to element"))?;

    // Build the transcoding pipeline: h265parse -> decoder -> videoconvert -> encoder -> h264parse -> rtph264pay
    let queue_in = make_queue("transcode_queue_in", buffer_size)?;
    let h265_parser = make_element("h265parse", "h265parser")?;

    // Try hardware decoders first for better performance, then fall back to software
    // Track whether we're using a VA-API decoder (outputs VA memory)
    let (decoder, is_vaapi_decoder) = {
        // Try VA-API 2.0 hardware decoder first
        if let Ok(dec) = make_element("vah265dec", "h265decoder") {
            log::info!("Using VA-API 2.0 hardware H.265 decoder: vah265dec");
            (dec, true)
        }
        // Try legacy VAAPI hardware decoder
        else if let Ok(dec) = make_element("vaapih265dec", "h265decoder") {
            log::info!("Using legacy VAAPI hardware H.265 decoder: vaapih265dec");
            (dec, true)
        }
        // Fall back to libav software decoder
        else if let Ok(dec) = make_element("avdec_h265", "h265decoder") {
            log::info!("Using libav software H.265 decoder: avdec_h265");
            (dec, false)
        }
        // Last resort: libde265 software decoder
        else {
            log::info!("Trying libde265 software H.265 decoder");
            (make_element("libde265dec", "h265decoder")?, false)
        }
    };

    // Build the format conversion pipeline based on decoder and encoder types
    // VA decoders output VA memory, software decoders output system memory
    // VA encoders need VA memory, software encoders need system memory
    //
    // Pipeline options:
    // 1. VAAPI decoder → VAAPI encoder: Use vapostproc (stays in VA memory)
    // 2. VAAPI decoder → Software encoder: Use vapostproc to download to system memory
    // 3. Software decoder → Software encoder: Use videoconvert (system memory)
    // 4. Software decoder → VAAPI encoder: Use videoconvert + upload (rare, but possible)

    // H.264 encoder - use the specified encoder type
    let encoder = match encoder_type {
        H264EncoderType::VAAPI => {
            log::debug!("Creating VAAPI hardware encoder");

            // Try new VA-API 2.0 encoder first (supports device property)
            // then fall back to old VAAPI encoder (doesn't support device property)
            let (enc, is_new_vaapi) = match make_element("vah264enc", "h264encoder") {
                Ok(enc) => {
                    log::info!("Using new VA-API 2.0 encoder (vah264enc)");
                    (enc, true)
                }
                Err(_) => {
                    log::info!("VA-API 2.0 encoder not available, using legacy VAAPI encoder (vaapih264enc)");
                    let enc = make_element("vaapih264enc", "h264encoder")
                        .map_err(|e| {
                            anyhow!("Failed to create VAAPI encoder element: {}. \
                            Check that gstreamer-vaapi package is installed and VAAPI drivers are available.", e)
                        })?;
                    (enc, false)
                }
            };

            // Set device path if specified (only supported by new VA-API 2.0)
            if let Some(device) = device_path {
                if is_new_vaapi {
                    log::info!("Configuring VA-API 2.0 encoder to use device: {}", device);
                    // The property name for vah264enc is "device-path" not "device"
                    enc.set_property("device-path", device);
                } else {
                    log::warn!("Device path '{}' specified but legacy VAAPI encoder doesn't support device selection", device);
                    log::warn!("The encoder will auto-detect the default device");
                    log::warn!("To use a specific device, install GStreamer VA-API 2.0 (vah264enc) or use environment variables:");
                    log::warn!("  LIBVA_DRIVER_NAME=iHD (for Intel iGPU)");
                    log::warn!("  LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri");
                }
            }

            // Validate encoder parameters before setting them
            let bitrate_kbps = stream_config.bitrate / 1000;
            let keyframe_period = stream_config.fps * 2;

            // Ensure bitrate is reasonable (at least 100 kbps, max 50 Mbps)
            let safe_bitrate = bitrate_kbps.clamp(100, 50000);
            if bitrate_kbps != safe_bitrate {
                log::warn!("VAAPI encoder: Adjusting bitrate from {} kbps to {} kbps", bitrate_kbps, safe_bitrate);
            }

            // Ensure keyframe period is reasonable (at least 10 frames, max 300)
            let safe_keyframe = keyframe_period.clamp(10, 300);
            if keyframe_period != safe_keyframe {
                log::warn!("VAAPI encoder: Adjusting keyframe-period from {} to {}", keyframe_period, safe_keyframe);
            }

            // Configure encoder (properties are similar between old and new VAAPI)
            if is_new_vaapi {
                // VA-API 2.0 properties
                enc.set_property_from_str("rate-control", "cbr");
                enc.set_property("bitrate", safe_bitrate);
                enc.set_property("key-int-max", safe_keyframe as u32);
                log::debug!("VA-API 2.0 encoder configured: bitrate={} kbps, key-int-max={}", safe_bitrate, safe_keyframe);
            } else {
                // Legacy VAAPI properties
                enc.set_property_from_str("rate-control", "cbr");
                enc.set_property("bitrate", safe_bitrate);
                enc.set_property("keyframe-period", safe_keyframe);
                log::debug!("Legacy VAAPI encoder configured: bitrate={} kbps, keyframe-period={}", safe_bitrate, safe_keyframe);
            }

            enc
        }
        H264EncoderType::Software => {
            log::debug!("Creating x264enc software encoder");
            let enc = make_element("x264enc", "h264encoder")
                .map_err(|e| anyhow!("Failed to create x264enc encoder: {}", e))?;

            // Configure x264enc for low latency and quality balance
            enc.set_property_from_str("tune", "zerolatency");
            enc.set_property_from_str("speed-preset", "medium");
            enc.set_property("bitrate", (stream_config.bitrate / 1024) as u32); // Convert to kbps
            enc.set_property("key-int-max", stream_config.fps * 2); // GOP size: 2 seconds

            log::debug!("x264enc configured: bitrate={} kbps, key-int-max={}",
                stream_config.bitrate / 1024, stream_config.fps * 2);
            enc
        }
    };

    // Create format conversion elements based on decoder and encoder types
    let conversion_elements: Vec<Element> = match (is_vaapi_decoder, encoder_type) {
        (true, H264EncoderType::VAAPI) => {
            // VAAPI → VAAPI: Use vapostproc to stay in VA memory
            log::debug!("Using vapostproc for VAAPI-to-VAAPI transcoding (VA memory only)");
            let vapostproc = make_element("vapostproc", "format_converter")?;
            vec![vapostproc]
        }
        (true, H264EncoderType::Software) => {
            // VAAPI → Software: Use vapostproc to download from VA memory to system memory
            log::debug!("Using vapostproc + videoconvert for VAAPI-to-software transcoding");
            let vapostproc = make_element("vapostproc", "va_downloader")?;
            let videoconvert = make_element("videoconvert", "format_converter")?;
            vec![vapostproc, videoconvert]
        }
        (false, H264EncoderType::VAAPI) => {
            // Software → VAAPI: Use videoconvert (GStreamer will auto-upload to VA memory if needed)
            log::debug!("Using videoconvert for software-to-VAAPI transcoding");
            let videoconvert = make_element("videoconvert", "format_converter")?;
            vec![videoconvert]
        }
        (false, H264EncoderType::Software) => {
            // Software → Software: Use videoconvert for format conversion
            log::debug!("Using videoconvert for software-to-software transcoding");
            let videoconvert = make_element("videoconvert", "format_converter")?;
            vec![videoconvert]
        }
    };

    let queue_out = make_queue("transcode_queue_out", buffer_size)?;
    let h264_parser = make_element("h264parse", "h264parser")?;
    let payload = make_element("rtph264pay", "pay0")?;

    // Configure rtph264pay for better compatibility
    payload.set_property("config-interval", -1i32); // Send SPS/PPS with every keyframe

    // Build element list for adding to bin
    let mut elements = vec![
        &source_element,
        &queue_in,
        &h265_parser,
        &decoder,
    ];

    // Add conversion elements
    let conversion_refs: Vec<&Element> = conversion_elements.iter().collect();
    elements.extend(conversion_refs.clone());

    // Add encoder and output elements
    elements.extend([
        &encoder,
        &queue_out,
        &h264_parser,
        &payload,
    ]);

    // Add all elements to the bin
    bin_clone.add_many(&elements)?;

    // Build linking list (same order as element list)
    let mut link_list = vec![
        &source_element,
        &queue_in,
        &h265_parser,
        &decoder,
    ];

    link_list.extend(conversion_refs);

    link_list.extend([
        &encoder,
        &queue_out,
        &h264_parser,
        &payload,
    ]);

    // Link the pipeline
    Element::link_many(&link_list).map_err(|e| {
        log::error!("Failed to link transcoding pipeline elements: {:?}", e);
        log::error!("Pipeline structure: decoder ({}) → conversion → encoder ({})",
            if is_vaapi_decoder { "VAAPI" } else { "software" },
            match encoder_type { H264EncoderType::VAAPI => "VAAPI", H264EncoderType::Software => "software" }
        );
        anyhow!("Pipeline linking failed: {:?}", e)
    })?;

    // Note: We CANNOT validate the pipeline here because this bin hasn't been added
    // to the RTSP server's pipeline yet. GStreamer bins only have a bus when they're
    // part of a top-level Pipeline element or already integrated into the pipeline hierarchy.
    //
    // Validation happens at two other stages:
    // 1. BEFORE construction: test_vaapi_encoder() tests if VAAPI works (lines 670-740)
    // 2. AFTER RTSP integration: check_live() verifies the appsrc after the RTSP server
    //    initializes the pipeline (called at line 267)
    //
    // GStreamer will also perform its own validation during state transitions (NULL->READY->PLAYING)
    // which happens after the RTSP server receives this bin.

    log::debug!("Transcoding pipeline constructed with {:?} encoder", encoder_type);
    log::debug!("Pipeline elements created and linked - validation will occur during RTSP server initialization");
    Ok(source)
}

fn pipe_aac(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    // Audio seems to run at about 800kbs
    let buffer_size = 512 * 1416;
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Aac pipeline");
    let source = make_element("appsrc", "audsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(false);
    source.set_block(false);
    source.set_min_latency(1000 / (stream_config.fps as i64));
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(false);
    source.set_stream_type(AppStreamType::Stream);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", buffer_size)?;
    let parser = make_element("aacparse", "audparser")?;
    let decoder = match make_element("faad", "auddecoder_faad") {
        Ok(ele) => Ok(ele),
        Err(_) => make_element("avdec_aac", "auddecoder_avdec_aac"),
    }?;

    // The fallback
    let silence = make_element("audiotestsrc", "audsilence")?;
    silence.set_property_from_str("wave", "silence");
    let fallback_switch = make_element("fallbackswitch", "audfallbackswitch");
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        fallback_switch.set_property("timeout", 3u64 * 1_000_000_000u64);
        fallback_switch.set_property("immediate-fallback", true);
    }

    let encoder = make_element("audioconvert", "audencoder")?;

    bin.add_many([&source, &queue, &parser, &decoder, &encoder])?;
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        bin.add_many([&silence, fallback_switch])?;
        Element::link_many([
            &source,
            &queue,
            &parser,
            &decoder,
            fallback_switch,
            &encoder,
        ])?;
        Element::link_many([&silence, fallback_switch])?;
    } else {
        Element::link_many([&source, &queue, &parser, &decoder, &encoder])?;
    }

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder,
    })
}

fn build_aac(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_aac(bin, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtpL16pay", "pay1")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

fn pipe_adpcm(bin: &Element, block_size: u32, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = 512 * 1416;
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Adpcm pipeline");
    // Original command line
    // caps=audio/x-adpcm,layout=dvi,block_align={},channels=1,rate=8000
    // ! queue silent=true max-size-bytes=10485760 min-threshold-bytes=1024
    // ! adpcmdec
    // ! audioconvert
    // ! rtpL16pay name=pay1

    let source = make_element("appsrc", "audsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;
    source.set_is_live(false);
    source.set_block(false);
    source.set_min_latency(1000 / (stream_config.fps as i64));
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(false);
    source.set_stream_type(AppStreamType::Stream);

    source.set_caps(Some(
        &Caps::builder("audio/x-adpcm")
            .field("layout", "div")
            .field("block_align", block_size as i32)
            .field("channels", 1i32)
            .field("rate", 8000i32)
            .build(),
    ));

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", buffer_size)?;
    let decoder = make_element("decodebin", "auddecoder")?;
    let encoder = make_element("audioconvert", "audencoder")?;
    let encoder_out = encoder.clone();

    bin.add_many([&source, &queue, &decoder, &encoder])?;
    Element::link_many([&source, &queue, &decoder])?;
    decoder.connect_pad_added(move |_element, pad| {
        let sink_pad = encoder
            .static_pad("sink")
            .expect("Encoder is missing its pad");
        pad.link(&sink_pad)
            .expect("Failed to link ADPCM decoder to encoder");
    });

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder_out,
    })
}

fn build_adpcm(bin: &Element, block_size: u32, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_adpcm(bin, block_size, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtpL16pay", "pay1")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

#[allow(dead_code)]
fn pipe_silence(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    // Audio seems to run at about 800kbs
    let buffer_size = 512 * 1416;
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Silence pipeline");
    let source = make_element("appsrc", "audsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(false);
    source.set_block(false);
    source.set_min_latency(1000 / (stream_config.fps as i64));
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(false);
    source.set_stream_type(AppStreamType::Stream);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let sink_queue = make_queue("audsinkqueue", buffer_size)?;
    let sink = make_element("fakesink", "silence_sink")?;

    let silence = make_element("audiotestsrc", "audsilence")?;
    silence.set_property_from_str("wave", "silence");
    let src_queue = make_queue("audsinkqueue", buffer_size)?;
    let encoder = make_element("audioconvert", "audencoder")?;

    bin.add_many([&source, &sink_queue, &sink, &silence, &src_queue, &encoder])?;

    Element::link_many([&source, &sink_queue, &sink])?;

    Element::link_many([&silence, &src_queue, &encoder])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder,
    })
}

#[allow(dead_code)]
struct AppSrcPair {
    vid: AppSrc,
    aud: Option<AppSrc>,
}

// #[allow(dead_code)]
// /// Experimental build a stream of MPEGTS
// fn build_mpegts(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrcPair> {
//     let buffer_size = buffer_size(stream_config.bitrate);
//     log::debug!(
//         "buffer_size: {buffer_size}, bitrate: {}",
//         stream_config.bitrate
//     );

//     // VID
//     let vid_link = match stream_config.vid_format {
//         VidFormat::H264 => pipe_h264(bin, stream_config)?,
//         VidFormat::H265 => pipe_h265(bin, stream_config)?,
//         VidFormat::None => unreachable!(),
//     };

//     // AUD
//     let aud_link = match stream_config.aud_format {
//         AudFormat::Aac => pipe_aac(bin, stream_config)?,
//         AudFormat::Adpcm(block) => pipe_adpcm(bin, block, stream_config)?,
//         AudFormat::None => pipe_silence(bin, stream_config)?,
//     };

//     let bin = bin
//         .clone()
//         .dynamic_cast::<Bin>()
//         .map_err(|_| anyhow!("Media source's element should be a bin"))?;

//     // MUX
//     let muxer = make_element("mpegtsmux", "mpeg_muxer")?;
//     let rtp = make_element("rtpmp2tpay", "pay0")?;

//     bin.add_many([&muxer, &rtp])?;
//     Element::link_many([&vid_link.output, &muxer, &rtp])?;
//     Element::link_many([&aud_link.output, &muxer])?;

//     Ok(AppSrcPair {
//         vid: vid_link.appsrc,
//         aud: Some(aud_link.appsrc),
//     })
// }

// Convenice funcion to make an element or provide a message
// about what plugin is missing
fn make_element(kind: &str, name: &str) -> AnyResult<Element> {
    ElementFactory::make_with_name(kind, Some(name)).with_context(|| {
        let plugin = match kind {
            "appsrc" => "app (gst-plugins-base)",
            "audioconvert" => "audioconvert (gst-plugins-base)",
            "videoconvert" => "videoconvert (gst-plugins-base)",
            "capsfilter" => "coreelements (gst-plugins-base)",
            "adpcmdec" => "Required for audio",
            "h264parse" => "videoparsersbad (gst-plugins-bad)",
            "h265parse" => "videoparsersbad (gst-plugins-bad)",
            "rtph264pay" => "rtp (gst-plugins-good)",
            "rtph265pay" => "rtp (gst-plugins-good)",
            "rtpjitterbuffer" => "rtp (gst-plugins-good)",
            "aacparse" => "audioparsers (gst-plugins-good)",
            "rtpL16pay" => "rtp (gst-plugins-good)",
            "x264enc" => "x264 (gst-plugins-ugly)",
            "x265enc" => "x265 (gst-plugins-bad)",
            "avdec_h264" => "libav (gst-libav)",
            "avdec_h265" => "libav (gst-libav)",
            "libde265dec" => "libde265 (gst-plugins-bad)",
            "vah264dec" => "va (gstreamer-va, VA-API 2.0)",
            "vah265dec" => "va (gstreamer-va, VA-API 2.0)",
            "vaapih265dec" => "vaapi (gstreamer-vaapi, legacy)",
            "vah264enc" => "va (gstreamer-va, VA-API 2.0)",
            "vah265enc" => "va (gstreamer-va, VA-API 2.0)",
            "vaapih264enc" => "vaapi (gstreamer-vaapi, legacy)",
            "vaapih265enc" => "vaapi (gstreamer-vaapi, legacy)",
            "vapostproc" => "va (gstreamer-va, VA-API 2.0 postprocessor)",
            "videotestsrc" => "videotestsrc (gst-plugins-base)",
            "imagefreeze" => "imagefreeze (gst-plugins-good)",
            "audiotestsrc" => "audiotestsrc (gst-plugins-base)",
            "decodebin" => "playback (gst-plugins-good)",
            _ => "Unknown",
        };
        format!(
            "Missing required gstreamer plugin `{}` for `{}` element",
            plugin, kind
        )
    })
}

#[allow(dead_code)]
fn make_dbl_queue(name: &str, buffer_size: u32) -> AnyResult<Element> {
    let queue = make_element("queue", &format!("queue1_{}", name))?;
    queue.set_property("max-size-bytes", buffer_size);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    // queue.set_property(
    //     "max-size-time",
    //     std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
    //         .unwrap_or(0),
    // );

    let queue2 = make_element("queue2", &format!("queue2_{}", name))?;
    queue2.set_property("max-size-bytes", buffer_size * 2u32 / 3u32);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue2.set_property(
        "max-size-time",
        std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
            .unwrap_or(0),
    );
    queue2.set_property("use-buffering", false);

    let bin = gstreamer::Bin::builder().name(name).build();
    bin.add_many([&queue, &queue2])?;
    Element::link_many([&queue, &queue2])?;

    let pad = queue
        .static_pad("sink")
        .expect("Failed to get a static pad from queue.");
    let ghost_pad = GhostPad::builder_with_target(&pad).unwrap().build();
    ghost_pad.set_active(true)?;
    bin.add_pad(&ghost_pad)?;

    let pad = queue2
        .static_pad("src")
        .expect("Failed to get a static pad from queue2.");
    let ghost_pad = GhostPad::builder_with_target(&pad).unwrap().build();
    ghost_pad.set_active(true)?;
    bin.add_pad(&ghost_pad)?;

    let bin = bin
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot convert bin"))?;
    Ok(bin)
}

fn make_queue(name: &str, buffer_size: u32) -> AnyResult<Element> {
    let queue = make_element("queue", &format!("queue1_{}", name))?;
    queue.set_property("max-size-bytes", buffer_size);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property(
        "max-size-time",
        std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
            .unwrap_or(0),
    );
    Ok(queue)
}

fn buffer_size(bitrate: u32) -> u32 {
    // 0.1 seconds (according to bitrate) or 4kb what ever is larger
    std::cmp::max(bitrate * 2 / 8u32, 4u32 * 1024u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_buffer_pool_success() {
        gstreamer::init().ok();
        let result = create_buffer_pool(1024);
        assert!(result.is_ok(), "Buffer pool creation should succeed");
        let pool = result.unwrap();
        assert!(pool.is_active(), "Pool should be active");
    }

    #[test]
    fn test_create_buffer_pool_various_sizes() {
        gstreamer::init().ok();
        // Test various sizes
        for size in [512, 1024, 4096, 8192, 16384] {
            let result = create_buffer_pool(size);
            assert!(
                result.is_ok(),
                "Buffer pool creation should succeed for size {}",
                size
            );
        }
    }

    #[test]
    fn test_buffer_size_calculation() {
        // Test minimum size
        assert_eq!(buffer_size(0), 4 * 1024);
        assert_eq!(buffer_size(1000), 4 * 1024);

        // Test bitrate-based size
        assert_eq!(buffer_size(1000000), 250000); // 1 Mbps
        assert_eq!(buffer_size(5000000), 1250000); // 5 Mbps
    }

    #[test]
    fn test_audio_type_variants() {
        let aac = AudioType::Aac;
        let adpcm = AudioType::Adpcm(512);

        // Ensure variants can be created and matched
        match aac {
            AudioType::Aac => {}
            _ => panic!("Expected Aac variant"),
        }

        match adpcm {
            AudioType::Adpcm(512) => {}
            _ => panic!("Expected Adpcm(512) variant"),
        }
    }

    #[test]
    fn test_stream_config_fps_update() {
        let mut config = StreamConfig {
            resolution: [1920, 1080],
            bitrate: 2000000,
            fps: 25,
            fps_table: vec![15, 20, 25, 30],
            bitrate_table: vec![1000000, 2000000, 3000000],
            vid_type: None,
            aud_type: None,
            transcode_to: None,
            transcode_device: None,
        };

        // Test FPS update from table
        config.update_fps(2);
        assert_eq!(config.fps, 25);

        // Test FPS update with out-of-bounds index
        config.update_fps(10);
        assert_eq!(config.fps, 10); // Should use the index value directly
    }

    #[test]
    fn test_stream_config_bitrate_update() {
        let mut config = StreamConfig {
            resolution: [1920, 1080],
            bitrate: 2000000,
            fps: 25,
            fps_table: vec![15, 20, 25, 30],
            bitrate_table: vec![1000000, 2000000, 3000000],
            vid_type: None,
            aud_type: None,
            transcode_to: None,
            transcode_device: None,
        };

        // Test bitrate update from table
        config.update_bitrate(1);
        assert_eq!(config.bitrate, 2000000);

        // Test bitrate update with out-of-bounds index
        config.update_bitrate(10);
        assert_eq!(config.bitrate, 10); // Should use the index value directly
    }

    #[cfg(feature = "gstreamer")]
    #[tokio::test]
    async fn test_make_dummy_factory() {
        gstreamer::init().ok();
        let result = make_dummy_factory(false, "smpte".to_string()).await;
        assert!(result.is_ok(), "Dummy factory creation should succeed");
    }

    #[cfg(feature = "gstreamer")]
    #[tokio::test]
    async fn test_make_dummy_factory_with_splash() {
        gstreamer::init().ok();
        let result = make_dummy_factory(true, "snow".to_string()).await;
        assert!(result.is_ok(), "Dummy factory with splash should succeed");
    }
}
