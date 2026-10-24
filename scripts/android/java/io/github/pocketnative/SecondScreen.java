package io.github.pocketnative;

import android.app.Activity;
import android.app.Presentation;
import android.content.Context;
import android.hardware.display.DisplayManager;
import android.util.Log;
import android.view.Display;
import android.view.MotionEvent;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.Window;
import android.view.WindowManager;

/**
 * One of the 3DS's screens on the handheld's second display, the bottom one
 * on the RG DS: a Presentation holding a SurfaceView, whose surface the
 * renderer draws into and whose touches go to the 3DS's touch screen. The
 * native methods are registered by frontend/src/android.rs.
 */
public final class SecondScreen implements SurfaceHolder.Callback, View.OnTouchListener {
    static native void surfaceReady(Surface surface);

    static native void surfaceGone();

    static native void touched(boolean pressed, float x, float y, int width, int height);

    /** kept so that it is not collected while it shows. */
    private static Presentation shown;

    /**
     * shows the screen on the first display that is not the activity's, from
     * the activity's own thread, and says whether there was such a display.
     */
    public static boolean show(final Activity activity) {
        DisplayManager displays = (DisplayManager) activity.getSystemService(Context.DISPLAY_SERVICE);
        int own = activity.getWindowManager().getDefaultDisplay().getDisplayId();
        Display other = null;
        for (Display display : displays.getDisplays()) {
            if (display.getDisplayId() != own) {
                other = display;
                break;
            }
        }
        if (other == null) {
            return false;
        }
        final Display target = other;
        Log.i("PocketNative", "second screen on display " + target.getDisplayId() + ", " + target.getName());
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Presentation presentation = new Presentation(activity, target);
                SurfaceView view = new SurfaceView(presentation.getContext());
                SecondScreen screen = new SecondScreen();
                view.getHolder().addCallback(screen);
                view.setOnTouchListener(screen);
                presentation.setContentView(view);
                presentation.setCancelable(false);
                Window window = presentation.getWindow();
                if (window != null) {
                    // touchable, but never taking the buttons from the game
                    window.addFlags(WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE
                            | WindowManager.LayoutParams.FLAG_FULLSCREEN
                            | WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                    window.getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY);
                }
                try {
                    presentation.show();
                    shown = presentation;
                } catch (WindowManager.InvalidDisplayException error) {
                    Log.w("PocketNative", "could not show the second screen", error);
                }
            }
        });
        return true;
    }

    @Override
    public void surfaceCreated(SurfaceHolder holder) {
    }

    @Override
    public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
        surfaceReady(holder.getSurface());
    }

    @Override
    public void surfaceDestroyed(SurfaceHolder holder) {
        surfaceGone();
    }

    @Override
    public boolean onTouch(View view, MotionEvent event) {
        int action = event.getActionMasked();
        boolean pressed = action == MotionEvent.ACTION_DOWN || action == MotionEvent.ACTION_MOVE
                || action == MotionEvent.ACTION_POINTER_DOWN;
        touched(pressed, event.getX(), event.getY(), view.getWidth(), view.getHeight());
        return true;
    }
}
