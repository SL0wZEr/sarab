#!/system/bin/sh
# Runs Probe (Probe.java) inside Android, with its arguments passed through.
# app_process needs zygote's classpath variables, which `sarab exec` passes.
# Build and push, from the checkout, with SDK set to an Android SDK:
#
#   javac -source 8 -target 8 -cp $SDK/platforms/android-33/android.jar -d /tmp/probe apps/media-probe/Probe.java
#   $SDK/build-tools/35.0.0/d8 --min-api 33 --lib $SDK/platforms/android-33/android.jar --output /tmp/probe /tmp/probe/*.class
#   for f in /tmp/probe/classes.dex apps/media-probe/run.sh media...; do
#     sarab exec sh -c "mkdir -p /data/local/tmp/probe && cat > /data/local/tmp/probe/${f##*/}" < $f
#   done
#   sarab exec sh /data/local/tmp/probe/run.sh /data/local/tmp/probe/sample.m4a
#
# Samples come from ffmpeg, e.g. `ffmpeg -f lavfi -i sine=f=440:d=1 -c:a aac
# sample.m4a` or `-f lavfi -i testsrc=size=320x240:rate=30 -t 1 -c:v libx264
# -pix_fmt yuv420p sample.mp4`. amdgpu.ids is missing from the image, and
# Mesa's complaint about it is filtered out.

cd /data/local/tmp/probe || exit 1
CLASSPATH=/data/local/tmp/probe/classes.dex app_process /data/local/tmp/probe Probe "$@" 2>&1 | grep -v amdgpu.ids
