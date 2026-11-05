// The Java half of the Android network monitor (docs/design/android.md,
// "Android monitor"). `mise run android:dex` compiles it into monitor.dex,
// which the native library embeds and loads with an InMemoryDexClassLoader,
// so the app ships no Java of euripos's own. Edit this file, then regenerate
// the dex; CI fails when the two differ.

package org.getkono.euripos;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.Proxy;
import android.net.ProxyInfo;
import android.net.RouteInfo;
import android.net.Uri;
import java.net.Inet4Address;
import java.net.Inet6Address;
import java.net.InetAddress;

/**
 * Follows the default network and reports it to native code on every change.
 *
 * <p>All state is guarded by the instance's lock: network callbacks arrive on
 * the connectivity thread, broadcasts on the main thread.
 */
final class NetworkMonitor extends ConnectivityManager.NetworkCallback {
    // The bits of `flags` in `changed`; android.rs reads the same values.
    static final int CAPABILITIES = 1;
    static final int NOT_METERED = 1 << 1;
    static final int TEMPORARILY_NOT_METERED = 1 << 2;
    static final int LINK = 1 << 3;
    static final int IPV4 = 1 << 4;
    static final int IPV6 = 1 << 5;
    static final int BLOCKED = 1 << 6;
    static final int DATA_SAVER_KNOWN = 1 << 7;
    static final int DATA_SAVER = 1 << 8;

    // NetworkCapabilities.NET_CAPABILITY_TEMPORARILY_NOT_METERED, from API 30;
    // below that no network has it.
    private static final int TEMPORARILY_NOT_METERED_CAPABILITY = 25;

    private final ConnectivityManager connectivity;
    private Network network;
    private NetworkCapabilities capabilities;
    private LinkProperties link;
    private boolean blocked;

    private final BroadcastReceiver receiver =
            new BroadcastReceiver() {
                @Override
                public void onReceive(Context context, Intent intent) {
                    synchronized (NetworkMonitor.this) {
                        report();
                    }
                }
            };

    private NetworkMonitor(ConnectivityManager connectivity) {
        this.connectivity = connectivity;
    }

    /**
     * Starts following the default network. Throws where the app may not:
     * registering the callback needs ACCESS_NETWORK_STATE.
     */
    static void start(Context context) {
        ConnectivityManager connectivity = context.getSystemService(ConnectivityManager.class);
        if (connectivity == null) {
            throw new IllegalStateException("no connectivity service");
        }
        NetworkMonitor monitor = new NetworkMonitor(connectivity);
        // Data Saver and a global proxy change without the network changing.
        IntentFilter filter = new IntentFilter(ConnectivityManager.ACTION_RESTRICT_BACKGROUND_CHANGED);
        filter.addAction(Proxy.PROXY_CHANGE_ACTION);
        // The application's context, so the receiver outlives an activity;
        // it is null only while the application itself is being created.
        Context application = context.getApplicationContext();
        (application != null ? application : context).registerReceiver(monitor.receiver, filter);
        connectivity.registerDefaultNetworkCallback(monitor);
    }

    @Override
    public synchronized void onAvailable(Network network) {
        follow(network);
        report();
    }

    @Override
    public synchronized void onCapabilitiesChanged(Network network, NetworkCapabilities capabilities) {
        follow(network);
        this.capabilities = capabilities;
        report();
    }

    @Override
    public synchronized void onLinkPropertiesChanged(Network network, LinkProperties link) {
        follow(network);
        this.link = link;
        report();
    }

    @Override
    public synchronized void onBlockedStatusChanged(Network network, boolean blocked) {
        follow(network);
        this.blocked = blocked;
        report();
    }

    @Override
    public synchronized void onLost(Network network) {
        if (network.equals(this.network)) {
            follow(null);
        }
        report();
    }

    /** Makes `network` the one followed, forgetting what was known of another. */
    private void follow(Network network) {
        if (network == null ? this.network != null : !network.equals(this.network)) {
            this.network = network;
            capabilities = null;
            link = null;
            blocked = false;
        }
    }

    private void report() {
        long handle = 0;
        int transports = 0;
        int flags = 0;
        String host = null;
        int port = -1;
        String exclusions = null;
        String pac = null;
        if (network != null) {
            handle = network.getNetworkHandle();
            if (capabilities != null) {
                flags |= CAPABILITIES;
                for (int transport = 0; transport < 31; transport++) {
                    if (capabilities.hasTransport(transport)) {
                        transports |= 1 << transport;
                    }
                }
                if (capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)) {
                    flags |= NOT_METERED;
                }
                if (capabilities.hasCapability(TEMPORARILY_NOT_METERED_CAPABILITY)) {
                    flags |= TEMPORARILY_NOT_METERED;
                }
            }
            if (link != null) {
                flags |= LINK;
                for (RouteInfo route : link.getRoutes()) {
                    if (route.isDefaultRoute()) {
                        InetAddress destination = route.getDestination().getAddress();
                        if (destination instanceof Inet4Address) {
                            flags |= IPV4;
                        } else if (destination instanceof Inet6Address) {
                            flags |= IPV6;
                        }
                    }
                }
            }
            if (blocked) {
                flags |= BLOCKED;
            }
            // The global proxy where one is set, else the default network's
            // LinkProperties proxy: what the platform's own HTTP stack uses.
            ProxyInfo proxy = connectivity.getDefaultProxy();
            if (proxy != null) {
                host = proxy.getHost();
                port = proxy.getPort();
                exclusions = String.join(",", proxy.getExclusionList());
                Uri script = proxy.getPacFileUrl();
                if (script != null && !Uri.EMPTY.equals(script)) {
                    pac = script.toString();
                }
            }
        }
        switch (connectivity.getRestrictBackgroundStatus()) {
            case ConnectivityManager.RESTRICT_BACKGROUND_STATUS_ENABLED:
                flags |= DATA_SAVER_KNOWN | DATA_SAVER;
                break;
            case ConnectivityManager.RESTRICT_BACKGROUND_STATUS_DISABLED:
            case ConnectivityManager.RESTRICT_BACKGROUND_STATUS_WHITELISTED:
                flags |= DATA_SAVER_KNOWN;
                break;
            default:
                break;
        }
        changed(handle, transports, flags, host, port, exclusions, pac);
    }

    /** Implemented in android.rs and registered before `start`. */
    private static native void changed(
            long handle,
            int transports,
            int flags,
            String proxyHost,
            int proxyPort,
            String proxyExclusions,
            String pacUrl);
}
