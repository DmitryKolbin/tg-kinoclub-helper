mod tmdb;
mod tg;
mod storage;

use dotenvy::dotenv;
use teloxide::prelude::*;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let bot = Bot::from_env();
    let tmdb_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY is missing");
    let tmdb = tmdb::TmdbClient::new(tmdb_key);

    // путь к файлу хранения (можно через ENV)
    let store_path = std::env::var("STORE_PATH").unwrap_or_else(|_| "movie_bot_state.json".to_string());
    let storage = storage::Storage::new(store_path).await?;

    tg::run(bot, tmdb, storage, false, true).await;
    Ok(())
}