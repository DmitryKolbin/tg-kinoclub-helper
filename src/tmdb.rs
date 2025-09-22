use reqwest::Client;
use serde::Deserialize;

#[derive(Clone)]
pub struct TmdbClient {
    api_key: String,
    http: Client,
}

impl TmdbClient {
    pub fn new(api_key: String) -> Self {
        Self { api_key, http: Client::new() }
    }

    /// Поиск фильмов (RU), максимум `limit` (1..10).
    pub async fn search_movies_ru(&self, query: &str, limit: usize) -> reqwest::Result<Vec<Movie>> {
        let url = format!(
            "https://api.themoviedb.org/3/search/movie?query={}&language=ru-RU&include_adult=false&page=1",
            urlencoding::encode(query)
        );
        let resp = self.http.get(url).bearer_auth(self.api_key.to_string()).send().await?;
        if !resp.status().is_success() {
            return Ok(vec![]);
        }
        let mut data: SearchResp = resp.json().await?;
        data.results.truncate(limit.min(10));
        Ok(data.results)
    }

    /// Детали фильма (RU) — чтобы «показать описание и постер» в списке.
    pub async fn movie_details_ru(&self, id: u64) -> reqwest::Result<Option<Movie>> {
        let url = format!(
            "https://api.themoviedb.org/3/movie/{}?language=ru-RU",
            id
        );
        let resp = self.http.get(url).bearer_auth(self.api_key.to_string()).send().await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let m: Movie = resp.json().await?;
        Ok(Some(m))
    }

    /// Лучший трейлер (YouTube), RU→EN
    pub async fn best_trailer_url(&self, movie_id: u64) -> reqwest::Result<Option<String>> {
        let mut all: Vec<Video> = Vec::new();
        for lang in ["ru-RU", "en-US"] {
            let url = format!(
                "https://api.themoviedb.org/3/movie/{}/videos?language={}",
                movie_id, lang
            );
            let resp = self.http.get(url).bearer_auth(self.api_key.to_string()).send().await?;
            if resp.status().is_success() {
                let mut v: VideosResp = resp.json().await?;
                all.append(&mut v.results);
            }
        }
        let mut candidates: Vec<&Video> = all.iter()
            .filter(|v| v.site.eq_ignore_ascii_case("YouTube"))
            .collect();
        candidates.sort_by_key(|v| {
            let official = if v.official.unwrap_or(false) { 0 } else { 1 };
            let typ = match v.r#type.as_str() { "Trailer" => 0, "Teaser" => 1, _ => 2 };
            (official, typ)
        });
        Ok(candidates.first().map(|v| format!("https://www.youtube.com/watch?v={}", v.key)))
    }
}

/* ======= DTOs ======= */

#[derive(Deserialize, Debug)]
struct SearchResp { results: Vec<Movie> }

#[derive(Deserialize, Debug, Clone)]
pub struct Movie {
    pub id: u64,
    pub title: String,
    pub original_title: String,
    pub overview: String,
    pub poster_path: Option<String>,
    pub release_date: Option<String>,
}

#[derive(Deserialize, Debug)]
struct VideosResp { results: Vec<Video> }

#[derive(Deserialize, Debug)]
struct Video {
    key: String,
    site: String,
    r#type: String,
    official: Option<bool>,
}