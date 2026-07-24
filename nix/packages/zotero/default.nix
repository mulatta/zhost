{
  lib,
  stdenv,
  callPackage,
  writeText,
  zotero,
  # Override only the endpoints the self-hosted server actually implements;
  # anything left at the upstream default keeps pointing at zotero.org.
  apiUrl ? "https://api.zotero.org/",
  wwwUrl ? "https://www.zotero.org/",
  streamUrl ? "wss://stream.zotero.org/",
  # Zotero preferences to bake into the bundle's default prefs (e.g.
  # { "extensions.zotero.automaticTags" = false; }). Appended to the in-bundle
  # defaults, so a later definition overrides the upstream default; the user can
  # still change them in-app.
  prefs ? { },
}:

# The two platforms need fundamentally different sources:
#   - darwin: the official notarized dmg, pinned in darwin.nix independently of
#     nixpkgs, so a flaky/uncached hydra darwin build never blocks a consumer's
#     nixpkgs bump. Re-signing is deferred to host activation.
#   - linux: post-process the binary-cached nixpkgs zotero, whose x86_64/aarch64
#     hydra builds are reliable and offloadable to remote builders.
let
  prefValue =
    v:
    if builtins.isBool v then
      (if v then "true" else "false")
    else if builtins.isInt v then
      toString v
    else
      "\"${v}\"";
  prefsText = lib.concatStringsSep "\n" (
    lib.mapAttrsToList (name: v: "pref(\"${name}\", ${prefValue v});") prefs
  );
  # null when there is nothing to add, so the platform builds skip the patch.
  prefsFile = if prefs == { } then null else writeText "zhost-prefs.js" (prefsText + "\n");
  args = {
    inherit
      apiUrl
      wwwUrl
      streamUrl
      prefsFile
      ;
  };
in
if stdenv.hostPlatform.isDarwin then
  callPackage ./darwin.nix (args // { inherit zotero; })
else
  callPackage ./linux.nix (args // { inherit zotero; })
