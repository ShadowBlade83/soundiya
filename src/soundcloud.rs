//! A minimal SoundCloud web-API client: anonymous by default, signed in with the OAuth
//! token of a browser session for likes and the personal library.
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};

const API: &str = "https://api-v2.soundcloud.com";
const HOME: &str = "https://soundcloud.com/";
const ASSETS: &str = "https://a-v2.sndcdn.com/assets/";
const AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

pub type Res<T> = Result<T, String>;

#[derive(Clone, Debug)]
struct Transcoding {
    url: String,
    protocol: String,
    mime: String,
    preset: String,
}
impl Transcoding {
    /// Plain MP3, not the `audio/mpegurl` of an adaptive-bitrate playlist.
    fn is_mp3(&self) -> bool {
        self.mime == "audio/mpeg" || self.mime.starts_with("audio/mpeg;")
    }
    /// Higher is better; `None` for formats this player cannot decode.
    fn rank(&self) -> Option<(u32, Codec)> {
        let hls = self.protocol == "hls";
        if hls && self.mime.starts_with("audio/mp4") && self.preset.starts_with("aac_") {
            // SoundCloud's current format: fragmented MP4 with AAC, 96k or 160k.
            let kbps = self.preset[4..].trim_end_matches('k').parse::<u32>().unwrap_or(0);
            Some((1000 + kbps, Codec::Aac))
        } else if self.is_mp3() && self.protocol == "progressive" {
            Some((500, Codec::Mp3))
        } else if self.is_mp3() && hls {
            Some((400, Codec::Mp3))
        } else {
            None
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Mp3,
    Aac,
}
#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub duration_ms: u64,
    authorization: String,
    transcodings: Vec<Transcoding>,
}
impl Track {
    pub fn label(&self) -> String {
        format!("{} - {}", self.artist, self.title)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub url: String,
    /// An HLS playlist of segments rather than one continuous file.
    pub hls: bool,
    pub codec: Codec,
}

pub struct Client {
    api: reqwest::Client,
    media: reqwest::Client,
    state_dir: PathBuf,
    client_id: tokio::sync::Mutex<Option<String>>,
    oauth: Mutex<Option<String>>,
    user_id: Mutex<Option<u64>>,
}

impl Client {
    /// `proxy` is the loopback bypass proxy; every request goes through it.
    pub fn new(state_dir: PathBuf, proxy: Option<&str>) -> Res<Self> {
        let build = |timeout: Option<Duration>| -> Res<reqwest::Client> {
            let mut builder = reqwest::Client::builder()
                .user_agent(AGENT)
                .connect_timeout(Duration::from_secs(20))
                // A middlebox that disguised handshakes can still cut a flow once the first
                // answer has gone through, so a finished connection is never reused: the next
                // request opens a new one and goes through the bypass again.
                .pool_max_idle_per_host(0);
            if let Some(timeout) = timeout {
                builder = builder.timeout(timeout);
            }
            builder = match proxy {
                Some(url) => builder.proxy(reqwest::Proxy::all(url).map_err(|e| e.to_string())?),
                None => builder.no_proxy(),
            };
            builder.build().map_err(|e| e.to_string())
        };
        let client_id = std::fs::read_to_string(state_dir.join("client-id"))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| valid_client_id(s));
        let oauth = std::fs::read_to_string(state_dir.join("oauth-token"))
            .ok()
            .and_then(|s| clean_token(&s));
        Ok(Self {
            // The whole-request timeout would cut long audio bodies, so streams get none.
            api: build(Some(Duration::from_secs(25)))?,
            media: build(None)?,
            state_dir,
            client_id: tokio::sync::Mutex::new(client_id),
            oauth: Mutex::new(oauth),
            user_id: Mutex::new(None),
        })
    }

    pub fn media_http(&self) -> reqwest::Client {
        self.media.clone()
    }
    pub fn logged_in(&self) -> bool {
        self.oauth.lock().unwrap().is_some()
    }

    pub fn set_token(&self, token: Option<String>) -> Res<()> {
        let path = self.state_dir.join("oauth-token");
        match &token {
            Some(token) => crate::config::write_private(&path, token).map_err(|e| e.to_string())?,
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        *self.oauth.lock().unwrap() = token;
        *self.user_id.lock().unwrap() = None;
        Ok(())
    }

    pub async fn client_id(&self, refresh: bool) -> Res<String> {
        let mut cached = self.client_id.lock().await;
        if !refresh && let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        let id = self.scrape_client_id().await?;
        let _ = crate::config::write_private(&self.state_dir.join("client-id"), &id);
        *cached = Some(id.clone());
        Ok(id)
    }

    /// The web player embeds a public client id in its JavaScript bundles.
    async fn scrape_client_id(&self) -> Res<String> {
        let page = self.text(HOME).await?;
        let mut scripts: Vec<&str> = page
            .split("<script")
            .filter_map(|part| {
                let rest = &part[part.find("src=\"")? + 5..];
                Some(&rest[..rest.find('"')?])
            })
            .filter(|src| src.starts_with(ASSETS) && src.ends_with(".js"))
            .collect();
        // The id lives in one of the last bundles.
        scripts.reverse();
        let mut failure = String::new();
        for src in scripts {
            match self.text(src).await {
                Ok(js) => {
                    if let Some(id) = find_client_id(&js) {
                        return Ok(id);
                    }
                }
                Err(e) => failure = e,
            }
        }
        Err(if failure.is_empty() {
            "could not find the SoundCloud client id".into()
        } else {
            format!("could not find the SoundCloud client id ({failure})")
        })
    }

    /// GET a page, retrying once; the first attempt through a fresh tunnel sometimes fails.
    async fn text(&self, url: &str) -> Res<String> {
        let mut last = String::new();
        for _ in 0..2 {
            let response = match self.api.get(url).send().await {
                Ok(response) => response,
                Err(e) => {
                    last = describe(e);
                    continue;
                }
            };
            if !response.status().is_success() {
                last = format!("{url}: HTTP {}", response.status());
                continue;
            }
            match response.text().await {
                Ok(text) => return Ok(text),
                Err(e) => last = e.to_string(),
            }
        }
        Err(last)
    }

    async fn call(
        &self,
        method: reqwest::Method,
        url: &str,
        params: &[(&str, &str)],
    ) -> Res<Value> {
        let token = self.oauth.lock().unwrap().clone();
        let mut last = String::new();
        for attempt in 0..2 {
            let id = self.client_id(attempt > 0 && last.contains("HTTP 401")).await?;
            let mut request = self
                .api
                .request(method.clone(), url)
                .query(&[("client_id", id.as_str())])
                .query(params);
            if let Some(token) = &token {
                request = request.header("Authorization", format!("OAuth {token}"));
            }
            let response = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    last = describe(e);
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                let text = response.text().await.map_err(describe)?;
                return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
            }
            last = format!("HTTP {status}");
            // A stale client id answers 401 or 403; fetch a fresh one and try again.
            if matches!(status.as_u16(), 401 | 403) {
                last = "HTTP 401".into();
            } else if status.as_u16() != 429 && !status.is_server_error() {
                break;
            }
        }
        Err(format!("{url}: {last}"))
    }

    pub async fn search(&self, query: &str, limit: usize) -> Res<Vec<Track>> {
        let limit = limit.to_string();
        let value = self
            .call(
                reqwest::Method::GET,
                &format!("{API}/search/tracks"),
                &[("q", query), ("limit", &limit), ("offset", "0")],
            )
            .await?;
        Ok(value["collection"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_track)
            .collect())
    }

    async fn me(&self) -> Res<u64> {
        if let Some(id) = *self.user_id.lock().unwrap() {
            return Ok(id);
        }
        if !self.logged_in() {
            return Err("not signed in".into());
        }
        let value = self
            .call(reqwest::Method::GET, &format!("{API}/me"), &[])
            .await?;
        let id = value["id"].as_u64().ok_or("SoundCloud rejected the token")?;
        *self.user_id.lock().unwrap() = Some(id);
        Ok(id)
    }

    /// Name of the signed-in account, which also proves that the token works.
    pub async fn username(&self) -> Res<String> {
        let value = self
            .call(reqwest::Method::GET, &format!("{API}/me"), &[])
            .await?;
        let name = value["username"].as_str().ok_or("SoundCloud rejected the token")?;
        Ok(name.to_owned())
    }

    pub async fn liked_tracks(&self, limit: usize) -> Res<Vec<Track>> {
        let user = self.me().await?;
        let limit = limit.to_string();
        let value = self
            .call(
                reqwest::Method::GET,
                &format!("{API}/users/{user}/track_likes"),
                &[("limit", &limit), ("offset", "0")],
            )
            .await?;
        let items: Vec<&Value> = value["collection"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| &item["track"])
            .collect();
        self.complete(&items).await
    }

    /// Turns API items into tracks. Older likes and most playlist entries come back as bare
    /// ids, which are fetched in batches; the order of `items` is kept.
    async fn complete(&self, items: &[&Value]) -> Res<Vec<Track>> {
        let mut tracks: Vec<Option<Track>> = items.iter().map(|v| parse_track(v)).collect();
        let missing: Vec<String> = items
            .iter()
            .zip(&tracks)
            .filter(|(_, parsed)| parsed.is_none())
            .filter_map(|(v, _)| v["id"].as_u64())
            .map(|id| id.to_string())
            .collect();
        for batch in missing.chunks(50) {
            let ids = batch.join(",");
            let full = self
                .call(reqwest::Method::GET, &format!("{API}/tracks"), &[("ids", &ids)])
                .await?;
            let full: Vec<Track> = full.as_array().into_iter().flatten().filter_map(parse_track).collect();
            for (slot, item) in tracks.iter_mut().zip(items) {
                if slot.is_none() {
                    *slot = full.iter().find(|t| Some(t.id) == item["id"].as_u64()).cloned();
                }
            }
        }
        Ok(tracks.into_iter().flatten().collect())
    }

    /// The tracks of one playlist or album, at most `limit` of them.
    async fn playlist_tracks(&self, id: u64, limit: usize) -> Res<Vec<Track>> {
        let value = self.call(reqwest::Method::GET, &format!("{API}/playlists/{id}"), &[]).await?;
        let items: Vec<&Value> = value["tracks"].as_array().into_iter().flatten().take(limit).collect();
        self.complete(&items).await
    }

    /// A playlist or album that fits `query`: the user's own and liked ones first, then a
    /// public search. Returns its title and tracks.
    pub async fn playlist(&self, query: &str, limit: usize) -> Res<Option<(String, Vec<Track>)>> {
        let mut candidates: Vec<(u64, String)> = Vec::new();
        let own_list = if self.logged_in()
            && let Ok(user) = self.me().await
        {
            let list = self
                .call(
                    reqwest::Method::GET,
                    &format!("{API}/users/{user}/playlists/liked_and_owned"),
                    &[("limit", "50")],
                )
                .await;
            if let Err(e) = &list {
                eprintln!("playlists: your own list is unavailable ({e})");
            }
            list.ok()
        } else {
            None
        };
        if let Some(value) = own_list {
            let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
            let mut own: Vec<(usize, u64, String)> = value["collection"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| {
                    let playlist = if item["playlist"].is_object() { &item["playlist"] } else { item };
                    let title = playlist["title"].as_str()?.to_owned();
                    let lower = title.to_lowercase();
                    let hits = words.iter().filter(|w| lower.contains(w.as_str())).count();
                    (hits == words.len() && !words.is_empty()).then(|| (hits, playlist["id"].as_u64().unwrap_or(0), title))
                })
                .filter(|(_, id, _)| *id != 0)
                .collect();
            eprintln!(
                "playlists: {} of yours match '{query}' among {}",
                own.len(),
                value["collection"].as_array().map_or(0, Vec::len)
            );
            own.sort_by_key(|(hits, ..)| std::cmp::Reverse(*hits));
            candidates.extend(own.into_iter().map(|(_, id, title)| (id, title)));
        }
        let found = self
            .call(reqwest::Method::GET, &format!("{API}/search/playlists"), &[("q", query), ("limit", "6")])
            .await?;
        candidates.extend(
            found["collection"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["track_count"].as_u64().unwrap_or(1) > 0)
                .filter_map(|p| Some((p["id"].as_u64()?, p["title"].as_str().unwrap_or(query).to_owned()))),
        );
        for (id, title) in candidates.into_iter().take(4) {
            match self.playlist_tracks(id, limit).await {
                Ok(tracks) if !tracks.is_empty() => return Ok(Some((title, tracks))),
                _ => {}
            }
        }
        Ok(None)
    }

    /// What a soundcloud.com link points to: a track, a playlist or album (also private ones
    /// with a secret link), or an artist (their latest tracks). Returns a title and the tracks.
    pub async fn link(&self, url: &str, limit: usize) -> Res<(String, Vec<Track>)> {
        let value = self.call(reqwest::Method::GET, &format!("{API}/resolve"), &[("url", url)]).await?;
        let title = value["title"].as_str().or(value["username"].as_str()).unwrap_or("SoundCloud").to_owned();
        let tracks = match value["kind"].as_str() {
            Some("track") => self.complete(&[&value]).await?,
            Some("playlist") | Some("system-playlist") => {
                let items: Vec<&Value> = value["tracks"].as_array().into_iter().flatten().take(limit).collect();
                self.complete(&items).await?
            }
            Some("user") => {
                let id = value["id"].as_u64().ok_or("the link has no user id")?;
                let limit = limit.to_string();
                let found = self
                    .call(reqwest::Method::GET, &format!("{API}/users/{id}/tracks"), &[("limit", &limit)])
                    .await?;
                let items: Vec<&Value> = found["collection"].as_array().into_iter().flatten().collect();
                self.complete(&items).await?
            }
            other => return Err(format!("this link is not a track, playlist or artist ({})", other.unwrap_or("unknown"))),
        };
        Ok((title, tracks))
    }

    /// Tracks like `track`, as SoundCloud's own "related" list suggests them.
    pub async fn related(&self, track: u64, limit: usize) -> Res<Vec<Track>> {
        let limit = limit.to_string();
        let value = self
            .call(reqwest::Method::GET, &format!("{API}/tracks/{track}/related"), &[("limit", &limit)])
            .await?;
        let items: Vec<&Value> = value["collection"].as_array().into_iter().flatten().collect();
        self.complete(&items).await
    }

    /// What is popular now: SoundCloud's trending chart, or for one genre the most played of
    /// its tracks (`trending` narrows those to the last week). `all-music` means no genre.
    /// The genre charts and the "top" chart answer 404 nowadays, so genres go through search.
    pub async fn chart(&self, trending: bool, genre: &str, limit: usize) -> Res<Vec<Track>> {
        let limit = limit.to_string();
        if genre == "all-music" {
            let value = self
                .call(
                    reqwest::Method::GET,
                    &format!("{API}/charts"),
                    &[("kind", "trending"), ("genre", "soundcloud:genres:all-music"), ("limit", &limit)],
                )
                .await?;
            let items: Vec<&Value> = value["collection"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| if item["track"].is_object() { &item["track"] } else { item })
                .collect();
            return self.complete(&items).await;
        }
        let tag = match genre {
            "jazzblues" => "jazz",
            "danceedm" => "edm",
            other => other,
        };
        let mut query = vec![("q", tag), ("filter.genre_or_tag", tag), ("limit", limit.as_str())];
        query.push(if trending { ("filter.created_at", "last_week") } else { ("sort", "popular") });
        let value = self.call(reqwest::Method::GET, &format!("{API}/search/tracks"), &query).await?;
        let items: Vec<&Value> = value["collection"].as_array().into_iter().flatten().collect();
        self.complete(&items).await
    }

    /// Asks SoundCloud for a playable URL in the best format this player can decode.
    pub async fn stream(&self, track: &Track) -> Res<Stream> {
        let mut choices: Vec<(u32, Codec, &Transcoding)> = track
            .transcodings
            .iter()
            .filter_map(|t| t.rank().map(|(rank, codec)| (rank, codec, t)))
            .collect();
        choices.sort_by_key(|(rank, ..)| std::cmp::Reverse(*rank));
        if choices.is_empty() {
            return Err("no playable stream format for this track".into());
        }
        let mut last = String::new();
        for (_, codec, choice) in choices {
            match self
                .call(
                    reqwest::Method::GET,
                    &choice.url,
                    &[("track_authorization", &track.authorization)],
                )
                .await
            {
                Ok(value) => {
                    if let Some(url) = value["url"].as_str() {
                        return Ok(Stream {
                            url: url.to_owned(),
                            hls: choice.protocol == "hls",
                            codec,
                        });
                    }
                }
                Err(e) => last = e,
            }
        }
        Err(if last.is_empty() { "no stream URL".into() } else { last })
    }
}

/// An error with every cause behind it (`error sending request: client error (Connect): ...`),
/// without the address, which for API calls is a long URL carrying a token.
fn describe(error: reqwest::Error) -> String {
    use std::error::Error;
    let mut causes = Vec::new();
    let mut source = error.source();
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    let mut parts = vec![error.without_url().to_string()];
    parts.extend(causes);
    parts.join(": ")
}

fn parse_track(v: &Value) -> Option<Track> {
    let id = v["id"].as_u64()?;
    if v["policy"] == "BLOCK" {
        return None;
    }
    let transcodings: Vec<Transcoding> = v["media"]["transcodings"]
        .as_array()?
        .iter()
        .filter_map(|t| {
            Some(Transcoding {
                url: t["url"].as_str()?.to_owned(),
                protocol: t["format"]["protocol"].as_str()?.to_owned(),
                mime: t["format"]["mime_type"].as_str()?.to_owned(),
                preset: t["preset"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect();
    if transcodings.is_empty() {
        return None;
    }
    Some(Track {
        id,
        title: v["title"].as_str().unwrap_or("Unknown").to_owned(),
        artist: v["user"]["username"].as_str().unwrap_or("Unknown").to_owned(),
        duration_ms: v["full_duration"]
            .as_u64()
            .or_else(|| v["duration"].as_u64())
            .unwrap_or(0),
        authorization: v["track_authorization"].as_str().unwrap_or_default().to_owned(),
        transcodings,
    })
}

fn valid_client_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_alphanumeric())
}
fn find_client_id(js: &str) -> Option<String> {
    for pattern in ["client_id:\"", "client_id=\"", "\"client_id\":\""] {
        let mut from = 0;
        while let Some(at) = js[from..].find(pattern) {
            let start = from + at + pattern.len();
            let id: String = js[start..].chars().take(32).collect();
            if valid_client_id(&id) {
                return Some(id);
            }
            from = start;
        }
    }
    None
}

/// Extracts an OAuth token from whatever the user pasted: the bare token, an
/// `Authorization: OAuth ...` header, a cURL command or an `oauth_token=` cookie.
pub fn clean_token(input: &str) -> Option<String> {
    let flat: String = input
        .chars()
        .map(|c| if matches!(c, ';' | '=' | '"' | '\'' | ',' | ':' | '\\' | '^') { ' ' } else { c })
        .collect();
    let looks_like_token = |piece: &&str| {
        let parts: Vec<&str> = piece.split('-').collect();
        parts.len() >= 4 && parts[..3].iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    // Newer web sessions carry a JWT (`eyJ...` header.payload.signature) instead.
    let looks_like_jwt = |piece: &&str| {
        let parts: Vec<&str> = piece.split('.').collect();
        piece.starts_with("eyJ")
            && piece.len() >= 40
            && parts.len() == 3
            && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
    };
    flat.split_whitespace()
        .find(looks_like_token)
        .or_else(|| flat.split_whitespace().find(looks_like_jwt))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_client_id_in_bundles() {
        let id = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6";
        let js = format!("var x={{api:\"/\"}};...,client_id:\"{id}\",env:\"prod\"");
        assert_eq!(find_client_id(&js).as_deref(), Some(id));
        assert_eq!(find_client_id("client_id:\"short\""), None);
    }
    #[test]
    fn extracts_tokens_from_pasted_text() {
        let token = "2-293001-123456789-AbCdEfGhIjKlMn";
        for text in [
            token.to_owned(),
            format!("OAuth {token}"),
            format!("authorization: OAuth {token}"),
            format!("-H ^\"authorization: OAuth {token}^\" ^"),
            format!("sc_anonymous_id=1; oauth_token={token}; foo=bar"),
        ] {
            assert_eq!(clean_token(&text).as_deref(), Some(token), "{text}");
        }
        assert_eq!(clean_token("hello world"), None);
    }
    #[test]
    fn extracts_jwt_tokens() {
        let jwt = "eyJraWQiOiJzYy16ZyIsImFsZyI6IlJTMjU2In0.eyJzdWIiOiJ1c2VyIn0.c2lnbmF0dXJlX3BhcnQ_-x";
        for text in [jwt.to_owned(), format!("authorization: OAuth {jwt}"), format!("a=1; oauth_token={jwt}; b=2")] {
            assert_eq!(clean_token(&text).as_deref(), Some(jwt), "{text}");
        }
        assert_eq!(clean_token("eyJhbGciOi.short"), None);
        // The classic format still wins when both are present.
        let classic = "2-293001-123456789-AbCdEfGhIjKlMn";
        assert_eq!(clean_token(&format!("{jwt} {classic}")).as_deref(), Some(classic));
    }
    #[test]
    fn parses_tracks_with_mp3_transcodings() {
        let value: Value = serde_json::from_str(
            r#"{"id":7,"title":"Song","user":{"username":"Band"},"duration":1000,
                "track_authorization":"abc","media":{"transcodings":[
                {"url":"https://x/p","preset":"mp3_1_0","format":{"protocol":"progressive","mime_type":"audio/mpeg"}},
                {"url":"https://x/a96","preset":"aac_96k","format":{"protocol":"hls","mime_type":"audio/mp4; codecs=\"mp4a.40.2\""}},
                {"url":"https://x/a160","preset":"aac_160k","format":{"protocol":"hls","mime_type":"audio/mp4; codecs=\"mp4a.40.2\""}},
                {"url":"https://x/abr","preset":"abr_sq","format":{"protocol":"hls","mime_type":"audio/mpegurl"}},
                {"url":"https://x/h","format":{"protocol":"hls","mime_type":"audio/ogg; codecs=\"opus\""}}]}}"#,
        )
        .unwrap();
        let track = parse_track(&value).unwrap();
        assert_eq!(track.label(), "Band - Song");
        assert_eq!(track.transcodings.len(), 5);
        // AAC 160k beats AAC 96k beats MP3; adaptive and Opus streams cannot be played.
        let mut ranked: Vec<_> = track
            .transcodings
            .iter()
            .filter_map(|t| t.rank().map(|(r, c)| (r, c, t.url.as_str())))
            .collect();
        ranked.sort_by_key(|(rank, ..)| std::cmp::Reverse(*rank));
        let urls: Vec<_> = ranked.iter().map(|(_, _, u)| *u).collect();
        assert_eq!(urls, ["https://x/a160", "https://x/a96", "https://x/p"]);
        assert_eq!(ranked[0].1, Codec::Aac);
        assert!(parse_track(&serde_json::json!({"id": 1})).is_none());
        assert!(parse_track(&serde_json::json!({"id":1,"policy":"BLOCK","media":{"transcodings":[]}})).is_none());
    }
}
