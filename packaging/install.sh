#!/bin/sh
# Installs or removes soundiya on the speaker. Run as root, from the unpacked package:
#   sh install.sh              install or update, then start
#   sh install.sh uninstall    remove (keeps your login unless --purge is given)
set -eu
SRC=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
DIR=/data/opt/soundiya
INIT=/etc/init.d/S99soundiya
SPOTIYA_INIT=/etc/init.d/S99spotify-connect
LINK=/usr/bin/soundiya

die() {
    echo "error: $*" >&2
    exit 1
}
[ "$(id -u)" -eq 0 ] || die 'run this installer as root on the speaker'

# The root filesystem is read-only on stock firmware; open it only for the few files that need it.
WAS_RO=0
root_rw() {
    if grep -qE '^[^ ]+ / [^ ]+ ro[ ,]' /proc/mounts; then
        WAS_RO=1
        mount -o remount,rw / || die 'cannot remount the root filesystem read-write'
    fi
}
root_restore() {
    if [ "$WAS_RO" -eq 1 ]; then
        sync
        mount -o remount,ro / || echo 'warning: could not restore the read-only root' >&2
        WAS_RO=0
    fi
}
trap root_restore EXIT

action=${1:-install}
case "$action" in
    uninstall)
        purge=${2:-}
        [ ! -x "$DIR/stop.sh" ] || "$DIR/stop.sh" || true
        root_rw
        rm -f "$INIT" "$LINK"
        if [ -f "$SPOTIYA_INIT" ] && [ ! -x "$SPOTIYA_INIT" ] && [ -f "$DIR/spotiya-was-enabled" ]; then
            chmod 755 "$SPOTIYA_INIT"
            rm -f "$DIR/spotiya-was-enabled"
            echo 'Spotiya autostart re-enabled'
        fi
        root_restore
        if [ "$purge" = --purge ]; then
            rm -rf "$DIR"
        else
            rm -f "$DIR/soundiya" "$DIR/run.sh" "$DIR/start.sh" "$DIR/stop.sh" "$DIR/install.sh"
            echo "kept $DIR/state (login and settings); add --purge to delete it"
        fi
        echo 'soundiya removed'
        exit 0
        ;;
    install) ;;
    *) die "unknown action '$action'" ;;
esac

[ -f "$SRC/soundiya" ] || die "no 'soundiya' binary next to the installer"
chmod 755 "$SRC/soundiya"
# Refuse a binary that cannot run here before touching anything.
"$SRC/soundiya" help >/dev/null 2>&1 || die 'the soundiya binary does not run on this speaker'
free_kb=$(df -k /data 2>/dev/null | tail -n 1 | tr -s ' ' | cut -d' ' -f4 || true)
case "$free_kb" in
    '' | *[!0-9]*) ;;
    *) [ "$free_kb" -gt 8192 ] || die 'less than 8 MB free in /data' ;;
esac

mkdir -p "$DIR/state"
chmod 700 "$DIR/state"
[ ! -x "$DIR/stop.sh" ] || "$DIR/stop.sh" >/dev/null 2>&1 || true

cp "$SRC/soundiya" "$DIR/soundiya.new"
chmod 755 "$DIR/soundiya.new"
mv "$DIR/soundiya.new" "$DIR/soundiya"
for f in run.sh start.sh stop.sh install.sh; do
    cp "$SRC/$f" "$DIR/$f.new"
    chmod 755 "$DIR/$f.new"
    mv "$DIR/$f.new" "$DIR/$f"
done
if [ ! -f "$DIR/state/config.json" ]; then
    cat >"$DIR/state/config.json" <<'EOF'
{
  "profile": "auto",
  "voice": true,
  "bypass": {
    "strategy": "auto",
    "doh": true,
    "domains": [],
    "upstream_proxy": null
  }
}
EOF
fi

# Spotiya (the Spotify receiver) and soundiya both want the sound card and Alice.
if [ -x "$SPOTIYA_INIT" ] && [ -x /data/opt/spotify-connect/spotiya ] && [ "${KEEP_SPOTIYA:-0}" != 1 ]; then
    echo 'Spotiya is installed: switching its autostart off (sh install.sh uninstall brings it back)'
    [ ! -x /data/opt/spotify-connect/stop.sh ] || /data/opt/spotify-connect/stop.sh || true
    : >"$DIR/spotiya-was-enabled"
    root_rw
    chmod 644 "$SPOTIYA_INIT"
fi

root_rw
cp "$SRC/S99soundiya" "$INIT.new"
chmod 755 "$INIT.new"
mv "$INIT.new" "$INIT"
ln -sf "$DIR/soundiya" "$LINK"
root_restore

"$DIR/start.sh"
echo
echo 'Done. Say: "Алиса, включи <исполнитель или трек>".'
echo 'Next, on this speaker:'
echo '  soundiya check    test SoundCloud access'
echo '  soundiya auto     find the bypass strategy that works on your network (a few minutes)'
echo '  soundiya login    optional sign-in for likes and favorites'
echo 'Remove with:  /data/opt/soundiya/install.sh uninstall'
