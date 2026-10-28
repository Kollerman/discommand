//! Update check: asks GitHub once at startup and then daily whether a newer
//! release has been published. Needs a public repository; without one (or
//! offline) it finds nothing and says nothing.

use std::time::Duration;

use serde_json::Value;

/// The repository releases are published in; `DISCOMMAND_REPO` at build
/// time points forks at their own.
const REPO: &str = match option_env!("DISCOMMAND_REPO") {
    Some(r) => r,
    None => "Kollerman/discommand",
};
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Off with `DISCOMMAND_NO_UPDATE_CHECK=1`.
pub fn enabled() -> bool {
    std::env::var_os("DISCOMMAND_NO_UPDATE_CHECK").is_none_or(|v| v.is_empty() || v == "0")
}

/// Calls `found` with the newest release's tag whenever it's newer than
/// this build. Sleeps in between, so it costs nothing while waiting.
pub async fn watch(found: impl Fn(String)) {
    let mut told = String::new();
    loop {
        if let Some(tag) = latest().await {
            if newer(&tag, env!("CARGO_PKG_VERSION")) && tag != told {
                told = tag.clone();
                found(tag);
            }
        }
        tokio::time::sleep(EVERY).await;
    }
}

async fn latest() -> Option<String> {
    let v: Value = reqwest::Client::builder()
        .user_agent(concat!("discommand/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    v["tag_name"].as_str().map(String::from)
}

/// Whether version `a` ("v0.2.0") is newer than `b` ("0.1.0").
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim().trim_start_matches(['v', 'V']).split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
    };
    parts(a) > parts(b)
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn compares_versions() {
        assert!(newer("v0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("v1.0", "0.9.9"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("v0.1.0", "0.2.0"));
        assert!(!newer("nightly", "0.1.0"));
    }
}
