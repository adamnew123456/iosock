# What is this?

**iosock** is a simple wrapper that connects a program's stdio to a Unix domain
socket server. When the socket is disconnected, the program receives no input
via stdin and its stdout and stderr go to iosocks' outputs. When the socket is
connected, data from the socket is passed to the process's stdin and its stdout
and stderr are sent to the socket.

# Why write this?

The use case is for allowing easy access to containerized servers that expose a
command line. For reasons unknown, `podman attach` does not work reliably for
me when running the container as a quadlet (probably having to do with having
no tty). Having this tool lets me export a Unix socket via a bind mount and
then connect to the admin interface via netcat.

# How do I use it?

Build it:

```sh
$ cargo build -r
```

Then run it with the path to bind the Unix domain socket to, along with any
arguments to pass to the command you want to run:

```sh
$ iosock ./cmd.sock dash -x
```

At this point the server is accepting connections. Connect to it via `socat`,
`nc`, or a similar client and run commands:

```sh
$ nc -U ./cmd.sock
ls
+ ls
Cargo.lock
Cargo.toml
cmd.sock
LICENSE
...
exit
+ exit
```

Closing the connection leaves the process open so you can reconnect to it
later. To run more commands:

```
$ nc -U ./cmd.sock
x=42
+ x=42
^C

$ nc -U ./cmd.sock
echo $x
+ echo $x
42
```

iosock itself exits when the child process does, or you send iosock a signal
that terminates it. It monitors SIGINT and SIGTERM and triggers a SIGTERM in
the child when receiving either of those signals. 

This makes it suitable for use as PID 1 within a container. `podman stop` will
cleanly shutdown whatever child process iosock is running:

```sh
exec /iosock /volume/cmd.sock /the/real/command
```

## Pseudo-terminals

iosock has very basic support for running its child process in a
pseudo-terminal. This is sufficient only for the most basic tools that use ptys
for line editing (`bash`, `python3` and other REPLs, etc) and not much else.

To use it, provide `-p` or `--pty` as the first argument.

``` sh
$ iosock ./cmd.sock bash
```

If you want to use iosock for controlling more complex applications, look into
`dtach` and related session managers. These tools implement terminal-aware
client programs that do a much better job of synchronizing the state of the pty
and your actual terminal (ensuring raw input, disabling echo, resizing, ...).
iosock is intended to be used with `nc -U` or another dumb client, which is
unaware of any of these terminal settings. 

# Limitations

## Multiple Connections

Only one connection is permitted at a time. Any connection received while
another connection is process will immediately be closed.
