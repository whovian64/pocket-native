#!/bin/bash
# make_apk.sh <title id> <label> <libpocket_native.so> <libtitle.so> <out.apk> <keystore> [icon.png]
#
# Packages one game's app with the SDK's build tools, no Gradle: the
# manifest linked against the platform, the second screen's Java class as
# classes.dex, the two libraries in lib/arm64-v8a, and the game's icon for
# the launcher when there is one, aligned and signed with the keystore,
# which is made the first time. Keep the keystore: an update signed with
# another one will not install over the app, and the app would have to be
# uninstalled first, saves and all.
set -euo pipefail

id="$1" label="$2" engine="$3" title="$4" out="$5" keystore="$6" icon="${7:-}"
here="$(cd "$(dirname "$0")" && pwd)"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$here/../../Cargo.toml" | head -1)
# a version code from the version, 0.2.0 as 2000
IFS=. read -r major minor patch <<< "$version"
code=$((major * 1000000 + minor * 1000 + patch))
package="io.github.pocketnative.t$(printf %s "$id" | tr A-F a-f)"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# the label goes into XML, so its own &, < and > are escaped first
escaped=$(printf %s "$label" | sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g; s/"/\&quot;/g')
icon_attribute="" resources=()
if [ -n "$icon" ]; then
    # the 3DS's 48x48 icon, scaled up four times, for the densest screens,
    # which Android scales down for the rest
    mkdir -p "$work/res/mipmap-xxxhdpi"
    cp "$icon" "$work/res/mipmap-xxxhdpi/icon.png"
    "$BUILD_TOOLS_BIN/aapt2" compile -o "$work/resources.zip" --dir "$work/res"
    icon_attribute='android:icon="@mipmap/icon"'
    resources=("$work/resources.zip")
fi
sed -e "s|@PACKAGE@|$package|" -e "s|@VERSION_CODE@|$code|" -e "s|@VERSION@|$version|" \
    -e "s|@LABEL@|$(printf %s "$escaped" | sed 's/[|&\\]/\\&/g')|" -e "s|@ICON@|$icon_attribute|" \
    "$here/AndroidManifest.xml" > "$work/AndroidManifest.xml"

"$BUILD_TOOLS_BIN/aapt2" link -o "$work/unaligned.apk" --manifest "$work/AndroidManifest.xml" -I "$ANDROID_JAR" "${resources[@]}"
# the one Java class, for the second screen, compiled against the platform
# and turned into the APK's classes.dex
mkdir -p "$work/classes" "$work/dex"
mapfile -t sources < <(find "$here/java" -name '*.java')
javac -nowarn --release 11 -classpath "$ANDROID_JAR" -d "$work/classes" "${sources[@]}"
mapfile -t classes < <(find "$work/classes" -name '*.class')
"$BUILD_TOOLS_BIN/d8" --min-api 30 --lib "$ANDROID_JAR" --output "$work/dex" "${classes[@]}"
(cd "$work/dex" && zip -q "$work/unaligned.apk" classes.dex)
mkdir -p "$work/lib/arm64-v8a"
cp "$engine" "$work/lib/arm64-v8a/libpocket_native.so"
cp "$title" "$work/lib/arm64-v8a/libtitle.so"
# stored, not compressed, and page-aligned by zipalign -p, so that Android
# loads them from the APK itself rather than unpacking a second copy
(cd "$work" && zip -q -r -0 unaligned.apk lib)
"$BUILD_TOOLS_BIN/zipalign" -p -f 4 "$work/unaligned.apk" "$work/aligned.apk"

if [ ! -f "$keystore" ]; then
    echo "== making a signing key for the apps, $keystore"
    keytool -genkeypair -keystore "$keystore" -storepass android -keypass android -alias pocketnative \
        -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Pocket Native" >/dev/null 2>&1
fi
"$BUILD_TOOLS_BIN/apksigner" sign --ks "$keystore" --ks-pass pass:android --ks-key-alias pocketnative --out "$out" "$work/aligned.apk"
rm -f "$out.idsig"
echo "$package"
