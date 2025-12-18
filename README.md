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

# Limitations

## Pseudo-terminals

There is no PTY support, which means that many interactive programs cannot be
used with this tool, and those that can may offer degraded functionality (like
lack of tab completion). This is intentional: if you want a fully-featured 
remote console and don't mind using a specialized client, `sshd` or `telnetd`
do a better job. iosock is meant to be small with a minimum of dependencies.

## Multiple Connections

Only one connection is permitted at a time. Any connection received while
another connection is process will immediately be closed.
