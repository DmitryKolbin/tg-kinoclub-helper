use reqwest::Client;
use serde::{Deserialize, Serialize};

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
    pub async fn search_movies_ru(&self, query: &str, limit: usize) -> reqwest::Result<Vec<MultiNorm>> {
        let url = format!(
            "https://api.themoviedb.org/3/search/multi?query={}&language=ru-RU&include_adult=false&page=1",
            urlencoding::encode(query)
        );
        let resp = self.http.get(url).bearer_auth(self.api_key.to_string()).send().await?;
        if !resp.status().is_success() {
            return Ok(vec![]);
        }

        let data: SearchResp<SearchMultiDto> = resp.json().await?;

        let items: Vec<MultiNorm> = data.results
            .into_iter()
            .filter(|item| matches!(item, SearchMultiDto::Movie { .. } | SearchMultiDto::Tv { .. }))
            .take(limit)
            .map(Into::into)
            .collect();
        
        Ok(items)
    }

    /// Детали фильма (RU) — чтобы «показать описание и постер» в списке.
    pub async fn movie_details_ru(&self, id: u64) -> reqwest::Result<Option<MultiNorm>> {
        let url = format!(
            "https://api.themoviedb.org/3/movie/{}?language=ru-RU",
            id
        );
        let resp = self.http.get(url).bearer_auth(self.api_key.to_string()).send().await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let m: MovieDetailsDto = resp.json().await?;
        Ok(Some(m.into()))
    }
    
    //Детали сериала (RU) — чтобы «показать описание и постер» в списке.
    pub async fn tv_details_ru(&self, id: u64) -> reqwest::Result<Option<MultiNorm>> {
        let url = format!(
            "https://api.themoviedb.org/3/tv/{}?language=ru-RU",
            id
        );
        let resp = self.http.get(url).bearer_auth(self.api_key.to_string()).send().await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let m: TvDetailsDto = resp.json().await?;
        Ok(Some(m.into()))
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
    
    /// Лучший трейлер сериала (YouTube), RU→EN
    pub async fn best_tv_trailer_url(&self, tv_id: u64) -> reqwest::Result<Option<String>> {
        let mut all: Vec<Video> = Vec::new();
        for lang in ["ru-RU", "en-US"] {
            let url = format!(
                "https://api.themoviedb.org/3/tv/{}/videos?language={}",
                tv_id, lang
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
pub struct SearchResp<T> {
    pub page: u32,
    pub results: Vec<T>,
    pub total_pages: u32,
    pub total_results: u32,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "media_type")]
pub enum SearchMultiDto {
    #[serde(rename = "movie")]
    Movie {
        id: u64,
        title: String,
        original_title: String,
        #[serde(default)]
        overview: String,
        poster_path: Option<String>,
        release_date: Option<String>,
    },
    #[serde(rename = "tv")]
    Tv {
        id: u64,
        name: String,
        original_name: String,
        #[serde(default)]
        overview: String,
        poster_path: Option<String>,
        first_air_date: Option<String>,
    },
    #[serde(rename = "person")]
    Person {
        id: u64,
        name: String,
        profile_path: Option<String>,
    },
}

#[derive(Deserialize, Debug, Clone)]
pub struct TvDetailsDto {
    pub id: u64,
    pub name: String,
    pub original_name: String,
    #[serde(default)]
    pub overview: String,
    pub poster_path: Option<String>,
    pub first_air_date: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MovieDetailsDto {
    pub id: u64,
    pub title: String,
    pub original_title: String,
    #[serde(default)]
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

#[derive(Debug, Clone)]
pub struct MultiNorm {
    pub id: u64,
    pub media_type: MediaKind,      // всегда есть
    pub title: String,              // гарантируем при маппинге
    pub original_title: String,     // гарантируем при маппинге (для person = title)
    pub overview: String,           // пустая строка, если нет
    pub release_date: Option<String>, // у person нет
    pub image_path: Option<String>, // poster_path или profile_path
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Movie,
    Tv,
    Person,
}

/* Mapping to internal model */

impl From<SearchMultiDto> for MultiNorm {
    fn from(x: SearchMultiDto) -> Self {
        match x {
            SearchMultiDto::Movie { id, title, original_title, overview, poster_path, release_date } => {
                Self {
                    id,
                    media_type: MediaKind::Movie,
                    title,
                    original_title,
                    overview,
                    release_date,
                    image_path: poster_path,
                }
            }
            SearchMultiDto::Tv { id, name, original_name, overview, poster_path, first_air_date } => {
                Self {
                    id,
                    media_type: MediaKind::Tv,
                    title: name,
                    original_title: original_name,
                    overview,
                    release_date: first_air_date,
                    image_path: poster_path,
                }
            }
            SearchMultiDto::Person { id, name, profile_path } => {
                Self {
                    id,
                    media_type: MediaKind::Person,
                    title: name.clone(),
                    original_title: name,
                    overview: String::new(),
                    release_date: None,
                    image_path: profile_path,
                }
            }
        }
    }
}

impl From<TvDetailsDto> for MultiNorm {
    fn from(tv: TvDetailsDto) -> Self {
        Self {
            id: tv.id,
            media_type: MediaKind::Tv,
            title: tv.name,
            original_title: tv.original_name,
            overview: tv.overview,
            release_date: tv.first_air_date,
            image_path: tv.poster_path,
        }
    }
}

impl From<MovieDetailsDto> for MultiNorm {
    fn from(m: MovieDetailsDto) -> Self {
        Self {
            id: m.id,
            media_type: MediaKind::Movie,
            title: m.title,
            original_title: m.original_title,
            overview: m.overview,
            release_date: m.release_date,
            image_path: m.poster_path,
        }
    }
}