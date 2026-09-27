# Redeploy benchmark

Answers one question: *I changed a line of Rust — how long until it is running
again, and where did that time go?*

```sh
nix develop --command dev/bench/redeploy-bench.sh
```

No cluster, no container runtime, and no resolver daemon are needed. The harness
drives the real `create` path — `imageless-runc create` against a real OCI
bundle, materializing a real embedded flake with real Nix — in the daemonless
in-process mode, and delegates to a stub runtime so that every microsecond it
reports belongs to imageless. The per-stage numbers come from the node's own
`imageless.timing.v1` telemetry sink, not from a separate clock.

## What it varies

Two shapes of seed flake, three scenarios each:

| Variant  | Seed flake |
| -------- | ---------- |
| `single` | one derivation builds the whole Cargo workspace |
| `split`  | a dependency derivation builds the slow crate; the application derivation reuses its `target/` |

| Scenario  | Meaning |
| --------- | ------- |
| `cold`    | nothing in the store yet |
| `restart` | byte-identical source, materialized again — a pod restart or a scale-up |
| `edit`    | one line changed in the application crate |

`split`/`edit` is the incremental build. `single`/`edit` is what a naive seed
flake costs on every redeploy.

Expect `split`/`cold` to be the *slowest* cell in the table: it builds the
dependency crate and the application crate in two derivations instead of one,
and pays to move `target/` through the store in between. That is the trade —
one slower first build to make every subsequent edit cheap.

The workload is generated, not vendored: `--modules` and `--functions` size a
crate of generic code whose monomorphization is what costs real compile time.
Each run salts the generated source, so `cold` is genuinely cold even on a
machine that has run the benchmark before.

## Reading the result

Three terms, and they behave differently:

- **Staging and GC-root registration** are imageless's own overhead. They are
  single-digit to low-double-digit milliseconds and they do not vary with the
  workload.
- **Nix evaluation** is a floor, reported separately by a probe. Nix caches
  evaluation against the flake's fingerprint, which covers every file in the
  tree — so changing one line misses the cache and nixpkgs is evaluated from
  scratch. Splitting derivations does not reduce this term. Nothing in the seed
  flake does; it is the price of the edit itself.
- **Compilation** is the term the split addresses, and the only one that grows
  with the size of your dependency tree.

That decomposition is the point. On a small crate the evaluation floor
dominates and splitting looks like it barely helps; on a real dependency tree
the compile term is minutes and the split is the whole game. Reading the two
terms separately is what tells you which case you are in.

`restart` is the number to check first. A restart of a locked seed is
answered from the node's evaluation memo (SPEC §2.4) without running Nix, so
it costs staging, a content hash and the GC-root registration: 45–49 ms with
`--modules 4 --functions 4`, against 1,435–1,503 ms with the memo off
(`IMAGELESS_EVALUATION_MEMO=none`), which was Nix's own evaluation-cache hit.
Set that to measure the path a node takes the first time it sees a seed. The
memo keys on content, not timestamps, and staged sources keep a pinned mtime
so Nix's fingerprint agrees.

## Believing the numbers

Each cell is the **fastest** of `--samples` repetitions (`cold` happens once, by
definition), and carries the load average from the moment that sample started.
The minimum is deliberate rather than flattering: contention only ever adds
time, since nothing about a busy host makes a build finish early, so the fastest
sample is the closest estimate of the real cost and every other one is that plus
an unknown amount of someone else's work. A cell whose `load` column is not near
idle is still an overestimate.

Check the load column before believing any absolute number. A contended host
moves these by an order of magnitude — far more than any effect being measured —
and the failure is not subtle: an `edit` costing 3× its own `cold` build is a
machine problem, not a finding. Ratios between variants measured minutes apart
are no safer than the absolutes, because the contention is not constant.

`--keep` leaves the work directory and the raw per-sample JSON in place, which
is what to reach for when a cell looks wrong: the per-sample loads say whether
it was the host.

The `split` variant needs a sandboxed Nix build. Cargo decides a unit is fresh
by stat-ing the paths recorded when it was built, so both derivations build at
`$NIX_BUILD_TOP/ws`, which is stable only under `sandbox = true`. Without the
sandbox the split degrades to a full rebuild — a worse number, not a wrong one.
