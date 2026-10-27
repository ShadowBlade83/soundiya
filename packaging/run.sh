#!/bin/sh
# Keeps soundiya running: restarts it a few seconds after a crash, exits when stop.sh asks.
DIR=/data/opt/soundiya
PIDFILE=/run/soundiya.pid
STOP=/run/soundiya.stop
LOG=/tmp/soundiya.log
umask 077
echo $$ >"$PIDFILE"
child=
trap 'touch "$STOP"; [ -z "$child" ] || kill "$child" 2>/dev/null' TERM INT
rm -f "$STOP"
while [ ! -f "$STOP" ]; do
    # /tmp is RAM; do not let the log grow without bound.
    if [ -f "$LOG" ] && [ "$(wc -c <"$LOG")" -gt 1048576 ]; then
        : >"$LOG"
    fi
    "$DIR/soundiya" --state-dir "$DIR/state" run >>"$LOG" 2>&1 &
    child=$!
    wait "$child"
    child=
    [ -f "$STOP" ] && break
    sleep 3
done
rm -f "$PIDFILE"
