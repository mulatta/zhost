{
  stdenv,
  callPackage,
  zotero,
  # Override only the endpoints the self-hosted server actually implements;
  # anything left at the upstream default keeps pointing at zotero.org.
  apiUrl ? "https://api.zotero.org/",
  wwwUrl ? "https://www.zotero.org/",
  streamUrl ? "wss://stream.zotero.org/",
}:

# The two platforms need fundamentally different sources:
#   - darwin: the official notarized dmg, pinned in darwin.nix independently of
#     nixpkgs, so a flaky/uncached hydra darwin build never blocks a consumer's
#     nixpkgs bump. Re-signing is deferred to host activation.
#   - linux: post-process the binary-cached nixpkgs zotero, whose x86_64/aarch64
#     hydra builds are reliable and offloadable to remote builders.
let
  args = { inherit apiUrl wwwUrl streamUrl; };
in
if stdenv.hostPlatform.isDarwin then
  callPackage ./darwin.nix (args // { inherit zotero; })
else
  callPackage ./linux.nix (args // { inherit zotero; })
