# Dev Kubernetes node for the imageless runtime (k3s)

A single-node [k3s](https://k3s.io/) cluster on this host, running flakes
through the imageless `RuntimeClass`. It is the k3s counterpart of
`dev/kind/`: same policy, same seed image, same pod.

**It modifies the host.** k3s runs directly on the machine, and `setup.sh`
writes to `/etc/imageless`, `/etc/rancher`, `/usr/local/bin` and
`/var/lib/rancher`. Use a VM or a machine you can throw away. `dev/kind/` is
the path that leaves the host's `/etc` alone.

**Requirements:** a Linux x86_64 host with Nix (flakes enabled) and a k3s
binary. Every command runs as root, from the repository root.

**Verified** end to end with k3s v1.35.5+k3s1 and containerd 2.2.3, on a
cgroup v1 host that withholds `CAP_SYS_RESOURCE` from root. That covers every
step below, including the plugin's push path and node-side GC.

## Why drop-ins instead of a template

k3s regenerates containerd's `config.toml` every time it starts, and the
documented way to customize that file is a `config*.toml.tmpl`. The right
template depends on the containerd generation of each k3s release, which is
why this port was deferred (see `ROADMAP.md`).

A template turns out to be unnecessary. The config k3s writes contains
`imports = [".../config-v3.toml.d/*.toml"]`, and containerd merges the plugin
tables it imports into its own. So one file in that directory adds the
`imageless` handler:

- the file survives every restart;
- the base config is left exactly as k3s renders it;
- there is nothing to keep in step with the base config across k3s releases.

Releases still on containerd 1.x write a v2 config and would need the
`plugins."io.containerd.grpc.v1.cri"` table instead. Those releases are not
covered here.

The node uses the host's own `/nix`. Nothing is imported, because k3s's
containerd, the bundles and the GC already share one mount namespace. That
shared namespace is what the GC-root guarantee depends on: a live container
survives `nix-collect-garbage`.

**On NixOS, use the module instead of this script.** `services.imageless.k3s.enable`
(next to `services.k3s.enable`) writes the same drop-in with both annotation
families and `SystemdCgroup = true`, and puts the shim's environment on the
`k3s` unit, whose containerd execs the shim. It turns the module's own
containerd off, since the node's containerd is k3s's.

## 1. Prepare the node

```sh
dev/k3s/setup.sh --registry
```

The script:

- builds `.#imageless`;
- adds GC roots for the shim and the Nix it drives;
- links the shim at `/usr/local/bin/imageless-runc`, the `BinaryName` the
  handler names;
- installs `examples/dev-policy.json` at `/etc/imageless/policy.json`,
  root-owned with mode 0600. It refuses to replace a different policy that is
  already there.

It then installs these files:

- **`containerd/imageless.toml`**, the runtime handler.
- **`containerd/restrict-oom.toml`**, only when this host withholds
  `CAP_SYS_RESOURCE`. Without the capability, kubelet cannot set the -998 OOM
  score it asks for on pod sandboxes, and every pod fails with
  `failed to update /proc/self/oom_score_adj: Permission denied`.
- **`registries.yaml`**, with `--registry`, so that the node pulls
  `127.0.0.1:5000` over plain HTTP.

The last line it prints is the `k3s server` command for this host. That
command includes `--kubelet-arg=fail-cgroupv1=false` when the host uses
cgroup v1, which kubelet 1.35 otherwise refuses.

## 2. Start k3s

Run the command `setup.sh` printed. For example:

```sh
k3s server --disable=traefik,metrics-server --write-kubeconfig-mode=644 \
  --kubelet-arg=fail-cgroupv1=false &
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
kubectl wait --for=condition=Ready node --all --timeout=180s
k3s crictl info | jq '.config.containerd.runtimes | keys'   # includes "imageless"
```

If `crictl info` lists `imageless`, the merge worked. `config.toml` itself
does not mention imageless, because the drop-in is merged at load time and
never written into it.

## 3. Run the embedded-flake pod

```sh
gunzip -c "$(nix build .#nginx-embedded-image --no-link --print-out-paths)" \
  | k3s ctr -n k8s.io images import -
kubectl label node --all imageless.run/runtime=v2 --overwrite
kubectl apply -f examples/runtimeclass.yaml -f dev/kind/pod-nginx-embedded.yaml
kubectl wait --for=condition=Ready pod/imageless-nginx --timeout=600s
curl -s "http://$(kubectl get pod imageless-nginx -o jsonpath='{.status.podIP}'):18080/"
```

Expected output: `imageless-nginx-ok`. On this host the pod became Ready in
about 11–15 s with the nixpkgs input already in the store. The image holds
only `flake.nix` and `flake.lock`. The same image under stock runc fails with
`exec: "/bin/nginx": stat /bin/nginx: no such file or directory`.

## 4. Push your own flake with the plugin

This step needs a registry listening on `127.0.0.1:5000`. `setup.sh
--registry` pointed k3s at it before k3s started. `registry:2` works, and so
does the `distribution` package from nixpkgs.

```sh
cp -r examples/nginx-embedded /tmp/app && chmod -R u+w /tmp/app
sed -i 's/imageless-nginx-ok/hello from k3s/' /tmp/app/flake.nix
"$(nix build .#kubectl-imageless --no-link --print-out-paths)/bin/kubectl-imageless" \
  run /tmp/app --name hello --repo 127.0.0.1:5000/team/hello --writable /tmp \
  -- /bin/nginx -c /etc/nginx/nginx.conf \
  | kubectl apply -f -
```

`--writable /tmp` gives the pod an emptyDir at `/tmp`, which this nginx
config writes to. The materialized root is read-only (SPEC §4.4), so any path
a workload writes has to come from a mount.

## 5. Check the GC-root lifecycle

```sh
cid=$(k3s crictl ps -q --name '^nginx$' | head -1)
root=$(jq -r .root.path /run/k3s/containerd/io.containerd.runtime.v2.task/k8s.io/$cid/config.json)
nix-store --gc; test -e "$root" && echo "survived GC while running"
kubectl delete pod imageless-nginx; nix-store --gc; test -e "$root" || echo "collected after delete"
```

Read the root path from the bundle's `config.json`. `crictl inspect` shows
containerd's spec before the rewrite, where the root path is still the
relative `rootfs`.

## Troubleshooting on unusual hosts

- **`ErrImageNeverPull` after an import that succeeded.** Kubelet's image GC
  removed the image before the pod could use it. This happens on hosts whose
  disk quota makes `statfs` report a nearly full volume. The sandbox this
  recipe was verified on reported 95% of 252 GiB as used while only 23 GiB
  actually were. Start k3s with
  `--kubelet-arg=image-gc-high-threshold=100 --kubelet-arg=image-gc-low-threshold=99 '--kubelet-arg=eviction-hard=nodefs.available<1%,imagefs.available<1%'`,
  or use the registry path in step 4, which re-pulls instead of failing.
- **`FailedCreatePodSandBox` on the imageless handler only.** The
  `SystemdCgroup` value in `containerd/imageless.toml` does not match the
  cgroup driver k3s chose for its own runc handler. k3s chooses systemd only
  when it runs as a systemd unit (`INVOCATION_ID` is set), the cpuset
  controller is present, and it is not in a user namespace. This recipe starts
  k3s from a shell, so `false` is right here; set `true` for k3s under
  systemd.
- **`kubectl imageless run --external` fails, saying the annotation was
  dropped.** The drop-in allow-lists `imageless.run/*` only, as
  `dev/kind` does, so containerd strips `run.imageless.source`. Add
  `"run.imageless.*"` to both annotation lists, and use a policy whose
  prefixes cover the reference (`examples/external-refs-policy.json`).
- **An agent exits with `flag provided but not defined`.** `disable` and
  `tls-san` are server flags; keep them out of an agent's `config.yaml`.
- **Agents dial the wrong API address.** With `node-external-ip` set, a
  server advertises the API on that address unless `advertise-address` is
  set too.
- **Evaluation fails with `Operation not permitted`.** Node-side evaluation
  runs in a private mount namespace (SPEC §2.4). A host that forbids creating
  one must set `unconfined_evaluation` in its policy.

## Teardown

```sh
k3s-killall.sh   # or stop `k3s server` and its containerd shims
rm -rf /var/lib/rancher/k3s /etc/rancher/k3s /etc/imageless/policy.json \
  /usr/local/bin/imageless-runc /nix/var/nix/gcroots/imageless-runc \
  /nix/var/nix/gcroots/imageless-nix
```
