#!/usr/bin/env bash
# Build apps/vpn-probe.apk, the VPN probe: whether an app's VpnService can
# open a tunnel under Sarab. Every VPN app needs that, and it failed while
# /dev/tun did not exist (sarab-ns binds the host's /dev/net/tun there).
# Needs the Android SDK (build-tools, platforms/android-33) and a JDK. It
# signs with keys/debug.p12, the key apps/sarab-home/build.sh uses, and makes
# it the same way when the checkout has none. The APK is not committed. To
# run it, with Android up:
#
#   sarab install apps/vpn-probe.apk
#   sarab exec -u shell appops set org.sarab.vpnprobe ACTIVATE_VPN allow
#   sarab exec -u shell am start -n org.sarab.vpnprobe/.Start
#   sarab logs -- -s sarab-vpnprobe       # "established fd N", or why not
#   sarab exec ip addr show tun0          # 10.9.0.2, for thirty seconds
#   sarab app rm org.sarab.vpnprobe
#
# The `appops` line stands in for the consent dialog a VPN app shows first.
set -euo pipefail
cd "$(dirname "$0")/../.."
SDK=${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}
BT=$(printf '%s\n' "$SDK"/build-tools/* | sort -V | tail -1)
JAR="$SDK/platforms/android-33/android.jar"
KS=keys/debug.p12
OUT=apps/vpn-probe.apk
if [ ! -f "$KS" ]; then
    mkdir -p keys
    keytool -genkeypair -keystore "$KS" -storetype PKCS12 -storepass android \
        -alias sarab-debug -keyalg RSA -keysize 2048 -validity 10000 \
        -dname "CN=Sarab debug" -noprompt
    echo "made a new signing key: $KS"
fi
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
javac -source 8 -target 8 -cp "$JAR" -d "$TMP/classes" apps/vpn-probe/src/org/sarab/vpnprobe/*.java
mapfile -t CLASSES < <(find "$TMP/classes" -name '*.class')
"$BT/d8" --min-api 33 --lib "$JAR" --output "$TMP" "${CLASSES[@]}"
"$BT/aapt2" link -o "$TMP/unsigned.apk" -I "$JAR" --manifest apps/vpn-probe/AndroidManifest.xml
(cd "$TMP" && zip -q unsigned.apk classes.dex)
"$BT/zipalign" -f 4 "$TMP/unsigned.apk" "$TMP/aligned.apk"
"$BT/apksigner" sign --ks "$KS" --ks-pass pass:android --ks-type PKCS12 --out "$OUT" "$TMP/aligned.apk"
rm -f "$OUT.idsig"
ls -la "$OUT"
