# Design: a build farm and `imgless`

Status: **proposal**, not on the roadmap. Nothing here changes SPEC.md.

## Summary

`imgless` should be one command that takes a flake and gives back a running
workload:

```bash
imgless ./project              # a directory with a flake.nix
imgless github:owner/project   # a flake reference
```

That needs three things the repository does not have yet: a **build farm** that
turns a flake into signed store paths, a **publisher** that turns those paths
into a digest-addressed release, and a **CLI** that ties authoring, building,
and applying into a single step. The node contract stays as it is. The shim
already runs everything this design produces: embedded seeds, external
references, and releases (SPEC §3, §6).

Cache-only nodes are **one posture among several, not the destination**. The
farm makes cache-only nodes practical, and it also makes evaluating nodes
cheaper, because an evaluating node that substitutes from the farm's cache
rarely builds anything itself. Operators choose a posture for each node pool,
and `imgless` adapts to whatever the target pool accepts.

## Why this sits outside the core

The roadmap is explicit that "the product is the shim and the spec", and it
lists a release publisher as an idea that is *not* on the roadmap. That still
holds. This design adds components **beside** the shim and changes nothing
inside it:

- The farm and the publisher are optional. Any CI that copies a closure to a
  cache and emits the manifest already conforms (SPEC §6). The farm is simply
  a good implementation of that CI.
- `imgless` is a client. Like `kubectl-imageless`, it has no Nix and needs
  none. It grows out of that plugin rather than competing with it.
- Nodes never learn that a farm exists. They see a signed cache and a manifest,
  or a flake to evaluate, exactly as they do today.

## Goals

1. `imgless <dir|script|flake-ref>` builds, publishes, and deploys in one
   command, and prints where the workload is reachable.
2. No Nix on the client, and no Nix build on cache-only nodes.
3. A one-line edit redeploys in time bounded by recompiling the changed code,
   not the whole dependency tree (incremental builds, below).
4. Multi-architecture builds with no per-developer setup.
5. The same artifacts serve the dev loop and production. Promotion means
   pointing at a digest, not rebuilding.

## Non-goals

- Changing the node contract, annotation set, or manifest schema. Where this
  design would want a manifest field, it uses the existing `provenance` and
  `sbom` evidence references.
- Replacing Hydra, buildbot-nix, or Hercules. The farm has an adapter
  seam so any of them can be the CI front door.
- A hosted service. Everything here is self-hostable. Garnix's code being open
  source is prior art, not a dependency.

## Background: what exists today

| Mode | Annotation | Who builds | Node policy |
|---|---|---|---|
| Embedded seed | none (auto-discovered `etc/imageless/flake.nix`) | the node | `cache_only: false`, `path:` allow-listed |
| External ref | `run.imageless.source` | the node | `cache_only: false`, ref prefix allow-listed |
| Release | `imageless.run/release-v1` | whoever published it | issuer, name pattern, and cache allow-listed; works under `cache_only: true` |

`kubectl-imageless` already drives all three from a Nix-free client. It packs a
directory or generates a seed from a `nix`-shebang script (`run <dir|script>`),
emits pinned external references (`run --external`), and resolves a channel to
a release digest (`pin`, `run --release`). `doctor` reports whether a cluster
is prepared.

What is missing is the step in between: *build this somewhere other than the
node, then deploy the result*.

## Node postures

A posture is a node pool's `policy.json`. A cluster can have several pools, and
a workload chooses one through the RuntimeClass `nodeSelector`, as it does
today.

| Posture | Policy | What the node does | Use it for |
|---|---|---|---|
| **Evaluating** | `cache_only: false`, prefixes allow-listed, node `nix.conf` substitutes from the farm cache | Evaluates, then *substitutes* from the farm; builds only on a cache miss | Dev clusters, single-tenant nodes, the zero-config embedded-flake experience |
| **Cache-only** | `cache_only: true`, issuers configured | Fetches manifest, realises signed paths from the one allow-listed cache | Shared and production nodes, managed Kubernetes, anywhere evaluation is not acceptable |
| **Mixed cluster** | One pool of each | Both | The recommended shape: iterate on evaluating nodes, promote to cache-only ones |

The line that makes evaluating nodes cheap is the node's own substituter
configuration. Pointing it at the farm cache means a node evaluating a flake
the farm already built downloads the build instead of repeating it. The shim
needs no change for this.

## Architecture

```text
               ┌───────────────────── imgless (client, no Nix) ─────────────────────┐
               │ classify input → pin → choose path → submit/emit → apply → report  │
               └───────┬──────────────────────────┬──────────────────────┬──────────┘
                       │ packed seed or pinned ref │                      │ pod/Deployment
                       ▼                           │                      ▼
   ┌────────────── Builder service ──────────────┐ │          ┌───────── Cluster ─────────┐
   │ queue, per-project chain heads, auth        │ │          │ evaluating pool           │
   │ nix build --builders <farm> .#rootfs        │ │          │   evaluates, substitutes  │
   │   (--override-input for incremental chain)  │ │          │   from the farm cache     │
   └───────┬─────────────────────────────────────┘ │          │ cache-only pool           │
           │ store paths per system                │          │   realises release paths  │
           ▼                                       │          └───────────▲───────────────┘
   ┌─ Farm: remote builders (x86_64, aarch64) ─┐   │                      │ signed NARs
   └───────┬───────────────────────────────────┘   │                      │
           │ nix copy (signed)                     │                      │
           ▼                                       │                      │
   ┌─ Binary cache ─┐ ◄── optional in-cluster pull-through mirror ────────┘
   └───────┬────────┘
           ▼
   ┌─ Publisher ─┐ → catalog: sha256/<digest>.json, refs/<name>/<channel>
   └─────────────┘

   CI front door (Hydra / buildbot-nix / Hercules) ──► same farm, same cache, same publisher
```

### Components

**`imgless` CLI.** It grows out of `kubectl-imageless`, reusing its packing,
bounds, registry auth, pinning, and shebang generation. New responsibilities:

- Classify the argument: a directory, a `nix`-shebang script, a flake
  reference, or `owner/project` shorthand for GitHub.
- Pin unpinned references on the client. For `github:owner/project`, resolve
  the default branch head through the forge API into `github:owner/project/<sha>`.
  This needs no Nix. The node's rule that it does not police pins stays as it
  is; the client does that job, as it already does for `--external`.
- Choose a path: *release* if the target pool is cache-only or the user asks for
  it, and *node evaluation* otherwise. `doctor` already reads the RuntimeClass
  and node labels. It gains a posture probe (below).
- Emit a Deployment plus a Service, not a bare Pod, with an optional HPA.
  `--pod` keeps today's behaviour.
- Default the command from release `process` metadata when the flake provides
  it, so `-- COMMAND` becomes optional.

**Builder service.** A small stateless API in front of the farm, plus one piece
of state: the incremental chain table.

```text
POST /v1/builds
  source:  { seed: <oci digest in registry> } | { ref: <pinned flake ref> }
  output:  "rootfs"                     (default)
  systems: ["x86_64-linux", "aarch64-linux"]
  chain:   { project, branch }          (optional; enables incremental)
  publish: { issuer, name, channels }   (optional; otherwise paths only)
→ 202 { build_id }
GET  /v1/builds/<id>        → state, per-system store paths, timings
GET  /v1/builds/<id>/log    → streamed build log
```

The builder evaluates and builds with ordinary Nix, `--builders` pointed at the
farm. It signs with the farm key (or the cache signs on upload), copies to the
cache, and hands the paths to the publisher. It reuses the node's staging
bounds for seeds, so a seed the node would refuse is refused here as well.

**Farm.** Plain Nix remote builders listed in a machines file. Nothing is
imageless-specific. Hydra's machines file is the same format, so one farm can
serve both front doors.

**Cache.** Any Nix binary cache: S3-compatible storage, Attic, or a local
directory for air-gapped use. The signing key lives on the farm. The public key
goes into node policy `issuers.<i>.caches.<c>.public_keys` for cache-only
pools, and into `nix.conf` `trusted-public-keys` for evaluating pools.

**Publisher.** A small library and CLI with this contract:

```text
publish(issuer, name, { system → rootfs path }, cache_id, process?, evidence?, channels?)
  → issuer/name@sha256:<digest>
```

Its output matches `nix/release-catalog.nix` byte for byte: canonical JSON with
sorted keys, and pointer files holding bare hex. This is the only seam every CI
front door calls, so switching Hydra for buildbot-nix never touches the
catalog.

**CI front doors.** A Hydra RunCommand hook, a buildbot-nix effect, or a
Hercules effect runs on success of an aggregate job over `rootfs.<system>` and
calls the publisher. That is the whole adapter. Because the paths are
content-addressed, `imgless` deploying a commit that CI already built is a
cache hit end to end.

## `imgless` user experience

```bash
# Dev loop against the cluster in the current context.
imgless ./api                       # pack → build on farm → deploy → print URL
imgless ./api --watch               # rebuild and roll on file change
imgless ./tool.py -- --port 8080    # nix-shebang script, as kubectl-imageless does today

# From a forge. Pinned on the client, built on the farm.
imgless github:acme/api             # default branch head
imgless github:acme/api/v1.4.2      # tag or commit
imgless acme/api                    # GitHub shorthand

# Choosing the path explicitly.
imgless ./api --on-node             # skip the farm; the evaluating pool builds (today's run)
imgless ./api --release             # force the release path, even on an evaluating pool

# Promotion: no rebuild, just a digest.
imgless promote acme/api@sha256:… --channel stable
imgless deploy  acme/api:stable --pool prod    # resolves the channel on the client, pins the digest
```

Path selection, in order:

1. An explicit flag (`--on-node`, `--release`) wins.
2. If the target pool is cache-only, use the release path. Without a
   configured builder this is an error that names the missing piece.
3. If the pool is evaluating and a builder is configured, use the release
   path (the farm builds it, the node just downloads it). Otherwise use node
   evaluation.

Rule 3 prefers the farm even on evaluating pools. The result is identical, the
build is shared with every other node and developer, and the node stays
lightly loaded. `--on-node` remains as the zero-infrastructure path.

**Posture probe.** The node-local half of the configuration has no Kubernetes
API representation (`doctor` already reports `node-config` as a permanent skip).
Pools advertise their posture instead, with a node label set by the NixOS
module or the installer, for example `imageless.run/posture=cache-only|evaluating`.
Treat it as a hint for choosing a path, never as an authorization: the node
still enforces its own policy, and a wrong label produces a clear create-time
refusal, not a silent misbuild.

## Incremental builds (Garnix-style)

The largest term in a redeploy is compilation (see `dev/bench`). Splitting
dependencies into their own derivation removes the dependency part of that
term. Garnix's incremental chains remove most of the rest by feeding the
previous build's compiler state forward.

**Contract with the flake author (opt-in):**

- The package declares an extra output, `intermediates`: object files,
  compiled interfaces, or Cargo's `target/`.
- The flake declares an input (name to be settled; Garnix's open-sourced code
  is the reference) that defaults to an empty tree.
- `preBuild` copies the input's intermediates into place. `postInstall` writes
  the new ones to `$intermediates`. Tools that track changes by modification
  time (Cargo, Make) need timestamps restored for unchanged sources.
  Hash-based tools (GHC 9.4 and later) do not. nixpkgs `checkpointBuildTools`
  overlaps and may be enough on its own.

**What the builder does:**

- It keeps a table `(project, branch, system) → last good intermediates path`
  and holds each entry as a GC root on the farm.
- It builds with `--override-input <input> path:<intermediates>` and advances
  the chain head only when the build succeeds.
- It records the intermediates path it used in the manifest's `provenance`
  evidence, which partly answers the roadmap item on reproducing a release
  after cache eviction.

**Why this is acceptable when node-side build retention was not.** The roadmap
rejects a mutable cache on the node for two reasons: output would depend on
hidden node state, and one tenant's artifacts would become another tenant's
build input. A chain avoids both:

- The previous intermediates are a named store path passed as an input, not
  ambient state. The build is still a function of its inputs.
- Chains are scoped by project and branch on the farm, never shared across
  tenants and never on a node. Per-tenant partitioning kills the hit rate of a
  shared node cache. It does not hurt here, because the useful hit is always
  the developer's own previous build.

**Guard rails:**

- Incremental output is *equivalent* to a clean build, not guaranteed to be
  bit-identical, and it is brittle across compiler upgrades. Chains are for the
  dev loop. Releases promoted to a production channel come from a **clean
  build** by the CI front door, which is the natural place for one.
- A chain resets automatically when its lock-file inputs change (a new
  toolchain or nixpkgs), and on demand with `imgless ./api --clean`.
- A failed incremental build is retried once from a clean state before it is
  reported as a failure, so a poisoned chain costs one slow build, not a stuck
  project.

## Cold start and autoscaling

- **Warm node, more replicas.** Paths are already valid, so a create costs a
  GC-root registration and a `root.path` rewrite. This is likely faster than an
  image pull, but it has not been measured, and it should be measured.
- **Cold node.** The first pod of each workload downloads its whole missing
  closure *inside* `runc create`. That is subject to kubelet's
  `--runtime-request-timeout` (default 2 minutes). Unlike an image pull, it is
  not a separate, visible phase. Mitigations, cheapest first:
  1. An in-cluster pull-through cache mirror, so downloads run at LAN speed and
     a scale-out event triggers one upstream fetch instead of N.
  2. Prewarming on node join, through the existing `ResolvePurpose::Prewarm`,
     fetching the pool's hot releases before the node is marked schedulable.
  3. A base store baked into the node image (libc, common runtimes).
  4. Closure-aware placement: `inspect` already reports
     `missing_download_bytes`. The roadmap files this under "measured before
     built", and this design keeps it there.
- **Scale to zero** (Knative or KEDA) is viable on warm nodes, and its cold
  start is the closure download. Small static services start fast. Large
  Python or ML closures are slow here as they are with images.
- **The farm scales too.** The builder queue drives the remote-builder count.
  Spot or preemptible instances suit it, because a lost build retries and a
  finished path is never lost.

## Cost model

| Line item | Scales with | Lever |
|---|---|---|
| Farm compute | Changed derivations only | Nix incrementality, chains, spot instances, scale to zero |
| Cache storage | Closure size × releases retained | A retention policy on the cache. Not solved today; see open questions |
| Cache egress | Bytes missing on cold nodes | An in-region cache, a zero-egress store, the in-cluster mirror |
| Node disk | Unique store paths per node | Sharing per store path (not per layer) makes this cheaper than images |
| Node CPU at create | Download and unpack (cache-only); plus evaluation (evaluating) | Farm-first path selection keeps evaluating nodes close to cache-only cost |

## Security and trust

- **The node trust boundary does not change.** A cache-only node trusts one
  thing: a path signed by a key in its policy, from the cache its policy names
  for that issuer.
- **The farm becomes the trusted computing base for releases.** Whoever holds
  the signing key chooses what runs. Keep the key on the farm, not in CI
  secrets reachable from pull-request builds.
- **Manifests are integrity-checked, not authenticated.** The roadmap's
  detached manifest signatures matter more once publishing is automatic.
  Without them, write access to the catalog is equivalent to deploy access.
  This design should not reach production before that roadmap item lands.
- **Garnix-style FOD verification belongs on the farm:** re-fetch
  fixed-output derivations and check their hashes before signing. The result is
  recorded as evidence and never re-checked on nodes.
- **Tenancy.** Builds from different tenants run in separate sandboxes, as
  Nix already does. Chains are never shared across tenants. Pull-request
  builds from forks never sign with the release key.

## Where it runs

The farm and builder run anywhere Nix does: NixOS, or any Linux with Nix
installed, in or out of the cluster. The node side is unchanged and keeps the
constraints found earlier. Evaluating and cache-only pools today both need
`nix-store` and a writable `/nix/store` on the host, and a containerd handler.
A separate track makes cache-only pools Nix-free by giving the shim its own
downloader (narinfo signature check, NAR fetch and unpack, a reference-counted
store that need not live at `/nix` on the host). Paired with a supported way
to add the containerd handler, such as Bottlerocket settings, a Talos system
extension, or the installer DaemonSet the roadmap lists as an idea, that
reaches managed Kubernetes. The farm design does not depend on that track, and
that track does not depend on the farm.

## Phased plan

Each phase ends on a measurement or a demo, in keeping with the roadmap's
"measured before built" rule.

| Phase | Deliverable | Exit criterion |
|---|---|---|
| **M0** | An `incremental` variant in `dev/bench`: a Rust seed with an `intermediates` output, fed back by `--override-input` | `incremental`/`edit` compared with `split`/`edit`. If the gain is marginal, drop chains from M2 |
| **M1** | Publisher library and CLI, byte-compatible with `nix/release-catalog.nix` | A published manifest resolves on the existing cache-only smoke |
| **M2** | Builder service (no chains yet) plus `imgless ./dir` emitting a Deployment on kind | Edit, then `imgless`, then new pod serving, on an unmodified `dev/kind` cluster |
| **M3** | Forge references and client-side pinning, path selection, posture label | `imgless github:owner/project` works against a cache-only pool and an evaluating pool |
| **M4** | Incremental chains in the builder (if M0 justifies them), `--watch`, `--clean` | Redeploy time within a small factor of the incremental bench cell |
| **M5** | CI front-door adapter (buildbot-nix or Hydra) calling the publisher; `promote` | A push builds clean on CI, and promotion is a pointer move with no rebuild |
| **M6** | In-cluster mirror and prewarm on node join | Cold-node p95 create time under a stated budget for a reference closure |

The Nix-free downloader and the managed-Kubernetes installer are a separate
track that can run alongside any of these phases.

## Open questions

1. **Name.** `imgless`, `imglss`, or a subcommand of `kubectl imageless`. Do
   we keep one binary with a `kubectl-` alias, or two?
2. **Where the builder runs.** In-cluster (simple to operate, but it competes
   with workloads) or beside the cluster (cleaner isolation). The farm itself is
   almost certainly outside.
3. **Cache retention.** Which releases and chain heads are kept, and who
   decides? Neither Nix nor this design has a GC policy for a remote cache.
4. **Garnix's input convention.** Adopt their special-input name and output
   layout as-is, so flakes written for Garnix work unchanged, or define our own?
   Compatibility is worth a lot now that the service has shut down and its
   users are migrating.
5. **Source upload for directories.** Push the packed seed to the OCI
   registry (reusing `kubectl-imageless` code, and the node can also consume
   it) or upload it straight to the builder (fewer moving parts)?
6. **Build secrets and private inputs.** Private flake inputs need forge
   credentials on the farm, never on the client or the node. Scope them per
   project, and keep them out of anything a pull request can trigger.

## References

- SPEC.md §3 (annotations), §6 (release profile)
- ROADMAP.md: "Measured before built" and "Ideas"; incremental build retention,
  the installer DaemonSet, manifest signatures
- `dev/bench/README.md`: the split-derivation pattern and the redeploy cost breakdown
- `nix/release-catalog.nix`: canonical manifest and channel pointers
- Garnix, "Incremental builds in Nix and garnix": https://garnix.io/blog/incremental-builds/
- NixOS Discourse, "Incremental builds in garnix": https://discourse.nixos.org/t/incremental-builds-in-garnix/56100
- Garnix, "garnix is joining Shopify" (service shutdown and open-sourcing): https://garnix.io/blog/shutting-down/
