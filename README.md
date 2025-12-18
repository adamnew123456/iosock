# What is this?

**iosock** is a simple wrapper that connects a program's stdio to a Unix domain
socket server. When the socket is disconnected, the program receives no input
via stdin and its stdout and stderr go to iosocks' outputs. When the socket is
connected, data from the socket is passed to the process's stdin and its stdout
and stderr are mirrored to the socket (they still go to iosock's outputs as
well).

# Why write this?

The use case is for allowing easy access to containerized servers that expose a
command line. For reasons unknown, `podman attach` does not work reliably for
me when running the container as a quadlet (probably having to do with having
no tty). Having this tool lets me export a Unix socket via a bind mount and
then connect to the admin interface via netcat.
