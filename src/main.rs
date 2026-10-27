mod audio_guard;
mod bypass;
mod config;
mod control;
mod diagnose;
mod ducking;
mod fmp4;
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
  soundiya play QUERY...      search SoundCloud and play
  soundiya fav                play your liked tracks (needs login)
  soundiya like               like the current track (needs login)
  soundiya pause | resume | next | prev
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
        "like" => command_line("LIKE", ""),
        "pause" | "stop" => command_line("PAUSE", ""),
        "resume" => command_line("RESUME", ""),
        "next" => command_line("NEXT", ""),
        "prev" | "previous" => command_line("PREV", ""),
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
    /// A search or library request failed (or found nothing).
    Failed {
        request: u64,
        error: String,
    },
    /// The track of this generation could not be started.
    Unplayable(u64, String),
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
            "{state}: {track} [{}:{:02} / {}:{:02}] queue {}/{} | account: {}",
            position / 60,
            position % 60,
            length / 60,
            length % 60,
            if self.active() { self.index + 1 } else { 0 },
            self.queue.len(),
            if self.client.logged_in() { "signed in" } else { "anonymous" },
        )
    }

    fn stop(&mut self) {
        self.generation = 0;
        self.player.stop();
    }

    fn start(&mut self, index: usize) {
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
                    player.play(generation, source);
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
                Ok(_) => Message::Failed { request, error: format!("nothing found for '{query}'") },
                Err(e) => Message::Failed { request, error: e },
            };
            let _ = messages.send(message);
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
                }
            } else {
                match client.liked_tracks(QUEUE).await {
                    Ok(tracks) if !tracks.is_empty() => Message::Queue { request, tracks },
                    Ok(_) => Message::Failed { request, error: "no liked tracks yet".into() },
                    Err(e) => Message::Failed { request, error: e },
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
        match event.intent {
            Intent::Play(query) => self.search(query, from_voice),
            Intent::Favorites => self.favorites(from_voice),
            Intent::Like => {
                // Always silence Alice, even when there is nothing to like.
                self.silence(from_voice);
                let Some(track) = self.queue.get(self.index).filter(|_| self.active()).cloned()
                else {
                    return eprintln!("like: nothing is playing");
                };
                if !self.client.logged_in() {
                    return eprintln!("like: sign in first with 'soundiya login'");
                }
                let client = self.client.clone();
                tokio::spawn(async move {
                    match client.like(track.id).await {
                        Ok(()) => eprintln!("like: added to favorites ({})", track.label()),
                        Err(e) => eprintln!("like: failed: {e}"),
                    }
                });
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
            // Not ours to handle: Alice answers by herself.
            Intent::Pause | Intent::Next | Intent::Previous => {}
        }
    }

    fn message(&mut self, message: Message) {
        match message {
            Message::Queue { request, tracks } if request == self.request => {
                self.failures = 0;
                self.queue = tracks;
                self.start(0);
            }
            Message::Failed { request, error } if request == self.request => {
                eprintln!("search: {error}")
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
    let client = Arc::new(Client::new(options.state_dir.clone(), Some(&format!("http://{proxy}")))?);
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
        tokio::spawn(voice::watch(sender.clone(), ducking.clone(), address));
    }
    if let Some(address) = native {
        let guard = guard.clone();
        tokio::spawn(async move { guard.run(address).await });
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
        println!("5. account: anonymous mode (run 'soundiya login' for likes and favorites)");
    }
    Ok(())
}
