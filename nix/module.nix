# NixOS node setup for the imageless runtime.
#
# Daemonless by default: the module writes the node policy to
# /etc/imageless/policy.json and registers imageless-runc as a containerd
# runtime handler; the shim materializes in-process under that policy. Enable
# `services.imageless.resolver` for the hardened profile — a node daemon with
# concurrency caps, single-flight coalescing, and privilege-separated
# evaluation — selected via IMAGELESS_RESOLVER_SOCKET.
{ config, lib, pkgs, ... }:

let
  inherit (lib) mkEnableOption mkIf mkOption types;
  cfg = config.services.imageless;
  cacheType = types.submodule {
    options = {
      substituter = mkOption {
        type = types.str;
        description = "Node-authorized HTTPS or file cache URI.";
      };
      publicKeys = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Nix trusted public keys scoped to this cache identity.";
      };
    };
  };
  issuerType = types.submodule {
    options = {
      source = {
        kind = mkOption {
          type = types.enum [ "local" "https" ];
          description = "Release-manifest catalog transport.";
        };
        directory = mkOption {
          type = types.nullOr types.path;
          default = null;
          description = "Catalog directory for a local source.";
        };
        baseUrl = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "HTTPS catalog base URL for a remote source.";
        };
      };
      allowedReleases = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Exact release names or trailing-* prefixes authorized for this issuer.";
      };
      caches = mkOption {
        type = types.attrsOf cacheType;
        default = { };
        description = "Cache identities this issuer's release targets may select.";
      };
    };
  };
  policy = {
    system = cfg.system;
    cache_only = cfg.policy.cacheOnly;
    eval_allowed_uri_prefixes = cfg.policy.evalAllowedUriPrefixes;
    issuers = lib.mapAttrs
      (_: issuer: {
        source =
          if issuer.source.kind == "local" then {
            kind = "local";
            directory = toString issuer.source.directory;
          } else {
            kind = "https";
            base_url = issuer.source.baseUrl;
          };
        allowed_releases = issuer.allowedReleases;
        caches = lib.mapAttrs
          (_: cache: {
            substituter = cache.substituter;
            public_keys = cache.publicKeys;
          })
          issuer.caches;
      })
      cfg.policy.issuers;
  };
  policyFile = pkgs.writeText "imageless-policy.json" (builtins.toJSON policy);

  # containerd strips custom annotations from the container OCI spec unless
  # the runtime handler allow-lists them.
  annotations = [ "imageless.run/*" "run.imageless.*" ];

  # What the engine that execs imageless-runc must carry: the shim inherits
  # the environment of containerd, which on a k3s node is k3s itself.
  engineService = {
    path = [ cfg.package ];
    restartTriggers = [ cfg.package policyFile ];
    environment = {
      IMAGELESS_REALIZATION_TIMEOUT_SECONDS = toString cfg.realizationTimeoutSeconds;
      IMAGELESS_TELEMETRY_PATH = cfg.telemetryPath;
      IMAGELESS_STORE_PROJECTION = cfg.storeProjection;
    } // lib.optionalAttrs cfg.resolver.enable {
      IMAGELESS_RESOLVER_SOCKET = cfg.resolver.socketPath;
    };
    requires = mkIf cfg.resolver.enable [ "imageless-resolver.service" ];
    after = mkIf cfg.resolver.enable [ "imageless-resolver.service" ];
  };

  # k3s regenerates containerd's config.toml on every start, but the config it
  # renders for containerd 2.x imports config-v3.toml.d/*.toml and containerd
  # merges imported plugin tables into its own (dev/k3s/README.md). The
  # handler therefore goes in a drop-in, and k3s's base config is untouched.
  k3sDropIn = (pkgs.formats.toml { }).generate "imageless.toml" {
    version = 3;
    plugins."io.containerd.cri.v1.runtime".containerd.runtimes.${cfg.runtimeHandler} = {
      runtime_type = "io.containerd.runc.v2";
      pod_annotations = annotations;
      container_annotations = annotations;
      options = {
        BinaryName = "${cfg.package}/bin/imageless-runc";
        SystemdCgroup = cfg.k3s.systemdCgroup;
      };
    };
  };
in
{
  options.services.imageless = {
    enable = mkEnableOption "the imageless container runtime";

    package = mkOption {
      type = types.package;
      description = "Package providing imageless-runc, imageless-resolver, and imageless-dev-resolver.";
    };

    runtimeHandler = mkOption {
      type = types.str;
      default = "imageless";
      description = "Name of the containerd runtime handler (and RuntimeClass handler) to register.";
    };

    realizationTimeoutSeconds = mkOption {
      type = types.ints.between 1 3600;
      default = 300;
      description = "End-to-end materialization deadline in seconds.";
    };

    storeProjection = mkOption {
      type = types.enum [ "node" "closure" ];
      default = "node";
      description = ''
        How the node /nix/store is projected into materialized containers.
        "node" binds the whole store read-only; "closure" binds only the
        realized root's closure, one read-only mount per store path, so a
        workload never sees unrelated node store paths.
      '';
    };

    telemetryPath = mkOption {
      type = types.str;
      default = "/run/imageless/timings.jsonl";
      description = "Root-owned JSON-lines sink for per-stage runtime timings.";
    };

    system = mkOption {
      type = types.str;
      default = pkgs.stdenv.hostPlatform.system;
      defaultText = lib.literalExpression "pkgs.stdenv.hostPlatform.system";
      description = "Nix system selected from multi-architecture release manifests.";
    };

    policy = {
      cacheOnly = mkOption {
        type = types.bool;
        default = true;
        description = ''
          When true (the fail-closed default) the node never evaluates a
          flake: only digest-addressed releases from configured issuers
          resolve. Set false to allow node-side evaluation of embedded flakes
          and allow-listed external references.
        '';
      };
      evalAllowedUriPrefixes = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "path:" "github:yourorg/" ];
        description = ''
          URI prefixes accepted for flake evaluation when cacheOnly is false.
          `path:` authorizes embedded flakes (the runtime's own rewrite of
          staged in-image sources — never a node path named by an annotation);
          a remote prefix such as `github:yourorg/` additionally authorizes
          external flake references (SPEC.md §3). Matching is a literal string
          prefix, so terminate entries at a boundary: `github:yourorg` would
          also authorize `github:yourorg-evil/anything`.
        '';
      };
      issuers = mkOption {
        type = types.attrsOf issuerType;
        default = { };
        description = "Release issuers, name policy, and cache trust (SPEC.md §6).";
      };
    };

    containerd.enable = mkOption {
      type = types.bool;
      default = !cfg.k3s.enable;
      defaultText = lib.literalExpression "!config.services.imageless.k3s.enable";
      description = ''
        Register the runtime handler under containerd's stock io.containerd.runc.v2
        shim with BinaryName pointed at imageless-runc, and allow-list the
        imageless annotation namespaces. Disable when wiring a different
        engine (for example a Docker runtime) by hand.
      '';
    };

    k3s = {
      enable = mkEnableOption ''
        the runtime handler in the containerd that k3s embeds, through a
        drop-in at /var/lib/rancher/k3s/agent/etc/containerd/config-v3.toml.d/
        (k3s releases on containerd 2.x). Servers and agents alike, since both
        run containerd. Implies containerd.enable = false by default, because
        the node's containerd is k3s's, not the NixOS one'';

      systemdCgroup = mkOption {
        type = types.bool;
        default = true;
        description = ''
          SystemdCgroup for the handler's runc options. It must match the
          cgroup driver k3s picks for its own runc handler, or every pod on
          this handler fails at FailedCreatePodSandBox. k3s picks systemd
          when it runs as a systemd unit (INVOCATION_ID set) on a host with
          the cpuset controller, outside a user namespace, which is what
          services.k3s gives on a cgroup v2 NixOS host. Set false when k3s
          uses cgroupfs.
        '';
      };
    };

    resolver = {
      enable = mkEnableOption "the optional imageless-resolver node daemon";

      socketPath = mkOption {
        type = types.str;
        default = "/run/imageless/resolver.sock";
        description = "Root-owned Unix socket the shim uses to reach the daemon.";
      };

      maxRealizations = mkOption {
        type = types.ints.between 1 64;
        default = 2;
        description = "Maximum concurrent Nix operations across the node.";
      };
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.policy.cacheOnly || cfg.policy.evalAllowedUriPrefixes != [ ];
        message = "services.imageless.policy.evalAllowedUriPrefixes must be non-empty when cacheOnly is false";
      }
      {
        assertion = !cfg.k3s.enable || config.services.k3s.enable;
        message = "services.imageless.k3s.enable requires services.k3s.enable";
      }
      {
        assertion = !(cfg.k3s.enable && cfg.containerd.enable);
        message = "services.imageless.k3s.enable and containerd.enable are exclusive: k3s runs its own containerd";
      }
    ] ++ lib.flatten (lib.mapAttrsToList
      (name: issuer: [
        {
          assertion =
            (issuer.source.kind == "local" && issuer.source.directory != null && issuer.source.baseUrl == null)
            || (issuer.source.kind == "https" && issuer.source.baseUrl != null && issuer.source.directory == null);
          message = "services.imageless.policy.issuers.${name}.source must set exactly the field selected by kind";
        }
        {
          assertion = issuer.allowedReleases != [ ];
          message = "services.imageless.policy.issuers.${name}.allowedReleases must be non-empty";
        }
        {
          assertion = issuer.caches != { };
          message = "services.imageless.policy.issuers.${name}.caches must be non-empty";
        }
      ])
      cfg.policy.issuers);

    environment.systemPackages = [ cfg.package ];

    # The single source of node policy for both materializer modes: the shim
    # reads it in-process by default path, the daemon is pointed at the same
    # file. The policy loader fail-closed rejects symlinks, so this must be a
    # real root-owned regular file — a tmpfiles copy, not an environment.etc
    # symlink.
    systemd.tmpfiles.rules = [
      "d /etc/imageless 0755 root root -"
      "C+ /etc/imageless/policy.json - - - - ${policyFile}"
      "z /etc/imageless/policy.json 0600 root root - -"
      "d /run/imageless 0700 root root -"
      "f ${cfg.telemetryPath} 0600 root root -"
    ];

    virtualisation.containerd = mkIf cfg.containerd.enable {
      enable = lib.mkDefault true;
      # Interpose at `runc create` under the STOCK runc-v2 shim. Under
      # containerd 2.x every pod runs a single shim and app containers are
      # created via ttrpc into it, so a custom runtime-v2 binary would only
      # ever observe the pod sandbox; the OCI runtime binary still execs once
      # per container in every containerd generation (SPEC.md §5).
      settings.plugins."io.containerd.grpc.v1.cri".containerd.runtimes.${cfg.runtimeHandler} = {
        runtime_type = "io.containerd.runc.v2";
        options.BinaryName = "${cfg.package}/bin/imageless-runc";
        pod_annotations = annotations;
        container_annotations = annotations;
      };
    };

    systemd.services.containerd = mkIf cfg.containerd.enable engineService;

    # A symlink into the store, the way services.k3s places its own
    # containerd template; parent directories are created as needed.
    systemd.tmpfiles.settings.imageless-k3s = mkIf cfg.k3s.enable {
      "/var/lib/rancher/k3s/agent/etc/containerd/config-v3.toml.d/imageless.toml"."L+".argument =
        "${k3sDropIn}";
    };

    systemd.services.k3s = mkIf cfg.k3s.enable (engineService // {
      restartTriggers = engineService.restartTriggers ++ [ k3sDropIn ];
    });

    # The daemon refuses evaluation without an unprivileged worker, so the
    # development evaluator user exists exactly when the resolver may evaluate.
    users.groups.imageless-dev = mkIf (cfg.resolver.enable && !cfg.policy.cacheOnly) { };
    users.users.imageless-dev = mkIf (cfg.resolver.enable && !cfg.policy.cacheOnly) {
      isSystemUser = true;
      group = "imageless-dev";
      description = "Unprivileged imageless development-source evaluator";
    };

    systemd.services.imageless-resolver = mkIf cfg.resolver.enable {
      description = "imageless rootfs resolver";
      wantedBy = [ "multi-user.target" ];
      restartTriggers = [ cfg.package policyFile ];
      serviceConfig = {
        Type = "simple";
        User = "root";
        Group = "root";
        ExecStart = lib.escapeShellArgs ([
          "${cfg.package}/bin/imageless-resolver"
          "--socket-path"
          cfg.resolver.socketPath
          "--max-realizations"
          (toString cfg.resolver.maxRealizations)
          "--realization-timeout-seconds"
          (toString cfg.realizationTimeoutSeconds)
          "--policy-file"
          "/etc/imageless/policy.json"
        ] ++ lib.optionals (!cfg.policy.cacheOnly) [
          "--development-worker"
          "${cfg.package}/bin/imageless-dev-resolver"
          "--development-worker-user"
          "imageless-dev"
        ]);
        Restart = "always";
        RestartSec = "1s";
        KillMode = "control-group";
        TimeoutStopSec = "15s";
        UMask = "0077";
        RuntimeDirectory = "imageless";
        RuntimeDirectoryMode = "0700";
        NoNewPrivileges = true;
        PrivateTmp = true;
      };
    };
  };
}
