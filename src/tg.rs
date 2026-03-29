use crate::storage::{Storage, StoredMovie};
use crate::tmdb;
use crate::tmdb::{MultiNorm, TmdbClient};
use once_cell::sync::Lazy;
use std::{collections::HashMap, sync::Arc};
use teloxide::types::Message;
use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt},
    prelude::*,
    types::{
        CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, InputMedia,
        InputMediaPhoto, ParseMode,
    },
    utils::command::BotCommands,
    RequestError,
};
use tokio::sync::RwLock;
/* ====== Хранилище состояния ======
   last_search: чат -> результаты последнего поиска (чтобы добавлять по кнопке) */
#[allow(clippy::type_complexity)]
static LAST_SEARCH: Lazy<Arc<RwLock<HashMap<ChatId, Vec<MultiNorm>>>>> =
    Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));

/* ====== Команды ====== */
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Команды:")]
enum Command {
    /// сброс списка
    #[command(description = "сбросить список")]
    Reset,
    /// показать список (до 10 фильмов)
    #[command(description = "показать список")]
    List,
    /// составить голосование (опрос + постеры + описания + трейлеры)
    #[command(description = "составить голосование")]
    Vote,
    /// помощь
    #[command(description = "помощь")]
    Help,
}

pub async fn run(bot: Bot, tmdb: TmdbClient, storage: Storage, anonymous: bool, multiple: bool) {
    let msg_handler = dptree::entry()
        .branch(
            Update::filter_message()
                .branch(dptree::entry().filter_command::<Command>().endpoint({
                    let tmdb = tmdb.clone();
                    let storage = storage.clone();
                    move |bot: Bot, msg: Message, cmd: Command| {
                        let tmdb = tmdb.clone();
                        let storage = storage.clone();
                        async move {
                            on_command(bot, msg, cmd, &tmdb, &storage, anonymous, multiple).await
                        }
                    }
                }))
                .branch({
                    let tmdb = tmdb.clone();
                    let storage = storage.clone();
                    dptree::endpoint(move |bot: Bot, msg: Message| {
                        let tmdb = tmdb.clone();
                        let storage = storage.clone();
                        async move { on_search_text(bot, msg, &tmdb, &storage).await }
                    })
                }),
        )
        .branch(Update::filter_callback_query().endpoint({
            let tmdb = tmdb.clone();
            let storage = storage.clone();
            move |bot: Bot, q: CallbackQuery| {
                let tmdb = tmdb.clone();
                let storage = storage.clone();
                async move { on_callback(bot, q, &tmdb, &storage).await }
            }
        }));

    Dispatcher::builder(bot, msg_handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

/* ====== Команды ====== */
async fn on_command<R>(
    bot: R,
    msg: Message,
    cmd: Command,
    tmdb: &TmdbClient,
    storage: &Storage,
    anonymous: bool,
    multiple: bool,
) -> ResponseResult<()>
where
    R: Requester<Err = RequestError>,
{
    match cmd {
        Command::Help => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string())
                .await?;
        }
        Command::Reset => {
            storage
                .remove_chat(msg.chat.id.0)
                .await
                .map_err(to_req_err)?;
            LAST_SEARCH.write().await.remove(&msg.chat.id);
            bot.send_message(msg.chat.id, "Список очищен.").await?;
        }
        Command::List => send_list_view(&bot, msg.chat.id, storage).await?,
        Command::Vote => {
            run_vote_flow(&bot, msg.chat.id, tmdb, storage, anonymous, multiple).await?
        }
    }
    Ok(())
}

/* ====== Поиск по тексту ====== */
async fn on_search_text<R>(
    bot: R,
    msg: Message,
    tmdb: &TmdbClient,
    _storage: &Storage,
) -> ResponseResult<()>
where
    R: Requester<Err = RequestError>,
{
    let Some(query) = message_text_any(&msg) else {
        return Ok(());
    };
    let query = query.trim();
    if query.is_empty() {
        return Ok(());
    }

    // Ищем до 10
    let results = match tmdb.search_movies_ru(query, 10).await {
        Ok(v) => v,
        Err(e) => {
            bot.send_message(msg.chat.id, e.user_msg()).await?;
            return Ok(());
        }
    };

    if results.is_empty() {
        bot.send_message(msg.chat.id, "Ничего не нашёл 😕").await?;
        return Ok(());
    }

    // Сохраним последний поиск (чтобы по кнопке "➕ Добавить" знать, что именно добавлять)
    LAST_SEARCH
        .write()
        .await
        .insert(msg.chat.id, results.clone());

    // Сообщение с названиями + краткими описаниями
    let mut blocks = Vec::new();
    for m in &results {
        blocks.push(make_block(m, 600)); // описания укоротим
    }
    let text = join_blocks(blocks, 3500); // запас до 4096
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await?;

    // Кнопки "➕ <Название (год)>"
    let kb = keyboard_add_results(&results);
    bot.send_message(msg.chat.id, "Выбери фильм, чтобы добавить в список:")
        .reply_markup(kb)
        .await?;

    Ok(())
}

/* ====== Callback-кнопки ======
   add:<id>   — добавить найденный фильм в список
   del:<id>   — удалить из списка
   show:<id>  — показать постер+описание из TMDb
*/
async fn on_callback<R>(
    bot: R,
    q: CallbackQuery,
    tmdb: &TmdbClient,
    storage: &Storage,
) -> ResponseResult<()>
where
    R: Requester<Err = RequestError>,
{
    let Some(data) = q.data.clone() else {
        return Ok(());
    };
    let chat_id = q.message.as_ref().map(|m| m.chat().id).unwrap_or(ChatId(0));
    let mut parts = data.splitn(3, ':');
    let cmd = parts.next().unwrap_or("");
    let id_str = parts.next().unwrap_or("");
    let media_type_str = parts.next().unwrap_or("");
    let Ok(id) = id_str.parse::<u64>() else {
        return Ok(());
    };

    let media_type = if media_type_str == "tv" {
        tmdb::MediaKind::Tv
    } else if media_type_str == "person" {
        tmdb::MediaKind::Person
    } else {
        tmdb::MediaKind::Movie
    };

    match cmd {
        "add" => {
            let movie_opt = {
                let map = LAST_SEARCH.read().await;
                map.get(&chat_id)
                    .and_then(|v| v.iter().find(|m| m.id == id))
                    .cloned()
            };
            if let Some(m) = movie_opt {
                let added = storage
                    .add_movie(
                        chat_id.0,
                        StoredMovie {
                            id: m.id,
                            title: m.title,
                            original_title: m.original_title,
                            poster_path: m.image_path.clone(),
                            release_date: m.release_date.clone(),
                            media_type: m.media_type,
                        },
                    )
                    .await
                    .map_err(to_req_err)?;
                if added {
                    answer_cb(&bot, &q, "Добавлено").await?;
                    send_list_view(&bot, chat_id, storage).await?;
                } else {
                    // либо уже есть, либо переполнено
                    // уточним причину:
                    let current = storage.get(chat_id.0).await;
                    if current.len() >= 10 {
                        answer_cb(&bot, &q, "В списке уже 10 фильмов").await?;
                    } else {
                        answer_cb(&bot, &q, "Уже в списке").await?;
                    }
                }
            } else {
                answer_cb(&bot, &q, "Не нашёл фильм в последнем поиске").await?;
            }
        }
        "del" => {
            let removed = storage
                .delete_movie(chat_id.0, id, media_type)
                .await
                .map_err(to_req_err)?;
            if removed {
                answer_cb(&bot, &q, "Удалено").await?;
                send_list_view(&bot, chat_id, storage).await?;
            } else {
                answer_cb(&bot, &q, "Не найдено в списке").await?;
            }
        }
        "show" => match tmdb.movie_details_ru(id, media_type).await {
            Ok(Some(m)) => {
                let text = make_block(&m, 2000);
                bot.send_message(chat_id, text)
                    .parse_mode(ParseMode::Html)
                    .await?;
                if let Some(p) = &m.image_path {
                    let url = format!("https://image.tmdb.org/t/p/w500{}", p);
                    if let Ok(bytes) = fetch_image(&url).await {
                        bot.send_photo(
                            chat_id,
                            InputFile::memory(bytes).file_name(format!("poster_{}.jpg", m.id)),
                        )
                        .await?;
                    }
                }
                answer_cb(&bot, &q, "Показал").await?;
            }
            Ok(None) => {
                answer_cb(&bot, &q, "Фильм не найден").await?;
                return Ok(());
            }
            Err(e) => {
                answer_cb(&bot, &q, e.user_msg()).await?;
                return Ok(());
            }
        },
        _ => {
            answer_cb(&bot, &q, "Неизвестная команда").await?;
        }
    }
    Ok(())
}

/* ====== /list: показать список с кнопками ====== */
async fn send_list_view<R>(bot: &R, chat: ChatId, storage: &Storage) -> ResponseResult<()>
where
    R: Requester<Err = RequestError>,
{
    let list = storage.get(chat.0).await;
    if list.is_empty() {
        bot.send_message(chat, "Список пуст. Пришли название — добавлю варианты.")
            .await?;
        return Ok(());
    }
    let mut lines = Vec::new();
    for m in &list {
        lines.push(one_line_title_stored(m));
    }
    let txt = format!("<b>В списке ({}/10):</b>\n{}", list.len(), lines.join("\n"));
    let kb = keyboard_list_two_columns_stored(&list);
    bot.send_message(chat, txt)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    Ok(())
}

async fn run_vote_flow<R>(
    bot: &R,
    chat: ChatId,
    tmdb: &TmdbClient,
    storage: &Storage,
    anonymous: bool,
    multiple_ans: bool,
) -> ResponseResult<()>
where
    R: Requester<Err = RequestError>,
{
    let list = storage.get(chat.0).await;
    if list.len() < 2 {
        bot.send_message(
            chat,
            "Нужно минимум 2 фильма в списке. Добавь и повтори /vote.",
        )
        .await?;
        return Ok(());
    }
    // опрос
    let options: Vec<teloxide::types::InputPollOption> = list
        .iter()
        .map(|m| teloxide::types::InputPollOption::new(one_line_title_stored(m)))
        .collect();
    bot.send_poll(chat, "Что смотрим?", options)
        .is_anonymous(anonymous)
        .allows_multiple_answers(multiple_ans)
        .await?;

    // альбом постеров (короткий общий caption)
    send_album_from_stored(bot, chat, &list, Some("<b>Постеры</b>")).await?;

    // описания + трейлеры (тянем детали по id)
    let mut blocks = Vec::new();
    let mut trailer_lines = Vec::new();
    for sm in &list {
        match sm.media_type {
            tmdb::MediaKind::Movie => {
                if let Some(m) = tmdb
                    .movie_details_ru(sm.id, sm.media_type)
                    .await
                    .map_err(to_req_err)?
                {
                    let trailer = tmdb
                        .best_trailer_url(m.clone())
                        .await
                        .map_err(to_req_err)
                        .ok()
                        .flatten();

                    if let Some(t) = trailer.as_ref() {
                        trailer_lines.push(format!(
                            "• <b>{}</b>: {}",
                            html_escape(&m.title),
                            html_escape(t)
                        ));
                    }
                    blocks.push(make_block(&m, 1200));
                }
            }
            tmdb::MediaKind::Tv => {
                if let Some(m) = tmdb
                    .movie_details_ru(sm.id, sm.media_type)
                    .await
                    .map_err(to_req_err)?
                {
                    let trailer = tmdb
                        .best_trailer_url(m.clone())
                        .await
                        .map_err(to_req_err)
                        .ok()
                        .flatten();

                    if let Some(t) = trailer.as_ref() {
                        trailer_lines.push(format!(
                            "• <b>{}</b>: {}",
                            html_escape(&m.title),
                            html_escape(t)
                        ));
                    }
                    blocks.push(make_block(&m, 1200));
                }
            }
            tmdb::MediaKind::Person => {
                // пропускаем
            }
        }
    }
    let text = join_blocks(blocks, 4000 - 50);
    for part in split_by_chars(&text, 4000) {
        bot.send_message(chat, part)
            .parse_mode(ParseMode::Html)
            .await?;
    }
    if !trailer_lines.is_empty() {
        bot.send_message(
            chat,
            format!("<b>Трейлеры</b>\n{}", trailer_lines.join("\n")),
        )
        .parse_mode(ParseMode::Html)
        .await?;
    }
    bot.send_message(chat, "Данные и изображения: © TMDB")
        .await?;
    Ok(())
}

/* ====== Кнопки ====== */

fn keyboard_add_results(results: &[MultiNorm]) -> InlineKeyboardMarkup {
    // по 1 в строке
    let mut rows = Vec::new();
    let mut row = Vec::new();
    for m in results {
        let btn = InlineKeyboardButton::callback(
            format!("➕ {}", one_line_title(m)),
            format!("add:{}", m.id),
        );
        row.push(btn);
        rows.push(row);
        row = Vec::new();
    }
    if !row.is_empty() {
        rows.push(row);
    }
    InlineKeyboardMarkup::new(rows)
}

/* ====== Вспомогательные ====== */

fn one_line_title(m: &MultiNorm) -> String {
    if let Some(y) = m.release_date.as_ref().and_then(|d| d.get(..4)) {
        format!("{} ({})", m.title, y)
    } else {
        m.title.clone()
    }
}

fn make_block(m: &MultiNorm, overview_limit: usize) -> String {
    let year = m
        .release_date
        .as_ref()
        .and_then(|d| d.get(..4))
        .unwrap_or("");
    let title = html_escape(&m.title);
    let body = if m.overview.trim().is_empty() {
        "<i>нет описания</i>".to_string()
    } else {
        clip(&html_escape(&m.overview), overview_limit)
    };

    if year.is_empty() {
        format!("<b>{}</b>\n\n{}", title, body)
    } else {
        format!("<b>{}</b> ({})\n\n{}", title, year, body)
    }
}

fn join_blocks(blocks: Vec<String>, limit_hint: usize) -> String {
    // аккуратно собираем, не превышая limit_hint
    let mut out = String::new();
    for b in blocks {
        let piece = if out.is_empty() {
            b
        } else {
            format!("\n\n{}", b)
        };
        if out.chars().count() + piece.chars().count() > limit_hint {
            // если не влезает — всё равно добавим, верхний слой потом порежет split_by_chars
            out.push_str(&piece);
            break;
        } else {
            out.push_str(&piece);
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

fn split_by_chars(s: &str, max: usize) -> Vec<String> {
    if s.chars().count() <= max {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if cur.chars().count() >= max {
            out.push(cur);
            cur = String::new();
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

async fn answer_cb<R>(bot: &R, q: &CallbackQuery, text: &str) -> ResponseResult<()>
where
    R: Requester<Err = RequestError>,
{
    bot.answer_callback_query(q.id.clone())
        .text(text)
        .show_alert(false)
        .await?;
    Ok(())
}

fn message_text_any(msg: &Message) -> Option<String> {
    if let Some(t) = msg.text() {
        return Some(t.to_string());
    }
    if let Some(c) = msg.caption() {
        return Some(c.to_string());
    }
    None
}

/* ====== Загрузка постера байтами (устойчиво к редиректам/CDN) ====== */
async fn fetch_image(url: &str) -> Result<Vec<u8>, teloxide::RequestError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; tg-bot/1.0)")
        .build()
        .map_err(to_req_err)?;
    let resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "image/*")
        .send()
        .await
        .map_err(to_req_err)?;
    if !resp.status().is_success() {
        return Err(to_req_err(format!("status {}", resp.status())));
    }
    if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE) {
        let ct = ct.to_str().unwrap_or("");
        if !ct.starts_with("image/") {
            return Err(to_req_err(format!("unexpected content-type: {ct}")));
        }
    }
    let bytes = resp.bytes().await.map_err(to_req_err)?;
    Ok(bytes.to_vec())
}

fn to_req_err<E: std::fmt::Display>(e: E) -> teloxide::RequestError {
    teloxide::RequestError::Io(std::sync::Arc::new(std::io::Error::other(e.to_string())))
}

fn one_line_title_stored(m: &StoredMovie) -> String {
    if let Some(y) = m.release_date.as_ref().and_then(|d| d.get(..4)) {
        format!("{} ({})", m.title, y)
    } else {
        m.title.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmdb::MediaKind;
    use std::path::PathBuf;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_one_line_title() {
        let m = MultiNorm {
            id: 1,
            media_type: MediaKind::Movie,
            title: "Inception".to_string(),
            original_title: "Inception".to_string(),
            overview: "".to_string(),
            release_date: Some("2010-07-16".to_string()),
            image_path: None,
        };
        assert_eq!(one_line_title(&m), "Inception (2010)");
    }

    #[test]
    fn test_make_block() {
        let m = MultiNorm {
            id: 1,
            media_type: MediaKind::Movie,
            title: "Inception".to_string(),
            original_title: "Inception".to_string(),
            overview: "A thief who steals corporate secrets...".to_string(),
            release_date: Some("2010-07-16".to_string()),
            image_path: None,
        };
        let block = make_block(&m, 10);
        assert!(block.contains("<b>Inception</b> (2010)"));
        assert!(block.contains("A thief wh…"));
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("A & B < C > D"), "A &amp; B &lt; C &gt; D");
    }

    #[tokio::test]
    async fn test_on_search_text_updates_last_search() {
        let server = MockServer::start().await;
        // Mock Telegram API response for send_message
        Mock::given(method("POST"))
            .and(path_regex(".*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "message_id": 1,
                    "date": 1,
                    "chat": {"id": 123, "type": "private", "first_name": "test"},
                    "text": "test"
                }
            })))
            .mount(&server)
            .await;

        std::env::set_var("TELOXIDE_API_URL", server.uri());
        std::env::set_var(
            "TELOXIDE_TOKEN",
            "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11",
        );
        let bot = Bot::from_env();

        let tmdb_server = MockServer::start().await;
        let tmdb = TmdbClient::new_test("token".to_string(), tmdb_server.uri());

        // Mock TMDB response
        let tmdb_response = serde_json::json!({
            "page": 1,
            "total_pages": 1,
            "total_results": 1,
            "results": [
                {
                    "media_type": "movie",
                    "id": 1,
                    "title": "Mock Movie",
                    "original_title": "Mock Movie",
                    "overview": "Overview",
                    "poster_path": "/path.jpg",
                    "release_date": "2023-01-01"
                }
            ]
        });
        Mock::given(method("GET"))
            .and(path("/search/multi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tmdb_response))
            .mount(&tmdb_server)
            .await;

        let storage_path = PathBuf::from("tests/data/tg_test_storage.json");
        let storage = Storage::new(storage_path).await.unwrap();

        let msg = serde_json::from_value::<Message>(serde_json::json!({
            "message_id": 1,
            "date": 1,
            "chat": {"id": 123, "type": "private", "first_name": "test"},
            "text": "test search"
        }))
        .unwrap();

        on_search_text(bot, msg, &tmdb, &storage).await.unwrap();

        let last_search = LAST_SEARCH.read().await;
        let results = last_search.get(&ChatId(123)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Mock Movie");
    }

    #[tokio::test]
    async fn test_full_flow_search_and_add() {
        let server = MockServer::start().await;
        // Mock for sendMessage
        Mock::given(method("POST"))
            .and(path_regex(".*Message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "message_id": 1,
                    "date": 1,
                    "chat": {"id": 456, "type": "private", "first_name": "test"},
                    "text": "test"
                }
            })))
            .mount(&server)
            .await;

        // Mock for answerCallbackQuery
        Mock::given(method("POST"))
            .and(path_regex(".*Query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .mount(&server)
            .await;

        std::env::set_var("TELOXIDE_API_URL", server.uri());
        std::env::set_var(
            "TELOXIDE_TOKEN",
            "456456:ABC-DEF4564ghIkl-zyx57W2v1u456ew11",
        );
        let bot = Bot::from_env();

        let tmdb_server = MockServer::start().await;
        let tmdb = TmdbClient::new_test("token".to_string(), tmdb_server.uri());

        // Mock TMDB response for search
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/search/multi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1,
                "total_pages": 1,
                "total_results": 1,
                "results": [
                    {
                        "media_type": "movie",
                        "id": 456,
                        "title": "Integration Movie",
                        "original_title": "Integration Movie",
                        "overview": "Integration Overview",
                        "poster_path": "/int.jpg",
                        "release_date": "2024-01-01"
                    }
                ]
            })))
            .mount(&tmdb_server)
            .await;

        let storage_path = PathBuf::from("tests/data/integration_test_storage.json");
        let _ = std::fs::remove_file(&storage_path);
        let storage = Storage::new(storage_path.clone()).await.unwrap();

        // 1. Simulate user searching for a movie
        let search_msg = serde_json::from_value::<Message>(serde_json::json!({
            "message_id": 1,
            "date": 1,
            "chat": {"id": 456, "type": "private", "first_name": "test"},
            "text": "integration"
        }))
        .unwrap();

        on_search_text(bot.clone(), search_msg, &tmdb, &storage)
            .await
            .unwrap();

        // Verify LAST_SEARCH is updated
        {
            let last_search = LAST_SEARCH.read().await;
            assert_eq!(last_search.get(&ChatId(456)).unwrap()[0].id, 456);
        }

        // 2. Simulate user clicking "Add" button
        let q = serde_json::from_value::<CallbackQuery>(serde_json::json!({
            "id": "1",
            "from": {"id": 456, "is_bot": false, "first_name": "test"},
            "chat_instance": "1",
            "data": "add:456:movie",
            "message": {
                "message_id": 2,
                "date": 2,
                "chat": {"id": 456, "type": "private", "first_name": "test"},
                "text": "results"
            }
        }))
        .unwrap();

        on_callback(bot, q, &tmdb, &storage).await.unwrap();

        // 3. Verify movie is in storage
        let stored = storage.get(456).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].title, "Integration Movie");

        let _ = std::fs::remove_file(storage_path);
    }
}
fn keyboard_list_two_columns_stored(list: &[StoredMovie]) -> InlineKeyboardMarkup {
    let mut rows = Vec::new();
    for m in list {
        let show = InlineKeyboardButton::callback(
            format!("🎬 {}", one_line_title_stored(m)),
            format!("show:{}:{}", m.id, m.media_type.as_str()),
        );
        let del = InlineKeyboardButton::callback(
            "🗑".to_string(),
            format!("del:{}:{}", m.id, m.media_type.as_str()),
        );
        rows.push(vec![show, del]);
    }
    InlineKeyboardMarkup::new(rows)
}

// отправка альбома из StoredMovie (постеры — по байтам)
async fn send_album_from_stored<R>(
    bot: &R,
    chat_id: ChatId,
    movies: &[StoredMovie],
    common_caption_html: Option<&str>,
) -> Result<(), teloxide::RequestError>
where
    R: Requester<Err = RequestError>,
{
    let mut media: Vec<InputMedia> = Vec::new();
    for (i, m) in movies.iter().take(10).enumerate() {
        if let Some(p) = &m.poster_path {
            let url = format!("https://image.tmdb.org/t/p/w500{}", p);
            if let Ok(bytes) = fetch_image(&url).await {
                let file = InputFile::memory(bytes).file_name(format!("poster_{i}.jpg"));
                if i == 0 {
                    let mut first = InputMediaPhoto::new(file);
                    if let Some(c) = common_caption_html {
                        first.caption = Some(clip(c, 1024));
                        first.show_caption_above_media = true;
                        first.parse_mode = Some(ParseMode::Html);
                    }
                    media.push(InputMedia::Photo(first));
                } else {
                    media.push(InputMedia::Photo(
                        InputMediaPhoto::new(file).show_caption_above_media(true),
                    ));
                }
            }
        }
    }
    if !media.is_empty() {
        bot.send_media_group(chat_id, media).await?;
    }
    Ok(())
}
