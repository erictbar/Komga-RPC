use discord_rich_presence::{activity, DiscordIpcClient, DiscordIpc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::Duration;
use tokio::time;
use reqwest::Client;
use std::env;
use std::time::SystemTime;
use log::{info, error, warn};
use env_logger;
use std::io::ErrorKind;
use std::collections::HashMap;
use chrono::{DateTime, Utc};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct Config {
    discord_client_id: String,
    komga_url: String,
    komga_api_key: String,
    show_progress: Option<bool>,
    use_imgur_cover: Option<bool>,
    imgur_client_id: Option<String>,
    exclude_libraries: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Library {
    id: String,
    name: String,
    #[serde(rename = "type")]
    lib_type: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct Series {
    id: String,
    title: Option<String>,
    authors: Option<Vec<SeriesAuthor>>,
    #[serde(rename = "processingStatus")]
    processing_status: Option<ProcessingStatusObject>,
}

#[derive(Debug, Deserialize, Clone)]
struct ProcessingStatusObject {
    #[serde(rename = "currentTask")]
    current_task: String,
    progress: f64,
    status: ProcessingStatus,
}

#[derive(Debug, Deserialize, Clone)]
struct SeriesAuthor {
    name: String,
    #[serde(rename = "fileAs")]
    file_as: Option<String>,
    role: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
enum ProcessingStatus {
    Uploaded,
    Processing,
    #[serde(rename = "COMPLETED")]
    Completed,
    Failed,
    #[serde(rename = "currentTask")]
    CurrentTask,
}

#[derive(Debug, Deserialize, Serialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
    token_type: String,
}

#[derive(Debug)]
struct PlaybackState {
    last_api_time: SystemTime,
    is_reading: bool,
}

#[derive(Debug)]
struct TimingInfo {
    last_api_time: Option<SystemTime>,
    last_position: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ImgurResponse {
    data: ImgurData,
    success: bool,
}

#[derive(Debug, Deserialize)]
struct ImgurData {
    link: String,
}

#[derive(Debug, Deserialize)]
struct SeriesPosition {
    timestamp: u64,
    locator: serde_json::Value, // We don't need to parse the full locator structure
}

#[derive(Debug, Deserialize)]
struct SeriesPage {
    content: Vec<Series>,
    // You can add more fields if needed (e.g., totalElements, etc.)
}

#[derive(Debug, Deserialize, Clone)]
struct Book {
    id: String,
    title: Option<String>,
    number: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BookReadProgress {
    page: Option<u32>,
    completed: bool,
    updated_at: Option<String>, // ISO8601 timestamp
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let client = Client::new();

    // Update check disabled: no releases page yet
    // if let Some(latest_version) = check_for_update(&client).await? {
    //     info!(
    //         "A new version is available: {}. You're currently running version {}.",
    //         latest_version, CURRENT_VERSION
    //     );
    //     info!("Please re-run the installer or visit https://github.com/erictbar/Storyteller-RPC to download the latest version.");
    // } else {
    //     info!("You're running the latest version: {}", CURRENT_VERSION);
    // }

    let config_file = parse_args()?;
    info!("Using config file: {}", config_file);

    let config = load_config(&config_file)?;
    let mut discord = DiscordIpcClient::new(&config.discord_client_id);    discord.connect()?;
    info!("Komga Discord RPC Connected!");    let mut playback_state = PlaybackState {
        last_api_time: SystemTime::now(),
        is_reading: false,
    };
    let mut current_series: Option<Series> = None;
    let mut timing_info = TimingInfo {
        last_api_time: None,
        last_position: None,
    };    let mut imgur_cache: HashMap<String, String> = HashMap::new();

    loop {
        if let Err(e) = set_activity(
            &client,
            &config,
            &mut discord,
            &mut playback_state,
            &mut current_series,
            &mut timing_info,
            &mut imgur_cache,
        )        .await
        {
            let mut is_pipe_error = false;
            let mut is_auth_error = false;

            // Check for authentication errors
            if let Some(source_err) = e.downcast_ref::<reqwest::Error>() {
                if let Some(status) = source_err.status() {
                    if status == reqwest::StatusCode::UNAUTHORIZED {
                        is_auth_error = true;
                    }
                }
            }

            if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                if io_err.kind() == ErrorKind::BrokenPipe || io_err.raw_os_error() == Some(232) || io_err.raw_os_error() == Some(32) {
                    is_pipe_error = true;
                }
            }

            if !is_pipe_error && !is_auth_error {
                let mut source = e.source();
                while let Some(err) = source {
                    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
                        if io_err.kind() == ErrorKind::BrokenPipe || io_err.raw_os_error() == Some(232) || io_err.raw_os_error() == Some(32) {
                            is_pipe_error = true;
                            break;
                        }
                    }
                    source = err.source();
                }
            }

            if is_auth_error {
                warn!("Authentication expired, re-authenticating...");
                // access_token = None;
                continue;
            }

            if is_pipe_error {
                warn!("Connection to Discord lost (pipe closed). Attempting to reconnect...");
                if let Err(close_err) = discord.close() {
                    error!("Error closing old Discord client (connection likely already broken): {}", close_err);
                }
                time::sleep(Duration::from_secs(5)).await;
                let mut new_discord = DiscordIpcClient::new(&config.discord_client_id);
                if let Err(connect_err) = new_discord.connect() {
                    error!("Failed to reconnect to Discord: {}", connect_err);
                } else {
                    info!("Successfully reconnected to Discord.");
                    discord = new_discord;
                }
            } else {
                error!("Error setting activity (not identified as pipe error): {}", e);
                error!("Full error details: {:?}", e);
            }
        }
        time::sleep(Duration::from_secs(15)).await;
    }
}

fn parse_args() -> Result<String, Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if let Some(index) = args.iter().position(|arg| arg == "-c") {
        if index + 1 < args.len() {
            Ok(args[index + 1].clone())
        } else {
            Err("Error: missing argument for -c option".into())
        }
    } else {
        Ok("config.json".to_string())
    }
}

fn load_config(config_file: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let config_str = fs::read_to_string(config_file)?;
    let config: Config = serde_json::from_str(&config_str)?;
    Ok(config)
}

#[allow(non_snake_case)]
async fn set_activity(
    client: &Client,
    config: &Config,
    discord: &mut DiscordIpcClient,
    playback_state: &mut PlaybackState,
    current_series: &mut Option<Series>,
    timing_info: &mut TimingInfo,
    imgur_cache: &mut HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Fetch in-progress books, sorted by most recently updated
    let books_url = format!(
        "{}/api/v1/books?readStatus=IN_PROGRESS&sort=lastModified,desc",
        config.komga_url
    );
    let response = client
        .get(&books_url)
        .header("X-API-Key", &config.komga_api_key)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("Failed to fetch in-progress books with status: {}", response.status()).into());
    }

    let books_page: serde_json::Value = response.json().await?;
    let books = books_page.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();

    if books.is_empty() {
        info!("No in-progress books found in Komga");
        discord.clear_activity()?;
        return Ok(());
    }

    // Use the most recently updated book (first in the sorted list)
    let book = &books[0];
    let book_id = book.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let book_title = book.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
    let series_id = book.get("seriesId").and_then(|v| v.as_str()).unwrap_or("");
    let page = book.get("readProgress").and_then(|rp| rp.get("page")).and_then(|v| v.as_u64()).map(|v| v as u32);
    let last_modified_str = book.get("readProgress").and_then(|rp| rp.get("lastModified")).and_then(|v| v.as_str());
    let last_modified = last_modified_str.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|dt| dt.with_timezone(&Utc));

    // Only show as reading if updated in the last 5 minutes
    let now = Utc::now();
    if let Some(updated_at) = last_modified {
        if (now - updated_at).num_seconds() >= 300 {
            info!("Most recent in-progress book activity is too old (timestamp: {}), clearing Discord status", updated_at);
            discord.clear_activity()?;
            return Ok(());
        }
    } else {
        info!("No valid lastModified timestamp for most recent in-progress book, clearing Discord status");
        discord.clear_activity()?;
        return Ok(());
    }

    // Fetch series info for the book
    let series_url = format!("{}/api/v1/series/{}", config.komga_url, series_id);
    let response = client
        .get(&series_url)
        .header("X-API-Key", &config.komga_api_key)
        .send()
        .await?;
    if !response.status().is_success() {
        error!("Failed to fetch series info for book {}", book_id);
        discord.clear_activity()?;
        return Ok(());
    }
    let series: Series = response.json().await?;

    // Check if this series should be excluded based on library configuration
    // (You may want to fetch the library name if needed, or skip this if not required)

    let authors: Vec<String> = series.authors.as_ref().map_or(vec![], |a| a.iter().map(|a| a.name.clone()).collect());
    let author_text = if authors.is_empty() {
        "Unknown Author".to_string()
    } else {
        authors.join(", ")
    };
    let series_title = series.title.as_deref().unwrap_or("Untitled");

    if current_series.as_ref().map_or(true, |s| s.id != series.id) {
        *current_series = Some(series.clone());
        *playback_state = PlaybackState {
            last_api_time: SystemTime::now(),
            is_reading: false,
        };
    }
    let large_text = if config.show_progress.unwrap_or(false) {
        "Reading"
    } else {
        "Komga"
    };

    let mut details = format!("{}", series_title);
    if let Some(page_num) = page {
        details = format!("{} (Page {})", series_title, page_num);
    }

    let activity_builder = activity::Activity::new()
        .details(&details)
        .state(&author_text)
        .activity_type(activity::ActivityType::Playing);

    let cover_url = get_komga_cover_path(client, config, &series.id, imgur_cache).await?;

    let final_activity = if let Some(ref url) = cover_url {
        activity_builder.assets(
            activity::Assets::new()
                .large_image(url)
                .large_text(large_text)
        )
    } else {
        activity_builder
    };

    discord.set_activity(final_activity)?;
    timing_info.last_api_time = Some(SystemTime::now());
    Ok(())
}

async fn get_komga_cover_path(
    client: &Client,
    config: &Config,
    series_id: &str,
    imgur_cache: &mut HashMap<String, String>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if config.use_imgur_cover.unwrap_or(true) {
        if let Some(imgur_client_id) = &config.imgur_client_id {
            let cache_key = format!("komga_{}", series_id);
            
            // Check cache first
            if let Some(cached_url) = imgur_cache.get(&cache_key) {
                return Ok(Some(cached_url.clone()));
            }            // Get cover from Komga - try /api/v1/series/{id}/thumbnail first, then fallback to Imgur
            let cover_url = format!("{}/api/v1/series/{}/thumbnail", config.komga_url, series_id);
            let response = client
                .get(&cover_url)
                .header("X-API-Key", &config.komga_api_key)
                .send()
                .await;

            if let Ok(resp) = response {
                let status = resp.status();
                if status.is_success() {
                    let cover_bytes = resp.bytes().await?;
                    // Upload to Imgur
                    if let Ok(imgur_url) = upload_to_imgur(client, imgur_client_id, &cover_bytes).await {
                        imgur_cache.insert(cache_key, imgur_url.clone());
                        return Ok(Some(imgur_url));
                    }
                }
                // If we get a 404, just return None
                if status == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                // For other errors, just return None
                return Ok(None);
            }
        }
    }

    // Fallback: no cover available for Komga right now
    // Could potentially implement external cover search here like the original
    Ok(None)
}

async fn upload_to_imgur(
    client: &Client,
    client_id: &str,
    image_data: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    let part = reqwest::multipart::Part::bytes(image_data.to_vec())
        .file_name("cover.jpg")
        .mime_str("image/jpeg")?;
    
    let form = reqwest::multipart::Form::new()
        .part("image", part);

    let response = client
        .post("https://api.imgur.com/3/image")
        .header("Authorization", format!("Client-ID {}", client_id))
        .multipart(form)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
        return Err(format!("Imgur upload failed with status: {} - {}", status, error_text).into());
    }

    let imgur_response: ImgurResponse = response.json().await?;
    
    if !imgur_response.success {
        return Err("Imgur upload was not successful".into());
    }

    Ok(imgur_response.data.link)
}

// Comment out the check_for_update function since it's not used
/*
async fn check_for_update(client: &Client) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let url = "https://github.com/erictbar/Storyteller-RPC";
    let resp = client
        .get(url)
        .header("User-Agent", "Storyteller-Discord-RPC")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!("GitHub API request failed with status: {}", resp.status()).into());
    }

    let release_info: ReleaseInfo = resp.json().await?;
    let latest_version = release_info.tag_name.trim_start_matches('v');

    if latest_version != CURRENT_VERSION {
        Ok(Some(latest_version.to_string()))
    } else {
        Ok(None)
    }
}
*/

fn should_show_as_reading_with_timestamp(now: &SystemTime, position_timestamp: u64) -> bool {
    // Show as reading if the last position update was within the last 5 minutes
    if let Ok(now_timestamp) = now.duration_since(SystemTime::UNIX_EPOCH) {
        let now_ms = now_timestamp.as_millis() as u64;
        let time_since_activity_ms = now_ms.saturating_sub(position_timestamp);
        let time_since_activity_secs = time_since_activity_ms / 1000;
        // Consider "reading" if activity within last 5 minutes (300 seconds)
        time_since_activity_secs < 300
    } else {
        false
    }
}