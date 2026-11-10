# buzz-mesh-join (spike)

Run a **stock `mesh-llm` release binary** as a member of a Buzz community, headless.

- Community membership (the relay's NIP-43 kind `13534` roster) becomes the node's owner allowlist.
- The node advertises itself with the same member-signed kind `30003` discovery note the Buzz
  desktop publishes, so desktop members see it as shared compute, and only members get in.

```sh
export BUZZ_RELAY_URL=wss://acme.communities.buzz.xyz
export BUZZ_NSEC=nsec1...          # a key that is on the community roster

buzz-mesh-join roster                                    # what the community advertises
buzz-mesh-join serve --model unsloth/Qwen3-8B-GGUF:Q4_K_M # serve to members only
buzz-mesh-join client                                    # OpenAI API on :9337 through a member's node
```

Options: `--mesh-llm <bin>` (default `mesh-llm` on PATH), `--owner-key <keystore>` (default
`~/.mesh-llm/owner-keystore.json`, created if missing), `--api-port`, `--console-port`,
`--poll-secs`. Anything after `--` goes straight to `mesh-llm serve|client`.

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

## Verified (local, membership-gated buzz-relay from block/buzz main, mesh-llm v0.78.1)

- Member A `serve` (SmolLM2-135M), member B `client`: discovered via the relay alone and inferred over
  QUIC through B's `:9337`.
- Stranger C: the relay rejects NIP-42. With A's leaked invite token passed straight to stock
  `mesh-llm client`, C sees the catalog but inference fails. B still works afterwards.
- Removing B with `buzz-admin remove-member`: A restarts with one owner (after confirming the
  shrink) and B's inference fails. Re-adding B: A re-admits it and B works again.

## Limits

- The key must be a plain member nsec. Agent keys that need a NIP-OA `auth` tag are not wired up.
- It hasn't been tried against a live Buzz desktop yet. The payload, tags and binding bytes follow
  `desktop/src-tauri/src/mesh_llm/{identity,discovery,coordinator}.rs` line for line.
- A roster change restarts the node (it reloads the model). A live update would need
  mesh-llm's `add_membership` (on main, after v0.78.1) or a trust-store reload.
- A member removed from the roster keeps its own client process running, but serving nodes stop
  admitting it.
