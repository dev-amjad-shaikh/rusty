# Linux laptops (Ubuntu, Fedora, Debian) — checks, commands, known causes

## Facts to get
- Distro and kernel: `cat /etc/os-release`, `uname -r`. Desktop: GNOME/KDE, Wayland or X11 (`echo $XDG_SESSION_TYPE`).
- Disk: `df -h /` and `df -h /home`; `/boot` full breaks kernel updates (`df -h /boot`).
- Logs: `journalctl -p err -b` (this boot), `journalctl -b -1` (last boot, for crashes), `dmesg -T | tail`.
- Hardware: `lspci -nnk` (drivers in use), `lsusb`, `upower -i $(upower -e | grep BAT)` for battery health.
- Management, if any: the organisation's agent (osquery, Fleet, Landscape); note the last check-in in its console.

## Symptom → ranked causes → fix
| Symptom | Most often | Check | Fix |
|---|---|---|---|
| Slow | A process (`top`/`htop`), swap thrash (`free -h`), a full disk, baloo/tracker indexing | `top`, `free -h`, `df -h` | Kill/restart the process; `sudo apt clean`/`dnf clean`; remove old kernels (`sudo apt autoremove`); pause indexing |
| Will not boot to desktop | A kernel update with a missing DKMS module (NVIDIA, VirtualBox), a full `/boot`, a broken display manager | Boot the previous kernel from GRUB (hold Shift/Esc); `journalctl -b -1 -p err` | Rebuild DKMS (`sudo dkms autoinstall`), free `/boot`, `sudo systemctl restart gdm`/`sddm` |
| Wi-Fi missing | Driver/firmware for the chipset (Realtek, Broadcom), rfkill, NetworkManager stopped | `nmcli device`, `rfkill list`, `dmesg | grep -i firmware` | `sudo rfkill unblock all`; install `linux-firmware`/vendor driver; `sudo systemctl restart NetworkManager` |
| VPN | See network reference; on Linux, DNS split via systemd-resolved (`resolvectl status`) is the usual culprit | `resolvectl status` | Set the VPN's DNS domains on the tunnel interface |
| Audio | PipeWire/PulseAudio picked the wrong sink, a muted channel | `wpctl status` / `pactl list sinks short` | Pick the sink in settings; `systemctl --user restart pipewire pipewire-pulse wireplumber` |
| External display | Wayland vs X11 quirks, NVIDIA driver | Try the other session at login | Switch session; update driver |
| Suspend/resume broken | Kernel/driver, especially NVIDIA and some Wi-Fi | `journalctl -b | grep -i suspend` | Update kernel/driver; disable a module on suspend as a workaround |
| Package manager broken | Interrupted upgrade, a held or broken package | `sudo apt --fix-broken install`, `sudo dpkg --configure -a`; `sudo dnf distro-sync` | As per check |
| Disk encryption prompt at boot fails | Wrong keyboard layout at the LUKS prompt, a changed passphrase | Try US layout | `cryptsetup luksChangeKey` with the old key |
| Printing | CUPS stopped, no driver | `systemctl status cups`, http://localhost:631 | Start CUPS; add the printer via IPP Everywhere |

## Escalate when
Hardware errors in `dmesg` (I/O errors, MCE), battery under 60% health, no power; anything that needs a distro reinstall without a backup.
