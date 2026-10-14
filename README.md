# ReSkate Installer

Installs and configures [ReSkate](https://github.com/Dingo-Shenanigans/ReSkate) for skate. on Linux.

ReSkate runs under Proton through a Steam launch option that starts ReSkate's launcher in place of EA's anti-cheat. This installer downloads ReSkate, sets that launch option, and reverses both on uninstall.

Requires skate. in your Steam library (native or Flatpak). It doesn't have to be installed: since ReSkate plays its own copy, the installer can add a placeholder that Steam treats as installed, and Steam then downloads only EA's anti-cheat (about 350 MB) instead of the whole game. Don't use Verify integrity of game files on it, which downloads the whole game. When skate. updates on Steam, click Repair. Not affiliated with ReSkate or Electronic Arts.

## Usage

Open the installer, choose a folder, and click Install. Then launch skate. from Steam. On first launch, ReSkate asks you to sign in to Steam and downloads its own copy of the game (about 14 GB).

If EA's anti-cheat error appears when launching, open the installer and click Repair.

From the command line:

```
reskate-installer install [<folder>] [--version <tag>] [--rpc]
reskate-installer off [--delete]
```

Steam must be closed unless `--close-steam` is passed. `--delete` also removes the game files.

Discord Rich Presence (the checkbox, or `--rpc`) installs [rpc-bridge](https://github.com/EnderIce2/rpc-bridge) into the ReSkate folder and adds it to the launch option. It works with Discord and Vesktop (with arRPC turned on), and needs a folder path without spaces. With Flatpak Steam, also run `flatpak override --user --filesystem=xdg-run/discord-ipc-0 com.valvesoftware.Steam`.

## Building

```
cargo build --release
```

## License

GPL-3.0
