---
layout: ../../layouts/Doc.astro
title: Managing your apps
description: Point Alchemy at your Docker Compose stacks, run them from the web UI or the CLI, and keep using docker compose by hand.
---

# Managing your apps

Alchemy runs your own Docker Compose stacks. It calls each one an **app**.
An app is a plain folder with a compose file in it, and it stays that way:
Alchemy reads the folder and runs `docker compose` in it, and you can keep
running `docker compose` in it yourself, with or without Alchemy.

This page assumes `alchemyd` is running on the machine and Docker is
installed (Docker Desktop, or Docker Engine with the Compose v2 plugin).
Every command below is also in `alchemy --help`.

## Opening the web UI

The desktop app opens signed in. In a browser, run this in a terminal on
the same machine:

```sh
alchemy open
```

It opens Alchemy at `http://127.0.0.1:7240` in your default browser,
signed in. Only you can do this: other people and programs on the machine
can't use the web UI or its API without signing in. The link works once,
within a minute, and the browser stays signed in until alchemyd stops. If
the web UI says **You're signed out of alchemyd**, run `alchemy open`
again.

If no browser opens, or it can't open the page (some sandboxed browsers,
such as Ubuntu's Firefox snap, can't), run `alchemy open --print` and
paste the link it prints into your browser.

## App folders

Alchemy looks for apps in **app folders**:

- **The home**, `~/Alchemy/apps` unless you choose another. It is searched
  first, and it is where new apps will go. It doesn't have to exist yet:
  Alchemy only looks, and never creates it just by looking.
- **Other folders**, such as an existing `/opt/stacks`, searched after the
  home in the order you list them.

Change them on the **Settings** page under **App folders**, or with the CLI:

```sh
alchemy settings show
alchemy settings set app-home /srv/apps        # a different home
alchemy settings set app-home default          # back to ~/Alchemy/apps
alchemy settings set app-folders /opt/stacks /srv/compose
alchemy settings set app-folders               # search only the home
```

Folders must be full paths. `app-folders` replaces the whole list. An app
folder may itself be a symlink, since you chose it.

## How apps are found

Each folder directly inside an app folder that holds a compose file is an
app, named after the folder:

```text
~/Alchemy/apps/
├── blog/
│   ├── compose.yaml
│   └── .env
├── photos/
│   └── docker-compose.yml
└── notes.txt            ignored: not a folder
```

This gives two apps, `blog` and `photos`.

- **The compose file** is the first of `compose.yaml`, `compose.yml`,
  `docker-compose.yaml`, and `docker-compose.yml` that exists, the same one
  `docker compose` would pick. An override file such as
  `compose.override.yaml` and the folder's `.env` are used as Compose uses
  them.
- **The name** is the folder's name. It must be lowercase letters, numbers,
  `-`, and `_`, start with a letter or number, and be at most 63 characters.

Alchemy checks the folders every few seconds, so a new folder shows up on
its own. It asks Compose to read an app again when its compose file, an
override, or `.env` changes.

### What gets skipped, and why

A folder that looks like an app but can't be one is **skipped** and listed
with the reason, on the Apps page and in `alchemy app list`:

| Reason                                     | Why                                                                                                                                                            |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| The name isn't a valid app name            | The folder name is the app's name in the UI, the CLI, and the API. Rename the folder to use it (see [Adopting existing stacks](#adopting-existing-stacks)).    |
| Another app with this name is already used | Two app folders hold a folder of the same name. The first app folder in the list wins.                                                                         |
| The folder is a symlink                    | Alchemy only uses folders that are really inside an app folder. Add the folder it points to as an app folder instead.                                          |
| The compose file is a symlink              | So an app can't point its compose file at files elsewhere. Alchemy doesn't fall back to the next file name either, so it always reads what Compose would read. |
| The compose file isn't a regular file      | For example, `compose.yaml` is a folder.                                                                                                                       |

Hidden folders (starting with `.`), plain files, and folders without a
compose file are ignored without a report. A missing home is normal and
isn't reported as a problem; a missing or unreadable other folder is.

### Status

An app is **Running** when every service has a running container, **Partly
running** when some do, and **Stopped** when none do. It is **Status
unknown** while Docker isn't running or Compose can't read the app. The app
page then says why, in Compose's own words when the compose file has a
mistake.

### Projects Alchemy doesn't manage

Compose projects with containers that belong to no app, such as one you
started by hand from a folder Alchemy doesn't look in, are listed under
**Running outside your app folders** on the Apps page and at the end of
`alchemy app list`, with the folder they were started from and their
status, including when one crashed. Alchemy shows them but doesn't act on
them. To manage one, add its parent folder as an app folder.

## Adopting existing stacks

Already running stacks with `docker compose`? Point Alchemy at them:

1. Add the folder that holds your stack folders, for example
   `alchemy settings set app-folders /opt/stacks`.
2. Open the Apps page, or run `alchemy app list`.

Nothing is restarted, moved, or rewritten. A stack that is already up shows
as Running straight away, because Alchemy recognises its containers by the
Compose project name.

Alchemy never picks a project name itself. Compose works it out from the
folder, exactly as it does when you type `docker compose` there: the
top-level `name:` in the compose file, else `COMPOSE_PROJECT_NAME` in the
folder's `.env`, else the folder's name. `alchemy app show NAME` prints the
result. Things to check when adopting:

- **A stack shows Stopped, but its containers are listed under Running
  outside your app folders.** It was started under another project name,
  with `docker compose -p` or a `COMPOSE_PROJECT_NAME` set in your shell.
  Put that name in the compose file as `name:` (or in `.env`) so the folder
  always means that project, or take it down by hand once with
  `docker compose -p OLDNAME down` and start it from Alchemy.
- **Renaming a folder** to make it a valid app name also changes the
  project name Compose derives from it. Add `name: oldname` to the compose
  file first to keep the existing containers, networks, and volumes.
- **Two apps that resolve to the same project** see the same containers.
  Both show a problem saying so, and actions on them are refused unless you
  force them (`--force` in the CLI), because they would reach both.

## Creating and editing apps

Alchemy can write two files for you: an app's compose file and its `.env`.
Every change is checked with `docker compose config` before it is saved,
and nothing Compose rejects is ever written, so a typo never leaves a
broken file on disk. You can still create and edit the files by hand;
Alchemy picks up the change within a few seconds.

### Creating an app

On the **Apps** page, choose **Add app**. Give it a name (lowercase
letters, digits, `-`, and `_`; it becomes the folder's name), pick the app
folder if you have more than one, paste the compose file, and optionally a
`.env` under **Environment file (.env)**. **Create app** opens the new
app's page; **Create and start** also starts it, and the page shows its
progress. From the CLI:

```sh
alchemy app create blog                          # copies ./compose.yaml
alchemy app create blog --file ~/blog.yaml --env-file ~/blog.env
cat compose.yaml | alchemy app create blog --file -
alchemy app create blog --folder /opt/stacks     # an app folder other than the home
alchemy app create blog --dry-run                # only check it with Compose
alchemy app create blog --start                  # create it, then start it
```

The app gets a new folder, `HOME/NAME` or `FOLDER/NAME`, holding
`compose.yaml` and, if you gave one, `.env`. The home is created the first
time an app goes in it. `--folder` must be one of your app folders.
Creating an app doesn't start it unless you ask: **Create and start** on
the form or `--start` in the CLI starts it once Alchemy has read the new
files, as **More actions › Create and start** (`alchemy app up NAME`)
would. If it can't be started, for example because Docker isn't running,
the app is still created and Alchemy says why it didn't start.

Alchemy never replaces a folder, since it may hold data: if the folder
already exists, even empty, or another app already has the name, the app
isn't created.

### Editing an app's files

Each app's page has an **Edit** section with its compose file and its
`.env`. Change one and choose **Save**, or **Save and start** to bring the
app up with it; **Undo changes** puts back what was loaded. From the CLI:

```sh
alchemy app edit blog         # the compose file
alchemy app edit blog --env   # the .env
alchemy app edit blog --start # save, then start it with the change
```

`alchemy app edit` opens a private copy in `$VISUAL` or `$EDITOR` (`vi` if
neither is set), so it needs a terminal. When you quit, Compose checks it.
If Compose rejects it, the editor opens again with Compose's message at the
top on lines starting `# alchemy:`; quit without changing anything to give
up. Once Compose accepts it, `alchemy` asks before saving (`--yes` skips
the question, `--dry-run` saves nothing).

- **Hand edits are never overwritten.** Alchemy saves only if the file on
  disk is still the one you started editing. If someone changed it in the
  meantime, by hand or from another window, nothing is saved: the web UI
  offers **Reload it**, and the CLI keeps your copy and says where it is.
  `alchemy app edit --force` saves your version over the change instead.
- **Only that file is replaced,** in one step, keeping its permissions. A
  `.env` written by Alchemy is always readable by you alone (`0600`).
- **`COMPOSE_FILE` in `.env` is followed.** The compose file you edit is
  the first one it lists, and Compose checks it together with the rest. That
  first file must sit in the app's folder, and `COMPOSE_FILE` can't use
  `$VARIABLES`; otherwise edit the files by hand.
- **Saving doesn't restart anything** unless you ask. **Save and start**
  (`alchemy app edit --start`) saves, then brings the app up with the
  change once Alchemy has read it, as **Create and start** would; if it
  can't start the app, the file stays saved and Alchemy says why.
  Otherwise run **Update** or **Create and start** to apply the change.
- Each file can be up to 256 KiB of text. Apps whose folder or file is a
  symlink can't be edited from Alchemy.

## Actions

Every app has the same actions in the web UI (on its page) and the CLI. Each
runs `docker compose` in the app's folder, with no `-p` or `-f`, so it acts on
the same project you would by hand.

| Action  | Web UI                          | CLI                        | Runs                                               |
| ------- | ------------------------------- | -------------------------- | -------------------------------------------------- |
| Start   | Start                           | `alchemy app start NAME`   | `docker compose start`: starts existing containers |
| Stop    | Stop                            | `alchemy app stop NAME`    | `docker compose stop`: stops them, keeps them      |
| Restart | Restart                         | `alchemy app restart NAME` | `docker compose restart`                           |
| Update  | Update                          | `alchemy app update NAME`  | See below                                          |
| Up      | More actions › Create and start | `alchemy app up NAME`      | `docker compose up --detach`                       |
| Pull    | More actions › Download images  | `alchemy app pull NAME`    | `docker compose pull`, without restarting anything |
| Build   | More actions › Build images     | `alchemy app build NAME`   | `docker compose build`, for services with `build:` |
| Down    | More actions › Take down        | `alchemy app down NAME`    | `docker compose down`                              |

- **Start** only starts containers that exist. For a new app, or one you
  took down, use **Create and start** (`up`), which creates the containers
  and downloads or builds any missing images first.
- **Take down** (`down`) removes the app's containers and networks. Its
  volumes and its folder are kept, and Create and start brings it back. It
  asks first; in the CLI, `--yes` skips the question, and without a
  terminal it refuses rather than waiting.
- **Update** gets new images and recreates the containers that changed:
  `docker compose pull`, then `up --detach`. For an app with a `build:`
  service, it pulls the services that don't build, runs
  `docker compose build --pull` so base images are fresh, then
  `up --detach`. There is no rollback: Alchemy doesn't keep the previous
  image for you. To go back, pin the previous tag in the compose file and
  update again.

`alchemy app ACTION NAME --dry-run` prints the exact commands an action
would run, and changes nothing.

One action runs on an app at a time. While one runs, the buttons wait and
the CLI is refused with the running action's ID. Actions are also refused
while Docker isn't running or before Compose has read the app.

### The environment Compose runs in

Alchemy runs `docker compose` with a short, fixed set of variables from its
own environment: `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `TZ`, `TMPDIR`,
`DOCKER_CONFIG`, `DBUS_SESSION_BUS_ADDRESS`, and the `LC_*` and `XDG_*`
variables. Everything else is left out on purpose, including every
`COMPOSE_*` variable, proxy settings, and `SSH_AUTH_SOCK`. Put the
variables an app needs in its `.env` file, where both Alchemy and a
hand-run `docker compose` read them.

## Opening an app

An app that serves a web page gets its own addresses, and **Open** on its
page uses the best one that works. The page lists them all and says where
each one works. `alchemy app show NAME` lists them too. For an app named
`photos`:

| Address                    | Works from                                             |
| -------------------------- | ------------------------------------------------------ |
| `http://photos.local`      | Any device on your network, such as your phone         |
| `http://photos.local:8101` | Any device on your network, when port 80 isn't free    |
| `http://192.168.1.20:8101` | Any device on your network, by this computer's address |
| `http://photos.localhost`  | This computer only                                     |

The port, from 8100 to 8999, is the app's own and doesn't change. Alchemy
works out which service and port to open. When it can't, for example
because the app has more than one web port, the app's page says what to
add to the compose file:

```yaml
x-alchemy:
  service: web # which service serves the web page
  port: 8080 # its port inside the container
  hostname: pics # optional: pics.local instead of photos.local
```

### Opening apps from your phone

Alchemy announces each app's `.local` name on your network, so phones,
tablets, and other computers find it with no setup. iPhones, iPads, and
Macs open `.local` names, Android does from version 12, and so do most
Linux desktops. The phone has to be on the same Wi-Fi as this computer,
and not a guest network. If the name doesn't open, turn off any VPN on the
phone, or use the address with the IP.

- **On a Mac,** if Alchemy asks to find devices on your local network,
  allow it. If the **Machines** page says the Mac isn't letting Alchemy
  announce apps, turn on Alchemy (or alchemyd) in System Settings ›
  Privacy & Security › Local Network.
- **If another device already uses the name,** the app's page says so and
  its `.local` address won't work. Pick another name with `hostname:` in
  `x-alchemy`, as above.
- **After you stop Alchemy** or turn names off, a phone or Mac may still
  find the old name for a few minutes.
- **Addresses without a port need port 80.** On Linux, Alchemy needs
  your permission for it: select **Allow port 80** on the **Machines**
  page, or run `alchemy machine privileges grant low-ports` (see
  [Changes that need your password](#changes-that-need-your-password)).
  Until then, Open uses the address with the port. If another program on
  this computer already takes port 80, the Machines page says so, and
  Open uses the address with the port instead.

To stop announcing apps, turn off **Announce apps on your network** under
**Local names** on the **Machines** page, or run:

```sh
alchemy settings set mdns off   # on to turn it back on
```

Apps then have no `.local` address; the others keep working. To leave
just one app out, add `mdns: false` to its `x-alchemy` block.

## Changes that need your password

Alchemy itself never runs as an administrator. A few changes to the
computer need one, and Alchemy asks first, every time, with the system's
own password prompt. Each one can be undone. They are listed under
**Privileges** on the **Machines** page:

| Change             | What it does                                                                               | When you see it                                  |
| ------------------ | ------------------------------------------------------------------------------------------ | ------------------------------------------------ |
| **Port 80**        | Lets Alchemy use port 80, so apps open at `http://photos.local` with no port number.       | Linux only; a Mac doesn't need it                |
| **Firewall**       | Opens port 80, the app ports (8100 to 8999), and local names in the firewall, all at once. | When ufw, firewalld, or the macOS firewall is on |
| **System service** | Runs Alchemy when the computer starts, before anyone signs in, as you rather than as root. | Not offered yet                                  |

Select **Allow port 80**, **Open the firewall**, or **Install the
service**, and enter your password when the system asks. To undo one,
select **Remove port 80 access**, **Close the firewall**, or **Remove the
service**. Port 80 also has an **Allow port 80** button on the warning
that says addresses on port 80 don't work.

**Allowing port 80 restarts Alchemy's background service** for a moment
so the change takes effect. Your apps keep running. The desktop app
reconnects on its own. A browser you opened with `alchemy open` is signed
out, so run `alchemy open` again to see the result.

**When Alchemy can't show a password prompt**, for example over SSH, on a
Linux computer with no desktop, or with a copy of Alchemy that isn't
installed safely, the row shows a command instead. Run it in a terminal on
that computer, then select **Try again**:

```sh
sudo /usr/libexec/alchemy/alchemy-helper grant-low-ports
```

From a terminal, the CLI does all of this for you:

```sh
alchemy machine privileges                     # what's needed, and what's allowed
alchemy machine privileges grant low-ports     # also: firewall, system-service
alchemy machine privileges revoke low-ports    # undo it
```

When it needs the command above, it runs it itself, so `sudo` asks for
your password right there. Without a terminal, such as in a script, it
prints the command and exits with status 3.

Some things Alchemy never does for you:

- **It doesn't add you to the `docker` group.** That gives full control of
  the computer, so Alchemy explains it and leaves the choice to you.
- **It doesn't edit nftables or iptables rules.** If you manage the
  firewall that way, allow 80/tcp, 8100-8999/tcp, and 5353/udp yourself.
- **After an upgrade or rebuild, port 80 may need allowing again** on
  Linux, because the permission is stored on the program file.

## Progress

An action can take a while, so Alchemy shows it as steps in plain words,
such as "Downloading images" with "2 of 3 images downloaded" under it.

- **Web UI:** the app page shows the latest action and its steps, with
  Compose's own output folded under each step. While building, that
  includes what each build step prints, such as the output of a failing
  `RUN`, as `[2/3] RUN npm ci | ...`. Any open window shows it,
  whichever window or command started it. **Cancel** stops it.
- **CLI:** each step is printed on stderr as it starts or fails, and the
  result on stdout. `--json` prints every event as one line on stdout. The
  command exits 1 if the action fails or is cancelled. Ctrl-C stops
  watching, but the action keeps running in alchemyd. `alchemy app cancel
  NAME` stops it, as does `alchemy app cancel` with the operation id the
  action printed when it started.

Cancelling interrupts `docker compose` as Ctrl-C would, and kills it if it
doesn't stop soon after, so an app can be left part way through, the same
as interrupting Compose by hand. Stopping alchemyd during an action
interrupts it the same way and records it as interrupted.

## Logs

The app page has a **Logs** section: pick **All services** or one service,
and leave **Follow new lines** on to keep it current. **Copy logs** copies
what is shown. From the CLI:

```sh
alchemy app logs blog                  # the last 200 lines, all services
alchemy app logs blog web --tail 1000  # one service, more history
alchemy app logs blog -f               # keep following until Ctrl-C
```

Lines print as `service | line`, merged across containers in the order
Docker recorded them. `--tail` goes up to 5000, keeping the newest lines
that fit in 2 MiB. Alchemy serves up to 16 log views at once, across the
web UI, the desktop app, and the CLI; past that it answers that it is busy
until one is closed. While following, a
container that restarts or is recreated, for example by Update, is picked up
again without repeating lines. Lines are read from Docker directly, the same
lines `docker compose logs` shows. A line longer than 8 KiB is cut short and
ends in `…`.

## Usage

Alchemy measures this machine and every running container once a minute,
whether or not anyone is looking, and keeps the history for 30 days.

- **This machine:** CPU, memory, network, and the disk that holds your home
  folder (space used and reads and writes).
- **Each app and container:** CPU, memory, and network.

The **Machines** page charts both, over the last hour, 12 hours, 24 hours,
7 days, or 30 days, and lists each app's share. An app's page shows its
current figures under **Usage**. From the CLI:

```sh
alchemy machine stats
alchemy app stats blog
```

CPU is a share of the whole machine, so an app's containers add up to the
machine's figure. A figure that couldn't be measured is shown as `–`, never
as 0. History has gaps for the time alchemyd wasn't running.

**On macOS, and with Docker Desktop:** containers run inside a virtual
machine. The machine figures are your computer's own, not the VM's, and an
app's CPU is its share of the VM. The Machines page and
`alchemy machine stats` say so when it applies.

## Why an app stopped

When an app or a service isn't running, Alchemy says why, problem first:

| Shown as                | What happened                                                                                                      |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------ |
| Crashed (exit code 1)   | It exited on its own with an error, or its process was killed from outside Docker.                                 |
| Crashed (out of memory) | It ran out of memory and was killed.                                                                               |
| Keeps crashing          | It crashed three times in a row, each within a minute of starting, and its restart policy keeps starting it again. |
| Stopped outside Alchemy | Someone stopped it through Docker, such as `docker stop`, `docker kill`, or `docker compose stop` run by hand.     |
| Stopped                 | Alchemy stopped it (Stop, Restart, or Take down), or the reason can't be told.                                     |
| Finished                | It exited with code 0 on its own, as a one-off job does. A finished job doesn't make its app Partly running.       |
| Not running             | It was created but never started, or the app has no containers.                                                    |

Alchemy remembers what it last asked of each app, so it still tells its own
stops from someone else's after it restarts. Containers that were already
stopped when alchemyd started are checked once for their exit code.

## Where Alchemy keeps its state

Your apps live in their folders. Alchemy's own state is one SQLite database,
`alchemy.db`, in a per-user folder:

| System | Folder                                                          |
| ------ | --------------------------------------------------------------- |
| Linux  | `$XDG_STATE_HOME/alchemy`, or `~/.local/state/alchemy` if unset |
| macOS  | `~/Library/Application Support/Alchemy`                         |

It holds the app folder settings, the history of recent actions (the
latest 1000), usage history, and what Alchemy last asked of each app. It
holds no copy of your compose files or app data. The folder is readable
only by you (`0700`, and the database `0600`).

If the database is damaged, or was written by a newer Alchemy, alchemyd
refuses to start and says so. Move the file aside to start fresh: your apps
are untouched, and you lose only the settings and history. To keep it
somewhere else, start alchemyd with `--state-dir PATH` or set
`ALCHEMY_STATE_DIR`.

Only one alchemyd can use a state folder at a time. A second one started
on the same folder refuses to start and says another alchemyd is already
using it.

## Using docker compose by hand

Alchemy is meant to be easy to step around. Because it never passes `-p`
or `-f` and uses the project name Compose itself works out, running
`docker compose` in an app's folder acts on exactly the containers Alchemy
manages:

```sh
cd ~/Alchemy/apps/blog
docker compose ps
docker compose logs -f web
docker compose stop
docker compose up -d
```

- **When alchemyd stops, your apps keep running.** Stopping the daemon
  doesn't touch containers, and `docker compose` keeps working on them.
  When alchemyd starts again, it finds them as they are.
- **Changes you make by hand show up in Alchemy,** such as a stop shown as
  Stopped outside Alchemy, or a service you added to the compose file.
- **Keep the project name in the folder.** Set it with `name:` or in `.env`,
  not with `-p` or a `COMPOSE_PROJECT_NAME` or `COMPOSE_FILE` exported in
  your shell. Alchemy ignores those, so the folder would mean one project to
  Alchemy and another to your shell.
- **To stop using Alchemy,** stop alchemyd and delete its state folder.
  Your app folders and containers are left exactly as they are.

Alchemy's tests check this against real Docker: an app started by Alchemy
is listed, stopped, and started again by a hand-run `docker compose` in its
folder after the daemon is stopped, including an app whose compose file
sets its own `name:`.
