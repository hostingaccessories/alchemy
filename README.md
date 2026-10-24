# Alchemy

> [!CAUTION]
> Alchemy is in alpha and is not ready for anyone to actually use.

Docker-based homelab management for everyone.

Run the homelab apps you want on the machines you already have, and open them
in a browser without thinking about ports, DNS, or certificates.

- [Managing your apps](docs/site/src/pages/docs/apps.md)
- [Vision](docs/VISION.md)
- [Personas](docs/PERSONAS.md)
- [Development](docs/DEVELOPMENT.md)
- [Architecture](docs/ARCHITECTURE.md)

## Why?

There's a wonderful world of free, open source software you can run on your
own computer at home: photo libraries, document scanners, media servers for
your own movies, and much more.

Getting it running is another story. Installing anything that isn't a Windows
`.exe` or a Mac `.dmg` gets overwhelming fast, and then come containers,
images, ports, DNS, reverse proxies, and certificates. A container that's
running isn't the same as an app you can open on your phone.

Package managers like apt and Homebrew, and Docker itself, solve a lot of
this. But they're tools for people who already understand them, and let's
face it, Docker confuses even seasoned engineers from time to time. Someone
following a YouTube video or blog post shouldn't need to learn any of it just
to back up their photos and get out of Google Photos or iCloud.

On the flip side, plenty of us already run homelab apps with Docker Compose.
We want something between running `docker compose` by hand and a full
platform as a service: something that handles addresses, certificates, and
diagnostics, keeps real compose files on disk, and otherwise stays out of the
way. Something we can comfortably recommend to friends and family who follow
directions well, and use ourselves, so when they call for help we're looking
at the same thing they are.

If most of this was confusing apart from escaping Google Photos, don't worry:
Alchemy is for you. If you nodded along, remembering the pain of learning all
of it, Alchemy is for you too.

## License

[MIT](LICENSE)
