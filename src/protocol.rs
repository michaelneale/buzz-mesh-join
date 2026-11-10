//! Buzz <-> MeshLLM discovery protocol, byte-compatible with the Buzz desktop
//! (`desktop/src-tauri/src/mesh_llm/{identity,discovery,coordinator}.rs`).
//!
//! * kind 13534 (NIP-43): the relay-signed community roster.
//! * kind 30003, `d=buzz-mesh-member-status:<ownerId>`, `k=buzz-mesh-status`:
//!   a member-signed note binding that member to a MeshLLM owner key and,
//!   when serving, to the exact endpoint tokens it advertises.

use std::collections::BTreeSet;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use mesh_llm_identity::OwnerKeypair;
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

pub const KIND_STATUS: u16 = 30_003;
pub const KIND_MEMBERSHIP: u16 = 13_534;
pub const STATUS_D_PREFIX: &str = "buzz-mesh-member-status";
pub const STATUS_K: &str = "buzz-mesh-status";
/// Desktop routing ignores serve targets older than this.
pub const STATUS_FRESH_SECS: u64 = 120;

pub fn member_binding_bytes(member: &str) -> Vec<u8> {
    format!(
        "buzz-mesh-owner-binding-v1:{}",
        member.trim().to_ascii_lowercase()
    )
    .into_bytes()
}

pub fn member_endpoint_binding_bytes(member: &str, tokens: &[String]) -> Vec<u8> {
    let mut endpoints: Vec<&str> = tokens
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    endpoints.sort_unstable();
    endpoints.dedup();
    let mut digest = Sha256::new();
    for e in endpoints {
        digest.update((e.len() as u64).to_be_bytes());
        digest.update(e.as_bytes());
    }
    format!(
        "buzz-mesh-owner-endpoint-binding-v1:{}:{}",
        member.trim().to_ascii_lowercase(),
        hex::encode(digest.finalize())
    )
    .into_bytes()
}

pub fn status_filter() -> Filter {
    Filter::new()
        .kind(Kind::Custom(KIND_STATUS))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::K), STATUS_K)
        .limit(500)
}

pub fn membership_filter() -> Filter {
    Filter::new().kind(Kind::Custom(KIND_MEMBERSHIP)).limit(1)
}

/// Latest roster. When the relay advertises a NIP-11 `self` key, only a roster
/// signed by it counts (the desktop trusts its own relay connection instead).
pub fn members(events: &[Event], relay_self: Option<&PublicKey>) -> Option<BTreeSet<String>> {
    events
        .iter()
        .filter(|e| e.kind.as_u16() == KIND_MEMBERSHIP)
        .filter(|e| relay_self.is_none_or(|pk| &e.pubkey == pk))
        .max_by_key(|e| e.created_at)
        .map(|e| {
            e.tags
                .iter()
                .filter_map(|t| {
                    let s = t.as_slice();
                    matches!(s.first().map(String::as_str), Some("member" | "p"))
                        .then(|| s.get(1))
                        .flatten()
                        .map(|pk| pk.trim().to_ascii_lowercase())
                })
                .filter(|pk| !pk.is_empty())
                .collect()
        })
}

fn content(e: &Event) -> Option<serde_json::Value> {
    serde_json::from_str(&e.content).ok()
}

fn hex_field<const N: usize>(v: &serde_json::Value, key: &str) -> Option<[u8; N]> {
    hex::decode(v.get(key)?.as_str()?.trim())
        .ok()?
        .try_into()
        .ok()
}

/// ownerId == sha256(ownerVerifyingKey) and ownerBindingSig covers the note author.
pub fn verified_owner(e: &Event) -> Option<(String, VerifyingKey)> {
    let c = content(e)?;
    let owner_id = c
        .get("ownerId")
        .or_else(|| c.get("owner_id"))?
        .as_str()?
        .trim()
        .to_string();
    let vk_bytes: [u8; 32] = hex_field(&c, "ownerVerifyingKey")?;
    if owner_id != hex::encode(Sha256::digest(vk_bytes)) {
        return None;
    }
    let vk = VerifyingKey::from_bytes(&vk_bytes).ok()?;
    let sig = Signature::from_bytes(&hex_field::<64>(&c, "ownerBindingSig")?);
    vk.verify(&member_binding_bytes(&e.pubkey.to_hex()), &sig)
        .ok()?;
    Some((owner_id, vk))
}

/// `(modelId, endpointAddr)` pairs, only if the endpoint binding covers them.
pub fn verified_targets(e: &Event) -> Vec<(String, String)> {
    let Some(((_, vk), c)) = verified_owner(e).zip(content(e)) else {
        return Vec::new();
    };
    let targets: Vec<(String, String)> = c
        .get("serveTargets")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let model = t.get("modelId")?.as_str()?.trim().to_string();
            let ep = t.get("endpointAddr")?.as_str()?.trim().to_string();
            (!ep.is_empty()).then_some((model, ep))
        })
        .collect();
    let tokens: Vec<String> = targets.iter().map(|(_, ep)| ep.clone()).collect();
    let ok = hex_field::<64>(&c, "ownerEndpointBindingSig")
        .map(|s| Signature::from_bytes(&s))
        .is_some_and(|sig| {
            vk.verify(
                &member_endpoint_binding_bytes(&e.pubkey.to_hex(), &tokens),
                &sig,
            )
            .is_ok()
        });
    if ok {
        targets
    } else {
        Vec::new()
    }
}

fn member_statuses<'a>(
    events: &'a [Event],
    members: &'a BTreeSet<String>,
) -> impl Iterator<Item = &'a Event> {
    events
        .iter()
        .filter(|e| e.kind.as_u16() == KIND_STATUS)
        .filter(move |e| members.contains(&e.pubkey.to_hex()))
}

/// Admission roster: owner ids of current members with valid bindings.
/// Freshness is deliberately ignored (membership is the trust boundary).
pub fn allowlist(events: &[Event], members: &BTreeSet<String>) -> BTreeSet<String> {
    member_statuses(events, members)
        .filter_map(|e| verified_owner(e).map(|(id, _)| id))
        .collect()
}

/// Fresh, verified serve targets from current members other than `self_owner`.
pub fn serve_targets(
    events: &[Event],
    members: &BTreeSet<String>,
    self_owner: &str,
    now: u64,
) -> Vec<(String, String, String)> {
    member_statuses(events, members)
        .filter(|e| e.created_at.as_secs() + STATUS_FRESH_SECS >= now)
        .filter_map(|e| {
            verified_owner(e)
                .filter(|(id, _)| id != self_owner)
                .map(|(id, _)| (id, e))
        })
        .flat_map(|(id, e)| {
            verified_targets(e)
                .into_iter()
                .map(move |(m, ep)| (id.clone(), m, ep))
        })
        .collect()
}

/// The member-signed status note the desktop publishes (same shape and tags).
pub fn status_event(
    keys: &Keys,
    owner: &OwnerKeypair,
    targets: &[(String, String)],
) -> anyhow::Result<Event> {
    let member = keys.public_key().to_hex();
    let tokens: Vec<String> = targets.iter().map(|(_, ep)| ep.clone()).collect();
    let payload = serde_json::json!({
        "ownerId": owner.owner_id(),
        "ownerVerifyingKey": hex::encode(owner.verifying_key().as_bytes()),
        "ownerBindingSig": hex::encode(owner.sign_bytes(&member_binding_bytes(&member))),
        "ownerEndpointBindingSig":
            hex::encode(owner.sign_bytes(&member_endpoint_binding_bytes(&member, &tokens))),
        "serveTargets": targets.iter().map(|(m, ep)| serde_json::json!({
            "modelId": m, "modelName": null, "endpointAddr": ep, "nodeName": null, "capacity": null,
        })).collect::<Vec<_>>(),
        "models": targets.iter().map(|(m, _)| serde_json::json!({"id": m})).collect::<Vec<_>>(),
    });
    let d = format!("{STATUS_D_PREFIX}:{}", owner.owner_id());
    Ok(
        EventBuilder::new(Kind::Custom(KIND_STATUS), payload.to_string())
            .tags([Tag::parse(["d", d.as_str()])?, Tag::parse(["k", STATUS_K])?])
            .sign_with_keys(keys)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(relay: &Keys, members: &[&Keys]) -> Event {
        let tags = members
            .iter()
            .map(|k| Tag::parse(["member", &k.public_key().to_hex()]).unwrap());
        EventBuilder::new(Kind::Custom(KIND_MEMBERSHIP), "")
            .tags(tags)
            .sign_with_keys(relay)
            .unwrap()
    }

    #[test]
    fn roundtrip_binding_and_membership() {
        let relay = Keys::generate();
        let (a, b, stranger) = (Keys::generate(), Keys::generate(), Keys::generate());
        let (oa, ob, os) = (
            OwnerKeypair::generate(),
            OwnerKeypair::generate(),
            OwnerKeypair::generate(),
        );
        let target = vec![("m".to_string(), "tok".to_string())];
        let events = vec![
            roster(&relay, &[&a, &b]),
            status_event(&a, &oa, &target).unwrap(),
            status_event(&b, &ob, &[]).unwrap(),
            status_event(&stranger, &os, &target).unwrap(),
        ];
        let m = members(&events, Some(&relay.public_key())).unwrap();
        let allow = allowlist(&events, &m);
        assert_eq!(allow, [oa.owner_id(), ob.owner_id()].into_iter().collect());
        let now = Timestamp::now().as_secs();
        let t = serve_targets(&events, &m, &ob.owner_id(), now);
        assert_eq!(t, vec![(oa.owner_id(), "m".into(), "tok".into())]);
        // A roster from someone other than the relay's NIP-11 `self` is ignored.
        assert!(members(&events, Some(&a.public_key())).is_none());
    }

    #[test]
    fn substituted_endpoint_is_rejected() {
        let a = Keys::generate();
        let oa = OwnerKeypair::generate();
        let good = status_event(&a, &oa, &[("m".into(), "tok".into())]).unwrap();
        let forged = good.content.replace("\"tok\"", "\"evil\"");
        let forged = EventBuilder::new(good.kind, forged)
            .tags(good.tags.clone())
            .sign_with_keys(&a)
            .unwrap();
        assert!(verified_owner(&forged).is_some());
        assert!(verified_targets(&forged).is_empty());
    }

    /// Fixed vector so a drift from the desktop's canonical bytes is loud.
    #[test]
    fn endpoint_binding_bytes_are_canonical() {
        let got = member_endpoint_binding_bytes(
            " AB ",
            &["b".into(), "a".into(), "a".into(), " ".into()],
        );
        let mut d = Sha256::new();
        for e in ["a", "b"] {
            d.update(1u64.to_be_bytes());
            d.update(e.as_bytes());
        }
        let want = format!(
            "buzz-mesh-owner-endpoint-binding-v1:ab:{}",
            hex::encode(d.finalize())
        );
        assert_eq!(got, want.into_bytes());
    }
}
