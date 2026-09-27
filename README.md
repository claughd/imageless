<p align="center">
  <img src="https://raw.githubusercontent.com/dmadisetti/imageless/HEAD/assets/imageless-logo.svg" width="150"
       alt="The imageless logo: a Polaroid photo dissolving into an empty dashed wireframe">
</p>

<h1 align="center">imageless</h1>

<p align="center"><b>Ship the flake, not the filesystem.</b></p>

<p align="center">An OCI image that carries a Nix flake in its layers bootstraps its own root
filesystem at container-create time — through stock Docker, containerd, and
Kubernetes.</p>

```text
seed image layer:  /etc/imageless/flake.nix  (+ lock + sources)
        │
        ▼  docker run / kubelet → containerd → runc create
imageless-runc interposes `create`
        │  realize flake#rootfs into /nix/store   (bounded, GC-rooted)
        │  atomically rewrite config.json root.path
        ▼
stock runc starts the realized rootfs
```

The image you push is an ordinary OCI image — registries, digests, admission
control, `docker run`, air-gapped mirrors all keep working. Like the Polaroid
above, it just doesn't contain the picture: it contains the *recipe*, and the
node's Nix store (or binary cache) develops the result.

imageless is two things, in this order:

1. **A shim** — `imageless-runc`, a runc-compatible interposer you register as
   a Docker runtime or a containerd `BinaryName`. No custom containerd shim, no
   kubelet changes, no image-format extension.
2. **A spec** — [SPEC.md](https://github.com/dmadisetti/imageless/blob/HEAD/SPEC.md), the contract for embedded flakes,
   annotations, materialization bounds, atomic rewrite, store projection, and
   GC-root lifecycle. Any OCI runtime can implement it; a Rust library
   (`imageless`) is provided for runtimes that want to link it instead
   ([Cowboy's](https://github.com/dmadisetti/cowboy) `cowboy-runtime` does
   exactly this).

## Quick start (Docker)

```bash
nix build .#imageless-runc

# 1. Register the runtime
# /etc/docker/daemon.json: {"runtimes": {"imageless": {"path": "/path/to/imageless-runc"}}}

# 2. Opt the node into evaluating embedded flakes. The shipped default is
#    cache_only: true (the node evaluates nothing); on a dev box you almost
#    always want the opt-in — examples/dev-policy.json sets "cache_only": false
sudo install -Dm600 examples/dev-policy.json /etc/imageless/policy.json

# 3. Build and load a real seed image. `nginx-embedded-image` is a ~2 KB
#    image whose entire contents are examples/nginx-embedded/{flake.nix,flake.lock}
docker load < "$(nix build .#nginx-embedded-image --print-out-paths)"

# 4. Run it. nginx does not exist in the image; the node materializes it.
docker run --rm --runtime=imageless --network=host --tmpfs /tmp \
  localhost/imageless-nginx-embedded:e2e
curl -s http://127.0.0.1:18080/   # => imageless-nginx-ok
```

**How the runtime found the flake:** nothing above passes an annotation. A
plain regular file at `etc/imageless/flake.nix` in the image's rootfs is
auto-discovered and selects the container, with source `/etc/imageless` and the
runtime's default output (`rootfs`, see `IMAGELESS_DEFAULT_OUTPUT`). That is
the zero-config path — an image with no such file is passed straight through to
stock runc, untouched. Deployer-side overrides (a different source, a different
output, per-container selection) are annotations only. Naming a *source* — a
release or an external flake reference — replaces discovery; the output and
per-container selectors refine whatever was discovered rather than suppressing
it. See [SPEC.md](https://github.com/dmadisetti/imageless/blob/HEAD/SPEC.md) for the annotation set.

No daemon. The shim materializes in-process by node policy. Multi-tenant
nodes can instead run the optional `imageless-resolver` daemon (selected via
`IMAGELESS_RESOLVER_SOCKET`) for node-wide concurrency caps, single-flight
coalescing of identical realizations, and privilege-separated evaluation.

`dev/docker/` runs exactly this against a throwaway Docker daemon that touches
nothing under `/etc` — the recommended way to try it on a machine you care
about. The hermetic end-to-end proof lives in `nix build .#docker-embedded-smoke`
(a NixOS VM test; it needs KVM): the seed's layer deliberately lacks the
executable that produces the expected HTTP response — a successful response
proves the rootfs was materialized from the flake, not shipped in the image.

## Kubernetes

Register a `RuntimeClass` backed by the **stock** runc-v2 shim with its OCI
runtime pointed at the interposer:

```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.imageless]
  runtime_type = "io.containerd.runc.v2"
  pod_annotations = ["imageless.run/*", "run.imageless.*"]
  container_annotations = ["imageless.run/*", "run.imageless.*"]
  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.imageless.options]
    BinaryName = "/path/to/imageless-runc"
```

Why this seam and not a custom containerd shim: under containerd 2.x, all of a
pod's containers are grouped into a **single** shim process, and app containers
are created by `Task.Create` RPC inside it — a custom runtime-v2 start/delete
binary only ever observes the pod sandbox. `runc create`, by contrast, runs
once per container in every containerd generation. Interposing there is
version-agnostic and is what the hermetic CRI VM test
(`nix build .#imageless-cri-vm`) validates: sandbox passthrough, per-container
selection, recreate, GC-while-running, delete-and-collect, and reboot recovery
on a real containerd/CRI node.

See `examples/` for the RuntimeClass, pod, and containerd configs, and the
NixOS module (`nixosModules.imageless`) for a packaged node setup. To try
all of this locally, `dev/kind/` stands up a throwaway
[kind](https://kind.sigs.k8s.io/) cluster wired for imageless — the
five-minute path from nothing to a running flake on real Kubernetes,
without touching the host.

### The kubectl plugin

`kubectl-imageless` gets a directory with a `flake.nix` onto a prepared
cluster with **zero Nix on the client** — the packing is a plain deterministic
tar and the evaluation happens on the node, under node policy:

```bash
nix build .#kubectl-imageless   # `kubectl imageless …` once result/bin is on PATH

kubectl imageless run ./app --repo registry.example/team/app \
  -- /bin/server --port=8080 | kubectl apply -f -
```

This packs `./app` into a seed OCI image under the same staging bounds the
node enforces (so a refusal happens at authoring time, with the offending path
in hand), pushes it to `registry.example/team/app` by digest, prints the
layer/config/manifest digests on stderr, and writes a digest-pinned pod
manifest on stdout — everything but the pod manifest stays out of stdout's
way. `--dry-run` stops before the push and needs no network. The node
materializes the root read-only, so `--writable PATH` (repeatable) gives the
pod an emptyDir at each path the workload writes, such as `/tmp`. Credentials come
from `docker login` (config.json `auths` and credential helpers; Basic and
Bearer auth). Loopback registries like kind's `localhost:5001` are plain HTTP
automatically; everything else is HTTPS unless you pass `--plain-http`, which
also consents to sending that registry's credentials in cleartext (with a
warning), to that host and no other. Some
registries garbage-collect untagged manifests (GHCR, ECR lifecycle policies) —
`--tag` adds a tag to protect the push while the pod reference stays
digest-pinned.

`--external` deploys a flake *reference* instead, for nodes whose policy
enables it:

```bash
kubectl imageless run --external \
  github:myorg/agent/0123456789abcdef0123456789abcdef01234567 \
  --repo registry.example/team/placeholder \
  -- /bin/agent | kubectl apply -f -
```

Nothing is packed and nothing is evaluated on the client; the pod carries
`run.imageless.source` and the node fetches and builds it under its own policy.
Kubernetes still needs an image to create the container from, so a content-free
placeholder is pushed to `--repo`. That image's only content is a flake that
`throw`s: the source annotation always takes precedence over an embedded flake,
so evaluating it means the annotation never arrived, and the throw turns the
one silent failure mode this design has into a create-time error that names its
own cause. `--image` names an image the cluster can already pull and skips the
push entirely, which needs no registry credentials and no network.

The reference must pin something — `?rev=`, `?narHash=`, or the
`github:owner/repo/<commit>` form — or the command refuses it and names the
pin; `--unpinned` overrides that for a scratch cluster. The node deliberately
does not police pin forms (SPEC §3), which is exactly why the authoring tool
does.

External mode needs two things the packed path does not, and the shipped
quickstart supplies neither:

- **`run.imageless.*` in the containerd handler's `pod_annotations` *and*
  `container_annotations`.** The examples and `dev/kind/kind-config.yaml`
  allow-list only `imageless.run/*` — the production release contract — so a
  `run.imageless.source` annotation is filtered out before the shim ever sees
  it, and the placeholder image's flake fails the create with exactly that
  diagnosis. `nixosModules.imageless` already passes both families.
- **A policy prefix covering the reference.** `examples/dev-policy.json`
  allow-lists `path:` alone, which is what an embedded flake needs and nothing
  more; `examples/external-refs-policy.json` is the one to copy, and the
  `github:yourorg/` in it is a placeholder to replace. Author prefixes to a
  boundary — an unterminated `github:myorg` also authorizes
  `github:myorg-evil/anything`.

`--release` deploys the third mode: a digest-addressed release the node resolves
against its own issuer catalogs, with no evaluation anywhere. Digests are for
machines, so the client resolves a channel name to one for you:

```bash
# Print the pinned reference and nothing else, so it composes.
kubectl imageless pin example/agent --catalog https://releases.example.com
# -> example/agent@sha256:3061…5072

# Or go straight to a pod, pinning on the way.
kubectl imageless run --release example/agent \
  --catalog https://releases.example.com \
  --image localhost/imageless-placeholder:v1 \
  -- /bin/agent | kubectl apply -f -
```

A coordinate is `issuer/name[:channel]`, defaulting to `:stable`. The catalog is
a local directory or an HTTPS base URL — `http://` is refused, since a pointer
read over plain HTTP chooses what a cluster runs. **The channel never reaches
the pod**: the manifest records the digest the channel pointed at, so
republishing a channel changes what the next `pin` returns and cannot change
what an admitted pod runs. This is also why `--catalog` has no default —
resolving against a catalog nobody named is how you deploy something nobody
chose.

`--catalog` is a *client-side* convenience that the node contract does not know
about. SPEC §6 requires nodes to ignore the `refs/<name>/<channel>` index
entirely; a node accepts digest-addressed references only, and node-side
resolution of a mutable pointer is non-conforming. Publishers building a catalog
with `nix/release-catalog.nix` get the index from a `channels = [ "stable" ];`
argument and the same pinned string as `passthru.reference`.

`--release` refuses to combine with `--external` (SPEC §3 makes the two
annotation families mutually exclusive) or with `--output` (a release manifest
names its own rootfs, so the node never reads an output annotation and a pod
claiming one would be quietly wrong).

`doctor` reports whether a cluster is prepared at all:

```bash
kubectl imageless doctor --context kind-imageless
kubectl imageless doctor --policy examples/external-refs-policy.json \
  --source github:yourorg/agent/0123456789abcdef0123456789abcdef01234567 --json
```

It checks the RuntimeClass, the node label that RuntimeClass schedules on (read
from the RuntimeClass, never assumed), which nodes carry it, and — offline, so
they work with no cluster at all — a policy file and a flake reference against
each other. The node-local half of the seam has no API representation, so
`node-config` is a permanent skip and the report says outright that green is
not proof a pod will start. Exit 3 means "could not look", which is a different
answer from exit 1 "a check failed". kubectl's connection flags are forwarded
verbatim, but they must come *after* `imageless`: kubectl stops collecting the
plugin command path at the first flag, so `kubectl --context x imageless doctor`
never reaches the plugin.

## Why not containix?

[containix](https://github.com/atmask/containix) proved the appeal of
flake-launched containers on Kubernetes, and its product shape — one obvious
use case, one interception point, stock runc afterwards — is the right one.
imageless exists because we need a different set of guarantees:

| | containix | imageless |
|---|---|---|
| **Where the flake lives** | A flake *reference* in pod metadata; the node fetches and builds whatever the annotation points at. | **In the image layers.** The deployable artifact is self-contained and content-addressed; registries, digest pinning, admission policy, and air-gapped nodes work unchanged. Pointing at an external flake ref is *also* supported — but as a mode the node's policy must explicitly enable, not the default trust model. |
| **Interception seam** | A containerd TTRPC shim wrapping `Task.Create` — coupled to containerd's shim interfaces and version, Kubernetes-only. | The `runc create` CLI seam (or direct library linkage in your runtime) — works for raw Docker, containerd 1.x and 2.x, and any CRI, with no TTRPC surface to track. |
| **Trust and policy** | The node builds what workloads name; isolation/allowlisting is future work. | Node-owned policy: evaluation is **off by default** (`cache_only`), an enabled node still evaluates only URI prefixes its policy allow-lists, staged sources are size/entry-bounded and symlink-free, and production nodes resolve only digest-addressed releases from allow-listed issuers and caches. Privilege separation is opt-in: see below. |
| **Lifecycle correctness** | Store GC is delegated to the operator. | GC roots are tied to the container: registered at create, released on failure or delete; a live container survives `nix-collect-garbage`. Atomic spec rewrite, bounded materialization with process-tree kill, fail-closed validation. |
| **Scope** | An experimental tool. | A spec with a reference shim, acceptance gates (Docker embedded-layer proof + CRI lifecycle VM test), and a library for embedding into other OCI runtimes. |

If you want "pod annotation → flake ref → container" with minimal machinery,
containix is simpler. If the artifact of record must remain an OCI image and
the node must decide what it will and won't build, that is imageless.

## Who runs the evaluation

Be precise about this, because the two deployment shapes differ:

- **Daemonless (the default).** `imageless-runc` materializes in-process and
  evaluation runs **as the calling process** — under Docker or containerd that
  is root. There is no privilege drop, no rlimit, and no environment scrub on
  this path. What bounds it is policy, not privilege: evaluation is off unless
  the node's `policy.json` sets `cache_only: false`, and even then only URIs
  matching `eval_allowed_uri_prefixes` are evaluated, under a wall-clock
  timeout with process-tree kill. Appropriate when every workload on the node
  is already trusted with root — a dev box, a single-tenant node.
- **Daemon (`imageless-resolver`, opt-in via `IMAGELESS_RESOLVER_SOCKET`).**
  The daemon refuses to enable evaluation at all without
  `--development-worker`/`--development-worker-user`, and runs every evaluation
  through that worker: a separate unprivileged user, CPU rlimit, and a cleared,
  explicitly reconstructed environment. This is the posture for multi-tenant
  nodes, and it also supplies node-wide concurrency caps and single-flight.

Release-profile materialization (digest-addressed, from allow-listed issuers
and caches) substitutes rather than evaluates and is the same on both paths.

## External flake references

The embedded flake is the default trust model, but a pod may instead point the
node at a flake it does not carry: set `run.imageless.source` to a flake
reference with an explicit remote scheme (`github:`, `git+https:`,
`tarball+https:`, …). The node opts in per prefix — this is evaluation, so it
needs `cache_only: false` *and* the reference's prefix allow-listed:

```json
{
  "system": "x86_64-linux",
  "cache_only": false,
  "eval_allowed_uri_prefixes": ["path:", "github:yourorg/"],
  "issuers": {}
}
```

(`examples/external-refs-policy.json`; `path:` keeps embedded flakes working —
it authorizes the runtime's own rewrite of staged in-image sources, never a
node path named by an annotation.) Two rules save real pain:

- **Terminate prefixes at a boundary.** Matching is a literal string prefix:
  `github:yourorg` also authorizes `github:yourorg-evil/anything`; write
  `github:yourorg/`.
- **Pin anything you would redeploy.** A mutable ref is not a deployment
  identity — the same annotation can materialize different software tomorrow.
  Pin with an explicit revision or content hash, and remember the referenced
  flake's *own* unlocked inputs still resolve at evaluation time (its committed
  `flake.lock` is honored; pinning the top-level ref pins nothing else):

```yaml
# examples/pod-external-ref.yaml — the annotation, pinned
run.imageless.source: "github:yourorg/yourapp/8b7c95329f0a143adf971a0f27a4a0af8ddf9d5b"
run.imageless.output: "rootfs"
```

The node does not police pin forms — that is authoring tooling's job — and
production deployments should not ride this mode at all: they resolve
digest-addressed releases (`imageless.run/release-v1`), where identity is a
digest, not a ref. External references are the development and trusted-cluster
convenience, gated so a node operator can keep them off entirely.

## Embedding the library

The shim is a thin consumer of the `imageless` crate — argv parsing, telemetry
export, and a delegation to real runc; every decision it makes lives in the
library. A runtime that owns its `create` path can skip the interposer
entirely:

```rust
use imageless::{prepare_bundle, remove_bundle_gc_roots, PrepareBundle};
use std::path::Path;

/// Call during OCI `create`, once the bundle is staged.
fn create(bundle: &Path) -> std::io::Result<()> {
    // Defaults: output `rootfs`, 300 s timeout, and the materializer picked by
    // the environment — the daemon socket when IMAGELESS_RESOLVER_SOCKET is
    // set, in-process under the IMAGELESS_POLICY file otherwise. Override the
    // struct's fields to make any of that explicit.
    let request = PrepareBundle::new(bundle.join("config.json"), bundle);
    // Ok(None) => not an imageless bundle; proceed unchanged.
    let _applied = prepare_bundle(&request)?;
    Ok(())
}

/// Call on any later failure, and at `delete`.
fn cleanup(bundle: &Path) -> std::io::Result<()> {
    remove_bundle_gc_roots(bundle)
}
```

This block is compiled as a doc-test of the `imageless` crate
(`crates/imageless/src/lib.rs`, `ReadmeDoctests`), so it cannot drift from the
API.

## Environment variables

Every knob below is read at runtime by `imageless-runc` (and, where noted, the
resolver daemon or the kubectl plugin). The NixOS module sets the
operator-facing ones for you; the list is here so a hand-rolled deployment is
not guesswork.

Operator-facing:

| Variable | Default | What it does |
|---|---|---|
| `IMAGELESS_RESOLVER_SOCKET` | unset | Non-empty selects the daemon: materialization goes over this UNIX socket instead of running in-process. Unset (or empty) is the daemonless default. |
| `IMAGELESS_POLICY` | `/etc/imageless/policy.json` | Node policy file for in-process materialization. An explicit path *must* load or create fails closed; the default path is allowed to be absent, in which case a fail-closed `cache_only` policy is synthesized. The file must be owned by the uid the runtime runs as and not group/world writable. Ignored when a resolver socket is configured. |
| `IMAGELESS_STORE_PROJECTION` | unset (`node`) | `closure` binds only the selected release's closure, one read-only mount per store path — the hardened backend. Any other value keeps the whole-node `/nix/store` bind. |
| `IMAGELESS_REALIZATION_TIMEOUT_SECONDS` | `300` | Wall-clock bound on materialization, 1–3600. Out-of-range or non-integer values fail the create. |
| `IMAGELESS_DEFAULT_OUTPUT` | `rootfs` | Flake output used when the image carries no `run.imageless.output` annotation. |
| `IMAGELESS_TELEMETRY_PATH` | unset | Append per-phase timing events as ndjson to this file. Unset disables telemetry; write failures are ignored and never fail a create. See below for the stages. |
| `IMAGELESS_RUNC` | the stock `runc` baked in at build time (`runc` on PATH for an unbaked build) | The real OCI runtime to delegate to after the rewrite. Set this if you install `imageless-runc` *as* `runc`, so delegation cannot recurse through `PATH`. |
| `IMAGELESS_NIX` / `IMAGELESS_NIX_STORE` | baked at build time (`nix` / `nix-store` on PATH for an unbaked build) | The `nix` and `nix-store` binaries the materializer drives, in-process and in the daemon. `IMAGELESS_NIX_STORE` is also used for closure enumeration under `IMAGELESS_STORE_PROJECTION=closure`. |
| `IMAGELESS_SYSTEM` | `x86_64-linux` | Nix system double for the synthesized fail-closed default policy. A loaded policy file carries its own `system` and wins, so this only matters when no policy file exists. |

Each create appends one `imageless.timing.v1` event per stage, carrying the
release identity, the duration, and an outcome of `success` or `error`. The
stages come in two sets, and **anything summing them must pick one**:

| Stage | Meaning |
|---|---|
| `selection` | Reading `config.json` and deciding what, if anything, to materialize. |
| `policy_verification` | Authorizing the release against node policy — *and* fetching its manifest. |
| `substitution` | Everything from the end of selection to a materialized rootfs. |
| `rewrite` | Applying the result to the OCI spec. |
| `delegate_startup` | Handing off to the real runtime. |

Those five span the create end to end. The rest are *carved out of two of
them* — they explain where `policy_verification` and `substitution` went rather
than adding to them:

| Stage | Carved out of | Meaning |
|---|---|---|
| `manifest_fetch` | `policy_verification` | Fetching the release manifest: a network round trip on an HTTPS issuer. A slow catalog used to be indistinguishable from a slow policy check. |
| `staging` | `substitution` | Copying an embedded development source out of the image. Zero for a release, and for an external reference evaluated where it stands. |
| `evaluation` | `substitution` | The Nix process itself. The field most likely to explain a create that spent minutes. |
| `root_registration` | `substitution` | Registering the GC root. For a create that joined another's in-flight materialization, this is the whole of its own Nix cost, and the gap to `substitution` is what it spent waiting. |

A create that *fails* now appends a single `preparation` event with outcome
`error` and no release identity — there is none yet — where it previously
recorded nothing at all. The duration separates a fast refusal, like a policy
denial, from a create that burned its whole deadline.

### Failures the workload's owner can see

Telemetry is node-owned, and so is every other channel a failed create used to
write to: a pod that would not start said `CreateContainerError` and nothing
more, and the reason lived on the node. When the calling runtime passes both
`--log` and `--log-format json` — as containerd does on every invocation —
`imageless-runc` appends one JSON record to that log on a terminal create
failure. containerd reads it back and surfaces it as an event, so
`kubectl describe pod` shows the category and a curated diagnostic:

```text
Warning  Failed  kubelet  Error: imageless: architecture_mismatch: release
                          targets aarch64-linux; this node is x86_64-linux
```

The category is the same string the telemetry record carries, so a pod event
and a node timing record can be matched by eye. The diagnostic is *curated*:
where a message continues into node-local remedies — "write
`/etc/imageless/policy.json` to authorize…" — the relay stops at that boundary.
The tenant learns why their pod will not start; they do not learn the node's
filesystem layout, and there is nothing in the relayed line they could not have
worked out from their own pod spec.

The relay writes nothing on success, nothing on a passthrough bundle (SPEC §4.1
makes that a strict no-op, and a log line is observable), and nothing when the
runtime asked for the default text log format — a JSON record in a text log
would corrupt an operator's file to no one's benefit. Write failures are
ignored: a create that has already failed is not improved by failing
differently.

Client-side, read only by `kubectl-imageless`:

| Variable | Default | What it does |
|---|---|---|
| `IMAGELESS_KUBECTL` | `kubectl` | The kubectl `doctor` drives. A bare name is resolved on `PATH` and an absolute path is taken as given; a relative path containing a separator is refused, because it would execute whatever happens to sit there in the working directory. |
| `DOCKER_CONFIG` | `$HOME/.docker` | Directory holding the `config.json` the push reads registry credentials from — `auths` entries and credential helpers alike. Standard Docker/BuildKit semantics; listed here only because the plugin honors it without a Docker daemon anywhere in sight. |

`doctor` also reads `KUBECONFIG` and `KUBERNETES_SERVICE_HOST` — but only to
*report* which config or in-cluster service account is in play. Resolving them
is kubectl's job, and the plugin never second-guesses the answer.

Internal / development only:

| Variable | Notes |
|---|---|
| `IMAGELESS_POLICY_JSON` | Node policy inline as JSON, ≤1 MiB, trusted because the daemon environment set it. Compiled in **only** under the `inline-policy` cargo feature (`nix build .#imageless-dev`) and absent from a production binary. Exists so the dev Docker harness can run a root daemon with no policy file to own — see `dev/docker/README.md`. Ignored when `IMAGELESS_RESOLVER_SOCKET` is set. |
| `IMAGELESS_DEV_PATH`, `IMAGELESS_DEV_SSL_CERT_FILE` | Build-time only (`option_env!`, baked by `nix/package.nix`): the sanitized `PATH` and CA bundle handed to the privilege-dropped development evaluator, which otherwise runs with a cleared environment. Setting them at runtime does nothing. |
| `IMAGELESS_DOCKER_*`, `IMAGELESS_SMOKE_*` | Inputs to the acceptance smoke harnesses only (`crates/imageless-runc/tests/`, `smoke/`). Not read by any shipped binary. |

## Repository layout

- `crates/imageless` — the library: spec types, bundle planning/rewrite, store
  projection, GC roots, release-manifest parsing, resolver client.
- `crates/imageless-runc` — the shim (the product).
- `crates/imageless-resolver` — the optional hardened profile: the node
  materializer daemon and its privilege-dropped evaluation worker, for
  multi-tenant nodes that want central concurrency caps, single-flight, and
  evaluation in a separate privilege domain.
- `crates/kubectl-imageless` — the kubectl plugin: deterministic seed packing
  and pod authoring with zero client-side Nix.
- `SPEC.md` — the contract.
- `examples/`, `smoke/` — deployment examples and the acceptance smokes.
- `dev/bench` — the redeploy benchmark: what one edit costs on the real create
  path, decomposed into staging, Nix evaluation, and compilation.

## Status and limitations

- Linux only; a flake's `rootfs` output is per-system (no implicit
  multi-arch — publish per-platform images or releases).
- The node needs Nix (evaluation posture) or a reachable binary cache
  (cache-only posture).
- The spec is v1 **draft**: annotation namespaces are settled
  (`imageless.run/*`, `run.imageless.*`), but schema details may still change
  before freeze.
- Extracted from and battle-tested inside
  [Cowboy](https://github.com/dmadisetti/cowboy); now developed standalone at
  [github.com/dmadisetti/imageless](https://github.com/dmadisetti/imageless).

---

<sub>The logo's landscape is derived from
<a href="https://github.com/googlefonts/noto-emoji">Google Noto Emoji</a>
(U+1F3DE, Apache-2.0).</sub>
