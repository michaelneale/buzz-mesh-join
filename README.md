# buzz-mesh-join (spike)

Run a **stock `mesh-llm` release binary** as a member of a Buzz community, headless.

- Community membership (the relay's NIP-43 kind `13534` roster) becomes the node's owner allowlist.
- The node advertises itself with the same member-signed kind `30003` discovery note the Buzz
  desktop publishes, so desktop members see it as shared compute, and only members get in.

```sh
# On the GPU box: get an invite link from a community admin (Invite members in Buzz), then
buzz-mesh-join join --accept-terms https://acme.communities.buzz.xyz/invite/<code>

buzz-mesh-join serve --model unsloth/Qwen3-8B-GGUF:Q4_K_M   # serve to members only
buzz-mesh-join client                                       # OpenAI API on :9337 through a member's node
buzz-mesh-join roster                                       # what the community advertises
```

## Where the key comes from

The box doesn't need anyone's personal key. On first run it makes its own Nostr key and keeps
it in `~/.config/buzz-mesh-join/node.json` (mode 0600; `--state` / `BUZZ_MESH_STATE` to move
it). It then becomes a community member the same way a person does:

- **Invite link:** `join <link>` redeems an admin's invite (`POST /api/invites/claim`, NIP-98
  signed by the node's key) and remembers the community, so later commands need no flags.
  Communities with terms (like hosted Buzz communities) need `--accept-terms`, and
  `--age-confirmed` where they ask for it; the error links the terms to read first.
- **Add by npub:** `whoami` prints the node's npub; an admin can add it directly in Buzz.

The node then shows up as its own member of the community, and admins remove it like anyone
else. Removing it de-admits it everywhere.

`--nsec` / `BUZZ_NSEC` still uses an existing member key instead, and `--relay` /
`BUZZ_RELAY_URL` overrides the remembered community.

Other options: `--mesh-llm <bin>` (default `mesh-llm` on PATH), `--owner-key <keystore>` (default
`~/.mesh-llm/owner-keystore.json`, created if missing; set `MESH_OWNER_PASSPHRASE` for an
encrypted one on a headless box), `--api-port`, `--console-port`, `--poll-secs`. Anything
after `--` goes straight to `mesh-llm serve|client`.

## Install

Needs Rust and a [mesh-llm](https://github.com/Mesh-LLM/mesh-llm/releases) release on PATH.

```sh
cargo install --locked --git https://github.com/michaelneale/buzz-mesh-join
```

## How it works

No mesh-llm changes are needed. Stock mesh-llm already has every knob:
`--owner-key --owner-required --trust-policy allowlist --trust-owner <id>... --join <token>`,
and it does not publish or auto-join unless asked. This tool is just the glue:

1. Authenticate to the relay (NIP-42) and read the roster plus members' discovery notes.
   When the relay's NIP-11 lists a `self` key, the tool only accepts a roster signed by it.
2. Admit owners from members' notes whose `ownerBindingSig` verifies (sha256(key) = ownerId,
   signed over the member's pubkey). Same rules as the desktop's `discovery.rs`.
3. Publish this node's own binding, then, once serving, the invite token as `serveTargets`,
   covered by `ownerEndpointBindingSig`. Heartbeat every 45s and withdraw on exit.
4. Poll the roster. Growth restarts mesh-llm with the new allowlist straight away; a shrink must be
   seen twice first, so one relay blip can't drop everyone. The desktop works the same way.
   mesh-llm only reads `--trust-owner` at startup.
5. `client` picks a fresh, verified serve target from another member and runs
   `mesh-llm client --join <token>` with the same allowlist.

## Verified

- `join` against the live hosted community relay: NIP-98 auth and the terms step are accepted
  (a made-up code is refused as `invite_invalid`, past auth). A real invite hasn't been redeemed yet.

Local, membership-gated buzz-relay from block/buzz main, mesh-llm v0.78.1:

- Member A `serve` (SmolLM2-135M), member B `client`: discovered via the relay alone and inferred over
  QUIC through B's `:9337`.
- Stranger C: the relay rejects NIP-42. With A's leaked invite token passed straight to stock
  `mesh-llm client`, C sees the catalog but inference fails. B still works afterwards.
- Removing B with `buzz-admin remove-member`: A restarts with one owner (after confirming the
  shrink) and B's inference fails. Re-adding B: A re-admits it and B works again.

## Limits

- Agent keys that need a NIP-OA `auth` tag are not wired up; the node uses its own member key.
- It hasn't been tried against a live Buzz desktop yet. The payload, tags and binding bytes follow
  `desktop/src-tauri/src/mesh_llm/{identity,discovery,coordinator}.rs` line for line.
- A roster change restarts the node (it reloads the model). A live update would need
  mesh-llm's `add_membership` (on main, after v0.78.1) or a trust-store reload.
- A member removed from the roster keeps its own client process running, but serving nodes stop
  admitting it.
