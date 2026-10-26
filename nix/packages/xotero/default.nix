{
  lib,
  rustPlatform,
  cacert,
}:

rustPlatform.buildRustPackage {
  pname = "xotero";
  version = "0.1.0";

  src = ../../..;
  cargoLock.lockFile = ../../../Cargo.lock;

  buildAndTestSubdir = "xotero";

  preCheck = ''
    export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
  '';

  meta = {
    description = "Self-hosted Zotero Web API v3 sync server";
    mainProgram = "xotero";
    license = lib.licenses.mit;
    platforms = lib.platforms.unix;
    maintainers = [ lib.maintainers.mulatta ];
  };
}
