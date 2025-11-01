use std::sync::{Arc, Weak};
use tokio::{
    sync::watch::{Receiver as WatchReceiver, Sender as WatchSender},
    time::{interval, sleep, timeout, Duration, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::{config::CameraConfig, utils::connect_and_login, AnyResult};
use neolink_core::bc_protocol::BcCamera;

#[derive(Eq, PartialEq, Copy, Clone)]
pub(crate) enum NeoCamThreadState {
    Connected,
    Disconnected,
}

pub(crate) struct NeoCamThread {
    state: WatchReceiver<NeoCamThreadState>,
    config: WatchReceiver<CameraConfig>,
    cancel: CancellationToken,
    camera_watch: WatchSender<Weak<BcCamera>>,
}

impl NeoCamThread {
    pub(crate) async fn new(
        watch_state_rx: WatchReceiver<NeoCamThreadState>,
        watch_config_rx: WatchReceiver<CameraConfig>,
        camera_watch_tx: WatchSender<Weak<BcCamera>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            state: watch_state_rx,
            config: watch_config_rx,
            cancel,
            camera_watch: camera_watch_tx,
        }
    }
    async fn run_camera(&mut self, config: &CameraConfig) -> AnyResult<()> {
        let name = config.name.clone();
        log::trace!("Attempting connection with config: {config:?}");
        let camera = Arc::new(connect_and_login(config).await?);
        log::trace!("  - Connected");

        sleep(Duration::from_secs(2)).await; // Delay a little since some calls will error if camera is waking up
        if let Err(e) = update_camera_time(&camera, &name, config.update_time).await {
            log::warn!("Could not set camera time, (perhaps missing on this camera of your login in not an admin): {e:?}");
        }
        sleep(Duration::from_secs(2)).await; // Delay a little since some calls will error if camera is waking up

        self.camera_watch.send_replace(Arc::downgrade(&camera));

        // Spawn active UDP keepalive task if enabled
        let keepalive_cancel = self.cancel.clone();
        let keepalive_camera = camera.clone();
        let keepalive_name = name.clone();
        let keepalive_interval_secs = config.keepalive_interval;
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(keepalive_interval_secs));
            loop {
                tokio::select! {
                    _ = keepalive_cancel.cancelled() => {
                        break;
                    }
                    _ = interval.tick() => {
                        log::trace!("{}: Sending active UDP keepalive", keepalive_name);
                        if let Err(e) = keepalive_camera.send_keepalive().await {
                            log::trace!("{}: Keepalive send failed (non-fatal): {:?}", keepalive_name, e);
                        }
                    }
                }
            }
        });

        let cancel_check = self.cancel.clone();
        // Now we wait for a disconnect
        tokio::select! {
            _ = cancel_check.cancelled() => {
                AnyResult::Ok(())
            }
            v = camera.join() => {
                v?;
                Ok(())
            },
            v = async {
                let mut interval = interval(Duration::from_secs(5));
                let mut missed_pings = 0;
                let max_retries = config.ping_retries;
                let mut ping_times = Vec::new();
                let mut total_pings = 0u64;
                let mut failed_pings = 0u64;
                let metrics_log_interval = 100; // Log metrics every 100 pings

                loop {
                    interval.tick().await;
                    log::trace!("Sending ping");
                    let start = Instant::now();

                    match timeout(Duration::from_secs(5), camera.get_linktype()).await {
                        Ok(Ok(_)) => {
                            let response_time = start.elapsed();
                            log::trace!("Ping reply in {:?}", response_time);

                            missed_pings = 0;
                            total_pings += 1;

                            // Track response times for metrics
                            ping_times.push(response_time.as_millis() as u64);
                            if ping_times.len() > 100 {
                                ping_times.remove(0); // Keep only last 100
                            }

                            // Log connection quality metrics periodically
                            if total_pings % metrics_log_interval == 0 {
                                if !ping_times.is_empty() {
                                    let avg_ms = ping_times.iter().sum::<u64>() / ping_times.len() as u64;
                                    let max_ms = ping_times.iter().max().unwrap_or(&0);
                                    let success_rate = ((total_pings - failed_pings) as f64 / total_pings as f64) * 100.0;
                                    log::debug!("{}: Connection quality: avg ping {}ms, max {}ms, success {:.1}%",
                                        name, avg_ms, max_ms, success_rate);
                                }
                            }

                            continue
                        },
                        Ok(Err(neolink_core::Error::UnintelligibleReply { reply, why })) => {
                            // Camera does not support pings just wait forever
                            log::trace!("Pings not supported: {reply:?}: {why}");
                            futures::future::pending().await
                        },
                        Ok(Err(e)) => {
                            failed_pings += 1;
                            break Err(e.into());
                        },
                        Err(_) => {
                            // Timeout
                            failed_pings += 1;
                            if missed_pings < max_retries {
                                missed_pings += 1;
                                log::warn!("{}: Ping timeout ({}/{})", name, missed_pings, max_retries);
                                continue;
                            } else {
                                log::error!("{}: Connection lost after {} consecutive ping timeouts", name, max_retries);
                                break Err(anyhow::anyhow!("Timed out waiting for camera ping reply"));
                            }
                        }
                    }
                }
            } => v,
        }?;

        let _ = camera.logout().await;
        let _ = camera.shutdown().await;

        Ok(())
    }

    // Will run and attempt to maintain the connection
    //
    // A watch sender is used to send the new camera
    // whenever it changes
    pub(crate) async fn run(&mut self) -> AnyResult<()> {
        const MAX_BACKOFF: Duration = Duration::from_secs(5);
        const MIN_BACKOFF: Duration = Duration::from_millis(50);

        let mut backoff = MIN_BACKOFF;

        loop {
            self.state
                .clone()
                .wait_for(|state| matches!(state, NeoCamThreadState::Connected))
                .await?;
            let mut config_rec = self.config.clone();

            let config = config_rec.borrow_and_update().clone();
            let now = Instant::now();
            let name = config.name.clone();

            let mut state = self.state.clone();

            let res = tokio::select! {
                Ok(_) = config_rec.changed() => {
                    None
                }
                Ok(_) = state.wait_for(|state| matches!(state, NeoCamThreadState::Disconnected)) => {
                    log::trace!("State changed to disconnect");
                    None
                }
                v = self.run_camera(&config) => {
                    Some(v)
                }
            };
            self.camera_watch.send_replace(Weak::new());

            if res.is_none() {
                // If None go back and reload NOW
                //
                // This occurs if there was a config change
                log::trace!("Config change or Manual disconnect");
                continue;
            }

            // Else we see what the result actually was
            let result = res.unwrap();

            if now.elapsed() > Duration::from_secs(60) {
                // Command ran long enough to be considered a success
                backoff = MIN_BACKOFF;
            }
            if backoff > MAX_BACKOFF {
                backoff = MAX_BACKOFF;
            }

            match result {
                Ok(()) => {
                    // Normal shutdown
                    log::trace!("Normal camera shutdown");
                    self.cancel.cancel();
                    return Ok(());
                }
                Err(e) => {
                    // An error
                    // Check if it is non-retry
                    let e_inner = e.downcast_ref::<neolink_core::Error>();
                    match e_inner {
                        Some(neolink_core::Error::CameraLoginFail) => {
                            // Fatal
                            log::error!("{name}: Login credentials were not accepted");
                            self.cancel.cancel();
                            return Err(e);
                        }
                        _ => {
                            // Non fatal
                            log::warn!("{name}: Connection Lost: {:?}", e);
                            log::info!("{name}: Attempt reconnect in {:?}", backoff);
                            sleep(backoff).await;
                            backoff *= 2;
                        }
                    }
                }
            }
        }
    }
}

impl Drop for NeoCamThread {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn update_camera_time(camera: &BcCamera, name: &str, update_time: bool) -> AnyResult<()> {
    let cam_time = camera.get_time().await?;
    let mut update = false;
    if let Some(time) = cam_time {
        log::info!("{}: Camera time is already set: {}", name, time);
        if update_time {
            update = true;
        }
    } else {
        update = true;
        log::warn!("{}: Camera has no time set, Updating", name);
    }
    if update {
        use std::time::SystemTime;
        let new_time = SystemTime::now();

        log::info!("{}: Setting time to {:?}", name, new_time);
        match camera.set_time(new_time.into()).await {
            Ok(_) => {
                let cam_time = camera.get_time().await?;
                if let Some(time) = cam_time {
                    log::info!("{}: Camera time is now set: {}", name, time);
                }
            }
            Err(e) => {
                log::error!(
                    "{}: Camera did not accept new time (is user an admin?): Error: {:?}",
                    name,
                    e
                );
            }
        }
    }
    Ok(())
}
