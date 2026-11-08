package io.github.pocketnative;

import android.app.Activity;
import android.app.ActivityManager;
import android.app.ActivityOptions;
import android.app.AlertDialog;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.res.Configuration;
import android.content.pm.PackageInstaller;
import android.content.pm.ShortcutInfo;
import android.content.pm.ShortcutManager;
import android.database.Cursor;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.Color;
import android.graphics.drawable.ColorDrawable;
import android.graphics.drawable.Icon;
import android.graphics.drawable.StateListDrawable;
import android.hardware.display.DisplayManager;
import android.net.Uri;
import android.os.Bundle;
import android.os.Environment;
import android.provider.DocumentsContract;
import android.util.TypedValue;
import android.view.Display;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.BaseAdapter;
import android.widget.Button;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.ListView;
import android.widget.FrameLayout;
import android.widget.ProgressBar;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.RandomAccessFile;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;
import org.json.JSONArray;
import org.json.JSONObject;

/**
 * Pocket Native's launcher: the games added to it, Add game for one the PC kit
 * built (its folder, or a pack file), Device for how games show, and Update
 * for a newer release from GitHub. A game plays in Game, in a process of its
 * own.
 *
 * Each game is a folder named after its title id in games/, in the app's
 * folder on a removable card when there is one, else on shared storage:
 * game.3ds, sysdata/ and data/ (saves, its log), with name.txt and icon.png
 * from the game's own banner. Its recompiled code goes in code/ in the app's
 * own storage, read only, where Android lets an app load code from.
 */
public final class Launcher extends Activity {
    private static final int GREY = 0xFF3A3A3A;
    private static final int BLUE = 0xFF2F6FE0;
    private static final int ADD_FOLDER = 1;
    private static final int ADD_PACK = 2;
    /** a shortcut's intent: the game in the "game" extra, played at once. */
    private static final String PLAY_GAME = "io.github.pocketnative.PLAY";
    private static final String RELEASES = "https://api.github.com/repos/whovian64/pocket-native/releases/latest";

    private final List<File> games = new ArrayList<>();
    private GameList list;
    private TextView empty;
    /** the game list, and the Device page in its place while Device is lit. */
    private View gamesPage;
    private ScrollView devicePage;
    private Button deviceButton;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        unpackProfiles();
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(Color.BLACK);
        root.setFitsSystemWindows(true);

        LinearLayout bar = new LinearLayout(this);
        bar.setOrientation(LinearLayout.HORIZONTAL);
        bar.setPadding(dp(8), dp(8), dp(8), dp(8));
        bar.addView(button("Update", v -> update()), weight());
        bar.addView(button("Add game", v -> addGame()), weight());
        deviceButton = button("Device", v -> showDevice(devicePage.getVisibility() != View.VISIBLE));
        bar.addView(deviceButton, weight());
        root.addView(bar, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

        FrameLayout body = new FrameLayout(this);
        LinearLayout page = new LinearLayout(this);
        page.setOrientation(LinearLayout.VERTICAL);
        gamesPage = page;
        body.addView(page);
        devicePage = new ScrollView(this);
        devicePage.setVisibility(View.GONE);
        body.addView(devicePage);
        root.addView(body, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1));

        empty = new TextView(this);
        empty.setText("No games yet. Build a game with Pocket Native's kit on a PC, copy its folder or pack file here, then Add game.");
        empty.setTextColor(0xFFBBBBBB);
        empty.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16);
        empty.setPadding(dp(16), dp(24), dp(16), dp(16));
        page.addView(empty);

        ListView view = new ListView(this);
        view.setDivider(new ColorDrawable(0xFF202020));
        view.setDividerHeight(1);
        view.setSelector(highlight(Color.TRANSPARENT));
        list = new GameList();
        view.setAdapter(list);
        view.setOnItemClickListener((parent, row, position, id) -> play(games.get(position)));
        view.setOnItemLongClickListener((parent, row, position, id) -> {
            pin(games.get(position));
            return true;
        });
        page.addView(view, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1));
        setContentView(root);
        refresh();
        playShortcut(getIntent());
    }

    /** a game from a shortcut on the home screen, its folder in the extra. */
    private void playShortcut(Intent intent) {
        String game = intent.getStringExtra("game");
        if (PLAY_GAME.equals(intent.getAction()) && game != null) {
            setIntent(new Intent());
            if (new File(game, "game.3ds").isFile() && intent.getBooleanExtra("again", false)) {
                playAgain(new File(game));
            } else if (new File(game, "game.3ds").isFile()) {
                play(new File(game));
            } else {
                Toast.makeText(this, "That game is not here any more", Toast.LENGTH_LONG).show();
            }
        }
    }

    /** a game's shortcut, its icon and name, for the home screen. */
    private ShortcutInfo shortcut(File folder) {
        Bitmap icon = BitmapFactory.decodeFile(new File(folder, "icon.png").getPath());
        Intent intent = new Intent(PLAY_GAME).setClass(this, Launcher.class).putExtra("game", folder.getAbsolutePath());
        ShortcutInfo.Builder builder = new ShortcutInfo.Builder(this, folder.getName()).setShortLabel(name(folder)).setIntent(intent);
        if (icon != null) {
            builder.setIcon(Icon.createWithBitmap(Bitmap.createScaledBitmap(icon, 192, 192, false)));
        }
        return builder.build();
    }

    /** asks the home screen, Cocoon or any other, to put a game on it. */
    private void pin(File folder) {
        ShortcutManager shortcuts = getSystemService(ShortcutManager.class);
        if (shortcuts == null || !shortcuts.isRequestPinShortcutSupported()) {
            Toast.makeText(this, "This home screen does not take shortcuts", Toast.LENGTH_LONG).show();
            return;
        }
        shortcuts.requestPinShortcut(shortcut(folder), null);
    }

    private int dp(int value) {
        return (int) TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, value, getResources().getDisplayMetrics());
    }

    /** an equal share of a row, a button's height. */
    private LinearLayout.LayoutParams weight() {
        LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(0, dp(48), 1);
        params.setMargins(dp(3), 0, dp(3), 0);
        return params;
    }

    /** blue when pressed or picked with a pad, the colour given otherwise. */
    private static StateListDrawable highlight(int normal) {
        StateListDrawable states = new StateListDrawable();
        states.addState(new int[] {android.R.attr.state_pressed}, new ColorDrawable(BLUE));
        states.addState(new int[] {android.R.attr.state_focused}, new ColorDrawable(BLUE));
        states.addState(new int[] {android.R.attr.state_selected}, new ColorDrawable(BLUE));
        states.addState(new int[] {}, new ColorDrawable(normal));
        return states;
    }

    private Button button(String label, View.OnClickListener action) {
        Button button = new Button(this);
        button.setText(label);
        button.setAllCaps(false);
        button.setTextColor(Color.WHITE);
        // on one line, the text smaller where the button is narrow
        button.setMaxLines(1);
        button.setPadding(dp(4), 0, dp(4), 0);
        button.setAutoSizeTextTypeUniformWithConfiguration(9, 16, 1, TypedValue.COMPLEX_UNIT_SP);
        button.setBackground(highlight(GREY));
        button.setOnClickListener(action);
        return button;
    }

    // ---- the games

    /**
     * the app's folders on each storage volume, the one with the most room
     * first, which new games go on: a card's small boot partition is
     * mounted as a card too.
     */
    private List<File> volumes() {
        List<File> found = new ArrayList<>();
        for (File dir : getExternalFilesDirs(null)) {
            if (dir != null && Environment.MEDIA_MOUNTED.equals(Environment.getExternalStorageState(dir))) {
                found.add(dir);
            }
        }
        Collections.sort(found, (a, b) -> Long.compare(b.getUsableSpace(), a.getUsableSpace()));
        return found;
    }

    private File codeFor(String id) {
        return new File(new File(getFilesDir(), "code"), id + ".so");
    }

    private void refresh() {
        games.clear();
        for (File volume : volumes()) {
            File[] folders = new File(volume, "games").listFiles();
            if (folders == null) {
                continue;
            }
            for (File folder : folders) {
                // not one still coming in, or left half copied
                if (!folder.getName().startsWith(".") && new File(folder, "game.3ds").isFile()) {
                    games.add(folder);
                }
            }
        }
        Collections.sort(games, (a, b) -> name(a).compareToIgnoreCase(name(b)));
        empty.setVisibility(games.isEmpty() ? View.VISIBLE : View.GONE);
        list.notifyDataSetChanged();
        // the games as the app's shortcuts too, for launchers that list them
        ShortcutManager shortcuts = getSystemService(ShortcutManager.class);
        if (shortcuts != null) {
            List<ShortcutInfo> each = new ArrayList<>();
            for (File folder : games.subList(0, Math.min(games.size(), shortcuts.getMaxShortcutCountPerActivity()))) {
                each.add(shortcut(folder));
            }
            try {
                shortcuts.setDynamicShortcuts(each);
            } catch (RuntimeException ignored) {
                // over the rate limit, the ones before stay
            }
        }
    }

    /** the game's name: its profile's, or the one its banner gives. */
    private String name(File folder) {
        String id = folder.getName();
        File profile = new File(new File(new File(getFilesDir(), "titles"), id), "profile.toml");
        try {
            for (String line : new String(readAll(profile), StandardCharsets.UTF_8).split("\n")) {
                if (line.startsWith("name = \"") && line.trim().endsWith("\"")) {
                    return line.trim().substring(8, line.trim().length() - 1);
                }
            }
        } catch (IOException ignored) {
        }
        try {
            return new String(readAll(new File(folder, "name.txt")), StandardCharsets.UTF_8).trim();
        } catch (IOException ignored) {
            return id;
        }
    }

    private static byte[] readAll(File file) throws IOException {
        try (FileInputStream in = new FileInputStream(file)) {
            byte[] bytes = new byte[(int) file.length()];
            int at = 0;
            while (at < bytes.length) {
                int read = in.read(bytes, at, bytes.length - at);
                if (read < 0) {
                    break;
                }
                at += read;
            }
            return bytes;
        }
    }

    private final class GameList extends BaseAdapter {
        @Override
        public int getCount() {
            return games.size();
        }

        @Override
        public Object getItem(int position) {
            return games.get(position);
        }

        @Override
        public long getItemId(int position) {
            return position;
        }

        @Override
        public View getView(int position, View reused, ViewGroup parent) {
            File folder = games.get(position);
            LinearLayout row = new LinearLayout(Launcher.this);
            row.setOrientation(LinearLayout.HORIZONTAL);
            row.setGravity(Gravity.CENTER_VERTICAL);
            row.setPadding(dp(12), dp(8), dp(12), dp(8));
            row.setMinimumHeight(dp(64));
            ImageView icon = new ImageView(Launcher.this);
            Bitmap bitmap = BitmapFactory.decodeFile(new File(folder, "icon.png").getPath());
            if (bitmap != null) {
                icon.setImageBitmap(bitmap);
            }
            row.addView(icon, new LinearLayout.LayoutParams(dp(48), dp(48)));
            TextView text = new TextView(Launcher.this);
            text.setText(name(folder));
            text.setTextColor(Color.WHITE);
            text.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
            text.setPadding(dp(16), 0, 0, 0);
            row.addView(text);
            return row;
        }
    }

    /**
     * a game: the one held in the background brought back, another started
     * once that one is ended, after asking.
     */
    private void play(File folder) {
        int pid = gameProcess();
        String running = null;
        try {
            running = pid == 0 ? null : new String(readAll(new File(getFilesDir(), "playing")), StandardCharsets.UTF_8).trim();
        } catch (IOException ignored) {
            // a game process that never said which game, ended below
        }
        if (pid == 0) {
            start(folder, true);
        } else if (folder.getAbsolutePath().equals(running)) {
            start(folder, false);
        } else {
            String other = running == null ? "Another game" : name(new File(running));
            new AlertDialog.Builder(this)
                    .setMessage(other + " is still running. End it and play " + name(folder) + "? Anything not saved in it is lost.")
                    .setPositiveButton("End it", (dialog, which) -> {
                        android.os.Process.killProcess(pid);
                        playAgain(folder);
                    })
                    .setNegativeButton("Cancel", null)
                    .show();
        }
    }

    /** the game process's id, 0 when none runs. */
    private int gameProcess() {
        String process = getPackageName() + ":game";
        for (ActivityManager.RunningAppProcessInfo info : getSystemService(ActivityManager.class).getRunningAppProcesses()) {
            if (process.equals(info.processName)) {
                return info.pid;
            }
        }
        return 0;
    }

    /**
     * the game in a task of its own on the main display, wherever the
     * launcher was opened, its second screen going on the other one: Cocoon
     * opens apps on the Thor's bottom display. Fresh, any task left from a
     * game before is cleared, else the one held in the background comes back.
     */
    private void start(File folder, boolean fresh) {
        Intent intent = new Intent(this, Game.class);
        intent.putExtra("game", folder.getAbsolutePath());
        intent.putExtra("orientation", prefs().getString("orientation", "auto"));
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | (fresh ? Intent.FLAG_ACTIVITY_CLEAR_TASK : 0));
        ActivityOptions options = ActivityOptions.makeBasic();
        options.setLaunchDisplayId(Display.DEFAULT_DISPLAY);
        startActivity(intent, options.toBundle());
    }

    /**
     * a game started fresh once the game process is gone, for Reset in its
     * menu and for one ended to play another: one started first would go
     * into the old process as it ends.
     */
    private void playAgain(File folder) {
        new Thread(() -> {
            for (int tries = 0; tries < 40 && gameProcess() != 0; tries++) {
                try {
                    Thread.sleep(50);
                } catch (InterruptedException ignored) {
                    return;
                }
            }
            runOnUiThread(() -> start(folder, true));
        }).start();
    }

    // ---- adding a game

    private void addGame() {
        new AlertDialog.Builder(this)
                .setTitle("Add game")
                .setItems(new String[] {"A folder the kit built", "A pack file"}, (dialog, which) -> {
                    if (which == 0) {
                        startActivityForResult(new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE), ADD_FOLDER);
                    } else {
                        Intent pick = new Intent(Intent.ACTION_OPEN_DOCUMENT);
                        pick.addCategory(Intent.CATEGORY_OPENABLE);
                        pick.setType("*/*");
                        startActivityForResult(pick, ADD_PACK);
                    }
                })
                .show();
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (result != RESULT_OK || data == null || data.getData() == null) {
            return;
        }
        Uri uri = data.getData();
        if (request == ADD_FOLDER) {
            inBackground("Adding", progress -> withCleanUp(() -> added(importFolder(uri, progress))));
        } else if (request == ADD_PACK) {
            inBackground("Adding", progress -> withCleanUp(() -> added(importPack(uri, progress))));
        }
    }

    private interface Progress {
        void at(String what, long done, long total);
    }

    private interface Work {
        /** what to tell the person when it is done. */
        String run(Progress progress) throws IOException;
    }

    private static String added(int count) {
        return count == 0 ? "No game there: a game's folder holds game.3ds and title.so" : count == 1 ? "Added a game" : "Added " + count + " games";
    }

    /** runs work off the main thread with a progress bar, then says how it went. */
    private void inBackground(String title, Work work) {
        LinearLayout box = new LinearLayout(this);
        box.setOrientation(LinearLayout.VERTICAL);
        box.setPadding(dp(24), dp(16), dp(24), dp(16));
        TextView label = new TextView(this);
        label.setText("Copying...");
        ProgressBar bar = new ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal);
        bar.setMax(1000);
        box.addView(label);
        box.addView(bar);
        AlertDialog dialog = new AlertDialog.Builder(this).setTitle(title).setView(box).setCancelable(false).show();
        new Thread(() -> {
            String outcome;
            try {
                outcome = work.run((what, done, total) -> runOnUiThread(() -> {
                    label.setText(what);
                    bar.setProgress(total > 0 ? (int) (done * 1000 / total) : 0);
                }));
            } catch (IOException error) {
                outcome = "That did not work: " + error.getMessage();
            }
            String shown = outcome;
            runOnUiThread(() -> {
                dialog.dismiss();
                if (shown != null) {
                Toast.makeText(this, shown, Toast.LENGTH_LONG).show();
            }
                refresh();
            });
        }).start();
    }

    /** a document in a picked tree: its id, name, kind and size. */
    private static final class Document {
        String id;
        String name;
        boolean folder;
        long size;
    }

    private List<Document> children(Uri tree, String parent) {
        List<Document> found = new ArrayList<>();
        Uri uri = DocumentsContract.buildChildDocumentsUriUsingTree(tree, parent);
        String[] columns = {
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
            DocumentsContract.Document.COLUMN_SIZE
        };
        try (Cursor cursor = getContentResolver().query(uri, columns, null, null, null)) {
            while (cursor != null && cursor.moveToNext()) {
                Document document = new Document();
                document.id = cursor.getString(0);
                document.name = cursor.getString(1);
                document.folder = DocumentsContract.Document.MIME_TYPE_DIR.equals(cursor.getString(2));
                document.size = cursor.isNull(3) ? -1 : cursor.getLong(3);
                found.add(document);
            }
        }
        return found;
    }

    /** every game folder in a picked tree, a few levels down: the kit's whole output works too. */
    private int importFolder(Uri tree, Progress progress) throws IOException {
        List<List<Document>> found = new ArrayList<>();
        findGames(tree, DocumentsContract.getTreeDocumentId(tree), 0, found);
        for (List<Document> game : found) {
            Document rom = null, code = null, font = null;
            List<Document> mii = new ArrayList<>();
            for (Document document : game) {
                if ("game.3ds".equals(document.name)) {
                    rom = document;
                } else if ("title.so".equals(document.name)) {
                    code = document;
                } else if ("shared_font.bin".equals(document.name)) {
                    font = document;
                } else if (miiParts(document.name)) {
                    mii.add(document);
                }
            }
            File incoming = incoming();
            copy(open(tree, rom.id), new File(incoming, "game.3ds"), "game.3ds", rom.size, progress);
            copy(open(tree, code.id), new File(incoming, "title.so"), "title.so", code.size, progress);
            if (font != null) {
                copy(open(tree, font.id), new File(new File(incoming, "sysdata"), "shared_font.bin"), "system font", font.size, progress);
            }
            for (Document parts : mii) {
                copy(open(tree, parts.id), new File(new File(incoming, "sysdata"), parts.name), "Mii parts", parts.size, progress);
            }
            settle(incoming);
        }
        return found.size();
    }

    private void findGames(Uri tree, String parent, int depth, List<List<Document>> found) {
        List<Document> here = children(tree, parent);
        boolean rom = false, code = false;
        for (Document document : here) {
            rom |= "game.3ds".equals(document.name);
            code |= "title.so".equals(document.name);
        }
        if (rom && code) {
            // the system font is in sysdata beside them
            List<Document> files = new ArrayList<>(here);
            for (Document document : here) {
                if (document.folder && "sysdata".equals(document.name)) {
                    files.addAll(children(tree, document.id));
                }
            }
            found.add(files);
            return;
        }
        if (depth < 4) {
            for (Document document : here) {
                if (document.folder) {
                    findGames(tree, document.id, depth + 1, found);
                }
            }
        }
    }

    private InputStream open(Uri tree, String id) throws IOException {
        InputStream in = getContentResolver().openInputStream(DocumentsContract.buildDocumentUriUsingTree(tree, id));
        if (in == null) {
            throw new IOException("could not open it");
        }
        return in;
    }

    /**
     * a pack file: a zip of a game's folder as the kit builds it, game.3ds,
     * title.so and sysdata/shared_font.bin, in any folder inside it.
     */
    private int importPack(Uri uri, Progress progress) throws IOException {
        InputStream raw = getContentResolver().openInputStream(uri);
        if (raw == null) {
            throw new IOException("could not open it");
        }
        File incoming = incoming();
        boolean rom = false, code = false;
        try (ZipInputStream zip = new ZipInputStream(raw)) {
            ZipEntry entry;
            while ((entry = zip.getNextEntry()) != null) {
                String name = new File(entry.getName()).getName();
                if ("game.3ds".equals(name)) {
                    copy(zip, new File(incoming, name), name, entry.getSize(), progress);
                    rom = true;
                } else if ("title.so".equals(name)) {
                    copy(zip, new File(incoming, name), name, entry.getSize(), progress);
                    code = true;
                } else if ("shared_font.bin".equals(name)) {
                    copy(zip, new File(new File(incoming, "sysdata"), name), "system font", entry.getSize(), progress);
                } else if (miiParts(name)) {
                    copy(zip, new File(new File(incoming, "sysdata"), name), "Mii parts", entry.getSize(), progress);
                }
            }
        }
        if (!rom || !code) {
            deleteAll(incoming);
            return 0;
        }
        settle(incoming);
        return 1;
    }

    /** the Mii parts the kit puts beside the system font, experimental. */
    private static boolean miiParts(String name) {
        return "CFL_Res.dat".equals(name) || "CFL_Res.placeholder.dat".equals(name);
    }

    private interface Adding {
        String run() throws IOException;
    }

    /** adding a game, the folder it was coming into gone if it failed. */
    private String withCleanUp(Adding adding) throws IOException {
        try {
            return adding.run();
        } catch (IOException error) {
            for (File volume : volumes()) {
                deleteAll(new File(new File(volume, "games"), ".incoming"));
            }
            throw error;
        }
    }

    /** an empty folder to copy a game into, on the volume games go on. */
    private File incoming() throws IOException {
        List<File> volumes = volumes();
        if (volumes.isEmpty()) {
            throw new IOException("no storage to put it on");
        }
        File folder = new File(new File(volumes.get(0), "games"), ".incoming");
        deleteAll(folder);
        if (!folder.mkdirs()) {
            throw new IOException("could not make " + folder);
        }
        return folder;
    }

    /** an incoming game in its place: its folder named by its title id, its code read only in the app's own storage. */
    private void settle(File incoming) throws IOException {
        String id = titleId(new File(incoming, "game.3ds"));
        File code = codeFor(id);
        code.getParentFile().mkdirs();
        code.setWritable(true);
        code.delete();
        File copied = new File(incoming, "title.so");
        try (InputStream in = new FileInputStream(copied)) {
            copy(in, code, "title.so", copied.length(), null);
        }
        code.setReadOnly();
        copied.delete();
        banner(new File(incoming, "game.3ds"), incoming);
        File folder = new File(incoming.getParentFile(), id);
        if (folder.isDirectory()) {
            // a game added again keeps its saves and settings
            for (String name : new String[] {"game.3ds", "sysdata", "name.txt", "icon.png"}) {
                File old = new File(folder, name);
                deleteAll(old);
                File fresh = new File(incoming, name);
                if (fresh.exists() && !fresh.renameTo(old)) {
                    throw new IOException("could not move " + name);
                }
            }
            deleteAll(incoming);
        } else if (!incoming.renameTo(folder)) {
            throw new IOException("could not move it into place");
        }
    }

    private static void copy(InputStream in, File to, String what, long total, Progress progress) throws IOException {
        to.getParentFile().mkdirs();
        byte[] buffer = new byte[1 << 20];
        long done = 0, shown = 0;
        try (OutputStream out = new FileOutputStream(to)) {
            int read;
            while ((read = in.read(buffer)) > 0) {
                out.write(buffer, 0, read);
                done += read;
                if (progress != null && done - shown > (8 << 20)) {
                    progress.at("Copying " + what, done, total);
                    shown = done;
                }
            }
        }
        if (!(in instanceof ZipInputStream)) {
            in.close();
        }
    }

    private static void deleteAll(File file) {
        File[] inside = file.listFiles();
        if (inside != null) {
            for (File child : inside) {
                deleteAll(child);
            }
        }
        file.delete();
    }

    /** the title id in a decrypted .3ds's header, as 16 hex digits. */
    private static String titleId(File rom) throws IOException {
        try (RandomAccessFile in = new RandomAccessFile(rom, "r")) {
            byte[] header = new byte[0x110];
            in.readFully(header);
            if (header[0x100] != 'N' || header[0x101] != 'C' || header[0x102] != 'S' || header[0x103] != 'D') {
                throw new IOException("game.3ds is not a decrypted .3ds");
            }
            long id = 0;
            for (int i = 7; i >= 0; i--) {
                id = (id << 8) | (header[0x108 + i] & 0xFF);
            }
            return String.format("%016X", id);
        }
    }

    /** name.txt and icon.png from the game's banner, its SMDH, when it has one. */
    private static void banner(File rom, File folder) {
        try (RandomAccessFile in = new RandomAccessFile(rom, "r")) {
            long first = u32(in, 0x120) * 0x200;
            byte[] magic = new byte[4];
            in.seek(first + 0x100);
            in.readFully(magic);
            if (!"NCCH".equals(new String(magic, StandardCharsets.US_ASCII))) {
                return;
            }
            long exefs = first + u32(in, first + 0x1A0) * 0x200;
            for (int entry = 0; entry < 10; entry++) {
                byte[] name = new byte[8];
                in.seek(exefs + entry * 16);
                in.readFully(name);
                if (!new String(name, StandardCharsets.US_ASCII).startsWith("icon")) {
                    continue;
                }
                long offset = u32(in, exefs + entry * 16 + 8);
                byte[] smdh = new byte[0x36C0];
                in.seek(exefs + 0x200 + offset);
                in.readFully(smdh);
                if (smdh[0] != 'S' || smdh[1] != 'M' || smdh[2] != 'D' || smdh[3] != 'H') {
                    return;
                }
                for (int language : new int[] {1, 0}) {
                    int at = 0x8 + language * 0x200;
                    String title = new String(smdh, at, 0x80, StandardCharsets.UTF_16LE).split("\0")[0].trim().replaceAll("\\s+", " ");
                    if (!title.isEmpty()) {
                        try (OutputStream out = new FileOutputStream(new File(folder, "name.txt"))) {
                            out.write(title.getBytes(StandardCharsets.UTF_8));
                        }
                        break;
                    }
                }
                // the large icon: 48x48 RGB565, in 8x8 tiles, each in Morton order
                int[] pixels = new int[48 * 48];
                for (int index = 0; index < 48 * 48; index++) {
                    int tile = index / 64, inside = index % 64;
                    int x = (tile % 6) * 8, y = (tile / 6) * 8;
                    for (int bit = 0; bit < 3; bit++) {
                        x += ((inside >> (2 * bit)) & 1) << bit;
                        y += ((inside >> (2 * bit + 1)) & 1) << bit;
                    }
                    int value = (smdh[0x24C0 + index * 2] & 0xFF) | ((smdh[0x24C0 + index * 2 + 1] & 0xFF) << 8);
                    int r = (value >> 11) & 31, g = (value >> 5) & 63, b = value & 31;
                    pixels[y * 48 + x] = Color.rgb((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2));
                }
                Bitmap icon = Bitmap.createBitmap(pixels, 48, 48, Bitmap.Config.ARGB_8888);
                try (OutputStream out = new FileOutputStream(new File(folder, "icon.png"))) {
                    icon.compress(Bitmap.CompressFormat.PNG, 100, out);
                }
                return;
            }
        } catch (IOException ignored) {
        }
    }

    private static long u32(RandomAccessFile in, long at) throws IOException {
        in.seek(at);
        byte[] bytes = new byte[4];
        in.readFully(bytes);
        return (bytes[0] & 0xFFL) | ((bytes[1] & 0xFFL) << 8) | ((bytes[2] & 0xFFL) << 16) | ((bytes[3] & 0xFFL) << 24);
    }

    /** the profiles in the app, unpacked into its own storage once a version. */
    private void unpackProfiles() {
        File titles = new File(getFilesDir(), "titles");
        String version = version();
        if (version.equals(prefs().getString("profiles", "")) && titles.isDirectory()) {
            return;
        }
        deleteAll(titles);
        try {
            String[] ids = getAssets().list("titles");
            if (ids != null) {
                for (String id : ids) {
                    try (InputStream in = getAssets().open("titles/" + id + "/profile.toml")) {
                        copy(in, new File(new File(titles, id), "profile.toml"), id, -1, null);
                    } catch (IOException ignored) {
                        // a title with overrides and no profile
                    }
                }
            }
            prefs().edit().putString("profiles", version).apply();
        } catch (IOException ignored) {
        }
    }

    // ---- the device

    private SharedPreferences prefs() {
        return getSharedPreferences("launcher", Context.MODE_PRIVATE);
    }

    private File oneScreen() {
        return new File(new File(getFilesDir(), "config"), "one-screen");
    }

    /** the Device page in the game list's place, or the list back. */
    private void showDevice(boolean shown) {
        devicePage.setVisibility(shown ? View.VISIBLE : View.GONE);
        gamesPage.setVisibility(shown ? View.GONE : View.VISIBLE);
        deviceButton.setSelected(shown);
        if (!shown) {
            return;
        }
        LinearLayout page = new LinearLayout(this);
        page.setOrientation(LinearLayout.VERTICAL);
        page.setPadding(0, 0, 0, dp(16));
        String[] orientations = {"auto", "landscape", "portrait"};
        page.addView(choice("Orientation", "How games are held. Auto turns with the device.",
                new String[] {"Auto", "Landscape", "Portrait"},
                indexOf(orientations, prefs().getString("orientation", "auto")), true,
                which -> prefs().edit().putString("orientation", orientations[which]).apply()));
        page.addView(choice("Touch controls", "Buttons and a circle pad on the screen. They hide while a gamepad is used.",
                new String[] {"On", "Off"}, "false".equals(setting("touch_controls", "true")) ? 1 : 0, true,
                which -> putSetting("touch_controls", which == 0 ? "true" : "false")));
        String[] aspects = {"Original", "Integer", "FourThree", "SixteenTen", "SixteenNine", "Stretch"};
        page.addView(choice("Aspect ratio", "The screens' shape: as on the 3DS, scaled by whole pixels, the top one wider or narrower, or both stretched to fill.",
                new String[] {"Original", "Integer scale", "4:3", "16:10", "16:9", "Stretch"},
                indexOf(aspects, setting("aspect", "\"Original\"").replace("\"", "")), true,
                which -> putSetting("aspect", "\"" + aspects[which] + "\"")));
        String[] resolutions = {"Native", "1.25x", "1.5x", "1.75x", "2x", "2.25x", "2.5x", "2.75x", "3x", "3.25x", "3.5x"};
        int resolution = 0;
        try {
            resolution = (int) Math.round((Double.parseDouble(setting("resolution", "1.0")) - 1.0) * 4.0);
        } catch (NumberFormatException ignored) {
        }
        page.addView(choice("Resolution", "How sharp the 3D is drawn, times the 3DS's own. Higher takes more of the GPU.",
                resolutions, Math.max(0, Math.min(resolution, resolutions.length - 1)), true,
                which -> putSetting("resolution", String.valueOf(1.0 + which * 0.25))));
        page.addView(choice("Smooth scaling", "Blends the pixels as the screens are scaled to fit. Off is sharper, with uneven pixels.",
                new String[] {"On", "Off"}, "false".equals(setting("smooth", "true")) ? 1 : 0, true,
                which -> putSetting("smooth", which == 0 ? "true" : "false")));
        boolean second = getSystemService(DisplayManager.class).getDisplays(DisplayManager.DISPLAY_CATEGORY_PRESENTATION).length > 0;
        page.addView(choice("Screens", second ? "With a second display, the bottom screen can go on it by itself." : "For handhelds with a second display. This one has one.",
                new String[] {"Bottom on the second", "Both on this one"}, oneScreen().exists() ? 1 : 0, second,
                which -> {
                    File flag = oneScreen();
                    if (which == 0) {
                        flag.delete();
                    } else {
                        flag.getParentFile().mkdirs();
                        try {
                            new FileOutputStream(flag).close();
                        } catch (IOException ignored) {
                        }
                    }
                }));
        TextView note = new TextView(this);
        note.setText("Changes take effect the next time a game starts. More settings are in the menu in game, the Menu button or Back, where Edit touch controls moves and sizes them.\n\nPocket Native " + version());
        note.setTextColor(0xFF777777);
        note.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
        note.setPadding(dp(16), dp(20), dp(16), 0);
        page.addView(note);
        devicePage.removeAllViews();
        devicePage.addView(page);
    }

    private interface Pick {
        void picked(int which);
    }

    /**
     * a setting: its name, what it does, and a button for each choice, three
     * to a row, the one chosen lit.
     */
    private View choice(String title, String help, String[] labels, int chosen, boolean enabled, Pick pick) {
        LinearLayout section = new LinearLayout(this);
        section.setOrientation(LinearLayout.VERTICAL);
        section.setPadding(dp(16), dp(16), dp(16), 0);
        TextView name = new TextView(this);
        name.setText(title);
        name.setTextColor(Color.WHITE);
        name.setTextSize(TypedValue.COMPLEX_UNIT_SP, 17);
        section.addView(name);
        TextView about = new TextView(this);
        about.setText(help);
        about.setTextColor(0xFF999999);
        about.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
        section.addView(about);
        List<Button> buttons = new ArrayList<>();
        // as many to a row as fit at 110dp each, the rows even
        int fit = Math.max(2, (getResources().getConfiguration().screenWidthDp - 32) / 110);
        int rows = (labels.length + fit - 1) / fit;
        int across = (labels.length + rows - 1) / rows;
        LinearLayout row = null;
        for (int i = 0; i < labels.length; i++) {
            if (i % across == 0) {
                row = new LinearLayout(this);
                row.setPadding(0, dp(8), 0, 0);
                section.addView(row);
            }
            final int which = i;
            Button button = button(labels[i], v -> {
                for (Button other : buttons) {
                    other.setSelected(other == v);
                }
                pick.picked(which);
            });
            button.setSelected(i == chosen);
            button.setEnabled(enabled);
            button.setAlpha(enabled ? 1f : 0.4f);
            buttons.add(button);
            row.addView(button, weight());
        }
        return section;
    }

    private static int indexOf(String[] values, String value) {
        for (int i = 0; i < values.length; i++) {
            if (values[i].equals(value)) {
                return i;
            }
        }
        return 0;
    }

    /** the settings every game shares, which the menu in game sets too. */
    private File settingsFile() {
        return new File(new File(getFilesDir(), "config"), "settings.toml");
    }

    /** a setting's value as settings.toml has it, quotes and all. */
    private String setting(String key, String fallback) {
        try {
            for (String line : new String(readAll(settingsFile()), StandardCharsets.UTF_8).split("\n")) {
                String[] parts = line.split("=", 2);
                if (parts.length == 2 && parts[0].trim().equals(key)) {
                    return parts[1].trim();
                }
            }
        } catch (IOException ignored) {
        }
        return fallback;
    }

    /** a setting's value into settings.toml, at the top, where no table holds it. */
    private void putSetting(String key, String value) {
        File file = settingsFile();
        List<String> lines = new ArrayList<>();
        try {
            for (String line : new String(readAll(file), StandardCharsets.UTF_8).split("\n")) {
                String[] parts = line.split("=", 2);
                if (!line.isEmpty() && !(parts.length == 2 && parts[0].trim().equals(key))) {
                    lines.add(line);
                }
            }
        } catch (IOException ignored) {
            // none yet, the games' defaults hold
        }
        lines.add(0, key + " = " + value);
        file.getParentFile().mkdirs();
        try (FileOutputStream out = new FileOutputStream(file)) {
            out.write((String.join("\n", lines) + "\n").getBytes(StandardCharsets.UTF_8));
        } catch (IOException error) {
            Toast.makeText(this, "Could not save the setting: " + error.getMessage(), Toast.LENGTH_LONG).show();
        }
    }

    @Override
    public void onConfigurationChanged(Configuration changed) {
        super.onConfigurationChanged(changed);
        // turned: the choices laid out again for the new width
        if (devicePage.getVisibility() == View.VISIBLE) {
            showDevice(true);
        }
    }

    @Override
    public void onBackPressed() {
        if (devicePage.getVisibility() == View.VISIBLE) {
            showDevice(false);
        } else {
            super.onBackPressed();
        }
    }

    // ---- updating

    private String version() {
        try {
            return getPackageManager().getPackageInfo(getPackageName(), 0).versionName;
        } catch (Exception error) {
            return "0";
        }
    }

    /** whether release version a, as 1.2.3, is newer than b. */
    private static boolean newer(String a, String b) {
        String[] x = a.split("\\."), y = b.split("\\.");
        for (int i = 0; i < Math.max(x.length, y.length); i++) {
            int p = i < x.length ? parse(x[i]) : 0, q = i < y.length ? parse(y[i]) : 0;
            if (p != q) {
                return p > q;
            }
        }
        return false;
    }

    private static int parse(String number) {
        try {
            return Integer.parseInt(number.replaceAll("[^0-9]", ""));
        } catch (NumberFormatException error) {
            return 0;
        }
    }

    private void update() {
        Toast.makeText(this, "Looking for a new version...", Toast.LENGTH_SHORT).show();
        new Thread(() -> {
            String message = null;
            try {
                HttpURLConnection connection = (HttpURLConnection) new URL(RELEASES).openConnection();
                connection.setRequestProperty("Accept", "application/vnd.github+json");
                JSONObject release;
                try (InputStream in = connection.getInputStream()) {
                    java.io.ByteArrayOutputStream text = new java.io.ByteArrayOutputStream();
                    byte[] buffer = new byte[8192];
                    int read;
                    while ((read = in.read(buffer)) > 0) {
                        text.write(buffer, 0, read);
                    }
                    release = new JSONObject(text.toString("UTF-8"));
                }
                String latest = release.getString("tag_name").replaceFirst("^v", "");
                String apk = null;
                JSONArray assets = release.getJSONArray("assets");
                for (int i = 0; i < assets.length(); i++) {
                    JSONObject asset = assets.getJSONObject(i);
                    if (asset.getString("name").endsWith(".apk")) {
                        apk = asset.getString("browser_download_url");
                    }
                }
                if (!newer(latest, version())) {
                    message = "Pocket Native " + version() + " is the latest";
                } else if (apk == null) {
                    message = "Pocket Native " + latest + " is out, but has no Android app yet";
                } else {
                    String url = apk;
                    runOnUiThread(() -> new AlertDialog.Builder(this)
                            .setTitle("Pocket Native " + latest)
                            .setMessage("Download and install it? Your games and saves stay.")
                            .setPositiveButton("Update", (dialog, which) -> download(url))
                            .setNegativeButton("Not now", null)
                            .show());
                }
            } catch (Exception error) {
                message = "Could not reach GitHub: " + error.getMessage();
            }
            String shown = message;
            if (shown != null) {
                runOnUiThread(() -> Toast.makeText(this, shown, Toast.LENGTH_LONG).show());
            }
        }).start();
    }

    private void download(String url) {
        inBackground("Updating", progress -> {
            HttpURLConnection connection = (HttpURLConnection) new URL(url).openConnection();
            File apk = new File(getCacheDir(), "update.apk");
            try (InputStream in = connection.getInputStream()) {
                copy(in, apk, "the update", connection.getContentLengthLong(), progress);
            }
            PackageInstaller installer = getPackageManager().getPackageInstaller();
            int id = installer.createSession(new PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL));
            try (PackageInstaller.Session session = installer.openSession(id)) {
                try (OutputStream out = session.openWrite("pocket-native.apk", 0, apk.length()); InputStream in = new FileInputStream(apk)) {
                    byte[] buffer = new byte[1 << 20];
                    int read;
                    while ((read = in.read(buffer)) > 0) {
                        out.write(buffer, 0, read);
                    }
                    session.fsync(out);
                }
                Intent done = new Intent(this, Launcher.class).setAction("installed");
                PendingIntent pending = PendingIntent.getActivity(this, 0, done, PendingIntent.FLAG_MUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
                session.commit(pending.getIntentSender());
            }
            return null;
        });
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        playShortcut(intent);
        // the installer asks the person first, through the intent it sends back
        if ("installed".equals(intent.getAction())
                && intent.getIntExtra(PackageInstaller.EXTRA_STATUS, 0) == PackageInstaller.STATUS_PENDING_USER_ACTION) {
            Intent confirm = (Intent) intent.getParcelableExtra(Intent.EXTRA_INTENT);
            if (confirm != null) {
                startActivity(confirm);
            }
        }
    }
}
