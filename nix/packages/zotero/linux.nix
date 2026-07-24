{
  lib,
  stdenv,
  zotero,
  zip,
  unzip,
  apiUrl,
  wwwUrl,
  streamUrl,
  prefsFile,
}:

# Post-process the binary-cached nixpkgs zotero: rewriting the sync endpoints in
# resource/config.mjs is a cheap patch, and on linux the bundle needs no signing.
# A source override would force a full local rebuild on every version bump, which
# the reliable linux hydra cache lets us avoid.
stdenv.mkDerivation {
  pname = "zotero";
  inherit (zotero) version;

  dontUnpack = true;
  nativeBuildInputs = [
    zip
    unzip
  ];

  buildPhase = ''
    runHook preBuild

    cp -r ${zotero} build
    chmod -R u+w build

    omni="$PWD/build/lib/app/omni.ja"
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

    runHook postBuild
  '';

  # bin/zotero is a relative symlink into lib/, so the copied tree stays
  # self-consistent and needs no wrapper regeneration.
  installPhase = ''
    runHook preInstall
    cp -r build "$out"
    runHook postInstall
  '';

  meta = zotero.meta // {
    description = "Zotero patched to sync against a self-hosted zhost server";
    platforms = lib.platforms.linux;
    maintainers = [ lib.maintainers.mulatta ];
  };
}
