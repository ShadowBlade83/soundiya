//! A small in-process answer to SNI-based DPI blocking, in the spirit of zapret.
//!
//! HTTPS requests to blocked hosts are routed through a loopback CONNECT proxy. When a
//! client sends its TLS ClientHello, the proxy cuts it in the middle of the server name,
//! either into two TCP segments, into two TLS records, or both, so a middlebox that does not
//! reassemble the handshake never sees the hostname. Everything after the handshake is relayed
//! untouched. Unlike zapret it needs no root, raw sockets or iptables, because it only ever
//! shapes this program's own connections.
use crate::{
    config::{Bypass, DEFAULT_DOMAINS},
    rawfake::{self, Fooling},
    strategy::{self, Preset, Step},
};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const REPLY_TIMEOUT: Duration = Duration::from_secs(6);
/// Delay between starting consecutive strategies in the race.
const RACE_STAGGER: Duration = Duration::from_millis(250);
/// Gap between segments of one hello, so that they leave in the intended order.
const PAUSE: Duration = Duration::from_millis(8);
const DNS_TTL: Duration = Duration::from_secs(300);
const DOH: &[&str] = &["https://1.1.1.1/dns-query", "https://8.8.8.8/resolve"];

/// How the ClientHello of one connection is sent.
#[derive(Clone, Copy)]
pub enum Strategy {
    /// Untouched.
    Plain,
    /// Two TCP segments, cut inside the server name.
    Split,
    /// Two TCP segments, cut after the first byte.
    SplitEarly,
    /// Two TLS records in one write, cut inside the server name.
    TlsRec,
    /// Two TLS records, each in its own TCP segment.
    Both,
    /// Two segments with an urgent (out-of-band) byte between them (ByeDPI `--oob`).
    Oob,
    /// The first half with a TTL that expires on the way (ByeDPI `--disorder`).
    Disorder,
    /// A decoy hello whose TTL dies before the server (zapret `fake` with `ttl` fooling).
    TtlFake(u8),
    /// The real hello over a connection opened for forging; the baseline for diagnostics.
    RawPlain,
    /// One of the strategies of Flowseal's zapret-discord-youtube.
    Preset(&'static Preset),
}
impl PartialEq for Strategy {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}
impl std::fmt::Debug for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}
impl Eq for Strategy {}
impl Strategy {
    /// The name used in the configuration.
    pub fn name(self) -> String {
        match self {
            Self::Plain => "plain".into(),
            Self::Split => "split".into(),
            Self::SplitEarly => "splitearly".into(),
            Self::TlsRec => "tlsrec".into(),
            Self::Both => "both".into(),
            Self::Oob => "oob".into(),
            Self::Disorder => "disorder".into(),
            Self::TtlFake(ttl) => format!("ttlfake{ttl}"),
            Self::RawPlain => "rawplain".into(),
            Self::Preset(preset) => preset.name.into(),
        }
    }
    /// One line for lists.
    pub fn describe(self) -> String {
        match self {
            Self::Plain => "no obfuscation".into(),
            Self::Split => "ClientHello in two TCP segments, cut inside the server name".into(),
            Self::SplitEarly => "ClientHello in two TCP segments, cut after the first byte".into(),
            Self::TlsRec => "ClientHello in two TLS records".into(),
            Self::Both => "two TLS records, each in its own TCP segment".into(),
            Self::Oob => "two segments with an urgent byte between them (ByeDPI --oob)".into(),
            Self::Disorder => "first half with an expiring TTL (ByeDPI --disorder)".into(),
            Self::TtlFake(ttl) => format!("decoy hello that dies after {ttl} hops (zapret fake, ttl fooling)"),
            Self::RawPlain => "diagnostics: untouched hello over a forging-capable connection".into(),
            Self::Preset(preset) => format!("Flowseal {}", preset.title),
        }
    }
    /// Whether the strategy forges packets, so that the connection's handshake numbers are needed.
    pub fn forges(self) -> bool {
        matches!(self, Self::TtlFake(_) | Self::RawPlain | Self::Preset(_))
    }
    /// Reads a strategy name: a built-in trick, `ttlfake<N>`, or a Flowseal preset in any of its
    /// spellings (`alt11`, `ALT 11`, `general (ALT11)`).
    pub fn parse(name: &str) -> Option<Self> {
        let key = name.trim().to_lowercase().replace([' ', '_', '-'], "");
        match key.as_str() {
            "plain" => Some(Self::Plain),
            "split" => Some(Self::Split),
            "splitearly" => Some(Self::SplitEarly),
            "tlsrec" => Some(Self::TlsRec),
            "both" => Some(Self::Both),
            "oob" => Some(Self::Oob),
            "disorder" => Some(Self::Disorder),
            "rawplain" => Some(Self::RawPlain),
            _ => match key.strip_prefix("ttlfake") {
                Some(ttl) => ttl.parse().ok().map(Self::TtlFake),
                None => strategy::find(name).map(Self::Preset),
            },
        }
    }
    /// Everything a diagnostic run should try, simplest first.
    pub fn all() -> Vec<Self> {
        let tricks = [
            Self::Split,
            Self::SplitEarly,
            Self::TlsRec,
            Self::Both,
            Self::Oob,
            Self::Disorder,
            Self::TtlFake(7),
        ];
        tricks.into_iter().chain(strategy::PRESETS.iter().map(Self::Preset)).collect()
    }
}

/// The strategies that a configuration value stands for, in the order they are tried. One name
/// means exactly that strategy; `auto` means a short list, raced.
pub fn order_for(setting: &str) -> Option<Vec<Strategy>> {
    match setting.trim().to_lowercase().as_str() {
        "auto" | "" => Some(auto_order()),
        "off" | "none" => Some(vec![Strategy::Plain]),
        _ => Strategy::parse(setting).map(|s| vec![s]),
    }
}

/// What `auto` tries when it has no remembered result: the harmless tricks first, then the
/// presets that need the least from the network. A full search is `soundiya auto`.
fn auto_order() -> Vec<Strategy> {
    let preset = |name| strategy::find(name).map(Strategy::Preset);
    [
        Some(Strategy::Both),
        Some(Strategy::TlsRec),
        Some(Strategy::Split),
        Some(Strategy::SplitEarly),
        preset("alt11"),
        preset("alt9"),
        preset("general"),
        preset("simple-fake"),
        Some(Strategy::Plain),
    ]
    .into_iter()
    .flatten()
    .collect()
}


/// The decoy: the real ClientHello with the server name replaced by a different, harmless
/// name of the same length, and the session id changed so that a reply to it can be told
/// apart from a reply to the real one.
fn make_fake(hello: &[u8]) -> Option<Vec<u8>> {
    let (start, len) = sni_range(hello)?;
    let mut fake = hello.to_vec();
    for (i, byte) in fake[start..start + len].iter_mut().enumerate() {
        *byte = b'a' + ((i * 7 + 3) % 26) as u8;
    }
    if len >= 8 {
        fake[start + len - 4..start + len].copy_from_slice(b".com");
    }
    let sid = SESSION_ID_AT;
    if let Some(&sid_len) = hello.get(sid) {
        for byte in fake.get_mut(sid + 1..sid + 1 + usize::from(sid_len))? {
            *byte ^= 0x5a;
        }
    }
    Some(fake)
}

/// Where the session id length sits: record header 5, handshake header 4, version 2, random 32.
const SESSION_ID_AT: usize = 5 + 4 + 2 + 32;

/// A ServerHello answers the ClientHello it was given by echoing its session id (TLS 1.3 does
/// so always). If it echoes something else, the server read the decoy.
fn echoes_session_id(hello: &[u8], reply: &[u8]) -> bool {
    let id = |message: &[u8]| {
        let len = usize::from(*message.get(SESSION_ID_AT)?);
        message.get(SESSION_ID_AT + 1..SESSION_ID_AT + 1 + len).map(<[u8]>::to_vec)
    };
    match (id(hello), id(reply)) {
        (Some(sent), Some(echoed)) if !sent.is_empty() && !echoed.is_empty() => sent == echoed,
        // Nothing to compare (empty ids, or a TLS 1.2 server that picked its own).
        _ => true,
    }
}


/// Sends one byte as TCP urgent data.
fn send_urgent(stream: &TcpStream) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let byte = b"a";
    // SAFETY: the pointer and length describe a live one-byte buffer for the whole call.
    let sent = unsafe { libc::send(stream.as_raw_fd(), byte.as_ptr().cast(), 1, libc::MSG_OOB) };
    if sent == 1 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

struct Shared {
    cfg: Bypass,
    /// The strategies in use, tried in this order; one entry means exactly that strategy.
    order: Mutex<Vec<Strategy>>,
    /// The setting `order` came from, as the user wrote it.
    setting: Mutex<String>,
    domains: Vec<String>,
    dns: Mutex<HashMap<String, (Vec<IpAddr>, Instant)>>,
    /// The strategy that last worked for each host, kept across restarts in `cache`.
    working: Mutex<HashMap<String, Strategy>>,
    cache: Option<PathBuf>,
    doh: reqwest::Client,
}
impl Shared {
    fn blocked(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.domains
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    }
}

/// Control over a running proxy.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}
impl Handle {
    /// Switches the strategy used for new connections; connections already open are untouched.
    pub fn set_strategy(&self, setting: &str) -> Result<(), String> {
        let order = order_for(setting)
            .ok_or_else(|| format!("unknown strategy '{setting}'; 'soundiya strategies' lists them"))?;
        *self.shared.order.lock().unwrap() = order;
        *self.shared.setting.lock().unwrap() = setting.trim().to_owned();
        Ok(())
    }
    /// The strategy setting in force (`auto`, `off`, or a strategy name).
    pub fn strategy(&self) -> String {
        self.shared.setting.lock().unwrap().clone()
    }
    /// True when new connections go out untouched (`off`): nothing about them to protect.
    pub fn untouched(&self) -> bool {
        *self.shared.order.lock().unwrap() == [Strategy::Plain]
    }
}

/// Starts the loopback proxy; the address is what `reqwest` should be pointed at.
pub async fn start(cfg: Bypass, state_dir: Option<&Path>) -> io::Result<(SocketAddr, Handle)> {
    let order = order_for(&cfg.strategy).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown bypass strategy '{}'; 'soundiya strategies' lists them", cfg.strategy),
        )
    })?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let domains = DEFAULT_DOMAINS
        .iter()
        .map(|d| (*d).to_owned())
        .chain(cfg.domains.iter().map(|d| d.trim().to_ascii_lowercase()))
        .filter(|d| !d.is_empty())
        .collect();
    let doh = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(io::Error::other)?;
    let cache = state_dir.map(|dir| dir.join("bypass-cache.json"));
    let working = cache
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<HashMap<String, String>>(&text).ok())
        .map(|names| {
            names
                .into_iter()
                .filter_map(|(host, name)| Some((host, Strategy::parse(&name)?)))
                .collect()
        })
        .unwrap_or_default();
    let setting = cfg.strategy.trim().to_owned();
    let shared = Arc::new(Shared {
        cfg,
        order: Mutex::new(order),
        setting: Mutex::new(setting),
        domains,
        dns: Mutex::new(HashMap::new()),
        working: Mutex::new(working),
        cache,
        doh,
    });
    let handle_for_caller = Handle { shared: shared.clone() };
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(client, shared).await {
                    log::debug!("bypass: {e}");
                }
            });
        }
    });
    Ok((address, handle_for_caller))
}

async fn read_head(client: &mut TcpStream) -> io::Result<(String, u16)> {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    // One byte at a time, so nothing of the TLS handshake that follows is consumed.
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err(io::Error::other("proxy request too large"));
        }
        if timeout(HELLO_TIMEOUT, client.read(&mut byte)).await?? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let mut parts = text.lines().next().unwrap_or_default().split_whitespace();
    if parts.next() != Some("CONNECT") {
        let _ = client
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            .await;
        return Err(io::Error::other("only CONNECT is supported"));
    }
    let target = parts.next().unwrap_or_default();
    let (host, port) = target
        .rsplit_once(':')
        .ok_or_else(|| io::Error::other("CONNECT without port"))?;
    let port = port
        .parse()
        .map_err(|_| io::Error::other("CONNECT with bad port"))?;
    Ok((host.trim_matches(['[', ']']).to_owned(), port))
}

async fn handle(mut client: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    client.set_nodelay(true)?;
    let (host, port) = read_head(&mut client).await?;
    let blocked = shared.blocked(&host);
    // A configured upstream proxy carries the traffic whole, so there is nothing to disguise.
    let disguise = shared.order.lock().unwrap().iter().any(|s| *s != Strategy::Plain);
    let desync = blocked && disguise && shared.cfg.upstream_proxy.is_none();
    if !desync {
        let mut upstream = match connect(&shared, &host, port, blocked).await {
            Ok(s) => s,
            Err(e) => {
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return Err(e);
            }
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    let hello = Arc::new(read_record(&mut client).await?);
    let (mut upstream, first, strategy) = establish(&shared, &host, port, &hello).await?;
    remember(&shared, &host, strategy);
    client.write_all(&first).await?;
    let (sent, received, error) = relay(&mut client, &mut upstream).await;
    log::debug!(
        "bypass: {host} relay ended: {sent} bytes up, {} bytes down{}",
        received + first.len() as u64,
        error.as_ref().map_or(String::new(), |e| format!(", error: {e}"))
    );
    error.map_or(Ok(()), Err)
}

/// Copies both directions until each side is done; reports the byte counts and the first error.
async fn relay(client: &mut TcpStream, upstream: &mut TcpStream) -> (u64, u64, Option<io::Error>) {
    async fn pump(
        from: &mut (impl AsyncReadExt + Unpin),
        to: &mut (impl AsyncWriteExt + Unpin),
    ) -> (u64, Option<io::Error>) {
        let mut buf = vec![0u8; 16 * 1024];
        let mut total = 0u64;
        loop {
            match from.read(&mut buf).await {
                Ok(0) => {
                    let _ = to.shutdown().await;
                    return (total, None);
                }
                Ok(n) => {
                    if let Err(e) = to.write_all(&buf[..n]).await {
                        return (total, Some(e));
                    }
                    total += n as u64;
                }
                Err(e) => return (total, Some(e)),
            }
        }
    }
    let (mut client_read, mut client_write) = client.split();
    let (mut upstream_read, mut upstream_write) = upstream.split();
    let ((sent, up_error), (received, down_error)) = tokio::join!(
        pump(&mut client_read, &mut upstream_write),
        pump(&mut upstream_read, &mut client_write)
    );
    (sent, received, up_error.or(down_error))
}

/// Finds a way to get the handshake through. A strategy that worked before is tried alone
/// first; otherwise all of them race, each starting a little after the previous one, and the
/// first real ServerHello wins. Trying them one after another would cost several silent
/// timeouts, which is longer than the HTTP client is willing to wait.
async fn establish(
    shared: &Arc<Shared>,
    host: &str,
    port: u16,
    hello: &Arc<Vec<u8>>,
) -> io::Result<(TcpStream, Vec<u8>, Strategy)> {
    let order = shared.order.lock().unwrap().clone();
    // One name means exactly that strategy: no memory, no race.
    if let [only] = order[..] {
        let (upstream, first) = attempt(shared, host, port, hello, only).await?;
        return Ok((upstream, first, only));
    }
    let cached = shared
        .working
        .lock()
        .unwrap()
        .get(host)
        .copied()
        .filter(|s| order.contains(s));
    if let Some(strategy) = cached {
        match attempt(shared, host, port, hello, strategy).await {
            Ok((upstream, first)) => return Ok((upstream, first, strategy)),
            Err(e) => log::debug!("bypass: {host} with remembered {strategy:?}: {e}"),
        }
    }
    let mut race = JoinSet::new();
    for (index, strategy) in order.iter().copied().filter(|s| Some(*s) != cached).enumerate() {
        let (shared, host, hello) = (shared.clone(), host.to_owned(), hello.clone());
        race.spawn(async move {
            tokio::time::sleep(RACE_STAGGER * index as u32).await;
            let result = attempt(&shared, &host, port, &hello, strategy).await;
            if let Err(e) = &result {
                log::debug!("bypass: {host} with {strategy:?}: {e}");
            }
            result.map(|(upstream, first)| (upstream, first, strategy))
        });
    }
    let mut last = io::Error::other("no strategy worked");
    while let Some(finished) = race.join_next().await {
        match finished {
            Ok(Ok(found)) => {
                // Dropping the rest closes their half-open connections.
                race.abort_all();
                return Ok(found);
            }
            Ok(Err(e)) => last = e,
            Err(_) => {}
        }
    }
    Err(last)
}

fn remember(shared: &Shared, host: &str, strategy: Strategy) {
    let snapshot = {
        let mut working = shared.working.lock().unwrap();
        if working.insert(host.to_owned(), strategy) == Some(strategy) {
            return;
        }
        log::info!("bypass: {host} reachable with {strategy:?}");
        working
            .iter()
            .map(|(host, strategy)| (host.clone(), strategy.name().to_owned()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    if let Some(path) = &shared.cache
        && let Ok(text) = serde_json::to_string(&snapshot)
    {
        let _ = crate::config::write_private(path, &text);
    }
}

/// Opens a fresh upstream connection, sends the ClientHello the given way and waits for the
/// server to answer, which is how a blocked handshake shows itself (silence or a reset).
async fn attempt(
    shared: &Shared,
    host: &str,
    port: u16,
    hello: &[u8],
    strategy: Strategy,
) -> io::Result<(TcpStream, Vec<u8>)> {
    let (mut upstream, handshake) = if strategy.forges() {
        connect_sniffed(shared, host, port).await?
    } else {
        (connect(shared, host, port, true).await?, None)
    };
    upstream.set_nodelay(true)?;
    send_hello(&mut upstream, hello, strategy, handshake.as_ref()).await?;
    let mut buf = vec![0u8; 4096];
    let n = timeout(REPLY_TIMEOUT, upstream.read(&mut buf)).await??;
    if n == 0 {
        return Err(io::ErrorKind::ConnectionReset.into());
    }
    buf.truncate(n);
    log::debug!("bypass: {host} with {strategy:?}: first bytes {:02x?}", &buf[..n.min(12)]);
    // A block page or an injected alert also "answers", but only a ServerHello is progress.
    if hello.first() == Some(&0x16) && !looks_like_server_hello(&buf) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "reply is not a TLS ServerHello"));
    }
    // If the decoy reached the server (TTL too high), the answer is to the decoy and the real
    // handshake would fail later.
    if strategy.forges() && !echoes_session_id(hello, &buf) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the server answered the decoy"));
    }
    Ok((upstream, buf))
}

fn looks_like_server_hello(reply: &[u8]) -> bool {
    // Handshake record, TLS 1.x, and the handshake type of the first message is ServerHello.
    reply.len() >= 6 && reply[0] == 0x16 && reply[1] == 0x03 && reply[5] == 0x02
}

/// Like [`connect`], but also learns the connection's sequence numbers so a decoy can be forged.
async fn connect_sniffed(
    shared: &Shared,
    host: &str,
    port: u16,
) -> io::Result<(TcpStream, Option<rawfake::Handshake>)> {
    let mut last = io::Error::other("no addresses");
    for address in resolve(shared, host, port, true).await? {
        match rawfake::connect(address, CONNECT_TIMEOUT).await {
            Ok((stream, handshake)) => return Ok((stream, Some(handshake))),
            Err(e) => last = e,
        }
    }
    Err(last)
}

async fn send_hello(
    upstream: &mut TcpStream,
    hello: &[u8],
    strategy: Strategy,
    handshake: Option<&rawfake::Handshake>,
) -> io::Result<()> {
    let middle = sni_range(hello).map(|(start, len)| (start + len / 2).clamp(6, hello.len() - 1));
    let pause = || tokio::time::sleep(PAUSE);
    match (strategy, middle) {
        (Strategy::Preset(preset), _) => {
            let handshake = handshake.ok_or_else(|| io::Error::other("no handshake data"))?;
            let seed = uuid::Uuid::new_v4().as_u128() as u64;
            let steps = strategy::plan(preset, hello, seed).map_err(io::Error::other)?;
            run_plan(upstream, handshake, hello, steps).await
        }
        (Strategy::Split, Some(at)) => {
            upstream.write_all(&hello[..at]).await?;
            upstream.flush().await?;
            pause().await;
            upstream.write_all(&hello[at..]).await
        }
        (Strategy::SplitEarly, _) if hello.len() > 1 => {
            upstream.write_all(&hello[..1]).await?;
            upstream.flush().await?;
            pause().await;
            upstream.write_all(&hello[1..]).await
        }
        (Strategy::Oob, Some(at)) => {
            upstream.write_all(&hello[..at]).await?;
            upstream.flush().await?;
            send_urgent(upstream)?;
            pause().await;
            upstream.write_all(&hello[at..]).await
        }
        (Strategy::TtlFake(ttl), Some(_)) => {
            let h = handshake.ok_or_else(|| io::Error::other("no handshake data"))?;
            let decoy = make_fake(hello).ok_or_else(|| io::Error::other("no server name to fake"))?;
            rawfake::inject(h, h.seq, &decoy, Fooling { ttl, ..Fooling::default() }, 1)?;
            pause().await;
            // The real hello follows with the very sequence number the decoy used.
            upstream.write_all(hello).await
        }
        (Strategy::Disorder, Some(at)) => {
            upstream.set_ttl(1)?;
            upstream.write_all(&hello[..at]).await?;
            upstream.flush().await?;
            pause().await;
            upstream.set_ttl(64)?;
            upstream.write_all(&hello[at..]).await
        }
        (Strategy::TlsRec, Some(at)) => {
            let (a, b) = split_records(hello, at);
            upstream.write_all(&[a, b].concat()).await
        }
        (Strategy::Both, Some(at)) => {
            let (a, b) = split_records(hello, at);
            upstream.write_all(&a).await?;
            upstream.flush().await?;
            pause().await;
            upstream.write_all(&b).await
        }
        _ => upstream.write_all(hello).await,
    }
}

/// Carries out a strategy's plan: slices of the real hello go through the kernel, the forged
/// segments are injected between them.
async fn run_plan(
    upstream: &mut TcpStream,
    handshake: &rawfake::Handshake,
    hello: &[u8],
    steps: Vec<Step>,
) -> io::Result<()> {
    for step in steps {
        match step {
            Step::Raw(forged) => {
                let seq = handshake.seq.wrapping_add(forged.offset as u32);
                rawfake::inject(handshake, seq, &forged.payload, forged.fooling, forged.repeats)?;
            }
            Step::Write(range) => {
                upstream.write_all(&hello[range]).await?;
                upstream.flush().await?;
            }
            Step::Pause => tokio::time::sleep(PAUSE).await,
        }
    }
    Ok(())
}

/// Reads exactly one TLS record (the ClientHello) from the client.
async fn read_record(client: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 5];
    timeout(HELLO_TIMEOUT, client.read_exact(&mut buf)).await??;
    if buf[0] != 0x16 {
        // Not TLS; hand over whatever has arrived and let it through untouched.
        return Ok(buf);
    }
    let len = usize::from(u16::from_be_bytes([buf[3], buf[4]]));
    if len > 16 * 1024 + 256 {
        return Err(io::Error::other("oversized TLS record"));
    }
    buf.resize(5 + len, 0);
    timeout(HELLO_TIMEOUT, client.read_exact(&mut buf[5..])).await??;
    Ok(buf)
}

/// Location (offset, length) of the host name inside a ClientHello record.
pub(crate) fn sni_range(record: &[u8]) -> Option<(usize, usize)> {
    let at = |p: usize| record.get(p).copied();
    let word = |p: usize| Some(usize::from(u16::from_be_bytes([at(p)?, at(p + 1)?])));
    if at(0)? != 0x16 || at(5)? != 0x01 {
        return None;
    }
    // Record header 5, handshake header 4, version 2, random 32.
    let mut p = 5 + 4 + 2 + 32;
    p += 1 + usize::from(at(p)?);
    p += 2 + word(p)?;
    p += 1 + usize::from(at(p)?);
    let end = (p + 2 + word(p)?).min(record.len());
    p += 2;
    while p + 4 <= end {
        let kind = word(p)?;
        let len = word(p + 2)?;
        p += 4;
        if kind == 0 {
            // server_name: list length 2, name type 1, name length 2, name.
            let name_len = word(p + 3)?;
            let start = p + 5;
            return (start + name_len <= record.len()).then_some((start, name_len));
        }
        p += len;
    }
    None
}

/// Re-encodes one handshake record as two valid records cut at `at`.
fn split_records(record: &[u8], at: usize) -> (Vec<u8>, Vec<u8>) {
    let payload = &record[5..];
    let cut = (at - 5).min(payload.len());
    let make = |part: &[u8]| {
        let mut out = vec![record[0], record[1], record[2]];
        out.extend_from_slice(&(part.len() as u16).to_be_bytes());
        out.extend_from_slice(part);
        out
    };
    (make(&payload[..cut]), make(&payload[cut..]))
}

async fn connect(shared: &Shared, host: &str, port: u16, blocked: bool) -> io::Result<TcpStream> {
    if blocked && let Some(proxy) = &shared.cfg.upstream_proxy {
        return connect_via_proxy(proxy, host, port).await;
    }
    let mut last = io::Error::other("no addresses");
    for address in resolve(shared, host, port, blocked).await? {
        match timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last = e,
            Err(_) => last = io::ErrorKind::TimedOut.into(),
        }
    }
    Err(last)
}

/// Tunnels through the configured proxy: `socks5://host:port` speaks SOCKS5 (no login),
/// anything else (`host:port`, `http://host:port`) is an HTTP CONNECT proxy.
async fn connect_via_proxy(proxy: &str, host: &str, port: u16) -> io::Result<TcpStream> {
    if let Some(address) = proxy.strip_prefix("socks5://") {
        return connect_via_socks5(address.trim_end_matches('/'), host, port).await;
    }
    let proxy = proxy.strip_prefix("http://").unwrap_or(proxy).trim_end_matches('/');
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(proxy)).await??;
    let request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 || timeout(CONNECT_TIMEOUT, stream.read(&mut byte)).await?? == 0 {
            return Err(io::Error::other("upstream proxy closed the connection"));
        }
        head.push(byte[0]);
    }
    if head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200") {
        Ok(stream)
    } else {
        Err(io::Error::other("upstream proxy refused CONNECT"))
    }
}

async fn connect_via_socks5(proxy: &str, host: &str, port: u16) -> io::Result<TcpStream> {
    let refused = |what: &str| io::Error::other(format!("SOCKS5 proxy: {what}"));
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(proxy)).await??;
    // Greeting: version 5, one method, "no authentication".
    stream.write_all(&[5, 1, 0]).await?;
    let mut answer = [0u8; 2];
    timeout(CONNECT_TIMEOUT, stream.read_exact(&mut answer)).await??;
    if answer != [5, 0] {
        return Err(refused("needs a login, which is not supported"));
    }
    // CONNECT by name, so the proxy resolves the host itself.
    let name = host.as_bytes();
    let length = u8::try_from(name.len()).map_err(|_| refused("host name too long"))?;
    let mut request = vec![5, 1, 0, 3, length];
    request.extend_from_slice(name);
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut head = [0u8; 4];
    timeout(CONNECT_TIMEOUT, stream.read_exact(&mut head)).await??;
    if head[0] != 5 || head[1] != 0 {
        return Err(refused(&format!("refused the connection (code {})", head[1])));
    }
    // Skip the bound address that follows the reply.
    let rest = match head[3] {
        1 => 4 + 2,
        4 => 16 + 2,
        3 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).await?;
            usize::from(length[0]) + 2
        }
        _ => return Err(refused("sent an unknown address type")),
    };
    let mut bound = vec![0u8; rest];
    timeout(CONNECT_TIMEOUT, stream.read_exact(&mut bound)).await??;
    Ok(stream)
}

async fn resolve(
    shared: &Shared,
    host: &str,
    port: u16,
    blocked: bool,
) -> io::Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    if blocked && shared.cfg.doh {
        let cached = shared
            .dns
            .lock()
            .unwrap()
            .get(host)
            .filter(|(_, at)| at.elapsed() < DNS_TTL)
            .map(|(ips, _)| ips.clone());
        let ips = match cached {
            Some(ips) => ips,
            None => {
                let ips = doh_lookup(&shared.doh, host).await;
                if !ips.is_empty() {
                    shared
                        .dns
                        .lock()
                        .unwrap()
                        .insert(host.to_owned(), (ips.clone(), Instant::now()));
                }
                ips
            }
        };
        if !ips.is_empty() {
            return Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect());
        }
    }
    Ok(tokio::net::lookup_host((host, port)).await?.collect())
}

/// IPv4 lookup over DNS-over-HTTPS, addressed by IP so that it needs no DNS and no SNI.
async fn doh_lookup(http: &reqwest::Client, host: &str) -> Vec<IpAddr> {
    for base in DOH {
        let Ok(response) = http
            .get(*base)
            .query(&[("name", host), ("type", "A")])
            .header("accept", "application/dns-json")
            .send()
            .await
        else {
            continue;
        };
        let Ok(text) = response.text().await else {
            continue;
        };
        let ips = parse_doh(&text);
        if !ips.is_empty() {
            return ips;
        }
    }
    Vec::new()
}
fn parse_doh(text: &str) -> Vec<IpAddr> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    json["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["type"] == 1)
        .filter_map(|a| a["data"].as_str()?.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn client_hello(host: &str) -> Vec<u8> {
        let mut ext = Vec::new();
        // A padding-ish extension first, so the name is not at a fixed offset.
        ext.extend_from_slice(&[0x00, 0x0b, 0x00, 0x02, 0x01, 0x00]);
        let name = host.as_bytes();
        let mut sni = Vec::new();
        sni.extend_from_slice(&((name.len() + 3) as u16).to_be_bytes());
        sni.push(0);
        sni.extend_from_slice(&(name.len() as u16).to_be_bytes());
        sni.extend_from_slice(name);
        ext.extend_from_slice(&[0x00, 0x00]);
        ext.extend_from_slice(&(sni.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sni);
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[7; 32]);
        body.push(32);
        body.extend_from_slice(&[9; 32]);
        body.extend_from_slice(&[0, 2, 0x13, 0x01]);
        body.extend_from_slice(&[1, 0]);
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);
        let mut hs = vec![0x01, 0, (body.len() >> 8) as u8, body.len() as u8];
        hs.extend_from_slice(&body);
        let mut record = vec![0x16, 0x03, 0x01];
        record.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        record.extend_from_slice(&hs);
        record
    }
    #[test]
    fn finds_the_server_name() {
        let hello = client_hello("api-v2.soundcloud.com");
        let (start, len) = sni_range(&hello).unwrap();
        assert_eq!(&hello[start..start + len], b"api-v2.soundcloud.com");
        assert_eq!(sni_range(&hello[..40]), None);
        assert_eq!(sni_range(b"GET / HTTP/1.1\r\n\r\n"), None);
    }
    #[test]
    fn record_split_keeps_the_handshake_intact_and_hides_the_name() {
        let hello = client_hello("a-v2.sndcdn.com");
        let (start, len) = sni_range(&hello).unwrap();
        let at = start + len / 2;
        let (a, b) = split_records(&hello, at);
        for part in [&a, &b] {
            assert_eq!(part[0], 0x16);
            assert_eq!(
                usize::from(u16::from_be_bytes([part[3], part[4]])),
                part.len() - 5
            );
        }
        let mut joined = a[5..].to_vec();
        joined.extend_from_slice(&b[5..]);
        assert_eq!(joined, hello[5..]);
        assert!(!a.windows(len).any(|w| w == b"a-v2.sndcdn.com"));
    }
    #[test]
    fn blocked_hosts_match_domains_and_subdomains_only() {
        let shared = Shared {
            cfg: Bypass::default(),
            order: Mutex::new(Vec::new()),
            setting: Mutex::new(String::new()),
            domains: vec!["sndcdn.com".into()],
            dns: Mutex::new(HashMap::new()),
            working: Mutex::new(HashMap::new()),
            cache: None,
            doh: reqwest::Client::new(),
        };
        assert!(shared.blocked("a-v2.sndcdn.com"));
        assert!(shared.blocked("SNDCDN.com."));
        assert!(!shared.blocked("notsndcdn.com"));
        assert!(!shared.blocked("example.org"));
    }
    /// What the fake server answers: the start of a TLS ServerHello, or an alert.
    const HELLO: &[u8] = b"\x16\x03\x03\x00\x04\x02\x00\x00\x00";
    const ALERT: &[u8] = b"\x15\x03\x03\x00\x02\x02\x28";
    /// An upstream that behaves like a naive SNI filter: it drops the connection when the
    /// first TLS record of the first segment contains the host name, and answers otherwise.
    async fn filtering_upstream() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let record = if n >= 5 {
                        5 + usize::from(u16::from_be_bytes([buf[3], buf[4]]))
                    } else {
                        n
                    };
                    if buf[..n.min(record)].windows(9).any(|w| w == b"127.0.0.1") {
                        return;
                    }
                    // Collect the rest of the handshake, then check it arrived intact:
                    // the records' payloads must add up to exactly the original message.
                    let mut data = buf[..n].to_vec();
                    while let Ok(Ok(m)) =
                        timeout(Duration::from_millis(300), socket.read(&mut buf)).await
                    {
                        if m == 0 {
                            break;
                        }
                        data.extend_from_slice(&buf[..m]);
                    }
                    let (mut payload, mut rest) = (Vec::new(), &data[..]);
                    while rest.len() >= 5 {
                        let len = 5 + usize::from(u16::from_be_bytes([rest[3], rest[4]]));
                        if len > rest.len() {
                            break;
                        }
                        payload.extend_from_slice(&rest[5..len]);
                        rest = &rest[len..];
                    }
                    let intact = rest.is_empty() && payload == client_hello("127.0.0.1")[5..];
                    let _ = socket
                        .write_all(if intact { HELLO } else { ALERT })
                        .await;
                    let _ = socket.read(&mut buf).await;
                });
            }
        });
        port
    }
    async fn through_proxy(strategy: &str, port: u16) -> Vec<u8> {
        let cfg = Bypass {
            strategy: strategy.into(),
            doh: false,
            domains: vec!["127.0.0.1".into()],
            upstream_proxy: None,
        };
        let (proxy, _) = start(cfg, None).await.unwrap();
        let mut client = TcpStream::connect(proxy).await.unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 200"));
        client.write_all(&client_hello("127.0.0.1")).await.unwrap();
        let mut reply = Vec::new();
        let _ = timeout(Duration::from_secs(10), async {
            let mut chunk = [0u8; 64];
            while reply.len() < HELLO.len() {
                match client.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => reply.extend_from_slice(&chunk[..n]),
                }
            }
        })
        .await;
        reply
    }
    #[tokio::test]
    async fn tunnels_past_a_filter_that_blocks_the_untouched_handshake() {
        let port = filtering_upstream().await;
        // Without the bypass the filter sees the name and drops the connection...
        assert_eq!(through_proxy("off", port).await, b"");
        // ...with it, every strategy (and auto, which tries them in turn) gets through.
        for strategy in ["split", "splitearly", "tlsrec", "both", "oob", "disorder", "auto"] {
            assert_eq!(through_proxy(strategy, port).await, HELLO, "{strategy}");
        }
    }
    #[tokio::test]
    async fn the_strategy_can_be_switched_while_the_proxy_runs() {
        let port = filtering_upstream().await;
        let cfg = Bypass {
            strategy: "off".into(),
            doh: false,
            domains: vec!["127.0.0.1".into()],
            upstream_proxy: None,
        };
        let (_, handle) = start(cfg, None).await.unwrap();
        assert_eq!(handle.strategy(), "off");
        assert!(handle.untouched());
        assert!(handle.set_strategy("no such strategy").is_err());
        assert_eq!(handle.strategy(), "off", "a bad name changes nothing");
        handle.set_strategy("ALT 11").unwrap();
        assert_eq!(handle.strategy(), "ALT 11");
        assert!(handle.set_strategy("auto").is_ok());
        let _ = port;
    }
    #[test]
    fn strategy_names_round_trip_and_every_preset_is_reachable() {
        for strategy in Strategy::all() {
            assert_eq!(Strategy::parse(&strategy.name()), Some(strategy), "{strategy:?}");
            assert!(!strategy.describe().is_empty());
        }
        assert_eq!(Strategy::all().len(), 7 + strategy::PRESETS.len());
        assert_eq!(Strategy::parse("ttlfake9"), Some(Strategy::TtlFake(9)));
        assert_eq!(Strategy::parse("Split Early"), Some(Strategy::SplitEarly));
        assert_eq!(Strategy::parse("general (SIMPLE FAKE ALT2)").map(Strategy::name).as_deref(), Some("simple-fake-alt2"));
        assert!(Strategy::parse("alt99").is_none());
        // Settings: one name is one strategy, `auto` is a list ending in the untouched hello.
        assert_eq!(order_for("alt9").unwrap().len(), 1);
        assert_eq!(order_for("off").unwrap(), [Strategy::Plain]);
        assert!(order_for("auto").unwrap().len() > 4);
        assert_eq!(order_for("auto").unwrap().last(), Some(&Strategy::Plain));
        assert!(order_for("bogus").is_none());
        for strategy in Strategy::all() {
            assert!(strategy.forges() == matches!(strategy, Strategy::TtlFake(_) | Strategy::Preset(_)));
        }
    }
    #[tokio::test]
    async fn an_upstream_proxy_carries_blocked_hosts_untouched() {
        // The "proxy" sees a CONNECT, then the client's hello exactly as the client sent it.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            socket.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.unwrap();
            let mut hello = vec![0u8; client_hello("blocked.test").len()];
            socket.read_exact(&mut hello).await.unwrap();
            socket.write_all(HELLO).await.unwrap();
            (String::from_utf8(head).unwrap(), hello)
        });
        let cfg = Bypass {
            strategy: "auto".into(),
            doh: false,
            domains: vec!["blocked.test".into()],
            upstream_proxy: Some(proxy_address.to_string()),
        };
        let (local, _) = start(cfg, None).await.unwrap();
        let mut client = TcpStream::connect(local).await.unwrap();
        client.write_all(b"CONNECT blocked.test:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let sent = client_hello("blocked.test");
        client.write_all(&sent).await.unwrap();
        let mut reply = vec![0u8; HELLO.len()];
        timeout(Duration::from_secs(5), client.read_exact(&mut reply)).await.unwrap().unwrap();
        assert_eq!(reply, HELLO);
        let (connect, hello) = seen.await.unwrap();
        assert!(connect.starts_with("CONNECT blocked.test:443 HTTP/1.1"));
        assert_eq!(hello, sent, "no desync applied through an upstream proxy");
    }
    #[tokio::test]
    async fn a_socks5_upstream_proxy_resolves_and_carries_the_host() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut greeting = [0u8; 3];
            socket.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            socket.write_all(&[5, 0]).await.unwrap();
            let mut head = [0u8; 5];
            socket.read_exact(&mut head).await.unwrap();
            assert_eq!(&head[..4], &[5, 1, 0, 3]);
            let mut name = vec![0u8; usize::from(head[4]) + 2];
            socket.read_exact(&mut name).await.unwrap();
            // Success, bound to 0.0.0.0:0.
            socket.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
            let mut hello = vec![0u8; client_hello("blocked.test").len()];
            socket.read_exact(&mut hello).await.unwrap();
            socket.write_all(HELLO).await.unwrap();
            name
        });
        let cfg = Bypass {
            strategy: "auto".into(),
            doh: false,
            domains: vec!["blocked.test".into()],
            upstream_proxy: Some(format!("socks5://{proxy_address}")),
        };
        let (local, _) = start(cfg, None).await.unwrap();
        let mut client = TcpStream::connect(local).await.unwrap();
        client.write_all(b"CONNECT blocked.test:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        client.write_all(&client_hello("blocked.test")).await.unwrap();
        let mut reply = vec![0u8; HELLO.len()];
        timeout(Duration::from_secs(5), client.read_exact(&mut reply)).await.unwrap().unwrap();
        assert_eq!(reply, HELLO);
        let name = seen.await.unwrap();
        assert_eq!(&name[..name.len() - 2], b"blocked.test");
        assert_eq!(&name[name.len() - 2..], &443u16.to_be_bytes());
    }
    #[test]
    fn the_decoy_differs_in_name_and_session_id_but_not_in_length() {
        let hello = client_hello("api-v2.soundcloud.com");
        let fake = make_fake(&hello).unwrap();
        let (start, len) = sni_range(&hello).unwrap();
        assert_eq!(fake.len(), hello.len());
        assert_eq!(sni_range(&fake), Some((start, len)));
        assert_ne!(fake[start..start + len], hello[start..start + len]);
        assert!(fake[start..start + len].ends_with(b".com"));
        assert_ne!(
            fake[SESSION_ID_AT..SESSION_ID_AT + 33],
            hello[SESSION_ID_AT..SESSION_ID_AT + 33]
        );
        // Everything after the session id (ciphers, key share, ...) is untouched.
        assert_eq!(fake[SESSION_ID_AT + 33..start], hello[SESSION_ID_AT + 33..start]);
    }
    #[test]
    fn a_reply_to_the_decoy_is_recognised_by_its_session_id() {
        let hello = client_hello("a-v2.sndcdn.com");
        let fake = make_fake(&hello).unwrap();
        let reply_to = |request: &[u8]| {
            let mut reply = vec![0x16, 0x03, 0x03, 0x00, 0x7a, 0x02, 0, 0, 0x76, 0x03, 0x03];
            reply.extend_from_slice(&[1; 32]);
            reply.extend_from_slice(&request[SESSION_ID_AT..SESSION_ID_AT + 33]);
            reply
        };
        assert!(echoes_session_id(&hello, &reply_to(&hello)));
        assert!(!echoes_session_id(&hello, &reply_to(&fake)));
        // Replies that carry no id to compare are given the benefit of the doubt.
        assert!(echoes_session_id(&hello, &[0x16, 0x03, 0x03]));
    }
    #[test]
    fn parses_doh_answers() {
        let text = r#"{"Answer":[{"type":5,"data":"x.example."},{"type":1,"data":"1.2.3.4"}]}"#;
        assert_eq!(parse_doh(text), vec!["1.2.3.4".parse::<IpAddr>().unwrap()]);
        assert!(parse_doh("garbage").is_empty());
    }
}
