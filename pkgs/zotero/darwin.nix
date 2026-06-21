{
  lib,
  stdenv,
  fetchurl,
  undmg,
  zip,
  unzip,
  writeShellApplication,
  # Read for its version so darwin and linux stay on the same release. Only the
  # version string is used — the nixpkgs darwin build is never pulled into the
  # closure (the official dmg is fetched instead).
  zotero,
  apiUrl,
  wwwUrl,
  streamUrl,
  prefsFile,
}:

let
  # Track the nixpkgs zotero version so both platforms sync at the same release.
  # The dmg hash can't be derived, so it's looked up here; an unknown version
  # throws — which fires exactly when zotero bumps, i.e. when the mac build needs
  # refreshing anyway. Get a new hash with:
  #   nix store prefetch-file https://download.zotero.org/client/release/<v>/Zotero-<v>.dmg
  inherit (zotero) version;
  dmgHashes = {
    "9.0.4" = "sha256-Wbdi7JaCqM+6tTu3YvU+qNW7F2lfQ4+z0zDpz5YDwtI=";
  };
  dmgHash =
    dmgHashes.${version}
      or (throw "zhost: no Zotero dmg hash for ${version}; prefetch it and add it to pkgs/zotero/darwin.nix");
in
stdenv.mkDerivation (finalAttrs: {
  pname = "zotero";
  inherit version;

  src = fetchurl {
    url = "https://download.zotero.org/client/release/${version}/Zotero-${version}.dmg";
    hash = dmgHash;
  };

  # undmg unpacks the dmg's volume contents (Zotero.app) into the build dir.
  sourceRoot = ".";
  nativeBuildInputs = [
    undmg
    zip
    unzip
  ];
  dontBuild = true;

  postPatch = ''
    chmod -R u+w Zotero.app

    omni="$PWD/Zotero.app/Contents/Resources/app/omni.ja"
    work="$PWD/omni-work"
    mkdir -p "$work"
    unzip -o "$omni" resource/config.mjs -d "$work" >/dev/null

    substituteInPlace "$work/resource/config.mjs" \
      --replace-warn "https://api.zotero.org/" "${apiUrl}" \
      --replace-warn "https://www.zotero.org/" "${wwwUrl}" \
      --replace-warn "wss://stream.zotero.org/" "${streamUrl}"

    ( cd "$work" && zip "$omni" resource/config.mjs >/dev/null )
    ${lib.optionalString (prefsFile != null) ''
      unzip -o "$omni" defaults/preferences/zotero.js -d "$work" >/dev/null
      cat ${prefsFile} >> "$work/defaults/preferences/zotero.js"
      ( cd "$work" && zip "$omni" defaults/preferences/zotero.js >/dev/null )
    ''}
  '';

  installPhase = ''
    runHook preInstall

    mkdir -p "$out/Applications"
    cp -R Zotero.app "$out/Applications/"

    runHook postInstall
  '';

  # The store bundle is intentionally left UNSIGNED. Patching omni.ja breaks the
  # notarized seal, and re-establishing it needs a deep ad-hoc re-sign via the
  # system codesign (nixpkgs' sigtool cannot sign bundles). Doing that in the
  # build would require disabling the sandbox, so we keep the build pure and
  # re-sign on the host at activation time. darwinInstall performs that
  # copy + deep-sign + de-quarantine into a writable location (default
  # ~/Applications); a nix-darwin/home-manager activation script calls it.
  # The codesign/xattr paths are deliberately the system ones: this runs on the
  # host at activation, not in the build sandbox.
  passthru.darwinInstall = writeShellApplication {
    name = "zotero-darwin-install";
    text = ''
      target="''${1:-$HOME/Applications}/Zotero.app"
      rm -rf "$target"
      mkdir -p "$(dirname "$target")"
      cp -R "${finalAttrs.finalPackage}/Applications/Zotero.app" "$target"
      chmod -R u+w "$target"
      /usr/bin/codesign --force --deep --sign - "$target"
      /usr/bin/xattr -dr com.apple.quarantine "$target" || true
    '';
  };

  meta = {
    description = "Zotero (official build) patched to sync against a self-hosted zhost server";
    homepage = "https://www.zotero.org";
    license = lib.licenses.agpl3Only;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = lib.platforms.darwin;
    mainProgram = "zotero";
    maintainers = [ lib.maintainers.mulatta ];
  };
})
