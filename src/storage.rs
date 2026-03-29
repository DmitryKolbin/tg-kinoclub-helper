use crate::tmdb::MediaKind;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::fs;
use tokio::sync::RwLock;

fn default_media_kind() -> MediaKind {
    MediaKind::Movie
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMovie {
    pub id: u64,
    pub title: String,
    pub original_title: String,
    #[serde(default = "default_media_kind")]
    pub media_type: MediaKind,
    pub poster_path: Option<String>,
    pub release_date: Option<String>,
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
                Ok(mut s) => {
                    if s.version == 0 {
                        s.version = 1;
                    }
                    s
                }
                Err(_) => FileState {
                    version: 1,
                    ..Default::default()
                },
            }
        } else {
            FileState {
                version: 1,
                ..Default::default()
            }
        };
        Ok(Self {
            inner: Arc::new(RwLock::new(state)),
            path,
        })
    }

    pub async fn get(&self, chat_id: i64) -> Vec<StoredMovie> {
        let guard = self.inner.read().await;
        guard.chats.get(&chat_id).cloned().unwrap_or_default()
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
        let added;
        {
            let mut guard = self.inner.write().await;
            let entry = guard.chats.entry(chat_id).or_default();
            if entry
                .iter()
                .any(|x| x.id == m.id && x.media_type == m.media_type)
                || entry.len() >= 10
            {
                added = false;
            } else {
                entry.push(m);
                added = true;
            }
        }
        if added {
            self.flush().await?;
        }
        Ok(added)
    }

    pub async fn delete_movie(
        &self,
        chat_id: i64,
        movie_id: u64,
        media_kind: MediaKind,
    ) -> anyhow::Result<bool> {
        let mut removed = false;
        {
            let mut guard = self.inner.write().await;
            if let Some(list) = guard.chats.get_mut(&chat_id) {
                let before = list.len();
                list.retain(|m| !(m.id == movie_id && m.media_type == media_kind));
                removed = list.len() < before;
            }
        }
        if removed {
            self.flush().await?;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::fs;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    async fn setup_temp_storage() -> (Storage, PathBuf) {
        let mut tmp_path = PathBuf::from("tests/data");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        tmp_path.push(format!("test_storage_{}_{}.json", now, counter));
        let storage = Storage::new(tmp_path.clone())
            .await
            .expect("Failed to create storage");
        (storage, tmp_path)
    }

    #[tokio::test]
    async fn test_storage_new_empty() {
        let (storage, path) = setup_temp_storage().await;
        assert_eq!(storage.get(123).await.len(), 0);
        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn test_add_movie_success() {
        let (storage, path) = setup_temp_storage().await;
        let movie = StoredMovie {
            id: 1,
            title: "Test Movie".to_string(),
            original_title: "Test Movie".to_string(),
            media_type: MediaKind::Movie,
            poster_path: None,
            release_date: None,
        };

        let added = storage.add_movie(123, movie.clone()).await.unwrap();
        assert!(added);

        let movies = storage.get(123).await;
        assert_eq!(movies.len(), 1);
        assert_eq!(movies[0].id, 1);

        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn test_add_movie_duplicate() {
        let (storage, path) = setup_temp_storage().await;
        let movie = StoredMovie {
            id: 1,
            title: "Test Movie".to_string(),
            original_title: "Test Movie".to_string(),
            media_type: MediaKind::Movie,
            poster_path: None,
            release_date: None,
        };

        storage.add_movie(123, movie.clone()).await.unwrap();
        let added = storage.add_movie(123, movie).await.unwrap();
        assert!(!added);
        assert_eq!(storage.get(123).await.len(), 1);

        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn test_add_movie_limit() {
        let (storage, path) = setup_temp_storage().await;
        for i in 0..10 {
            let movie = StoredMovie {
                id: i,
                title: format!("Movie {}", i),
                original_title: format!("Movie {}", i),
                media_type: MediaKind::Movie,
                poster_path: None,
                release_date: None,
            };
            assert!(storage.add_movie(123, movie).await.unwrap());
        }

        let extra_movie = StoredMovie {
            id: 11,
            title: "Extra Movie".to_string(),
            original_title: "Extra Movie".to_string(),
            media_type: MediaKind::Movie,
            poster_path: None,
            release_date: None,
        };
        let added = storage.add_movie(123, extra_movie).await.unwrap();
        assert!(!added);
        assert_eq!(storage.get(123).await.len(), 10);

        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn test_delete_movie() {
        let (storage, path) = setup_temp_storage().await;
        let movie = StoredMovie {
            id: 1,
            title: "Test Movie".to_string(),
            original_title: "Test Movie".to_string(),
            media_type: MediaKind::Movie,
            poster_path: None,
            release_date: None,
        };

        storage.add_movie(123, movie).await.unwrap();
        let deleted = storage
            .delete_movie(123, 1, MediaKind::Movie)
            .await
            .unwrap();
        assert!(deleted);
        assert_eq!(storage.get(123).await.len(), 0);

        let deleted_again = storage
            .delete_movie(123, 1, MediaKind::Movie)
            .await
            .unwrap();
        assert!(!deleted_again);

        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn test_persistence() {
        let (tmp_path, storage) = {
            let mut p = PathBuf::from("tests/data");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let counter = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            p.push(format!("test_persistence_{}_{}.json", now, counter));
            let s = Storage::new(p.clone()).await.unwrap();
            (p.clone(), s)
        };

        let movie = StoredMovie {
            id: 1,
            title: "Persistent Movie".to_string(),
            original_title: "Persistent Movie".to_string(),
            media_type: MediaKind::Movie,
            poster_path: None,
            release_date: None,
        };
        storage.add_movie(123, movie).await.unwrap();

        // Re-load storage from the same file
        let reloaded_storage = Storage::new(tmp_path.clone()).await.unwrap();
        let movies = reloaded_storage.get(123).await;
        assert_eq!(movies.len(), 1);
        assert_eq!(movies[0].title, "Persistent Movie");

        let _ = fs::remove_file(tmp_path).await;
    }
}
