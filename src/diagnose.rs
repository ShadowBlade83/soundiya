//! Automatic diagnostics: tries every strategy, one at a time, and says which of them gets
//! SoundCloud through on this network.
//!
//! The strategies are tried strictly one after another, each on its own loopback proxy and with
//! fresh connections: several forged handshakes at once disturb each other and would make the
//! result meaningless. A strategy passes when the SoundCloud home page and the first script it
//! links to download completely, which takes far more than the few kilobytes after which some
//! middleboxes cut a flow, and when a request with 3 KB of headers is still answered, which
//! catches strategies that only damage connections that upload more than a little.
use crate::{
    bypass::{self, Strategy},
    config::Bypass,
};
use std::time::{Duration, Instant};

const HOME: &str = "https://soundcloud.com/";
const ASSETS: &str = "https://a-v2.sndcdn.com/assets/";
/// The longest one strategy may take before it counts as failed.
const BUDGET: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct Outcome {
    pub strategy: Strategy,
    pub passed: bool,
    pub elapsed: Duration,
    /// Why it failed, or how it passed.
    pub detail: String,
}

/// How the run is shaped.
#[derive(Clone, Default)]
pub struct Options {
    /// Stop at the first strategy that passes.
    pub first_only: bool,
    /// Extra settings carried over from the configuration (domains, upstream proxy, DoH).
    pub base: Bypass,
}

/// Tries each strategy and reports through `progress` as it goes.
pub async fn run(options: &Options, mut progress: impl FnMut(&Outcome)) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for strategy in Strategy::all() {
        let started = Instant::now();
        let result = tokio::time::timeout(BUDGET, probe(options, strategy)).await;
        let (passed, detail) = match result {
            Ok(Ok(bytes)) => (true, format!("{} KB downloaded", bytes / 1024)),
            Ok(Err((_, why))) => (false, why),
            Err(_) => (false, "no result within the time budget".into()),
        };
        let outcome = Outcome { strategy, passed, elapsed: started.elapsed(), detail };
        progress(&outcome);
        let done = outcome.passed && options.first_only;
        outcomes.push(outcome);
        if done {
            break;
        }
    }
    outcomes
}

/// The fastest strategy among those that passed.
pub fn best(outcomes: &[Outcome]) -> Option<&Outcome> {
    outcomes.iter().filter(|o| o.passed).min_by_key(|o| o.elapsed)
}

/// Downloads the home page and its first script through a proxy that uses only `strategy`.
/// Errors carry the bytes received before the failure.
async fn probe(options: &Options, strategy: Strategy) -> Result<usize, (usize, String)> {
    let cfg = Bypass { strategy: strategy.name(), ..options.base.clone() };
    let (address, _handle) = bypass::start(cfg, None).await.map_err(|e| (0, e.to_string()))?;
    let proxy = reqwest::Proxy::all(format!("http://{address}")).map_err(|e| (0, e.to_string()))?;
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .connect_timeout(Duration::from_secs(10))
        .proxy(proxy)
        .build()
        .map_err(|e| (0, e.to_string()))?;
    let page = download(&http, HOME).await?;
    let text = String::from_utf8_lossy(&page);
    let script = text
        .split("<script")
        .filter_map(|part| {
            let rest = &part[part.find("src=\"")? + 5..];
            Some(&rest[..rest.find('"')?])
        })
        .find(|src| src.starts_with(ASSETS) && src.ends_with(".js"))
        .ok_or((page.len(), "the home page lists no script".to_owned()))?
        .to_owned();
    let script = download(&http, &script).await.map_err(|(bytes, why)| (page.len() + bytes, why))?;
    let total = page.len() + script.len();
    // Real use sends far more than a bare GET: signed media URLs make requests of 1 KB and up.
    // A strategy that damages a connection once the client has uploaded about a kilobyte passes
    // every plain download and fails here. Any HTTP answer counts, even an error status.
    tokio::time::timeout(
        Duration::from_secs(12),
        http.get(HOME).header("x-padding", "a".repeat(3000)).send(),
    )
    .await
    .map_err(|_| (total, "a request with 3 KB of headers got no answer".to_owned()))?
    .map_err(|e| (total, format!("a request with 3 KB of headers failed ({})", e.without_url())))?;
    Ok(total)
}

/// A whole download, in pieces so that a stall is noticed quickly.
async fn download(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, (usize, String)> {
    let mut response = tokio::time::timeout(Duration::from_secs(12), http.get(url).send())
        .await
        .map_err(|_| (0, "no answer".to_owned()))?
        .map_err(|e| (0, e.to_string()))?;
    if !response.status().is_success() {
        return Err((0, format!("HTTP {}", response.status())));
    }
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(8), response.chunk()).await {
            Ok(Ok(Some(chunk))) => body.extend_from_slice(&chunk),
            Ok(Ok(None)) => return Ok(body),
            Ok(Err(e)) => return Err((body.len(), format!("cut off after {} bytes ({e})", body.len()))),
            Err(_) => return Err((body.len(), format!("stalled after {} bytes", body.len()))),
        }
    }
}

/// One line of the report: verdict, strategy, time, and why.
pub fn row(outcome: &Outcome) -> String {
    format!(
        "{:<4}{:<22}{:>6.1} s   {}",
        if outcome.passed { "ok" } else { "-" },
        outcome.strategy.name(),
        outcome.elapsed.as_secs_f64(),
        outcome.detail
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn outcome(name: &str, passed: bool, millis: u64) -> Outcome {
        Outcome {
            strategy: Strategy::parse(name).unwrap(),
            passed,
            elapsed: Duration::from_millis(millis),
            detail: String::new(),
        }
    }
    #[test]
    fn the_fastest_passing_strategy_wins() {
        let outcomes = [outcome("split", false, 100), outcome("alt9", true, 900), outcome("alt11", true, 400)];
        assert_eq!(best(&outcomes).map(|o| o.strategy.name()).as_deref(), Some("alt11"));
        assert!(best(&[outcome("split", false, 1)]).is_none());
        assert!(best(&[]).is_none());
    }
    #[test]
    fn a_row_shows_the_verdict_the_strategy_and_the_reason() {
        let mut failed = outcome("split", false, 1500);
        failed.detail = "stalled after 7505 bytes".into();
        let line = row(&failed);
        assert!(line.starts_with('-') && line.contains("split") && line.contains("1.5 s"));
        assert!(line.contains("stalled after 7505 bytes"));
        let line = row(&outcome("alt11", true, 400));
        assert!(line.starts_with("ok") && line.contains("alt11"));
    }
}
