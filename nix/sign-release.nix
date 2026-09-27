# Signs a release catalog (SPEC.md §6.1) outside the Nix store.
#
# nix/release-catalog.nix builds catalogs in the store, where no signing key
# may ever be: a store path is world-readable and outlives every secret it
# touches. So signing is a separate step, run where the key is:
#
#   nix run .#imageless-sign-release -- --secret-key /run/secrets/release.key \
#     --public-key release.pub "$(nix build .#my-release --print-out-paths)" ./publish
#
# It copies the catalog into the output directory (which may already hold
# earlier releases: a publish directory accumulates), checks that every
# manifest's bytes match its digest name, and writes a minisign sidecar beside
# each manifest that has none yet. `--resign` replaces existing sidecars, for
# key rotation. With `--public-key`, every signature is verified afterwards.
{ writeShellApplication, minisign, coreutils, findutils }:

writeShellApplication {
  name = "imageless-sign-release";
  runtimeInputs = [ minisign coreutils findutils ];
  text = ''
    usage() {
      echo "usage: imageless-sign-release --secret-key FILE [--public-key FILE] [--trusted-comment TEXT] [--resign] CATALOG OUT" >&2
      exit 2
    }
    secret=''' public=''' comment=''' resign=''' store_key_ok='''
    positional=()
    while [ $# -gt 0 ]; do
      case $1 in
        --secret-key) secret=''${2:?}; shift 2 ;;
        --public-key) public=''${2:?}; shift 2 ;;
        --trusted-comment) comment=''${2:?}; shift 2 ;;
        --resign) resign=1; shift ;;
        # For test fixtures only: a key committed to a repository and read
        # from the store protects nothing, which is fine for a smoke test and
        # nowhere else.
        --insecure-key-in-store) store_key_ok=1; shift ;;
        --) shift; positional+=("$@"); break ;;
        -*) usage ;;
        *) positional+=("$1"); shift ;;
      esac
    done
    [ ''${#positional[@]} -eq 2 ] && [ -n "$secret" ] || usage
    catalog=''${positional[0]} out=''${positional[1]}

    key_path=$(realpath -e "$secret")
    case $key_path in
      /nix/store/*)
        if [ -z "$store_key_ok" ]; then
          echo "refusing a secret key in the Nix store ($key_path): anyone who can read the store can sign releases" >&2
          exit 1
        fi ;;
    esac
    [ -d "$catalog/sha256" ] || { echo "$catalog has no sha256/ directory: not a release catalog" >&2; exit 1; }

    mkdir -p "$out"
    if [ "$(realpath "$catalog")" != "$(realpath "$out")" ]; then
      # Copy content, not store permissions: the output must stay writable so
      # a sidecar can land beside each manifest.
      cp -r --no-preserve=mode,ownership,timestamps "$catalog"/. "$out"/
    fi

    signed=0
    for manifest in "$out"/sha256/*.json; do
      [ -e "$manifest" ] || continue
      name=$(basename "$manifest" .json)
      actual=$(sha256sum "$manifest" | cut -d' ' -f1)
      if [ "$actual" != "$name" ]; then
        echo "$manifest: bytes hash to $actual, not its name; refusing to sign it" >&2
        exit 1
      fi
      if [ -e "$manifest.minisig" ] && [ -z "$resign" ]; then
        continue
      fi
      minisign -S -s "$secret" -m "$manifest" -x "$manifest.minisig.tmp" \
        -t "''${comment:-imageless release sha256:$name}" >/dev/null
      mv "$manifest.minisig.tmp" "$manifest.minisig"
      signed=$((signed + 1))
    done

    if [ -n "$public" ]; then
      for manifest in "$out"/sha256/*.json; do
        [ -e "$manifest" ] || continue
        minisign -V -q -p "$public" -m "$manifest" || {
          echo "$manifest: signature does not verify under $public" >&2
          exit 1
        }
      done
    fi
    echo "signed $signed manifest(s) in $out" >&2
  '';
}
