#!/usr/bin/env bash
# Prepare THIS host as a single-node k3s imageless node: the production shim
# and the nix it drives (GC-rooted in the host store), the dev policy, and the
# containerd drop-ins k3s merges at start. Run it before `k3s server`, as root,
# on a host you can throw away — unlike dev/kind, this writes to /etc,
# /usr/local/bin and /var/lib/rancher.
#
# The node uses the host's own /nix: k3s's containerd, the bundles and the GC
# share one mount namespace, which is what the GC-root guarantee (a live
# container survives nix-collect-garbage) requires. No store copy, no import.
#
# Usage: dev/k3s/setup.sh [--registry] [--force-policy]
#   --registry      also install registries.yaml (plain-HTTP 127.0.0.1:5000)
#   --force-policy  replace an existing /etc/imageless/policy.json
#
# Idempotent. It prints the `k3s server` command for this host at the end.
set -euo pipefail

registry=0
force_policy=0
for argument in "$@"; do
  case "$argument" in
    --registry) registry=1 ;;
    --force-policy) force_policy=1 ;;
    *)
      echo "unknown argument: $argument (usage: setup.sh [--registry] [--force-policy])" >&2
      exit 2
      ;;
  esac
done
if [ "$(id -u)" != 0 ]; then
  echo "run as root: the node policy, the containerd drop-ins and k3s itself are root's" >&2
  exit 1
fi
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
dropins=/var/lib/rancher/k3s/agent/etc/containerd/config-v3.toml.d

echo "==> building .#imageless (the production shim and its baked nix)"
build="$(nix build "$repo#imageless" --no-link --print-out-paths)"
node_nix="$(nix eval --raw "$repo#imageless.materializerNix.outPath")"

echo "==> rooting the shim and its nix, and linking the shim for BinaryName"
# Workload closures are rooted per bundle by the runtime; these two roots keep
# the runtime itself alive across node-side GC.
nix-store --realise "$build" --add-root /nix/var/nix/gcroots/imageless-runc >/dev/null
nix-store --realise "$node_nix" --add-root /nix/var/nix/gcroots/imageless-nix >/dev/null
ln -sfn "$build/bin/imageless-runc" /usr/local/bin/imageless-runc

echo "==> installing the dev policy"
policy=/etc/imageless/policy.json
if [ -e "$policy" ] && ! cmp -s "$policy" "$repo/examples/dev-policy.json" && [ "$force_policy" = 0 ]; then
  echo "$policy exists and is not examples/dev-policy.json; refusing to replace a" >&2
  echo "node's policy (pass --force-policy on a host that is really disposable)" >&2
  exit 1
fi
# Root-owned 0600: the production runtime's ownership check fails closed on
# anything else.
install -D -m 0600 -o 0 -g 0 "$repo/examples/dev-policy.json" "$policy"

echo "==> installing containerd drop-ins into $dropins"
install -D -m 0644 "$here/containerd/imageless.toml" "$dropins/imageless.toml"
# CAP_SYS_RESOURCE is bit 24 of the bounding set.
bounding="$(awk '/^CapBnd:/ { print $2 }' /proc/self/status)"
if (( (16#$bounding >> 24 & 1) == 0 )); then
  echo "    this host withholds CAP_SYS_RESOURCE: adding restrict-oom.toml"
  install -m 0644 "$here/containerd/restrict-oom.toml" "$dropins/restrict-oom.toml"
else
  rm -f "$dropins/restrict-oom.toml"
fi

if [ "$registry" = 1 ]; then
  echo "==> installing registries.yaml (plain-HTTP 127.0.0.1:5000)"
  install -D -m 0644 "$here/registries.yaml" /etc/rancher/k3s/registries.yaml
fi

flags="--disable=traefik,metrics-server --write-kubeconfig-mode=644"
if [ "$(stat -fc %T /sys/fs/cgroup)" != cgroup2fs ]; then
  # kubelet >= 1.35 refuses cgroup v1 unless told otherwise.
  flags="$flags --kubelet-arg=fail-cgroupv1=false"
fi
echo "==> node prepared; start k3s with:"
echo "    k3s server $flags"
