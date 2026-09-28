#!/usr/bin/env bash
# Build apps/sarab-home.apk from apps/sarab-home/AndroidManifest.xml.
# The APK is committed and compiled into `sarab` (policy.rs), so this is only
# for changing it. Rebuild sarab afterwards.
# Needs the Android SDK build-tools (aapt2, apksigner) and platforms/android-33,
# and keytool (any JDK) the first time, to make a signing key.
#
# The key is keys/debug.p12, which is not in the repository: every checkout
# makes its own on first build. Android refuses to update an app in place with
# a different key, so on a /data that already has org.sarab.home from another
# key, `sarab policy apply` cannot install the rebuilt one until the old one is
# removed: `sarab exec -u shell pm uninstall org.sarab.home`.
set -euo pipefail
cd "$(dirname "$0")/../.."
SDK=${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}
BT=$(printf '%s\n' "$SDK"/build-tools/* | sort -V | tail -1)
JAR="$SDK/platforms/android-33/android.jar"
KS=keys/debug.p12
OUT=apps/sarab-home.apk
if [ ! -f "$KS" ]; then
    mkdir -p keys
    keytool -genkeypair -keystore "$KS" -storetype PKCS12 -storepass android \
        -alias sarab-debug -keyalg RSA -keysize 2048 -validity 10000 \
        -dname "CN=Sarab debug" -noprompt
    echo "made a new signing key: $KS"
fi
"$BT/aapt2" link -o "$OUT.unsigned" -I "$JAR" --manifest apps/sarab-home/AndroidManifest.xml
"$BT/apksigner" sign --ks "$KS" --ks-pass pass:android --ks-type PKCS12 --out "$OUT" "$OUT.unsigned"
rm -f "$OUT.unsigned" "$OUT.idsig"
ls -la "$OUT"
