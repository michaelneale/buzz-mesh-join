//! The node's own Buzz identity, and joining a community with an invite link.
//!
//! A headless box should not hold anyone's personal nsec. Instead it makes its
//! own key once, kept in a local state file, and becomes a community member
//! the same way a person does: by redeeming an invite link
//! (`POST /api/invites/claim`, NIP-98 signed by the joining key), or by an
//! admin adding its npub directly.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use base64::Engine;
use nostr_sdk::hashes::Hash as _;
use nostr_sdk::prelude::*;
use serde_json::{json, Value};

/// What the node remembers between runs.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// The node's own Nostr secret key, hex.
    pub secret: Option<String>,
    /// The community relay joined with `join`.
    pub relay: Option<String>,
}

pub fn default_state_path() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("no HOME or XDG_CONFIG_HOME")?;
    Ok(base.join("buzz-mesh-join").join("node.json"))
}

pub fn load(path: &Path) -> anyhow::Result<State> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save(path: &Path, state: &State) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The node's key: `--nsec` if given, else the state file's, created on first use.
pub fn keys(nsec: Option<&str>, path: &Path, state: &mut State) -> anyhow::Result<Keys> {
    if let Some(nsec) = nsec {
        return Keys::parse(nsec.trim()).context("parsing --nsec");
    }
    if let Some(secret) = &state.secret {
        return Keys::parse(secret).with_context(|| format!("key in {}", path.display()));
    }
    let keys = Keys::generate();
    state.secret = Some(keys.secret_key().to_secret_hex());
    save(path, state)?;
    eprintln!("created this node's Buzz key in {}", path.display());
    Ok(keys)
}

/// An invite link: `https://<host>/invite/<code>` or `buzz://join?relay=<ws>&code=<code>`.
pub struct Invite {
    pub relay: String,
    pub http: String,
    pub code: String,
}

pub fn parse_invite(link: &str) -> anyhow::Result<Invite> {
    let url = Url::parse(link.trim()).context("invite must be a full link")?;
    let (relay, code) = match url.scheme() {
        "buzz" => {
            let q = |k: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.into_owned())
            };
            (
                q("relay").context("buzz:// link has no relay")?,
                q("code").context("buzz:// link has no code")?,
            )
        }
        scheme @ ("https" | "http") => {
            let host = url.host_str().context("invite link has no host")?;
            let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
            let segs: Vec<&str> = url
                .path_segments()
                .map(|s| s.filter(|s| !s.is_empty()).collect())
                .unwrap_or_default();
            let ["invite", code] = segs.as_slice() else {
                bail!("expected a link like https://<community>/invite/<code>");
            };
            let ws = if scheme == "https" { "wss" } else { "ws" };
            (format!("{ws}://{host}{port}"), code.to_string())
        }
        other => bail!("unsupported invite link scheme {other}"),
    };
    let http = relay
        .trim_end_matches('/')
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    Ok(Invite { relay, http, code })
}

fn nip98(keys: &Keys, url: &str, body: &[u8]) -> anyhow::Result<String> {
    let data = HttpData::new(Url::parse(url)?, HttpMethod::POST)
        .payload(nostr_sdk::hashes::sha256::Hash::hash(body));
    let event = EventBuilder::http_auth(data).sign_with_keys(keys)?;
    Ok(format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(event.as_json())
    ))
}

async fn post(http: &reqwest::Client, keys: Option<&Keys>, url: &str, body: Value) -> anyhow::Result<Value> {
    let body = serde_json::to_vec(&body)?;
    let mut req = http
        .post(url)
        .header("Content-Type", "application/json")
        .body(body.clone());
    if let Some(keys) = keys {
        req = req.header("Authorization", nip98(keys, url, &body)?);
    }
    let resp = req.send().await.with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let value: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = value.get("error").and_then(Value::as_str).unwrap_or("");
        bail!("{url}: {status} {msg}");
    }
    Ok(value)
}

/// Redeem `invite` as `keys`. Returns the relay's claim status.
pub async fn claim(
    keys: &Keys,
    invite: &Invite,
    accept_terms: bool,
    age_confirmed: bool,
) -> anyhow::Result<String> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let policy: Value = http
        .get(format!("{}/api/join-policy", invite.http))
        .send()
        .await?
        .json()
        .await
        .unwrap_or(Value::Null);
    let mut receipt = None;
    if let Some(policy) = policy.get("policy") {
        if !accept_terms {
            bail!(
                "this community asks members to accept its terms first.\n  terms:   {0}/api/join-policy/terms\n  privacy: {0}/api/join-policy/privacy\nRe-run with --accept-terms once you have read them{1}.",
                invite.http,
                if policy["age_attestation_required"].as_bool() == Some(true) {
                    " (and --age-confirmed: it also requires a minimum-age confirmation)"
                } else {
                    ""
                }
            );
        }
        let accepted = post(
            &http,
            None,
            &format!("{}/api/invites/accept-policy", invite.http),
            json!({
                "code": invite.code,
                "policy_version": policy["version"],
                "age_confirmed": age_confirmed,
            }),
        )
        .await?;
        receipt = accepted.get("receipt").cloned();
    }
    let mut body = json!({ "code": invite.code });
    if let Some(r) = receipt {
        body["policy_receipt"] = r;
    }
    let claimed = post(
        &http,
        Some(keys),
        &format!("{}/api/invites/claim", invite.http),
        body,
    )
    .await?;
    Ok(claimed["status"].as_str().unwrap_or("joined").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_invite_link_forms() {
        let a = parse_invite("https://acme.communities.buzz.xyz/invite/v2.abc").unwrap();
        assert_eq!(a.relay, "wss://acme.communities.buzz.xyz");
        assert_eq!(a.http, "https://acme.communities.buzz.xyz");
        assert_eq!(a.code, "v2.abc");
        let b = parse_invite("buzz://join?relay=ws://localhost:3000&code=xyz").unwrap();
        assert_eq!(b.relay, "ws://localhost:3000");
        assert_eq!(b.http, "http://localhost:3000");
        assert_eq!(b.code, "xyz");
        assert!(parse_invite("https://acme.example/other/xyz").is_err());
    }
}
