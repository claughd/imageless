# The imageless contract — v1 (draft)

This document specifies how an OCI container image carries a Nix flake in its
layers and how a conforming runtime materializes that flake into the container's
root filesystem at create time. `imageless-runc` is the reference
implementation; any OCI runtime may implement this contract directly or by
linking the `imageless` library.

Status: draft. Identifiers (`run.imageless.*`, `imageless.run/*`,
`imageless.release.v1`) are stable in the reference implementation but the
spec may still renumber or renamespace them before v1 is frozen.

## 1. Terms

- **Image** — an ordinary OCI image. Nothing in this contract changes how
  images are built, pushed, pulled, or admitted.
- **Bundle** — the OCI runtime bundle (`config.json` + `rootfs/`) a runtime
  receives at `create`.
- **Materialization** — realizing a Nix installable to exactly one store path
  that becomes the container's root filesystem.
- **Conforming runtime** — the component that interposes on per-container
  `create`: the `imageless-runc` shim, or a runtime linking the library.
- **Materializer** — the component that performs the Nix work. It may run
  in-process inside the conforming runtime or as the optional
  `imageless-resolver` node daemon; the contract is identical either way.

## 2. The embedded flake (core contract)

An image opts in by carrying a flake at a conventional path in its layers:

```
rootfs/
└── etc/imageless/
    ├── flake.nix          # required for the zero-config path
    ├── flake.lock         # required when the flake has inputs (§2.3)
    └── ...                # any source files the flake references
```

### 2.1 Zero-config default

If `etc/imageless/flake.nix` exists in the bundle rootfs as a regular file, the
container is selected with:

- source: `/etc/imageless`
- output: `rootfs`

A `flake.nix` that exists there but is not a regular file (a symlink, a
directory) fails the create of any container that would have been selected. The
pod sandbox and containers the selectors skip pass through, as they would for
any image.

The installable is the canonical equivalent of
`path:<bundle-rootfs>/etc/imageless#rootfs`. The flake output must evaluate to
a derivation whose single output path is a usable root filesystem.

Note: materializing an embedded flake is node-side evaluation, which the
reference materializer ships **disabled** (`cache_only: true`, §6). Deployment
documentation and examples should show the opt-in (`cache_only: false`) —
enabling it is the expected configuration wherever embedded flakes are the
point — but the fail-closed default is deliberate and stays. The same opt-in
gates external flake references (§3): they are evaluation too, plus a prefix
allow-list of their own.

### 2.2 The flake is the metadata

There is no sidecar metadata file. The flake is code, so every image-side
degree of freedom is expressed *in the flake*: an image whose "real" output
lives elsewhere aliases it to the conventional name —

```nix
# flake.nix
outputs = { self, ... }: {
  rootfs = self.packages.x86_64-linux.my-actual-thing;
};
```

Deployer-side (not image-side) overrides are what annotations are for (§3).

### 2.3 Source staging

Before evaluation, the runtime stages a copy of the source tree out of the
bundle rootfs. The staged tree is bounded: at most 16 MiB and 4096 entries,
regular files and directories only (symlinks are rejected). The staged copy is
what the materializer evaluates; the container never controls paths outside its
own rootfs.

The rejection covers the path *to* the source as well as the tree under it. The
runtime reads the rootfs from the host, where an image symlink such as
`/a -> /` resolves against the node's root, so a source whose in-image path
passes through any symlink fails the create. Zero-config discovery (§2.1) does
not fail on a symlinked `etc` or `etc/imageless`; the image simply carries no
embedded flake there and passes through.

### 2.4 Evaluation confinement and locked inputs

Staging bounds what the installable names; it cannot bound what the flake's own
inputs name, and Nix fetches inputs — direct or transitive, `path:` and
`file://` among them — from whatever filesystem the evaluator can see. The
reference materializer therefore evaluates in private mount and PID
namespaces. The root holds only the store, the system program and library
trees, and the `/etc` entries Nix needs (its configuration, TLS roots, name
resolution, account lookup). It also holds the character devices Nix opens
(`null`, `zero`, `full`, `random`, `urandom`, `tty`), a private devpts
instance, a procfs mounted from inside the new PID namespace, a private
`/tmp`, the staged source, and the evaluator's fetcher and pending-root
scratch. The host's `/proc` and `/dev` are never exposed. In the host PID
namespace, `/proc/<pid>/root` would lead back to the node's filesystem. A
node-local input resolves against that root and reaches nothing of the
node's. The evaluator is PID 1 of its namespace, so nothing it spawns outlives
it. A node that cannot create mount namespaces must
say so in its policy (`unconfined_evaluation: true`); the default fails the
create instead. TLS roots outside the allowlist must be named through
`NIX_SSL_CERT_FILE`, which the evaluator binds.

An in-image flake evaluates against the lock it ships
(`--no-update-lock-file`): a flake with inputs and no complete `flake.lock`
fails the create, and a lock that pins a node-local input — an absolute or
`..`-escaping `path`, or any `file:` URL — is refused before evaluation, naming
the input. A `path` inside `/nix/store` locked by its `narHash` is not
node-local: it is immutable, world-readable content that Nix verifies before
use, and it is how a flake locked against a local nixpkgs checkout records
that input. Development nodes may set `allow_unlocked_inputs: true` to let the
node lock such a seed at evaluation time; confinement still applies. External
references (§3) are confined the same way; their own lock is honored as
written.

A seed evaluated against its own lock is a function of the staged tree, the
output, the node's system and the evaluator, so the reference runtime
memoizes it. The key is a hash over those four. The value is the realised
path, held by a GC root under `/nix/var/nix/gcroots/imageless-memo`. A
repeat create (a restart, a replica) registers the bundle's root against the
recorded path without running Nix. The memo is consulted only after every
check above, keeps its 64 most recently used entries, and is never used for
`allow_unlocked_inputs` or external references. `IMAGELESS_EVALUATION_MEMO`
names another directory, or `none` to evaluate every time.

## 3. Annotations (highest precedence)

OCI annotations override the zero-config default. Annotation values are
limited to 4096 bytes; selectors to 1024.

Development / source-evaluation namespace:

| Annotation | Meaning |
|---|---|
| `run.imageless.source` | Absolute in-rootfs path (staged and evaluated as `path:<rootfs><source>`) or a flake reference. |
| `run.imageless.output` | Flake output attribute; defaults to the runtime's configured default (`rootfs`). |
| `run.imageless.containers` | Selector list: only named containers are materialized. |
| `run.imageless.skip-containers` | Selector list: named containers are passed through. |

A `source` that is not an absolute in-image path is an **external flake
reference**. External references are a supported mode, but the node must opt in
through materializer policy (`eval_allowed_uri_prefixes`); a node that has not
allow-listed the reference's prefix fails the request. External references must
carry an explicit remote scheme: node-local schemes (`path:`, `file:`, and any
`*+file:` transport) and registry names (bare words, `flake:`) are rejected
before policy is consulted. The in-image `/` form is the only way an annotation
names node-local content — the `path:` prefix a policy allow-lists authorizes
the runtime's own rewrite of staged in-image sources, never an
annotation-supplied path. Pin external references (locked inputs, explicit
revisions) for anything beyond development — a mutable ref is not a deployment
identity.

Every `source` resolution — the staged in-image form and the external form
alike — is node-side evaluation: a `cache_only` node refuses it
(`EvaluationDisabled`) before the prefix allow-list is consulted, so the
external mode requires **both** `cache_only: false` and a matching prefix.
Prefix matching is a literal byte-prefix comparison against the reference (its
output fragment removed), performed before any canonicalization or registry
resolution. Author prefixes to a boundary: `github:myorg/` — an unterminated
`github:myorg` also authorizes `github:myorg-evil/anything`.

What counts as a pin: an explicit revision (`?rev=`, or the
commit-addressed `github:owner/repo/<rev>` form) or a content hash
(`?narHash=`). The referenced flake's own lock file is honored as written for
its locked inputs; inputs it left unlocked resolve at evaluation time —
pinning the top-level reference does not pin them. The node deliberately does
not police pin forms: rejecting mutable references is authoring- and
admission-tooling's job, the node contract stays the prefix allow-list, and a
*production* identity remains a release digest (§6). External fetches are
bounded only by the materialization deadline (§4) — the §2.3 staging bounds
apply to in-image sources alone.

Release namespace (cache-only production, §6):

| Annotation | Meaning |
|---|---|
| `imageless.run/release-v1` | A digest-addressed release reference. Mutually exclusive with `run.imageless.source`. |
| `imageless.run/containers-v1` | Release-mode container selector. |
| `imageless.run/skip-containers-v1` | Release-mode skip selector. |

Kubernetes handling:

- A container annotated `io.kubernetes.cri.container-type: sandbox` is **never**
  materialized. The pause sandbox always runs its ordinary rootfs.
- `io.kubernetes.cri.container-name` participates in selector matching.
- Under containerd, the runtime handler must allow-list these annotation
  prefixes (`pod_annotations` / `container_annotations`) or they never reach the
  OCI spec.

## 4. Runtime obligations

A conforming runtime, at per-container `create`:

1. **Selects or passes through.** A bundle with no embedded flake and no
   annotations proceeds unchanged. Passthrough must be a no-op: no
   materializer contact, no bundle mutation. One exception, which never
   changes the create's outcome: a pod sandbox whose annotations name a
   release may start realizing that release in the background (a
   *prefetch*), so the download or build overlaps the sandbox's boot,
   networking, image pulls and init containers. The sandbox's config is
   untouched, and the prefetch pins what it realizes with GC roots in the
   sandbox bundle, which live exactly as long as the pod. A prefetch that
   fails costs nothing, because each container's create realizes what it
   needs and joins a download still in flight. The reference runtime does
   this unless `IMAGELESS_PREFETCH=off`.
2. **Validates fail-closed.** Malformed metadata, invalid selectors, oversized
   values, or contradictory annotations (e.g. release + source) fail creation.
   The runtime must never delegate a partially rewritten spec.
3. **Materializes boundedly.** Materialization has a deadline (reference
   default 300 s, configurable 1–3600 s). On expiry the materializer's whole
   process tree is killed. Exactly one realized store path is accepted;
   ambiguous output is an error.
4. **Rewrites atomically.** `root.path` in `config.json` is replaced via
   write-to-temp + rename, preserving file mode and fsyncing the file and its
   parent directory. The document is re-serialized: every field the runtime
   does not rewrite keeps its value, but formatting and key order are not
   preserved. The file's consumers parse it; none compare its bytes.
   `root.readonly` is forced to `true`: the new root is a store path
   shared with every other container and the node itself, so a workload that
   needs writable paths gets them from mounts (`tmpfs`, volumes), never from
   the root. Process metadata is only applied when the release manifest
   explicitly requests it.
5. **Never lets the OCI runtime write into the store.** An OCI runtime
   creates the destination of every mount inside the root before mounting
   over it: `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`, a
   service-account token directory, any volume. A store path can take none of
   those writes. Where the store is read-only (NixOS, and any hardened node)
   the create fails. Where it is writable, the runtime modifies a store path
   in place, which invalidates its hash for every container and the node. So
   `root.path` names a **mountpoint layer**: an overlay whose only lower
   layer is the realized store path, with a small upper layer private to
   the container that receives the runtime's mountpoints and nothing else,
   because the root is still made read-only before the workload runs. The
   layer stays mounted at its node path until the container is deleted,
   because the OCI runtime keeps using that path (runc starts every `exec`
   in it), and is released after that. The reference runtime stages layers
   under a private mount point (`/run/imageless-roots`), ties each one to
   the runc state file of the container created over it, and releases it
   once that state is gone. The layer is not a writable root, and a later
   revision that offered one would be a new opt-in.
6. **Projects the store.** The realized closure must be visible to the
   container. Reference modes: `node` (bind the node's `/nix/store` read-only),
   `closure` (read-only bind mounts scoped to the closure of the realized
   root, computed by the materializer), or `runtime` (the rewrite adds no store
   mount, because the runtime consuming the bundle projects the store itself,
   as an embedding sandbox that builds its own view from `root.path` does). In
   every mode the rewrite refuses workload mounts at or under `/nix/store`.
7. **Holds GC roots for the container's lifetime.** Materialization registers
   Nix GC roots tied to the bundle (`.imageless-rootfs-gcroot`,
   `.imageless-store-gcroots/`). Roots are released when creation fails, the
   delegate exits unsuccessfully, or the container is deleted. A live container
   must survive `nix-collect-garbage`; a deleted container must not pin its
   realization.

## 5. Interposition seam

The contract binds at the point that runs **once per container**: the OCI
runtime's `create`.

- Generic nodes: `imageless-runc` interposes the runc CLI (`create` triggers
  resolution; every verb delegates to the real runc). Under containerd —
  including 2.x pod-shim grouping, where all of a pod's containers share one
  shim process — this seam still fires per container, because the shim execs
  the OCI runtime binary per `runc create`.
- Embedded runtimes: link the `imageless` library and call it during `create`.

A containerd runtime-v2 `start`/`delete` binary interposer is **not** a
conforming implementation: under containerd 2.x it observes only the pod
sandbox, not each workload container.

## 6. Release profile (optional, cache-only)

Production nodes may refuse all evaluation (`cache_only: true`, the default
policy) and resolve only digest-addressed releases:

- `imageless.run/release-v1` carries a reference resolved against
  node-configured **issuer catalogs** (local directory or HTTPS), fetching an
  `imageless.release.v1` manifest addressed as `sha256/<digest>.json`, at most
  64 KiB, validated against its digest (canonical JSON).
- Node policy allow-lists issuers, release-name patterns, and the substituters
  (with public keys) a release may be fetched from.
- The manifest maps target systems to store paths and may carry explicit
  process metadata.

Digest references are for machines, not fingers. A catalog MAY additionally
publish a name/channel index (`refs/<name>/<channel>` → digest) so that
*client-side* tooling can resolve a human-friendly name to a pinned reference
at authoring or apply time. Nodes MUST ignore the index: the annotation a node
accepts is always digest-addressed, and node-side resolution of mutable
pointers is non-conforming.

A pointer is **64 lowercase hexadecimal digits**, optionally surrounded by
whitespace, and nothing else — not JSON, and carrying no metadata. Readers MUST
reject anything else, including uppercase hex, a `sha256:` prefix, or a second
line. A pointer is the one file in this design a publisher rewrites in place,
so the format is deliberately too small to grow a schema: a field added here
would be a field a client must interpret to decide what to deploy, which is
exactly the mutable, evaluated deployment identity the release profile exists
to avoid. Metadata about a release belongs in the manifest, whose bytes are
covered by the digest. Readers SHOULD bound the read (the reference
implementation stops at 128 bytes), since the file is served by whatever host
publishes the catalog.

The index is *conventional*, not authoritative: republishing a channel changes
what future `pin` calls resolve to and never what an existing pod runs, because
the pod records the digest. Nothing verifies that a pointer names a manifest
the catalog actually holds — a client learns that when the fetch of
`sha256/<digest>.json` fails.

The publisher that produces manifests is out of scope for this spec; any CI
that can copy a Nix closure to a cache and emit the manifest JSON conforms.

### 6.1 Manifest signatures

The digest proves the manifest is the one the pod named. It does not prove the
issuer published it: whoever can write a catalog can add a manifest under any
release name, with any entrypoint, environment, and store paths the cache
holds. Signatures close that gap.

- A signature is a detached [minisign](https://jedisct1.github.io/minisign/)
  signature over the manifest's exact bytes, published beside it as
  `sha256/<digest>.json.minisig` and bounded to 4 KiB. Both minisign
  algorithms are accepted: `ED` (over the BLAKE2b-512 hash, what `minisign -S`
  writes) and legacy `Ed`. The global signature over the trusted comment MUST
  verify too; nothing in either comment is interpreted.
- Node policy lists, per issuer, the public keys it accepts for that issuer.
  A key trusted for one issuer never authenticates another issuer's manifest,
  because the manifest's `issuer` must match the reference (§6).
- A node MUST refuse a release whose issuer has keys configured unless the
  sidecar verifies under one of them. An issuer with no keys is accepted
  unsigned only where node policy says so explicitly; the reference
  implementation requires `allow_unsigned: true` for that and refuses a
  policy that sets neither.
- A node MAY refuse specific digests (`revoked_manifests` in the reference
  policy) even when they are validly signed.

The sidecar is not covered by the digest, so it can be replaced without
changing any reference. That is what makes the procedures below possible:

- **Rotation.** Add the new key to every node's policy, re-sign the live
  manifests with it (replacing their sidecars), then remove the old key.
  Until the last step, both keys verify.
- **A compromised signing key.** Rotate as above, and remove the old key at
  once rather than last. Releases not yet re-signed stop resolving on nodes
  that have dropped the key; nothing an attacker signed with it resolves
  anywhere.
- **A compromised catalog, keys intact.** The attacker cannot add a release a
  node will run. They can delete manifests or sidecars (denial of service),
  and they can re-point a channel at an older release the issuer really
  signed. Pins taken from the catalog after the compromise deserve review;
  a withdrawn release belongs in `revoked_manifests`.

Signing is the publisher's job and needs only the stock tool, for example
`minisign -S -s issuer.key -m sha256/<digest>.json`. The signing key never
belongs on a node, in a Nix store, or anywhere a pull request's build can
read it.

## 7. Conformance

An implementation conforms when it passes the acceptance gates in this
repository:

1. **Raw Docker embedded-layer bootstrap** — a seed image whose layer contains
   only the flake and its inputs (not the executable that produces the expected
   response) serves the expected response after materialization; an ordinary
   image passes through unchanged.
2. **Kubernetes CRI lifecycle** — a real containerd node with a
   `RuntimeClass`, proving sandbox passthrough, per-container selection,
   recreate, GC-while-running, delete-and-collect, and reboot recovery.

Conformance evidence — who has passed which gate against which pinned
component versions, and which spec sections have been exercised by real
deployments — is recorded in `CONFORMANCE.md`.

## 8. Stability and deprecation

This section binds when the status line above reads **frozen**. Until then
v0.x is greenfield: identifiers may still be renumbered or renamespaced, and
there is no compatibility surface to deprecate.

### 8.1 The frozen surface

What freezes is the deployment interface — everything a conforming image,
deployer, or runtime can observe:

- The annotation names in §3 and their grammars: `run.imageless.source`,
  `run.imageless.output`, `run.imageless.containers`,
  `run.imageless.skip-containers`, `imageless.run/release-v1`,
  `imageless.run/containers-v1`, `imageless.run/skip-containers-v1`; the
  4096-byte value and 1024-byte selector limits; the external-reference
  scheme rules.
- The embedded convention (§2): `etc/imageless/flake.nix` as a regular file,
  zero-config selection of `/etc/imageless#rootfs`, and the staging bounds
  (16 MiB, 4096 entries, regular files and directories only, no symlinks).
- The runtime obligations (§4), including the three store projection modes and the
  GC-root names (`.imageless-rootfs-gcroot`, `.imageless-store-gcroots/`).
- The release profile (§6): the `imageless.release.v1` manifest schema,
  canonical-JSON digest addressing, the `sha256/<digest>.json` catalog layout
  and 64 KiB manifest cap, the `refs/<name>/<channel>` index rules, and the
  `sha256/<digest>.json.minisig` signature sidecar (§6.1).

Explicitly **not** part of the frozen surface:

- the resolver wire protocol and `PROTOCOL_VERSION` — private to the
  reference shim and its optional daemon, versioned by their handshake,
  changeable in any release;
- the node policy file (`/etc/imageless/policy.json`) schema — node-operator
  configuration owned by the materializer implementation;
- the `imageless` Rust crate API — governed by Cargo semver on its own clock;
- diagnostic text and the timing-telemetry format.

### 8.2 Changing the frozen surface

- **Additive** changes — those that cannot change the outcome of any input a
  frozen-spec consumer already produces (a new annotation, a new optional
  manifest field, a new projection mode) — may land in a minor revision of
  this document.
- **Breaking** changes arrive as **new suffixed identifiers**
  (`imageless.run/release-v2`); a frozen identifier's meaning is never
  repurposed. The old identifier keeps its exact behavior through a
  deprecation window of at least one minor release and six months, during
  which a conforming runtime warns through its telemetry and diagnostics but
  never silently changes the outcome. Removal is a major revision.
- **Errata** — wording fixes with no observable behavior change — may amend
  the text in place, recorded in an errata log appended to this document.

### 8.3 Gates are spec

§7 defines conformance as passing the acceptance gates, so once frozen, a
gate change is a spec change. A gate may be strengthened in a minor revision
only when every already-conforming implementation still passes; anything else
is a breaking change and follows §8.2.
