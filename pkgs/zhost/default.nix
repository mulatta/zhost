{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "zhost";
  version = "0.1.0";

  src = ../../server;
  cargoLock.lockFile = ../../server/Cargo.lock;

  meta = {
    description = "Self-hosted Zotero Web API v3 sync server";
    mainProgram = "zhost";
    license = lib.licenses.mit;
    platforms = lib.platforms.unix;
    maintainers = [ lib.maintainers.mulatta ];
  };
}
