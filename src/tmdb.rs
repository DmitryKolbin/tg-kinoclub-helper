use std::cmp::PartialEq;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::time::{sleep, Duration};

#[derive(Debug, Error)]
pub enum TmdbErr {
    #[error("TMDb: недоступно (сетевой таймаут/ошибка).")]
    Net,
    #[error("TMDb: превышен лимит запросов (429). Подождите немного.")]
    RateLimited,
    #[error("TMDb: неверный ключ API (401). Проверьте TMDB_API_KEY.")]
    Auth,
    #[error("TMDb: доступ запрещён (403).")]
    Forbidden,
    #[error("TMDb: не найдено (404).")]
    NotFound,
    #[error("TMDb: внутренняя ошибка ({0}).")]
    Server(u16),
    #[error("TMDb: неожиданный статус ({0}).")]
    Unexpected(u16),
}

impl TmdbErr {
    pub fn user_msg(&self) -> &'static str {
        match self {
            TmdbErr::Net => "TMDb сейчас не отвечает. Попробуйте ещё раз через минуту.",
            TmdbErr::RateLimited => "Слишком часто спрашиваем TMDb. Подождите немного и повторите.",
            TmdbErr::Auth => "Неверный TMDB_API_KEY на сервере бота. Сообщите администратору.",
            TmdbErr::Forbidden => "TMDb отклонил запрос (403). Попробуйте другой фильм.",
            TmdbErr::NotFound => "Ничего не нашлось в TMDb.",
            TmdbErr::Server(_) => "TMDb временно недоступен. Повторите позже.",
            TmdbErr::Unexpected(_) => "Неожиданный ответ TMDb. Попробуйте ещё раз.",
        }
    }
}

#[derive(Clone)]
pub struct TmdbClient {
    bearer_token: String,
    http: Client,
}

impl PartialEq for MediaKind {
    fn eq(&self, other: &Self) -> bool {
        matches!((self, other),
            (MediaKind::Movie, MediaKind::Movie) |
            (MediaKind::Tv, MediaKind::Tv) |
            (MediaKind::Person, MediaKind::Person)
        )
    }
}

impl TmdbClient {
    pub fn new(bearer_token: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(12))
            .user_agent("tg-movie-bot/1.0 (+teloxide)")
            .build()
            .expect("reqwest client");
        Self { bearer_token, http }
    }

    // Обобщённая загрузка + JSON с ретраями (для 5xx/429/сетевых)
    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, TmdbErr> {
        // 3 попытки, бэкофф 300/800/1500 мс
        let mut delays = [300u64, 800, 1500].into_iter();
        loop {
            let req = self.http
                .get(url)
                .bearer_auth(&self.bearer_token); // 👈 тут
            let resp = match req.send().await {
                Ok(r) => r,
                Err(_) => {
                    if let Some(ms) = delays.next() {
                        sleep(Duration::from_millis(ms)).await;
                        continue;
                    } else {
                        return Err(TmdbErr::Net);
                    }
                }
            };

            match resp.status() {
                StatusCode::OK => {
                    let v = resp.json::<T>().await.map_err(|_| TmdbErr::Net)?;
                    return Ok(v);
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    if let Some(ms) = delays.next() {
                        sleep(Duration::from_millis(ms)).await;
                        continue;
                    } else {
                        return Err(TmdbErr::RateLimited);
                    }
                }
                StatusCode::UNAUTHORIZED => return Err(TmdbErr::Auth),
                StatusCode::FORBIDDEN => return Err(TmdbErr::Forbidden),
                StatusCode::NOT_FOUND => return Err(TmdbErr::NotFound),
                s if s.is_server_error() => {
                    if let Some(ms) = delays.next() {
                        sleep(Duration::from_millis(ms)).await;
                        continue;
                    } else {
                        return Err(TmdbErr::Server(s.as_u16()));
                    }
                }
                s => return Err(TmdbErr::Unexpected(s.as_u16())),
            }
        }
    }

    /// Поиск фильмов (RU), максимум `limit` (1..10).
    pub async fn search_movies_ru(&self, query: &str, limit: usize) -> Result<Vec<MultiNorm>, TmdbErr> {
        let url = format!(
            "https://api.themoviedb.org/3/search/multi?query={}&language=ru-RU&include_adult=false&page=1",
            urlencoding::encode(query)
        );

        let data: SearchResp<SearchMultiDto> = self.get_json(&url).await?;

        let items = data
            .results
            .into_iter()
            .filter(|item| matches!(item, SearchMultiDto::Movie { .. } | SearchMultiDto::Tv { .. }))
            .map(Into::into) // -> MultiNorm
            .take(limit)
            .collect();

        Ok(items)
    }

    /// Детали фильма (RU) — чтобы «показать описание и постер» в списке.
    pub async fn movie_details_ru(&self, id: u64, media_type: MediaKind) -> Result<Option<MultiNorm>, TmdbErr> {
        let section = match media_type {
            MediaKind::Movie => "movie",
            MediaKind::Tv => "tv",
            MediaKind::Person => return Ok(None), // у персоны нет трейлеров
        };


        let url = format!(
            "https://api.themoviedb.org/3/{}/{}?language=ru-RU",
            section,
            id
        );

        let res = match media_type {
            MediaKind::Movie => {
                let data: MovieDetailsDto = self.get_json(&url).await?;
                data.into()
            }
            MediaKind::Tv => {
                let data: TvDetailsDto = self.get_json(&url).await?;
                data.into()
            } 
            MediaKind::Person => return Ok(None),
        };
        
        Ok(Some(res) )
    }

    /// Лучший трейлер (YouTube), RU→EN
    pub async fn best_trailer_url(&self, video: MultiNorm) -> Result<Option<String>, TmdbErr> {
        let mut all: Vec<Video> = Vec::new();
        let mut any_ok = false;
        let mut last_err: Option<TmdbErr> = None;

        let section = match video.media_type {
            MediaKind::Movie => "movie",
            MediaKind::Tv => "tv",
            MediaKind::Person => return Ok(None), // у персоны нет трейлеров
        };
        for lang in ["ru-RU", "en-US"] {
            let url = format!(
                "https://api.themoviedb.org/3/{}/{}/videos?language={}",
                section,
                video.id, lang
            );

            match self.get_json::<VideosResp>(&url).await {
                Ok(mut v) => {
                    any_ok = true;
                    all.append(&mut v.results);
                }
                Err(e) => {
                    // запомним ошибку, но попробуем следующий язык
                    last_err = Some(e);
                }
            }
        }
        // Если оба запроса провалились — отдаём ошибку пользователю/в верхний слой
        if !any_ok {
            return Err(last_err.unwrap_or(TmdbErr::Net));
        }

        // Фильтруем и сортируем кандидатов
        let mut candidates: Vec<&Video> = all
            .iter()
            .filter(|v| v.site.eq_ignore_ascii_case("YouTube"))
            .collect();

        candidates.sort_by_key(|v| {
            let official = if v.official.unwrap_or(false) { 0 } else { 1 };
            let typ = match v.r#type.as_str() {
                "Trailer" => 0,
                "Teaser" => 1,
                _ => 2,
            };
            (official, typ)
        });

        Ok(candidates
            .first()
            .map(|v| format!("https://www.youtube.com/watch?v={}", v.key)))
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

impl MediaKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            MediaKind::Movie => "movie",
            MediaKind::Tv => "tv",
            MediaKind::Person => "person",
        }
    }
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