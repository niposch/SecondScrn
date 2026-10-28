SecondScrn for Linux
====================

Requirements
  * GNOME or KDE Plasma 6 on Wayland (the desktop portal's virtual monitors)
  * GTK 4.14+ and libadwaita 1.5+ (Fedora 40+, Ubuntu 24.04+, Arch, openSUSE
    Tumbleweed, KDE neon)
  * GStreamer 1.22+ with the base, good and bad plugins and the PipeWire
    plugin; VA-API drivers (Intel, AMD) or the NVIDIA driver for hardware
    encoding
      Fedora:   sudo dnf install gstreamer1-plugins-base gstreamer1-plugins-good
                gstreamer1-plugins-bad-free pipewire-gstreamer
      Arch:     sudo pacman -S gst-plugins-base gst-plugins-good gst-plugins-bad
                gst-plugin-pipewire gst-plugin-va
  * The tablet app must run the same SecondScrn version.

Install
  sudo ./install.sh          # into /usr/local, with the pen tablet udev rule
  ./install.sh --user        # into ~/.local, no root (pen moves the pointer)

Log out and in again once after installing so the pen tablet permission
applies. Then start SecondScrn from the application menu.

Uninstall
  sudo ./install.sh --uninstall   (or ./install.sh --user --uninstall)
