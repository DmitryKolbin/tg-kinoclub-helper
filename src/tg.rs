use crate::storage::{Storage, StoredMovie};
use crate::tmdb::{TmdbClient, Movie};
use once_cell::sync::Lazy;
use regex::Regex;
use std::{collections::{HashMap, HashSet}, sync::Arc};
use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt},
    prelude::*,
    types::{
        CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile,
        InputMedia, InputMediaPhoto, ParseMode,
    },
    utils::command::BotCommands,
};
use tokio::sync::RwLock;

/* ====== Хранилище состояния ======
   selected: чат -> выбранные фильмы (макс 10)
   last_search: чат -> результаты последнего поиска (чтобы добавлять по кнопке) */
static SELECTED: Lazy<Arc<RwLock<HashMap<ChatId, Vec<Movie>>>>> =
    Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));
static LAST_SEARCH: Lazy<Arc<RwLock<HashMap<ChatId, Vec<Movie>>>>> =
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
                .branch(
                    dptree::entry()
                        .filter_command::<Command>()
                        .endpoint({
                            let tmdb = tmdb.clone();
                            let storage = storage.clone();
                            move |bot: Bot, msg: Message, cmd: Command| {
                                let tmdb = tmdb.clone();
                                let storage = storage.clone();
                                async move { on_command(bot, msg, cmd, &tmdb, &storage, anonymous, multiple).await }
                            }
                        })
                )
                .branch({
                    let tmdb = tmdb.clone();
                    let storage = storage.clone();
                    dptree::endpoint(move |bot: Bot, msg: Message| {
                        let tmdb = tmdb.clone();
                        let storage = storage.clone();
                        async move { on_search_text(bot, msg, &tmdb, &storage).await }
                    })
                })
        )
        .branch(
            Update::filter_callback_query().endpoint({
                let tmdb = tmdb.clone();
                let storage = storage.clone();
                move |bot: Bot, q: CallbackQuery| {
                    let tmdb = tmdb.clone();
                    let storage = storage.clone();
                    async move { on_callback(bot, q, &tmdb, &storage).await }
                }
            })
        );

    Dispatcher::builder(bot, msg_handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

/* ====== Команды ====== */
async fn on_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    tmdb: &TmdbClient,
    storage: &Storage,
    anonymous: bool,
    multiple: bool,
) -> ResponseResult<()> {
    match cmd {
        Command::Help => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string()).await?;
        }
        Command::Reset => {
            storage.remove_chat(msg.chat.id.0).await.map_err(to_req_err)?;
            LAST_SEARCH.write().await.remove(&msg.chat.id);
            bot.send_message(msg.chat.id, "Список очищен.").await?;
        }
        Command::List => send_list_view(&bot, msg.chat.id, storage).await?,
        Command::Vote => run_vote_flow(&bot, msg.chat.id, tmdb, storage, anonymous, multiple).await?,
    }
    Ok(())
}

/* ====== Поиск по тексту ====== */
async fn on_search_text(
    bot: Bot,
    msg: Message,
    tmdb: &TmdbClient,
    _storage: &Storage,
) -> ResponseResult<()> {
    let Some(query) = message_text_any(&msg) else { return Ok(()); };
    let query = query.trim();
    if query.is_empty() { return Ok(()); }

    // Ищем до 10
    let results = tmdb.search_movies_ru(query, 10).await.map_err(to_req_err)?;
    if results.is_empty() {
        bot.send_message(msg.chat.id, "Ничего не нашёл 😕").await?;
        return Ok(());
    }

    // Сохраним последний поиск (чтобы по кнопке "➕ Добавить" знать, что именно добавлять)
    LAST_SEARCH.write().await.insert(msg.chat.id, results.clone());

    // Сообщение с названиями + краткими описаниями
    let mut blocks = Vec::new();
    for m in &results {
        blocks.push(make_block(m, 600)); // описания укоротим
    }
    let text = join_blocks(blocks, 3500); // запас до 4096
    bot.send_message(msg.chat.id, text).parse_mode(ParseMode::Html).await?;

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
async fn on_callback(
    bot: Bot,
    q: CallbackQuery,
    tmdb: &TmdbClient,
    storage: &Storage,
) -> ResponseResult<()> {
    let Some(data) = q.data.clone() else { return Ok(()); };
    let chat_id = q.message.as_ref().map(|m| m.chat().id).unwrap_or(ChatId(0));
    let mut parts = data.splitn(2, ':');
    let cmd = parts.next().unwrap_or("");
    let id_str = parts.next().unwrap_or("");
    let Ok(id) = id_str.parse::<u64>() else { return Ok(()); };

    match cmd {
        "add" => {
            let movie_opt = {
                let map = LAST_SEARCH.read().await;
                map.get(&chat_id).and_then(|v| v.iter().find(|m| m.id == id)).cloned()
            };
            if let Some(m) = movie_opt {
                let added = storage.add_movie(chat_id.0, StoredMovie {
                    id: m.id,
                    title: m.title.clone(),
                    original_title: m.original_title.clone(),
                    poster_path: m.poster_path.clone(),
                    release_date: m.release_date.clone(),
                }).await.map_err(to_req_err)?;
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
            let removed = storage.delete_movie(chat_id.0, id).await.map_err(to_req_err)?;
            if removed {
                answer_cb(&bot, &q, "Удалено").await?;
                send_list_view(&bot, chat_id, storage).await?;
            } else {
                answer_cb(&bot, &q, "Не найдено в списке").await?;
            }
        }
        "show" => {
            if let Some(m) = tmdb.movie_details_ru(id).await.map_err(to_req_err)? {
                let text = make_block(&m, 2000);
                bot.send_message(chat_id, text).parse_mode(ParseMode::Html).await?;
                if let Some(p) = &m.poster_path {
                    let url = format!("https://image.tmdb.org/t/p/w500{}", p);
                    if let Ok(bytes) = fetch_image(&url).await {
                        bot.send_photo(chat_id, InputFile::memory(bytes).file_name(format!("poster_{}.jpg", m.id))).await?;
                    }
                }
                answer_cb(&bot, &q, "Показал").await?;
            } else {
                answer_cb(&bot, &q, "Не удалось получить данные").await?;
            }
        }
        _ => { answer_cb(&bot, &q, "Неизвестная команда").await?; }
    }
    Ok(())
}


/* ====== /list: показать список с кнопками ====== */
async fn send_list_view(bot: &Bot, chat: ChatId, storage: &Storage) -> ResponseResult<()> {
    let list = storage.get(chat.0).await;
    if list.is_empty() {
        bot.send_message(chat, "Список пуст. Пришли название — добавлю варианты.").await?;
        return Ok(());
    }
    let mut lines = Vec::new();
    for m in &list {
        lines.push(one_line_title_stored(m));
    }
    let txt = format!("<b>В списке ({}/10):</b>\n{}", list.len(), lines.join("\n"));
    let kb = keyboard_list_two_columns_stored(&list);
    bot.send_message(chat, txt).parse_mode(ParseMode::Html).reply_markup(kb).await?;
    Ok(())
}

async fn run_vote_flow(bot: &Bot, chat: ChatId, tmdb: &TmdbClient, storage: &Storage, anonymous:bool, multiple_ans: bool) -> ResponseResult<()> {
    let list = storage.get(chat.0).await;
    if list.len() < 2 {
        bot.send_message(chat, "Нужно минимум 2 фильма в списке. Добавь и повтори /vote.").await?;
        return Ok(());
    }
    // опрос
    let options: Vec<teloxide::types::InputPollOption> =
        list.iter().map(|m| teloxide::types::InputPollOption::new(one_line_title_stored(m))).collect();
    bot.send_poll(chat, "Что смотрим?", options).is_anonymous(anonymous).allows_multiple_answers(multiple_ans).await?;

    // альбом постеров (короткий общий caption)
    send_album_from_stored(bot, chat, &list, Some("<b>Постеры</b>")).await?;

    // описания + трейлеры (тянем детали по id)
    let mut blocks = Vec::new();
    let mut trailer_lines = Vec::new();
    for sm in &list {
        if let Some(m) = tmdb.movie_details_ru(sm.id).await.map_err(to_req_err)? {
            let trailer = tmdb.best_trailer_url(m.id).await.map_err(to_req_err).ok().flatten();
            if let Some(t) = trailer.as_ref() {
                trailer_lines.push(format!("• <b>{}</b>: {}", html_escape(&m.title), html_escape(t)));
            }
            blocks.push(make_block(&m, 1200));
        }
    }
    let text = join_blocks(blocks, 4000 - 50);
    for part in split_by_chars(&text, 4000) {
        bot.send_message(chat, part).parse_mode(ParseMode::Html).await?;
    }
    if !trailer_lines.is_empty() {
        bot.send_message(chat, format!("<b>Трейлеры</b>\n{}", trailer_lines.join("\n")))
            .parse_mode(ParseMode::Html)
            .await?;
    }
    bot.send_message(chat, "Данные и изображения: © TMDB").await?;
    Ok(())
}

/* ====== Кнопки ====== */

fn keyboard_add_results(results: &[Movie]) -> InlineKeyboardMarkup {
    // по 1 в строке
    let mut rows = Vec::new();
    let mut row = Vec::new();
    for m in results {
        let btn = InlineKeyboardButton::callback(format!("➕ {}", one_line_title(m)), format!("add:{}", m.id));
        row.push(btn);
        rows.push(row);
        row = Vec::new();

    }
    if !row.is_empty() { rows.push(row); }
    InlineKeyboardMarkup::new(rows)
}


/* ====== Вспомогательные ====== */

fn one_line_title(m: &Movie) -> String {
    if let Some(y) = m.release_date.as_ref().and_then(|d| d.get(..4)) {
        format!("{} ({})", m.title, y)
    } else {
        m.title.clone()
    }
}

fn make_block(m: &Movie, overview_limit: usize) -> String {
    let year = m.release_date.as_ref().and_then(|d| d.get(..4)).unwrap_or("");
    let title = html_escape(&m.title);
    let mut body = if m.overview.trim().is_empty() {
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
        let piece = if out.is_empty() { b } else { format!("\n\n{}", b) };
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
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max).collect::<String>() + "…" }
}

fn split_by_chars(s: &str, max: usize) -> Vec<String> {
    if s.chars().count() <= max { return vec![s.to_string()]; }
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if cur.chars().count() >= max { out.push(cur); cur = String::new(); }
        cur.push(ch);
    }
    if !cur.is_empty() { out.push(cur); }
    out
}

async fn answer_cb(bot: &Bot, q: &CallbackQuery, text: &str) -> ResponseResult<()> {
    bot.answer_callback_query(q.id.clone())
        .text(text)
        .show_alert(false)
        .await?;
    Ok(())
}

fn message_text_any(msg: &Message) -> Option<String> {
    if let Some(t) = msg.text() { return Some(t.to_string()); }
    if let Some(c) = msg.caption() { return Some(c.to_string()); }
    None
}

/* ====== Загрузка постера байтами (устойчиво к редиректам/CDN) ====== */
async fn fetch_image(url: &str) -> Result<Vec<u8>, teloxide::RequestError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; tg-bot/1.0)")
        .build()
        .map_err(to_req_err)?;
    let resp = client.get(url)
        .header(reqwest::header::ACCEPT, "image/*")
        .send().await.map_err(to_req_err)?;
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
    teloxide::RequestError::Io(std::sync::Arc::new(std::io::Error::new(
        std::io::ErrorKind::Other,
        e.to_string(),
    )))
}


fn one_line_title_stored(m: &StoredMovie) -> String {
    if let Some(y) = m.release_date.as_ref().and_then(|d| d.get(..4)) {
        format!("{} ({})", m.title, y)
    } else {
        m.title.clone()
    }
}
fn keyboard_list_two_columns_stored(list: &[StoredMovie]) -> InlineKeyboardMarkup {
    let mut rows = Vec::new();
    for m in list {
        let show = InlineKeyboardButton::callback(
            format!("🎬 {}", one_line_title_stored(m)),
            format!("show:{}", m.id),
        );
        let del = InlineKeyboardButton::callback("🗑".to_string(), format!("del:{}", m.id));
        rows.push(vec![show, del]);
    }
    InlineKeyboardMarkup::new(rows)
}

// отправка альбома из StoredMovie (постеры — по байтам)
async fn send_album_from_stored(
    bot: &teloxide::Bot,
    chat_id: ChatId,
    movies: &[StoredMovie],
    common_caption_html: Option<&str>,
) -> Result<(), teloxide::RequestError> {
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
                    media.push(InputMedia::Photo(InputMediaPhoto::new(file)));
                }
            }
        }
    }
    if !media.is_empty() {
        bot.send_media_group(chat_id, media).await?;
    }
    Ok(())
}