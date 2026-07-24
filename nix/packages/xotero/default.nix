{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "xotero";
  version = "0.1.0";

  src = ../../../xotero;
  cargoLock.lockFile = ../../../xotero/Cargo.lock;

  meta = {
    description = "Self-hosted Zotero Web API v3 sync server";
    mainProgram = "xotero";
    license = lib.licenses.mit;
    platforms = lib.platforms.unix;
    maintainers = [ lib.maintainers.mulatta ];
  };
}
