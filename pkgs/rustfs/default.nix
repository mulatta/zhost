# RustFS — an S3-compatible object store, used only as the storage backend in
# the integration test (the production deployment talks to Cloudflare R2). Pinned
# and built from source rather than pulled in as a flake input so this repo stays
# self-contained. The recipe mirrors the upstream niks3 packaging; the
# Content-Encoding patch it carries is for zstd-compressed nix narinfo files and
# is irrelevant here (Zotero stores attachment bytes verbatim), so it is dropped.
{
  lib,
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  protobuf,
  openssl,
}:

rustPlatform.buildRustPackage rec {
  pname = "rustfs";
  version = "1.0.0-alpha.72";

  src = fetchFromGitHub {
    owner = "rustfs";
    repo = "rustfs";
    rev = version;
    hash = "sha256-iWaZgvy40RW67oqyVttaWyrFrAVy17UJz5JydI51uDM=";
  };

  cargoHash = "sha256-ApVUUpeLXpMwqRnuNI/Q20/FTEvUyPTtDSpmPsDco2I=";

  nativeBuildInputs = [
    pkg-config
    protobuf
  ];

  buildInputs = [
    openssl
  ];

  # Only build the main rustfs binary (the workspace has several crates).
  cargoBuildFlags = [
    "--package"
    "rustfs"
  ];

  # Upstream tests need a full environment; we only need the server binary.
  doCheck = false;

  meta = {
    description = "High-performance S3-compatible object storage";
    homepage = "https://rustfs.com";
    license = lib.licenses.asl20;
    mainProgram = "rustfs";
  };
}
