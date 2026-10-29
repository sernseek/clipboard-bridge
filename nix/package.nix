{
  lib,
  rustPlatform,
  pkgsCross,
  source ? ../.,
}:
let
  sourceDirectory = toString source;
  # Rust sources and manifests of the bridge (src/) and the Windows guest
  # agent (guest/), which also builds src/guest/wire.rs.
  under = directory: path: path == directory || lib.hasPrefix "${directory}/" path;
  src = builtins.path {
    name = "clipboard-bridge-source";
    path = sourceDirectory;
    filter =
      path: type:
      path == sourceDirectory
      || (
        type == "directory"
        && (
          under "${sourceDirectory}/src" path
          || path == "${sourceDirectory}/guest"
          || under "${sourceDirectory}/guest/src" path
        )
      )
      || (
        type == "regular"
        && (
          builtins.elem path [
            "${sourceDirectory}/Cargo.toml"
            "${sourceDirectory}/Cargo.lock"
            "${sourceDirectory}/guest/Cargo.toml"
            "${sourceDirectory}/guest/Cargo.lock"
          ]
          || (
            (under "${sourceDirectory}/src" path || under "${sourceDirectory}/guest/src" path)
            && lib.hasSuffix ".rs" path
          )
        )
      );
  };
  version = (builtins.fromTOML (builtins.readFile "${src}/Cargo.toml")).package.version;
in
rustPlatform.buildRustPackage {
  pname = "clipboard-bridge";
  inherit src version;
  cargoLock.lockFile = "${src}/Cargo.lock";

  # The Windows agent for a passthrough guest (`--guest-port`).
  passthru.guest = pkgsCross.mingwW64.rustPlatform.buildRustPackage {
    pname = "clipboard-bridge-guest";
    inherit src version;
    cargoRoot = "guest";
    buildAndTestSubdir = "guest";
    cargoLock.lockFile = "${src}/guest/Cargo.lock";
    meta = {
      description = "Windows guest agent of clipboard-bridge";
      mainProgram = "clipboard-bridge-guest";
      platforms = lib.platforms.windows;
    };
  };

  meta = {
    description = "Native Wayland, X11 and passthrough guest clipboard synchronization";
    mainProgram = "clipboard-bridge";
    platforms = lib.platforms.linux;
  };
}
