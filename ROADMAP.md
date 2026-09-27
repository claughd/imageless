# Roadmap

The product is the shim and the spec. Everything below is staged so that
platform features have to *earn* their way in against a measured need, instead
of arriving alongside the core the way they did in the incubation repo.

## v0.1 — the extraction (complete)

- [x] Standalone workspace: own `Cargo.toml`/`Cargo.lock`, own flake +
      complete `flake.lock`, CI where **every** package is in `checks` and
      lint gates merge.
- [x] Namespace cutover to the owned domain: `imageless.run/*` and
      `run.imageless.*`. Greenfield — no legacy names, no compat carve-outs;
      the code reads as if the incubation identifiers never existed.
- [x] Zero-config embedded discovery: `etc/imageless/flake.nix` alone selects
      a container; `source.json` is retired (the flake is the metadata —
      alias nonstandard outputs to `#rootfs` in the flake itself).
- [x] Deleted, not migrated: the containerd start/delete interposer (invalid
      under containerd 2.x pod-shim grouping), `imagelessctl`, the release
      publisher and `publish.json`.
- [x] Acceptance gates as the only gates: the raw-Docker embedded-layer smoke
      and the CRI lifecycle VM test, the latter extended to prove
      embedded-layer bootstrap (flake present only in the image layer).
- [x] Cowboy consumes this repo as a flake input; `cowboy-runtime` stays the
      first embedded-library consumer.
- [x] **In-process materializer — no mandatory daemon.** A `Materializer`
      seam with a direct backend: the shim and library consumers read node
      policy (`/etc/imageless/policy.json` or `IMAGELESS_POLICY`) and perform
      the Nix work in-process. `imageless-resolver` becomes purely the optional
      hardened profile — node-wide concurrency caps, single-flight coalescing,
      privilege-separated evaluation — selected via
      `IMAGELESS_RESOLVER_SOCKET`.

## v0.2 — make the base product complete

- [x] **External flake references as a first-class mode.** `run.imageless.source`
      may name a flake ref, gated by node policy
      (`eval_allowed_uri_prefixes`). Docs recommend enabling evaluation
      (`cache_only: false`) wherever embedded flakes are the point; the shipped
      default stays fail-closed (`cache_only: true`). Document pinning
      expectations — a mutable ref is not a deployment identity.
- [x] **Compatibility matrix in CI.** The CRI gate against pinned containerd
      1.x and 2.x, the Docker smoke against a pinned Docker, plus rootless
      stock-runc — recorded per version so a containerd change cannot silently
      invalidate the claim.
- [ ] Publish the `imageless` crate once the `prepare_bundle`-shaped API
      settles.
- [x] **`kubectl-imageless` plugin — zero Nix on the client.**
      `kubectl imageless run ./dir` packs the directory into a seed-image
      layer at `/etc/imageless/` (plain tar + manifest — no Nix, no Docker),
      pushes to a configured registry, and prints a digest-pinned Pod with
      `runtimeClassName: imageless`; `kubectl imageless run --external
      <flake-ref>` deploys the external-ref mode instead (annotation only,
      needs node policy allow-listing, and refuses an unpinned reference the
      node deliberately would not). Client-side enforcement of the spec's
      staging bounds, a loud warning on missing `flake.lock`, and a `doctor`
      subcommand that diagnoses unprepared clusters — including the two
      offline checks, policy and reference, that need no cluster at all.
      Evaluation happens on the node, under node policy — the client stays
      Nix-free.
- [x] **Documented dev-cluster quickstart (kind).** `dev/kind/` takes a host
      with Docker from nothing to a flake serving HTTP on Kubernetes, and the
      walkthrough is executed rather than asserted: every step — including the
      plugin's pack/push/apply path and the node-side `nix-store --gc`, which
      live containers survive — verified end to end against kind 0.31 /
      containerd 2.2.0.
- [x] **k3s dev node.** `dev/k3s/` covers a single-node k3s on the host. It
      adds the runtime through a containerd drop-in (`config-v3.toml.d`),
      which k3s's generated config imports and every restart keeps, instead
      of a per-release template. The walkthrough was executed on k3s
      v1.35.5+k3s1 / containerd 2.2.3: the seed pod, the plugin
      pack/push/apply path, and GC-while-running plus collect-after-delete.
      k3d and k3s releases on containerd 1.x are not covered.
      Pods keep Kubernetes' default service-account token: runc's
      mountpoints go to a per-container overlay layer over the store path
      (SPEC §4.5), found when imgless's NixOS VM test hit the read-only
      store. Earlier builds wrote those mountpoints into writable store paths.
      On NixOS, `services.imageless.k3s.enable` writes the same drop-in
      (both annotation families, `SystemdCgroup = true` for k3s as a systemd
      unit) and puts the shim's environment on the `k3s` unit. The module
      check evaluates it; no VM test in this repository boots it yet.
- [x] **No hand-typed digests.** Optional catalog name/channel index
      (`refs/<name>/<channel>` → digest, client-side only; nodes ignore it)
      plus `kubectl imageless pin <issuer>/<name>` and pin-on-apply
      (`run --release`), so release references are resolved to `@sha256:…` by
      tooling at authoring time. SPEC §6 fixes the pointer format at 64
      lowercase hex digits — too small to grow a schema a client would have to
      interpret. `nix/release-catalog.nix` publishes the index from a
      `channels` argument, so the repo's own helper emits a catalog `pin`
      reads. Examples never show `REPLACE_DIGEST` — they show the pin flow or
      Nix's `.reference` templating. The node contract stays digest-only.
- [x] **Shebang scripts as deployables.** `kubectl imageless run ./script.sh`
      where the file carries a `#!/usr/bin/env nix` + `#! nix shell …
      --command <interpreter>` shebang: the client parses it and *generates* a
      seed — a flake whose `rootfs` is a `buildEnv` of the named packages, a
      real `flake.lock`, and the script — then packs and pushes that. Pure
      client-side desugaring: the spec, the shim, and the node never learn
      shebangs exist, and the pod they see is the packed-directory pod.
      Reproducible by default. The client has no Nix and so cannot compute a
      `narHash`; the lock it writes pins the nixpkgs *this binary was built
      against*, baked in by `nix/package.nix` from the flake's own input — the
      same rev `examples/nginx-embedded` pins by hand, so a node that has ever
      built imageless resolves a generated seed offline. A `cargo`-built plugin
      has no such pin and says so rather than inventing a hash; `--unpinned`
      opts out into a `nixos-unstable` seed with no lock, loudly.
      `--emit-seed DIR` writes the generated tree out, which is the way *out*
      of this mode: the result is an ordinary seed directory, and nothing about
      it required the plugin to produce. Two deliberate bounds: a single file
      (not yet a directory whose entry file is shebang'd), and `nixpkgs#`
      installables only — a second flake reference would be an input the client
      cannot lock, which is the property this mode is built on.

## Spec v1 freeze

Freeze when, and not before:

- two independent conforming consumers (the shim and `cowboy-runtime`) pass
  both acceptance gates on the pinned compatibility matrix;
- the external-reference and release-profile sections have been exercised by a
  real deployment each;
- a deprecation policy for post-freeze changes is written down (pre-freeze
  there is none — v0.1 is greenfield and carries no compatibility surface).

## v0.3 — hardening the profiles that have users

The cache-only release profile has a live consumer (Cowboy's caged-agent
nodes), so its hardening is committed work, paced by that deployment:

- [ ] Threat-model document: compromised workloads, malicious caches, staging
      abuse, reboot cleanup; move the development evaluator into a dedicated
      service cgroup rather than UID-wide rlimits.
- [x] Detached signatures over canonical manifest bytes (SPEC §6.1): stock
      minisign sidecars (`sha256/<digest>.json.minisig`), node-owned keys per
      issuer, signatures required unless an issuer is explicitly
      `allow_unsigned`, several keys at once for rotation, and
      `revoked_manifests` for signed releases a node must still refuse.
      Verified against minisign 0.12's own output and through the resolver
      daemon's refusal paths. The CRI VM gate now signs its catalog, but it
      has not been booted since, because the environment that wrote this
      had no KVM.
- [ ] Private/authorized cache access without distributing cache credentials to
      workloads.
- [ ] Enough recorded evaluation/build input to reproduce a release after cache
      eviction.

## Measured before built

These return only in response to numbers from the timing telemetry, and as a
separate operations project rather than core:

- prewarm / readiness / desired-state reconciliation (the deleted
  `imagelessctl` functionality);
- closure byte-cost inspection and prepared-node labels;
- cache-affinity scheduling.

## Ideas (explicitly not on the roadmap)

Prior-art notes live with the incubation history; none of this defines the
runtime contract:

- a release *publisher* product (any CI that copies a closure and emits the
  manifest JSON conforms — that is the point of the manifest);
- Garnix-style recursive FOD verification, SBOM/SLSA emission,
  hardware-attested builders;
- incremental build retention — a mutable build cache the node keeps across
  creates. Gated rather than merely unscheduled, and the gate is worth writing
  down because the obvious objections are not the binding ones. It would need a
  break glass that clears the cache and a TTL that bounds staleness, plus a size
  cap, since a TTL bounds age and not disk. Those are the cheap half. The
  expensive half is that a cache surviving between creates makes the same flake
  produce different output depending on node state — the one property the
  digest-addressed contract exists to deny — and makes one tenant's build
  artifacts an input to another's build, which is poisoning in one direction and
  disclosure in the other. Partitioning per tenant answers that and removes most
  of the hit rate that motivated it.

  The alternative needs none of it: a seed flake that builds its dependencies in
  their own derivation gets the same edit-loop win with the Nix store as the
  cache, where `nix-collect-garbage` is already the break glass and GC roots are
  already the TTL. `dev/bench` measures the difference between the two flake
  shapes, and `dev/bench/README.md` describes the pattern;
- a containerd TTRPC `Create` wrapper adapter — only worth revisiting if the
  runc seam ever proves insufficient, which no current containerd generation
  suggests;
- automated multi-architecture placeholder publishing;
- a cluster-installer DaemonSet (drop the shim + containerd config onto nodes,
  gVisor/Kata style) — high adoption value but real blast radius (containerd
  restarts on live nodes); revisit once the kubectl plugin has users asking
  for it.

## Non-goals

Unchanged from incubation, and load-bearing:

- no reimplementation of containerd's ttrpc task service;
- no general CI system or database on nodes;
- no compilation, FOD rebuilding, or provenance generation during container
  start;
- no mutable remote flake refs as a *production* deployment identity;
- no policy hidden inside placeholder OCI images.
