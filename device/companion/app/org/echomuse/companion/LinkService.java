package org.echomuse.companion;

import android.app.AppOpsManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.graphics.PixelFormat;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import android.os.SystemClock;
import android.provider.Settings;
import android.util.Log;
import android.view.Gravity;
import android.view.WindowManager;

import java.io.BufferedInputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * The companion's only component: a foreground service that holds the local
 * link to the firmware (§3), draws the light bar over the kiosk and turns
 * touches in an active zone into presses. The firmware starts it with
 * {@code am start-foreground-service} and restarts it when the link stays
 * down (decision 5); it keeps no state across starts.
 *
 * <p>The link thread connects and reads; the writer thread writes; every
 * other field is confined to the main thread, which the link thread reaches
 * through {@link #main}. Events from a connection that is no longer current
 * are dropped.
 */
public final class LinkService extends Service {
    private static final String TAG = "EchoMuse";
    private static final String HOST = "127.0.0.1";
    private static final int PORT = 8771;
    private static final int CONNECT_TIMEOUT_MS = 2000;
    private static final long BACKOFF_MIN_MS = 500;
    private static final long BACKOFF_MAX_MS = 5000;
    /** screen wake: a brief timed lock; the kiosk's own timeout applies after it. */
    private static final long WAKE_MS = 5000;
    private static final String CHANNEL = "link";
    private static final int NOTIFICATION_ID = 1;

    private final Handler main = new Handler(Looper.getMainLooper());
    private final ExecutorService writer = Executors.newSingleThreadExecutor();
    private volatile boolean running;
    private volatile Socket socket;
    private Thread linkThread;

    private WindowManager windows;
    private NotificationManager notifications;
    private AppOpsManager appOps;
    private PowerManager power;
    private PowerManager.WakeLock wakeLock;
    private PowerManager.WakeLock holdLock;
    private BarView bar;
    private WindowManager.LayoutParams barParams;
    private boolean attached;
    private String appVersion;

    // Link state, main thread only.
    private Conn conn;
    private boolean linked;
    private long unlinkedSince;
    private String firmware;
    private Link.Status status;
    private long controllerDownSince;
    private Link.Frame frame;
    private boolean sentOverlay;
    private boolean sentScreenOn;
    private String shownTitle;
    private String shownText;

    private final Runnable tick = this::render;

    private final AppOpsManager.OnOpChangedListener opListener =
            (op, packageName) -> main.post(this::overlayChanged);

    private final BroadcastReceiver screenReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            reportState();
        }
    };

    @Override
    @SuppressWarnings("deprecation") // SCREEN_BRIGHT_WAKE_LOCK: still how a service lights the screen on API 30
    public void onCreate() {
        super.onCreate();
        windows = getSystemService(WindowManager.class);
        notifications = getSystemService(NotificationManager.class);
        appOps = getSystemService(AppOpsManager.class);
        power = getSystemService(PowerManager.class);
        try {
            appVersion = getPackageManager().getPackageInfo(getPackageName(), 0).versionName;
        } catch (PackageManager.NameNotFoundException e) {
            throw new IllegalStateException("own package not found", e);
        }

        // The lowest importance; a silent channel with no badge. Phase C0
        // item 3 confirms LineageOS 18.1 accepts IMPORTANCE_MIN for a
        // foreground service's notification and what the shade shows.
        NotificationChannel channel = new NotificationChannel(CHANNEL, "EchoMuse link", NotificationManager.IMPORTANCE_MIN);
        channel.setSound(null, null);
        channel.enableVibration(false);
        channel.enableLights(false);
        channel.setShowBadge(false);
        notifications.createNotificationChannel(channel);
        unlinkedSince = SystemClock.elapsedRealtime();
        startForeground(NOTIFICATION_ID, notification(StatusText.title(null, 0), StatusText.versions(null, appVersion)));

        int lock = PowerManager.SCREEN_BRIGHT_WAKE_LOCK | PowerManager.ON_AFTER_RELEASE;
        wakeLock = power.newWakeLock(lock | PowerManager.ACQUIRE_CAUSES_WAKEUP, "echomuse:wake");
        wakeLock.setReferenceCounted(false);
        holdLock = power.newWakeLock(lock, "echomuse:hold");
        holdLock.setReferenceCounted(false);

        bar = new BarView(this, this::press);
        barParams = new WindowManager.LayoutParams(
                WindowManager.LayoutParams.MATCH_PARENT, bar.windowHeight(),
                WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
                WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE
                        | WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE
                        | WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
                PixelFormat.TRANSLUCENT);
        barParams.gravity = Gravity.BOTTOM;
        // The bottom edge itself, not above a navigation bar.
        barParams.setFitInsetsTypes(0);
        barParams.setTitle("EchoMuse bar");

        IntentFilter screen = new IntentFilter(Intent.ACTION_SCREEN_ON);
        screen.addAction(Intent.ACTION_SCREEN_OFF);
        registerReceiver(screenReceiver, screen);
        appOps.startWatchingMode(AppOpsManager.OPSTR_SYSTEM_ALERT_WINDOW, getPackageName(), opListener);
        if (Settings.canDrawOverlays(this)) {
            attach();
        }
        render();

        running = true;
        linkThread = new Thread(this::runLink, "echomuse-link");
        linkThread.start();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        // Every start-foreground-service must be answered with startForeground,
        // including the firmware's restarts of a service already running.
        startForeground(NOTIFICATION_ID, notification(shownTitle, shownText));
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        running = false;
        Socket s = socket;
        if (s != null) {
            close(s);
        }
        linkThread.interrupt();
        writer.shutdownNow();
        main.removeCallbacks(tick);
        appOps.stopWatchingMode(opListener);
        unregisterReceiver(screenReceiver);
        detach();
        wakeLock.release();
        holdLock.release();
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    // ── link thread ─────────────────────────────────────────────────────────

    /** Connects, reads until the connection ends, and retries with backoff. */
    private void runLink() {
        long backoff = BACKOFF_MIN_MS;
        while (running) {
            Socket s = new Socket();
            Conn c = null;
            try {
                s.connect(new InetSocketAddress(HOST, PORT), CONNECT_TIMEOUT_MS);
                s.setTcpNoDelay(true);
                socket = s;
                c = new Conn(s, s.getOutputStream());
                final Conn opened = c;
                main.post(() -> connected(opened));
                Link.LineReader reader = new Link.LineReader(new BufferedInputStream(s.getInputStream()));
                boolean hello = false;
                String line;
                while ((line = reader.readLine()) != null) {
                    Link.Message m = Link.decode(line);
                    if (m == null) {
                        continue;
                    }
                    if (!hello) {
                        if (!(m instanceof Link.Hello)) {
                            throw new Link.ProtocolException("first message is not hello");
                        }
                        long version = ((Link.Hello) m).link;
                        if (version != Link.VERSION) {
                            // The firmware closes too and reinstalls its own app.
                            throw new Link.ProtocolException("firmware speaks link " + version + ", not " + Link.VERSION);
                        }
                        hello = true;
                        backoff = BACKOFF_MIN_MS;
                    }
                    final Conn from = c;
                    main.post(() -> receive(from, m));
                }
            } catch (Link.ProtocolException e) {
                Log.w(TAG, "link: " + e.getMessage());
            } catch (IOException e) {
                // Refused while the firmware is not listening, or reset: retry.
            } finally {
                close(s);
                socket = null;
                if (c != null) {
                    final Conn ended = c;
                    main.post(() -> lost(ended));
                }
            }
            try {
                Thread.sleep(backoff);
            } catch (InterruptedException e) {
                return;
            }
            backoff = Math.min(backoff * 2, BACKOFF_MAX_MS);
        }
    }

    private static void close(Socket s) {
        try {
            s.close();
        } catch (IOException ignored) {
            // Closing is all that is left to do with it.
        }
    }

    /** One connection's writing side; writes run on the writer thread. */
    private final class Conn {
        private final Socket sock;
        private final OutputStream out;

        Conn(Socket sock, OutputStream out) {
            this.sock = sock;
            this.out = out;
        }

        void send(String line) {
            byte[] bytes = (line + "\n").getBytes(StandardCharsets.UTF_8);
            writer.execute(() -> {
                try {
                    out.write(bytes);
                    out.flush();
                } catch (IOException e) {
                    // The reader sees the closed socket and reports the loss.
                    close(sock);
                }
            });
        }
    }

    // ── main thread ─────────────────────────────────────────────────────────

    private void connected(Conn c) {
        if (!running) {
            return;
        }
        conn = c;
        sentOverlay = Settings.canDrawOverlays(this);
        sentScreenOn = power.isInteractive();
        c.send(Link.hello(appVersion, sentOverlay, sentScreenOn));
    }

    private void receive(Conn c, Link.Message m) {
        if (!running || c != conn) {
            return;
        }
        if (m instanceof Link.Hello) {
            linked = true;
            firmware = ((Link.Hello) m).firmware;
        } else if (m instanceof Link.Status) {
            Link.Status s = (Link.Status) m;
            if (s.link == Link.State.DOWN && (status == null || status.link != Link.State.DOWN)) {
                controllerDownSince = SystemClock.elapsedRealtime();
            }
            status = s;
        } else if (m instanceof Link.Frame) {
            frame = (Link.Frame) m;
        } else if (m instanceof Link.Touch) {
            bar.setZone(((Link.Touch) m).zone);
        } else if (m instanceof Link.Screen) {
            Link.Screen s = (Link.Screen) m;
            if (s.wake) {
                wakeLock.acquire(WAKE_MS);
            }
            if (s.hold != null) {
                hold(s.hold);
            }
        }
        render();
    }

    /**
     * The connection ended. Touches and the screen hold end at once; the bar
     * and the status stand for the grace period, in case the firmware is only
     * restarting, and then go.
     */
    private void lost(Conn c) {
        if (!running || c != conn) {
            return;
        }
        conn = null;
        if (linked) {
            linked = false;
            unlinkedSince = SystemClock.elapsedRealtime();
        }
        bar.setZone(Link.Zone.NONE);
        hold(false);
        render();
    }

    private void hold(boolean on) {
        if (on && !holdLock.isHeld()) {
            holdLock.acquire();
        } else if (!on && holdLock.isHeld()) {
            holdLock.release();
        }
    }

    private void press(boolean down, long heldMs) {
        if (conn != null && linked) {
            conn.send(down ? Link.pressDown() : Link.pressUp(heldMs));
        }
    }

    /** The overlay op changed: follow it and tell the firmware. */
    private void overlayChanged() {
        if (!running) {
            return;
        }
        if (Settings.canDrawOverlays(this)) {
            attach();
        } else {
            detach();
        }
        reportState();
    }

    /** Sends state when the overlay op or the screen differs from what the firmware last heard. */
    private void reportState() {
        boolean overlay = Settings.canDrawOverlays(this);
        boolean screenOn = power.isInteractive();
        if (conn != null && (overlay != sentOverlay || screenOn != sentScreenOn)) {
            sentOverlay = overlay;
            sentScreenOn = screenOn;
            conn.send(Link.state(overlay, screenOn));
        }
    }

    /** Brings the bar, the window and the notification up to date. */
    private void render() {
        long now = SystemClock.elapsedRealtime();
        long unlinkedMs = linked ? -1 : now - unlinkedSince;
        long downMs = now - controllerDownSince;
        bar.setFrame(StatusText.blank(unlinkedMs) ? null : frame);
        bar.setStatus(StatusText.overlay(status, unlinkedMs, downMs));
        layout();
        notify(StatusText.title(status, unlinkedMs), StatusText.versions(firmware, appVersion));
        main.removeCallbacks(tick);
        long next = StatusText.nextChangeMs(status, unlinkedMs, downMs);
        if (next >= 0) {
            main.postDelayed(tick, next);
        }
    }

    private void attach() {
        if (attached) {
            return;
        }
        layoutParams();
        try {
            windows.addView(bar, barParams);
            attached = true;
        } catch (RuntimeException e) {
            // BadTokenException or SecurityException: the op is not in force
            // yet. The next op change tries again.
            Log.w(TAG, "overlay: " + e);
        }
    }

    private void detach() {
        if (attached) {
            windows.removeView(bar);
            attached = false;
        }
    }

    /** Resizes the window, and makes it touchable, when the bar's needs change. */
    private void layout() {
        if (layoutParams() && attached) {
            windows.updateViewLayout(bar, barParams);
        }
    }

    /** Updates barParams from the bar; true when they changed. */
    private boolean layoutParams() {
        int height = bar.windowHeight();
        int flags = bar.touchable()
                ? barParams.flags & ~WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE
                : barParams.flags | WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE;
        if (height == barParams.height && flags == barParams.flags) {
            return false;
        }
        barParams.height = height;
        barParams.flags = flags;
        return true;
    }

    private void notify(String title, String text) {
        if (title.equals(shownTitle) && text.equals(shownText)) {
            return;
        }
        notifications.notify(NOTIFICATION_ID, notification(title, text));
    }

    /** The link state line and the versions; no actions (decision 13). */
    private Notification notification(String title, String text) {
        shownTitle = title;
        shownText = text;
        return new Notification.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(title)
                .setContentText(text)
                .setCategory(Notification.CATEGORY_SERVICE)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setShowWhen(false)
                .build();
    }
}
