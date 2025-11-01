///
/// # Neolink RTSP
///
/// This module serves the rtsp streams for the
/// `neolink rtsp` subcommand
///
/// All camera specified in the config.toml will be served
/// over rtsp. By default it bind to all local ip addresses
/// on the port 8554.
///
/// You can view the streams with any rtsp compliement program
/// such as ffmpeg, vlc, blue-iris, home-assistant, zone-minder etc.
///
/// Each camera has it own endpoint based on its name. For example
/// a camera named `"Garage"` in the config can be found at.
///
/// `rtsp://my.ip.address:8554/Garage`
///
/// With the lower resolution stream at
///
/// `rtsp://my.ip.address:8554/Garage/subStream`
///
/// # Usage
///
/// To start the subcommand use the following in a shell.
///
/// ```bash
/// neolink rtsp --config=config.toml
/// ```
///
/// # Example Config
///
/// ```toml
// [[cameras]]
// name = "Cammy"
// username = "****"
// password = "****"
// address = "****:9000"
//   [cameras.pause]
//   on_motion = false
//   on_client = false
//   mode = "none"
//   timeout = 1.0
// ```
//
// - When `on_motion` is true the camera will pause streaming when motion is stopped and resume it when motion is started
// - When `on_client` is true the camera will pause while there is no client connected.
// - `timeout` handels how long to wait after motion stops before pausing the stream
// - `mode` has the following values:
//   - `"black"`: Switches to a black screen. Requires more cpu as the stream is fully reencoded
//   - `"still"`: Switches to a still image. Requires more cpu as the stream is fully reencoded
//   - `"test"`: Switches to the gstreamer test image. Requires more cpu as the stream is fully reencoded
//   - `"none"`: Resends the last iframe the camera. This does not reencode at all.  **Most use cases should use this one as it has the least effort on the cpu and gives what you would expect**
//
use anyhow::{Context, Result};
use log::*;
use neolink_core::bc_protocol::StreamKind;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::{
    sync::watch::channel as watch,
    task::JoinSet,
    time::{interval, Duration},
};
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

mod cmdline;
mod ffmpeg;
mod mediamtx;
mod stream;

// Keep old modules for now during migration
#[cfg(feature = "gstreamer")]
mod factory;
#[cfg(feature = "gstreamer")]
mod gst;

// GStreamer imports only needed for legacy code
#[cfg(feature = "gstreamer")]
use gstreamer_rtsp_server::prelude::*;

use crate::common::{NeoInstance, NeoReactor};
use stream::*;

pub(crate) use cmdline::Opt;

// Use MediaMTX client instead of GStreamer RTSP server
use mediamtx::MediaMtxClient;

type AnyResult<T> = anyhow::Result<T, anyhow::Error>;

/// Entry point for the rtsp subcommand
///
/// Opt is the command line options
pub(crate) async fn main(_opt: Opt, reactor: NeoReactor) -> Result<()> {
    let global_cancel = CancellationToken::new();
    let mut set = JoinSet::new();

    // Get configuration for MediaMTX and FFmpeg
    let config = reactor.config().await?.borrow().clone();
    let mediamtx_api_url = config.mediamtx_api_url.clone();
    let mediamtx_rtsp_url = config.mediamtx_rtsp_url.clone();
    let ffmpeg_path = config.ffmpeg_path.clone();

    info!("Initializing RTSP streaming with MediaMTX");
    info!("MediaMTX API: {}", mediamtx_api_url);
    info!("MediaMTX RTSP: {}", mediamtx_rtsp_url);
    info!("FFmpeg path: {}", ffmpeg_path);

    // Create MediaMTX client
    let mediamtx = Arc::new(MediaMtxClient::new(
        mediamtx_api_url,
        mediamtx_rtsp_url,
    ));

    // Health check MediaMTX
    info!("Performing MediaMTX health check...");
    match mediamtx.health_check().await {
        Ok(_) => info!("MediaMTX is accessible and running"),
        Err(e) => {
            error!("MediaMTX health check failed: {}", e);
            error!("Please ensure MediaMTX is running and accessible");
            error!("You can start MediaMTX with: mediamtx");
            error!("Or install it from: https://github.com/bluenviron/mediamtx");
            return Err(e);
        }
    }

    // Startup and stop cameras as they are added/removed to the config
    let mut thread_config = reactor.config().await?;
    let thread_cancel = global_cancel.clone();
    let thread_mediamtx = mediamtx.clone();
    let thread_reactor = reactor.clone();
    let thread_ffmpeg_path = ffmpeg_path.clone();
    set.spawn(async move {
        let mut set = JoinSet::<AnyResult<()>>::new();
        let thread_cancel2 = thread_cancel.clone();
        tokio::select!{
            _ = thread_cancel.cancelled() => AnyResult::Ok(()),
            v = async {
                let mut cameras: HashMap<String, CancellationToken> = Default::default();
                let mut config_names = HashSet::new();
                loop {
                    config_names = thread_config.wait_for(|config| {
                        let current_names = config.cameras.iter().filter(|a| a.enabled).map(|cam_config| cam_config.name.clone()).collect::<HashSet<_>>();
                        current_names != config_names
                    }).await.with_context(|| "Camera Config Watcher")?.clone().cameras.iter().filter(|a| a.enabled).map(|cam_config| cam_config.name.clone()).collect::<HashSet<_>>();

                    for name in config_names.iter() {
                        if ! cameras.contains_key(name) {
                            log::info!("{name}: RTSP Starting");
                            let local_cancel = CancellationToken::new();
                            cameras.insert(name.clone(),local_cancel.clone() );
                            let thread_global_cancel = thread_cancel2.clone();
                            let thread_mediamtx2 = thread_mediamtx.clone();
                            let thread_reactor2 = thread_reactor.clone();
                            let thread_ffmpeg_path2 = thread_ffmpeg_path.clone();
                            let name = name.clone();
                            set.spawn(async move {
                                let camera = thread_reactor2.get(&name).await?;
                                tokio::select!(
                                    _ = thread_global_cancel.cancelled() => {
                                        AnyResult::Ok(())
                                    },
                                    _ = local_cancel.cancelled() => {
                                        AnyResult::Ok(())
                                    },
                                    v = camera_main(camera, &thread_mediamtx2, &thread_ffmpeg_path2) => v,
                                )
                            }) ;
                        }
                    }

                    for (running_name, token) in cameras.iter() {
                        if ! config_names.contains(running_name) {
                            token.cancel();
                        }
                    }
                }
            } => v,
        }
    });

    info!("RTSP streaming service started successfully");
    info!("Streams will be published to MediaMTX and available via RTSP");
    info!("Access cameras at: {}/CameraName", mediamtx.get_publish_url("").trim_end_matches('/'));

    // Wait for all tasks to complete or error
    while let Some(joined) = set
        .join_next()
        .await
        .map(|s| s.map_err(anyhow::Error::from))
    {
        match &joined {
            Err(e) | Ok(Err(e)) => {
                // Panicked or error in task
                // Cancel all and await terminate
                log::error!("Error: {e}");
                global_cancel.cancel();
            }
            Ok(Ok(_)) => {
                // Task completed successfully
            }
        }
    }

    info!("RTSP streaming service shutting down...");
    info!("All FFmpeg processes have been terminated");
    info!("RTSP service shutdown complete");

    Ok(())
}

/// Top level camera entry point
///
/// It checks which streams are supported and then starts them
async fn camera_main(camera: NeoInstance, mediamtx: &MediaMtxClient, ffmpeg_path: &str) -> Result<()> {
    let name = camera.config().await?.borrow().name.clone();
    log::debug!("{name}: Camera Main");
    let later_camera = camera.clone();
    let (supported_streams_tx, supported_streams) = watch(HashSet::<StreamKind>::new());

    let mut set = JoinSet::new();

    // Spawn a task to periodically check supported streams
    set.spawn(async move {
        let mut i = IntervalStream::new(interval(Duration::from_secs(15)));
        while i.next().await.is_some() {
            let stream_info = later_camera
                .run_passive_task(|cam| Box::pin(async move { Ok(cam.get_stream_info().await?) }))
                .await?;

            let new_supported_streams = stream_info
                .stream_infos
                .iter()
                .flat_map(|stream_info| stream_info.encode_tables.clone())
                .flat_map(|encode| match encode.name.as_str() {
                    "mainStream" => Some(StreamKind::Main),
                    "subStream" => Some(StreamKind::Sub),
                    "externStream" => Some(StreamKind::Extern),
                    new_stream_name => {
                        log::debug!("New stream name {}", new_stream_name);
                        None
                    }
                })
                .collect::<HashSet<_>>();
            supported_streams_tx.send_if_modified(|old| {
                if *old != new_supported_streams {
                    *old = new_supported_streams;
                    true
                } else {
                    false
                }
            });
        }
        AnyResult::Ok(())
    });

    let mut camera_config = camera.config().await?.clone();
    loop {
        let prev_stream_config = camera_config.borrow_and_update().stream;
        let active_streams = prev_stream_config
            .as_stream_kinds()
            .drain(..)
            .collect::<HashSet<_>>();

        // This select is for changes to camera_config.stream
        break tokio::select! {
            v = camera_config.wait_for(|config| config.stream != prev_stream_config) => {
                if let Err(e) = v {
                    AnyResult::Err(e.into())
                } else {
                    // config.stream changed restart
                    continue;
                }
            },
            v = async {
                // This select handles enabling the right stream
                let mut supported_streams_1 = supported_streams.clone();
                let mut supported_streams_2 = supported_streams.clone();
                let mut supported_streams_3 = supported_streams.clone();

                tokio::select! {
                    v = async {
                        let name = camera.config().await?.borrow().name.clone();
                        let mut paths = vec![
                            format!("/{name}/main"),
                            format!("/{name}/Main"),
                            format!("/{name}/mainStream"),
                            format!("/{name}/MainStream"),
                            format!("/{name}/Mainstream"),
                            format!("/{name}/mainstream"),
                        ];
                        paths.push(format!("/{name}"));

                        log::debug!("{}: Preparing main stream at {}", name, paths.join(", "));

                        // Wait for stream to be supported
                        supported_streams_1.wait_for(|ss| ss.contains(&StreamKind::Main)).await?;
                        stream_main(camera.clone(), StreamKind::Main, mediamtx, ffmpeg_path, &paths).await
                    }, if active_streams.contains(&StreamKind::Main) => v,

                    v = async {
                        let name = camera.config().await?.borrow().name.clone();
                        let mut paths = vec![
                            format!("/{name}/sub"),
                            format!("/{name}/Sub"),
                            format!("/{name}/subStream"),
                            format!("/{name}/SubStream"),
                            format!("/{name}/Substream"),
                            format!("/{name}/substream"),
                        ];
                        if !active_streams.contains(&StreamKind::Main) {
                            paths.push(format!("/{name}"));
                        }

                        log::debug!("{}: Preparing sub stream at {}", name, paths.join(", "));

                        // Wait for stream to be supported
                        supported_streams_2.wait_for(|ss| ss.contains(&StreamKind::Sub)).await?;
                        stream_main(camera.clone(), StreamKind::Sub, mediamtx, ffmpeg_path, &paths).await
                    }, if active_streams.contains(&StreamKind::Sub) => v,

                    v = async {
                        let name = camera.config().await?.borrow().name.clone();
                        let mut paths = vec![
                            format!("/{name}/extern"),
                            format!("/{name}/Extern"),
                            format!("/{name}/externStream"),
                            format!("/{name}/ExternStream"),
                            format!("/{name}/Externstream"),
                            format!("/{name}/externstream"),
                        ];
                        if !active_streams.contains(&StreamKind::Main) && !active_streams.contains(&StreamKind::Sub) {
                            paths.push(format!("/{name}"));
                        }

                        log::debug!("{}: Preparing extern stream at {}", name, paths.join(", "));

                        // Wait for stream to be supported
                        supported_streams_3.wait_for(|ss| ss.contains(&StreamKind::Extern)).await?;
                        stream_main(camera.clone(), StreamKind::Extern, mediamtx, ffmpeg_path, &paths).await
                    }, if active_streams.contains(&StreamKind::Extern) => v,

                    else => {
                        // all disabled just wait here until config is changed
                        futures::future::pending().await
                    }
                }
            } => v,
        };
    }?;

    Ok(())
}
