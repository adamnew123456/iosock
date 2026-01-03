#!/bin/sh
repodir="$(git rev-parse --show-toplevel)"

die() {
    echo "$1"
    exit
}

[ -n "$repodir" ] || die "Must be run within the iosock repository"
cd "$repodir" || die "Cannot go to the repository: $repodir"

iosock=./target/release/iosock
sock=./console-test.sock
fifo=./console-test.fifo

echo "Rebuilding $iosock"
cargo build -r 

if [ -e "$sock" ]; then
    echo "Old $sock present, removing"
    rm "$sock"
fi

if [ -e "$fifo" ]; then
    echo "Old $fifo present, removing"
    rm "$fifo"
fi
mkfifo "$fifo"

echo "Running /dev/zero -> /dev/null copy for 10 seconds..."
pv "$fifo" > /dev/null &
"$iosock" "$sock" cat /dev/zero > "$fifo" &

victim="$!"
sleep 10
kill -s TERM "$victim"
sleep 1
echo # Avoid clobbering pv output
echo "$victim stopped"
wait
rm "$fifo"
