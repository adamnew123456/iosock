#!/bin/sh
repodir="$(git rev-parse --show-toplevel)"

die() {
    echo "$1"
    exit
}

[ -n "$repodir" ] || die "Must be run within the iosock repository"
cd "$repodir" || die "Cannot go to the repository: $repodir"

iosock=./target/release/iosock
sock=./network-test.sock
fifo=./network-test.fifo

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

echo "Running /dev/zero -> nc -> /dev/null copy for 10 seconds..."
"$iosock" "$sock" cat &
victim="$!"
pv "$fifo" > /dev/null &
nc -U  "$sock" < /dev/zero > "$fifo" &

sleep 10
kill -s TERM "$victim"
sleep 1
echo # Avoid clobbering pv output
echo
echo "$victim stopped"
wait
rm "$fifo"
