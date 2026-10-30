mod audio_guard;
mod bypass;
mod config;
mod control;
mod diagnose;
mod ducking;
mod fmp4;
mod buttons;
mod mixer;
mod player;
mod rawfake;
mod soundcloud;
mod strategy;
mod voice;
mod volume_guard;

use player::{Event as PlayerEvent, IDLE, PAUSED, PLAYING, Player};
use soundcloud::{Client, Track};
use std::{
    fs,
    io::{Read, Write},
    net::SocketAddr,
    os::unix::net::UnixStream,
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};
use tokio::sync::mpsc;
use voice::Intent;

const DEFAULT_STATE: &str = "/data/opt/soundiya/state";
const VOICE_EXPIRY: Duration = Duration::from_secs(5);
/// How many search results are queued behind the first one.
const QUEUE: usize = 25;
const MAX_FAILURES: u32 = 4;

const USAGE: &str = "\
soundiya - SoundCloud for Yandex Stations

  soundiya run                daemon (started by the init script)
  soundiya login [TOKEN]      sign in with a SoundCloud OAuth token (see README)
  soundiya logout             forget the token, go back to anonymous mode
  soundiya play QUERY...      search SoundCloud and play; a soundcloud.com link of a track,
                              playlist (also private) or artist plays that
  soundiya fav                play your liked tracks (needs login)
  soundiya playlist NAME      play a playlist or album (yours first when signed in)
  soundiya top [GENRE]        the SoundCloud top chart; `trending [GENRE]` for the rising one
  soundiya related            more tracks like the current one
  soundiya now                Alice says which track plays
  soundiya pause | resume | next | prev
  soundiya seek SECONDS       jump within the track: seek 20, seek -10
  soundiya restart            play the current track from the beginning
  soundiya repeat [on|off]    repeat the current track endlessly
  soundiya volume 0-100
  soundiya status             what is playing now
  soundiya check              test SoundCloud access and the DPI bypass
  soundiya strategies         list the bypass strategies (Flowseal presets and built-in tricks)
  soundiya strategy NAME      choose one (alt11, general, simple-fake, split, auto, off, ...)
  soundiya auto [--first]     diagnostics: try every strategy, keep the best one
  soundiya fetch URL          download a URL through the bypass, report where it stalls
  soundiya tone               play a one-second test tone through the speaker

options: --state-dir DIR   --profile mini1|mini2|generic|FILE.json   --no-voice";

struct Options {
    state_dir: PathBuf,
    profile: Option<String>,
    voice: bool,
}

fn main() -> ExitCode {
    let mut options = Options {
        state_dir: DEFAULT_STATE.into(),
        profile: None,
        voice: true,
    };
    let mut words = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => match args.next() {
                Some(dir) => options.state_dir = dir.into(),
                None => return fail("missing state directory"),
            },
            "--profile" => match args.next() {
                Some(profile) => options.profile = Some(profile),
                None => return fail("missing profile"),
            },
            "--no-voice" => options.voice = false,
            _ => words.push(arg),
        }
    }
    // Only the daemon and `check` log; the thin CLI commands print their own answers.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,soundiya=info"),
    )
    .format_timestamp(None)
    .init();
    let Some((command, rest)) = words.split_first() else {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    };
    let rest = rest.join(" ");
    let result = match command.as_str() {
        "run" => runtime().and_then(|rt| rt.block_on(daemon(options))),
        "check" => runtime().and_then(|rt| rt.block_on(check(options))),
        "fetch" => runtime().and_then(|rt| rt.block_on(fetch(options, &rest))),
        "strategies" => strategies(&options),
        "strategy" => choose_strategy(&options, &rest),
        "auto" => runtime().and_then(|rt| rt.block_on(auto(options, &rest))),
        "tone" => {
            let profile = config::load(&options.state_dir)
                .and_then(|c| config::profile(options.profile.as_deref().unwrap_or(&c.profile)))
                .map_err(|e| e.to_string());
            profile.and_then(|p| player::tone(&p.audio.device))
        }
        "login" => login(&rest),
        "play" | "search" => command_line("PLAY", &rest),
        "fav" | "favorites" => command_line("FAV", ""),
        "playlist" | "album" => command_line("PLAYLIST", &rest),
        "top" => command_line("TOP", &rest),
        "trending" => command_line("TRENDING", &rest),
        "related" | "similar" => command_line("RELATED", ""),
        "now" => command_line("NOW", ""),
        "pause" | "stop" => command_line("PAUSE", ""),
        "resume" => command_line("RESUME", ""),
        "next" => command_line("NEXT", ""),
        "prev" | "previous" => command_line("PREV", ""),
        "seek" => command_line("SEEK", &rest),
        "restart" => command_line("RESTART", ""),
        "repeat" => command_line("REPEAT", &rest),
        "volume" => command_line("VOLUME", &rest),
        "status" => command_line("STATUS", ""),
        "logout" => command_line("LOGOUT", ""),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// `soundiya strategies`: what can be chosen, and what is chosen now.
fn strategies(options: &Options) -> Result<(), String> {
    let current = config::load(&options.state_dir)
        .map_or_else(|_| "auto".to_owned(), |config| config.bypass.strategy);
    println!("current: {current}\n");
    println!("{:<22}remembered or raced choice among a short list", "auto");
    println!("{:<22}no obfuscation", "off");
    for strategy in bypass::Strategy::all() {
        println!("{:<22}{}", strategy.name(), strategy.describe());
    }
    println!("\n'soundiya strategy NAME' chooses one; 'soundiya auto' finds the best for this network.");
    Ok(())
}

/// `soundiya strategy NAME`: choose a strategy, for the running receiver and for later starts.
fn choose_strategy(options: &Options, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return strategies(options);
    }
    bypass::order_for(name)
        .ok_or_else(|| format!("unknown strategy '{name}'; 'soundiya strategies' lists them"))?;
    config::save_strategy(&options.state_dir, name).map_err(|e| e.to_string())?;
    println!("saved: bypass.strategy = {name}");
    if command_line("STRATEGY", name).is_err() {
        println!("the receiver is not running; the choice applies when it starts");
    }
    Ok(())
}

/// `soundiya auto`: tries every strategy against SoundCloud and keeps the best one.
async fn auto(options: Options, flags: &str) -> Result<(), String> {
    let config = config::load(&options.state_dir).map_err(|e| e.to_string())?;
    let first_only = flags.split_whitespace().any(|flag| matches!(flag, "--first" | "first"));
    // The strategies are what is under test, so a configured upstream proxy stays out of it.
    let mut base = config.bypass.clone();
    base.upstream_proxy = None;
    println!(
        "Trying {} strategies against SoundCloud, one at a time (up to 30 s each)...",
        bypass::Strategy::all().len()
    );
    let outcomes = diagnose::run(&diagnose::Options { first_only, base }, |outcome| {
        println!("{}", diagnose::row(outcome));
    })
    .await;
    let Some(best) = diagnose::best(&outcomes) else {
        return Err("No strategy got SoundCloud through on this network. Set bypass.upstream_proxy \
(an HTTP CONNECT or socks5:// proxy that can reach SoundCloud) in config.json."
            .into());
    };
    let name = best.strategy.name();
    config::save_strategy(&options.state_dir, &name).map_err(|e| e.to_string())?;
    println!("\nBest: {name}. Saved as bypass.strategy.");
    if command_line("STRATEGY", &name).is_err() {
        println!("The receiver is not running; the choice applies when it starts.");
    }
    Ok(())
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}
fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

/// Sends one line to the running daemon and prints its answer.
fn command_line(command: &str, argument: &str) -> Result<(), String> {
    let mut socket = UnixStream::connect(voice::CONTROL)
        .map_err(|_| "soundiya is not running; start it with /data/opt/soundiya/start.sh".to_owned())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(40)))
        .map_err(|e| e.to_string())?;
    writeln!(socket, "{command} {argument}").map_err(|e| e.to_string())?;
    socket
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| e.to_string())?;
    let mut reply = String::new();
    socket.read_to_string(&mut reply).map_err(|e| e.to_string())?;
    match reply.trim().split_once(' ') {
        Some(("OK", text)) => {
            println!("{text}");
            Ok(())
        }
        Some(("ERROR", text)) => Err(text.to_owned()),
        _ => Err("unexpected answer from soundiya".into()),
    }
}

fn login(argument: &str) -> Result<(), String> {
    let mut text = argument.to_owned();
    if soundcloud::clean_token(&text).is_none() {
        eprintln!("Paste the SoundCloud token (or the whole copied header/cURL), then press Enter.");
        eprintln!("An empty line, Ctrl-D, q or exit cancels.");
        let mut line = String::new();
        // A multi-line cURL paste may need several lines before the token shows up.
        while soundcloud::clean_token(&text).is_none() && text.len() < 32 * 1024 {
            line.clear();
            if std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                break;
            }
            if matches!(line.trim(), "" | "q" | "quit" | "exit") {
                break;
            }
            text.push(' ');
            text.push_str(&line);
        }
    }
    let token = soundcloud::clean_token(&text).ok_or(
        "no SoundCloud token found: expected `2-…-…-…` (OAuth) or `eyJ…` (JWT) from the Authorization header",
    )?;
    command_line("LOGIN", &token)
}

enum Message {
    Queue {
        request: u64,
        tracks: Vec<Track>,
    },
    /// A search or library request failed (or found nothing). `say` is what Alice tells
    /// the listener when the request came by voice.
    Failed {
        request: u64,
        error: String,
        say: Option<String>,
    },
    /// The track of this generation could not be started.
    Unplayable(u64, String),
}

/// What to put in the queue besides a plain search.
enum Fetch {
    Playlist(String),
    Chart { trending: bool, genre: &'static str },
    Related(u64),
    /// A soundcloud.com link to a track, playlist or artist.
    Link(String),
}

fn is_link(text: &str) -> bool {
    let text = text.trim();
    (text.starts_with("https://") || text.starts_with("http://")) && text.contains("soundcloud.com")
        && !text.contains(char::is_whitespace)
}

/// Alice's answer to a failed voice request, worded for her voice.
fn nothing_found(from_voice: bool) -> Option<String> {
    from_voice.then(|| "Ничего не нашла".to_owned())
}
fn unreachable_service(from_voice: bool) -> Option<String> {
    from_voice.then(|| "Не получилось связаться с Саундклауд".to_owned())
}

struct App {
    client: Arc<Client>,
    player: Arc<Player>,
    guard: Arc<audio_guard::AudioGuard>,
    native: Option<SocketAddr>,
    messages: mpsc::UnboundedSender<Message>,
    state_dir: PathBuf,
    queue: Vec<Track>,
    index: usize,
    /// The playing generation, or 0 when nothing of ours is loaded or playing.
    generation: u64,
    request: u64,
    failures: u32,
    last_query: Option<String>,
    /// Alice may read answers aloud (`speech` in config.json, on by default).
    speech: bool,
    /// The last voice request went to Yandex Music: control phrases belong to Alice until
    /// the next search.
    handed_over: bool,
    /// The current track starts again when it ends; cleared when a new queue is loaded.
    repeat: bool,
}

impl App {
    fn active(&self) -> bool {
        self.generation != 0
    }
    fn status(&self) -> String {
        let state = match self.player.state() {
            PLAYING => "playing",
            PAUSED => "paused",
            _ if self.active() => "loading",
            _ => "idle",
        };
        let track = self
            .queue
            .get(self.index)
            .filter(|_| self.active())
            .map_or(String::from("-"), Track::label);
        let position = self.player.shared.position_ms.load(Ordering::Relaxed) / 1000;
        let length = self.queue.get(self.index).map_or(0, |t| t.duration_ms / 1000);
        format!(
            "{state}: {track} [{}:{:02} / {}:{:02}] queue {}/{}{} | account: {}",
            position / 60,
            position % 60,
            length / 60,
            length % 60,
            if self.active() { self.index + 1 } else { 0 },
            self.queue.len(),
            if self.repeat { " repeat" } else { "" },
            if self.client.logged_in() { "signed in" } else { "anonymous" },
        )
    }

    fn stop(&mut self) {
        self.generation = 0;
        self.player.stop();
    }

    fn start(&mut self, index: usize) {
        self.start_at(index, 0);
    }
    /// Starts track `index`, skipping its first `from_ms` milliseconds (a backward seek).
    fn start_at(&mut self, index: usize, from_ms: u64) {
        let Some(track) = self.queue.get(index).cloned() else {
            return self.stop();
        };
        self.index = index;
        self.generation = self.player.begin();
        let (generation, client, player, messages) = (
            self.generation,
            self.client.clone(),
            self.player.clone(),
            self.messages.clone(),
        );
        eprintln!("player: {}", track.label());
        tokio::spawn(async move {
            match client.stream(&track).await {
                Ok(stream) if player.is_current(generation) => {
                    let source = player.open(generation, client.media_http(), stream);
                    player.play_from(generation, source, from_ms);
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = messages.send(Message::Unplayable(generation, e));
                }
            }
        });
    }

    fn advance(&mut self) {
        if self.index + 1 < self.queue.len() {
            self.start(self.index + 1);
        } else {
            self.stop();
        }
    }

    fn fail_current(&mut self, error: &str) {
        eprintln!("player: {error}");
        self.failures += 1;
        if self.failures < MAX_FAILURES {
            self.advance();
        } else {
            eprintln!("player: too many unplayable tracks in a row; stopping");
            self.stop();
        }
    }

    fn search(&mut self, query: String, from_voice: bool) {
        self.request += 1;
        let (request, client, native, messages) = (
            self.request,
            self.client.clone(),
            self.native,
            self.messages.clone(),
        );
        let _ = fs::write(self.state_dir.join("last-query"), &query);
        self.last_query = Some(query.clone());
        tokio::spawn(async move {
            // Silence Alice and the station's own player first; the search takes a moment.
            let _ = voice::prepare_playback(native, from_voice).await;
            let message = match client.search(&query, QUEUE).await {
                Ok(tracks) if !tracks.is_empty() => Message::Queue { request, tracks },
                Ok(_) => Message::Failed {
                    request,
                    error: format!("nothing found for '{query}'"),
                    say: nothing_found(from_voice),
                },
                Err(e) => Message::Failed { request, error: e, say: unreachable_service(from_voice) },
            };
            let _ = messages.send(message);
        });
    }

    /// Replaces the queue with a playlist, a chart or tracks related to the current one.
    fn load(&mut self, what: Fetch, from_voice: bool) {
        self.request += 1;
        let (request, client, native, messages) = (
            self.request,
            self.client.clone(),
            self.native,
            self.messages.clone(),
        );
        tokio::spawn(async move {
            let _ = voice::prepare_playback(native, from_voice).await;
            let found: Result<Vec<Track>, String> = match what {
                Fetch::Playlist(name) => match client.playlist(&name, 100).await {
                    Ok(Some((title, tracks))) => {
                        eprintln!("playlist: {title} ({} tracks)", tracks.len());
                        Ok(tracks)
                    }
                    // No such playlist: the words may still name a track or an artist.
                    Ok(None) | Err(_) => client.search(&name, QUEUE).await,
                },
                Fetch::Chart { trending, genre } => match client.chart(trending, genre, 50).await {
                    Ok(tracks) if !tracks.is_empty() || genre == "all-music" => Ok(tracks),
                    _ => client.chart(trending, "all-music", 50).await,
                },
                Fetch::Related(track) => client.related(track, 50).await,
                Fetch::Link(url) => client.link(&url, 100).await.map(|(title, tracks)| {
                    eprintln!("link: {title} ({} tracks)", tracks.len());
                    tracks
                }),
            };
            let message = match found {
                Ok(tracks) if !tracks.is_empty() => Message::Queue { request, tracks },
                Ok(_) => Message::Failed {
                    request,
                    error: "nothing found".into(),
                    say: nothing_found(from_voice),
                },
                Err(e) => Message::Failed { request, error: e, say: unreachable_service(from_voice) },
            };
            let _ = messages.send(message);
        });
    }

    /// Has Alice say `text`. Her own answer to a voice command was cancelled a moment ago,
    /// and the repeated cancels (the last one about 1.5 s later) must be over before she
    /// speaks, or the last one cuts the sentence short.
    fn say(&self, text: String, from_voice: bool) {
        let Some(address) = self.native.filter(|_| self.speech) else { return };
        // Keeps the music guard from pausing the station's player (and her voice) meanwhile.
        self.guard.hold(Duration::from_secs(20));
        tokio::spawn(async move {
            if from_voice {
                tokio::time::sleep(voice::CANCELS_DONE).await;
            }
            match voice::say(address, &text).await {
                Ok(()) => eprintln!("say: '{text}'"),
                Err(e) => eprintln!("say: {e}"),
            }
        });
    }

    fn favorites(&mut self, from_voice: bool) {
        self.request += 1;
        let (request, client, native, messages) = (
            self.request,
            self.client.clone(),
            self.native,
            self.messages.clone(),
        );
        tokio::spawn(async move {
            let _ = voice::prepare_playback(native, from_voice).await;
            let message = if !client.logged_in() {
                Message::Failed {
                    request,
                    error: "liked tracks need a signed-in account: run 'soundiya login'".into(),
                    say: from_voice
                        .then(|| "Чтобы включить избранное, войдите в аккаунт Саундклауд".to_owned()),
                }
            } else {
                match client.liked_tracks(QUEUE).await {
                    Ok(tracks) if !tracks.is_empty() => Message::Queue { request, tracks },
                    Ok(_) => Message::Failed {
                        request,
                        error: "no liked tracks yet".into(),
                        say: from_voice.then(|| "В избранном пока пусто".to_owned()),
                    },
                    Err(e) => Message::Failed { request, error: e, say: unreachable_service(from_voice) },
                }
            };
            let _ = messages.send(message);
        });
    }

    /// Alice has to be told to stay quiet for any command we act on.
    fn silence(&self, from_voice: bool) {
        let native = self.native;
        tokio::spawn(async move {
            let _ = voice::prepare_command(native, from_voice).await;
        });
    }

    fn intent(&mut self, event: voice::Event) {
        let from_voice = event.from_voice;
        if self.handed_over {
            if from_voice || matches!(event.intent, Intent::Toggle) {
                match event.intent {
                    // Alice's own player is in charge: no pause, no cancel of her answer.
                    Intent::Pause
                    | Intent::Resume
                    | Intent::Next
                    | Intent::Previous
                    | Intent::Related
                    | Intent::NowPlaying
                    | Intent::Toggle
                    | Intent::Seek(_)
                    | Intent::Restart
                    | Intent::Loop(_)
                    | Intent::Handover => return,
                    _ => self.handed_over = false,
                }
            } else {
                // A console command takes control back.
                self.handed_over = false;
            }
        }
        match event.intent {
            Intent::Handover => {
                // Stop ours and let Alice answer: she plays the request from Yandex Music.
                if self.player.state() == PLAYING {
                    self.player.pause();
                }
                self.handed_over = true;
                eprintln!("voice: handed over to Alice (Yandex Music)");
            }
            Intent::Play(link) if is_link(&link) => self.load(Fetch::Link(link.trim().to_owned()), from_voice),
            Intent::Play(query) => self.search(query, from_voice),
            Intent::Favorites => self.favorites(from_voice),
            Intent::Playlist(name) => self.load(Fetch::Playlist(name), from_voice),
            Intent::Chart { trending, genre } => self.load(Fetch::Chart { trending, genre }, from_voice),
            Intent::Related if self.active() => {
                if let Some(track) = self.queue.get(self.index) {
                    let id = track.id;
                    self.load(Fetch::Related(id), from_voice);
                }
            }
            Intent::NowPlaying if self.active() && self.speech => {
                if let Some(track) = self.queue.get(self.index) {
                    self.silence(from_voice);
                    let (title, artist) = (voice::spoken(&track.title), voice::spoken(&track.artist));
                    self.say(format!("Сейчас играет {title}, исполнитель {artist}"), from_voice);
                }
            }
            Intent::Pause if self.active() => {
                self.silence(from_voice);
                self.player.pause();
            }
            Intent::Resume => {
                if self.player.state() == PAUSED {
                    self.silence(from_voice);
                    self.player.resume();
                } else if self.active() {
                    // Still loading; nothing to resume.
                } else if let Some(query) = self.last_query.clone() {
                    self.search(query, from_voice);
                }
            }
            Intent::Next if self.active() => {
                self.silence(from_voice);
                if self.index + 1 < self.queue.len() {
                    self.start(self.index + 1);
                }
            }
            Intent::Previous if self.active() => {
                self.silence(from_voice);
                self.start(self.index.saturating_sub(1));
            }
            Intent::Seek(seconds) if self.active() => {
                self.silence(from_voice);
                let delta = u64::from(seconds.unsigned_abs()) * 1000;
                if seconds > 0 {
                    eprintln!("player: seek +{seconds} s");
                    self.player.skip(delta);
                } else {
                    // The stream only reads forward: back means the same track again, from earlier.
                    let position = self.player.shared.position_ms.load(Ordering::Relaxed);
                    eprintln!("player: seek {seconds} s");
                    self.start_at(self.index, position.saturating_sub(delta));
                }
            }
            Intent::Restart if self.active() => {
                self.silence(from_voice);
                self.start_at(self.index, 0);
            }
            Intent::Loop(on) if self.active() || !on => {
                self.silence(from_voice);
                self.repeat = on && self.active();
                eprintln!("player: repeat {}", if self.repeat { "on" } else { "off" });
                let words = if self.repeat { "Повторяю этот трек" } else { "Повтор выключен" };
                self.say(words.to_owned(), from_voice);
            }
            // The button also reaches Alice, who would complain about the missing Plus.
            Intent::Toggle => {
                self.silence(true);
                match self.player.state() {
                    PLAYING => self.player.pause(),
                    PAUSED => self.player.resume(),
                    _ => {}
                }
            }
            // Not ours to handle: Alice answers by herself.
            Intent::Pause
            | Intent::Next
            | Intent::Previous
            | Intent::Related
            | Intent::NowPlaying
            | Intent::Seek(_)
            | Intent::Restart
            | Intent::Loop(_) => {}
        }
    }

    fn message(&mut self, message: Message) {
        match message {
            Message::Queue { request, tracks } if request == self.request => {
                self.failures = 0;
                self.repeat = false;
                self.queue = tracks;
                self.start(0);
            }
            Message::Failed { request, error, say } if request == self.request => {
                eprintln!("search: {error}");
                if let Some(text) = say {
                    self.say(text, true);
                }
            }
            Message::Unplayable(generation, error) if generation == self.generation => {
                self.fail_current(&error)
            }
            _ => {}
        }
    }

    fn player_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Started(g) if g == self.generation => self.failures = 0,
            // On repeat the same track starts again instead of the next one.
            PlayerEvent::Ended(g) if g == self.generation && self.repeat => self.start_at(self.index, 0),
            PlayerEvent::Ended(g) if g == self.generation => self.advance(),
            PlayerEvent::Failed(g, error) if g == self.generation => self.fail_current(&error),
            _ => {}
        }
    }
}

async fn daemon(options: Options) -> Result<(), String> {
    let err = |e: std::io::Error| e.to_string();
    fs::create_dir_all(&options.state_dir).map_err(err)?;
    let config = config::load(&options.state_dir).map_err(err)?;
    let profile =
        config::profile(options.profile.as_deref().unwrap_or(&config.profile)).map_err(err)?;
    eprintln!("receiver: profile {}", profile.id);

    let (proxy, bypass_handle) = bypass::start(config.bypass.clone(), Some(&options.state_dir)).await.map_err(err)?;
    eprintln!("bypass: {:?} via 127.0.0.1:{}", bypass_handle.strategy(), proxy.port());
    let client = Arc::new(
        Client::new(options.state_dir.clone(), Some(&format!("http://{proxy}")))?
            .reuse_connections_when({
                let bypass = bypass_handle.clone();
                move || bypass.untouched()
            }),
    );
    eprintln!(
        "soundcloud: {}",
        if client.logged_in() { "signed in" } else { "anonymous mode" }
    );

    let ducking = Arc::new(ducking::Ducking::new());
    let mixer = Arc::new(mixer::DeviceMixer::new(profile.audio.clone(), ducking.clone()));
    if let Ok(volume) = mixer.read() {
        mixer.desired.store(volume, Ordering::Release);
    }
    let (player, mut player_events) = Player::spawn(profile.audio.device.clone(), ducking.clone());
    let player = Arc::new(player);
    let guard = Arc::new(audio_guard::AudioGuard::new());

    let native = profile.voice.as_ref().map(|v| v.address);
    let (sender, mut receiver) = mpsc::channel::<voice::Event>(8);
    let voice_enabled = options.voice && config.voice && native.is_some();
    if voice_enabled && let Some(address) = native {
        // Alice starts listening: open the connection the coming request will need.
        let warm = Arc::new(tokio::sync::Notify::new());
        tokio::spawn({
            let (warm, client) = (warm.clone(), client.clone());
            async move {
                loop {
                    warm.notified().await;
                    client.warm().await;
                }
            }
        });
        tokio::spawn(voice::watch(sender.clone(), ducking.clone(), guard.clone(), warm, address));
    }
    if let Some(address) = native {
        let guard = guard.clone();
        tokio::spawn(async move { guard.run(address).await });
    }

    let buttons_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if config.buttons && native.is_some() {
        buttons::spawn(sender.clone(), buttons_active.clone());
    }

    let context = Arc::new(control::Context {
        client: client.clone(),
        status: Mutex::new(String::new()),
        mixer: mixer.clone(),
        bypass: bypass_handle.clone(),
        state_dir: options.state_dir.clone(),
    });
    tokio::spawn({
        let context = context.clone();
        async move {
            if control::serve(sender, context).await.is_err() {
                eprintln!("control: local command socket unavailable");
            }
        }
    });

    let (messages, mut inbox) = mpsc::unbounded_channel();
    let mut app = App {
        client,
        player,
        guard,
        native,
        messages,
        state_dir: options.state_dir.clone(),
        queue: Vec::new(),
        index: 0,
        generation: 0,
        request: 0,
        failures: 0,
        speech: config.speech,
        handed_over: false,
        repeat: false,
        last_query: fs::read_to_string(options.state_dir.join("last-query"))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
    };

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(err)?;
    eprintln!("receiver: ready");
    loop {
        tokio::select! {
            Some(event) = receiver.recv() => {
                if event.from_voice && (!voice_enabled || event.received.elapsed() > VOICE_EXPIRY) {
                    continue;
                }
                app.intent(event);
            }
            Some(message) = inbox.recv() => app.message(message),
            Some(event) = player_events.recv() => app.player_event(event),
            _ = tick.tick() => {
                let state = app.player.state();
                app.guard.set_playing(state == PLAYING);
                buttons_active.store(app.active(), Ordering::Release);
                mixer.follow(state != IDLE || app.active());
                *context.status.lock().unwrap() = app.status();
            }
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    app.stop();
    let _ = fs::remove_file(voice::CONTROL);
    eprintln!("receiver: stopped");
    Ok(())
}

/// `soundiya fetch URL`: downloads a URL through the bypass and reports how far it got, which
/// tells a handshake that is blocked from a connection that is cut off after a few kilobytes.
async fn fetch(options: Options, url: &str) -> Result<(), String> {
    let err = |e: std::io::Error| e.to_string();
    let _ = fs::create_dir_all(&options.state_dir);
    let config = config::load(&options.state_dir).map_err(err)?;
    let (proxy, _) = bypass::start(config.bypass.clone(), Some(&options.state_dir)).await.map_err(err)?;
    let client = Client::new(options.state_dir.clone(), Some(&format!("http://{proxy}")))?;
    let started = std::time::Instant::now();
    let mut response = tokio::time::timeout(Duration::from_secs(20), client.media_http().get(url).send())
        .await
        .map_err(|_| "request: no answer within 20 s".to_owned())?
        .map_err(|e| format!("request: {e}"))?;
    println!("HTTP {} after {} ms", response.status(), started.elapsed().as_millis());
    let (mut bytes, mut next_report) = (0usize, 16 * 1024);
    loop {
        match tokio::time::timeout(Duration::from_secs(15), response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                bytes += chunk.len();
                if bytes >= next_report {
                    println!("{bytes} bytes, {} ms", started.elapsed().as_millis());
                    next_report = bytes + 64 * 1024;
                }
                if bytes > 2 * 1024 * 1024 {
                    println!("stopped after {bytes} bytes");
                    return Ok(());
                }
            }
            Ok(Ok(None)) => {
                println!("complete: {bytes} bytes in {} ms", started.elapsed().as_millis());
                return Ok(());
            }
            Ok(Err(e)) => return Err(format!("cut off after {bytes} bytes ({} ms): {e:?}", started.elapsed().as_millis())),
            Err(_) => return Err(format!("stalled after {bytes} bytes")),
        }
    }
}

/// `soundiya check`: walks through everything that has to work and says what does not.
async fn check(options: Options) -> Result<(), String> {
    let err = |e: std::io::Error| e.to_string();
    let _ = fs::create_dir_all(&options.state_dir);
    let config = config::load(&options.state_dir).map_err(err)?;
    let (proxy, bypass_handle) = bypass::start(config.bypass.clone(), Some(&options.state_dir)).await.map_err(err)?;
    println!("bypass mode: {:?} (proxy 127.0.0.1:{})", bypass_handle.strategy(), proxy.port());
    let client = Client::new(options.state_dir.clone(), Some(&format!("http://{proxy}")))?;

    let id = client.client_id(false).await;
    println!("1. SoundCloud web player + client id: {}", id.as_ref().map_or_else(|e| format!("FAILED - {e}"), |_| "ok".into()));
    id?;
    let tracks = client.search("lofi", 5).await;
    println!("2. search: {}", tracks.as_ref().map_or_else(|e| format!("FAILED - {e}"), |t| format!("ok, {} tracks, first: {}", t.len(), t.first().map_or(String::new(), Track::label))));
    let track = tracks?.into_iter().next().ok_or("search returned nothing")?;
    let stream = client.stream(&track).await;
    println!(
        "3. stream URL: {}",
        stream.as_ref().map_or_else(
            |e| format!("FAILED - {e}"),
            |s| format!(
                "ok ({} {:?})",
                if s.hls { "HLS" } else { "progressive" },
                s.codec
            )
        )
    );
    let decoded = player::probe(client.media_http(), stream?).await;
    println!(
        "4. download and decode: {}",
        decoded.map_or_else(
            |e| format!("FAILED - {e}"),
            |(rate, frames)| format!("ok, {frames} frames at {rate} Hz")
        )
    );
    if client.logged_in() {
        println!("5. account: {}", client.username().await.unwrap_or_else(|e| format!("FAILED - {e}")));
    } else {
        println!("5. account: anonymous mode (run 'soundiya login' for your favorites)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_link;
    #[test]
    fn recognizes_soundcloud_links_only() {
        assert!(is_link("https://soundcloud.com/artist/sets/mix"));
        assert!(is_link(" https://on.soundcloud.com/AbCd12 "));
        assert!(!is_link("soundcloud.com/artist"));
        assert!(!is_link("https://example.com/soundcloud"));
        assert!(!is_link("https://soundcloud.com/a b"));
        assert!(!is_link("radiohead creep"));
    }
}
