/*
 * The VPN probe's entry point (build.sh has the commands). It shows nothing:
 * without the user's consent, which build.sh grants with `appops`, it logs
 * that and stops; with it, it starts Tunnel and finishes.
 */
package org.sarab.vpnprobe;

import android.app.Activity;
import android.content.Intent;
import android.net.VpnService;
import android.os.Bundle;
import android.util.Log;

public class Start extends Activity {
    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        if (VpnService.prepare(this) != null) {
            Log.e(Tunnel.TAG, "no VPN consent: appops set org.sarab.vpnprobe ACTIVATE_VPN allow");
        } else {
            startService(new Intent(this, Tunnel.class));
        }
        finish();
    }
}
