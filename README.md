# discommand

A small, vim-style terminal client for **Discord, WhatsApp, Google Calendar and Microsoft To Do**, written in Rust.
It's inspired by [discordo](https://github.com/ayn2op/discordo) and builds as native
binaries for ARM (Windows on ARM, Linux aarch64/armv7, Raspberry Pi, Termux) and x86.
Press **Shift+Tab** to cycle Discord → WhatsApp → Calendar → Tasks, and **right Shift+Tab** to go back.

What it does to save battery and memory:

- Redraws only when something changes. There's no render loop and no idle timers.
- Runs a single-threaded async runtime.
- Uses a zlib-compressed Discord gateway, so less radio time.
- Drops presence, typing and member-list events unread, before any JSON parsing.
- Doesn't subscribe to typing, activities or member lists.
- Never sends typing or online status to WhatsApp, and sends no WhatsApp read receipts unless you turn them on.
- Shows pictures as low-resolution half-block art (`▀`, two pixels per cell) inside the
  chat. Discord's media proxy is asked for an ~80 px copy, and WhatsApp uses the small
  thumbnail already inside each message, so a picture costs a few KB. Pictures only load
  for messages on screen and are decoded off the UI thread. Turn them off with `:images off`.
  Selecting a message (`j`/`k`) loads its pictures in high resolution, filling the
  pane: a 320 px copy from Discord, or the original image from WhatsApp.

Supported: Discord text channels and DMs; WhatsApp chats and groups (text, replies,
edits, deletes, reactions shown, media opened on demand). Voice, calls, forums,
threads and status updates aren't supported.

> **Heads-up:** both services forbid unofficial clients in their terms.
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

## Build and install

You need Rust and Go. The install scripts build both programs into `~/.cargo/bin`;
re-run them to update.

```sh
./install.ps1    # Windows
./install.sh     # Linux, macOS, Termux
```

- **Windows on ARM without Visual Studio:** `install.ps1` uses
  [llvm-mingw](https://github.com/mstorsjo/llvm-mingw/releases) (`*-ucrt-aarch64.zip`)
  automatically when it's unpacked to `%LOCALAPPDATA%\llvm-mingw`. You also need
  `rustup toolchain install stable-aarch64-pc-windows-gnullvm`. With *Visual Studio
  Build Tools* (C++ workload, ARM64 tools) installed, plain `cargo` works instead.
- **Linux ARM / Raspberry Pi:** the default TLS needs `libssl-dev`. `install.sh`
  switches to pure-Rust TLS when OpenSSL isn't found.
- **Termux:** `pkg install rust golang`, then `./install.sh` (untested).
- The helper always builds with `CGO_ENABLED=0`, using SQLite written in pure Go,
  so no C compiler is needed.

## Login

Both accounts are required. On first start:

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

The config dir is `%APPDATA%` on Windows and `~/.config` on Linux. The WhatsApp
session and message history live in `<config dir>/discommand/whatsapp.db`; treat
that file like a password. WhatsApp only syncs recent history when a device is
linked. Older messages appear once the phone has sent them.

- `discommand --logout` or `:logout` removes the saved Discord token.
- `:wa logout` unlinks WhatsApp.

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
  scripts, …) is shown selected in Explorer instead of being run. Downloaded file names
  are sanitized, downloads are capped at 256 MB, and downloads older than a day are
  deleted.
- **Privacy:** pictures from link previews load only through Discord's proxy, never
  directly from third-party sites, and picture downloads never carry your Discord token.

## Keys

The app is modal like vim. Press `?` inside it for this list.

| Mode | Keys |
| --- | --- |
| anywhere (normal) | `i`/`a` insert · `:` command · `/` or `Ctrl-k` find chat · `?` help · **`Shift-Tab` / `gt` next service, right `Shift-Tab` / `gT` previous, `2gt` the 2nd** (right Shift is detected on Windows only) · `Tab`, `Ctrl-w w`, `Ctrl-h`/`Ctrl-l` switch pane |
| sidebar | `j k` · `gg G` · counts (`5j`) · `l`/`o`/`Enter` open/expand · `h` collapse/parent · `za zo zc zM` |
| messages | `j k Ctrl-u Ctrl-d` select · `gg` top + load history · `G`/`Esc` newest · `r`/`R` reply (ping/no ping) · `+` react (then `1`–`5`: ❤️ 👍 😂 💀 😅; the same again removes it) · `e` edit · `dd` delete · `o`/`gx` open link, attachment or WhatsApp media |
| insert | `Enter` send · `Alt-Enter` newline · `↑` on empty input edits your last message · `Esc` normal · `Shift-Tab` switch service |
| commands | `:b <name>` open chat · `:r` reload the open chat / sync the calendar · `:read` mark Discord read · `:images on\|off` · `:wa receipts on\|off` · `:wa logout` · `:q` quit · `:logout` |

Each service keeps its own open chat, sidebar position and unsent draft. The
statusline shows which service is active, and how many unread chats or mentions
are waiting in the other one.

**WhatsApp read receipts are off by default.** Opening a chat marks it read on
this device only; your contacts don't see blue ticks. `:wa receipts on` turns them
on and is remembered.

**German keyboards (QWERTZ):** `:`, `/` and `?` need Shift on German keyboards, so
each also has an unshifted key: `ö` opens the command line (it sits where `;` is on US
keyboards), `-` or `#` opens the finder, and `ß` shows help. AltGr characters
(`@ { } [ ] \ | ~`) and umlauts type normally in insert mode and never trigger Ctrl
shortcuts.

When the terminal is narrower than 70 columns (phones), only one pane is shown at a time.

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

**Reminders.** When an event's popup reminder from Google comes due, the terminal
bell rings and the status line shows it. The next event within 12 hours is always in
the status line. The calendar syncs every 10 minutes. Between syncs, the app sleeps
until the next reminder or event change, with no polling.

## Tasks (Microsoft To Do)

Microsoft To Do is optional. It's the fourth mode (`4gt`): your lists on the left
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
has no date. Reminders ring the bell and show in the status line.

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
Remember that both services' terms forbid unofficial clients, and say so to your users.

## Troubleshooting

- `DISCOMMAND_WA_LOG=path` makes the WhatsApp helper write a debug log.
- "client version rejected": update whatsmeow (`cd wa && go get go.mau.fi/whatsmeow@latest`)
  and re-run the install script.
