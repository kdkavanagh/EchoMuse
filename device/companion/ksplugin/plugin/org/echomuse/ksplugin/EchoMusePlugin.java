package org.echomuse.ksplugin;

import android.content.Context;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.view.View;

import java.io.BufferedInputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Collections;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;

import me.jxl.kiosk.plugins.KioskPlugin;
import me.jxl.kiosk.plugins.KsTheme;
import me.jxl.kiosk.plugins.OverlayFactory;
import me.jxl.kiosk.plugins.OverlaySpec;
import me.jxl.kiosk.plugins.PluginHost;
import org.echomuse.companion.Link;

/**
 * The EchoMuse turn and timer views inside Kiosk Satellite
 * (docs/echo-show-companion-plan.md §11). The firmware installs it and keeps
 * it current (decision 2); it holds the local link, role {@code plugin}
 * (§11.3), draws each turn the controller's turn.view describes in one
 * long-lived overlay (decision 9), and the speaker's HA timers from its
 * timer.view in another, in the top-right corner, hidden while a turn is up.
 *
 * <p>Threads: Kiosk Satellite runs StrictMode, so the link thread alone
 * connects, writes the hello (the plugin's only line) and reads; every view
 * field is confined to the main thread, which the link thread reaches
 * through {@link #main}. Callbacks from Kiosk Satellite arrive on its plugin
 * worker. Anything run on the main thread catches its own exceptions: one
 * escaping there ends the Kiosk Satellite app (SDK docs/overlays.md).
 */
public final class EchoMusePlugin implements KioskPlugin {
    private static final String HOST = "127.0.0.1";
    private static final int PORT = 8771;
    private static final int CONNECT_TIMEOUT_MS = 2000;
    private static final long BACKOFF_MIN_MS = 500;
    private static final long BACKOFF_MAX_MS = 5000;
    /** stop() must return within Kiosk Satellite's 3 s callback deadline. */
    private static final long JOIN_MS = 2500;
    private static final String OVERLAY = "turn_view";
    private static final String TIMER_OVERLAY = "timers";
    private static final String SCREENSAVER = "screensaver.state";

    private final Handler main = new Handler(Looper.getMainLooper());
    private volatile PluginHost host;
    private volatile boolean running;
    private volatile Socket socket;
    private Thread linkThread;

    /** Kiosk Satellite's screensaver is up; written on the plugin worker. */
    private volatile boolean screensaverActive;
    /** Counts screensaver events, so a state read issued before one cannot overwrite it. */
    private final AtomicInteger screensaverEvents = new AtomicInteger();

    // Main thread only.
    private final TurnView turn = new TurnView();
    private ViewOverlay overlay;
    /** The latest frame's colours, or null while the bar is dark or the link is down. */
    private int[] glow;
    private final TimerView timerView = new TimerView();
    private TimerOverlay timerOverlay;

    private final Runnable tick = guarded(() -> {
        turn.tick(SystemClock.uptimeMillis());
        render();
    });
    private final Runnable timerTick = guarded(() -> {
        timerView.tick(SystemClock.uptimeMillis());
        render();
    });

    @Override
    public void start(PluginHost host, Map<String, Object> settings) {
        this.host = host;
        running = true;
        host.showOverlay(OVERLAY, OverlaySpec.fullScreen().onTop(true).touchable(false).closeOnBack(false),
                new Factory());
        host.showOverlay(TIMER_OVERLAY, OverlaySpec.at(OverlaySpec.TOP_RIGHT)
                .size(TimerOverlay.WIDTH_DP, TimerOverlay.HEIGHT_DP).inset(24)
                .onTop(true).touchable(false).closeOnBack(false), new TimerFactory());
        // Subscribe first, then read: an event after the read wins (SDK docs/ks-api.md).
        host.subscribe(SCREENSAVER);
        int seen = screensaverEvents.get();
        host.executeCommand("isScreensaverActive", Collections.<String, Object>emptyMap(), (ok, data, error) -> {
            if (!ok) {
                log("isScreensaverActive: " + error);
            } else if (screensaverEvents.get() == seen) {
                screensaverActive = Boolean.TRUE.equals(data);
            }
        });
        linkThread = new Thread(this::runLink, "echomuse-ksplugin-link");
        linkThread.setDaemon(true);
        linkThread.start();
    }

    @Override
    public void configure(Map<String, Object> settings) {
        // No settings: the linger comes from the device config over the link.
    }

    @Override
    public void execute(String command, Map<String, Object> arguments) {
        // No commands.
    }

    @Override
    public void onEvent(String event, Map<String, Object> payload) {
        if (("ks." + SCREENSAVER).equals(event) && payload != null) {
            screensaverEvents.incrementAndGet();
            screensaverActive = Boolean.TRUE.equals(payload.get("active"));
        }
    }

    @Override
    public void stop() throws InterruptedException {
        running = false;
        Socket s = socket;
        if (s != null) {
            close(s);
        }
        Thread t = linkThread;
        if (t != null) {
            t.interrupt();
            t.join(JOIN_MS);
        }
        // Kiosk Satellite removes the overlay with the session.
        main.removeCallbacksAndMessages(null);
    }

    // ── link thread ─────────────────────────────────────────────────────────

    /** Connects, reads until the connection ends, and retries with backoff. */
    private void runLink() {
        long backoff = BACKOFF_MIN_MS;
        while (running) {
            Socket s = new Socket();
            socket = s;
            // stop() may have read socket before it was set: check after.
            if (!running) {
                close(s);
                return;
            }
            boolean linked = false;
            try {
                s.connect(new InetSocketAddress(HOST, PORT), CONNECT_TIMEOUT_MS);
                s.setTcpNoDelay(true);
                OutputStream out = s.getOutputStream();
                out.write((Link.pluginHello(BuildInfo.VERSION) + "\n").getBytes(StandardCharsets.UTF_8));
                out.flush();
                Link.LineReader reader = new Link.LineReader(new BufferedInputStream(s.getInputStream()));
                String line;
                while ((line = reader.readLine()) != null) {
                    Link.Message m = Link.decode(line);
                    if (m == null) {
                        continue;
                    }
                    if (!linked) {
                        String refused = refusal(m);
                        if (refused != null) {
                            // The firmware closes too, and installs its own plugin
                            // over this one (decision 2), which ends this session.
                            log(refused);
                            break;
                        }
                        linked = true;
                        backoff = BACKOFF_MIN_MS;
                        log("linked to firmware " + ((Link.Hello) m).firmware);
                        continue;
                    }
                    main.post(guarded(() -> receive(m)));
                }
            } catch (Link.ProtocolException e) {
                log("link: " + e.getMessage());
            } catch (IOException e) {
                // Refused while the firmware is not listening, or reset: retry.
            } finally {
                close(s);
                socket = null;
                if (linked) {
                    main.post(guarded(this::lost));
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

    /** Why the firmware's first message ends the connection, or null for a matching hello. */
    private static String refusal(Link.Message m) {
        if (!(m instanceof Link.Hello)) {
            return "link: first message is not hello";
        }
        Link.Hello h = (Link.Hello) m;
        if (h.link != Link.VERSION) {
            return "link: firmware speaks link " + h.link + ", not " + Link.VERSION;
        }
        if (!BuildInfo.VERSION.equals(h.plugin)) {
            return "link: firmware " + h.firmware + " embeds plugin " + h.plugin + ", this is " + BuildInfo.VERSION;
        }
        return null;
    }

    private static void close(Socket s) {
        try {
            s.close();
        } catch (IOException ignored) {
            // Closing is all that is left to do with it.
        }
    }

    // ── main thread ─────────────────────────────────────────────────────────

    private void receive(Link.Message m) {
        if (!running) {
            return;
        }
        if (m instanceof Link.Config) {
            turn.linger(((Link.Config) m).turnViewLinger);
        } else if (m instanceof Link.Frame) {
            Link.Frame f = (Link.Frame) m;
            int[] colours = f.dark() ? null : f.argb();
            if (Arrays.equals(colours, glow)) {
                return;
            }
            glow = colours;
            if (overlay != null) {
                overlay.setGlow(glow);
            }
        } else if (m instanceof Link.View) {
            Link.View v = (Link.View) m;
            turn.view(v, SystemClock.uptimeMillis());
            if (v.phase == Link.Phase.LISTENING && screensaverActive) {
                // The black screensaver holds the backlight at 1 of 255, so a
                // view drawn over it is not seen (findings §8.7).
                stopScreensaver();
            }
            render();
        } else if (m instanceof Link.Timers) {
            timerView.timers((Link.Timers) m, SystemClock.uptimeMillis());
            render();
        }
    }

    /** The link went down after its hello. */
    private void lost() {
        if (!running) {
            return;
        }
        turn.linkLost(SystemClock.uptimeMillis());
        timerView.linkLost();
        glow = null;
        if (overlay != null) {
            overlay.setGlow(null);
        }
        render();
    }

    /** Shows the reducers' state and arms each one's tick for its next deadline. */
    private void render() {
        if (overlay != null) {
            overlay.show(turn.shown());
        }
        if (timerOverlay != null) {
            timerOverlay.show(timerView, turn.shown() != null);
        }
        main.removeCallbacks(tick);
        long at = turn.hideAt();
        if (at != TurnView.NEVER) {
            main.postAtTime(tick, at);
        }
        main.removeCallbacks(timerTick);
        at = timerView.staleAt();
        if (at != TimerView.NEVER) {
            main.postAtTime(timerTick, at);
        }
    }

    private void stopScreensaver() {
        PluginHost h = host;
        try {
            h.executeCommand("stopScreensaver", Collections.<String, Object>emptyMap(), (ok, data, error) -> {
                if (!ok) {
                    log("stopScreensaver: " + error);
                }
            });
        } catch (RuntimeException e) {
            // The session has ended, or the command budget is spent.
            log("stopScreensaver: " + e);
        }
    }

    private void log(String message) {
        PluginHost h = host;
        try {
            h.log(message);
        } catch (RuntimeException ignored) {
            // The session has ended: nowhere left to log to.
        }
    }

    private Runnable guarded(Runnable r) {
        return () -> {
            try {
                r.run();
            } catch (Throwable t) {
                log("main thread: " + t);
            }
        };
    }

    /** Builds the turn view's overlay; Kiosk Satellite calls these on the main thread. */
    private final class Factory implements OverlayFactory {
        @Override
        public View create(Context context, KsTheme theme) {
            ViewOverlay v = new ViewOverlay(context, theme);
            v.setGlow(glow);
            v.show(turn.shown());
            overlay = v;
            return v;
        }

        @Override
        public void onThemeChanged(View view, KsTheme theme) {
            try {
                ((ViewOverlay) view).setTheme(theme);
            } catch (RuntimeException e) {
                log("theme: " + e);
            }
        }

        @Override
        public void onDestroy(View view) {
            if (overlay == view) {
                overlay = null;
            }
        }
    }

    /** Builds the timer view's overlay, as {@link Factory} does the turn view's. */
    private final class TimerFactory implements OverlayFactory {
        @Override
        public View create(Context context, KsTheme theme) {
            TimerOverlay v = new TimerOverlay(context, theme);
            v.show(timerView, turn.shown() != null);
            timerOverlay = v;
            return v;
        }

        @Override
        public void onThemeChanged(View view, KsTheme theme) {
            try {
                ((TimerOverlay) view).setTheme(theme);
            } catch (RuntimeException e) {
                log("theme: " + e);
            }
        }

        @Override
        public void onDestroy(View view) {
            if (timerOverlay == view) {
                timerOverlay = null;
            }
        }
    }
}
