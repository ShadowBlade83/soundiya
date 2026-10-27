//! The strategies of Flowseal's zapret-discord-youtube, as data, and the packet plan that each
//! one produces for a TLS ClientHello.
//!
//! A preset mirrors the options of the HTTPS (`--filter-tcp=80,443`) profile of one `.bat` file:
//! `--dpi-desync` modes, split positions, `seqovl`, fakes and their mods, fooling, repeats. A
//! plan is the ordered list of things to do: write a slice of the real hello through the kernel,
//! or forge a segment with a given sequence offset. Planning is pure, so it is covered by tests;
//! the executor in `bypass.rs` carries the plan out.
//!
//! One difference from `winws`: it intercepts the kernel's packets and replaces them, so every
//! segment on the wire is its own. Here the kernel sends the real data itself and only the extra
//! (forged) segments are injected next to it. For fakes, `seqovl` and the split modes that is
//! equivalent to a server and close to it for a middlebox.
use crate::{bypass::sni_range, rawfake::Fooling};
use std::ops::Range;

pub const STUN: &[u8] = include_bytes!("../fakes/stun.bin");
pub const STUN2: &[u8] = include_bytes!("../fakes/stun2.bin");
pub const GOOGLE: &[u8] = include_bytes!("../fakes/tls_clienthello_www_google_com.bin");
pub const SFERUM: &[u8] = include_bytes!("../fakes/tls_clienthello_www_sferum_ru.bin");
pub const PDA4: &[u8] = include_bytes!("../fakes/tls_clienthello_4pda_to.bin");
pub const SOCHI: &[u8] = include_bytes!("../fakes/tls_clienthello_sochi_park.bin");

/// Where a fake's bytes come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fake {
    Stun,
    Stun2,
    Google,
    Sferum,
    Pda4,
    Sochi,
    /// `0x00000000`
    Zero4,
    /// zapret's built-in fake (`!`); a real ClientHello of the same kind stands in for it.
    Standard,
}
impl Fake {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Stun => STUN,
            Self::Stun2 => STUN2,
            Self::Google | Self::Standard => GOOGLE,
            Self::Sferum => SFERUM,
            Self::Pda4 => PDA4,
            Self::Sochi => SOCHI,
            Self::Zero4 => &[0; 4],
        }
    }
    fn is_tls(self) -> bool {
        !matches!(self, Self::Stun | Self::Stun2 | Self::Zero4)
    }
}

/// `--dpi-desync-fake-tls-mod`: how a fake ClientHello is altered before it is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlsMod {
    pub rnd: bool,
    pub rndsni: bool,
    pub dupsid: bool,
    pub sni: Option<&'static str>,
}
impl TlsMod {
    pub const NONE: Self = Self { rnd: false, rndsni: false, dupsid: false, sni: None };
    /// What zapret does to its built-in fake when no mod is given (every preset states its own).
    #[cfg(test)]
    pub const DEFAULT: Self = Self { rnd: true, rndsni: true, dupsid: true, sni: None };
    const fn with_sni(sni: &'static str) -> Self {
        Self { rnd: true, rndsni: false, dupsid: true, sni: Some(sni) }
    }
}

/// `--dpi-desync` modes this program understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Fake,
    Multisplit,
    Multidisorder,
    Fakedsplit,
    Hostfakesplit,
    /// Data in the SYN. The kernel writes its own SYN, so this one cannot be applied and is skipped.
    Syndata,
}

/// A split position: an absolute offset or a marker with an offset. Not every marker zapret
/// knows is used by a preset, but all of them are understood in a plan.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Abs(i32),
    Host(i32),
    EndHost(i32),
    Sld(i32),
    MidSld(i32),
    EndSld(i32),
    /// Start of the data field of the SNI extension.
    SniExt(i32),
}

#[derive(Clone, Copy, Debug)]
pub struct Preset {
    /// The short name used in the configuration, such as `alt11`.
    pub name: &'static str,
    /// The name of the `.bat` file, minus `general`.
    pub title: &'static str,
    pub modes: &'static [Mode],
    pub split: &'static [Marker],
    /// `--dpi-desync-split-seqovl` and its pattern.
    pub seqovl: Option<(usize, Fake)>,
    pub fakes: &'static [(Fake, TlsMod)],
    pub repeats: u32,
    pub ts: bool,
    pub md5: bool,
    /// `badseq` fooling with its increment.
    pub badseq: Option<i32>,
    /// `--dpi-desync-hostfakesplit-mod`: the fake host template and `altorder`.
    pub host: Option<&'static str>,
    pub altorder: u8,
}
impl PartialEq for Preset {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

const BASE: Preset = Preset {
    name: "",
    title: "",
    modes: &[],
    split: &[],
    seqovl: None,
    fakes: &[],
    repeats: 1,
    ts: false,
    md5: false,
    badseq: None,
    host: None,
    altorder: 0,
};

use Fake::{Google, Pda4, Sferum, Sochi, Standard, Stun, Stun2, Zero4};
use Marker::{Abs, MidSld, SniExt};
use Mode::{Fake as F, Fakedsplit, Hostfakesplit, Multidisorder, Multisplit, Syndata};

const NO: TlsMod = TlsMod::NONE;
const GOOGLE_SNI: TlsMod = TlsMod::with_sni("www.google.com");

/// Every HTTPS profile of the repository, in the order of the file list.
pub const PRESETS: &[Preset] = &[
    Preset { name: "general", title: "general", modes: &[Multisplit], split: &[Abs(1)], seqovl: Some((568, Pda4)), ..BASE },
    Preset {
        name: "alt", title: "general (ALT)", modes: &[F, Fakedsplit], repeats: 6, ts: true,
        fakes: &[(Stun, NO), (Google, NO)], ..BASE
    },
    Preset { name: "alt2", title: "general (ALT2)", modes: &[Multisplit], split: &[Abs(2)], seqovl: Some((652, Google)), ..BASE },
    Preset {
        name: "alt3", title: "general (ALT3)", modes: &[F, Hostfakesplit], ts: true, host: Some("ya.ru"), altorder: 1,
        fakes: &[(Standard, TlsMod::with_sni("ya.ru"))], ..BASE
    },
    Preset {
        name: "alt4", title: "general (ALT4)", modes: &[F, Multisplit], repeats: 6, badseq: Some(1000),
        fakes: &[(Stun, NO), (Google, NO)], ..BASE
    },
    Preset { name: "alt5", title: "general (ALT5)", modes: &[Syndata, Multidisorder], ..BASE },
    Preset { name: "alt6", title: "general (ALT6)", modes: &[Multisplit], split: &[Abs(1)], seqovl: Some((681, Google)), ..BASE },
    Preset {
        name: "alt7", title: "general (ALT7)", modes: &[Multisplit], split: &[Abs(2), SniExt(1)],
        seqovl: Some((679, Google)), ..BASE
    },
    Preset {
        name: "alt8", title: "general (ALT8)", modes: &[F], repeats: 6, badseq: Some(2),
        fakes: &[(Standard, NO)], ..BASE
    },
    Preset {
        name: "alt9", title: "general (ALT9)", modes: &[Hostfakesplit], repeats: 4, ts: true, md5: true,
        host: Some("ozon.ru"), ..BASE
    },
    Preset {
        name: "alt10", title: "general (ALT10)", modes: &[F], repeats: 6, ts: true,
        fakes: &[(Stun, NO), (Pda4, NO)], ..BASE
    },
    Preset {
        name: "alt11", title: "general (ALT11)", modes: &[F, Multisplit], split: &[Abs(1)],
        seqovl: Some((664, Sferum)), repeats: 8, ts: true, fakes: &[(Stun2, NO), (Sferum, NO)], ..BASE
    },
    Preset {
        name: "alt12", title: "general (ALT12)", modes: &[F, Multisplit], split: &[Abs(1)],
        seqovl: Some((664, Sferum)), repeats: 8, ts: true, fakes: &[(Stun, NO), (Sferum, NO)], ..BASE
    },
    Preset {
        name: "alt13", title: "general (ALT13)", modes: &[F, Hostfakesplit], repeats: 5, ts: true,
        host: Some("mail.ru"), altorder: 1, fakes: &[(Sochi, NO), (Stun2, NO)], ..BASE
    },
    Preset {
        name: "exp", title: "general (EXP)", modes: &[F, Multisplit], split: &[Abs(1)],
        seqovl: Some((480, Stun2)), repeats: 4, ts: true, fakes: &[(Sferum, NO)], ..BASE
    },
    Preset {
        name: "fake-tls-auto", title: "general (FAKE TLS AUTO)", modes: &[F, Multidisorder],
        split: &[Abs(1), MidSld(0)], repeats: 11, badseq: Some(-10_000),
        fakes: &[(Zero4, NO), (Standard, GOOGLE_SNI)], ..BASE
    },
    Preset {
        name: "fake-tls-auto-alt", title: "general (FAKE TLS AUTO ALT)", modes: &[F, Fakedsplit],
        split: &[Abs(1)], repeats: 8, badseq: Some(2), fakes: &[(Standard, GOOGLE_SNI)], ..BASE
    },
    Preset {
        name: "fake-tls-auto-alt2", title: "general (FAKE TLS AUTO ALT2)", modes: &[F, Multisplit],
        split: &[Abs(1)], seqovl: Some((681, Google)), repeats: 8, badseq: Some(10_000_000),
        fakes: &[(Standard, GOOGLE_SNI)], ..BASE
    },
    Preset {
        name: "fake-tls-auto-alt3", title: "general (FAKE TLS AUTO ALT3)", modes: &[F, Multisplit],
        split: &[Abs(1)], seqovl: Some((681, Google)), repeats: 8, ts: true,
        fakes: &[(Standard, GOOGLE_SNI)], ..BASE
    },
    Preset {
        name: "simple-fake", title: "general (SIMPLE FAKE)", modes: &[F], repeats: 6, ts: true,
        fakes: &[(Google, NO)], ..BASE
    },
    Preset {
        name: "simple-fake-alt", title: "general (SIMPLE FAKE ALT)", modes: &[F], repeats: 6, badseq: Some(2),
        fakes: &[(Stun2, NO), (Google, NO)], ..BASE
    },
    Preset {
        name: "simple-fake-alt2", title: "general (SIMPLE FAKE ALT2)", modes: &[F], repeats: 6, ts: true,
        fakes: &[(Stun2, NO), (Sferum, NO)], ..BASE
    },
];

/// Finds a preset by its short name or the name of its `.bat` file, ignoring case, spaces,
/// dashes and brackets: `alt11`, `ALT 11`, `general (ALT11)`.
pub fn find(name: &str) -> Option<&'static Preset> {
    let key = |s: &str| -> String {
        let key: String = s.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase();
        key.strip_prefix("general").filter(|rest| !rest.is_empty()).map_or(key.clone(), str::to_owned)
    };
    let wanted = key(name);
    PRESETS.iter().find(|p| key(p.name) == wanted || key(p.title) == wanted)
}

// ---------------------------------------------------------------------------------------------
// Plans

/// A forged segment. `offset` is relative to the first byte of the hello.
#[derive(Clone, Debug, PartialEq)]
pub struct Forged {
    pub offset: i64,
    pub payload: Vec<u8>,
    pub fooling: Fooling,
    pub repeats: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Forge a segment.
    Raw(Forged),
    /// Let the kernel send this slice of the real hello.
    Write(Range<usize>),
    /// A short pause, so that consecutive segments leave in order.
    Pause,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn alnum(&mut self) -> u8 {
        b"abcdefghijklmnopqrstuvwxyz0123456789"[(self.next() % 36) as usize]
    }
}

/// What the plan needs to know about the real hello.
struct Ctx {
    len: usize,
    host: Option<Range<usize>>,
}
impl Ctx {
    fn new(hello: &[u8]) -> Self {
        Self {
            len: hello.len(),
            host: sni_range(hello).map(|(start, len)| start..start + len),
        }
    }
    /// The second-level domain of the host name, as a range of the hello.
    fn sld(&self, hello: &[u8]) -> Option<Range<usize>> {
        let host = self.host.clone()?;
        let name = &hello[host.clone()];
        let labels: Vec<&[u8]> = name.split(|b| *b == b'.').collect();
        if labels.len() < 2 {
            return Some(host);
        }
        let tld = labels[labels.len() - 1].len();
        let sld = labels[labels.len() - 2].len();
        let end = host.end - tld - 1;
        Some(end - sld..end)
    }
    fn resolve(&self, marker: Marker, hello: &[u8]) -> Option<usize> {
        let at = |base: usize, delta: i32| usize::try_from(base as i64 + i64::from(delta)).ok();
        match marker {
            Marker::Abs(n) if n >= 0 => at(0, n),
            Marker::Abs(n) => at(self.len, n),
            Marker::Host(d) => at(self.host.clone()?.start, d),
            Marker::EndHost(d) => at(self.host.clone()?.end, d),
            Marker::Sld(d) => at(self.sld(hello)?.start, d),
            Marker::EndSld(d) => at(self.sld(hello)?.end, d),
            Marker::MidSld(d) => {
                let sld = self.sld(hello)?;
                at(sld.start + (sld.end - sld.start) / 2, d)
            }
            Marker::SniExt(d) => at(self.host.clone()?.start.checked_sub(5)?, d),
        }
    }
}

/// Where the session id is: record header 5, handshake header 4, version 2, random 32.
const SESSION_ID_AT: usize = 5 + 4 + 2 + 32;
fn session_id_range(hello: &[u8]) -> Option<Range<usize>> {
    let len = usize::from(*hello.get(SESSION_ID_AT)?);
    (hello.len() >= SESSION_ID_AT + 1 + len).then_some(SESSION_ID_AT + 1..SESSION_ID_AT + 1 + len)
}

/// Resolved, sorted, deduplicated cut positions strictly inside the hello.
fn cuts(markers: &[Marker], ctx: &Ctx, hello: &[u8]) -> Vec<usize> {
    let mut cuts: Vec<usize> = markers
        .iter()
        .filter_map(|m| ctx.resolve(*m, hello))
        .filter(|p| *p > 0 && *p < ctx.len)
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

fn pieces(cuts: &[usize], len: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    for cut in cuts {
        out.push(from..*cut);
        from = *cut;
    }
    out.push(from..len);
    out
}

/// The single position `fakedsplit` uses: the first marker that is not absolute, else the first
/// absolute one, else 1.
fn single_cut(markers: &[Marker], ctx: &Ctx, hello: &[u8]) -> usize {
    let valid = |p: &usize| *p > 0 && *p < ctx.len;
    markers
        .iter()
        .filter(|m| !matches!(m, Marker::Abs(_)))
        .chain(markers.iter().filter(|m| matches!(m, Marker::Abs(_))))
        .filter_map(|m| ctx.resolve(*m, hello))
        .find(valid)
        .unwrap_or(1)
}

fn repeat_pattern(pattern: &[u8], len: usize) -> Vec<u8> {
    pattern.iter().copied().cycle().take(len).collect()
}

/// A fake host name of exactly `len` bytes. With a template the result ends in it and a random
/// subdomain pads the front; without one a dot is placed to look like a three-letter TLD.
fn fake_host(template: Option<&str>, len: usize, rng: &mut Rng) -> Option<Vec<u8>> {
    let random = |rng: &mut Rng, n: usize| -> Vec<u8> { (0..n).map(|_| rng.alnum()).collect() };
    match template {
        Some(template) => {
            let template = template.as_bytes();
            match len.checked_sub(template.len())? {
                0 => Some(template.to_vec()),
                // A single padding byte would leave an empty label.
                1 => None,
                pad => {
                    let mut name = random(rng, pad - 1);
                    name.push(b'.');
                    name.extend_from_slice(template);
                    Some(name)
                }
            }
        }
        None if len >= 7 => {
            let mut name = random(rng, len - 4);
            name.push(b'.');
            name.extend_from_slice(b"com");
            Some(name)
        }
        None => Some(random(rng, len)),
    }
}

/// The bytes of one fake, after its mods.
fn render_fake(fake: Fake, tls_mod: &TlsMod, hello: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut bytes = fake.bytes().to_vec();
    if !fake.is_tls() {
        return bytes;
    }
    if let Some(name) = tls_mod.sni {
        replace_sni(&mut bytes, name.as_bytes());
    } else if tls_mod.rndsni
        && let Some((_, len)) = sni_range(&bytes)
        && let Some(name) = fake_host(None, len, rng)
    {
        replace_sni(&mut bytes, &name);
    }
    if tls_mod.rnd && bytes.len() >= SESSION_ID_AT {
        for byte in &mut bytes[5 + 4 + 2..SESSION_ID_AT] {
            *byte = rng.next() as u8;
        }
        if let Some(range) = session_id_range(&bytes) {
            for byte in &mut bytes[range] {
                *byte = rng.next() as u8;
            }
        }
    }
    if tls_mod.dupsid
        && let Some(sid) = session_id_range(hello)
    {
        set_session_id(&mut bytes, &hello[sid]);
    }
    bytes
}

/// Adds `delta` to a big-endian length field of `width` bytes at `at`.
fn adjust(buf: &mut [u8], at: usize, width: usize, delta: i64) {
    let mut value = 0i64;
    for byte in &buf[at..at + width] {
        value = (value << 8) | i64::from(*byte);
    }
    value += delta;
    for (i, byte) in buf[at..at + width].iter_mut().enumerate() {
        *byte = (value >> (8 * (width - 1 - i))) as u8;
    }
}

/// Replaces the session id, fixing the handshake and record lengths.
fn set_session_id(hello: &mut Vec<u8>, sid: &[u8]) {
    let Some(old) = session_id_range(hello) else {
        return;
    };
    let delta = sid.len() as i64 - old.len() as i64;
    hello.splice(SESSION_ID_AT..old.end, std::iter::once(sid.len() as u8).chain(sid.iter().copied()));
    adjust(hello, 3, 2, delta);
    adjust(hello, 6, 3, delta);
}

/// Replaces the server name, fixing every length that contains it.
fn replace_sni(hello: &mut Vec<u8>, name: &[u8]) {
    let Some((start, old)) = sni_range(hello) else {
        return;
    };
    let delta = name.len() as i64 - old as i64;
    // Where the extensions block starts: after the session id, the cipher suites and the
    // compression methods.
    let Some(sid) = session_id_range(hello) else {
        return;
    };
    let ciphers = sid.end;
    let ciphers_len = usize::from(u16::from_be_bytes([hello[ciphers], hello[ciphers + 1]]));
    let comp = ciphers + 2 + ciphers_len;
    let exts_at = comp + 1 + usize::from(hello[comp]);
    hello.splice(start..start + old, name.iter().copied());
    adjust(hello, start - 2, 2, delta); // name length
    adjust(hello, start - 5, 2, delta); // server name list length
    adjust(hello, start - 7, 2, delta); // extension length
    adjust(hello, exts_at, 2, delta); // all extensions
    adjust(hello, 6, 3, delta); // handshake
    adjust(hello, 3, 2, delta); // record
}

/// The ordered actions for sending `hello` the way `preset` describes.
pub fn plan(preset: &Preset, hello: &[u8], seed: u64) -> Result<Vec<Step>, String> {
    let ctx = Ctx::new(hello);
    let mut rng = Rng(seed | 1);
    let fooling = Fooling {
        ttl: 0,
        ts: preset.ts,
        md5: preset.md5,
        ..Fooling::default()
    };
    let has = |mode: Mode| preset.modes.contains(&mode);
    let len = hello.len();
    let mut steps = Vec::new();

    if has(Mode::Fake) {
        let bad = i64::from(preset.badseq.unwrap_or(0));
        for (fake, tls_mod) in preset.fakes {
            steps.push(Step::Raw(Forged {
                offset: bad,
                payload: render_fake(*fake, tls_mod, hello, &mut rng),
                fooling,
                repeats: preset.repeats,
            }));
        }
        if !preset.fakes.is_empty() {
            steps.push(Step::Pause);
        }
    }

    if has(Mode::Multisplit) || has(Mode::Multidisorder) {
        let markers: &[Marker] = if preset.split.is_empty() { &[Abs(2)] } else { preset.split };
        let parts = pieces(&cuts(markers, &ctx, hello), len);
        if parts.len() < 2 {
            steps.push(Step::Write(0..len));
        } else if has(Mode::Multisplit) {
            if let Some((overlap, pattern)) = preset.seqovl
                // The overlap and the first piece must fit one segment; zapret cancels it otherwise.
                && overlap + parts[0].len() <= 1400
            {
                let mut payload = repeat_pattern(pattern.bytes(), overlap);
                payload.extend_from_slice(&hello[parts[0].clone()]);
                steps.push(Step::Raw(Forged {
                    offset: -(overlap as i64),
                    payload,
                    fooling: Fooling::default(),
                    repeats: 1,
                }));
                steps.push(Step::Pause);
            }
            for part in parts {
                steps.push(Step::Write(part));
                steps.push(Step::Pause);
            }
        } else {
            // multidisorder: later pieces first, then the kernel sends the whole thing, which
            // the server takes as a retransmission of what it already holds.
            for part in parts.iter().rev() {
                steps.push(Step::Raw(Forged {
                    offset: part.start as i64,
                    payload: hello[part.clone()].to_vec(),
                    fooling: Fooling::default(),
                    repeats: 1,
                }));
                steps.push(Step::Pause);
            }
            steps.push(Step::Write(0..len));
        }
    } else if has(Mode::Fakedsplit) {
        let cut = single_cut(preset.split, &ctx, hello);
        let bad = i64::from(preset.badseq.unwrap_or(0));
        // `--dpi-desync-fakedsplit-pattern` defaults to 0x00; the fakes copy the size of the
        // segment they imitate. altorder=0: fake 1st, 1st, fake 1st, fake 2nd, 2nd, fake 2nd.
        let fake = |from: usize, to: usize| {
            Step::Raw(Forged {
                offset: from as i64 + bad,
                payload: vec![0; to - from],
                fooling,
                repeats: preset.repeats,
            })
        };
        steps.extend([
            fake(0, cut),
            Step::Pause,
            Step::Write(0..cut),
            Step::Pause,
            fake(0, cut),
            fake(cut, len),
            Step::Pause,
            Step::Write(cut..len),
            Step::Pause,
            fake(cut, len),
        ]);
    } else if has(Mode::Hostfakesplit) {
        let host = ctx.host.clone().ok_or("the hello has no server name")?;
        let fake = fake_host(preset.host, host.len(), &mut rng).ok_or("the server name is too short to fake")?;
        let forged = |offset: usize, payload: Vec<u8>, fooling: Fooling, repeats: u32| {
            Step::Raw(Forged { offset: offset as i64, payload, fooling, repeats })
        };
        steps.push(Step::Write(0..host.start));
        steps.push(Step::Pause);
        steps.push(forged(host.start, fake.clone(), fooling, preset.repeats));
        steps.push(Step::Pause);
        if preset.altorder == 0 {
            // before the name, fake name, real name, fake name, after the name
            steps.push(Step::Write(host.clone()));
            steps.push(Step::Pause);
            steps.push(forged(host.end, fake, fooling, preset.repeats));
            steps.push(Step::Pause);
            steps.push(Step::Write(host.end..len));
        } else {
            // before the name, fake name, after the name, real name
            steps.push(forged(host.end, hello[host.end..].to_vec(), Fooling::default(), 1));
            steps.push(Step::Pause);
            steps.push(Step::Write(host.start..len));
        }
    } else {
        steps.push(Step::Write(0..len));
    }

    while steps.last() == Some(&Step::Pause) {
        steps.pop();
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ClientHello with a 32-byte session id and the given server name.
    fn client_hello(host: &str) -> Vec<u8> {
        let name = host.as_bytes();
        let mut sni = Vec::new();
        sni.extend_from_slice(&((name.len() + 3) as u16).to_be_bytes());
        sni.push(0);
        sni.extend_from_slice(&(name.len() as u16).to_be_bytes());
        sni.extend_from_slice(name);
        let mut ext = vec![0x00, 0x0b, 0x00, 0x02, 0x01, 0x00, 0x00, 0x00];
        ext.extend_from_slice(&(sni.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sni);
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[7; 32]);
        body.push(32);
        body.extend_from_slice(&[9; 32]);
        body.extend_from_slice(&[0, 2, 0x13, 0x01, 1, 0]);
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);
        let mut hs = vec![0x01, 0, (body.len() >> 8) as u8, body.len() as u8];
        hs.extend_from_slice(&body);
        let mut record = vec![0x16, 0x03, 0x01];
        record.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        record.extend_from_slice(&hs);
        record
    }
    fn writes(steps: &[Step]) -> Vec<Range<usize>> {
        steps.iter().filter_map(|s| if let Step::Write(r) = s { Some(r.clone()) } else { None }).collect()
    }
    fn lengths_are_consistent(hello: &[u8]) -> bool {
        let record = usize::from(u16::from_be_bytes([hello[3], hello[4]]));
        let handshake = (usize::from(hello[6]) << 16) | (usize::from(hello[7]) << 8) | usize::from(hello[8]);
        let (start, len) = sni_range(hello).expect("sni");
        let name_len = usize::from(u16::from_be_bytes([hello[start - 2], hello[start - 1]]));
        let list_len = usize::from(u16::from_be_bytes([hello[start - 5], hello[start - 4]]));
        record == hello.len() - 5 && handshake == hello.len() - 9 && name_len == len && list_len == len + 3
    }

    #[test]
    fn every_preset_writes_each_byte_of_the_real_hello_exactly_once_in_order() {
        for host in ["soundcloud.com", "api-v2.soundcloud.com", "a-v2.sndcdn.com"] {
            let hello = client_hello(host);
            for preset in PRESETS {
                let steps = plan(preset, &hello, 42).unwrap_or_else(|e| panic!("{} on {host}: {e}", preset.name));
                let mut next = 0;
                for range in writes(&steps) {
                    assert_eq!(range.start, next, "{} on {host}: gap or overlap", preset.name);
                    next = range.end;
                }
                assert_eq!(next, hello.len(), "{} on {host}: the hello is not fully written", preset.name);
                assert_ne!(steps.last(), Some(&Step::Pause), "{}", preset.name);
            }
        }
    }

    #[test]
    fn alt11_sends_two_fakes_eight_times_then_the_overlap_segment_then_two_pieces() {
        let hello = client_hello("api-v2.soundcloud.com");
        let preset = find("alt11").unwrap();
        let steps = plan(preset, &hello, 1).unwrap();
        let Step::Raw(first) = &steps[0] else { panic!("{steps:?}") };
        let Step::Raw(second) = &steps[1] else { panic!("{steps:?}") };
        assert_eq!(first.payload, STUN2);
        assert_eq!(second.payload, SFERUM);
        for fake in [first, second] {
            assert_eq!((fake.offset, fake.repeats), (0, 8));
            assert!(fake.fooling.ts && !fake.fooling.md5);
        }
        let overlap = steps
            .iter()
            .find_map(|s| match s {
                Step::Raw(f) if f.offset < 0 => Some(f),
                _ => None,
            })
            .expect("seqovl segment");
        assert_eq!(overlap.offset, -664);
        assert_eq!(overlap.payload.len(), 665);
        // 661 bytes of the pattern, then it repeats up to 664, then the real first byte.
        assert_eq!(&overlap.payload[..661], SFERUM);
        assert_eq!(&overlap.payload[661..664], &SFERUM[..3]);
        assert_eq!(overlap.payload[664], hello[0]);
        assert!(!overlap.fooling.ts);
        assert_eq!(writes(&steps), [0..1, 1..hello.len()]);
    }

    #[test]
    fn alt9_fakes_the_host_name_on_both_sides_of_the_real_one() {
        let hello = client_hello("api-v2.soundcloud.com");
        let (start, len) = sni_range(&hello).unwrap();
        let steps = plan(find("alt9").unwrap(), &hello, 7).unwrap();
        let fakes: Vec<&Forged> = steps.iter().filter_map(|s| if let Step::Raw(f) = s { Some(f) } else { None }).collect();
        assert_eq!(fakes.len(), 2);
        assert_eq!(fakes[0].offset, start as i64);
        assert_eq!(fakes[1].offset, (start + len) as i64);
        for fake in &fakes {
            assert_eq!(fake.payload.len(), len);
            assert!(fake.payload.ends_with(b"ozon.ru"));
            assert!((fake.fooling.ts, fake.fooling.md5, fake.repeats) == (true, true, 4));
        }
        assert_eq!(writes(&steps), [0..start, start..start + len, start + len..hello.len()]);
    }

    #[test]
    fn altorder_one_sends_the_tail_before_the_real_host() {
        let hello = client_hello("api-v2.soundcloud.com");
        let (start, len) = sni_range(&hello).unwrap();
        let steps = plan(find("alt13").unwrap(), &hello, 3).unwrap();
        // Fakes for the TLS part come first, then the kernel writes everything before the host,
        // a fake host, the real tail forged ahead of time, and finally host plus tail.
        let tail = steps
            .iter()
            .find_map(|s| match s {
                Step::Raw(f) if f.offset == (start + len) as i64 && f.payload == hello[start + len..] => Some(f),
                _ => None,
            })
            .expect("tail forged ahead of the host");
        assert!(!tail.fooling.ts);
        assert_eq!(writes(&steps), [0..start, start..hello.len()]);
    }

    #[test]
    fn markers_resolve_against_the_host_name() {
        let hello = client_hello("api-v2.soundcloud.com");
        let ctx = Ctx::new(&hello);
        let (start, len) = sni_range(&hello).unwrap();
        let sld_start = start + "api-v2.".len();
        assert_eq!(ctx.resolve(Marker::Host(0), &hello), Some(start));
        assert_eq!(ctx.resolve(Marker::EndHost(0), &hello), Some(start + len));
        assert_eq!(ctx.resolve(Marker::Sld(0), &hello), Some(sld_start));
        assert_eq!(ctx.resolve(Marker::EndSld(0), &hello), Some(sld_start + "soundcloud".len()));
        assert_eq!(ctx.resolve(Marker::MidSld(0), &hello), Some(sld_start + 5));
        assert_eq!(ctx.resolve(Marker::SniExt(1), &hello), Some(start - 4));
        assert_eq!(ctx.resolve(Marker::Abs(-10), &hello), Some(hello.len() - 10));
        // Two positions and a duplicate collapse to a sorted list.
        assert_eq!(cuts(&[Abs(5), Marker::Host(0), Abs(5), Abs(0), Abs(9999)], &ctx, &hello), [5, start]);
    }

    #[test]
    fn fakedsplit_prefers_a_relative_marker_and_falls_back_to_one() {
        let hello = client_hello("soundcloud.com");
        let ctx = Ctx::new(&hello);
        assert_eq!(single_cut(&[], &ctx, &hello), 1);
        assert_eq!(single_cut(&[Abs(7)], &ctx, &hello), 7);
        let (start, _) = sni_range(&hello).unwrap();
        assert_eq!(single_cut(&[Abs(7), Marker::Host(0)], &ctx, &hello), start);
    }

    #[test]
    fn badseq_moves_only_the_fakes_and_overlap_pieces_stay_real() {
        let hello = client_hello("soundcloud.com");
        let steps = plan(find("alt4").unwrap(), &hello, 5).unwrap();
        let offsets: Vec<i64> = steps.iter().filter_map(|s| if let Step::Raw(f) = s { Some(f.offset) } else { None }).collect();
        assert_eq!(offsets, [1000, 1000]);
        assert_eq!(writes(&steps), [0..2, 2..hello.len()], "default split position is 2");
    }

    #[test]
    fn multidisorder_forges_pieces_in_reverse_then_writes_everything() {
        let hello = client_hello("soundcloud.com");
        let (start, len) = sni_range(&hello).unwrap();
        // "soundcloud.com": the middle of the second-level domain is five bytes into the name.
        let mid = start + 5;
        assert!(len > 5);
        let steps = plan(find("fake-tls-auto").unwrap(), &hello, 9).unwrap();
        let forged: Vec<&Forged> = steps.iter().filter_map(|s| if let Step::Raw(f) = s { Some(f) } else { None }).collect();
        // two fakes (zeros, google) with badseq -10000 and 11 repeats, then the pieces backwards
        assert_eq!(forged[0].payload, [0, 0, 0, 0]);
        assert_eq!(forged[0].offset, -10_000);
        assert_eq!(forged[0].repeats, 11);
        assert_eq!(forged[1].offset, -10_000);
        let pieces = &forged[2..];
        assert_eq!(pieces.len(), 3, "cuts at 1 and the middle of the second-level domain");
        let offsets: Vec<i64> = pieces.iter().map(|p| p.offset).collect();
        assert_eq!(offsets, [mid as i64, 1, 0]);
        assert!(pieces.iter().all(|p| p.fooling == Fooling::default() && p.repeats == 1));
        assert_eq!(writes(&steps), [0..hello.len()]);
    }

    #[test]
    fn fake_mods_keep_every_length_in_the_fake_consistent() {
        let hello = client_hello("api-v2.soundcloud.com");
        let mut rng = Rng(99);
        let fake = render_fake(Standard, &GOOGLE_SNI, &hello, &mut rng);
        assert!(lengths_are_consistent(&fake), "lengths");
        let (start, len) = sni_range(&fake).unwrap();
        assert_eq!(&fake[start..start + len], b"www.google.com");
        // dupsid: the real hello's session id is carried over.
        assert_eq!(&fake[session_id_range(&fake).unwrap()], &hello[session_id_range(&hello).unwrap()]);
        // sni=ya.ru is shorter than the original, which must shrink every length field.
        let short = render_fake(Standard, &TlsMod::with_sni("ya.ru"), &hello, &mut rng);
        assert!(lengths_are_consistent(&short));
        assert!(short.len() < GOOGLE.len());
        // The default mods randomise the name but keep the length and consistency.
        let random = render_fake(Standard, &TlsMod::DEFAULT, &hello, &mut rng);
        assert!(lengths_are_consistent(&random));
        assert_ne!(sni_range(&random).map(|(s, l)| random[s..s + l].to_vec()), sni_range(GOOGLE).map(|(s, l)| GOOGLE[s..s + l].to_vec()));
        // Files without mods and non-TLS fakes are sent exactly as they are.
        assert_eq!(render_fake(Sferum, &NO, &hello, &mut rng), SFERUM);
        assert_eq!(render_fake(Stun, &GOOGLE_SNI, &hello, &mut rng), STUN);
    }

    #[test]
    fn bundled_fakes_are_the_sizes_flowseal_ships() {
        assert_eq!([STUN.len(), STUN2.len(), GOOGLE.len(), SFERUM.len(), PDA4.len(), SOCHI.len()], [100, 120, 681, 661, 284, 244]);
        for bytes in [GOOGLE, SFERUM, PDA4, SOCHI] {
            assert_eq!(bytes[0], 0x16);
            assert!(sni_range(bytes).is_some());
        }
    }

    #[test]
    fn fake_hosts_have_the_requested_length_and_template() {
        let mut rng = Rng(5);
        for len in 9..40 {
            let name = fake_host(Some("ozon.ru"), len, &mut rng).unwrap();
            assert_eq!(name.len(), len);
            assert!(name.ends_with(b"ozon.ru"));
            assert!(name.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.'));
        }
        assert_eq!(fake_host(Some("ozon.ru"), 7, &mut rng).unwrap(), b"ozon.ru");
        assert!(fake_host(Some("ozon.ru"), 8, &mut rng).is_none());
        assert!(fake_host(Some("ozon.ru"), 3, &mut rng).is_none());
        assert_eq!(fake_host(None, 10, &mut rng).unwrap().len(), 10);
        assert!(fake_host(None, 10, &mut rng).unwrap().ends_with(b".com"));
    }

    #[test]
    fn presets_are_found_by_short_name_or_file_name() {
        assert_eq!(PRESETS.len(), 22);
        for spelling in ["alt11", "ALT11", "ALT 11", "general (ALT11)", "General (alt11)"] {
            assert_eq!(find(spelling).map(|p| p.name), Some("alt11"), "{spelling}");
        }
        assert_eq!(find("general").map(|p| p.name), Some("general"));
        assert_eq!(find("FAKE TLS AUTO ALT2").map(|p| p.name), Some("fake-tls-auto-alt2"));
        assert_eq!(find("general (SIMPLE FAKE)").map(|p| p.name), Some("simple-fake"));
        assert!(find("nonsense").is_none());
        let mut names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), PRESETS.len(), "names are unique");
    }

    #[test]
    fn a_hello_without_a_server_name_cannot_be_host_faked() {
        let mut hello = client_hello("soundcloud.com");
        hello.truncate(60);
        assert!(plan(find("alt9").unwrap(), &hello, 1).is_err());
        // The split presets fall back to one plain write.
        let steps = plan(find("alt2").unwrap(), &hello, 1).unwrap();
        assert_eq!(writes(&steps), [0..2, 2..60]);
    }
}
