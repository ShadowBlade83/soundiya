//! Streams a track from SoundCloud, decodes it (AAC in fragmented MP4, or MP3) and plays it
//! through ALSA.
//!
//! Network fetching runs on the tokio runtime and feeds a bounded channel; a dedicated
//! thread decodes from that channel and writes to the sound card, so a slow network only
//! ever starves the buffer and never blocks the rest of the receiver.
use crate::{
    ducking::{CHANNELS, Ducking, Gain},
    fmp4,
    soundcloud::{Codec, Stream},
};
use alsa::{
    Direction, ValueOr,
    pcm::{Access, Format, HwParams, PCM},
};
use bytes::Bytes;
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
        mpsc as std_mpsc,
    },
    time::Duration,
};
use symphonia::core::{
    audio::{Channels, SampleBuffer},
    codecs::{CODEC_TYPE_AAC, CODEC_TYPE_NULL, CodecParameters, Decoder, DecoderOptions},
    errors::Error as DecodeError,
    formats::{FormatOptions, FormatReader, Packet},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    probe::Hint,
};
use tokio::sync::mpsc;

pub const IDLE: u8 = 0;
pub const PLAYING: u8 = 1;
pub const PAUSED: u8 = 2;
const STALL: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum Event {
    Started(u64),
    Ended(u64),
    Failed(u64, String),
}

pub struct Shared {
    pub state: AtomicU8,
    pub position_ms: AtomicU64,
    /// The only track generation allowed to keep reading; a newer one aborts blocked reads.
    generation: AtomicU64,
}

enum Command {
    Play(u64, ChannelSource),
    Pause,
    Resume,
    Stop,
}

pub struct Player {
    commands: std_mpsc::Sender<Command>,
    pub shared: Arc<Shared>,
}
impl Player {
    pub fn spawn(device: String, ducking: Arc<Ducking>) -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (commands, receiver) = std_mpsc::channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state: AtomicU8::new(IDLE),
            position_ms: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        });
        let worker = Worker {
            receiver,
            events,
            shared: shared.clone(),
            device,
            ducking,
        };
        std::thread::Builder::new()
            .name("soundiya-audio".into())
            .stack_size(256 * 1024)
            .spawn(move || worker.run())
            .expect("audio thread");
        (Self { commands, shared }, event_rx)
    }
    /// Starts a new generation, which cancels everything still loading for older ones.
    pub fn begin(&self) -> u64 {
        self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
    /// Starts downloading `stream` for `generation`; the result is handed to [`Player::play`].
    pub fn open(&self, generation: u64, http: reqwest::Client, stream: Stream) -> ChannelSource {
        spawn_fetch(http, stream, generation, self.shared.clone())
    }
    pub fn play(&self, generation: u64, source: ChannelSource) {
        let _ = self.commands.send(Command::Play(generation, source));
    }
    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }
    pub fn resume(&self) {
        let _ = self.commands.send(Command::Resume);
    }
    pub fn stop(&self) {
        self.begin();
        let _ = self.commands.send(Command::Stop);
    }
    /// True while `generation` has not been superseded or stopped.
    pub fn is_current(&self, generation: u64) -> bool {
        self.shared.generation.load(Ordering::Acquire) == generation
    }
    pub fn state(&self) -> u8 {
        self.shared.state.load(Ordering::Acquire)
    }
}

/// Bytes arriving from the network, presented to the decoder as a plain stream.
pub struct ChannelSource {
    receiver: mpsc::Receiver<io::Result<Bytes>>,
    pending: Bytes,
    generation: u64,
    shared: Arc<Shared>,
    codec: Codec,
}
impl Read for ChannelSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if !self.pending.is_empty() {
                let n = out.len().min(self.pending.len());
                out[..n].copy_from_slice(&self.pending.split_to(n));
                return Ok(n);
            }
            match self.receiver.try_recv() {
                Ok(Ok(bytes)) => self.pending = bytes,
                Ok(Err(e)) => return Err(e),
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(0),
                Err(mpsc::error::TryRecvError::Empty) => {
                    if self.shared.generation.load(Ordering::Acquire) != self.generation {
                        return Err(io::ErrorKind::Interrupted.into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}
impl Seek for ChannelSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::ErrorKind::Unsupported.into())
    }
}
impl MediaSource for ChannelSource {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn spawn_fetch(
    http: reqwest::Client,
    stream: Stream,
    generation: u64,
    shared: Arc<Shared>,
) -> ChannelSource {
    let (sender, receiver) = mpsc::channel(24);
    let codec = stream.codec;
    tokio::spawn(async move {
        let result = if stream.hls {
            fetch_hls(&http, &stream.url, &sender).await
        } else {
            fetch(&http, &stream.url, &sender).await
        };
        if let Err(e) = result {
            let _ = sender.send(Err(io::Error::other(e))).await;
        }
    });
    ChannelSource {
        receiver,
        pending: Bytes::new(),
        generation,
        shared,
        codec,
    }
}

async fn fetch(
    http: &reqwest::Client,
    url: &str,
    sender: &mpsc::Sender<io::Result<Bytes>>,
) -> Result<(), String> {
    let mut response = tokio::time::timeout(STALL, http.get(url).send())
        .await
        .map_err(|_| "audio download: no answer from the server".to_owned())?
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("audio download: HTTP {}", response.status()));
    }
    loop {
        match tokio::time::timeout(STALL, response.chunk()).await {
            Ok(Ok(Some(bytes))) => {
                if sender.send(Ok(bytes)).await.is_err() {
                    return Ok(());
                }
            }
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(e)) => return Err(format!("audio download: {e}")),
            Err(_) => return Err("audio download stalled".into()),
        }
    }
}

async fn fetch_hls(
    http: &reqwest::Client,
    url: &str,
    sender: &mpsc::Sender<io::Result<Bytes>>,
) -> Result<(), String> {
    let playlist = tokio::time::timeout(STALL, async {
        http.get(url).send().await?.text().await
    })
    .await
    .map_err(|_| "audio playlist: no answer from the server".to_owned())?
    .map_err(|e| e.to_string())?;
    let base = &url[..url.rfind('/').map_or(0, |i| i + 1)];
    let (init, segments) = playlist_parts(&playlist, base);
    // The init segment comes first so the decoder sees one continuous fMP4 stream.
    for part in init.into_iter().chain(segments) {
        if sender.is_closed() {
            return Ok(());
        }
        fetch(http, &part, sender).await?;
    }
    Ok(())
}

/// The init segment (`#EXT-X-MAP`, fragmented MP4 only) and the media segments, as absolute URLs.
fn playlist_parts(playlist: &str, base: &str) -> (Option<String>, Vec<String>) {
    let absolute = |l: &str| {
        if l.starts_with("http") {
            l.to_owned()
        } else {
            format!("{base}{l}")
        }
    };
    let init = playlist.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("#EXT-X-MAP:")?;
        let uri = &rest[rest.find("URI=\"")? + 5..];
        Some(absolute(&uri[..uri.find('"')?]))
    });
    let segments = playlist
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(absolute)
        .collect();
    (init, segments)
}

/// Plays a quiet one-second 440 Hz tone through the same output path as music, which proves the
/// sound card, its sample rate and the ALSA setup work independently of the network.
pub fn tone(device: &str) -> Result<(), String> {
    let mut output = Output::open(device, 44_100)?;
    let samples: Vec<i16> = (0..output.rate)
        .flat_map(|n| {
            let phase = f64::from(n) * 440.0 * std::f64::consts::TAU / f64::from(output.rate);
            let fade = (f64::from(n) / 2000.0).min(f64::from(output.rate - n) / 2000.0).min(1.0);
            let value = (phase.sin() * 2500.0 * fade) as i16;
            [value, value]
        })
        .collect();
    output.write(&samples)?;
    output.pcm.drain().map_err(|e| format!("audio: {e}"))
}

/// Downloads and decodes the first couple of seconds without touching the sound card; used by
/// `soundiya check` to prove that the whole pipeline works. Returns (sample rate, frames).
pub async fn probe(http: reqwest::Client, stream: Stream) -> Result<(u32, usize), String> {
    let shared = Arc::new(Shared {
        state: AtomicU8::new(IDLE),
        position_ms: AtomicU64::new(0),
        generation: AtomicU64::new(1),
    });
    let source = spawn_fetch(http, stream, 1, shared);
    tokio::task::spawn_blocking(move || {
        let (mut demux, mut decoder) = Demux::open(source)?;
        let (mut frames, mut rate) = (0usize, 0u32);
        while frames < 88_200 {
            let Some(packet) = demux.next()? else {
                break;
            };
            match decoder.decode(&packet) {
                Ok(decoded) => {
                    rate = decoded.spec().rate;
                    frames += decoded.frames();
                }
                Err(DecodeError::DecodeError(_)) => {}
                Err(e) => return Err(format!("decode: {e}")),
            }
        }
        if frames == 0 {
            return Err("no audio decoded".into());
        }
        Ok((rate, frames))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Folds any channel layout down to interleaved stereo.
fn to_stereo(samples: &[i16], channels: usize, out: &mut Vec<i16>) {
    out.clear();
    match channels {
        2 => out.extend_from_slice(samples),
        1 => out.extend(samples.iter().flat_map(|s| [*s, *s])),
        n => out.extend(samples.chunks_exact(n).flat_map(|f| [f[0], f[1]])),
    }
}

/// Streaming linear-interpolation resampler for the rare track whose rate the card refuses.
struct Resampler {
    step: f64,
    pos: f64,
    last: [i16; 2],
}
impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: f64::from(from) / f64::from(to),
            pos: 0.0,
            last: [0; 2],
        }
    }
    fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        let frames = input.len() / CHANNELS;
        if frames == 0 {
            return;
        }
        let frame = |i: isize, last: [i16; 2]| -> [f64; 2] {
            if i < 0 {
                [f64::from(last[0]), f64::from(last[1])]
            } else {
                let i = i as usize * CHANNELS;
                [f64::from(input[i]), f64::from(input[i + 1])]
            }
        };
        while self.pos < (frames - 1) as f64 {
            let index = self.pos.floor();
            let fraction = self.pos - index;
            let a = frame(index as isize, self.last);
            let b = frame(index as isize + 1, self.last);
            for channel in 0..CHANNELS {
                out.push((a[channel] + (b[channel] - a[channel]) * fraction) as i16);
            }
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        self.last = [input[(frames - 1) * CHANNELS], input[(frames - 1) * CHANNELS + 1]];
    }
}

struct Output {
    pcm: PCM,
    rate: u32,
    frames: u64,
    resampler: Option<Resampler>,
}
impl Output {
    fn open(device: &str, track_rate: u32) -> Result<Self, String> {
        let pcm = PCM::new(device, Direction::Playback, false)
            .map_err(|e| format!("audio device {device}: {e}"))?;
        let rate = {
            let err = |e: alsa::Error| format!("audio device {device}: {e}");
            let mut hw = HwParams::any(&pcm).map_err(err)?;
            hw.set_access(Access::RWInterleaved).map_err(err)?;
            hw.set_format(Format::s16()).map_err(err)?;
            hw.set_channels(CHANNELS as u32).map_err(err)?;
            hw.set_rate(track_rate, ValueOr::Nearest).map_err(err)?;
            // Half a second of buffer; if the card objects, keep its defaults.
            let working = hw.clone();
            if hw
                .set_buffer_size_near((track_rate / 2) as alsa::pcm::Frames)
                .is_err()
            {
                hw = working;
            }
            pcm.hw_params(&hw).map_err(err)?;
            hw.get_rate().map_err(err)?
        };
        Ok(Self {
            pcm,
            rate,
            frames: 0,
            resampler: (rate != track_rate).then(|| Resampler::new(track_rate, rate)),
        })
    }
    fn write(&mut self, mut data: &[i16]) -> Result<(), String> {
        let io = self.pcm.io_i16().map_err(|e| format!("audio: {e}"))?;
        while !data.is_empty() {
            match io.writei(data) {
                Ok(frames) => {
                    self.frames += frames as u64;
                    data = &data[frames * CHANNELS..];
                }
                Err(e) => self
                    .pcm
                    .try_recover(e, true)
                    .map_err(|e| format!("audio write: {e}"))?,
            }
        }
        Ok(())
    }
}

/// Compressed packets from either container, plus the decoder that understands them.
enum Demux {
    Mp3 {
        format: Box<dyn FormatReader>,
        track_id: u32,
    },
    Aac {
        reader: fmp4::Reader<ChannelSource>,
        timestamp: u64,
    },
}
impl Demux {
    fn open(source: ChannelSource) -> Result<(Self, Box<dyn Decoder>), String> {
        let codecs = symphonia::default::get_codecs();
        match source.codec {
            Codec::Mp3 => {
                let stream = MediaSourceStream::new(Box::new(source), Default::default());
                let mut hint = Hint::new();
                hint.with_extension("mp3");
                let probed = symphonia::default::get_probe()
                    .format(&hint, stream, &FormatOptions::default(), &MetadataOptions::default())
                    .map_err(|e| format!("unsupported audio: {e}"))?;
                let format = probed.format;
                let track = format
                    .tracks()
                    .iter()
                    .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
                    .ok_or("no audio track")?;
                let track_id = track.id;
                let decoder = codecs
                    .make(&track.codec_params, &DecoderOptions::default())
                    .map_err(|e| format!("decoder: {e}"))?;
                Ok((Self::Mp3 { format, track_id }, decoder))
            }
            Codec::Aac => {
                let mut reader = fmp4::Reader::new(source);
                let config = reader.read_init().map_err(|e| format!("stream: {e}"))?;
                let channels = if config.channels == 1 {
                    Channels::FRONT_LEFT
                } else {
                    Channels::FRONT_LEFT | Channels::FRONT_RIGHT
                };
                let mut params = CodecParameters::new();
                params
                    .for_codec(CODEC_TYPE_AAC)
                    .with_sample_rate(config.sample_rate)
                    .with_channels(channels)
                    .with_extra_data(config.asc.into_boxed_slice());
                let decoder = codecs
                    .make(&params, &DecoderOptions::default())
                    .map_err(|e| format!("decoder: {e}"))?;
                Ok((Self::Aac { reader, timestamp: 0 }, decoder))
            }
        }
    }
    /// `None` once the stream has ended normally.
    fn next(&mut self) -> Result<Option<Packet>, String> {
        match self {
            Self::Mp3 { format, track_id } => loop {
                match format.next_packet() {
                    Ok(packet) if packet.track_id() == *track_id => return Ok(Some(packet)),
                    Ok(_) => {}
                    Err(DecodeError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        return Ok(None);
                    }
                    Err(e) => return Err(format!("stream: {e}")),
                }
            },
            Self::Aac { reader, timestamp } => match reader.next_sample() {
                Ok(Some(frame)) => {
                    let packet = Packet::new_from_slice(0, *timestamp, 1024, &frame);
                    *timestamp += 1024;
                    Ok(Some(packet))
                }
                Ok(None) => Ok(None),
                Err(e) => Err(format!("stream: {e}")),
            },
        }
    }
}

enum Outcome {
    Ended,
    Stopped,
    Replace(u64, ChannelSource),
}
struct Worker {
    receiver: std_mpsc::Receiver<Command>,
    events: mpsc::UnboundedSender<Event>,
    shared: Arc<Shared>,
    device: String,
    ducking: Arc<Ducking>,
}
impl Worker {
    fn run(self) {
        let mut next = None;
        loop {
            let (generation, source) = match next.take() {
                Some(job) => job,
                None => match self.receiver.recv() {
                    Ok(Command::Play(generation, source)) => (generation, source),
                    Ok(_) => continue,
                    Err(_) => return,
                },
            };
            let result = self.track(generation, source);
            self.shared.state.store(IDLE, Ordering::Release);
            match result {
                Ok(Outcome::Ended) => {
                    let _ = self.events.send(Event::Ended(generation));
                }
                Ok(Outcome::Stopped) => {}
                Ok(Outcome::Replace(generation, source)) => next = Some((generation, source)),
                // Whatever went wrong after the track was replaced is not an error.
                Err(_) if self.shared.generation.load(Ordering::Acquire) != generation => {}
                Err(e) => {
                    let _ = self.events.send(Event::Failed(generation, e));
                }
            }
        }
    }

    /// Waits out a pause; returns what to do next if playback does not simply continue.
    fn paused(&self, output: &Option<Output>) -> Option<Outcome> {
        if let Some(out) = output {
            let _ = out.pcm.drop();
        }
        self.shared.state.store(PAUSED, Ordering::Release);
        loop {
            match self.receiver.recv() {
                Ok(Command::Resume) => {
                    if let Some(out) = output {
                        let _ = out.pcm.prepare();
                    }
                    self.shared.state.store(PLAYING, Ordering::Release);
                    return None;
                }
                Ok(Command::Play(g, s)) => return Some(Outcome::Replace(g, s)),
                Ok(Command::Stop) | Err(_) => return Some(Outcome::Stopped),
                Ok(Command::Pause) => {}
            }
        }
    }

    fn track(&self, generation: u64, source: ChannelSource) -> Result<Outcome, String> {
        let (mut demux, mut decoder) = Demux::open(source)?;
        let mut output: Option<Output> = None;
        let mut gain = Gain::new();
        let mut stereo: Vec<i16> = Vec::new();
        let mut resampled: Vec<i16> = Vec::new();
        let mut samples: Option<SampleBuffer<i16>> = None;
        loop {
            match self.receiver.try_recv() {
                Ok(Command::Stop) => return Ok(Outcome::Stopped),
                Ok(Command::Play(g, s)) => return Ok(Outcome::Replace(g, s)),
                Ok(Command::Pause) => {
                    if let Some(outcome) = self.paused(&output) {
                        return Ok(outcome);
                    }
                }
                Ok(Command::Resume) | Err(std_mpsc::TryRecvError::Empty) => {}
                Err(std_mpsc::TryRecvError::Disconnected) => return Ok(Outcome::Stopped),
            }
            let Some(packet) = demux.next()? else {
                break;
            };
            let decoded = match decoder.decode(&packet) {
                Ok(d) => d,
                Err(DecodeError::DecodeError(_)) => continue,
                Err(e) => return Err(format!("decode: {e}")),
            };
            let spec = *decoded.spec();
            let buffer =
                samples.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
            buffer.copy_interleaved_ref(decoded);
            to_stereo(buffer.samples(), spec.channels.count().max(1), &mut stereo);
            if output.is_none() {
                output = Some(Output::open(&self.device, spec.rate)?);
            }
            let out = output.as_mut().unwrap();
            let data: &mut [i16] = match out.resampler.as_mut() {
                Some(resampler) => {
                    resampled.clear();
                    resampler.process(&stereo, &mut resampled);
                    &mut resampled
                }
                None => &mut stereo,
            };
            gain.apply_i16(data, self.ducking.active());
            out.write(data)?;
            if self.shared.state.swap(PLAYING, Ordering::AcqRel) == IDLE {
                let _ = self.events.send(Event::Started(generation));
            }
            self.shared
                .position_ms
                .store(out.frames * 1000 / u64::from(out.rate), Ordering::Relaxed);
        }
        if let Some(out) = &output {
            let _ = out.pcm.drain();
        }
        Ok(Outcome::Ended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hls_playlists_yield_absolute_init_and_segment_urls() {
        let playlist = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4?sig=1\"\n#EXTINF:10,\ndata000.m4s?sig=1\n#EXTINF:10,\nhttps://cdn/x/data001.m4s\n#EXT-X-ENDLIST\n";
        let (init, segments) = playlist_parts(playlist, "https://cdn/base/");
        assert_eq!(init.as_deref(), Some("https://cdn/base/init.mp4?sig=1"));
        assert_eq!(
            segments,
            ["https://cdn/base/data000.m4s?sig=1", "https://cdn/x/data001.m4s"]
        );
        assert_eq!(playlist_parts("#EXTM3U\nseg.mp3\n", "b/").0, None);
    }
    #[test]
    fn channel_layouts_become_stereo() {
        let mut out = Vec::new();
        to_stereo(&[1, 2, 3], 1, &mut out);
        assert_eq!(out, [1, 1, 2, 2, 3, 3]);
        to_stereo(&[1, 2, 3, 4], 2, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
        to_stereo(&[1, 2, 9, 3, 4, 9], 3, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
    }
    #[test]
    fn resampling_scales_length_and_is_independent_of_chunking() {
        let input: Vec<i16> = (0..4800).flat_map(|i| [i as i16, -(i as i16)]).collect();
        let mut whole = Vec::new();
        Resampler::new(48_000, 44_100).process(&input, &mut whole);
        let expected = 4800.0 * 44_100.0 / 48_000.0;
        assert!((whole.len() as f64 / 2.0 - expected).abs() < 3.0);
        let mut split = Vec::new();
        let mut resampler = Resampler::new(48_000, 44_100);
        for chunk in input.chunks(2 * 576) {
            resampler.process(chunk, &mut split);
        }
        let common = whole.len().min(split.len());
        assert!(common > 4000);
        // Float rounding may differ by one step at chunk edges, never more.
        assert!(
            whole[..common - 4]
                .iter()
                .zip(&split[..common - 4])
                .all(|(a, b)| (i32::from(*a) - i32::from(*b)).abs() <= 1)
        );
    }
}
