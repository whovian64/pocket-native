#!/bin/bash
# make_apk.sh <package> <label> <libpocketnative.so> <assets dir> <out.apk> <keystore>
#
# Packages Pocket Native's app with the SDK's build tools, no Gradle: the
# manifest linked against the platform with the profiles as assets, the
# launcher's and the game's Java classes as classes.dex, the engine in
# lib/arm64-v8a, aligned and signed with the keystore, which is made the
# first time. Keep the keystore: an update signed with another one will not
# install over the app, which would have to be uninstalled first, saves and
# all. KEYSTORE_PASS is its password, "android" for a test key.
set -euo pipefail

package="$1" label="$2" engine="$3" assets="$4" out="$5" keystore="$6"
pass="${KEYSTORE_PASS:-android}"
here="$(cd "$(dirname "$0")" && pwd)"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$here/../../Cargo.toml" | head -1)
# a version code from the version, 0.3.0 as 3000
IFS=. read -r major minor patch <<< "$version"
code=$((major * 1000000 + minor * 1000 + patch))

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
escaped=$(printf %s "$label" | sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g; s/"/\&quot;/g')
sed -e "s|@PACKAGE@|$package|" -e "s|@VERSION_CODE@|$code|" -e "s|@VERSION@|$version|" \
    -e "s|@LABEL@|$(printf %s "$escaped" | sed 's/[|&\\]/\\&/g')|" \
    "$here/AndroidManifest.xml" > "$work/AndroidManifest.xml"

"$BUILD_TOOLS_BIN/aapt2" compile --dir "$here/res" -o "$work/res.zip"
"$BUILD_TOOLS_BIN/aapt2" link -o "$work/unaligned.apk" --manifest "$work/AndroidManifest.xml" -I "$ANDROID_JAR" -A "$assets" "$work/res.zip"
mkdir -p "$work/classes" "$work/dex"
mapfile -t sources < <(find "$here/java" -name '*.java')
javac -nowarn --release 11 -classpath "$ANDROID_JAR" -d "$work/classes" "${sources[@]}"
mapfile -t classes < <(find "$work/classes" -name '*.class')
"$BUILD_TOOLS_BIN/d8" --min-api 30 --lib "$ANDROID_JAR" --output "$work/dex" "${classes[@]}"
(cd "$work/dex" && zip -q "$work/unaligned.apk" classes.dex)
mkdir -p "$work/lib/arm64-v8a"
cp "$engine" "$work/lib/arm64-v8a/libpocketnative.so"
# stored, not compressed, and page-aligned by zipalign -p, so that Android
# loads it from the APK itself rather than unpacking a second copy
(cd "$work" && zip -q -r -0 unaligned.apk lib)
"$BUILD_TOOLS_BIN/zipalign" -p -f 4 "$work/unaligned.apk" "$work/aligned.apk"

if [ ! -f "$keystore" ]; then
    echo "== making a signing key, $keystore"
    keytool -genkeypair -keystore "$keystore" -storepass "$pass" -keypass "$pass" -alias pocketnative \
        -keyalg RSA -keysize 4096 -validity 10000 -dname "CN=Pocket Native" >/dev/null 2>&1
fi
"$BUILD_TOOLS_BIN/apksigner" sign --ks "$keystore" --ks-pass "pass:$pass" --ks-key-alias pocketnative --out "$out" "$work/aligned.apk"
rm -f "$out.idsig"
