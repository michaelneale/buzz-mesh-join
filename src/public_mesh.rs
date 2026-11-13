//! One public serving node, with a separately admitted, inbound-only Buzz
//! membership. Only the attachment is replaced when the roster changes.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use mesh_llm_sdk::embedded_node::{EmbeddedMeshNodeConfig, EmbeddedTrustPolicy};
use mesh_llm_sdk::{MeshMembershipConfig, MeshNode, MeshNodeBuilder};
use sha2::{Digest, Sha256};

use crate::{
    protocol, ready_targets_from_status, removal_confirmed, Cli, Cmd, Supervisor, HEARTBEAT,
};

pub fn validate(cli: &Cli) -> anyhow::Result<()> {
    if let Cmd::Serve {
        public: true,
        extra,
        buzz_api_port,
        ..
    } = &cli.cmd
    {
        ensure!(extra.is_empty(), "--public does not accept arguments after --; use --config <mesh.toml> for model/runtime tuning");
        let ports = [cli.api_port, cli.console_port, *buzz_api_port];
        ensure!(
            ports.iter().all(|p| *p != 0),
            "--public needs fixed, nonzero API, console and Buzz API ports"
        );
        ensure!(
            ports.into_iter().collect::<BTreeSet<_>>().len() == 3,
            "public API, console and Buzz API ports must differ"
        );
    }
    Ok(())
}

pub async fn install_runtime() -> anyhow::Result<()> {
    use mesh_llm_sdk::native_runtime::{
        current_runtime_release, current_skippy_abi_version, install_native_runtime,
        mesh_native_runtime_install_options, native_runtime_versions_match_current_sdk,
        NativeRuntimeInstallOptions, RuntimeSelection,
    };
    let outcome = install_native_runtime(NativeRuntimeInstallOptions {
        release_version: current_runtime_release().to_string(),
        skippy_abi_version: Some(current_skippy_abi_version()),
        selection: RuntimeSelection::Recommended,
        ..mesh_native_runtime_install_options()
    })
    .await?;
    let runtime = outcome.runtime;
    ensure!(
        native_runtime_versions_match_current_sdk(
            &runtime.release_version,
            &runtime.manifest.runtime.skippy_abi
        ),
        "installed runtime is incompatible with this SDK"
    );
    runtime.load_plan()?;
    println!(
        "native runtime {} ready for this SDK",
        runtime.release_version
    );
    Ok(())
}

fn primary_config(cli: &Cli, owner_path: &Path) -> EmbeddedMeshNodeConfig {
    let Cmd::Serve { model, config, .. } = &cli.cmd else {
        unreachable!()
    };
    let mut builder = MeshNode::builder()
        .serve()
        .model(model)
        .api_port(cli.api_port)
        .console_port(cli.console_port)
        .console_ui(false)
        .owner_key(owner_path)
        .owner_required(false)
        .trust_policy(EmbeddedTrustPolicy::Off)
        .auto_join_public_mesh()
        .publish(true)
        .isolated_config(true)
        .startup_timeout(Duration::from_secs(120));
    if let Some(path) = config {
        builder = builder.config_path(path);
    }
    builder.build()
}

/// Byte-compatible with Buzz's mesh-compute `mesh_name_for_relay`.
fn mesh_name_for_relay(relay: &str) -> String {
    let trimmed = relay.trim();
    let normalized = reqwest::Url::parse(trimmed)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|_| trimmed.trim_end_matches('/').to_ascii_lowercase());
    format!(
        "buzz-community-{}",
        &hex::encode(Sha256::digest(normalized.as_bytes()))[..32]
    )
}

fn membership_config(name: &str, port: u16, owners: &BTreeSet<String>) -> MeshMembershipConfig {
    MeshMembershipConfig {
        name: name.to_string(),
        // Like Buzz-only serve, accept connections discovered through our signed
        // note; do not dial private peers or import public bootstrap tokens.
        join_tokens: Vec::new(),
        trusted_owners: owners.iter().cloned().collect(),
        api_port: port,
        node_key_path: None,
        enumerate_host: false,
    }
}

/// Growth is immediate; de-admission needs two consecutive agreeing reads.
fn replacement_confirmed(
    current: Option<&BTreeSet<String>>,
    wanted: &BTreeSet<String>,
    pending: &mut Option<BTreeSet<String>>,
) -> bool {
    if current == Some(wanted) {
        *pending = None;
        return false;
    }
    if current.is_some_and(|c| !wanted.is_superset(c)) && pending.as_ref() != Some(wanted) {
        *pending = Some(wanted.clone());
        return false;
    }
    *pending = None;
    true
}

async fn announce(sup: &Supervisor<'_>, targets: &[(String, String)]) -> bool {
    match sup.publish(targets).await {
        Ok(()) => true,
        Err(e) => {
            eprintln!("Buzz advertisement failed; will retry: {e:#}");
            false
        }
    }
}

async fn ready_targets(node: &MeshNode, name: &str) -> anyhow::Result<Vec<(String, String)>> {
    let memberships = node.memberships().await?;
    let membership = memberships
        .into_iter()
        .find(|m| m.name == name)
        .context("Buzz membership is not running")?;
    let status = tokio::time::timeout(Duration::from_secs(5), node.status()).await??;
    // status.invite_token is PUBLIC. Bind only the current Buzz attachment token.
    Ok(ready_targets_from_status(
        &status.payload,
        &membership.invite_token,
    ))
}

pub async fn run(sup: &mut Supervisor<'_>) -> anyhow::Result<()> {
    // Do not start public serving unless this invocation initially belongs to Buzz.
    sup.fetch().await?;
    sup.publish(&[]).await?;
    let Cmd::Serve { buzz_api_port, .. } = &sup.cli.cmd else {
        unreachable!()
    };
    let name = mesh_name_for_relay(sup.cli.relay.as_deref().context("missing relay")?);
    let config = primary_config(sup.cli, &sup.owner_path);
    eprintln!("starting public Mesh serving; Buzz gets a separate allowlisted membership");
    sup.public_node = Some(MeshNodeBuilder::from_config(config).start().await
        .context("starting embedded public node (a compatible native runtime must already be bundled/cached; run `buzz-mesh-join install-runtime` to install one explicitly)")?);
    let node = sup.public_node.as_ref().expect("started public node");
    let mut current: Option<BTreeSet<String>> = None;
    let mut pending = None;
    let mut missing_roster = false;
    let mut member = true;
    let mut advertised = Vec::new();
    let mut last_beat = Instant::now();

    loop {
        match sup.fetch().await {
            Ok((events, members)) => {
                member = true;
                missing_roster = false;
                let mut owners = protocol::allowlist(&events, &members);
                owners.insert(sup.owner.owner_id());
                if replacement_confirmed(current.as_ref(), &owners, &mut pending) {
                    announce(sup, &[]).await;
                    advertised.clear();
                    current = replace_membership(node, &name, *buzz_api_port, owners).await?;
                }
            }
            Err(e) => {
                pending = None;
                if removal_confirmed(&e, &mut missing_roster) {
                    if member {
                        // No stale allowlist, and no public runtime/model restart.
                        node.remove_membership(&name)
                            .await
                            .context("detaching Buzz after roster removal")?;
                        current = None;
                        member = false;
                        announce(sup, &[]).await;
                        advertised.clear();
                        eprintln!(
                            "removed from Buzz; attachment detached, public serving continues"
                        );
                    }
                } else {
                    eprintln!("Buzz roster query failed; keeping current membership: {e:#}");
                }
            }
        }
        let ready = if member && current.is_some() {
            match ready_targets(node, &name).await {
                Ok(ready) => ready,
                Err(e) => {
                    eprintln!("Buzz readiness check failed; withdrawing targets: {e:#}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        if member
            && (ready != advertised || last_beat.elapsed() >= HEARTBEAT)
            && announce(sup, &ready).await
        {
            advertised = ready;
            last_beat = Instant::now();
        }
        tokio::time::sleep(Duration::from_secs(
            sup.cli.poll_secs.clamp(1, HEARTBEAT.as_secs()),
        ))
        .await;
    }
}

async fn replace_membership(
    node: &MeshNode,
    name: &str,
    port: u16,
    owners: BTreeSet<String>,
) -> anyhow::Result<Option<BTreeSet<String>>> {
    match node
        .add_membership(membership_config(name, port, &owners))
        .await
    {
        Ok(_) => {
            eprintln!(
                "Buzz membership active ({} admitted owners; model stays loaded)",
                owners.len()
            );
            Ok(Some(owners))
        }
        Err(e) => {
            // The SDK may have preserved/rolled back the old allowlist. Fail
            // closed even on a shrink, then retry without reloading the model.
            node.remove_membership(name)
                .await
                .context("removing failed Buzz replacement")?;
            eprintln!(
                "Buzz attachment failed; detached and will retry (public serving continues): {e:#}"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod smoke_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn owners(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn community_identity_matches_buzz_and_normalizes_origin() {
        let expected = "buzz-community-d91dbf16b4343b6383e4ff067f7d9ecc";
        for relay in [
            "wss://meshllm.communities.buzz.xyz/",
            " WSS://MESHLLM.COMMUNITIES.BUZZ.XYZ/path?ignored=1 ",
        ] {
            assert_eq!(mesh_name_for_relay(relay), expected);
        }
        assert_ne!(
            mesh_name_for_relay("wss://other.communities.buzz.xyz"),
            expected
        );
        assert_ne!(
            mesh_name_for_relay("ws://meshllm.communities.buzz.xyz"),
            expected
        );
    }

    #[test]
    fn public_and_buzz_admission_and_discovery_stay_separate() {
        let cli = Cli::parse_from([
            "buzz-mesh-join",
            "serve",
            "--model",
            "model.gguf",
            "--public",
        ]);
        validate(&cli).unwrap();
        let primary = primary_config(&cli, Path::new("owner.json"));
        assert!(primary.network.auto_join && primary.network.publish);
        assert!(!primary.admission.owner_required);
        assert_eq!(
            primary.admission.trust_policy,
            Some(EmbeddedTrustPolicy::Off)
        );
        assert!(primary.admission.trusted_owners.is_empty());
        assert!(primary.network.join_tokens.is_empty());
        assert_eq!(primary.serving.models, vec!["model.gguf"]);
        let private =
            membership_config("buzz-community-example", 9338, &owners(&["verified-owner"]));
        assert_eq!(private.trusted_owners, vec!["verified-owner"]);
        assert!(private.join_tokens.is_empty());
        assert!(!private.enumerate_host);
        assert_ne!(private.api_port, primary.http.api_port);
        assert_ne!(private.api_port, primary.http.console_port);
    }

    #[test]
    fn shrink_confirmation_growth_and_recovery() {
        let full = owners(&["a", "b"]);
        let smaller = owners(&["a"]);
        let different = owners(&["b"]);
        let mut pending = None;
        assert!(replacement_confirmed(None, &full, &mut pending));
        assert!(!replacement_confirmed(Some(&full), &smaller, &mut pending));
        assert!(!replacement_confirmed(
            Some(&full),
            &different,
            &mut pending
        ));
        assert!(replacement_confirmed(Some(&full), &different, &mut pending));
        assert!(pending.is_none());
        assert!(replacement_confirmed(Some(&smaller), &full, &mut pending));
        assert!(!replacement_confirmed(Some(&full), &smaller, &mut pending));
        assert!(!replacement_confirmed(Some(&full), &full, &mut pending));
        assert!(pending.is_none());
        assert!(!replacement_confirmed(Some(&full), &smaller, &mut pending));
        // Failed replacement/removal clears current: a new attachment is immediate.
        assert!(replacement_confirmed(None, &smaller, &mut pending));
    }

    #[test]
    fn invalid_public_options_fail_before_keys_or_network() {
        for args in [
            vec!["serve", "--public", "--model", "m", "--", "--parallel", "4"],
            vec!["--api-port", "9338", "serve", "--public", "--model", "m"],
            vec![
                "--console-port",
                "9338",
                "serve",
                "--public",
                "--model",
                "m",
            ],
            vec!["--api-port", "0", "serve", "--public", "--model", "m"],
        ] {
            let cli = Cli::try_parse_from(std::iter::once("buzz-mesh-join").chain(args)).unwrap();
            assert!(validate(&cli).is_err());
        }
        assert!(Cli::try_parse_from([
            "buzz-mesh-join",
            "serve",
            "--model",
            "m",
            "--config",
            "mesh.toml"
        ])
        .is_err());
        let stock = Cli::parse_from([
            "buzz-mesh-join",
            "serve",
            "--model",
            "m",
            "--",
            "--parallel",
            "4",
        ]);
        validate(&stock).unwrap();
        assert!(
            matches!(stock.cmd, Cmd::Serve { public: false, extra, .. } if extra == ["--parallel", "4"])
        );
    }

    #[test]
    fn only_local_ready_models_get_the_buzz_token() {
        let status = serde_json::json!({
            "token": "PUBLIC-NOT-BUZZ",
            "hosted_models": ["hosted", "duplicate"],
            "models": ["public-peer-model"],
            "runtime": {"models": [
                {"model_id": "duplicate", "status": "ready"},
                {"name": "local", "status": "ready"},
                {"model_id": "loading", "status": "loading"}
            ]}
        });
        assert_eq!(
            ready_targets_from_status(&status, "BUZZ"),
            vec![
                ("duplicate".into(), "BUZZ".into()),
                ("hosted".into(), "BUZZ".into()),
                ("local".into(), "BUZZ".into()),
            ]
        );
        assert!(ready_targets_from_status(&status, " ").is_empty());
        let keys = nostr_sdk::Keys::generate();
        let owner = mesh_llm_identity::OwnerKeypair::generate();
        let event =
            protocol::status_event(&keys, &owner, &ready_targets_from_status(&status, "BUZZ"))
                .unwrap();
        assert!(!event.content.contains("PUBLIC-NOT-BUZZ"));
        assert_eq!(protocol::verified_targets(&event).len(), 3);
    }
}
