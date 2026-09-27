{
  description = "imageless — ship the flake, not the filesystem: OCI images that materialize their rootfs from an embedded Nix flake at container-create time";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/8c3cede7ddc26bd659d2d383b5610efbd2c7a16e";
    # Compatibility-matrix leg (ROADMAP v0.2): the last nixpkgs generation
    # still carrying containerd 1.x. Used only for the containerd package in
    # the 1.x CRI gate — everything else in that VM comes from the primary
    # pin, so the gate isolates exactly one variable.
    nixpkgs-containerd1.url = "github:nixos/nixpkgs/50ab793786d9de88ee30ec4e4c24fb4236fc2674";
  };

  outputs = { self, nixpkgs, nixpkgs-containerd1 }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      nixosModules.imageless = { pkgs, lib, ... }: {
        imports = [ ./nix/module.nix ];
        services.imageless.package =
          lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.imageless;
      };

      overlays.default = final: _prev: {
        imageless = final.callPackage ./nix/package.nix { vendoredNixpkgs = nixpkgs; };
      };

      # Full per-system set, INCLUDING the two NixOS VM acceptance gates. This
      # is not a standard flake output name, so `nix flake check` never
      # evaluates it — `packages` (below) drops the VM gates and `legacyPackages`
      # re-exposes them without duplicating their definitions.
      allPackages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;

          # The whole workspace in one package: the shim, the optional
          # resolver daemon, and the development evaluator. The NixOS module
          # points every consumer here.
          imageless = pkgs.callPackage ./nix/package.nix { vendoredNixpkgs = nixpkgs; };

          # Same workspace built with the `inline-policy` feature, for the dev
          # Docker harness only (a root daemon reads policy from
          # `IMAGELESS_POLICY_JSON` — see dev/docker/README.md). Never deploy it
          # to a shared node: production `.#imageless` cannot read env policy.
          imageless-dev = pkgs.callPackage ./nix/package.nix { inlinePolicy = true; vendoredNixpkgs = nixpkgs; };

          # Single-binary views for the quick starts (`nix build .#imageless-runc`).
          binView = name: pkgs.runCommand name
            { meta.mainProgram = name; }
            ''
              # A dirty-tree build that omitted the binary (e.g. an untracked
              # crate) must fail here, not ship a dangling symlink.
              test -x ${imageless}/bin/${name}
              mkdir -p $out/bin
              ln -s ${imageless}/bin/${name} $out/bin/${name}
            '';
          imageless-runc = binView "imageless-runc";
          imageless-resolver = binView "imageless-resolver";
          kubectl-imageless = binView "kubectl-imageless";

          # -- Docker embedded-layer acceptance gate (SPEC.md §7.1) ---------
          # The seed carries ONLY the flake and its inputs: no metadata file,
          # no labels — presence of etc/imageless/flake.nix is the contract.
          # The seed's top level deliberately has no /bin/busybox, so the
          # workload cannot pass by running unchanged.
          docker-embedded-seed = pkgs.runCommand "imageless-docker-embedded-seed" { } ''
            root=$out/etc/imageless/rootfs
            mkdir -p $root/bin $root/dev $root/etc $root/nix/store $root/proc \
              $root/sys $root/tmp $root/www
            cp ${pkgs.pkgsStatic.busybox}/bin/busybox $root/bin/busybox
            printf 'imageless-docker-embedded-ok\n' > $root/www/index.html
            printf 'root:x:0:0:root:/tmp:/bin/busybox\n' > $root/etc/passwd
            printf 'root:x:0:\n' > $root/etc/group
            touch $root/etc/hostname $root/etc/hosts $root/etc/resolv.conf
            printf '%s\n' '{' \
              '  outputs = { self }: {' \
              '    rootfs = builtins.path {' \
              '      path = ./rootfs;' \
              '      name = "imageless-docker-embedded-rootfs";' \
              '    };' \
              '  };' \
              '}' > $out/etc/imageless/flake.nix
          '';
          docker-embedded-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-docker-embedded";
            tag = "e2e";
            copyToRoot = docker-embedded-seed;
            config = {
              Entrypoint = [ "/bin/busybox" "httpd" "-f" "-p" "18080" "-h" "/www" ];
              ExposedPorts."18080/tcp" = { };
            };
          };

          # A real dynamically-linked service (nginx) shipped the imageless
          # way. The seed carries ONLY examples/nginx-embedded/{flake.nix,
          # flake.lock} — no nginx, no closure, a few kilobytes with ZERO
          # /nix/store references (so dockerTools cannot drag a closure into
          # the layer). The node evaluates the embedded flake's SPARSE `#rootfs`
          # at container-create, materializes nginx from its own cache, and
          # binds /nix/store read-only (StoreProjection::Node) so the loader
          # resolves. This exercises the true late-bound path — unlike the
          # static-busybox seed above, which ships its own filesystem. nginx
          # needs writable pid/temp dirs under the readonly root, so run it with
          # `--tmpfs /tmp`.
          nginx-embedded-seed = pkgs.runCommand "imageless-nginx-embedded-seed" { } ''
            mkdir -p $out/etc/imageless
            cp ${./examples/nginx-embedded/flake.nix} $out/etc/imageless/flake.nix
            cp ${./examples/nginx-embedded/flake.lock} $out/etc/imageless/flake.lock
          '';
          nginx-embedded-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-nginx-embedded";
            tag = "e2e";
            copyToRoot = nginx-embedded-seed;
            config = {
              # -e redirects the compile-time default error log (opened before
              # the config's error_log directive applies) off the read-only
              # /var/log/nginx path to the container's stderr.
              Entrypoint = [ "/bin/nginx" "-e" "/proc/self/fd/2" "-c" "/etc/nginx/nginx.conf" ];
              ExposedPorts."18080/tcp" = { };
            };
          };
          # The tiny CRI placeholder for release-profile pods
          # (examples/pod-imageless.yaml): the release manifest supplies the
          # real rootfs and process metadata, the image is only a pull target.
          placeholder-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-placeholder";
            tag = "v1";
            config.Cmd = [ "/imageless-placeholder" ];
          };
          # An ordinary image with no embedded flake: must pass through the
          # imageless runtime completely unchanged.
          docker-passthrough-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-passthrough";
            tag = "e2e";
            copyToRoot = pkgs.runCommand "imageless-passthrough-root" { } ''
              mkdir -p $out/bin
              cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/bin/busybox
              ln -s busybox $out/bin/true
            '';
            config.Cmd = [ "/bin/true" ];
          };
          docker-embedded-scenario = pkgs.writeShellApplication {
            name = "imageless-docker-embedded-scenario";
            runtimeInputs = [ pkgs.coreutils pkgs.curl pkgs.docker-client pkgs.gnused pkgs.jq ];
            text = ''
              exec bash ${./crates/imageless-runc/tests/docker-embedded-e2e.sh} "$@"
            '';
          };
          # Scaffolding for the isolated harness below, not a subject under
          # test: the smallest image that can cross into the host bind at
          # /host. Its busybox mounts the private daemon's tmpfs roots and then
          # chroots into /host, where every binary the test actually exercises
          # already lives — so nothing else belongs in this layer.
          docker-helper-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-docker-helper";
            tag = "e2e";
            copyToRoot = pkgs.runCommand "imageless-docker-helper-root" { } ''
              mkdir -p $out/bin
              cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/bin/busybox
              # The only applet Docker resolves through PATH is the `sh` in the
              # container's argv; the helper spells every other one as
              # `/bin/busybox NAME`.
              ln -s busybox $out/bin/sh
            '';
            config.Env = [ "PATH=/bin" ];
          };
          # The same scenario against a PRIVATE dockerd this harness boots and
          # destroys: `nix run .#docker-embedded-isolated`. It is
          # docker-embedded-smoke without the VM — the Docker acceptance path on
          # a workstation with no KVM, and without the host's daemon ever seeing
          # the workload. The automated counterpart of dev/docker/README.md.
          #
          # A writeShellApplication on purpose. `nix flake check` builds every
          # package, and building this one only writes and shellchecks a script;
          # the Docker socket and the privileges are needed at RUN time, which
          # check never reaches. Keep it that way — do not wrap it in a
          # runCommand or a VM test that executes it.
          docker-embedded-isolated = pkgs.writeShellApplication {
            name = "imageless-docker-embedded-isolated";
            runtimeInputs = [ pkgs.coreutils pkgs.docker-client pkgs.jq pkgs.procps ];
            text = ''
              export IMAGELESS_DOCKER_IMAGE_ARCHIVE=''${IMAGELESS_DOCKER_IMAGE_ARCHIVE:-${docker-embedded-image}}
              export IMAGELESS_DOCKER_HELPER_ARCHIVE=${docker-helper-image}
              export IMAGELESS_DOCKERD=${pkgs.docker}/bin/dockerd
              export IMAGELESS_CONTAINERD_BIN=${pkgs.containerd}/bin
              # The delegate imageless-runc execs, overriding the path baked in
              # at build time, so the shim and the interposer agree on one runc.
              export IMAGELESS_STOCK_RUNC=${pkgs.runc}/bin/runc
              export IMAGELESS_RESOLVER=${imageless}/bin/imageless-resolver
              export IMAGELESS_DEV_RESOLVER=${imageless}/bin/imageless-dev-resolver
              export IMAGELESS_RUNC_CLIENT=${imageless}/bin/imageless-runc
              export IMAGELESS_DOCKER_SCENARIO=${docker-embedded-scenario}/bin/imageless-docker-embedded-scenario
              exec bash ${./crates/imageless-runc/tests/docker-embedded-isolated-e2e.sh} "$@"
            '';
          };
          # Hermetic VM proof of the DAEMONLESS path: raw Docker, the shim
          # registered as a Docker runtime, node policy from
          # /etc/imageless/policy.json, and no resolver daemon anywhere in
          # the VM. Materialization happens in-process inside imageless-runc.
          docker-embedded-smoke = pkgs.testers.runNixOSTest {
            name = "imageless-docker-embedded";
            nodes.node = { ... }: {
              imports = [ self.nixosModules.imageless ];
              services.imageless = {
                enable = true;
                # Docker is wired by hand below; keep containerd out of it.
                containerd.enable = false;
                policy.cacheOnly = false;
                policy.evalAllowedUriPrefixes = [ "path:" ];
              };
              virtualisation.docker = {
                enable = true;
                daemon.settings.runtimes.imageless.path =
                  "${imageless}/bin/imageless-runc";
              };
              virtualisation = {
                writableStore = true;
                memorySize = 3072;
                diskSize = 4096;
                cores = 2;
              };
              environment.systemPackages = [ docker-embedded-scenario ];
            };
            testScript = ''
              node.wait_for_unit("docker.service")

              # Record the matrix cell in the gate log: conformance evidence
              # is per-version (CONFORMANCE.md).
              print("### MATRIX " + node.succeed(
                  "docker --version && ${pkgs.runc}/bin/runc --version && nix --version"
              ))

              # No daemon in this VM: the socket the daemon profile would use
              # must not exist, so a passing smoke proves the in-process path.
              node.fail("test -e /run/imageless/resolver.sock")

              # An ordinary image passes through unchanged.
              node.succeed("docker load < ${docker-passthrough-image}")
              node.succeed(
                  "docker run --rm --runtime imageless --network none "
                  "localhost/imageless-passthrough:e2e"
              )

              # The seed's layer lacks the executable that produces the
              # response; serving it proves the rootfs was materialized from
              # the embedded flake.
              print(node.succeed(
                  "IMAGELESS_DOCKER_IMAGE_ARCHIVE=${docker-embedded-image} "
                  "IMAGELESS_DOCKER_NETWORK=none "
                  "imageless-docker-embedded-scenario 2>&1"
              ))
            '';
          };

          # -- CRI lifecycle acceptance gate (SPEC.md §7.2) -----------------
          smoke-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-smoke";
            tag = "phase0";
            copyToRoot = pkgs.buildEnv {
              name = "imageless-smoke-root";
              paths = [ pkgs.busybox ];
              pathsToLink = [ "/bin" ];
            };
            config.Cmd = [ "/bin/sleep" "600" ];
          };
          # CRI variant of the embedded-layer seed: the image layer carries
          # ONLY the flake and its source inputs — no top-level /bin — so the
          # workload can only run if the rootfs was materialized from the
          # flake. Unlike the Docker seed, the source rootfs pre-creates the
          # mountpoints CRI binds over a read-only root (hostname, hosts,
          # resolv.conf, /dev/shm).
          cri-embedded-seed = pkgs.runCommand "imageless-cri-embedded-seed" { } ''
            root=$out/etc/imageless/rootfs
            mkdir -p $root/bin $root/dev/shm $root/etc $root/nix/store \
              $root/proc $root/sys $root/tmp
            cp ${pkgs.pkgsStatic.busybox}/bin/busybox $root/bin/busybox
            printf 'imageless-cri-embedded-ok\n' > $root/etc/imageless-cri-embedded
            printf 'root:x:0:0:root:/tmp:/bin/busybox\n' > $root/etc/passwd
            printf 'root:x:0:\n' > $root/etc/group
            touch $root/etc/hostname $root/etc/hosts $root/etc/resolv.conf
            printf '%s\n' '{' \
              '  outputs = { self }: {' \
              '    rootfs = builtins.path {' \
              '      path = ./rootfs;' \
              '      name = "imageless-cri-embedded-rootfs";' \
              '    };' \
              '  };' \
              '}' > $out/etc/imageless/flake.nix
          '';
          cri-embedded-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-cri-embedded";
            tag = "phase0";
            copyToRoot = cri-embedded-seed;
            config.Cmd = [ "/bin/busybox" "sleep" "600" ];
          };
          # External-reference source (SPEC §3): the same busybox rootfs shape
          # as the embedded seed, but it reaches the node as
          # `tarball+http://…?narHash=…` served by the smoke itself on guest
          # loopback — present in NO image layer, which is exactly the mode
          # under test. Single top-level directory, as the tarball fetcher
          # expects; a distinct marker file keeps the assertion honest.
          cri-external-src = pkgs.runCommand "imageless-cri-external-src" { } ''
            root=$out/source/rootfs
            mkdir -p $root/bin $root/dev/shm $root/etc $root/nix/store \
              $root/proc $root/sys $root/tmp
            cp ${pkgs.pkgsStatic.busybox}/bin/busybox $root/bin/busybox
            printf 'imageless-cri-external-ok\n' > $root/etc/imageless-cri-external
            printf 'root:x:0:0:root:/tmp:/bin/busybox\n' > $root/etc/passwd
            printf 'root:x:0:\n' > $root/etc/group
            touch $root/etc/hostname $root/etc/hosts $root/etc/resolv.conf
            printf '%s\n' '{' \
              '  outputs = { self }: {' \
              '    rootfs = builtins.path {' \
              '      path = ./rootfs;' \
              '      name = "imageless-cri-external-rootfs";' \
              '    };' \
              '  };' \
              '}' > $out/source/flake.nix
          '';
          cri-external-tarball = pkgs.runCommand "imageless-cri-external-tarball" { } ''
            mkdir $out
            tar -czf $out/flake.tar.gz -C ${cri-external-src} source
          '';
          smoke-rootfs = pkgs.runCommand "imageless-smoke-rootfs" { } ''
            mkdir -p $out/bin $out/etc $out/tmp $out/nix/store $out/proc $out/sys \
              $out/dev/shm
            ln -s ${pkgs.bash}/bin/bash $out/bin/sh
            ln -s ${pkgs.coreutils}/bin/sleep $out/bin/sleep
            # CRI bind-mounts the sandbox hostname/hosts/resolv.conf (and shm)
            # over the rootfs; the store path is read-only, so the mountpoints
            # must already exist in the materialized root.
            touch $out/etc/hostname $out/etc/hosts $out/etc/resolv.conf
          '';
          smoke-release = pkgs.callPackage ./nix/release-catalog.nix {
            rootfsTargets.${system} = smoke-rootfs;
            issuer = "imageless-smoke";
            releaseName = "cri";
            cache = "local";
          };
          # Parameterized over containerd so the in-guest ctr client always
          # matches the node's daemon generation (a 2.x ctr against a 1.x
          # daemon would test client skew, not the runtime contract).
          mkCriSmoke = containerdPackage: pkgs.writeShellApplication {
            name = "imageless-cri-smoke";
            runtimeInputs = [
              pkgs.coreutils
              containerdPackage
              pkgs.cri-tools
              pkgs.findutils
              pkgs.gnugrep
              pkgs.gzip
              pkgs.jq
              pkgs.nix
              pkgs.util-linux
            ];
            text = ''
              export IMAGELESS_SMOKE_IMAGE_ARCHIVE=${smoke-image}
              export IMAGELESS_SMOKE_EMBEDDED_IMAGE_ARCHIVE=${cri-embedded-image}
              # A bare path string, context discarded on purpose: this only
              # names the .drv the smoke realizes in-guest. The .drv (and its
              # input closure) is seeded and GC-rooted in the guest store by
              # the VM test's system.extraDependencies; carrying context here would make
              # imageless-cri-smoke depend on the .drv and drag it into
              # `nix flake check` eval, which cannot instantiate it on a fresh
              # store.
              export IMAGELESS_SMOKE_ROOTFS_DRV=${builtins.unsafeDiscardStringContext smoke-rootfs.drvPath}
              export IMAGELESS_SMOKE_RELEASE_REFERENCE=${smoke-release.reference}
              export IMAGELESS_SMOKE_EXTERNAL_TARBALL=${cri-external-tarball}/flake.tar.gz
              export IMAGELESS_SMOKE_HTTPD=${pkgs.pkgsStatic.busybox}/bin/busybox
              ${builtins.readFile ./smoke/imageless-cri-smoke.sh}
            '';
          };
          imageless-cri-smoke = mkCriSmoke pkgs.containerd;
          # A real containerd/CRI node driven through the full lifecycle —
          # sandbox passthrough, per-container selection, recreate,
          # GC-while-running, delete-and-collect, reboot recovery — on BOTH
          # store-projection backends. This gate runs the resolver-daemon
          # profile, so CI covers both materializer modes (the Docker gate
          # above covers in-process).
          #
          # Parameterized into a compatibility matrix (ROADMAP v0.2): each
          # instance pins one containerd generation and carries the settings
          # shape that generation expects, so a containerd change can never
          # silently invalidate the conformance claim for the other one.
          mkCriVm = { containerdPackage ? null, criSettings, nameSuffix ? "" }:
            pkgs.testers.runNixOSTest (
              let
                criSmoke =
                  if containerdPackage == null
                  then imageless-cri-smoke
                  else mkCriSmoke containerdPackage;
                nodeBase = { ... }: {
                  imports = [ self.nixosModules.imageless ];
                  # The containerd NixOS module hardcodes pkgs.containerd
                  # (no package option), so a compatibility leg swaps the
                  # node's generation through an overlay; pkgsReadOnly is
                  # lifted below only for those legs, keeping the primary
                  # gate's eval on the shared pkgs.
                  nixpkgs.overlays = lib.mkIf (containerdPackage != null) [
                    (_final: _prev: { containerd = containerdPackage; })
                  ];
                  services.imageless = {
                    enable = true;
                    resolver.enable = true;
                    # The gate must prove embedded-layer bootstrap — a flake
                    # present only in the image layer selecting and
                    # materializing — and that is node-side evaluation, opted
                    # into exactly as a deployment would (SPEC.md §2.1). The
                    # release-annotated workloads keep exercising the digest
                    # path against this same policy, and evaluation runs
                    # through the resolver's privilege-separated development
                    # worker, which no other gate covers. The loopback tarball
                    # prefix is the external-reference mode (SPEC.md §3): the
                    # smoke serves the flake over guest-local HTTP, and any
                    # OTHER external prefix stays un-listed so the smoke can
                    # assert the deny path too.
                    policy.cacheOnly = false;
                    policy.evalAllowedUriPrefixes = [
                      "path:"
                      "tarball+http://127.0.0.1:8081/"
                    ];
                    policy.issuers.imageless-smoke = {
                      source = {
                        kind = "local";
                        directory = smoke-release;
                      };
                      allowedReleases = [ "cri" ];
                      caches.local = {
                        substituter = "file:///nix/store";
                        publicKeys = [ ];
                      };
                    };
                  };

                  # The VM is hermetic (no network). Each matrix instance
                  # points the pod-sandbox ("pause") image at the
                  # locally-imported smoke image — through that generation's
                  # settings shape — so RunPodSandbox resolves locally instead
                  # of pulling the containerd default from registry.k8s.io.
                  virtualisation.containerd.settings = criSettings;

                  # The smoke GCs the disposable rootfs and asserts the reboot
                  # reclaims it, then re-realises it in-guest. Three pieces keep
                  # that honest, and each is load-bearing:
                  #
                  #  * additionalPaths seeds the rootfs .drv — and, because
                  #    closure-info runs exportReferencesGraph over it, the
                  #    OUTPUT path too — into the guest store and its boot-time
                  #    registration. The output must be seeded wherever it is
                  #    registered: registration alone leaves a valid-but-absent
                  #    path that `nix-store --realise` happily no-ops on, and
                  #    runc then delegates onto a rootfs that does not exist.
                  #
                  #  * The rebuild inputs — the .drv closure and every
                  #    build-time output (inputDerivation) — are ALSO rooted
                  #    through the system closure. additionalPaths registers
                  #    without rooting, the smoke runs `nix-store --gc` itself
                  #    (post-reboot BEFORE re-realising), and anything unrooted
                  #    is collected, degrading the rebuild into an offline
                  #    world-rebuild that dies on the first fetch. Only the
                  #    rootfs OUTPUT stays unrooted: that is the disposable
                  #    path whose collection the gate asserts.
                  #
                  #  * The writable store lives on the VM disk, not the default
                  #    tmpfs upper layer, so what the post-reboot GC acts on is
                  #    state that genuinely survived the reboot.
                  system.extraDependencies = [
                    smoke-rootfs.drvPath
                    smoke-rootfs.inputDerivation
                  ];
                  virtualisation = {
                    writableStore = true;
                    writableStoreUseTmpfs = false;
                    memorySize = 4096;
                    cores = 2;
                    diskSize = 8192;
                    additionalPaths = [ smoke-rootfs.drvPath ];
                  };

                  # `jq` for the projection-shape assertions in the testScript.
                  environment.systemPackages = [ criSmoke pkgs.jq ];

                  # The retained witness workload (`sleep 600`, PID-ns init, so
                  # it ignores SIGTERM) would otherwise hold each reboot at the
                  # 90s default stop timeout before systemd-shutdown SIGKILLs it.
                  systemd.settings.Manager.DefaultTimeoutStopSec = "10s";
                };
              in
              {
                name = "imageless-cri-lifecycle${nameSuffix}";
                nodes.node = nodeBase;
                nodes.closure = { ... }: {
                  imports = [ nodeBase ];
                  services.imageless.storeProjection = "closure";
                };
                testScript = ''
                  # The driver passes qemu `-no-reboot` unless a machine is
                  # started with allow_reboot, so the deliberate in-guest reboot
                  # below would exit the VM and drop the backdoor shell. Boot
                  # explicitly before wait_for_unit lazily boots without it.
                  for machine in (node, closure):
                      machine.start(allow_reboot=True)

                  for machine in (node, closure):
                      machine.wait_for_unit("imageless-resolver.service")
                      machine.wait_for_unit("containerd.service")

                  # Record the matrix cell in the gate log: conformance
                  # evidence is per-version (CONFORMANCE.md), so the run must
                  # state what it actually exercised.
                  print("### MATRIX " + node.succeed(
                      "containerd --version && nix --version"
                  ))

                  def rewritten_bundle(machine):
                      # The retained witness container from `pre-reboot` is
                      # still running; its bundle is the one carrying the
                      # imageless GC root (the pod sandbox is never rewritten).
                      return machine.succeed(
                          "dirname \"$(find "
                          "/run/containerd/io.containerd.runtime.v2.task/k8s.io "
                          "-name .imageless-rootfs-gcroot | head -1)\""
                      ).strip()

                  def run_smoke(machine, phase):
                      # On failure the smoke's EXIT trap tears the pod down, so
                      # bundle dirs vanish — but the resolver journal and the
                      # telemetry sink persist and are the decisive signal for
                      # whether the shim ran per-container resolution at all.
                      status, output = machine.execute(
                          "imageless-cri-smoke " + phase + " 2>&1"
                      )
                      print(output)
                      if status != 0:
                          print("### DIAG " + machine.name + " " + phase + " ###")
                          print("--- imageless-resolver journal ---")
                          print(machine.execute(
                              "journalctl -u imageless-resolver --no-pager "
                              "--no-hostname | tail -n 80"
                          )[1])
                          print("--- resolver timings (/run/imageless/timings.jsonl) ---")
                          print(machine.execute(
                              "cat /run/imageless/timings.jsonl 2>&1"
                          )[1])
                          print("--- containerd journal (shim/imageless) ---")
                          print(machine.execute(
                              "journalctl -u containerd --no-pager --no-hostname | "
                              "grep -iE 'imageless|shim start|bundle selection|"
                              "gcroot|resolve|runc.v2' | tail -n 60"
                          )[1])
                          print("--- live task bundles ---")
                          print(machine.execute(
                              "ls -laR /run/containerd/io.containerd.runtime.v2.task"
                              "/k8s.io/ 2>&1 | tail -n 80"
                          )[1])
                          raise Exception(
                              machine.name + " imageless-cri-smoke " + phase + " failed"
                          )

                  # Full pre-reboot lifecycle on each backend: annotated
                  # sandbox, selected init, unselected sidecar, main, live-GC
                  # survival, CRI recreate, failed start, delete, then a
                  # retained witness workload.
                  run_smoke(node, "pre-reboot")
                  run_smoke(closure, "pre-reboot")

                  # The node backend binds the whole store once and no
                  # per-path store mounts.
                  node.succeed(
                      "jq -e '([.mounts[]|select(.destination==\"/nix/store\")]|length==1) "
                      "and ([.mounts[]|select(.destination|startswith(\"/nix/store/\"))]|length==0)' "
                      + rewritten_bundle(node) + "/config.json"
                  )
                  # The closure backend binds only per-path closure mounts, lays a
                  # single empty tmpfs over /nix/store so those mountpoints can be
                  # created in the read-only rootfs, and never binds the whole node
                  # store.
                  closure.succeed(
                      "jq -e '([.mounts[]|select(.destination|startswith(\"/nix/store/\"))]|length>0) "
                      "and ([.mounts[]|select(.destination==\"/nix/store\" and .type==\"tmpfs\")]|length==1) "
                      "and ([.mounts[]|select(.destination==\"/nix/store\" and .type==\"bind\")]|length==0)' "
                      + rewritten_bundle(closure) + "/config.json"
                  )

                  for machine in (node, closure):
                      machine.reboot()
                      machine.wait_for_unit("containerd.service")
                      machine.wait_for_unit("imageless-resolver.service")

                  # Post-reboot on each backend: the /run bundle GC root is
                  # gone (tmpfs), GC reclaims the now-unrooted disposable
                  # rootfs, and a fresh workload still resolves.
                  run_smoke(node, "post-reboot")
                  run_smoke(closure, "post-reboot")
                '';
              } // lib.optionalAttrs (containerdPackage != null) {
                # Only the overlay-carrying legs pay for their own nixpkgs
                # evaluation; the primary instance keeps the shared read-only
                # pkgs.
                node.pkgsReadOnly = false;
              }
            );
          imageless-cri-vm = mkCriVm {
            # Primary pin: containerd 2.x straight from the flake's nixpkgs.
            # 2.x reads its sandbox pin from the CRI image-service plugin
            # table.
            criSettings.plugins."io.containerd.cri.v1.images".pinned_images.sandbox =
              "localhost/imageless-smoke:phase0";
          };
          imageless-cri-vm-containerd1 = mkCriVm {
            # Compatibility leg: containerd 1.x from the dedicated input.
            # 1.x predates the split image-service plugin; its sandbox pin
            # is `sandbox_image` on the monolithic CRI plugin.
            containerdPackage = nixpkgs-containerd1.legacyPackages.${system}.containerd;
            criSettings.plugins."io.containerd.grpc.v1.cri" = {
              sandbox_image = "localhost/imageless-smoke:phase0";
              # 1.x refuses to load the CRI plugin unless the default
              # runtime it names (runc) is configured explicitly — 2.x
              # synthesizes this entry. The runc binary comes from the
              # containerd unit's PATH (the module wires pkgs.runc there).
              containerd.runtimes.runc.runtime_type = "io.containerd.runc.v2";
            };
            nameSuffix = "-containerd1";
          };

          # Store-projection shape proof against completely stock runc: a
          # materialized-style rootfs whose binaries live in the node store
          # runs under an unmodified rootless runc with a single read-only
          # store bind.
          stock-oci-smoke = pkgs.writeShellApplication {
            name = "imageless-stock-oci-smoke";
            runtimeInputs = [ pkgs.binutils pkgs.coreutils pkgs.gnused pkgs.jq ];
            text = ''
              exec bash ${./crates/imageless-runc/tests/stock-runc.sh} \
                ${pkgs.runc}/bin/runc \
                ${smoke-rootfs}
            '';
          };
        in
        {
          inherit imageless imageless-dev imageless-runc imageless-resolver kubectl-imageless;
          inherit docker-embedded-seed docker-embedded-image docker-passthrough-image placeholder-image docker-helper-image;
          inherit nginx-embedded-seed nginx-embedded-image;
          inherit docker-embedded-scenario docker-embedded-isolated;
          inherit docker-embedded-smoke imageless-cri-vm imageless-cri-vm-containerd1;
          inherit smoke-image smoke-rootfs smoke-release imageless-cri-smoke;
          inherit cri-embedded-seed cri-embedded-image;
          inherit stock-oci-smoke;
          default = imageless-runc;
        });

      # `nix flake check` evaluates every derivation under `packages` and
      # `checks`. A NixOS VM test's qemu closure reads the .drv it seeds via
      # virtualisation.additionalPaths (smoke-rootfs) at EVAL time, which is not
      # valid on a fresh store — so the VM gates cannot live under either.
      # They go under legacyPackages, the one derivation-bearing output flake
      # check skips. `nix build .#imageless-cri-vm` still resolves them (the
      # installable lookup falls through packages -> legacyPackages), and the
      # KVM acceptance-gates CI job builds them there.
      packages = forAllSystems (system:
        builtins.removeAttrs self.allPackages.${system} [
          "docker-embedded-smoke"
          "imageless-cri-vm"
          "imageless-cri-vm-containerd1"
        ]);

      legacyPackages = forAllSystems (system: {
        inherit (self.allPackages.${system})
          docker-embedded-smoke imageless-cri-vm imageless-cri-vm-containerd1;
      });

      checks = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;
          src = lib.fileset.toSource {
            root = ./.;
            # README.md is compiled as a doc-test (crates/imageless/src/lib.rs),
            # and both it and LICENSE are symlinked into crates/imageless so the
            # published tarball carries them. Drop either from the fileset and
            # the link dangles in the sandbox.
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
              ./README.md
              ./LICENSE
            ];
          };
          # Workspace-wide gate: fails on any rustfmt drift or clippy warning,
          # and on the library crate becoming unpackageable — publishing is a
          # release step, staying publishable is a merge gate.
          lint = pkgs.stdenv.mkDerivation {
            name = "imageless-lint";
            inherit src;
            cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
            ];
            buildPhase = ''
              export HOME=$TMPDIR
              cargo fmt --check
              cargo clippy --workspace --all-targets --offline -- -D warnings
              cargo package -p imageless --offline --no-verify
              # Packageable is not the same as publishable: `cargo package`
              # succeeds happily on a crate that declares Apache-2.0 and ships
              # no license text, because an SPDX expression carries no bytes
              # and nothing checks that any file backs it. That is what this
              # crate did until README.md and LICENSE became entries in the
              # package directory. Assert the bytes are actually in the tarball.
              # Listed to a file rather than piped: `grep -q` exits on the first
              # match, and under stdenv's pipefail tar's resulting EPIPE would
              # fail the build on success.
              tar tzf target/package/imageless-*.crate > packaged-files
              for f in README.md LICENSE; do
                grep -qx "imageless-[^/]*/$f" packaged-files \
                  || { echo "packaged crate is missing $f" >&2; exit 1; }
              done
              # crates.io resolves a README's relative URLs against the crate's
              # path in the repository — `path_in_vcs`, which is
              # `crates/imageless`, not the root. This README reaches the crate
              # by symlink and is written for the root, so a root-relative link
              # renders as a 404 on the crate page: the logo did exactly that
              # until it was made absolute. Absolute is the only form correct in
              # GitHub, crates.io and docs.rs at once. Checked against the
              # PACKAGED copy, because that is the byte stream crates.io renders.
              # The member pattern is quoted and given to tar, not to the shell:
              # unquoted it is a glob against the build directory, matches
              # nothing there, and stdenv's nullglob deletes the word — leaving
              # tar to dump all 15 members and this scan to run over the crate's
              # Rust sources, where a rustdoc intra-doc link reads as a relative
              # URL and fails the build blaming a README that is fine.
              tar xzOf target/package/imageless-*.crate \
                --wildcards 'imageless-*/README.md' > packaged-readme
              { grep -oE '\]\([^)]+\)' packaged-readme | sed 's/^](//; s/)$//'
                grep -oE '(src|href)="[^"]+"' packaged-readme | sed 's/^[a-z]*="//; s/"$//'
              } | grep -vE '^(https?://|#)' > relative-urls || true
              if [ -s relative-urls ]; then
                echo "README.md carries relative URLs; crates.io resolves them" >&2
                echo "under crates/imageless/ and they would 404 on the crate page:" >&2
                cat relative-urls >&2
                exit 1
              fi
            '';
            installPhase = "touch $out";
          };
          # Evaluate the NixOS module in both materializer modes and assert
          # the wiring it promises, so module regressions fail eval — not a
          # node.
          module-eval =
            let
              evalNode = extra:
                (nixpkgs.lib.nixosSystem {
                  modules = [
                    self.nixosModules.imageless
                    {
                      nixpkgs.hostPlatform = system;
                      boot.loader.grub.enable = false;
                      fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
                      system.stateVersion = "25.05";
                    }
                    extra
                  ];
                }).config;
              daemonless = evalNode {
                services.imageless.enable = true;
              };
              daemon = evalNode {
                services.imageless = {
                  enable = true;
                  resolver.enable = true;
                  policy.cacheOnly = false;
                  policy.evalAllowedUriPrefixes = [ "path:" ];
                };
              };
              runtime = daemonless.virtualisation.containerd.settings.plugins."io.containerd.grpc.v1.cri".containerd.runtimes.imageless;
            in
            assert lib.hasSuffix "/bin/imageless-runc" runtime.options.BinaryName;
            assert runtime.runtime_type == "io.containerd.runc.v2";
            assert runtime.pod_annotations == [ "imageless.run/*" "run.imageless.*" ];
            assert runtime.container_annotations == [ "imageless.run/*" "run.imageless.*" ];
            # Daemonless: no socket in containerd's environment, no resolver
            # unit, policy staged at the in-process default path.
            assert !(daemonless.systemd.services.containerd.environment ? IMAGELESS_RESOLVER_SOCKET);
            assert !(daemonless.systemd.services ? imageless-resolver)
              || daemonless.systemd.services.imageless-resolver.enable == false;
            # Policy is a real copied file at the in-process default path —
            # the loader fail-closed rejects environment.etc symlinks.
            assert lib.any (lib.hasPrefix "C+ /etc/imageless/policy.json")
              daemonless.systemd.tmpfiles.rules;
            # Daemon profile: socket exported, resolver runs with the
            # unprivileged development worker when evaluation is allowed.
            assert daemon.systemd.services.containerd.environment.IMAGELESS_RESOLVER_SOCKET
              == "/run/imageless/resolver.sock";
            assert lib.hasInfix "--development-worker"
              daemon.systemd.services.imageless-resolver.serviceConfig.ExecStart;
            assert daemon.users.users ? imageless-dev;
            pkgs.writeText "imageless-module-eval" "ok";
          # dev/kind and dev/k3s are executable documentation; lint what can
          # drift: the setup scripts, the manifests, and the containerd patch
          # and drop-in, which must stay identical to the runtime table of
          # examples/containerd-config.toml.
          quickstart-lint =
            let
              image = self.packages.${system}.nginx-embedded-image;
            in
            pkgs.runCommand "imageless-quickstart-lint"
              {
                nativeBuildInputs = [ pkgs.shellcheck pkgs.yq-go pkgs.python3 ];
                quickstart = lib.fileset.toSource {
                  root = ./.;
                  fileset = lib.fileset.unions [
                    ./dev/kind
                    ./dev/k3s
                    ./examples/containerd-config.toml
                  ];
                };
                expectedImage = "${image.imageName}:${image.imageTag}";
              }
              ''
                shellcheck "$quickstart/dev/kind/setup.sh" "$quickstart/dev/k3s/setup.sh"
                yq eval '.' "$quickstart/dev/k3s/registries.yaml" >/dev/null
                yq eval '.' "$quickstart/dev/kind/kind-config.yaml" >/dev/null
                yq eval '.' "$quickstart/dev/kind/pod-nginx-embedded.yaml" >/dev/null
                test "$(yq eval '.spec.containers[0].image' "$quickstart/dev/kind/pod-nginx-embedded.yaml")" \
                  = "$expectedImage"
                yq eval '.containerdConfigPatches[0]' "$quickstart/dev/kind/kind-config.yaml" >patch.toml
                python3 - "$quickstart" <<'PY'
                import pathlib
                import sys
                import tomllib

                source = pathlib.Path(sys.argv[1])

                def runtime(config):
                    plugins = config["plugins"]["io.containerd.cri.v1.runtime"]
                    return plugins["containerd"]["runtimes"]["imageless"]

                # SystemdCgroup is the one field that legitimately differs, and
                # comparing it was what made the correct kind config
                # unrepresentable: it tracks the node's cgroup driver, which is
                # a property of the node rather than of the imageless contract.
                # A kind node resolves its own runc handler to cgroupfs; a
                # production node is systemd-managed. Mismatch it and the
                # sandbox never starts, so neither file may leave it to a
                # default.
                def driver(table, origin):
                    options = dict(table["options"])
                    assert "SystemdCgroup" in options, (
                        f"{origin} must state SystemdCgroup, not inherit it"
                    )
                    setting = options.pop("SystemdCgroup")
                    return {**table, "options": options}, setting

                patch, patch_driver = driver(
                    runtime(tomllib.loads(pathlib.Path("patch.toml").read_text())),
                    "dev/kind/kind-config.yaml",
                )
                example, example_driver = driver(
                    runtime(
                        tomllib.loads(
                            (source / "examples/containerd-config.toml").read_text()
                        )
                    ),
                    "examples/containerd-config.toml",
                )
                assert patch == example, (
                    "dev/kind patch drifted from examples/containerd-config.toml"
                )
                assert patch_driver is False, (
                    "a kind node is cgroupfs: dev/kind must set SystemdCgroup = false"
                )
                assert example_driver is True, (
                    "a production node is systemd-managed: "
                    "examples/containerd-config.toml must set SystemdCgroup = true"
                )
                dropin, dropin_driver = driver(
                    runtime(
                        tomllib.loads(
                            (source / "dev/k3s/containerd/imageless.toml").read_text()
                        )
                    ),
                    "dev/k3s/containerd/imageless.toml",
                )
                assert dropin == example, (
                    "dev/k3s drop-in drifted from examples/containerd-config.toml"
                )
                # The oom drop-in must only ever carry the one setting it is for.
                oom = tomllib.loads(
                    (source / "dev/k3s/containerd/restrict-oom.toml").read_text()
                )
                assert oom["plugins"] == {
                    "io.containerd.cri.v1.runtime": {"restrict_oom_score_adj": True}
                }, "dev/k3s/containerd/restrict-oom.toml carries more than the clamp"
                PY
                touch $out
              '';
        in
        # Every package is a check (a package that doesn't build is a broken
          # release), plus the lint/module-eval gates only checks carry. The two
          # NixOS VM gates are deliberately absent from `packages` (they live under
          # legacyPackages — see above), so they are naturally excluded here too.
        self.packages.${system} // {
          inherit lint module-eval quickstart-lint;
        });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.rust-analyzer
              pkgs.jq
              pkgs.nixpkgs-fmt
            ];
          };
        });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixpkgs-fmt);
    };
}
