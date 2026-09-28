/*
 * The VPN probe's service: `establish` is where Android opens /dev/tun and
 * configures the interface through netd, so its result is the test. It asks
 * for 10.9.0.2/32 with a route to 10.9.0.0/24 only, so the rest of Android's
 * traffic is untouched, logs "established <interface>" or the failure under
 * the tag `sarab-vpnprobe`, holds the tunnel for thirty seconds so it can be
 * looked at (`ip addr`), then closes it and stops.
 */
package org.sarab.vpnprobe;

import android.content.Intent;
import android.net.VpnService;
import android.os.ParcelFileDescriptor;
import android.util.Log;

public class Tunnel extends VpnService {
    static final String TAG = "sarab-vpnprobe";

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        new Thread(() -> {
            try {
                ParcelFileDescriptor tun = new Builder()
                        .setSession("sarab vpn probe")
                        .addAddress("10.9.0.2", 32)
                        .addRoute("10.9.0.0", 24)
                        .establish();
                if (tun == null) {
                    Log.e(TAG, "establish returned null: no consent, or revoked");
                } else {
                    Log.i(TAG, "established fd " + tun.getFd());
                    Thread.sleep(30000);
                    tun.close();
                    Log.i(TAG, "closed");
                }
            } catch (Throwable t) {
                Log.e(TAG, "establish failed", t);
            }
            stopSelf();
        }).start();
        return START_NOT_STICKY;
    }
}
