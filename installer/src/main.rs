use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;
use reqwest::Client;
use serde_json::Value;
use serde::{Deserialize, Serialize};
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    komga_url: String,
    komga_api_key: String,
    discord_client_id: String,
    #[serde(default)]
    use_imgur_cover: bool,
    #[serde(default)]
    imgur_client_id: Option<String>,
    #[serde(default)]
    exclude_libraries: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Komga Discord RPC");

    let config_path = PathBuf::from("config.json");
    let config: Config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        serde_json::from_str(&content)?
    } else {
        let config = prompt_config()?;
        fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
        config
    };

    let client = Client::new();
    let mut discord = DiscordIpcClient::new(&config.discord_client_id)?;
    discord.connect()?;

    loop {
        match get_current_reading(&client, &config).await {
            Ok(Some((series, book, page, cover_url))) => {
                let details = format!("{} - {}", series, book);
                let state = format!("Page {}", page);
                let mut act = activity::Activity::new()
                    .state(&state)
                    .details(&details)
                    .assets(activity::Assets::new().large_image(&cover_url));
                discord.set_activity(act)?;
            }
            Ok(None) => {
                discord.clear_activity()?;
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                discord.clear_activity()?;
            }
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

fn prompt_config() -> Result<Config, io::Error> {
    println!("Please enter the following information:");
    let komga_url = prompt("Komga URL (e.g. http://localhost:25600)")?;
    let komga_api_key = prompt("Komga API Key")?;
    let discord_client_id = prompt_with_default("Discord Client ID", "1387202171270861033")?;
    Ok(Config { komga_url, komga_api_key, discord_client_id, use_imgur_cover: false, imgur_client_id: None, exclude_libraries: Vec::new() })
}

fn prompt_with_default(prompt: &str, default: &str) -> Result<String, io::Error> {
    print!("{} [{}]: ", prompt, default);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    if input.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(input.to_string())
    }
}

fn prompt(prompt: &str) -> Result<String, io::Error> {
    print!("{}: ", prompt);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

async fn get_current_reading(client: &Client, config: &Config) -> Result<Option<(String, String, u32, String)>, Box<dyn std::error::Error>> {
    // Get current user
    let user_resp = client
        .get(format!("{}/api/v2/users/me", config.komga_url))
        .header("X-API-Key", &config.komga_api_key)
        .send()
        .await?;
    if !user_resp.status().is_success() {
        return Err(format!("Failed to get user: {}", user_resp.status()).into());
    }
    let user: Value = user_resp.json().await?;
    let user_id = user.get("id").and_then(|v| v.as_str()).ok_or("No user id")?;

    // Get reading history (last entry is current)
    let history_resp = client
        .get(format!("{}/api/v1/history", config.komga_url))
        .header("X-API-Key", &config.komga_api_key)
        .send()
        .await?;
    if !history_resp.status().is_success() {
        return Ok(None);
    }
    let history: Value = history_resp.json().await?;
    let entries = history.get("content").and_then(|v| v.as_array()).ok_or("No history content")?;
    let last = entries.iter().find(|entry| entry.get("userId").and_then(|v| v.as_str()) == Some(user_id));
    let last = match last {
        Some(e) => e,
        None => return Ok(None),
    };
    let book = last.get("bookTitle").and_then(|v| v.as_str()).unwrap_or("");
    let series = last.get("seriesTitle").and_then(|v| v.as_str()).unwrap_or("");
    let page = last.get("page").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let series_id = last.get("seriesId").and_then(|v| v.as_str()).unwrap_or("");
    let library_name = last.get("libraryName").and_then(|v| v.as_str()).unwrap_or("");

    // Exclude libraries if configured
    if !config.exclude_libraries.is_empty() && config.exclude_libraries.iter().any(|lib| lib.eq_ignore_ascii_case(library_name)) {
        return Ok(None);
    }

    // Get cover art
    let mut cover_url = format!("{}/api/v1/series/{}/thumbnail", config.komga_url, series_id);
    if config.use_imgur_cover {
        if let Some(imgur_client_id) = &config.imgur_client_id {
            if let Ok(imgur_url) = fetch_and_upload_imgur_cover(client, &cover_url, imgur_client_id).await {
                cover_url = imgur_url;
            }
        }
    }
    Ok(Some((series.to_string(), book.to_string(), page, cover_url)))
}

async fn fetch_and_upload_imgur_cover(client: &Client, cover_url: &str, imgur_client_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    // Download the cover image from Komga
    let img_bytes = client.get(cover_url).send().await?.bytes().await?;
    // Upload to Imgur
    let resp = client.post("https://api.imgur.com/3/image")
        .header("Authorization", format!("Client-ID {}", imgur_client_id))
        .form(&[ ("image", base64::encode(&img_bytes)) ])
        .send().await?;
    let json: Value = resp.json().await?;
    if let Some(link) = json.get("data").and_then(|d| d.get("link")).and_then(|l| l.as_str()) {
        Ok(link.to_string())
    } else {
        Err("Failed to upload to Imgur".into())
    }
}
