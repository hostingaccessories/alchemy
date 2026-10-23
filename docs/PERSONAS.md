# Alchemy: Personas

Alchemy is designed for two people: **The Tinkerer** and **The Novice**.
Design docs, PRDs, and UX reviews should name which persona a change serves
and check it against both. See [VISION.md](VISION.md) for the product they
share.

**Design rule:** The Novice's path never requires knowledge that The
Tinkerer's path takes for granted. The Tinkerer's path is never blocked by
The Novice's guardrails.

---

## The Tinkerer

### Who they are

An experienced web developer, software engineer, and systems
administrator. Comfortable with Docker, networking, DNS, and TLS, at home on
the command line and in YAML, and used to reading logs to find out what went
wrong.

### Their setup

- Already runs homelab services with Docker Compose, on one or more Linux
  machines.
- Writes compose files by hand or with an AI assistant.
- Runs a mix of stacks: multi-container apps with their own databases, apps
  that need USB devices or host networking, and downloads routed through a
  VPN container.
- Often happy with `.local` addresses because they're easy. Sometimes owns a
  domain and wants real certificates through Cloudflare or DigitalOcean.

### What they want

A management layer between "compose files in a folder" and a full
deployment platform. Something that adds web addresses, certificates,
diagnostics, and multiple machines, and otherwise stays out of the way.

### What they need from Alchemy

- Real compose files on disk that work without Alchemy.
- A choice of install: native CLI, container, or desktop app, whichever fits
  the machine.
- Agent mode to manage several machines from one place.
- A CLI that can do everything the web interface can, scriptable
  (`--json`), with sharp knives available and standard protections
  (`--dry-run`, `--force`, `--yes`).
- Eject from the catalog at any time. No lock-in.
- Automatic choices that are visible and overridable.

### How they get help

Docs, logs, source code, and AI assistants. They are also often the person
The Novice turns to.

### What success looks like

Their existing stacks run under Alchemy across their machines, every app has
an address, and they never hand-edit DNS or proxy config again.

### Design implications

- Never hide a file or setting they could reasonably want; fold it under
  Advanced instead.
- Every web action has a CLI equivalent.
- Defaults are a starting point, never a wall.
- Explain automatic decisions in terms they can verify (paths, ports,
  networks).

---

## The Novice

### Who they are

Comfortable following a tutorial step by step and running the commands it
gives them. Read a blog post about self-hosting and wants to try it on the
computer they already own.

### Their setup

- Their everyday Mac, or a Linux desktop, with Docker Desktop installed.
- The machine sleeps, restarts for updates, and sometimes Docker Desktop
  isn't running.
- No domain of their own; apps live at `.local` addresses.
- One machine.

### What they want

- **Immich**, to back up family photos from their phone.
- **A media server** for family movies.
- **A home dashboard** or **Home Assistant**.

### What Alchemy handles for them

These are Alchemy's responsibilities, so The Novice can focus on the apps
they came for:

- **Ports.** They never pick one or resolve a conflict.
- **Addresses, DNS, and domains.** Every app gets a web address that works
  from their phone, with no DNS setup.
- **Getting from installed to open.** An install isn't finished until the
  app opens in a browser.
- **Keeping things working.** After a restart, sleep, or update, apps come
  back, or Alchemy says plainly what happened and what to do.
- **Knowing what's safe.** It's always clear which actions are routine and
  which could affect their data, such as photos in Immich.

### How they get help

- **Tutorials and blog posts.** Instructions should map cleanly onto what
  Alchemy shows.
- **AI assistants.** They paste messages and errors into a chat, so messages
  must be accurate, specific, and easy to copy.
- **A more technical friend or family member**, often The Tinkerer. Alchemy
  should make it easy to share what's going on (a copyable diagnostic
  summary, consistent names for things).

### What success looks like

They install Alchemy and Immich on their Mac, open Immich on their phone,
and it's still working next week.

### Design implications

- Plain, consistent words: "app", "machine", "web address", "start", "stop".
  Avoid internal terms like stack, node, or container in the main path.
- Every install ends with an **Open ‹app›** button.
- Show health problem-first: the one thing that's wrong and the next step,
  with details available on request.
- Show progress as plain steps ("Downloading the app…", "Starting the
  app…") with the raw output folded away.
- Distinguish a deliberate stop from a crash, and say which it was.
- Advanced options live under Advanced; the default view is enough to
  install, open, update, and stop an app.
- Never ask them to choose a port, edit DNS, or edit a compose file to
  finish an install.
