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

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct Config {
    discord_client_id: String,
    storyteller_url: String,
    storyteller_username: String,
    storyteller_password: String,
    show_progress: Option<bool>,
    use_storyteller_cover: Option<bool>,
    imgur_client_id: Option<String>,
    exclude_keywords: Option<Vec<String>>,
}

#[derive(Debug)]
struct Book {
    id: i64,
    name: String,
    authors: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ReleaseInfo {
    tag_name: String,
}

#[derive(Debug, Deserialize)]
struct BooksResponse {
    #[serde(flatten)]
    books: Vec<BookDetail>,
}

#[derive(Debug, Deserialize)]
struct BookDetail {
    id: i64,
    title: String,
    authors: Vec<BookAuthor>,
    #[serde(rename = "processingStatus")]
    processing_status: Option<ProcessingStatusObject>,
}

#[derive(Debug, Deserialize)]
struct ProcessingStatusObject {
    #[serde(rename = "currentTask")]
    current_task: String,
    progress: f64,
    status: ProcessingStatus,
}

#[derive(Debug, Deserialize)]
struct BookAuthor {
    name: String,
    #[serde(rename = "fileAs")]
    file_as: Option<String>,
    role: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
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
struct BookPosition {
    timestamp: u64,
    locator: serde_json::Value, // We don't need to parse the full locator structure
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
    info!("Storyteller Discord RPC Connected!");    let mut playback_state = PlaybackState {
        last_api_time: SystemTime::now(),
        is_reading: false,
    };
    let mut current_book: Option<Book> = None;
    let mut timing_info = TimingInfo {
        last_api_time: None,
        last_position: None,
    };    let mut imgur_cache: HashMap<String, String> = HashMap::new();
    let mut access_token: Option<String> = None;

    loop {
        // Authenticate if we don't have a token
        if access_token.is_none() {
            match authenticate_storyteller(&client, &config).await {
                Ok(token) => {
                    access_token = Some(token);
                    info!("Successfully authenticated with Storyteller");
                }
                Err(e) => {
                    error!("Failed to authenticate with Storyteller: {}", e);
                    time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            }
        }

        if let Err(e) = set_activity(
            &client,
            &config,
            access_token.as_ref().unwrap(),
            &mut discord,
            &mut playback_state,
            &mut current_book,
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
                access_token = None;
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
    access_token: &str,
    discord: &mut DiscordIpcClient,
    playback_state: &mut PlaybackState,
    current_book: &mut Option<Book>,
    timing_info: &mut TimingInfo,
    imgur_cache: &mut HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {    // Get all books from Storyteller - try /api/books first, then /books
    let books_endpoints = [
        format!("{}/api/books", config.storyteller_url),
        format!("{}/books", config.storyteller_url),
    ];
    
    let mut resp: Option<Vec<BookDetail>> = None;
    for books_url in &books_endpoints {
        let response = client
            .get(books_url)
            .bearer_auth(access_token)
            .send()
            .await?;        if response.status().is_success() {
            resp = Some(response.json().await?);
            break;
        }
        
        // If we get a 404, try the next endpoint
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        
        // For other errors (like auth errors), return them immediately
        return Err(format!("Failed to fetch books with status: {}", response.status()).into());
    }
    
    let resp = resp.ok_or("No working books endpoint found")?;    if resp.is_empty() {
        info!("No books found in Storyteller library");
        discord.clear_activity()?;
        return Ok(());
    }

    // Find the book with the most recent reading activity by checking position timestamps
    let mut most_recent_book: Option<(&BookDetail, u64)> = None;
    
    for book in &resp {
        // Only check completed/synced books
        if let Some(ref status_obj) = book.processing_status {
            if status_obj.status != ProcessingStatus::Completed {
                continue;
            }
        } else {
            continue;
        }
        
        // Try to get the position for this book to check recent activity
        let position_endpoints = [
            format!("{}/api/books/{}/positions", config.storyteller_url, book.id),
            format!("{}/books/{}/positions", config.storyteller_url, book.id),
        ];
        
        for position_url in &position_endpoints {
            match client
                .get(position_url)
                .bearer_auth(access_token)
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() {
                        if let Ok(position) = response.json::<BookPosition>().await {
                            // Check if this book has more recent activity
                            if most_recent_book.as_ref().map_or(true, |(_, timestamp)| position.timestamp > *timestamp) {
                                most_recent_book = Some((book, position.timestamp));
                            }
                            break; // Successfully got position, move to next book
                        }
                    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
                        // No position found for this book, but that's ok - try next endpoint
                        continue;
                    }
                }
                Err(_) => {
                    // Error getting position, try next endpoint
                    continue;
                }
            }
        }
    }    // Check if we have recent activity, or clear Discord status
    let book = if let Some((book, timestamp)) = most_recent_book {
        // Check if the activity is recent enough
        let now = SystemTime::now();
        if should_show_as_reading_with_timestamp(&now, timestamp) {
            info!("Found most recently active book: {} (last activity: {})", book.title, timestamp);
            book
        } else {
            info!("Most recent book activity is too old (timestamp: {}), clearing Discord status", timestamp);
            discord.clear_activity()?;
            return Ok(());
        }
    } else {
        // No position data found for any book, clear Discord status
        info!("No position data found for any completed books, clearing Discord status");
        discord.clear_activity()?;
        return Ok(());
    };

    // Check if this book should be excluded based on keywords
    if let Some(ref exclude_keywords) = config.exclude_keywords {
        for keyword in exclude_keywords {
            if book.title.to_lowercase().contains(&keyword.to_lowercase()) {
                info!("Book '{}' contains excluded keyword '{}', skipping Discord RPC", book.title, keyword);
                discord.clear_activity()?;
                return Ok(());
            }
        }
    }
    let authors: Vec<String> = book.authors.iter().map(|a| a.name.clone()).collect();
    let author_text = if authors.is_empty() {
        "Unknown Author".to_string()
    } else {
        authors.join(", ")
    };    let book_name = &book.title;

    // At this point, we know we have recent activity, so we can proceed with setting Discord status

    if current_book.as_ref().map_or(true, |b| b.id != book.id) {
        *current_book = Some(Book {
            id: book.id,
            name: book_name.clone(),
            authors: authors.clone(),
        });
        *playback_state = PlaybackState {
            last_api_time: SystemTime::now(),
            is_reading: false,
        };
    }    let large_text = if config.show_progress.unwrap_or(false) {
        "Reading"
    } else {
        "Storyteller"
    };

    let activity_builder = activity::Activity::new()
        .details(book_name)
        .state(&author_text)
        .activity_type(activity::ActivityType::Playing);

    let cover_url = get_storyteller_cover_path(client, config, access_token, book.id, imgur_cache).await?;

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

async fn authenticate_storyteller(
    client: &Client,
    config: &Config,
) -> Result<String, Box<dyn std::error::Error>> {
    let login_request = LoginRequest {
        username: config.storyteller_username.clone(),
        password: config.storyteller_password.clone(),
    };

    // Try /api/token first (v0.2+), then fall back to /token (v0.1)
    let endpoints = [
        format!("{}/api/token", config.storyteller_url),
        format!("{}/token", config.storyteller_url),
    ];
    
    for token_url in &endpoints {
        let resp = client
            .post(token_url)
            .form(&login_request)
            .send()
            .await?;

        if resp.status().is_success() {
            let login_response: LoginResponse = resp.json().await?;
            return Ok(login_response.access_token);
        }
        
        // If we get a 404, try the next endpoint
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        
        // For other errors, return them immediately
        return Err(format!("Authentication failed with status: {}", resp.status()).into());
    }
    
    Err("Authentication failed: no working token endpoint found".into())
}

fn should_show_as_reading(now: &SystemTime, playback_state: &PlaybackState) -> bool {
    // Simple heuristic: show as reading if it's been less than 30 minutes since last activity
    // In a real implementation, you'd track actual reading state from the mobile app or web interface
    if let Ok(elapsed) = now.duration_since(playback_state.last_api_time) {
        elapsed.as_secs() < 1800 // 30 minutes
    } else {
        false
    }
}

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

async fn get_storyteller_cover_path(
    client: &Client,
    config: &Config,
    access_token: &str,
    book_id: i64,
    imgur_cache: &mut HashMap<String, String>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if config.use_storyteller_cover.unwrap_or(true) {
        if let Some(imgur_client_id) = &config.imgur_client_id {
            let cache_key = format!("storyteller_{}", book_id);
            
            // Check cache first
            if let Some(cached_url) = imgur_cache.get(&cache_key) {
                return Ok(Some(cached_url.clone()));
            }            // Get cover from Storyteller server - try /api/books/{id}/cover first, then /books/{id}/cover
            let cover_endpoints = [
                format!("{}/api/books/{}/cover", config.storyteller_url, book_id),
                format!("{}/books/{}/cover", config.storyteller_url, book_id),
            ];
              for cover_url in &cover_endpoints {
                let response = client
                    .get(cover_url)
                    .bearer_auth(access_token)
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
                    
                    // If we get a 404, try the next endpoint
                    if status == reqwest::StatusCode::NOT_FOUND {
                        continue;
                    }
                    
                    // For other errors, break and continue with fallback
                    break;
                }
            }
        }
    }

    // Fallback: no cover available for Storyteller right now
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