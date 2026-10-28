//! The local command socket the `soundiya` CLI talks to.
use crate::{
    bypass::Handle,
    config,
    mixer::{DeviceMixer, from_percent},
    soundcloud::{Client, clean_token},
    voice::{self, Event, Intent},
};
use std::{fs, io, os::unix::fs::PermissionsExt, path::PathBuf, sync::{Arc, Mutex}};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
    time::Instant,
};

pub struct Context {
    pub client: Arc<Client>,
    pub status: Mutex<String>,
    pub mixer: Arc<DeviceMixer>,
    /// The running bypass proxy, whose strategy can be switched on the fly.
    pub bypass: Handle,
    pub state_dir: PathBuf,
}

pub async fn serve(sender: mpsc::Sender<Event>, context: Arc<Context>) -> io::Result<()> {
    let _ = fs::remove_file(voice::CONTROL);
    let listener = UnixListener::bind(voice::CONTROL)?;
    fs::set_permissions(voice::CONTROL, fs::Permissions::from_mode(0o600))?;
    loop {
        let (socket, _) = listener.accept().await?;
        let (sender, context) = (sender.clone(), context.clone());
        tokio::spawn(async move {
            let _ = session(socket, sender, context).await;
        });
    }
}

async fn session(
    socket: UnixStream,
    sender: mpsc::Sender<Event>,
    context: Arc<Context>,
) -> io::Result<()> {
    let (reader, mut writer) = socket.into_split();
    let mut line = String::new();
    // A pasted browser header or cURL command is long, but never more than this.
    BufReader::new(reader.take(32 * 1024)).read_line(&mut line).await?;
    let reply = match handle(line.trim(), &sender, &context).await {
        Ok(text) => format!("OK {text}\n"),
        Err(text) => format!("ERROR {text}\n"),
    };
    writer.write_all(reply.as_bytes()).await
}

fn send(sender: &mpsc::Sender<Event>, intent: Intent) -> Result<String, String> {
    sender
        .try_send(Event {
            intent,
            received: Instant::now(),
            from_voice: false,
        })
        .map(|_| "sent".to_owned())
        .map_err(|_| "the receiver is busy".to_owned())
}

async fn handle(
    line: &str,
    sender: &mpsc::Sender<Event>,
    context: &Context,
) -> Result<String, String> {
    let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
    let argument = argument.trim();
    match command.to_ascii_uppercase().as_str() {
        "STATUS" => Ok(context.status.lock().unwrap().clone()),
        "STRATEGY" if argument.is_empty() => Ok(context.bypass.strategy()),
        "STRATEGY" => {
            // Switch first (it validates the name), then remember the choice for the next start.
            context.bypass.set_strategy(argument)?;
            config::save_strategy(&context.state_dir, argument)
                .map_err(|e| format!("switched, but could not save it to config.json ({e})"))?;
            Ok(format!("strategy {argument}"))
        }
        "PLAY" if !argument.is_empty() => send(sender, Intent::Play(argument.to_owned())),
        "PLAY" => Err("missing search query".into()),
        "FAV" => send(sender, Intent::Favorites),
        "PLAYLIST" if !argument.is_empty() => send(sender, Intent::Playlist(argument.to_owned())),
        "PLAYLIST" => Err("missing playlist name".into()),
        "TOP" | "TRENDING" => {
            let genre = voice::genre_of(&argument.to_lowercase()).unwrap_or("all-music");
            send(sender, Intent::Chart { trending: command.eq_ignore_ascii_case("TRENDING"), genre })
        }
        "RELATED" => send(sender, Intent::Related),
        "NOW" => send(sender, Intent::NowPlaying),
        "PAUSE" => send(sender, Intent::Pause),
        "RESUME" => send(sender, Intent::Resume),
        "NEXT" => send(sender, Intent::Next),
        "PREV" => send(sender, Intent::Previous),
        "SEEK" => {
            let seconds: i32 = argument.parse().map_err(|_| "seek takes seconds: 20 or -10".to_owned())?;
            if seconds == 0 || seconds.unsigned_abs() > 3600 {
                return Err("seek takes seconds: 20 or -10 (up to an hour)".into());
            }
            send(sender, Intent::Seek(seconds))
        }
        "RESTART" => send(sender, Intent::Restart),
        "REPEAT" => match argument.to_ascii_lowercase().as_str() {
            "" | "on" => send(sender, Intent::Loop(true)),
            "off" => send(sender, Intent::Loop(false)),
            _ => Err("repeat takes on or off".into()),
        },
        "VOLUME" => {
            let percent: u16 = argument.parse().map_err(|_| "volume is 0-100".to_owned())?;
            if percent > 100 {
                return Err("volume is 0-100".into());
            }
            context.mixer.set(from_percent(percent));
            Ok(format!("volume {percent}%"))
        }
        "LOGIN" => {
            let token = clean_token(argument)
                .ok_or("no SoundCloud token found in the pasted text")?;
            context.client.set_token(Some(token))?;
            match context.client.username().await {
                Ok(name) => Ok(format!("signed in as {name}")),
                Err(e) => {
                    let _ = context.client.set_token(None);
                    Err(format!("SoundCloud did not accept the token ({e})"))
                }
            }
        }
        "LOGOUT" => {
            context.client.set_token(None)?;
            Ok("signed out".into())
        }
        _ => Err("unknown command".into()),
    }
}
