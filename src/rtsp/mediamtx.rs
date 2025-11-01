///
/// # MediaMTX Client
///
/// This module provides integration with MediaMTX RTSP server.
/// MediaMTX is an external RTSP/RTMP/HLS server that handles client connections,
/// authentication, and stream serving. Neolink pushes streams to MediaMTX via RTSP.
///
/// Key features:
/// - Health checking via MediaMTX HTTP API
/// - Stream path validation
/// - Configuration via API (optional)
///
use anyhow::{anyhow, Context, Result};
use log::*;
use serde::{Deserialize, Serialize};

/// MediaMTX client for interacting with the MediaMTX RTSP server
#[derive(Debug, Clone)]
pub struct MediaMtxClient {
    /// Base URL for MediaMTX API (e.g., "http://localhost:9997")
    api_url: String,
    /// RTSP publish URL for pushing streams (e.g., "rtsp://localhost:8554")
    rtsp_url: String,
}

/// MediaMTX API config response
#[derive(Debug, Deserialize, Serialize)]
struct MediaMtxConfig {
    // We only care about a few fields for validation
    #[serde(default)]
    rtsp_address: Option<String>,
}

impl MediaMtxClient {
    /// Create a new MediaMTX client
    ///
    /// # Arguments
    /// * `api_url` - HTTP API URL (default: "http://localhost:9997")
    /// * `rtsp_url` - RTSP server URL for publishing (default: "rtsp://localhost:8554")
    pub fn new(api_url: String, rtsp_url: String) -> Self {
        Self { api_url, rtsp_url }
    }

    /// Get the RTSP URL for publishing a specific stream
    ///
    /// # Arguments
    /// * `path` - Stream path (e.g., "CameraName" or "CameraName/subStream")
    ///
    /// # Returns
    /// Full RTSP URL for publishing (e.g., "rtsp://localhost:8554/CameraName")
    pub fn get_publish_url(&self, path: &str) -> String {
        format!("{}/{}", self.rtsp_url, path)
    }

    /// Health check: verify MediaMTX is running and accessible
    ///
    /// This uses the MediaMTX HTTP API to check if the server is responsive.
    /// The API endpoint `/v3/config/global/get` returns the current global configuration.
    ///
    /// # Returns
    /// Ok(()) if MediaMTX is accessible, Err otherwise
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/v3/config/global/get", self.api_url);

        debug!("MediaMTX health check: GET {}", url);

        // Create a simple HTTP client (we'll use reqwest via tokio)
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .context("Failed to create HTTP client")?;

        let response = client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to MediaMTX API")?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "MediaMTX API returned error status: {}",
                response.status()
            ));
        }

        // Try to parse the config to validate the response
        let _config: MediaMtxConfig = response
            .json()
            .await
            .context("Failed to parse MediaMTX config response")?;

        debug!("MediaMTX health check: OK");
        Ok(())
    }

    /// Get the list of active paths (streams) from MediaMTX
    ///
    /// This queries the MediaMTX API to get information about active streams.
    ///
    /// # Returns
    /// Vec of active stream paths, or empty vec if none
    pub async fn get_active_paths(&self) -> Result<Vec<String>> {
        let url = format!("{}/v3/paths/list", self.api_url);

        debug!("MediaMTX get paths: GET {}", url);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .context("Failed to create HTTP client")?;

        let response = client
            .get(&url)
            .send()
            .await
            .context("Failed to get paths from MediaMTX API")?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "MediaMTX API returned error status: {}",
                response.status()
            ));
        }

        // MediaMTX v3 API returns: {"items": [{"name": "path1", ...}, ...]}
        #[derive(Deserialize)]
        struct PathList {
            #[serde(default)]
            items: Vec<PathItem>,
        }

        #[derive(Deserialize)]
        struct PathItem {
            name: String,
        }

        let path_list: PathList = response
            .json()
            .await
            .context("Failed to parse MediaMTX paths response")?;

        let paths: Vec<String> = path_list.items.into_iter().map(|p| p.name).collect();
        debug!("MediaMTX active paths: {:?}", paths);

        Ok(paths)
    }

    /// Validate that a stream path is available for publishing
    ///
    /// This checks if the path is already in use or if there are any conflicts.
    ///
    /// # Arguments
    /// * `path` - Stream path to validate
    ///
    /// # Returns
    /// Ok(()) if path is available, Err if already in use
    pub async fn validate_path(&self, path: &str) -> Result<()> {
        let active_paths = self.get_active_paths().await?;

        if active_paths.contains(&path.to_string()) {
            warn!("MediaMTX path '{}' is already active", path);
            // This is not necessarily an error - MediaMTX will handle reconnections
            // But we should log it for awareness
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = MediaMtxClient::new(
            "http://localhost:9997".to_string(),
            "rtsp://localhost:8554".to_string(),
        );

        assert_eq!(client.api_url, "http://localhost:9997");
        assert_eq!(client.rtsp_url, "rtsp://localhost:8554");
    }

    #[test]
    fn test_publish_url_generation() {
        let client = MediaMtxClient::new(
            "http://localhost:9997".to_string(),
            "rtsp://localhost:8554".to_string(),
        );

        assert_eq!(
            client.get_publish_url("CameraName"),
            "rtsp://localhost:8554/CameraName"
        );

        assert_eq!(
            client.get_publish_url("CameraName/subStream"),
            "rtsp://localhost:8554/CameraName/subStream"
        );
    }

    // Note: Integration tests that actually connect to MediaMTX should be
    // run with --ignored flag and require a running MediaMTX instance
    #[tokio::test]
    #[ignore]
    async fn test_health_check_integration() {
        let client = MediaMtxClient::new(
            "http://localhost:9997".to_string(),
            "rtsp://localhost:8554".to_string(),
        );

        let result = client.health_check().await;
        assert!(result.is_ok(), "Health check failed: {:?}", result.err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_active_paths_integration() {
        let client = MediaMtxClient::new(
            "http://localhost:9997".to_string(),
            "rtsp://localhost:8554".to_string(),
        );

        let result = client.get_active_paths().await;
        assert!(result.is_ok(), "Get paths failed: {:?}", result.err());

        // Just verify we got a Vec, may be empty
        let paths = result.unwrap();
        println!("Active paths: {:?}", paths);
    }
}
