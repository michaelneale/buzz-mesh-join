//! Serve into a Buzz community using a stock `mesh-llm` child, or opt in to
//! an embedded SDK host sharing one model with the public Mesh. The Buzz
//! endpoint has its own roster-derived owner allowlist and signed discovery
//! note in either mode; public membership never inherits that private roster.

mod node;
mod protocol;
mod public_mesh;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use mesh_llm_identity::{
    default_keystore_path, keystore_exists, keystore_metadata, load_keystore,
    load_owner_keypair_from_keychain, save_keystore, OwnerKeypair,
};
use nostr_sdk::prelude::*;
use tokio::process::{Child, Command};

const HEARTBEAT: Duration = Duration::from_secs(45);

/// The relay answered, and this key is not a member: either the roster omits it
/// (`no_roster: false`, authoritative) or the relay gave it no roster at all,
/// which is how a hosted community answers a non-member (`no_roster: true`).
#[derive(Debug)]
struct NotMember {
    no_roster: bool,
}

impl std::fmt::Display for NotMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.no_roster {
            f.write_str("relay returned no NIP-43 roster (is this key a community member?)")
        } else {
            f.write_str("this key is not on the community roster")
        }
    }
}

impl std::error::Error for NotMember {}

/// Whether to stop after this poll. An explicit roster without us is final at
/// once; a missing roster must be seen twice, so one odd read can't stop a node.
fn removal_confirmed(e: &anyhow::Error, missing_roster_before: &mut bool) -> bool {
    match e.downcast_ref::<NotMember>() {
        Some(NotMember { no_roster: false }) => true,
        Some(NotMember { no_roster: true }) => {
            let confirmed = *missing_roster_before;
            *missing_roster_before = true;
            confirmed
        }
        None => {
            *missing_roster_before = false;
            false
        }
    }
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Buzz community relay, e.g. wss://acme.communities.buzz.xyz.
    /// Defaults to the community joined with `join`.
    #[arg(long, env = "BUZZ_RELAY_URL")]
    relay: Option<String>,
    /// Use this Nostr secret key (nsec or hex) instead of the node's own key.
    /// It must be on the community roster.
    #[arg(long, env = "BUZZ_NSEC", hide_env_values = true)]
    nsec: Option<String>,
    /// Where the node keeps its own key and joined community.
    #[arg(long, env = "BUZZ_MESH_STATE")]
    state: Option<PathBuf>,
    /// MeshLLM owner keystore (created, unencrypted, if missing).
    #[arg(long, env = "MESH_OWNER_KEY")]
    owner_key: Option<PathBuf>,
    /// The stock mesh-llm binary to supervise.
    #[arg(long, env = "MESH_LLM_BIN", default_value = "mesh-llm")]
    mesh_llm: PathBuf,
    #[arg(long, default_value_t = 9337)]
    api_port: u16,
    #[arg(long, default_value_t = 3131)]
    console_port: u16,
    /// Roster poll interval.
    #[arg(long, default_value_t = 30)]
    poll_secs: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Clone)]
enum Cmd {
    /// Serve a model to the community (optionally also to the public Mesh).
    Serve {
        #[arg(long)]
        model: String,
        /// Share one loaded model with the public Mesh and a separate Buzz membership.
        #[arg(long)]
        public: bool,
        /// Mesh TOML for model/runtime tuning in --public mode (no CLI passthrough).
        #[arg(long, requires = "public")]
        config: Option<PathBuf>,
        /// Separate loopback API for the Buzz membership in --public mode.
        #[arg(long, default_value_t = 9338, requires = "public")]
        buzz_api_port: u16,
        /// Extra args passed through to stock `mesh-llm serve` (Buzz-only mode).
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// Explicitly download/cache the recommended native runtime for --public mode.
    InstallRuntime,
    /// Join a member's served node and expose it on the local OpenAI port.
    Client {
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// Print the roster, verified owner bindings and serve targets, then exit.
    Roster,
    /// Join a community with an invite link, as this node's own key.
    Join {
        /// https://<community>/invite/<code> or buzz://join?relay=…&code=…
        invite: String,
        /// Accept the community's terms and privacy policy, if it has them.
        #[arg(long)]
        accept_terms: bool,
        /// Confirm the minimum age, if the community requires it.
        #[arg(long)]
        age_confirmed: bool,
    },
    /// Print this node's npub (an admin can add it to a community directly).
    Whoami,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The SDK engine can re-exec this binary for a built-in plugin.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--plugin") {
        let name = args.next().context("--plugin needs a plugin name")?;
        return mesh_llm_sdk::plugin::run_plugin_process(name, args.collect()).await;
    }
    let mut cli = Cli::parse();
    public_mesh::validate(&cli)?;
    if matches!(cli.cmd, Cmd::InstallRuntime) {
        return public_mesh::install_runtime().await;
    }
    let state_path = match &cli.state {
        Some(p) => p.clone(),
        None => node::default_state_path()?,
    };
    let mut state = node::load(&state_path)?;
    let keys = node::keys(cli.nsec.as_deref(), &state_path, &mut state)?;
    match &cli.cmd {
        Cmd::Whoami => {
            println!("{}", keys.public_key().to_bech32()?);
            if let Some(relay) = cli.relay.as_ref().or(state.relay.as_ref()) {
                eprintln!("community: {relay}");
            }
            return Ok(());
        }
        Cmd::Join {
            invite,
            accept_terms,
            age_confirmed,
        } => {
            let invite = node::parse_invite(invite)?;
            let status = node::claim(&keys, &invite, *accept_terms, *age_confirmed).await?;
            state.relay = Some(invite.relay.clone());
            node::save(&state_path, &state)?;
            println!(
                "{status}: {} is a member of {}",
                keys.public_key().to_bech32()?,
                invite.relay
            );
            return Ok(());
        }
        _ => {}
    }
    let relay = match cli.relay.clone().or(state.relay.clone()) {
        Some(r) => r,
        None => {
            bail!("no community: run `buzz-mesh-join join <invite-link>` or set BUZZ_RELAY_URL")
        }
    };
    cli.relay = Some(relay.clone());
    let owner_path = match &cli.owner_key {
        Some(p) => p.clone(),
        None => default_keystore_path()?,
    };
    let owner = load_or_create_owner(&owner_path)?;
    let relay_self = relay_self_pubkey(&relay).await;
    eprintln!(
        "member {} | owner {} | relay roster signer {}",
        keys.public_key().to_bech32()?,
        owner.owner_id(),
        relay_self
            .map(|p| p.to_hex())
            .unwrap_or_else(|| "unverified (no NIP-11 self)".into())
    );

    let client = Client::builder()
        .signer(keys.clone())
        .opts(ClientOptions::new().automatic_authentication(true))
        .build();
    client.add_relay(&relay).await?;
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(10)).await;

    let mut sup = Supervisor {
        cli: &cli,
        keys: &keys,
        owner: &owner,
        owner_path,
        client: &client,
        relay_self,
        child: None,
        public_node: None,
    };
    let result = tokio::select! {
        r = sup.run() => r,
        _ = tokio::signal::ctrl_c() => { eprintln!("interrupted"); Ok(()) }
    };
    if !matches!(cli.cmd, Cmd::Roster) {
        // Withdraw serve targets so desktops stop routing here at once.
        let _ = sup.publish(&[]).await;
    }
    sup.stop_child().await;
    let shutdown = match sup.public_node.take() {
        Some(node) => node.shutdown().await.context("stopping public Mesh node"),
        None => Ok(()),
    };
    client.disconnect().await;
    result.and(shutdown)
}

struct Supervisor<'a> {
    cli: &'a Cli,
    keys: &'a Keys,
    owner: &'a OwnerKeypair,
    owner_path: PathBuf,
    client: &'a Client,
    relay_self: Option<PublicKey>,
    child: Option<(Child, Launch)>,
    public_node: Option<mesh_llm_sdk::MeshNode>,
}

#[derive(Clone, PartialEq, Debug)]
struct Launch {
    allow: BTreeSet<String>,
    join: Option<String>,
}

impl Supervisor<'_> {
    async fn run(&mut self) -> anyhow::Result<()> {
        if matches!(self.cli.cmd, Cmd::Roster) {
            return self.print_roster().await;
        }
        if matches!(self.cli.cmd, Cmd::Serve { public: true, .. }) {
            return public_mesh::run(self).await;
        }
        // Presence first: other members' serve nodes admit owners they can see.
        self.publish(&[]).await?;
        let mut pending_shrink: Option<Launch> = None;
        let mut missing_roster = false;
        let mut last_beat = Instant::now();
        let mut advertised: Vec<(String, String)> = Vec::new();
        loop {
            let desired = self.desired().await;
            if desired.is_ok() {
                missing_roster = false;
            }
            match desired {
                Err(e) if removal_confirmed(&e, &mut missing_roster) => {
                    // Removed in Buzz: stop serving rather than run on a stale allowlist.
                    self.stop_child().await;
                    bail!("removed from the community: {e}; stopped mesh-llm");
                }
                Err(e) => eprintln!("roster query failed; keeping current node: {e:#}"),
                Ok(None) => eprintln!("waiting for a serve target from another member"),
                Ok(Some(want)) => {
                    let current = self.child.as_ref().map(|(_, l)| l.clone());
                    let shrinks = current
                        .as_ref()
                        .is_some_and(|c| !want.allow.is_superset(&c.allow));
                    if current.as_ref() != Some(&want) {
                        // Like the desktop: grow at once, shrink only when seen twice,
                        // so one relay blip never de-admits everyone.
                        if shrinks && pending_shrink.as_ref() != Some(&want) {
                            eprintln!("roster shrink observed; confirming on next poll");
                            pending_shrink = Some(want);
                        } else {
                            pending_shrink = None;
                            self.restart(want).await?;
                            advertised.clear();
                        }
                    } else {
                        pending_shrink = None;
                    }
                }
            }
            if let Some((child, launch)) = &mut self.child {
                if let Some(status) = child.try_wait()? {
                    eprintln!("mesh-llm exited ({status}); restarting");
                    let launch = launch.clone();
                    self.child = None;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    self.restart(launch).await?;
                    advertised.clear();
                }
            }
            if matches!(self.cli.cmd, Cmd::Serve { .. }) {
                let ready = self.ready_targets().await.unwrap_or_default();
                if ready != advertised {
                    eprintln!("advertising {} serve target(s)", ready.len());
                    advertised = ready;
                    self.publish(&advertised).await?;
                    last_beat = Instant::now();
                }
            }
            if last_beat.elapsed() >= HEARTBEAT {
                self.publish(&advertised).await?;
                last_beat = Instant::now();
            }
            tokio::time::sleep(Duration::from_secs(
                self.cli.poll_secs.min(HEARTBEAT.as_secs()),
            ))
            .await;
        }
    }

    async fn fetch(&self) -> anyhow::Result<(Vec<Event>, BTreeSet<String>)> {
        let mut events: Vec<Event> = Vec::new();
        for f in [protocol::membership_filter(), protocol::status_filter()] {
            events.extend(self.client.fetch_events(f, Duration::from_secs(10)).await?);
        }
        let members = protocol::members(&events, self.relay_self.as_ref())
            .ok_or(NotMember { no_roster: true })?;
        if !members.contains(&self.keys.public_key().to_hex()) {
            return Err(NotMember { no_roster: false }.into());
        }
        Ok((events, members))
    }

    /// What the mesh-llm child should be running with right now.
    async fn desired(&self) -> anyhow::Result<Option<Launch>> {
        let (events, members) = self.fetch().await?;
        let mut allow = protocol::allowlist(&events, &members);
        allow.insert(self.owner.owner_id());
        let join = match self.cli.cmd {
            Cmd::Client { .. } => {
                let targets =
                    protocol::serve_targets(&events, &members, &self.owner.owner_id(), now());
                // Stay on the current target while it is still advertised.
                let current = self.child.as_ref().and_then(|(_, l)| l.join.clone());
                match current.filter(|c| targets.iter().any(|(_, _, ep)| ep == c)) {
                    Some(c) => Some(c),
                    None => match targets.into_iter().next() {
                        Some((_, _, ep)) => Some(ep),
                        None => return Ok(None),
                    },
                }
            }
            _ => None,
        };
        Ok(Some(Launch { allow, join }))
    }

    async fn restart(&mut self, launch: Launch) -> anyhow::Result<()> {
        self.stop_child().await;
        let mut cmd = Command::new(&self.cli.mesh_llm);
        match &self.cli.cmd {
            Cmd::Serve { model, extra, .. } => {
                cmd.args(["serve", "--model", model]).args(extra);
            }
            Cmd::Client { extra } => {
                cmd.arg("client").args(extra);
            }
            Cmd::Roster | Cmd::Join { .. } | Cmd::Whoami | Cmd::InstallRuntime => unreachable!(),
        }
        if let Some(token) = &launch.join {
            cmd.args(["--join", token]);
        }
        cmd.arg("--owner-key")
            .arg(&self.owner_path)
            .args([
                "--owner-required",
                "--trust-policy",
                "allowlist",
                "--headless",
            ])
            .args([
                "--port",
                &self.cli.api_port.to_string(),
                "--console",
                &self.cli.console_port.to_string(),
            ]);
        for owner in &launch.allow {
            cmd.args(["--trust-owner", owner]);
        }
        // Never --publish or --auto: the Buzz relay is the only discovery surface.
        eprintln!(
            "starting mesh-llm (admitting {} owner(s){})",
            launch.allow.len(),
            if launch.join.is_some() {
                ", joining member target"
            } else {
                ""
            }
        );
        let log_path = self
            .owner_path
            .with_file_name("buzz-mesh-join.mesh-llm.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        cmd.stdout(log.try_clone()?)
            .stderr(log)
            .stdin(std::process::Stdio::null());
        eprintln!("mesh-llm output -> {}", log_path.display());
        let child = cmd
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", self.cli.mesh_llm.display()))?;
        self.child = Some((child, launch));
        Ok(())
    }

    async fn stop_child(&mut self) {
        if let Some((mut child, _)) = self.child.take() {
            if let Some(pid) = child.id() {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status()
                    .await;
            }
            if tokio::time::timeout(Duration::from_secs(15), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }

    /// `(modelId, endpointAddr)` for models this node has ready, from its own API.
    async fn ready_targets(&self) -> anyhow::Result<Vec<(String, String)>> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        let status: serde_json::Value = http
            .get(format!(
                "http://127.0.0.1:{}/api/status",
                self.cli.console_port
            ))
            .send()
            .await?
            .json()
            .await?;
        let token = status
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        Ok(ready_targets_from_status(&status, &token))
    }

    async fn publish(&self, targets: &[(String, String)]) -> anyhow::Result<()> {
        let event = protocol::status_event(self.keys, self.owner, targets)?;
        let out = self.client.send_event(&event).await?;
        if out.success.is_empty() {
            bail!("relay rejected status note: {:?}", out.failed);
        }
        Ok(())
    }

    async fn print_roster(&self) -> anyhow::Result<()> {
        let (events, members) = self.fetch().await?;
        let allow = protocol::allowlist(&events, &members);
        let targets = protocol::serve_targets(&events, &members, "", now());
        println!("members: {}", members.len());
        println!(
            "member owner bindings (admission allowlist): {}",
            allow.len()
        );
        for id in &allow {
            println!("  owner {id}");
        }
        println!("fresh verified serve targets: {}", targets.len());
        for (owner, model, ep) in &targets {
            println!(
                "  {model} from owner {}… endpoint {}…",
                &owner[..12],
                &ep[..ep.len().min(24)]
            );
        }
        Ok(())
    }
}

/// Local models only. The caller supplies the endpoint for the intended mesh,
/// never a peer's model or (in dual mode) the public node's token.
fn ready_targets_from_status(status: &serde_json::Value, token: &str) -> Vec<(String, String)> {
    let token = token.trim();
    if token.is_empty() {
        return Vec::new();
    }
    let mut models = BTreeSet::new();
    for model in status
        .get("hosted_models")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str())
    {
        models.insert(model.to_string());
    }
    for model in status
        .pointer("/runtime/models")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if model.get("status").and_then(|v| v.as_str()) == Some("ready") {
            if let Some(id) = model
                .get("model_id")
                .or_else(|| model.get("name"))
                .and_then(|v| v.as_str())
            {
                models.insert(id.to_string());
            }
        }
    }
    models
        .into_iter()
        .map(|id| (id, token.to_string()))
        .collect()
}

fn now() -> u64 {
    Timestamp::now().as_secs()
}

fn load_or_create_owner(path: &std::path::Path) -> anyhow::Result<OwnerKeypair> {
    if !keystore_exists(path) {
        eprintln!("creating MeshLLM owner keystore at {}", path.display());
        save_keystore(path, &OwnerKeypair::generate(), None, false)?;
    }
    let encrypted = keystore_metadata(path)
        .map(|m| m.encrypted)
        .unwrap_or(false);
    let passphrase = std::env::var("MESH_OWNER_PASSPHRASE").ok();
    if encrypted && passphrase.is_none() {
        // mesh-llm keeps an encrypted keystore's passphrase in the OS keychain.
        return load_owner_keypair_from_keychain(path)
            .map_err(|e| anyhow::anyhow!("loading owner key from keychain: {e:?}"));
    }
    load_keystore(path, passphrase.as_deref())
        .with_context(|| format!("loading owner keystore {}", path.display()))
}

/// NIP-11 `self`: the key that signs the relay's NIP-43 roster.
async fn relay_self_pubkey(relay: &str) -> Option<PublicKey> {
    let url = relay
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    let info: serde_json::Value = reqwest::Client::new()
        .get(url)
        .header("Accept", "application/nostr+json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    PublicKey::from_hex(info.get("self")?.as_str()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_needs_explicit_roster_or_two_missing_rosters() {
        let mut missing = false;
        let gone = || anyhow::Error::new(NotMember { no_roster: false });
        let none = || anyhow::Error::new(NotMember { no_roster: true });
        let blip = || anyhow::anyhow!("timeout");
        assert!(removal_confirmed(&gone(), &mut missing));
        missing = false;
        assert!(!removal_confirmed(&none(), &mut missing));
        assert!(
            !removal_confirmed(&blip(), &mut missing),
            "a relay error resets the count"
        );
        assert!(!removal_confirmed(&none(), &mut missing));
        assert!(removal_confirmed(&none(), &mut missing));
    }
}
