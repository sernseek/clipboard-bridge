{
  lib,
  rustPlatform,
  source ? ../.,
}:
let
  sourceDirectory = toString source;
  src = builtins.path {
    name = "clipboard-bridge-source";
    path = sourceDirectory;
    filter =
      path: type:
      path == sourceDirectory
      || (
        type == "directory"
        && (path == "${sourceDirectory}/src" || lib.hasPrefix "${sourceDirectory}/src/" path)
      )
      || (
        type == "regular"
        && (
          (
            builtins.dirOf path == sourceDirectory
            && builtins.elem (builtins.baseNameOf path) [
              "Cargo.toml"
              "Cargo.lock"
            ]
          )
          || (lib.hasPrefix "${sourceDirectory}/src/" path && lib.hasSuffix ".rs" path)
        )
      );
  };
in
rustPlatform.buildRustPackage {
  pname = "clipboard-bridge";
  version = (builtins.fromTOML (builtins.readFile "${src}/Cargo.toml")).package.version;
  inherit src;
  cargoLock.lockFile = "${src}/Cargo.lock";
  meta = {
    description = "Native Wayland and X11 clipboard synchronization";
    mainProgram = "clipboard-bridge";
    platforms = lib.platforms.linux;
  };
}
