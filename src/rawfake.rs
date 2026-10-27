//! Forging TCP segments that belong to one of our own connections, as zapret's `nfqws` does.
//!
//! To forge a packet of a live connection one has to know its sequence numbers and timestamps.
//! The kernel does not hand those out, so the connection is opened through a socket that is
//! bound first; a packet socket with a BPF filter for that port then catches the SYN and the
//! SYN-ACK, which carry both sides' numbers. Forged segments go out through a raw socket.
//! Needs root, which stations run as.
//!
//! A segment can be made invisible to the server in several ways ("fooling"): a short TTL, a
//! TCP timestamp far in the past (the server's PAWS check discards it), or an MD5 signature
//! option the server cannot verify. Middleboxes check none of these.
use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};
use tokio::{
    net::{TcpSocket, TcpStream},
    time::{Instant, sleep, timeout},
};

/// How far back a fooled timestamp is moved; the same default as zapret's `ts` fooling.
const TS_BACK: u32 = 600_000;

/// What is needed to forge segments of a connection.
#[derive(Debug, Clone, Copy)]
pub struct Handshake {
    pub local: SocketAddrV4,
    pub remote: SocketAddrV4,
    /// Our next sequence number, which the first data we send will carry.
    pub seq: u32,
    /// The next byte expected from the server.
    pub ack: u32,
    /// Our timestamp from the SYN and the server's from the SYN-ACK, if timestamps are in use.
    pub timestamps: Option<(u32, u32)>,
}

/// How a forged segment is hidden from the server. All off means a plain, valid segment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fooling {
    /// Time to live; 0 keeps the default.
    pub ttl: u8,
    /// Timestamp far in the past, so the server's PAWS check throws the segment away.
    pub ts: bool,
    /// A TCP MD5 signature option that no server has a key for.
    pub md5: bool,
    /// A wrong TCP checksum (zapret `badsum`); servers drop the segment, many routers do too.
    pub badsum: bool,
    /// The ACK flag cleared (zapret `datanoack`); a data segment without it is ignored.
    pub no_ack: bool,
}

struct Fd(libc::c_int);
impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// A packet socket that only receives TCP segments of one local port.
fn sniffer(port: u16) -> io::Result<Fd> {
    // Only a socket for every protocol also sees the packets we send ourselves.
    let protocol = (libc::ETH_P_ALL as u16).to_be();
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            i32::from(protocol),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = Fd(fd);
    let filter = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
    // IPv4 only, not a fragment, TCP, and either port equal to `port`.
    let mut program = [
        filter(0x30, 0, 0, 0),             //  0 ldb [0]
        filter(0x54, 0, 0, 0xf0),          //  1 and #0xf0        IP version
        filter(0x15, 0, 9, 0x40),          //  2 jeq #0x40 else drop (IPv4 only)
        filter(0x30, 0, 0, 9),             //  3 ldb [9]          protocol
        filter(0x15, 0, 7, 6),             //  4 jeq #6 else drop (TCP only)
        filter(0x28, 0, 0, 6),             //  5 ldh [6]          fragment offset
        filter(0x45, 5, 0, 0x1fff),        //  6 jset #0x1fff -> drop
        filter(0xb1, 0, 0, 0),             //  7 ldx 4*([0]&0xf) header length
        filter(0x48, 0, 0, 0),             //  8 ldh [x+0]        source port
        filter(0x15, 3, 0, u32::from(port)), // 9 jeq port -> accept
        filter(0x48, 0, 0, 2),             // 10 ldh [x+2]        destination port
        filter(0x15, 1, 0, u32::from(port)), // 11 jeq port -> accept
        filter(0x06, 0, 0, 0),             // 12 ret #0           drop
        filter(0x06, 0, 0, 65535),         // 13 ret #65535       accept
    ];
    let fprog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr(),
    };
    let result = unsafe {
        libc::setsockopt(
            fd.0,
            libc::SOL_SOCKET,
            libc::SO_ATTACH_FILTER,
            (&raw const fprog).cast(),
            std::mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// The parts of a captured TCP segment that matter here.
#[derive(Debug, PartialEq, Eq)]
struct Segment {
    source: SocketAddrV4,
    flags: u8,
    seq: u32,
    ack: u32,
    /// (TSval, TSecr) from the timestamp option.
    timestamps: Option<(u32, u32)>,
    /// Bytes of TCP payload and the advertised window, for the trace.
    payload: usize,
    window: u16,
}

fn parse_segment(packet: &[u8]) -> Option<Segment> {
    let header = usize::from(packet.first()? & 0x0f) * 4;
    let tcp = packet.get(header..)?;
    let words = usize::from(*tcp.get(12)? >> 4) * 4;
    let options = tcp.get(20..words)?;
    let source = Ipv4Addr::new(*packet.get(12)?, packet[13], packet[14], packet[15]);
    let be32 = |at: usize| u32::from_be_bytes([tcp[at], tcp[at + 1], tcp[at + 2], tcp[at + 3]]);
    Some(Segment {
        source: SocketAddrV4::new(source, u16::from_be_bytes([tcp[0], tcp[1]])),
        flags: tcp[13],
        seq: be32(4),
        ack: be32(8),
        timestamps: timestamp_option(options),
        payload: usize::from(u16::from_be_bytes([*packet.get(2)?, packet[3]])).saturating_sub(header + words),
        window: u16::from_be_bytes([tcp[14], tcp[15]]),
    })
}

/// With `SOUNDIYA_TRACE` set, prints every packet of the connection on stderr with sequence
/// numbers relative to the handshake. It shows whether a stalled download is the server
/// retransmitting (our acknowledgements are lost) or the server going quiet.
fn spawn_trace(packets: Fd, local: SocketAddrV4, isn_local: u32, isn_remote: u32) {
    tokio::spawn(async move {
        // Without this the block would capture only the integer field and the socket would be
        // closed as soon as the function returns.
        let packets = packets;
        let started = Instant::now();
        let mut buf = [0u8; 256];
        while started.elapsed() < Duration::from_secs(45) {
            let n = unsafe { libc::recv(packets.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n <= 0 {
                sleep(Duration::from_millis(2)).await;
                continue;
            }
            let Some(s) = parse_segment(&buf[..n as usize]) else {
                continue;
            };
            let outgoing = s.source == local;
            let (own, peer) = if outgoing { (isn_local, isn_remote) } else { (isn_remote, isn_local) };
            eprintln!(
                "trace {:>6}ms {} flags={:02x} seq={} ack={} len={} win={}",
                started.elapsed().as_millis(),
                if outgoing { ">" } else { "<" },
                s.flags,
                s.seq.wrapping_sub(own),
                s.ack.wrapping_sub(peer),
                s.payload,
                s.window,
            );
        }
    });
}

fn timestamp_option(mut options: &[u8]) -> Option<(u32, u32)> {
    while let Some(&kind) = options.first() {
        let length = match kind {
            0 => return None,
            1 => 1,
            _ => usize::from(*options.get(1)?),
        };
        if length < 2 && kind != 1 {
            return None;
        }
        if kind == 8 && length == 10 {
            let value = options.get(2..10)?;
            return Some((
                u32::from_be_bytes(value[..4].try_into().ok()?),
                u32::from_be_bytes(value[4..].try_into().ok()?),
            ));
        }
        options = options.get(length..)?;
    }
    None
}

/// Opens a connection and records the numbers of its handshake.
pub async fn connect(remote: SocketAddr, limit: Duration) -> io::Result<(TcpStream, Handshake)> {
    let SocketAddr::V4(remote) = remote else {
        return Err(io::Error::other("forged segments need an IPv4 destination"));
    };
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from(([0, 0, 0, 0], 0)))?;
    let port = socket.local_addr()?.port();
    // Listening starts before the SYN is sent, so neither handshake packet can be missed.
    let packets = sniffer(port)?;
    let stream = timeout(limit, socket.connect(SocketAddr::V4(remote))).await??;
    let SocketAddr::V4(local) = stream.local_addr()? else {
        return Err(io::Error::other("unexpected IPv6 local address"));
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut buf = [0u8; 256];
    let (mut syn, mut syn_ack) = (None, None);
    let mut seen = Vec::new();
    loop {
        let n = unsafe { libc::recv(packets.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n > 0 {
            if let Some(segment) = parse_segment(&buf[..n as usize]) {
                seen.push((segment.source, segment.flags));
                if segment.source == remote && segment.flags & 0x12 == 0x12 {
                    syn_ack = Some(segment);
                } else if segment.source == local && segment.flags & 0x12 == 0x02 {
                    syn = Some(segment);
                }
            }
            if let (Some(syn), Some(syn_ack)) = (&syn, &syn_ack) {
                let timestamps = syn.timestamps.zip(syn_ack.timestamps).map(|(ours, theirs)| (ours.0, theirs.0));
                if std::env::var_os("SOUNDIYA_TRACE").is_some() {
                    spawn_trace(packets, local, syn_ack.ack.wrapping_sub(1), syn_ack.seq);
                }
                return Ok((
                    stream,
                    Handshake {
                        local,
                        remote,
                        seq: syn_ack.ack,
                        ack: syn_ack.seq.wrapping_add(1),
                        timestamps,
                    },
                ));
            }
        } else if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "did not see the whole handshake on the wire (local {local}, remote {remote}, saw {seen:?})"
            )));
        } else {
            sleep(Duration::from_millis(2)).await;
        }
    }
}

fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    let mut carry = None;
    for byte in parts.iter().flat_map(|p| p.iter()) {
        match carry.take() {
            Some(high) => sum += u32::from(u16::from_be_bytes([high, *byte])),
            None => carry = Some(*byte),
        }
    }
    if let Some(high) = carry {
        sum += u32::from(u16::from_be_bytes([high, 0]));
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// The TCP options a forged segment carries.
fn options(h: &Handshake, fooling: Fooling) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    if fooling.ts {
        let (ours, theirs) = h
            .timestamps
            .ok_or_else(|| io::Error::other("timestamps are not in use on this connection"))?;
        out.extend_from_slice(&[1, 1, 8, 10]);
        out.extend_from_slice(&ours.wrapping_sub(TS_BACK).to_be_bytes());
        out.extend_from_slice(&theirs.to_be_bytes());
    }
    if fooling.md5 {
        out.extend_from_slice(&[19, 18]);
        out.extend_from_slice(&[0x5a; 16]);
        out.extend_from_slice(&[1, 1]);
    }
    Ok(out)
}

/// An IPv4 + TCP packet carrying `payload` with sequence number `seq`.
fn build_packet(h: &Handshake, seq: u32, payload: &[u8], fooling: Fooling) -> io::Result<Vec<u8>> {
    let options = options(h, fooling)?;
    let tcp_length = 20 + options.len() + payload.len();
    let mut tcp = Vec::with_capacity(tcp_length);
    tcp.extend_from_slice(&h.local.port().to_be_bytes());
    tcp.extend_from_slice(&h.remote.port().to_be_bytes());
    tcp.extend_from_slice(&seq.to_be_bytes());
    tcp.extend_from_slice(&h.ack.to_be_bytes());
    let flags = if fooling.no_ack { 0x08 } else { 0x18 }; // PSH, plus ACK unless fooled
    tcp.extend_from_slice(&[((20 + options.len()) / 4 << 4) as u8, flags]);
    tcp.extend_from_slice(&0xffffu16.to_be_bytes()); // window
    tcp.extend_from_slice(&[0, 0, 0, 0]); // checksum (filled below), urgent pointer
    tcp.extend_from_slice(&options);
    tcp.extend_from_slice(payload);
    let length = (tcp_length as u16).to_be_bytes();
    let sum = checksum(&[&h.local.ip().octets(), &h.remote.ip().octets(), &[0, 6], &length, &tcp]);
    // The complement of the right sum is never the right sum.
    let sum = if fooling.badsum { !sum } else { sum };
    tcp[16..18].copy_from_slice(&sum.to_be_bytes());

    let ttl = if fooling.ttl == 0 { 64 } else { fooling.ttl };
    let mut ip = vec![0x45, 0];
    ip.extend_from_slice(&((20 + tcp_length) as u16).to_be_bytes());
    ip.extend_from_slice(&[0x12, 0x34, 0x40, 0]); // identification, don't fragment
    ip.extend_from_slice(&[ttl, 6, 0, 0]);
    ip.extend_from_slice(&h.local.ip().octets());
    ip.extend_from_slice(&h.remote.ip().octets());
    let sum = checksum(&[&ip]);
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    ip.extend_from_slice(&tcp);
    Ok(ip)
}

/// Sends `payload` as a forged data segment with sequence number `seq`, `repeats` times.
pub fn inject(h: &Handshake, seq: u32, payload: &[u8], fooling: Fooling, repeats: u32) -> io::Result<()> {
    let packet = build_packet(h, seq, payload, fooling)?;
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::IPPROTO_RAW) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = Fd(fd);
    let destination = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(h.remote.ip().octets()) },
        sin_zero: [0; 8],
    };
    for _ in 0..repeats.max(1) {
        let sent = unsafe {
            libc::sendto(
                fd.0,
                packet.as_ptr().cast(),
                packet.len(),
                0,
                (&raw const destination).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if sent as usize != packet.len() {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn handshake() -> Handshake {
        Handshake {
            local: SocketAddrV4::new(Ipv4Addr::new(192, 168, 31, 47), 40000),
            remote: SocketAddrV4::new(Ipv4Addr::new(52, 84, 150, 39), 443),
            seq: 1000,
            ack: 2000,
            timestamps: Some((5_000_000, 9_000)),
        }
    }
    fn plain() -> Fooling {
        Fooling { ttl: 6, ..Fooling::default() }
    }
    #[test]
    fn checksum_matches_the_rfc_example() {
        // RFC 1071: the words 0001 f203 f4f5 f6f7 sum to ddf2, whose complement is 220d.
        assert_eq!(checksum(&[&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]]), 0x220d);
        // An odd number of bytes is padded with zero, whatever the chunking.
        assert_eq!(checksum(&[&[1, 2], &[3]]), checksum(&[&[1, 2, 3]]));
    }
    #[test]
    fn built_packets_verify_and_carry_the_numbers() {
        let h = handshake();
        let packet = build_packet(&h, 1000, b"decoy", plain()).unwrap();
        assert_eq!(packet.len(), 45);
        assert_eq!(packet[8], 6, "ttl");
        // A correct IPv4 header sums to zero, and so does the TCP segment with its pseudo header.
        assert_eq!(checksum(&[&packet[..20]]), 0);
        let length = 25u16.to_be_bytes();
        let sum = checksum(&[&h.local.ip().octets(), &h.remote.ip().octets(), &[0, 6], &length, &packet[20..]]);
        assert_eq!(sum, 0);
        assert_eq!(u32::from_be_bytes(packet[24..28].try_into().unwrap()), 1000);
        assert_eq!(u32::from_be_bytes(packet[28..32].try_into().unwrap()), 2000);
        assert_eq!(&packet[40..], b"decoy");
    }
    #[test]
    fn badsum_breaks_the_checksum_and_datanoack_clears_the_ack_flag() {
        let h = handshake();
        let verifies = |packet: &[u8]| {
            let length = ((packet.len() - 20) as u16).to_be_bytes();
            checksum(&[&h.local.ip().octets(), &h.remote.ip().octets(), &[0, 6], &length, &packet[20..]]) == 0
        };
        let good = build_packet(&h, 1000, b"x", Fooling::default()).unwrap();
        assert!(verifies(&good));
        assert_eq!(good[33], 0x18);
        let bad = build_packet(&h, 1000, b"x", Fooling { badsum: true, ..Fooling::default() }).unwrap();
        assert!(!verifies(&bad));
        let no_ack = build_packet(&h, 1000, b"x", Fooling { no_ack: true, ..Fooling::default() }).unwrap();
        assert_eq!(no_ack[33], 0x08);
        assert!(verifies(&no_ack));
    }
    #[test]
    fn a_sequence_number_before_the_stream_start_wraps() {
        let h = Handshake { seq: 100, ..handshake() };
        let packet = build_packet(&h, h.seq.wrapping_sub(664), b"x", plain()).unwrap();
        assert_eq!(u32::from_be_bytes(packet[24..28].try_into().unwrap()), 100u32.wrapping_sub(664));
    }
    #[test]
    fn timestamp_fooling_moves_the_value_back_and_keeps_the_header_valid() {
        let h = handshake();
        let fooling = Fooling { ts: true, md5: true, ..Fooling::default() };
        let packet = build_packet(&h, 1000, b"fake", fooling).unwrap();
        let tcp = &packet[20..];
        // 20 header + 12 timestamp + 20 md5 option bytes, header length counted in words.
        assert_eq!(tcp[12] >> 4, 13);
        assert_eq!(packet[8], 64, "default ttl");
        let parsed = parse_segment(&packet).unwrap();
        assert_eq!(parsed.timestamps, Some((5_000_000 - TS_BACK, 9_000)));
        let length = (tcp.len() as u16).to_be_bytes();
        assert_eq!(checksum(&[&h.local.ip().octets(), &h.remote.ip().octets(), &[0, 6], &length, tcp]), 0);
        assert!(tcp.windows(2).any(|w| w == [19, 18]), "md5 option present");
        assert_eq!(&packet[packet.len() - 4..], b"fake");
        // Without timestamps on the connection the fooling cannot be applied.
        let no_ts = Handshake { timestamps: None, ..h };
        assert!(build_packet(&no_ts, 1000, b"fake", Fooling { ts: true, ..Fooling::default() }).is_err());
    }
    #[test]
    fn captured_handshake_segments_are_read_with_their_timestamps() {
        let h = handshake();
        // A SYN-ACK from the server: swap the addresses, set SYN+ACK, carry a timestamp.
        let mut packet = build_packet(&h, 7777, b"", Fooling { ts: true, ..Fooling::default() }).unwrap();
        packet[12..16].copy_from_slice(&h.remote.ip().octets());
        packet[16..20].copy_from_slice(&h.local.ip().octets());
        packet[20..22].copy_from_slice(&443u16.to_be_bytes());
        packet[22..24].copy_from_slice(&40000u16.to_be_bytes());
        packet[28..32].copy_from_slice(&1001u32.to_be_bytes());
        packet[33] = 0x12;
        let segment = parse_segment(&packet).unwrap();
        assert_eq!(segment.source, h.remote);
        assert_eq!((segment.seq, segment.ack, segment.flags), (7777, 1001, 0x12));
        assert!(segment.timestamps.is_some());
        assert_eq!(parse_segment(&packet[..30]), None);
        // Odd options: padding, then an unknown option, then the timestamp.
        assert_eq!(timestamp_option(&[1, 1, 3, 3, 7, 8, 10, 0, 0, 0, 1, 0, 0, 0, 2]), Some((1, 2)));
        assert_eq!(timestamp_option(&[2, 4, 5, 180]), None);
    }
}
