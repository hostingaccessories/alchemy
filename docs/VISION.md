# Alchemy: Vision

> Run the homelab apps you want on the machines you already have, and open
> them in a browser without thinking about ports, DNS, or certificates.

---

## 1. Problem

A container that is running is not the same as an app that works. Between
`docker compose up` and opening the app on a phone sit a handful of details:

- Which port the app listens on, and whether another app already has it.
- What address to type, and whether it works from other devices on the
  network, not just the machine it runs on.
- DNS, domains, and TLS certificates, if you want a nicer address.
- A firewall that quietly blocks traffic.
- What happens after a reboot, a laptop going to sleep, or an update.

Tutorials usually cover the compose file and stop there. Alchemy's job is
everything after it.

## 2. Who it's for

Alchemy is built for two people, described in detail in
[PERSONAS.md](PERSONAS.md):

- **The Tinkerer** has run web services and servers for years and already
  manages homelab apps with Docker Compose. They want a management layer
  that handles addresses, certificates, diagnostics, and multiple machines
  without hiding the files or getting in the way.
- **The Novice** read a blog post about self-hosting and wants to try Immich
  or a media server on the computer they already own. They follow
  instructions well and want Alchemy to take care of the rest.

**Design rule:** The Novice's path never requires knowledge that The
Tinkerer's path takes for granted. The Tinkerer's path is never blocked by
The Novice's guardrails.

## 3. What Alchemy is

A Docker Compose management platform that:

1. **Installs apps from a catalog** with good defaults: pinned images,
   generated secrets, and declared storage, devices, and ports.
2. **Runs your own compose stacks**: edit the YAML, build, pull, start,
   stop, restart, update, and read logs. Stacks can build from their own
   Dockerfiles.
3. **Gives every app a web address automatically**, through a managed
   reverse proxy.
4. **Handles ports.** Web apps need no published ports. Ports that must be
   published are chosen and conflict-checked by Alchemy.
5. **Explains problems in plain language.** When an app doesn't open,
   Alchemy finds the first broken link (container, network, proxy, name,
   certificate, firewall) and says what to do about it.
6. **Keeps real compose files on disk**, so every app can be managed by hand.

## 4. How people get it

Docker itself is no longer the main way Alchemy is installed. Every option
ships the same web interface.

| Option | Mainly for | Notes |
|---|---|---|
| **Desktop app** | The Novice | Installed alongside Docker Desktop on macOS or Linux. Windows later. Open it and start installing apps. |
| **Native CLI** | The Tinkerer | OS-native install of the `alchemy` CLI and background service, with the web interface bundled in. |
| **Container image** | The Tinkerer | The same software running in Docker, for machines where everything runs in containers. |

The Tinkerer can pick whichever fits the machine, including the desktop app.

## 5. One machine first, more when you want them

The primary use case is a single machine: Alchemy and the apps it manages
run on the same computer.

The same binary or container can run in **agent mode** on additional
machines. One Alchemy instance then manages containers across all of them,
from one interface. This is mainly a Tinkerer capability; The Novice never
needs to know it exists.

## 6. Names and addresses

| Mode | Example | Who uses it |
|---|---|---|
| **Local** (default) | `http://immich.local` | The Novice almost always. The Tinkerer often, because it's easy. |
| **Own domain** | `https://immich.home.example.com` | The Tinkerer, when they want their own domain. TLS is optional, through a supported DNS provider (Cloudflare and DigitalOcean first, more later). |

Local mode uses mDNS and needs no setup. Every app also gets a plain
`IP:port` address for devices that don't resolve `.local` names.

For The Novice, the host is usually an everyday laptop. It sleeps, restarts,
and sometimes Docker Desktop isn't running. Alchemy treats these as normal
events: apps come back on their own, and when they can't, Alchemy says why
and what to do.

## 7. Sharp knives

Powerful and destructive options exist, but they are not the first thing
anyone sees.

- **Web interface:** defaults are safe. Options that can break an app or
  touch its data (editing generated files, ejecting from the catalog,
  deleting data, low-level container access) live behind **Advanced** tabs
  and disclosures.
- **CLI:** a sharp knife by design, with standard protections scaled to
  what's at risk: confirmations, `--dry-run` to preview, `--force` to
  override a safety check, and `--yes` for scripts.
- Anything that deletes data is always an explicit, separate action.

## 8. Principles

1. **Compose files on disk are the source of truth.** Every app is a real
   directory with a real `compose.yaml` that works with `docker compose`.
2. **If Alchemy stops, apps keep running** and stay manageable by hand.
3. **Explain, don't hide.** Every automatic choice (port, address, network,
   certificate) is visible in the interface and readable in the files.
4. **Opinionated defaults, honest escape hatches.** Any catalog app can be
   ejected into a plain compose stack.
5. **Secure by default.** No unauthenticated control surfaces, no exposed
   Docker API, nothing reachable from the internet unless someone chooses it.
6. **Works on any home network.** Alchemy never assumes an address range is
   free; it checks before creating networks.
7. **No telemetry.**
8. **Small and boring.** Few moving parts, quick to install, easy to remove.

## 9. Non-goals

- A build-and-deploy pipeline: pulling source from git, building on push,
  preview environments. Custom stacks can still use `build:` with their own
  Dockerfiles, and Alchemy can build, pull, start, and stop them like any
  other stack.
- Swarm, Kubernetes, or any other orchestrator.
- Multi-tenant or public hosting.
- Configuring an app's internals. Alchemy runs apps; it doesn't set them up
  for you inside.

## 10. Success criteria

- **Novice test:** on their own Mac with Docker Desktop, with no help, The
  Novice installs Alchemy and Immich and opens Immich from their phone in
  under 15 minutes.
- **Sleep test:** after the laptop sleeps or restarts, apps come back on
  their own, or Alchemy says plainly what to do.
- **Tinkerer test:** The Tinkerer moves their existing compose stacks onto
  Alchemy across two or more machines, using native or container installs,
  without hand-editing DNS or proxy config.
- **Home-network test:** install succeeds on `10.0.1.0/24`, `172.17.0.0/16`,
  and `192.168.0.0/16` networks with no manual Docker networking changes.
- **Firewall test:** with a firewall blocking an app's port, Alchemy names
  the cause within one click.
- **Escape test:** stop Alchemy entirely, and every app keeps running and
  can be managed with `docker compose` from its directory.
