# discommand

A small, vim-style terminal client for **Discord, WhatsApp, Matrix, Google Calendar, Microsoft To Do and Twos**, written in Rust.
It's inspired by [discordo](https://github.com/ayn2op/discordo) and runs natively on
**Windows (x64 and ARM64), Linux (x64 and ARM64) and macOS (Apple Silicon and Intel)**.
Press **Shift+Tab** to cycle Discord → WhatsApp → Matrix → Calendar → Tasks → Twos, and **right Shift+Tab** to go back.

What it does to save battery and memory:

- Redraws only when something changes. There's no render loop and no idle timers.
- Runs a single-threaded async runtime.
- Uses a zlib-compressed Discord gateway, so less radio time.
- Drops presence, typing and member-list events unread, before any JSON parsing.
- Doesn't subscribe to typing, activities or member lists.
- Never sends typing or online status to WhatsApp or Matrix, and sends no read receipts unless you turn them on
  (Matrix gets private receipts, which clear unread counts without telling anyone).
- Shows pictures as low-resolution half-block art (`▀`, two pixels per cell) inside the
  chat. Discord's media proxy is asked for an ~80 px copy, and WhatsApp uses the small
  thumbnail already inside each message, so a picture costs a few KB. Pictures only load
  for messages on screen and are decoded off the UI thread. Turn them off with `:images off`.
  Selecting a message (`j`/`k`) loads its pictures in high resolution, filling the
  pane: a 320 px copy from Discord, or the original image from WhatsApp.

Supported: Discord text channels and DMs; WhatsApp chats and groups; Matrix rooms
and DMs, end-to-end encrypted ones included (text, replies, edits, deletes,
reactions, media opened on demand). Voice, calls, forums,
threads and status updates aren't supported.

> **Heads-up:** Discord and WhatsApp forbid unofficial clients in their terms (Matrix
> is an open protocol and welcomes them).
> Discord logs in with your user token. WhatsApp links as a companion device through
> [whatsmeow](https://github.com/tulir/whatsmeow), the library behind mautrix-whatsapp.
> Normal personal use is generally fine, but accounts can be banned, more readily on
> WhatsApp. Use it at your own risk.

## Layout

| Program | What it is |
| --- | --- |
| `discommand` | The terminal UI and Discord client (Rust). |
| `discommand-wa` | The WhatsApp helper (Go, [`wa/`](wa/)). The UI starts it and talks to it with JSON lines over stdin/stdout. |

`discommand` looks for the helper next to its own executable, then on `PATH`.

## Install

discommand is built from source. That takes a few minutes the first time, and you
need **Rust**, **Go** (1.21 or newer) and **Git**. The install script builds both
programs and puts them into `~/.cargo/bin`, which the Rust installer already added
to your `PATH`.

> **One-step Google/Microsoft sign-in:** if you got a `keys.env` file from the
> maintainer, copy it into the `keys/` folder **before** running the install script
> (see [keys/README.md](keys/README.md)). Without it everything works too, but
> Calendar and To Do need a one-time setup of your own (explained in the app).

### Windows (x64 and ARM64)

1. Install the tools in PowerShell:

   ```powershell
   winget install Rustlang.Rustup GoLang.Go Git.Git
   winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
   ```

   The second line installs the C++ build tools Rust needs (about 2 GB). Then
   **close and reopen** the terminal.
2. Download and install:

   ```powershell
   git clone https://github.com/Kollerman/discommand.git
   cd discommand
   # copy keys.env into the keys folder now, if you have it
   powershell -ExecutionPolicy Bypass -File install.ps1
   ```

3. Start it with `discommand`.

Use **Windows Terminal** (preinstalled on Windows 11) for correct colors and pictures.

*Windows on ARM without Visual Studio:* instead of the build tools, unpack
[llvm-mingw](https://github.com/mstorsjo/llvm-mingw/releases) (`*-ucrt-aarch64.zip`)
to `%LOCALAPPDATA%\llvm-mingw` and run
`rustup toolchain install stable-aarch64-pc-windows-gnullvm`. `install.ps1` uses it
automatically.

### macOS (Apple Silicon and Intel)

1. Install the tools in Terminal:

   ```sh
   xcode-select --install                                          # compiler and git
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  # Rust
   ```

   Install Go with `brew install go` or the installer from [go.dev/dl](https://go.dev/dl).
   Then **open a new terminal**.
2. Download and install:

   ```sh
   git clone https://github.com/Kollerman/discommand.git
   cd discommand
   # copy keys.env into the keys folder now, if you have it
   sh install.sh
   ```

3. Start it with `discommand`.

The built-in Terminal app shows only 256 colors, so pictures look rough there.
[iTerm2](https://iterm2.com), [Ghostty](https://ghostty.org), [WezTerm](https://wezfurlong.org/wezterm/)
or [kitty](https://sw.kovidgoyal.net/kitty/) look better.

### Linux (x64 and ARM64, including Raspberry Pi)

1. Install the tools:

   ```sh
   # Debian, Ubuntu, Raspberry Pi OS
   sudo apt install build-essential pkg-config libssl-dev git golang
   # Fedora
   sudo dnf install gcc pkg-config openssl-devel git golang
   # Arch
   sudo pacman -S base-devel openssl git go

   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  # Rust
   ```

   Then **open a new terminal**. If your distribution's Go is older than 1.21, use
   the one from [go.dev/dl](https://go.dev/dl). Newer versions fetch the exact Go
   release they need by themselves.
2. Download and install:

   ```sh
   git clone https://github.com/Kollerman/discommand.git
   cd discommand
   # copy keys.env into the keys folder now, if you have it
   sh install.sh
   ```

3. Start it with `discommand`.

Opening links and files uses `xdg-open`. Without OpenSSL headers, `install.sh`
switches to a TLS stack written in Rust by itself.
*Termux (Android):* `pkg install rust golang git`, then the same steps (untested).

### Choosing integrations

The install script asks which integrations to build, one at a time (Enter = yes):

```text
  Discord [Y/n]: y
  WhatsApp (needs Go) [Y/n]: n
  Google Calendar [Y/n]:
  Microsoft To Do [Y/n]: n
```

Next time it offers to reuse that choice. To skip the questions, name the
integrations directly:

```sh
sh install.sh --with discord,calendar          # Linux, macOS
.\install.ps1 -With discord,calendar           # Windows
```

| Name | Integration |
| --- | --- |
| `discord` | Discord |
| `whatsapp` | WhatsApp (the only one that needs Go) |
| `matrix` | Matrix (the biggest: about 3–5 minutes of extra build time and a larger program) |
| `calendar` | Google Calendar |
| `todo` | Microsoft To Do |
| `twos` | Twos |
| `moodle` | Moodle (any university or school site) |

Integrations that aren't built in are left out of the program completely: no tab, no
login at startup, no background activity. `--with all` (or `-With all`) builds
everything. Without a terminal (for example in CI), the script doesn't ask and
builds the last choice, or everything. `discommand --version`
shows what a build contains. With plain cargo:
`cargo build --release --no-default-features --features native-tls,discord,calendar`.

### Updating and uninstalling

- **Update:** in the `discommand` folder run `git pull`, then the install script again.
- **Uninstall:** run `discommand --logout` and `:wa logout` first if you want to sign
  out. Then delete `discommand` and `discommand-wa` from `~/.cargo/bin`, and the
  `discommand` folder in your config directory (see below).

## Login

The chat services built in (Discord, WhatsApp, Matrix) need their account; you log in
on first start:

1. **Discord:** a QR code appears. In the Discord app on your phone, open Settings →
   **Scan QR Code**, scan it and confirm. There's no password, captcha or 2FA prompt;
   this is the same login as the desktop app's "Log in with QR Code". The token is
   saved encrypted (see [Security](#security)).
   As a fallback, start with `--discord-token` and paste a token (DevTools → Network →
   any `discord.com/api` request → `Authorization` header). The paste prompt also
   appears if the QR login fails. `$DISCORD_TOKEN` takes precedence if set.
2. **WhatsApp:** a QR code appears. On your phone go to WhatsApp → Settings →
   Linked devices → Link a device, and scan it. To link with a code instead, start
   with `discommand --wa-phone +491701234567`.
3. **Matrix:** enter your Matrix ID (`@you:matrix.org`). Servers with the new login
   (matrix.org and most others) open a sign-in page in the browser; older ones ask for
   the password (hidden). Then enter your **recovery key** (Element: Settings →
   Encryption) to read encrypted history: it verifies this device and fetches the
   message keys from the server's key backup. Enter alone skips it; then only new
   messages can be read.

The config dir is `%APPDATA%` on Windows, `~/Library/Application Support` on macOS
and `~/.config` on Linux. The WhatsApp
session and message history live in `<config dir>/discommand/whatsapp.db`; treat
that file like a password. WhatsApp only syncs recent history when a device is
linked. Older messages appear once the phone has sent them.

- `discommand --logout` or `:logout` removes the saved Discord token.
- `:wa logout` unlinks WhatsApp, `:mx logout` logs out of Matrix.
- `discommand --logout-a` logs out of **everything** (close discommand first):
  - **Discord:** logged out on Discord's side, so the old token stops working.
  - **WhatsApp:** unlinked, so it leaves your phone's *Linked devices*.
  - **Matrix:** logged out on the server, so the session disappears from your devices list.
  - **Google:** access is revoked.
  - **Microsoft and Twos:** the saved logins are deleted.
  - **Local data:** the WhatsApp chat history, the Matrix store (keys, room state)
    and cached pictures and downloads are deleted too.

  You can also revoke the Twos key under Twos → Settings → API Keys, and the
  Microsoft access at account.microsoft.com → Privacy.

## Security

- **Discord token:** on Windows it's encrypted with DPAPI (`token.dpapi`), which ties
  it to your Windows account; copied to another machine or user, it can't be
  decrypted. Plain-text tokens from older versions are migrated and wiped. On Linux,
  macOS and Termux it's a file only you can read (mode 600, folder 700).
- **WhatsApp session and media:** on Windows Pro/Enterprise the data folder
  (`%APPDATA%\discommand`) and the media cache are encrypted with EFS, transparently
  and tied to your account. On Windows Home this step is skipped.
- **Terminal safety:** control and bidirectional-override characters are removed from
  everything drawn, so names or messages can't send escape sequences to your terminal.
- **Opening things:** links open only if they're http(s). Downloaded files open
  directly only for media, PDF and text types. Anything else (`.exe`, `.bat`, `.lnk`,
  scripts, …) is shown selected in Explorer or Finder (or its folder is opened on Linux) instead of
being run. Downloaded file names
  are sanitized and downloads are capped at 256 MB.
- **Cleanup:** at every start, cached files older than two weeks are deleted (Matrix
  downloads and the Matrix picture cache too). WhatsApp downloads go even sooner,
  after a day.
- **Privacy:** pictures from link previews load only through Discord's proxy, never
  directly from third-party sites, and picture downloads never carry your Discord token.

## Keys

The app is modal like vim. Press `?` inside it for this list.

| Mode | Keys |
| --- | --- |
| anywhere (normal) | `i`/`a` insert · **`A` mark every chat of this service read** · `:` command · `/` or `Ctrl-k` find chat · `?` help · **`Shift-Tab` / `gt` next service, right `Shift-Tab` / `gT` previous, `2gt` the 2nd** (right Shift is detected on Windows only) · `Tab`, `Ctrl-w w`, `Ctrl-h`/`Ctrl-l` switch pane |
| sidebar | `j k` · `gg G` · counts (`5j`) · `l`/`o`/`Enter` open/expand · `h` collapse/parent · `za zo zc zM` |
| sending files | **`F`** opens the file dialog; the file waits above the input (`📎 name`), you can type a caption, `Enter` sends it, `Esc` removes it. `:attach <path>` does the same without the dialog (SSH, Termux). Discord, WhatsApp (photo/video/audio/document by file type, up to 100 MB) and Matrix (encrypted in encrypted rooms). |
| messages | `j k Ctrl-u Ctrl-d` select · `gg` top + load history · `G`/`Esc` newest · `r`/`R` reply (ping/no ping) · `+` react (then `1`–`5`: ❤️ 👍 😂 💀 😅; the same again removes it) · `e` edit · `dd` delete · `o`/`gx` open link, attachment or WhatsApp media |
| insert | `Enter` send · `Alt-Enter` newline · `↑` on empty input edits your last message · `Esc` normal · `Shift-Tab` switch service |
| commands | `:b <name>` open chat · `:r` reload the open chat / sync the calendar · `:read` the same as `A` · `:read all` every chat in Discord, WhatsApp and Matrix · `:mute [15m\|1h\|8h\|24h\|7d]` / `:unmute` the selected Discord server, category, channel or DM · `:images on\|off` · `:wa receipts on\|off` · `:wa logout` · `:mx receipts on\|off` · `:mx logout` · `:q` quit · `:logout` |

Each service keeps its own open chat, sidebar position and unsent draft. The
statusline shows which service is active, and how many unread chats or mentions
are waiting in the other one.

**Muted Discord servers, categories, channels and DMs** (muted in Discord or with
`:mute`) are dimmed and get no unread dot; mentions still show and ring, as in Discord.
Timed mutes end by themselves.

**WhatsApp read receipts are off by default.** Opening a chat marks it read on
this device only; your contacts don't see blue ticks. `:wa receipts on` turns them
on and is remembered.

**German keyboards (QWERTZ):** `:`, `/` and `?` need Shift on German keyboards, so
each also has an unshifted key: `ö` opens the command line (it sits where `;` is on US
keyboards), `-` or `#` opens the finder, and `ß` shows help. AltGr characters
(`@ { } [ ] \ | ~`) and umlauts type normally in insert mode and never trigger Ctrl
shortcuts.

When the terminal is narrower than 70 columns (phones), only one pane is shown at a time.

## Matrix

Matrix is the third mode (`3gt`) and works like WhatsApp: rooms and DMs on the left,
most recently active first (`#` rooms, `@` DMs), with the same keys for replying,
editing, deleting, reacting and opening media. Encryption is handled by
[matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk), the library behind
Element X. Keys, room state and the session are stored in an encrypted SQLite
database in `<config dir>/discommand/matrix/`, whose passphrase is kept with the other
secrets.

- Pictures: the server scales unencrypted ones; encrypted ones use the thumbnail the
  sender attached.
- Read receipts are private by default; `:mx receipts on` makes them public.
- Spaces aren't shown as a tree; their rooms appear in the list. Invitations have to
  be accepted in another app for now.
- A message that can't be decrypted shows 🔒. Its key usually arrives from the key
  backup; `:r` reloads the room.

## Calendar

Google Calendar is optional. Samsung Calendar has no API, but it can show your
Google calendars: add your Google account in Samsung Calendar, and events in a
Google calendar appear on the phone and here.

**Connect.** In a release built with a Google app ID (see
[Publishing a release](#publishing-a-release)), type `:cal login` and sign in in the
browser; that's all. Otherwise set up your own app once (about 10 minutes).
Google requires every app to have its own
OAuth client, so you create one for yourself:

1. Open [console.cloud.google.com](https://console.cloud.google.com) and create a project.
2. *APIs & Services → Library*: enable **Google Calendar API**.
3. *APIs & Services → OAuth consent screen*: user type *External*, fill in an app
   name and your e-mail, then under *Audience* click **Publish app**. Otherwise Google
   signs you out every 7 days.
4. *Credentials → Create credentials → OAuth client ID*, type **Desktop app**, then
   **Download JSON**.
5. In discommand, run `:cal setup C:\path\to\client_secret_….json`. Your browser
   opens: sign in and allow access. Google warns that the app isn't verified; it's
   your own app, so choose *Continue*.

The login (client and refresh token) is stored encrypted like the Discord token.
`:cal logout` removes it, and `:cal login` signs in again.

**Use.** Like Samsung Calendar, the default view is a **time grid**: hours down the
left (30-minute rows), days as columns, events as blocks in their calendar's colour,
side by side where they overlap. A red line marks the current time, all of a day's
all-day events are listed under its date (up to a quarter of the screen, then
"+N more"), and past events are darker. `v` cycles **week → 3 days →
day → agenda** (a list grouped by day). On narrow screens fewer days fit.

| Key | Action |
| --- | --- |
| `h` `j` `k` `l` | move the cursor a day / 30 minutes (counts work: `4j` = 2 h) |
| `H` / `L` | previous / next page (week, 3 days, day) |
| `w` / `b` | select the next / previous event (one at a time, including all-day ones) |
| `Tab` | next event at the cursor: simultaneous events and the day's all-day events |
| `t` | now |
| `n` / `i` | new event at the cursor (date and time are filled in, type the title) |
| `e` | edit the event under the cursor |
| `dd` | delete it |
| `Enter` | details (description, reminders, …) |
| `o` | open it in Google Calendar (browser) |
| `r` or `:r` | sync now |
| `v` | change view |

The line under the grid shows the event under the cursor.

Type new events in one line, in English or German:

```text
morgen 14:00 Zahnarzt @ Praxis Dr. Weber
fri 9:30-11 standup
28.10. Geburtstag Ana            (no time: all-day event)
2026-11-03 16uhr Team 2 sync
```

Times look like `14:00`, `9:30-11`, `16uhr` or `8h`; bare numbers stay in the title.
Dates can be `heute`/`today`, `morgen`/`tomorrow`, `übermorgen`, weekday names,
`28.10.`, `28.10.2026` or `2026-10-28`. `@` starts the place. Without a recognisable
date or time, the text goes to Google's own parser ("Lunch with Ana tomorrow 1pm").
New events go to your primary calendar.

**Reminders.** When an event's popup reminder from Google comes due, the
status line shows it. The next event within 12 hours is always in
the status line. The calendar syncs every 10 minutes. Between syncs, the app sleeps
until the next reminder or event change, with no polling.

## Tasks (Microsoft To Do)

Microsoft To Do is optional. It's the fifth mode (`5gt`): your lists on the left
with their open tasks, the selected task's details on the right. Overdue tasks are
red, tasks due today yellow, important ones bold. Tasks with a due date also appear in
the calendar's all-day row (`☐ …`, in To Do blue), and the status line shows how many
are due today or overdue in every mode.

**Connect.** In a release built with a Microsoft app ID, type `:todo login` and enter
the code shown at microsoft.com/devicelogin. Otherwise set up your own app once
(about 5 minutes). Microsoft also requires your own app
registration, but it needs no secret and no publishing:

1. [portal.azure.com](https://portal.azure.com) → *App registrations* → *New registration*.
   Supported account types: **Accounts in any organizational directory and personal
   Microsoft accounts**. No redirect URI.
2. In the new app: *Authentication* → **Allow public client flows: Yes** → Save.
3. Copy the **Application (client) ID** from *Overview*.
4. In discommand, run `:todo setup <that id>`. A sign-in page opens and the Tasks pane
   shows a code to enter there. This also works over SSH or in Termux, since the
   browser can be on any device.

If Azure asks you to create a directory/tenant first, a free Azure account works.
The login is stored encrypted like the other tokens. `:todo logout` removes it, and
`:todo login` signs in again.

| Key | Action |
| --- | --- |
| `n` | new task in the selected list |
| `x` / `Space` | done / not done (done tasks stay, struck through, until the next sync, so `x` undoes a slip) |
| `e` / `dd` | edit / delete |
| `h` / `l` | fold / unfold a list |
| `r` or `:r` | sync now |
| `x` in the calendar | tick off a task shown there |

New tasks use the calendar's one-line syntax: `morgen 14:00 Arzt anrufen` is due
tomorrow with a reminder at 14:00; `28.10. Steuer` is due on that date; `Milch kaufen`
has no date. Reminders show in the status line.

## Twos

[Twos](https://www.twosapp.com) is the sixth mode (`6gt`) and follows the app's day model.
The **sidebar** holds the quick to-dos:

- **Today** with its to-dos, also finished ones (struck through).
- **Rollover:** unfinished to-dos from earlier days roll over and sit on top of Today,
  marked with the day they came from (`↪ Tue 27 Oct`). Notes don't roll over.
- **Upcoming days** (Tomorrow, …) with open to-dos, then your favorite lists and the
  lists with open to-dos (folded; `l` unfolds), and every other list under
  **More lists**. `/` finds lists too.

The **main pane** shows the selected list whole, centered like the web app and in its
order: notes, bullets, numbered lines, headings, quotes, indented lines, dividers
(`---`) and to-dos with their reminders. `Tab` (or `Ctrl+l`) moves into it: `j`/`k`
select any line, a selected line shows its note, `Enter` opens a sub-list or a link,
and `h` goes back to the sidebar.

The keys are the same as in Tasks, in either pane: `x` ticks a to-do off, and `n` adds
one to the selected list (Today if nothing else is selected). `e` edits any line and
`dd` deletes it. A date or time typed with a to-do (`morgen 14:00 Arzt anrufen`) becomes
its Twos reminder, since Twos has no separate due date. Reminders show in the calendar
(in orange) and in the status line.

Day lists are recognised by their title: today's title shows the date format your
account uses.

**Connect.** In Twos (app or twosapp.com) open *Settings → API Keys*, create a key with
the scopes `read:lists`, `read:things` and `write:things`, and run
`:twos login <key>` in discommand. The key is stored encrypted like the other logins.
`:twos logout` removes it, and revoking the key in Twos cuts access at once. No app
registration is needed, so nothing goes into `keys/keys.env`.

In the sidebar, a to-do outside Today whose reminder has passed shows without a date
(Twos only lists *upcoming* reminders); its list in the main pane shows it. Twos allows
1000 API requests an hour: the first sync reads all your lists (a few seconds), later
ones take a few requests every 10 minutes, and each list shown is read once per sync.

## Moodle

Moodle is the last mode and works with any Moodle site. Connect it once from a
terminal:

```sh
discommand --moodle-login
```

It asks for the site (e.g. `elearning.uni-regensburg.de`), your user name and your
password (hidden).

- **Sidebar:** **Deadlines** first (assignments, quizzes and the like from the last
  two weeks on, overdue ones in red), then **Notifications** (the bell) and
  **Messages** with their unread counts, **Grades**, and your courses. GRIPS users get
  their own "Meine Kurse" folders; other sites group courses by course category.
  Folders with running courses start open.
- **Main pane:** a course opens as a page of its sections and activities.
- **Enter on an activity opens it here,** as a page of its content. Forums, folders,
  assignments and pages show their text and links. A link to another Moodle page
  (e.g. a forum discussion) opens the next page, a file (PDF, slides …) downloads and
  opens, and an outside link goes to the browser.
  Subsections (groups inside a section) appear nested under their section.
  Forums list their discussions (newest first, pinned ones bold); a discussion shows
  its posts with author, date and attachments.
  Tables in general read one row per line.

**Assignments** open in their own view: the dates (opened, due, last chance) and the
time left on top, the description with its files, then what you can do, the
submission status, your submitted files (Enter downloads them) and the feedback
(grade, comments, files).

- **Upload:** `F` (or Enter on 📤) opens the file dialog. discommand first checks the
  file against the assignment's rules (size per file, number of files, allowed types)
  and refuses it with the reason if it doesn't fit. Then a window shows the
  assignment, its due date and time left, the file and its size, the files already
  in your submission, whether a file of the same name gets replaced, and what
  happens next. Only `y` uploads; any other key cancels. Afterwards discommand
  reads the assignment again and checks that the file is listed, then shows the new
  status.
- **Hand in:** if the course wants submissions handed in separately, Enter on ✅
  shows the files that will be handed in and the course's declaration in full;
  `y` hands in (and agrees to it). discommand checks the result again.
- Other parts of the submission form (e.g. online text) are sent back unchanged.
  Removing a file, and assignments that need online text, still go through the
  browser (`o`). Messages and notifications can be read, not answered.
  Links that would change something on the site (subscribe, mark, log out …) are
  shown as text and can't be followed. `o` always opens the selected thing
  in the browser (e.g. to take a quiz or hand something in), and `h` goes back.
- **Calendar:** deadlines also show there, with a reminder a day ahead.
- **Read-only:** `x` ticks off activities that are completed by hand. Handing in and
  the like stays in Moodle.

**How it signs in** depends on the site:

- **The site allows the official Moodle app:** discommand uses the same access. The
  password is used once and only a key is kept.
- **Moodle has its own login form:** discommand signs in like a browser and uses the
  same interface Moodle's pages use. The password is stored encrypted, to sign in
  again when the session ends.
- **The university's own login page (single sign-on, as on GRIPS in Regensburg):**
  discommand goes through that login like a browser. It passes the forwarding
  pages, puts your user name and password into the university's login form, and
  follows the redirects back to Moodle. The password is stored encrypted, and
  discommand signs in again by itself whenever Moodle's session ends. If a site
  asks for a one-time code (two-factor) or has an unusual login page,
  `--moodle-login` falls back to logging in in the browser: you copy its
  `MoodleSession` cookie once (F12 → Application/Storage → Cookies), and
  discommand keeps that session alive while it runs.

`:moodle logout` removes the login, and `:moodle refresh` (or `r`)
syncs now; otherwise it syncs every 30 minutes.

## Publishing a release

To let other people sign in to Google and Microsoft with nothing but their account,
register the two apps **once** as the publisher and build their IDs into the program.
Users then never see a developer console.

1. **Google:** create a Cloud project, enable the Calendar API, set up the OAuth
   consent screen (External) and create an OAuth client of type *Desktop app*.
   Calendar access is a *sensitive* scope. Until Google has verified the app (privacy
   policy, a homepage on your own domain, a short demo video, usually a few weeks), at
   most 100 users can sign in and they see an "unverified app" warning.
2. **Microsoft:** register an app for "Accounts in any organizational directory and
   personal Microsoft accounts" and set *Allow public client flows* to Yes. There's no
   secret and no review needed. Optional *publisher verification* (free Microsoft
   partner account) removes the "unverified" label.
3. Put the IDs in the master file: copy `keys/keys.env.example` to `keys/keys.env`
   and fill it in. `keys/keys.env` is in `.gitignore`, so it never lands on GitHub.
   Give that one file to anyone who builds discommand. They drop it into `keys/`
   and run the installer (or `cargo build --release`). The IDs are then built into
   the program. Without the file the build still works, just without one-step
   sign-in.

   Environment variables with the same names (`DISCOMMAND_GOOGLE_CLIENT_ID`,
   `DISCOMMAND_GOOGLE_CLIENT_SECRET`, `DISCOMMAND_MS_CLIENT_ID`) override the file,
   both when building (for example from CI secrets) and at runtime. For installed
   apps, Google doesn't treat the client secret as confidential, so shipping it in
   the binary is the documented approach. Still, keep the file out of public places.

Discord and WhatsApp need nothing from the publisher: both log in by QR code.

### Announcing updates

discommand checks GitHub at startup and then once a day for a newer **release**. If
there is one, the status line says so, along with what kind of update it is (bugfixes, new
features or a new full release, see below), and keeps a `⬆ v0.2.0` hint. The repository
has to be public for this; while it's private, the check finds nothing and stays
silent.

Version numbers are `major.feature.fix`:

| Raise | When | Example |
| --- | --- | --- |
| `0.0.x` | a bugfix | `0.2.0` → `0.2.1` |
| `0.x.0` | a new feature (the last number goes back to 0) | `0.2.1` → `0.3.0` |
| `x.0.0` | a new full release (both others go back to 0) | `0.3.0` → `1.0.0` |

To publish an update:

1. Raise `version` in `Cargo.toml` (e.g. `0.2.1`), commit and push.
2. On GitHub: *Releases → Draft a new release*, tag `v0.2.1` (the same number, with
   a `v` in front and no dot after it), publish.

Plain pushes don't trigger the notice; only releases do. The check is one small
request to `api.github.com` (which sees the user's IP address, like any visit to
GitHub). Users can turn it off by setting `DISCOMMAND_NO_UPDATE_CHECK=1`.

## Adding an integration

Each integration is a Cargo feature and a tab:

1. Add a feature to `[features]` in `Cargo.toml`, and to `default`.
2. Add a variant to `Service` in `src/app.rs` and an entry to `Service::ALL`, marked
   `#[cfg(feature = "…")]`. Tab order, Shift+Tab and `gt` follow from that list.
3. Only start or connect it when `Service::YourService.built()` is true.
   A task app only needs a `todo::Backend` variant (see `src/twos.rs`); the task
   tab, calendar entries and reminders come with it.
4. Add the name to `all=` in `install.sh`, to the table above and to the
   integration loop in `.github/workflows/build.yml`.
Remember that both services' terms forbid unofficial clients, and say so to your users.

## Troubleshooting

- `DISCOMMAND_WA_LOG=path` makes the WhatsApp helper write a debug log.
- `DISCOMMAND_MOODLE_LOG=path` logs each step of a university (single sign-on) Moodle
  login: addresses and form field names, never values or passwords.
- "client version rejected": update whatsmeow (`cd wa && go get go.mau.fi/whatsmeow@latest`)
  and re-run the install script.
