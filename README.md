# buzz-mesh-join (spike)

Run a **stock `mesh-llm` release binary** as a member of a Buzz community, headless.
Or opt in to `serve --public` to share one loaded model with **both Buzz and the public
Mesh**, using independent memberships through the embedded SDK.

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

## Share the same model with Buzz and the public Mesh

Opt in with `serve --public`. This uses the **Mesh SDK pinned to v0.79.0**, not the
`--mesh-llm` executable. It starts one public serving runtime and adds a separate Buzz
membership sharing its loaded models. It **does not attach to an already-running Mesh
process**. Do not start it alongside an existing GPU service expecting another free model
copy; replacing a live service is a separate operator action.

After adding this utility's npub to the community as above:

```sh
# Explicit setup: resolves/downloads the recommended compatible native runtime,
# or reuses the compatible cache. Does not join Buzz or start serving.
buzz-mesh-join install-runtime

# API :9337 and console :3131 belong to the public node.
# Buzz gets its own loopback API :9338 and separate endpoint identity.
buzz-mesh-join serve --public --model unsloth/Qwen3-8B-GGUF:Q4_K_M
```

Serving itself never downloads a native runtime. A compatible runtime must already be in
the SDK's cache or packaged beside **this utility**; installing a stock `mesh-llm` binary
elsewhere on PATH is not sufficient. Model resolution may still download model weights.
Use `install-runtime` explicitly when needed, on the machine/account that will serve.

- **Public Mesh:** normal public auto-join and publication, without the Buzz allowlist.
  Non-community users can use the shared model. Do not enable this for private-only weights.
- **Buzz:** community-derived mesh identity, separate endpoint key, owner-required allowlist
  from verified current members, and no public discovery. Like Buzz-only `serve`, it accepts
  inbound connections discovered via its signed Buzz note rather than dialing member nodes.
- Buzz advertisements contain **only the Buzz attachment's current invite token**, never
  the public node's token. Public peers do not learn Buzz peers or receive routes to them.
- Owner growth replaces only the attachment immediately; a shrink needs two consecutive
  agreeing reads. A failed replacement removes any preserved/rolled-back attachment, then
  retries. The public runtime and model stay loaded. Replacement briefly interrupts Buzz.
- If this node is removed from Buzz, the attachment and advertisement are withdrawn;
  **public serving continues**. Re-adding it allows the utility to attach again. Ctrl-C stops
  both. Initial startup still requires verified community membership.

### Model tuning and ports

Arguments after `--` still pass to stock `serve`/`client` in Buzz-only mode, but are rejected
with `--public`: they cannot silently configure an SDK host. Use an explicit Mesh TOML
config for context, concurrency, KV cache and native-runtime selection:

```sh
buzz-mesh-join --api-port 9447 --console-port 3241 serve --public \
  --buzz-api-port 9448 --config /private/operator/mesh.toml \
  --model unsloth/Qwen3-8B-GGUF:Q4_K_M
```

The three ports must be nonzero and different; the Buzz API binds to loopback. With no
`--config`, the SDK uses an isolated configuration (including disabled telemetry/blobstore
plugins) rather than the user's default Mesh config. An explicit config is operator-trusted
and follows normal Mesh config semantics, including its plugin and telemetry settings.
See the [v0.79.0 configuration guide](https://github.com/Mesh-LLM/mesh-llm/blob/v0.79.0/mesh/docs/USAGE.md)
for the supported model settings. Protect the local APIs and owner/state files; do not
publish a management console directly to the Internet.

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
else. Removing it revokes community access; opt-in public serving is independent.

`--nsec` / `BUZZ_NSEC` still uses an existing member key instead, and `--relay` /
`BUZZ_RELAY_URL` overrides the remembered community.

Other options: `--mesh-llm <bin>` (default `mesh-llm` on PATH), `--owner-key <keystore>` (default
`~/.mesh-llm/owner-keystore.json`, created if missing; set `MESH_OWNER_PASSPHRASE` for an
encrypted one on a headless box), `--api-port`, `--console-port`, `--poll-secs`. Anything
after `--` goes straight to `mesh-llm serve|client` in Buzz-only mode. `--public` instead
uses the embedded SDK and accepts tuning through `--config`.

## How it works

No mesh-llm changes are needed. In **Buzz-only mode**, stock mesh-llm already has every knob:
`--owner-key --owner-required --trust-policy allowlist --trust-owner <id>... --join <token>`,
and it does not publish or auto-join unless asked. This tool is just the glue:

1. Authenticate to the relay (NIP-42) and read the roster plus members' discovery notes.
   When the relay's NIP-11 lists a `self` key, the tool only accepts a roster signed by it.
2. Admit owners from members' notes whose `ownerBindingSig` verifies (sha256(key) = ownerId,
   signed over the member's pubkey). Same rules as the desktop's `discovery.rs`.
3. Publish this node's own binding, then, once serving, the invite token as `serveTargets`,
   covered by `ownerEndpointBindingSig`. Heartbeat every 45s and withdraw on exit.
4. Poll the roster. Growth restarts mesh-llm with the new allowlist straight away; a shrink must be
   seen twice first, so one relay blip can't drop everyone (up to about a minute). The Buzz desktop
   ([buzz-app `mesh-compute`](https://github.com/block/buzz-app/blob/602944d9f/src/bundled/mesh-compute/compute/src/desktop/coordinator.rs))
   is quicker: it re-reads the roster every 15s and drops a removed member on the first read that
   leaves them out. Both restart only their mesh node, never Buzz. mesh-llm only reads
   `--trust-owner` at startup.
5. `client` picks a fresh, verified serve target from another member and runs
   `mesh-llm client --join <token>` with the same allowlist.

## Historical Buzz-only verification

These checks predate `--public`; they are not dual-mode validation.

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

## Local validation

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

The default suite does not load a model or contact a community. The opt-in real-model test
uses one SDK node with public discovery/relays disabled, exercises both local APIs, then
checks attachment growth/shrink, fail-closed replacement, detach and clean shutdown:

```sh
# Use a disposable HOME (especially on macOS, to avoid personal Keychain access).
# Supply an already installed compatible runtime and a local GGUF. No model download.
H=$(mktemp -d)
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" HOME="$H" \
  MESH_LLM_NATIVE_RUNTIME_CACHE_DIR=/absolute/path/to/test-native-cache \
  BUZZ_MESH_SMOKE_MODEL=/absolute/path/to/model.gguf \
  cargo test --locked -- --include-ignored --test-threads=1
rm -rf "$H"
```

This is local shared-serving/lifecycle coverage, **not** a real Buzz roster/discovery,
remote stranger-denial, cross-host NAT, public-mesh discovery or Buzz-agent acceptance test.

## Limits

- Agent keys that need a NIP-OA `auth` tag are not wired up; the node uses its own member key.
- The served model is listed as `local-gguf/sha256-…`, not the name passed to `--model`, so requests
  must use that id.
- Buzz-only mode still reloads the model on roster changes. `--public` uses the released
  v0.79.0 SDK `add_membership` API to replace only Buzz. The CLI `--memberships-config`
  shared-serving mode is a separate contract, not the integration used here.
- SDK attachments share the operator process, model execution capacity and owner/certificate
  revocations. They are not hostile-tenant sandboxing or per-community capacity reservations.
  Extra memberships accept unsigned invitations and do not carry split-stage, plugin or
  owner-control transports; the primary public node's settings still apply to those features.
- A member removed from the roster keeps its own nodes running until they notice; serving nodes
  stop admitting it either way. This tool's Buzz-only node stops and exits on its own removal;
  `--public` detaches only Buzz and continues public serving.
- The live checks above are historical **Buzz-only** checks. They do not verify the new dual
  mode. Dual-mode local checks and any outstanding live-validation limits belong in the PR.
