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
    Favorites,
    Resume,
    Pause,
    Next,
    Previous,
    /// The Alice button: pause what plays, resume what is paused.
    Toggle,
}
pub struct Event {
    pub intent: Intent,
    pub received: Instant,
    pub from_voice: bool,
}

pub fn intent(text: &str) -> Option<Intent> {
    let text = text
        .trim()
        .trim_matches(['.', '!', '?'])
        .to_lowercase()
        .replace('ё', "е");
    let text = text
        .strip_prefix("алиса, ")
        .or_else(|| text.strip_prefix("алиса "))
        .unwrap_or(&text)
        .trim();
    match text {
        "стоп"
        | "пауза"
        | "выключи"
        | "выключись"
        | "отключи"
        | "останови музыку"
        | "выключи музыку"
        | "поставь на паузу"
        | "останови воспроизведение" => return Some(Intent::Pause),
        "дальше" | "следующий" | "следующий трек" | "следующая песня" | "переключи трек" =>
        {
            return Some(Intent::Next);
        }
        "назад" | "предыдущий трек" | "предыдущая песня" => {
            return Some(Intent::Previous);
        }
        "продолжи" | "продолжи музыку" | "продолжай музыку" | "включи музыку" =>
        {
            return Some(Intent::Resume);
        }
        "включи избранное"
        | "включи любимое"
        | "включи любимые"
        | "включи любимые треки"
        | "включи мою музыку"
        | "моя музыка"
        | "любимые треки" => {
            return Some(Intent::Favorites);
        }
        // Likes cannot be written from the speaker; do not search for a track named "лайк".
        "лайк" | "поставь лайк" | "добавь в избранное" | "сохрани песню" | "мне нравится" => {
            return None;
        }
        _ => {}
    }
    let rest =["включи ", "поставь ", "запусти ", "сыграй "]
        .iter()
        .find_map(|p| text.strip_prefix(p))?
        .trim();
    let music_prefix = [
        "песню ",
        "трек ",
        "музыку ",
        "группу ",
        "исполнителя ",
        "альбом ",
        "плейлист ",
    ];
    let explicit = music_prefix.iter().find_map(|p| rest.strip_prefix(p));
    let query = explicit.unwrap_or(rest).trim().trim_matches('"').trim();
    if query.is_empty() || query.len() > 400 {
        return None;
    }
    let home = [
        "свет",
        "освещение",
        "ламп",
        "люстр",
        "торшер",
        "ночник",
        "розетк",
        "телевизор",
        "тв",
        "кондиционер",
        "вентилятор",
        "обогревател",
        "пылесос",
        "робот",
        "чайник",
        "будильник",
        "таймер",
        "блютуз",
        "bluetooth",
        "режим",
        "экран",
        "микрофон",
        "громк",
        "звук",
        "отоплен",
        "увлажнител",
        "очистител",
        "зарядк",
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
    seen: &mut VecDeque<String>,
) -> io::Result<()> {
    let mut data = Vec::with_capacity(4096);
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
        if let Some(busy) = alice_busy(&data) {
            ducking.set_busy(busy);
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
            if let Some(intent) = intent(&text) {
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
    address: SocketAddr,
) {
    let mut seen: VecDeque<String> = VecDeque::new();
    loop {
        let result: io::Result<()> = async {
            let mut socket = TcpStream::connect(address).await?;
            eprintln!("voice: connected to native Alice events");
            read_events(&mut socket, &sender, &ducking, &mut seen).await
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
