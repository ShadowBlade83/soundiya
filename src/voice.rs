use std::{collections::VecDeque, io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::Instant,
};

pub const CONTROL: &str = "/run/soundiya.control";
const MAX_FRAME: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Play(String),
    /// A playlist or album by name; falls back to a track search when none is found.
    Playlist(String),
    /// SoundCloud's charts: `trending` picks the rising list instead of the top one.
    Chart { trending: bool, genre: &'static str },
    /// More tracks like the one that plays.
    Related,
    /// Alice says which track plays.
    NowPlaying,
    Favorites,
    Resume,
    Pause,
    Next,
    Previous,
    /// The Alice button: pause what plays, resume what is paused.
    Toggle,
    /// The phrase names Yandex ("... на Яндекс Музыке"): Alice plays it herself.
    Handover,
}
pub struct Event {
    pub intent: Intent,
    pub received: Instant,
    pub from_voice: bool,
}

/// Lower case, no punctuation, single spaces; the polite and filler words around a command
/// are dropped so "Алиса, а можешь пожалуйста включить Radiohead?" reads like "включи radiohead".
fn normalize(text: &str) -> String {
    let lower = text.to_lowercase().replace('ё', "е");
    let plain: String = lower
        .chars()
        .map(|c| if matches!(c, ',' | '!' | '?' | '"' | '«' | '»' | ';' | ':' | '(' | ')' | '…') { ' ' } else { c })
        .collect();
    let mut words: Vec<&str> = plain
        .split_whitespace()
        .filter(|w| !matches!(*w, "пожалуйста" | "плиз"))
        .collect();
    const LEADING: [&str; 12] = [
        "алиса", "ну", "слушай", "пожалуйста", "а", "и", "так", "тогда", "скажи", "эй", "окей", "ладно",
    ];
    const TRAILING: [&str; 7] = ["пожалуйста", "плиз", "сейчас", "мне", "нам", "пожалуйста-пожалуйста", "быстрее"];
    while words.first().is_some_and(|w| LEADING.contains(w)) {
        words.remove(0);
    }
    while words.last().is_some_and(|w| TRAILING.contains(w)) {
        words.pop();
    }
    words.join(" ").trim_matches('.').trim().to_owned()
}

const LIKES: &[&str] = &["лайк", "поставь лайк", "добавь в избранное", "сохрани песню", "мне нравится", "лайкни"];
const PAUSE: &[&str] = &[
    "стоп", "пауза", "выключи", "выключись", "отключи", "останови", "остановись", "останови музыку",
    "останови воспроизведение", "выключи музыку", "выключи песню", "отключи музыку", "поставь на паузу",
    "поставь паузу", "на паузу", "приостанови", "приостанови музыку", "хватит", "хватит музыки",
    "стоп музыка", "музыку на паузу",
];
const NEXT: &[&str] = &[
    "дальше", "следующий", "следующая", "следующую", "следующий трек", "следующая песня",
    "следующую песню", "переключи", "переключи трек", "переключи песню", "пропусти", "пропусти трек",
    "пропусти песню", "скип", "другую песню", "другой трек", "включи другую", "включи другое",
    "включи следующий", "включи следующую", "включи следующий трек", "включи следующую песню",
    "давай дальше", "давай следующую",
];
const PREVIOUS: &[&str] = &[
    "назад", "предыдущий", "предыдущая", "предыдущую", "предыдущий трек", "предыдущая песня",
    "предыдущую песню", "прошлая песня", "прошлую песню", "прошлый трек", "вернись", "верни предыдущую",
    "верни предыдущий", "включи предыдущий", "включи предыдущую", "включи прошлую", "поставь предыдущую",
];
const RESUME: &[&str] = &[
    "продолжи", "продолжай", "продолжи музыку", "продолжай музыку", "продолжить", "продолжить музыку",
    "играй дальше", "верни музыку", "включи музыку", "включи обратно", "сними с паузы", "сними паузу",
    "возобнови", "возобнови музыку",
];
const FAVORITES: &[&str] = &[
    "избранное", "любимое", "любимые", "любимые треки", "любимую музыку", "мою музыку", "мою любимую музыку",
    "мои лайки", "мои треки", "мои песни", "лайкнутое", "то что мне нравится", "моя музыка",
];
const RELATED: &[&str] = &[
    "похожее", "похожие", "похожую музыку", "похожие треки", "похожие песни", "что нибудь похожее",
    "что-нибудь похожее", "что то похожее", "что-то похожее", "такое же", "такую же музыку",
    "такие же песни", "еще такое", "еще похожее",
];
const TOP: &[&str] = &[
    "популярное", "популярную музыку", "что нибудь популярное", "что-нибудь популярное", "топ", "чарты",
    "хиты", "что популярно", "что сейчас популярно", "топ треков", "топ чартов",
];
const TRENDING: &[&str] = &["тренды", "в тренде", "горячее", "новинки", "что в тренде", "новое", "свежее"];
const GENRES: &[(&str, &str)] = &[
    ("хип-хоп", "hiphoprap"), ("хип хоп", "hiphoprap"), ("рэп", "hiphoprap"), ("реп", "hiphoprap"),
    ("рок", "rock"), ("поп", "pop"), ("электрон", "electronic"), ("танцевальн", "danceedm"),
    ("метал", "metal"), ("джаз", "jazzblues"), ("блюз", "jazzblues"), ("классик", "classical"),
    ("классич", "classical"),
    ("регги", "reggae"), ("кантри", "country"), ("хаус", "house"), ("техно", "techno"),
    ("транс", "trance"), ("трэп", "trap"), ("трап", "trap"), ("эмбиент", "ambient"),
    ("латин", "latin"), ("саундтрек", "soundtrack"),
];

/// The SoundCloud genre a phrase such as "популярный рэп" names.
pub fn genre_of(text: &str) -> Option<&'static str> {
    GENRES
        .iter()
        // A stem plus an ending, so "поп" matches "попсу" but not "популярное".
        .find(|(stem, _)| {
            text.split_whitespace()
                .any(|w| w.starts_with(stem) && w.chars().count() <= stem.chars().count() + 5)
        })
        .map(|(_, id)| *id)
}

fn now_playing(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    let has = |part: &str| words.iter().any(|w| w.starts_with(part));
    if words.len() > 8 {
        return false;
    }
    (text.starts_with("что") && (has("играет") || has("играют") || text.contains("за песня") || text.contains("за трек") || text.contains("за музыка") || text.contains("за мелодия")))
        || (text.starts_with("как называется") && (has("песня") || has("трек") || has("композиция") || has("мелодия")))
        || (text.starts_with("кто") && (has("поет") || has("поют") || has("исполняет") || has("исполнитель") || has("автор")))
        || text.starts_with("название песни")
        || text.starts_with("название трека")
        || text.starts_with("что за исполнитель")
}

const VERBS: &[&str] = &[
    "давай послушаем", "давай включим", "давай поставим", "давай запустим", "я хочу послушать",
    "хочу послушать", "хочу услышать", "дай послушать", "можешь включить", "можешь поставить",
    "можешь запустить", "можешь сыграть", "включить", "включай", "включи", "вруби", "врубай", "врубить",
    "поставить", "поставь", "запустить", "запусти", "сыграй", "играй", "проиграй", "заведи",
    "воспроизведи", "найди", "найти", "послушаем", "послушать", "поищи", "покажи",
];
/// Verbs that Alice also uses for things that are not music.
const SOFT_VERBS: &[&str] = &["найди", "найти", "поищи", "покажи", "заведи"];

pub fn intent(text: &str) -> Option<Intent> {
    let text = normalize(text);
    if text.is_empty() || LIKES.contains(&text.as_str()) {
        return None;
    }
    // "включи Сплин на Яндекс Музыке", "Яндекс музыка, включи ...": leave it to Alice.
    if text.split_whitespace().any(|w| w.starts_with("яндекс")) {
        return Some(Intent::Handover);
    }
    // Longest verbs first, so "хочу послушать" is not read as "послушать".
    let mut verbs = VERBS.to_vec();
    verbs.sort_by_key(|v| std::cmp::Reverse(v.len()));
    let verb = verbs.iter().find(|v| text.strip_prefix(*v).is_some_and(|rest| rest.starts_with(' ')));
    let rest = match verb {
        Some(v) => text[v.len()..].trim(),
        None => text.as_str(),
    };
    // "включи предыдущую песню" is a command whether or not the set lists it with the verb.
    let exact = |set: &[&str]| set.contains(&text.as_str()) || (verb.is_some() && set.contains(&rest));
    if exact(PAUSE) {
        return Some(Intent::Pause);
    }
    if exact(NEXT) {
        return Some(Intent::Next);
    }
    if exact(PREVIOUS) {
        return Some(Intent::Previous);
    }
    if exact(RESUME) {
        return Some(Intent::Resume);
    }
    if now_playing(&text) {
        return Some(Intent::NowPlaying);
    }
    let soft =verb.is_some_and(|v| SOFT_VERBS.contains(v));
    if FAVORITES.contains(&rest) && (verb.is_some() || rest == "моя музыка" || rest == "мои лайки" || rest == "избранное") {
        return Some(Intent::Favorites);
    }
    // The remaining commands need a verb; a bare phrase is a question for Alice.
    verb?;
    if RELATED.contains(&rest) || (rest.starts_with("похож") && rest.split_whitespace().count() <= 3) {
        return Some(Intent::Related);
    }
    let genre = genre_of(rest);
    let chart_rest = rest.strip_prefix("музыку ").unwrap_or(rest);
    let chart_words: Vec<&str> = chart_rest.split_whitespace().collect();
    let first = chart_words.first().copied().unwrap_or("");
    // A lone keyword, or a keyword plus a genre ("популярный рэп"), but not "хиты Земфиры".
    let short = chart_words.len() == 1 || (genre.is_some() && chart_words.len() <= 3);
    let popular = first.starts_with("популярн") && (short || chart_words.len() == 2);
    if TOP.contains(&chart_rest) || popular || (matches!(first, "топ" | "хиты" | "чарты") && short) {
        return Some(Intent::Chart { trending: false, genre: genre.unwrap_or("all-music") });
    }
    if TRENDING.contains(&chart_rest) || (matches!(first, "новинки" | "тренды" | "тренд") && short) {
        return Some(Intent::Chart { trending: true, genre: genre.unwrap_or("all-music") });
    }
    // "музыку, похожую на X" is a search for X.
    if rest.contains("похож")
        && let Some((_, tail)) = rest.split_once(" на ")
        && !tail.trim().is_empty()
    {
        return Some(Intent::Play(tail.trim().to_owned()));
    }
    for prefix in [
        "мой плейлист ", "мою подборку ", "мой альбом ", "плейлист ", "альбом ", "подборку ", "сборник ",
        "playlist ",
    ] {
        if let Some(name) = rest.strip_prefix(prefix) {
            let name = name.trim().trim_start_matches("под названием ").trim();
            return (!name.is_empty() && name.len() <= 400).then(|| Intent::Playlist(name.to_owned()));
        }
    }
    // Moods and occasions are better served by a playlist than by one track.
    if ["музыку для ", "песни для ", "что нибудь для ", "что-нибудь для "].iter().any(|p| rest.starts_with(p)) {
        let name = rest.split_once(" для ").map_or(rest, |(_, occasion)| occasion);
        return Some(Intent::Playlist(format!("для {name}")));
    }
    let music_prefix = [
        "песню ", "песни ", "трек ", "треки ", "музыку ", "группу ", "исполнителя ", "композицию ",
        "что нибудь из ", "что-нибудь из ", "что нибудь от ", "что-нибудь от ",
    ];
    let explicit = music_prefix.iter().find_map(|p| rest.strip_prefix(p));
    if soft && explicit.is_none() {
        return None;
    }
    let query = explicit.unwrap_or(rest).trim().trim_matches('\'').trim();
    if query.is_empty() || query.len() > 400 {
        return None;
    }
    let home = [
        "свет", "освещение", "ламп", "люстр", "торшер", "ночник", "розетк", "телевизор", "тв",
        "кондиционер", "вентилятор", "обогревател", "пылесос", "робот", "чайник", "будильник", "таймер",
        "блютуз", "bluetooth", "режим", "экран", "микрофон", "громк", "звук", "отоплен", "увлажнител",
        "очистител", "зарядк", "радио", "новост", "сказк", "аудиокниг", "подкаст", "фильм", "сериал",
        "мультик", "канал", "видео", "погод", "ютуб", "youtube", "телеканал", "напомин", "ролик",
    ];
    if explicit.is_none()
        && query
            .split_whitespace()
            .any(|word| home.iter().any(|p| word.starts_with(p)))
    {
        return None;
    }
    Some(Intent::Play(query.to_owned()))
}

/// What Alice should read aloud for a track: no brackets, links or decoration.
pub fn spoken(label: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in label.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            c if c.is_alphanumeric() || c.is_whitespace() || matches!(c, '-' | '\'' | '.' | '&' | ',') => out.push(c),
            _ => out.push(' '),
        }
    }
    let mut text = out.split_whitespace().collect::<Vec<_>>().join(" ");
    text.truncate(text.char_indices().nth(160).map_or(text.len(), |(i, _)| i));
    text
}

fn native_volume_request(text: &str) -> bool {
    let normalized = text.trim().to_lowercase().replace('ё', "е");
    let text = normalized.strip_prefix("алиса, ").unwrap_or(&normalized);
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if matches!(
        first.trim_end_matches(['.', '!', '?']),
        "громче" | "тише" | "погромче" | "потише"
    ) {
        return true;
    }
    if first == "громкость" {
        return words.next().is_some();
    }
    matches!(
        first,
        "сделай"
            | "поставь"
            | "установи"
            | "измени"
            | "увеличь"
            | "уменьши"
            | "добавь"
            | "убавь"
            | "прибавь"
            | "включи"
            | "выключи"
            | "отключи"
    ) && words.next().is_some_and(|word| {
        matches!(
            word,
            "звук" | "громкость" | "громче" | "тише" | "погромче" | "потише"
        )
    })
}

// QuasarMessage uses {252: serialized payload, 253: message type}.
// Read AliceState activity and final voice recognition without retaining other payloads.
fn varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*offset)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Some(value);
        }
    }
    None
}
#[derive(Clone, Copy)]
enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}
fn field(data: &[u8], wanted: u64) -> Option<Value<'_>> {
    let mut offset = 0;
    while offset < data.len() {
        let key = varint(data, &mut offset)?;
        if key >> 3 == 0 {
            return None;
        }
        let value = match key & 7 {
            0 => Value::Number(varint(data, &mut offset)?),
            2 => {
                let len = usize::try_from(varint(data, &mut offset)?).ok()?;
                let end = offset.checked_add(len)?;
                let bytes = data.get(offset..end)?;
                offset = end;
                Value::Bytes(bytes)
            }
            1 | 5 => {
                let len = if key & 7 == 1 { 8 } else { 4 };
                offset = offset.checked_add(len)?;
                if offset > data.len() {
                    return None;
                }
                continue;
            }
            _ => return None,
        };
        if key >> 3 == wanted {
            return Some(value);
        }
    }
    None
}
fn bytes(data: &[u8], number: u64) -> Option<&[u8]> {
    if let Value::Bytes(v) = field(data, number)? {
        Some(v)
    } else {
        None
    }
}
fn number(data: &[u8], field_number: u64) -> Option<u64> {
    if let Value::Number(v) = field(data, field_number)? {
        Some(v)
    } else {
        None
    }
}
/// AliceState activity number (0 idle, 1 listening, 2 speaking, 3 busy, 5 shazam, 6 none).
fn alice_state(data: &[u8]) -> Option<u64> {
    if number(data, 253) != Some(3) {
        return None;
    }
    Some(number(bytes(data, 252)?, 1).unwrap_or(0))
}
fn alice_busy(data: &[u8]) -> Option<bool> {
    if number(data, 253) != Some(3) {
        return None;
    }
    let state = bytes(data, 252)?;
    match number(state, 1).unwrap_or(0) {
        0 | 6 => Some(false),        // IDLE / NONE
        1 | 2 | 3 | 5 => Some(true), // LISTENING / SPEAKING / BUSY / SHAZAM
        _ => None,
    }
}
fn recognized(data: &[u8]) -> Option<(String, String)> {
    if number(data, 253) != Some(3) {
        return None;
    }
    let state = bytes(data, 252)?;
    // Quasar marks the phrase final while still LISTENING, about a second before it turns
    // BUSY. Acting then lets the cancel reach the server before Alice's own reply, such as
    // a Yandex Plus refusal, which would otherwise be spoken.
    if !matches!(number(state, 1), Some(1 | 3)) {
        return None;
    }
    let result = bytes(state, 15)?;
    if number(result, 2) != Some(1) || number(result, 4) != Some(1) {
        return None;
    }
    let text = std::str::from_utf8(bytes(result, 1)?).ok()?;
    if text.is_empty() || text.len() > 512 || Some(text.as_bytes()) != bytes(state, 2) {
        return None;
    }
    let request = std::str::from_utf8(bytes(state, 7)?).ok()?;
    if request.len() != 36 {
        return None;
    }
    Some((request.to_owned(), text.to_owned()))
}
fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn put_number(out: &mut Vec<u8>, field: u64, value: u64) {
    put_var(out, field << 3);
    put_var(out, value);
}
fn put_bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    put_var(out, (field << 3) | 2);
    put_var(out, value.len() as u64);
    out.extend_from_slice(value);
}
fn rpc(capability: &str, field: u64, method: &[u8]) -> Vec<u8> {
    let mut remoting = Vec::new();
    put_bytes(&mut remoting, 7, capability.as_bytes());
    put_bytes(&mut remoting, field, method);
    let mut message = Vec::new();
    put_bytes(&mut message, 252, &remoting);
    put_number(&mut message, 253, 210);
    let mut frame = (message.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&message);
    frame
}
pub async fn cancel_alice(address: SocketAddr) -> io::Result<()> {
    // Native CLI 'c': AliceCapabilityMethod kind 3, cancel the conversation.
    send_native(address, rpc("AliceCapability", 8, &[8, 3])).await
}
/// Has Alice read `text` aloud in her own voice (AliceCapabilityMethod START_VOCALIZE).
pub async fn say(address: SocketAddr, text: &str) -> io::Result<()> {
    // Closing the connection right after the request seems to cancel the speech with it, so
    // the connection stays open while she talks (about 15 characters a second at most).
    let mut socket = tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(address))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native connect timeout"))??;
    socket.write_all(&vocalize(text)).await?;
    let talking = Duration::from_secs(3) + Duration::from_millis(100 * text.chars().count() as u64);
    let mut sink = [0u8; 4096];
    let _ = tokio::time::timeout(talking, async {
        // Read and drop whatever the station sends until the sentence must be over.
        while socket.read(&mut sink).await.is_ok_and(|n| n > 0) {}
    })
    .await;
    Ok(())
}
fn vocalize(text: &str) -> Vec<u8> {
    let mut directive = Vec::new();
    put_bytes(&mut directive, 1, text.as_bytes());
    put_number(&mut directive, 2, 2); // ESynthesisMode.Interrupt
    let mut method = vec![8, 18]; // method = START_VOCALIZE
    put_bytes(&mut method, 13, &directive);
    rpc("AliceCapability", 8, &method)
}
pub async fn pause_native(address: SocketAddr) -> io::Result<()> {
    // Native CLI 'o': PlaybackControlCapabilityMethod kind 1 (pause).
    let mut source = Vec::new();
    put_number(&mut source, 1, 14);
    put_number(&mut source, 2, 3);
    put_number(&mut source, 3, 1);
    put_bytes(&mut source, 4, uuid::Uuid::new_v4().to_string().as_bytes());
    put_bytes(&mut source, 6, b"SoundcloudVoice");
    let mut method = vec![8, 1];
    put_bytes(&mut method, 5, &source);
    put_number(&mut method, 7, 0);
    send_native(address, rpc("PlaybackControlCapability", 13, &method)).await
}
// The first cancel can land while Alice is still LISTENING and be forgotten once her own
// answer (such as the Yandex Plus offer) starts, so repeat it briefly to catch that moment.
/// The repeats below land 0.6 s and 1.5 s after the first cancel; anything Alice should say
/// afterwards has to wait for this long.
pub const CANCELS_DONE: Duration = Duration::from_millis(1900);
fn cancel_again(address: SocketAddr) {
    tokio::spawn(async move {
        for delay in [600, 900] {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let _ = cancel_alice(address).await;
        }
    });
}
/// Silences Alice and the station's own player before this receiver starts a track.
pub async fn prepare_playback(address: Option<SocketAddr>, from_voice: bool) -> io::Result<()> {
    if let Some(address) = address {
        if from_voice {
            cancel_alice(address).await?;
            cancel_again(address);
        }
        let _ = pause_native(address).await;
    }
    Ok(())
}
/// Commands that do not start playback only need Alice silenced; pausing the native player
/// would just stop the music the user is listening to.
pub async fn prepare_command(address: Option<SocketAddr>, from_voice: bool) -> io::Result<()> {
    if let Some(address) = address
        && from_voice
    {
        cancel_alice(address).await?;
        cancel_again(address);
    }
    Ok(())
}
async fn send_native(address: SocketAddr, frame: Vec<u8>) -> io::Result<()> {
    tokio::time::timeout(Duration::from_millis(500), async {
        let mut socket = TcpStream::connect(address).await?;
        socket.write_all(&frame).await?;
        socket.shutdown().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native command timeout"))?
}
async fn read_events(
    socket: &mut (impl tokio::io::AsyncRead + Unpin),
    sender: &mpsc::Sender<Event>,
    ducking: &crate::ducking::Ducking,
    guard: &crate::audio_guard::AudioGuard,
    seen: &mut VecDeque<String>,
) -> io::Result<()> {
    let mut data = Vec::with_capacity(4096);
    let mut was_busy = false;
    let mut last_state = None;
    loop {
        let size = socket.read_u32_le().await? as usize;
        if size == 0 || size > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native frame length",
            ));
        }
        data.resize(size, 0);
        socket.read_exact(&mut data).await?;
        if let Some(state) = alice_state(&data)
            && last_state != Some(state)
        {
            last_state = Some(state);
            eprintln!("voice: alice state {state}");
        }
        if let Some(busy) = alice_busy(&data) {
            ducking.set_busy(busy);
            // Her own player may have started with the answer; pause it once she is done.
            if was_busy && !busy {
                guard.nudge();
            }
            was_busy = busy;
        }
        if let Some((request, text)) = recognized(&data)
            && !seen.contains(&request)
        {
            seen.push_back(request);
            if seen.len() > 64 {
                seen.pop_front();
            }
            if native_volume_request(&text) {
                ducking.volume.native_command();
            }
            let intent = intent(&text);
            eprintln!("voice: heard '{text}' -> {intent:?}");
            if let Some(intent) = intent {
                let _ = sender.try_send(Event {
                    intent,
                    received: Instant::now(),
                    from_voice: true,
                });
            }
        }
        if data.capacity() > 64 * 1024 {
            data = Vec::with_capacity(4096);
        }
    }
}
pub async fn watch(
    sender: mpsc::Sender<Event>,
    ducking: Arc<crate::ducking::Ducking>,
    guard: Arc<crate::audio_guard::AudioGuard>,
    address: SocketAddr,
) {
    let mut seen: VecDeque<String> = VecDeque::new();
    loop {
        let result: io::Result<()> = async {
            let mut socket = TcpStream::connect(address).await?;
            eprintln!("voice: connected to native Alice events");
            read_events(&mut socket, &sender, &ducking, &guard, &mut seen).await
        }
        .await;
        ducking.set_busy(false);
        if sender.is_closed() {
            return;
        }
        if result.is_err() {
            eprintln!("voice: native connection lost; retrying");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_commands_use_the_profile_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move {
            let mut objects = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let size = socket.read_u32_le().await.unwrap();
                let mut msg = vec![0; size as usize];
                socket.read_exact(&mut msg).await.unwrap();
                assert_eq!(number(&msg, 253), Some(210));
                objects.push(
                    std::str::from_utf8(bytes(bytes(&msg, 252).unwrap(), 7).unwrap())
                        .unwrap()
                        .to_owned(),
                );
            }
            objects
        });
        prepare_playback(Some(address), true).await.unwrap();
        let objects = tokio::time::timeout(Duration::from_secs(2), receiver)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(objects, ["AliceCapability", "PlaybackControlCapability"]);
        prepare_playback(None, true).await.unwrap();
    }
    #[test]
    fn native_volume_commands_are_distinct_from_music_and_questions() {
        for text in [
            "громче",
            "тише!",
            "громкость пять",
            "сделай потише",
            "выключи звук",
            "Алиса, установи громкость 3",
        ] {
            assert!(native_volume_request(text), "{text}");
        }
        for text in [
            "какая погода",
            "почему музыка тихая",
            "включи песню Тише",
            "включи Radiohead",
            "поставь таймер",
        ] {
            assert!(!native_volume_request(text), "{text}");
        }
    }
    #[test]
    fn recognizes_music_commands_and_leaves_household_commands_alone() {
        assert_eq!(
            intent("Включи radiohead"),
            Some(Intent::Play("radiohead".into()))
        );
        assert_eq!(
            intent("Включи песню Всё идёт по плану"),
            Some(Intent::Play("все идет по плану".into()))
        );
        assert_eq!(intent("Останови музыку"), Some(Intent::Pause));
        assert_eq!(intent("Алиса выключи"), Some(Intent::Pause));
        assert_eq!(intent("Включи музыку"), Some(Intent::Resume));
        assert_eq!(intent("следующий трек"), Some(Intent::Next));
        // SoundCloud's bot protection blocks likes from the speaker, so they are left to Alice.
        assert_eq!(intent("Поставь лайк"), None);
        assert_eq!(intent("лайк"), None);
        assert_eq!(intent("Алиса, включи Сплин на Яндекс Музыке"), Some(Intent::Handover));
        assert_eq!(intent("Яндекс музыка включи сплин"), Some(Intent::Handover));
        assert_eq!(intent("включи сплин в яндексе"), Some(Intent::Handover));
        assert_eq!(intent("включи избранное"), Some(Intent::Favorites));
        for s in [
            "включи свет",
            "включи свет в спальне",
            "поставь таймер на пять минут",
            "включи телевизор",
            "какая погода",
        ] {
            assert_eq!(intent(s), None, "{s}");
        }
    }
    #[test]
    fn understands_polite_and_varied_wording() {
        let play = |q: &str| Some(Intent::Play(q.into()));
        for (said, want) in [
            ("Алиса, а можешь пожалуйста включить Radiohead?", play("radiohead")),
            ("Врубай Linkin Park", play("linkin park")),
            ("Давай послушаем Земфиру", play("земфиру")),
            ("Хочу послушать что-нибудь из Кино", play("кино")),
            ("Алиса, найди песню Группа крови", play("группа крови")),
            ("Включи, пожалуйста, песню Звезда по имени Солнце", play("звезда по имени солнце")),
            ("Пропусти трек", Some(Intent::Next)),
            ("Алиса, включи предыдущую песню", Some(Intent::Previous)),
            ("Сними с паузы", Some(Intent::Resume)),
            ("Хватит", Some(Intent::Pause)),
            ("Включи мои лайки", Some(Intent::Favorites)),
            ("Алиса, включи мою любимую музыку", Some(Intent::Favorites)),
            ("Включи что-нибудь похожее", Some(Intent::Related)),
            ("Включи музыку, похожую на Radiohead", play("radiohead")),
            ("Включи плейлист для тренировки", Some(Intent::Playlist("для тренировки".into()))),
            ("Включи альбом OK Computer", Some(Intent::Playlist("ok computer".into()))),
            ("Алиса, включи мой плейлист Для дороги", Some(Intent::Playlist("для дороги".into()))),
            ("Включи музыку для сна", Some(Intent::Playlist("для сна".into()))),
            ("Включи популярное", Some(Intent::Chart { trending: false, genre: "all-music" })),
            ("Включи популярный рэп", Some(Intent::Chart { trending: false, genre: "hiphoprap" })),
            ("Включи новинки", Some(Intent::Chart { trending: true, genre: "all-music" })),
            ("Что сейчас играет?", Some(Intent::NowPlaying)),
            ("Алиса, как называется эта песня", Some(Intent::NowPlaying)),
            ("Кто это поёт", Some(Intent::NowPlaying)),
            ("Что это за песня играет", Some(Intent::NowPlaying)),
        ] {
            assert_eq!(intent(said), want, "{said}");
        }
        // Things that belong to Alice or to the home stay with her.
        for said in [
            "Найди рецепт борща",
            "Включи радио",
            "Включи новости",
            "Включи сказку",
            "Включи сериал",
            "Включи фильм",
            "Поставь будильник на семь",
            "Что играет по телевизору в пятницу вечером после программы время",
            "Кто президент",
            "Как называется столица Франции",
            "Включи свет на кухне",
        ] {
            assert_eq!(intent(said), None, "{said}");
        }
    }
    #[test]
    fn spoken_titles_drop_decoration() {
        assert_eq!(spoken("Radiohead - Creep (Official Video) [HD]"), "Radiohead - Creep");
        assert_eq!(spoken("A - Song ★ 🔥 feat. B"), "A - Song feat. B");
        assert!(spoken(&"слово ".repeat(100)).chars().count() <= 160);
    }
    #[test]
    fn vocalize_asks_alice_to_read_the_text() {
        let frame = vocalize("Сейчас играет Creep");
        let message = &frame[4..];
        assert_eq!(number(message, 253), Some(210));
        let remoting = bytes(message, 252).unwrap();
        assert_eq!(bytes(remoting, 7), Some(&b"AliceCapability"[..]));
        let method = bytes(remoting, 8).unwrap();
        assert_eq!(number(method, 1), Some(18));
        let directive = bytes(method, 13).unwrap();
        assert_eq!(bytes(directive, 1), Some("Сейчас играет Creep".as_bytes()));
        assert_eq!(number(directive, 2), Some(2));
    }
    fn state_message(state: Option<u64>, kind: u64) -> Vec<u8> {
        let mut body = Vec::new();
        if let Some(state) = state {
            put_number(&mut body, 1, state);
        }
        let mut msg = Vec::new();
        put_bytes(&mut msg, 252, &body);
        put_number(&mut msg, 253, kind);
        msg
    }
    #[test]
    fn alice_activity_includes_listening_thinking_and_speech_only() {
        for state in [0, 6] {
            assert_eq!(alice_busy(&state_message(Some(state), 3)), Some(false));
        }
        for state in [1, 2, 3, 5] {
            assert_eq!(alice_busy(&state_message(Some(state), 3)), Some(true));
        }
        assert_eq!(alice_busy(&state_message(Some(99), 3)), None);
        assert_eq!(alice_busy(&state_message(Some(1), 210)), None);
        assert_eq!(alice_busy(&[]), None);
    }
    #[test]
    fn final_phrase_while_listening_or_busy_is_recognized() {
        let id = b"12345678-1234-1234-1234-123456789012";
        let text = "Включи radiohead".as_bytes();
        let mut asr = Vec::new();
        put_bytes(&mut asr, 1, text);
        put_number(&mut asr, 2, 1);
        put_number(&mut asr, 4, 1);
        let with_state = |alice: u8, asr: &[u8]| {
            let mut state = Vec::new();
            put_number(&mut state, 1, u64::from(alice));
            put_bytes(&mut state, 2, text);
            put_bytes(&mut state, 7, id);
            put_bytes(&mut state, 15, asr);
            let mut msg = Vec::new();
            put_bytes(&mut msg, 252, &state);
            put_number(&mut msg, 253, 3);
            msg
        };
        let msg = with_state(3, &asr);
        assert_eq!(
            recognized(&msg),
            Some((
                String::from_utf8(id.to_vec()).unwrap(),
                "Включи radiohead".into()
            ))
        );
        for n in 0..msg.len() {
            assert_eq!(recognized(&msg[..n]), None);
        }
        assert!(recognized(&with_state(1, &asr)).is_some());
        let mut partial = Vec::new();
        put_bytes(&mut partial, 1, text);
        put_number(&mut partial, 2, 0);
        put_number(&mut partial, 4, 1);
        assert_eq!(recognized(&with_state(1, &partial)), None);
        assert_eq!(recognized(&with_state(2, &asr)), None);
        assert_eq!(recognized(&with_state(0, &asr)), None);
    }
}
