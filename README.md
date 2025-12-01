# Porkbun Dynamic DNS Client

A small application, meant to be run as a service, that uses Porkbun's API to
update DNS `A` and `AAAA` records.

Some parts of this software are more complex than they need to be. That's
because I love writing code and I wanted to have fun!

This project is not yet complete (but it's close!). The goal is to package it in
such a way that it is compatible with `pacman` as a `*-git`-style package.
Hopefully, that package will come with all the standard amenities like a
dedicated unit file and service user.

## Todo

- [ ] Write systemd timer unit
- [ ] Write `PKGBUILD` for Arch

### Later todo

- [ ] Document TOML configuration file in `--help`
- [x] Configure `tokio` features to only include the ones we need
- [ ] Evaluate binary size, configure build optimization flags if needed
