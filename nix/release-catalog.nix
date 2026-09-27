# Canonical, digest-addressed release-manifest catalog (SPEC.md §6). The
# target store paths deliberately have their Nix string context discarded:
# publishing metadata must not force one build machine to realise every
# architecture. Each target closure is built and pushed to its named cache by
# whatever release pipeline the issuer runs — any CI that can copy a closure
# and emit this JSON conforms.
#
# The catalog this builds is unsigned, because it lives in the store and a
# signing key never may. Nodes require signatures by default (SPEC.md §6.1),
# so sign the built catalog outside the store with nix/sign-release.nix
# (`nix run .#imageless-sign-release`).
{ lib
, runCommand
, writeText
, rootfsTargets
, issuer
, releaseName
, cache ? "default"
, process ? null
, channels ? [ ]
}:

let
  manifest = {
    schema = "imageless.release.v1";
    inherit issuer;
    name = releaseName;
    targets = lib.mapAttrs
      (_: rootfs: {
        rootfs = builtins.unsafeDiscardStringContext (toString rootfs);
        inherit cache;
      } // lib.optionalAttrs (process != null) { inherit process; })
      rootfsTargets;
  };
  # `builtins.toJSON` emits compact JSON with lexicographically sorted object
  # keys, matching the resolver's canonical serializer.
  json = builtins.toJSON manifest;
  digest = builtins.hashString "sha256" json;
  reference = "${issuer}/${releaseName}@sha256:${digest}";
  # Written through `writeText` rather than echoed by the builder so the file's
  # bytes are exactly the ones hashed above: a node validates the fetched
  # manifest against its digest, so a stray newline would fail resolution.
  manifestFile = writeText "imageless-release-manifest" json;
in
runCommand "imageless-release-${issuer}"
{
  passthru = { inherit digest manifest reference channels; };
}
  (''
    mkdir -p "$out/sha256"
    cp ${manifestFile} "$out/sha256/${digest}.json"
  ''
  # The name/channel index (SPEC.md §6): a pointer is 64 lowercase hex digits
  # and nothing else. It exists so client-side tooling — `kubectl imageless
  # pin`, or `run --release` — can turn a human-friendly name into the pinned
  # reference a node accepts. Nodes MUST ignore it, so publishing a channel is
  # a convenience for authors and never part of what a running pod resolves.
  #
  # Republishing a channel is the whole point of having one: it changes what
  # the next `pin` returns, and cannot change any pod already admitted, because
  # the pod records the digest rather than the channel.
  + lib.concatMapStrings
    (channel: ''
      mkdir -p "$out/refs/${releaseName}"
      printf '%s\n' "${digest}" > "$out/refs/${releaseName}/${channel}"
    '')
    channels)
