use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::fs;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMovie {
    pub id: u64,
    pub title: String,
    pub original_title: String,
    pub poster_path: Option<String>,
    pub release_date: Option<String>,
    // overview хранить не обязательно; для показа детальной инфы всё равно тянем из TMDb
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FileState {
    version: u32,
    // chat_id -> movies
    chats: HashMap<i64, Vec<StoredMovie>>,
}

#[derive(Clone)]
pub struct Storage {
    inner: Arc<RwLock<FileState>>,
    path: PathBuf,
}

impl Storage {
    pub async fn new(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let state = if fs::try_exists(&path).await.unwrap_or(false) {
            let data = fs::read(&path).await?;
            match serde_json::from_slice::<FileState>(&data) {
                Ok(mut s) => { if s.version == 0 { s.version = 1; } s }
                Err(_) => FileState { version: 1, ..Default::default() },
            }
        } else {
            FileState { version: 1, ..Default::default() }
        };
        Ok(Self { inner: Arc::new(RwLock::new(state)), path })
    }

    pub async fn get(&self, chat_id: i64) -> Vec<StoredMovie> {
        let guard = self.inner.read().await;
        guard.chats.get(&chat_id).cloned().unwrap_or_default()
    }

    /// Полностью заменить список фильмов для чата (макс. 10), с сохранением на диск.
    pub async fn put(&self, chat_id: i64, mut movies: Vec<StoredMovie>) -> anyhow::Result<()> {
        if movies.len() > 10 { movies.truncate(10); }
        // 1) обновляем память
        {
            let mut guard = self.inner.write().await;
            guard.chats.insert(chat_id, movies);
        }
        // 2) атомарная запись снапшота
        self.flush().await
    }

    pub async fn remove_chat(&self, chat_id: i64) -> anyhow::Result<()> {
        {
            let mut guard = self.inner.write().await;
            guard.chats.remove(&chat_id);
        }
        self.flush().await
    }

    pub async fn add_movie(&self, chat_id: i64, m: StoredMovie) -> anyhow::Result<bool> {
        // возвращает: true — если добавили, false — если уже был/переполнен
        let mut added = false;
        {
            let mut guard = self.inner.write().await;
            let entry = guard.chats.entry(chat_id).or_default();
            if entry.iter().any(|x| x.id == m.id) {
                added = false;
            } else if entry.len() >= 10 {
                added = false;
            } else {
                entry.push(m);
                added = true;
            }
        }
        if added { self.flush().await?; }
        Ok(added)
    }

    pub async fn delete_movie(&self, chat_id: i64, movie_id: u64) -> anyhow::Result<bool> {
        let mut removed = false;
        {
            let mut guard = self.inner.write().await;
            if let Some(list) = guard.chats.get_mut(&chat_id) {
                let before = list.len();
                list.retain(|m| m.id != movie_id);
                removed = list.len() < before;
            }
        }
        if removed { self.flush().await?; }
        Ok(removed)
    }

    async fn flush(&self) -> anyhow::Result<()> {
        // клонируем снапшот под read‑локом и пишем вне лока (без дедлоков)
        let snapshot = {
            let guard = self.inner.read().await;
            serde_json::to_vec_pretty(&*guard)?
        };
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &snapshot).await?;
        fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}