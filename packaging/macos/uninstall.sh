#!/bin/sh
# Remove the Tether host: stop + remove the LaunchAgent, delete the binary and logs.
# Pairing state (~/.config/tether: host key, paired devices, config) is kept
# unless you confirm — deleting it means every phone must pair again.
# Run as yourself (it uses sudo only to delete /usr/local files).
set -u

PLIST="$HOME/Library/LaunchAgents/com.tether.daemon.plist"
launchctl bootout "gui/$(id -u)/com.tether.daemon" 2>/dev/null || true
rm -f "$PLIST"
rm -rf "$HOME/Library/Logs/Tether"
sudo rm -f /usr/local/bin/tetherd
sudo rm -rf /usr/local/share/tether
sudo pkgutil --forget com.tether.host >/dev/null 2>&1 || true

if [ -d "$HOME/.config/tether" ]; then
  printf "Also delete pairing state in ~/.config/tether? Every device would need to pair again. [y/N] "
  answer=""
  read -r answer || answer=""  # EOF / no tty: treat as No
  case "$answer" in
    y|Y|yes|YES) rm -rf "$HOME/.config/tether"; echo "Removed ~/.config/tether." ;;
    *) echo "Kept ~/.config/tether." ;;
  esac
fi
echo "Tether host uninstalled."
