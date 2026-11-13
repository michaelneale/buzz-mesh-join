//! Explicit real-runtime check. Run with a disposable HOME, a compatible
//! bundled/cached native runtime, and BUZZ_MESH_SMOKE_MODEL=/path/to/model.gguf.
//! Public discovery and relays are disabled; nothing joins the real community.

use std::net::{Ipv4Addr, TcpListener};

use super::*;
use clap::Parser;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a native runtime, local GGUF and disposable HOME; see README"]
async fn one_model_survives_buzz_replacement_failure_and_detach() -> anyhow::Result<()> {
    let model = std::env::var("BUZZ_MESH_SMOKE_MODEL").context("set BUZZ_MESH_SMOKE_MODEL")?;
    let temp = tempfile::tempdir()?;
    let owner_path = temp.path().join("owner.json");
    let owner = crate::load_or_create_owner(&owner_path)?;
    let sockets: Vec<TcpListener> = (0..3)
        .map(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
        .collect::<Result<_, _>>()?;
    let ports: Vec<u16> = sockets
        .iter()
        .map(|s| s.local_addr().unwrap().port())
        .collect();
    let cli = Cli::parse_from(["buzz-mesh-join", "serve", "--public", "--model", &model]);
    let mut config = primary_config(&cli, &owner_path);
    config.http.api_port = ports[0];
    config.http.console_port = ports[1];
    config.network.auto_join = false;
    config.network.publish = false;
    config.network.disable_iroh_relays = true;
    config.network.enumerate_host = false;
    config.network.bind_ip = Some(Ipv4Addr::LOCALHOST.into());
    config.network.mesh_name = Some("buzz-dual-smoke-primary".into());
    drop(sockets);
    let node = MeshNodeBuilder::from_config(config).start().await?;
    let result = exercise_membership(&node, ports[2], &owner.owner_id()).await;
    let stopped = tokio::time::timeout(Duration::from_secs(30), node.shutdown()).await?;
    result?;
    stopped?;
    for port in ports {
        TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .context("shutdown must release all API ports")?;
    }
    Ok(())
}

async fn exercise_membership(node: &MeshNode, port: u16, owner: &str) -> anyhow::Result<()> {
    let name = mesh_name_for_relay("wss://isolated-test.example");
    let owners = BTreeSet::from([owner.to_string()]);
    let attached = node
        .add_membership(membership_config(&name, port, &owners))
        .await?;
    let models = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let targets = ready_targets(node, &name).await?;
            if !targets.is_empty() {
                break Ok::<_, anyhow::Error>(targets);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    let model = &models[0].0;
    let primary_token = node.status().await?.invite_token.context("primary token")?;
    ensure!(
        models.iter().all(|(_, token)| token != &primary_token),
        "never publish public token to Buzz"
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    complete(&http, node.api_base_url(), model).await?;
    complete(&http, &attached.api_base_url, model).await?;

    let mut grown = owners.clone();
    grown.insert(mesh_llm_identity::OwnerKeypair::generate().owner_id());
    ensure!(replace_membership(node, &name, port, grown)
        .await?
        .is_some());
    complete(&http, &attached.api_base_url, model).await?;
    ensure!(replace_membership(node, &name, port, owners.clone())
        .await?
        .is_some());
    complete(&http, node.api_base_url(), model).await?;

    // Invalid replacement checks leave the SDK's old membership running. Our
    // adapter must explicitly remove it rather than preserve an obsolete ACL.
    let primary_port = reqwest::Url::parse(node.api_base_url())?
        .port()
        .context("API port")?;
    ensure!(
        replace_membership(node, &name, primary_port, owners.clone())
            .await?
            .is_none()
    );
    ensure!(
        node.memberships().await?.is_empty(),
        "failed replacement must fail closed"
    );
    ensure!(
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok(),
        "old membership must release its listener"
    );
    complete(&http, node.api_base_url(), model).await?;

    node.add_membership(membership_config(&name, port, &owners))
        .await?;
    complete(&http, &attached.api_base_url, model).await?;
    ensure!(node.remove_membership(&name).await?);
    ensure!(node.memberships().await?.is_empty());
    ensure!(
        http.get(format!("{}/models", attached.api_base_url))
            .send()
            .await
            .is_err(),
        "detached Buzz API must close"
    );
    complete(&http, node.api_base_url(), model).await?;
    println!("dual-membership smoke: primary + Buzz inference, growth/shrink, fail-closed replacement, detach, public-side continuation passed");
    Ok(())
}

async fn complete(http: &reqwest::Client, api: &str, model: &str) -> anyhow::Result<()> {
    let response = http
        .post(format!("{api}/chat/completions"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Say hello."}],
            "max_tokens": 16,
            "temperature": 0
        }))
        .send()
        .await?;
    let status = response.status();
    let payload: serde_json::Value = response.json().await?;
    ensure!(
        status.is_success(),
        "completion failed: {status}: {payload}"
    );
    ensure!(
        payload
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty()),
        "completion must generate text: {payload}"
    );
    Ok(())
}
