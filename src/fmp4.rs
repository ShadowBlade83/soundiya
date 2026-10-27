//! Just enough of fragmented MP4 (CMAF) to pull AAC frames out of SoundCloud's HLS segments:
//! the codec configuration from the init segment, then the samples of each `moof`/`mdat` pair.
use std::{
    collections::VecDeque,
    io::{self, Read},
};

const MAX_BOX: u64 = 32 * 1024 * 1024;
const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

#[derive(Debug, PartialEq, Eq)]
pub struct AacConfig {
    /// The AudioSpecificConfig bytes the decoder is initialised with.
    pub asc: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u8,
}

pub struct Reader<R: Read> {
    reader: R,
    sizes: VecDeque<u32>,
    ready: VecDeque<Vec<u8>>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}
fn be32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

impl<R: Read> Reader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            sizes: VecDeque::new(),
            ready: VecDeque::new(),
        }
    }

    /// Reads the init segment (`ftyp`, `moov`) and returns the AAC configuration in it.
    pub fn read_init(&mut self) -> io::Result<AacConfig> {
        loop {
            let (kind, payload) = self
                .next_box()?
                .ok_or_else(|| invalid("stream ended before the init segment"))?;
            match &kind {
                b"moov" => return parse_config(&payload).ok_or_else(|| invalid("no AAC config")),
                b"moof" | b"mdat" => return Err(invalid("missing init segment")),
                _ => {}
            }
        }
    }

    /// The next compressed AAC frame, or `None` at the end of the stream.
    pub fn next_sample(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(sample) = self.ready.pop_front() {
                return Ok(Some(sample));
            }
            let Some((kind, payload)) = self.next_box()? else {
                return Ok(None);
            };
            match &kind {
                b"moof" => self.sizes = sample_sizes(&payload).into(),
                b"mdat" => {
                    let mut offset = 0;
                    while let Some(size) = self.sizes.pop_front() {
                        let end = offset + size as usize;
                        let sample = payload.get(offset..end).ok_or_else(|| invalid("short mdat"))?;
                        self.ready.push_back(sample.to_vec());
                        offset = end;
                    }
                }
                _ => {}
            }
        }
    }

    fn next_box(&mut self) -> io::Result<Option<([u8; 4], Vec<u8>)>> {
        let mut head = [0u8; 8];
        // A clean end of stream is only allowed between boxes.
        if self.reader.read(&mut head[..1])? == 0 {
            return Ok(None);
        }
        self.reader.read_exact(&mut head[1..])?;
        let mut size = u64::from(u32::from_be_bytes(head[..4].try_into().unwrap()));
        let kind: [u8; 4] = head[4..].try_into().unwrap();
        let mut header = 8;
        if size == 1 {
            let mut large = [0u8; 8];
            self.reader.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
            header = 16;
        }
        if size < header || size - header > MAX_BOX {
            return Err(invalid("unsupported MP4 box size"));
        }
        let mut payload = vec![0u8; (size - header) as usize];
        self.reader.read_exact(&mut payload)?;
        Ok(Some((kind, payload)))
    }
}

/// Splits a buffer of sibling boxes.
fn children(mut buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while buf.len() >= 8 {
        let size = be32(buf, 0).unwrap() as usize;
        if size < 8 || size > buf.len() {
            break;
        }
        out.push((buf[4..8].try_into().unwrap(), &buf[8..size]));
        buf = &buf[size..];
    }
    out
}

/// Sample sizes, in order, of every `trun` in a `moof`.
fn sample_sizes(moof: &[u8]) -> Vec<u32> {
    let mut sizes = Vec::new();
    for (kind, traf) in children(moof) {
        if &kind != b"traf" {
            continue;
        }
        let mut default_size = 0;
        for (kind, body) in children(traf) {
            match &kind {
                b"tfhd" => {
                    let flags = be32(body, 0).unwrap_or(0) & 0xff_ffff;
                    let mut at = 8;
                    at += if flags & 0x01 != 0 { 8 } else { 0 };
                    at += if flags & 0x02 != 0 { 4 } else { 0 };
                    at += if flags & 0x08 != 0 { 4 } else { 0 };
                    if flags & 0x10 != 0 {
                        default_size = be32(body, at).unwrap_or(0);
                    }
                }
                b"trun" => {
                    let flags = be32(body, 0).unwrap_or(0) & 0xff_ffff;
                    let count = be32(body, 4).unwrap_or(0);
                    let mut at = 8;
                    at += if flags & 0x01 != 0 { 4 } else { 0 };
                    at += if flags & 0x04 != 0 { 4 } else { 0 };
                    for _ in 0..count {
                        at += if flags & 0x100 != 0 { 4 } else { 0 };
                        if flags & 0x200 != 0 {
                            sizes.push(be32(body, at).unwrap_or(0));
                            at += 4;
                        } else {
                            sizes.push(default_size);
                        }
                        at += if flags & 0x400 != 0 { 4 } else { 0 };
                        at += if flags & 0x800 != 0 { 4 } else { 0 };
                    }
                }
                _ => {}
            }
        }
    }
    sizes
}

/// Finds the `esds` box in a `moov` and decodes the AudioSpecificConfig inside it.
fn parse_config(moov: &[u8]) -> Option<AacConfig> {
    let at = moov.windows(4).position(|w| w == b"esds")? + 4 + 4; // type, version/flags
    let buf = moov.get(at..)?;
    let mut p = 0;
    let descriptor = |p: &mut usize| -> Option<(u8, usize)> {
        let tag = *buf.get(*p)?;
        *p += 1;
        let mut len = 0usize;
        for _ in 0..4 {
            let byte = *buf.get(*p)?;
            *p += 1;
            len = (len << 7) | usize::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
        Some((tag, len))
    };
    // ES_Descriptor: id (2), flags (1), then optional dependency/URL/OCR fields.
    let (tag, _) = descriptor(&mut p)?;
    if tag != 0x03 {
        return None;
    }
    let flags = *buf.get(p + 2)?;
    p += 3;
    if flags & 0x80 != 0 {
        p += 2;
    }
    if flags & 0x40 != 0 {
        p += 1 + usize::from(*buf.get(p)?);
    }
    if flags & 0x20 != 0 {
        p += 2;
    }
    // DecoderConfigDescriptor: object type, stream type, buffer size, two bitrates.
    let (tag, _) = descriptor(&mut p)?;
    if tag != 0x04 {
        return None;
    }
    p += 13;
    let (tag, len) = descriptor(&mut p)?;
    if tag != 0x05 {
        return None;
    }
    let asc = buf.get(p..p + len)?.to_vec();
    let (sample_rate, channels) = parse_asc(&asc)?;
    Some(AacConfig { asc, sample_rate, channels })
}

fn parse_asc(asc: &[u8]) -> Option<(u32, u8)> {
    let mut bits = asc.iter().flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1));
    let mut take = |n: u32| -> Option<u32> {
        (0..n).try_fold(0u32, |acc, _| Some((acc << 1) | u32::from(bits.next()?)))
    };
    if take(5)? == 31 {
        take(6)?;
    }
    let index = take(4)?;
    let rate = if index == 15 {
        take(24)?
    } else {
        *SAMPLE_RATES.get(index as usize)?
    };
    let channels = take(4)? as u8;
    (channels > 0).then_some((rate, channels.min(2)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }
    fn init_segment(asc: &[u8]) -> Vec<u8> {
        // DecoderSpecificInfo inside DecoderConfigDescriptor inside ES_Descriptor.
        let mut dcd_body = vec![0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        dcd_body.extend_from_slice(&[0x05, asc.len() as u8]);
        dcd_body.extend_from_slice(asc);
        let mut es_body = vec![0, 1, 0, 0x04, dcd_body.len() as u8];
        es_body.extend_from_slice(&dcd_body);
        let mut esds = vec![0, 0, 0, 0, 0x03, es_body.len() as u8];
        esds.extend_from_slice(&es_body);
        let moov = make_box(b"moov", &make_box(b"trak", &make_box(b"esds", &esds)));
        [make_box(b"ftyp", b"iso5\0\0\0\0"), moov].concat()
    }
    fn fragment(samples: &[&[u8]]) -> Vec<u8> {
        let mut trun = vec![0, 0, 2, 1]; // version 0, flags: data offset + sample size
        trun.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        trun.extend_from_slice(&0u32.to_be_bytes());
        for s in samples {
            trun.extend_from_slice(&(s.len() as u32).to_be_bytes());
        }
        let tfhd = make_box(b"tfhd", &[0, 2, 0, 0, 0, 0, 0, 1]);
        let traf = make_box(b"traf", &[tfhd, make_box(b"trun", &trun)].concat());
        [
            make_box(b"moof", &traf),
            make_box(b"mdat", &samples.concat()),
        ]
        .concat()
    }
    #[test]
    fn reads_the_aac_config_from_the_init_segment() {
        // AAC-LC, 44.1 kHz, stereo.
        let init = init_segment(&[0x12, 0x10]);
        let config = Reader::new(&init[..]).read_init().unwrap();
        assert_eq!(config.asc, [0x12, 0x10]);
        assert_eq!((config.sample_rate, config.channels), (44_100, 2));
        assert!(Reader::new(&init[..10]).read_init().is_err());
        assert!(Reader::new(&fragment(&[b"x"])[..]).read_init().is_err());
    }
    #[test]
    fn splits_segments_into_frames_across_fragments() {
        let stream = [
            init_segment(&[0x12, 0x10]),
            fragment(&[b"aaa", b"bb"]),
            make_box(b"sidx", &[0; 12]),
            fragment(&[b"c"]),
        ]
        .concat();
        let mut reader = Reader::new(&stream[..]);
        reader.read_init().unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = reader.next_sample().unwrap() {
            frames.push(frame);
        }
        assert_eq!(frames, [b"aaa".to_vec(), b"bb".to_vec(), b"c".to_vec()]);
    }
    #[test]
    fn truncated_boxes_are_errors_not_silence() {
        let stream = [init_segment(&[0x12, 0x10]), fragment(&[b"abc"])].concat();
        let mut reader = Reader::new(&stream[..stream.len() - 2]);
        reader.read_init().unwrap();
        assert!(reader.next_sample().is_err());
    }
    #[test]
    fn decodes_audio_specific_config() {
        assert_eq!(parse_asc(&[0x11, 0x90]), Some((48_000, 2)));
        assert_eq!(parse_asc(&[0x12, 0x08]), Some((44_100, 1)));
        assert_eq!(parse_asc(&[]), None);
    }
}
