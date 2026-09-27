# Dev Kubernetes cluster for the imageless runtime (kind)

From nothing to a flake serving HTTP on a real Kubernetes cluster, in about
five minutes, without touching the host's `/etc` or its Nix store. This is
the local Kubernetes counterpart of `dev/docker/` — same policy, same seed
image, but through kubelet, CRI, and a `RuntimeClass` instead of
`docker run`.

The authoritative proof of this wiring is the hermetic CRI VM gate
(`.#imageless-cri-vm`); this harness reproduces its node setup on a
[kind](https://kind.sigs.k8s.io/) node so you can poke at it interactively.

**Requirements:** Linux x86_64 host, Nix with flakes, Docker,
kind ≥ 0.27 (containerd 2.x nodes), kubectl. All commands run from the
repository root.

## 1. Create the cluster

```sh
kind create cluster --name imageless --config dev/kind/kind-config.yaml
```

Expected: `Set kubectl context to "kind-imageless"` after ~60–90 s. The
config's `containerdConfigPatches` merges the `imageless` runtime entry
(the body of `examples/containerd-config.toml`) at node creation — there is
no containerd restart step, and no later reconciliation: config changes mean
recreating the cluster.

## 2. Make the node an imageless node

```sh
dev/kind/setup.sh imageless
```

Expected: stage lines ending in `node imageless-control-plane ready` after
~30–90 s. The script builds `.#imageless`, stages its closure as a
self-contained store (`nix copy --to 'local?root=…'`), imports it into the
node, links `imageless-runc` where the containerd patch's `BinaryName`
expects it, installs `examples/dev-policy.json` root-owned at
`/etc/imageless/policy.json` (the production binary's ownership check passes
because `docker exec` writes it as the node's root), and labels the node to
satisfy `examples/runtimeclass.yaml`'s scheduling gate.

Why the node gets its own store instead of a bind of the host's: the GC-root
guarantee — a live container survives `nix-collect-garbage` — only holds
when the store, the bundles, and the GC share one mount namespace. A host
bind would leave root symlinks pointing at paths the host GC cannot
resolve, and the guarantee would fail silently.

Add `--prewarm` to pre-realize the nginx rootfs on the node (adds 1–2 min
here, and the first pod then starts in seconds instead of minutes).

To check the cluster-visible half of that wiring before going further:

```sh
nix build .#kubectl-imageless
kubectl apply -f examples/runtimeclass.yaml
./result/bin/kubectl-imageless doctor --context kind-imageless
```

Expected: `pass` on `runtime-class`, `scheduling`, and `nodes` (`1 of 1
nodes`). A failure here is the cheap version of the failure you would
otherwise meet as a pod stuck Pending in step 4. What `doctor` cannot see is
the node-local half this step just installed — the containerd handler, the
shim binary, the policy file have no API representation — so it reports
`node-config` as a permanent skip and says as much. Step 4 remains the proof.

## 3. Load the seed image

```sh
nix build .#nginx-embedded-image
zcat -f result | docker exec -i imageless-control-plane \
  ctr --namespace k8s.io images import -
```

Expected: a `saved` line naming
`localhost/imageless-nginx-embedded:e2e`, then `Importing ... elapsed`. The
image is a few kilobytes: its only contents are
`examples/nginx-embedded/{flake.nix,flake.lock}` at `etc/imageless/`. The
`zcat` matters — `dockerTools.buildImage` emits a gzipped archive and the
node's containerd import wants a plain tar.

Not `kind load image-archive`, which fails here. `kind load` invokes `ctr
images import --all-platforms`, and `dockerTools.buildImage` emits a legacy
Docker v1 archive — `manifest.json` plus a layer directory, no `index.json`
and no platform descriptor anywhere. containerd 2.x finds nothing to unpack
and refuses with `no unpack platforms defined`. Importing without
`--all-platforms` is exactly what the CRI VM gate does
(`smoke/imageless-cri-smoke.sh`), which is why that gate stays green on an
archive this step could not load.

## 4. Run it

```sh
kubectl apply -f examples/runtimeclass.yaml -f dev/kind/pod-nginx-embedded.yaml
kubectl wait --for=condition=Ready pod/imageless-nginx --timeout=300s
```

Expected: `pod/imageless-nginx condition met`. Without `--prewarm` the first
pod takes 2–4 min: the node fetches the flake's pinned nixpkgs, evaluates
`#rootfs`, and substitutes nginx from cache.nixos.org. If a slow network
trips the kubelet's create deadline, the automatic retry hits the
now-warm store and succeeds. Subsequent pods start in seconds.

```sh
kubectl port-forward pod/imageless-nginx 18080:18080 &
curl -s http://127.0.0.1:18080/   # => imageless-nginx-ok
```

## 5. Prove it was imageless

```sh
docker exec imageless-control-plane ctr -n k8s.io images ls | grep nginx-embedded
```

The image the node holds is ~11 KiB and contains no nginx: its entire payload
is `etc/imageless/flake.nix` and `etc/imageless/flake.lock`. The filesystem
serving that response was materialized on the node from that flake at
container-create.

`kubectl exec` into this pod does not work, and that is the demonstration
rather than a defect — the rootfs is the flake's `#rootfs` output, which
carries nginx and nothing else, so there is no shell to exec into. To see the
materialization, look at the GC root the runtime registered for the bundle:

```sh
docker exec imageless-control-plane sh -c \
  'for l in /nix/var/nix/gcroots/auto/*; do
     t=$(readlink "$l"); echo "$t -> $(readlink "$t")"
   done'
```

Each live container has one indirect root pointing at its bundle's
`.imageless-rootfs-gcroot`, which resolves to the store path serving as its
root filesystem. That indirection is the guarantee: `nix-store --gc` on the
node cannot collect a rootfs while a container is using it.

## 6. Author your own, with the kubectl plugin

Steps 3 and 4 load a seed image built by Nix. `kubectl imageless run` does
the same job for an arbitrary directory, with no Nix and no Docker on the
client — it packs, pushes, and prints the pod:

The plugin pushes to a registry, so the node needs one it can pull from.
`localhost:5001` on the client is not `localhost:5001` inside the node — that
is the node container's own loopback — so the node needs a mirror entry
pointing at the registry over kind's network. Nothing here touches
`kind-config.yaml`: kind already sets containerd's registry `config_path`, and
`certs.d` is read per-pull, so this needs no cluster recreation.

```sh
docker run -d --name kind-registry --restart=always \
  -p 127.0.0.1:5001:5000 registry:2
docker network connect kind kind-registry

docker exec imageless-control-plane mkdir -p /etc/containerd/certs.d/localhost:5001
docker exec -i imageless-control-plane \
  sh -c 'cat > /etc/containerd/certs.d/localhost:5001/hosts.toml' <<'EOF'
[host."http://kind-registry:5000"]
  capabilities = ["pull", "resolve"]
EOF
```

Then pack, push, and apply:

```sh
nix build .#kubectl-imageless

./result/bin/kubectl-imageless run examples/nginx-embedded \
  --repo localhost:5001/team/nginx --name my-nginx \
  -- /bin/nginx -e /proc/self/fd/2 -c /etc/nginx/nginx.conf \
  | kubectl apply -f -
```

Expected: `pushed localhost:5001/team/nginx@sha256:…`, then the pod reaches
`Running` and serves the same `imageless-nginx-ok` on port 18080.

The command matters. `nginx-embedded`'s own `nginx.conf` already sets
`daemon off;`, so adding `-g 'daemon off;'` makes nginx exit 1 at startup with
`"daemon" directive is duplicate` — a container that pulls, creates, and starts
before dying, which looks nothing like an imageless problem and is not one.

`localhost:5001` needs no push flags: loopback registries are pushed over plain
HTTP automatically. The pod that lands references the image by digest, never
by tag.

One thing worth understanding about what lands. The plugin annotates the pod
`run.imageless.source: /etc/imageless`, which is the *development* family — and
`kind-config.yaml` allow-lists only `imageless.run/*`, so containerd strips it
before the shim ever sees it. The pod works anyway, because with no source
annotation the shim falls back to zero-config discovery and finds the very same
`etc/imageless/flake.nix` the annotation was pointing at. The redundancy is
load-bearing here; do not read this working pod as evidence that the
development annotation family is allow-listed on this cluster. It is not, which
is exactly why `--external` below cannot work.

Two caveats worth knowing before pointing this at a shared registry:

- Registries that garbage-collect untagged manifests (GHCR's cleanup
  actions, ECR lifecycle rules with `tagStatus: untagged`) will eventually
  reap a digest-only push. Pass `--tag` to keep a name on it; the pod
  reference stays digest-pinned either way.
- `--dry-run` prints the same digests and pod manifest and touches no
  network, which is the way to inspect what would be pushed.

`kubectl imageless run --external <flake-ref>` deploys a flake reference
instead of packing anything — and **this cluster will not run it.** Two
things stand in the way, both deliberate:

- `kind-config.yaml` allow-lists `imageless.run/*` annotations only, so
  containerd filters `run.imageless.source` out of the OCI spec and the shim
  never sees it. The placeholder image the plugin pushes exists precisely to
  make that visible: its flake throws, and the create fails saying the
  annotation was dropped rather than starting some unrelated container.
- `examples/dev-policy.json` allow-lists the `path:` prefix — enough for the
  embedded flake this harness demonstrates, and nothing else.

To try it anyway: add `"run.imageless.*"` to both annotation lists in
`kind-config.yaml` (which means recreating the cluster — there is no
reconciliation), and install `examples/external-refs-policy.json` with your
own prefix in place of `github:yourorg/` instead of `dev-policy.json`. The
node then fetches and builds what the pod names, which is a materially
larger trust surface than an image you pushed; `--dry-run` and
`kubectl imageless doctor --policy … --source …` both tell you whether the
reference would be admitted before you find out from a stuck pod.

## 7. Deploy a script, with no flake at all

A file whose first lines are a nix shebang needs no seed directory — the plugin
generates one:

```sh
cat > hello.py <<'EOF'
#!/usr/bin/env nix
#! nix shell nixpkgs#python3 --command python3
import sys
print("imageless-shebang-ok", sys.version_info[0])
EOF

./result/bin/kubectl-imageless run hello.py \
  --repo localhost:5001/team/hello --name hello-shebang \
  | kubectl apply -f -

kubectl wait --for=jsonpath='{.status.phase}'=Succeeded pod/hello-shebang --timeout=300s
kubectl logs hello-shebang   # => imageless-shebang-ok 3
```

No `--` command: the shebang already said what runs, so the pod's command is
`/bin/python3 /share/imageless/hello.py`. The pod's `restartPolicy` is `Never`,
which is what makes a script that exits look like success rather than a crash
loop.

Nothing about this reaches the node. What was pushed is an ordinary seed image
carrying a generated `flake.nix`, a `flake.lock`, and the script — you can see
exactly what, and stop using the shebang, with:

```sh
./result/bin/kubectl-imageless run hello.py --emit-seed ./hello-seed
cat hello-seed/flake.nix
```

From there `run ./hello-seed --repo … -- /bin/python3 /share/imageless/hello.py`
is the step 6 path, and the generated flake is yours to edit. The lock pins the
same nixpkgs revision the examples do, so the node resolves the flake *input*
without fetching it again. Expect the first run to be slow anyway: `python3`'s
closure is about 200 MiB that nothing earlier in this walkthrough put on the
node, so container-create substitutes it from `cache.nixos.org` the same way
step 4 warns about. That is what the `--timeout=300s` above is sized for.

## Cleanup, GC, and re-runs

- Teardown: `kind delete cluster --name imageless` (removes the node and its
  store; nothing was installed on the host).
- The node's `/nix` grows on the Docker data-root (pinned nixpkgs plus the
  workload closures, a few hundred MB for the nginx demo). Safe cleanup is
  node-side GC, which honors the per-bundle roots of live containers and the
  shim's own root:

  ```sh
  docker exec imageless-control-plane \
    /nix/var/nix/gcroots/imageless-nix/bin/nix-store --gc
  ```

  That path is a GC root `setup.sh` registers for exactly this reason. It is
  *not* interchangeable with `imageless-runc`'s root: the shim and the Nix it
  drives are separate closures, and `.#imageless`'s `bin/` carries the four
  imageless binaries and no `nix-store`. Do not `docker system prune` your way
  out while the cluster exists.
- Re-running `setup.sh` after rebuilding `.#imageless` refreshes the node
  store from a fresh staging copy; store paths the node materialized on its
  own are forgotten by the imported db, so treat a re-run as node re-init
  and recreate imageless pods.

## Production is not this

This harness runs the **development posture**: `cache_only: false` with
`path:` evaluation allowed (`examples/dev-policy.json`), so the node
evaluates embedded flakes. Production nodes keep the shipped fail-closed
default (`cache_only: true`) and resolve digest-addressed releases — see
"Who runs the evaluation" in the top-level README and SPEC.md.

## k3s

See `dev/k3s/`. The template problem that kept k3s out of this document went
away: k3s's generated containerd config imports `config-v3.toml.d/*.toml`, and
a drop-in placed there adds the runtime handler without a template. That
recipe runs k3s directly on the host, not inside Docker. k3d is still not
covered.
