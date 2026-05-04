{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
}:

rustPlatform.buildRustPackage {
  pname = "statix-agent";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ./..;
    filter = path: type:
      let
        baseName = baseNameOf path;
      in
      !(type == "directory" && baseName == "target")
      && !(type == "directory" && baseName == ".git");
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
  ];

  buildInputs = [
    openssl
  ];

  meta = {
    description = "Statix node agent";
    homepage = "https://statix.se";
    license = lib.licenses.asl20;
    mainProgram = "statix";
  };
}
