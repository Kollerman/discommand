//! Update check: asks GitHub once at startup and then daily whether a newer
//! version has been published: the latest release, or (if there is none)
//! the highest version tag. Needs a public repository; without one (or
//! offline) it finds nothing and says nothing.

use std::time::Duration;

use serde_json::Value;

/// The repository versions are published in; `DISCOMMAND_REPO` points forks
/// (at build time) or a test (at runtime) at another one.
const REPO: &str = match option_env!("DISCOMMAND_REPO") {
    Some(r) => r,
    None => "Kollerman/discommand",
};
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

fn repo() -> String {
    std::env::var("DISCOMMAND_REPO").ok().filter(|r| r.contains('/')).unwrap_or_else(|| REPO.to_string())
}

/// Off with `DISCOMMAND_NO_UPDATE_CHECK=1`.
pub fn enabled() -> bool {
    std::env::var_os("DISCOMMAND_NO_UPDATE_CHECK").is_none_or(|v| v.is_empty() || v == "0")
}

/// Calls `found` with the newest version whenever it's newer than this
/// build. Sleeps in between, so it costs nothing while waiting.
pub async fn watch(found: impl Fn(String)) {
    let mut told = String::new();
    loop {
        if let Ok(Some(tag)) = latest().await {
            if newer(&tag, env!("CARGO_PKG_VERSION")) && tag != told {
                told = tag.clone();
                found(tag);
            }
        }
        tokio::time::sleep(EVERY).await;
    }
}

/// `--check-update`: checks now and says what it found.
pub async fn check_now() -> String {
    let current = env!("CARGO_PKG_VERSION");
    match latest().await {
        Ok(Some(tag)) if newer(&tag, current) => format!("discommand {tag} is out (this is {current}): run git pull, then the install script again"),
        Ok(Some(tag)) => format!("up to date: this is {current}, the newest on {} is {tag}", repo()),
        Ok(None) => format!("{} has no releases or version tags yet (this is {current})", repo()),
        Err(e) => format!("couldn't check {}: {e}", repo()),
    }
}

/// The latest release's tag, else the highest version tag.
async fn latest() -> Result<Option<String>, String> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("discommand/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let get = |path: String| {
        http.get(format!("https://api.github.com/repos/{}/{path}", repo()))
            .header("Accept", "application/vnd.github+json")
            .send()
    };
    let resp = get("releases/latest".into()).await.map_err(|e| e.to_string())?;
    match resp.status().as_u16() {
        200 => {
            let v: Value = resp.json().await.map_err(|e| e.to_string())?;
            return Ok(v["tag_name"].as_str().map(String::from));
        }
        // No release published yet: fall back to tags.
        404 => {}
        code => return Err(format!("GitHub answered {code}")),
    }
    let resp = get("tags?per_page=100".into()).await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub answered {}", resp.status().as_u16()));
    }
    let tags: Vec<Value> = resp.json().await.map_err(|e| e.to_string())?;
    Ok(highest(tags.iter().filter_map(|t| t["name"].as_str())))
}

/// The highest version among tags ("v0.1.1"); other tags are ignored.
fn highest<'a>(tags: impl Iterator<Item = &'a str>) -> Option<String> {
    tags.filter(|t| !version(t).is_empty()).max_by(|a, b| version(a).cmp(&version(b))).map(String::from)
}

fn version(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches(['v', 'V']).split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
}

/// Whether version `a` ("v0.2.0") is newer than `b` ("0.1.0").
fn newer(a: &str, b: &str) -> bool {
    version(a) > version(b)
}

#[cfg(test)]
mod tests {
    use super::{highest, newer};

    #[test]
    fn compares_versions() {
        assert!(newer("v0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("v1.0", "0.9.9"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("v0.1.0", "0.2.0"));
        assert!(!newer("nightly", "0.1.0"));
    }

    #[test]
    fn picks_the_highest_tag() {
        let tags = ["v0.1.1", "nightly", "v0.10.0", "v0.9.2"];
        assert_eq!(highest(tags.into_iter()).as_deref(), Some("v0.10.0"));
        assert_eq!(highest(["latest"].into_iter()), None);
    }
}
