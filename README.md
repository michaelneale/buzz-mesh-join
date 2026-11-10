# buzz-mesh-join (spike)

Run a **stock `mesh-llm` release binary** as a member of a Buzz community, headless.

- Community membership (the relay's NIP-43 kind `13534` roster) becomes the node's owner allowlist.
- The node advertises itself with the same member-signed kind `30003` discovery note the Buzz
  desktop publishes, so desktop members see it as shared compute, and only members get in.

## Quick start

You need [mesh-llm](https://github.com/Mesh-LLM/mesh-llm/releases) on PATH, Rust, and an
admin of the Buzz community.

**1. Install**
```sh
cargo install --locked --git https://github.com/michaelneale/buzz-mesh-join
```

**2. Get the node's npub** (it makes its own key the first time)
```sh
export BUZZ_RELAY_URL=wss://<community>.communities.buzz.xyz
buzz-mesh-join whoami
```

**3. Add it to the community.** In Buzz, right-click the community → **Invite to community**
(or **Settings → Membership → Invite members**) → add by npub → paste it.

**4. Check it's in**
```sh
buzz-mesh-join roster        # lists members and the nodes they advertise
```

**5. Serve**
```sh
buzz-mesh-join serve --model unsloth/Qwen3-8B-GGUF:Q4_K_M
```
Leave it running. Community members now see it as shared compute in Buzz, and only members can use it.

**To stop:** Ctrl-C (it withdraws its advertisement). To remove it for good, remove the member in Buzz.

**When membership changes** you do nothing on the box. It re-reads the roster every 30s:
someone added is let in at once; someone removed is dropped once two reads agree. Either way it
restarts its `mesh-llm` (reloading the model). If the box itself is removed, it stops `mesh-llm`
and exits.

Other commands:
```sh
buzz-mesh-join client        # use other members' nodes: OpenAI API on :9337
buzz-mesh-join join --accept-terms --age-confirmed https://<community>/invite/<code>   # instead of step 3
```

Running it on a Mac that already runs Buzz shared compute? Give it its own state and ports so
it doesn't clash: `BUZZ_MESH_STATE=$PWD/node.json MESH_OWNER_KEY=$PWD/owner.json` and
`--api-port 9447 --console-port 3241` before `serve`.

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

Live hosted community (meshllm.communities.buzz.xyz), added by npub:

- `roster` reads the relay-signed roster (58 members, 44 with valid owner bindings).
- `serve` publishes the binding and serve target, then peers with another member's **Buzz desktop**
  node; each side verifies the other's owner. Inference through the headless node to the desktop's
  Qwen3.5-9B works, so the desktop accepts this tool's discovery note.
- `join` with an invite link: NIP-98 auth and the terms step are accepted (a made-up code is refused
  as `invite_invalid`). A real invite hasn't been redeemed yet.

Local, membership-gated buzz-relay from block/buzz main, mesh-llm v0.78.1:

- Member A `serve` (SmolLM2-135M), member B `client`: discovered via the relay alone and inferred over
  QUIC through B's `:9337`.
- Stranger C: the relay rejects NIP-42. With A's leaked invite token passed straight to stock
  `mesh-llm client`, C sees the catalog but inference fails. B still works afterwards.
- Removing B with `buzz-admin remove-member`: A restarts with one owner (after confirming the
  shrink) and B's inference fails. Re-adding B: A re-admits it and B works again.

## Limits

- Agent keys that need a NIP-OA `auth` tag are not wired up; the node uses its own member key.
- The served model is listed as `local-gguf/sha256-…`, not the name passed to `--model`, so requests
  must use that id.
- A roster change restarts the node (it reloads the model). A live update would need
  mesh-llm's `add_membership` (on main, after v0.78.1) or a trust-store reload.
- A member removed from the roster keeps its own nodes running until they notice; serving nodes
  stop admitting it either way. This tool's own node stops and exits once it sees it was removed.
