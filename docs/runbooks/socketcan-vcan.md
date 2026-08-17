# SocketCAN vcan qualification

Run this only on Linux with SocketCAN support and privileges to manage a temporary `vcan` link.
Create one isolated virtual interface using the host's documented network tooling, then run the
ignored SocketCAN adapter test against that interface. Record the kernel version, interface name,
bitrate/mode, test command, and cleanup result. Remove the temporary interface immediately after
the test, including on failure.

The library and broker never create privileged links themselves. Physical interface testing also
requires explicit ownership, a known peer, and a restoration check for link state and configuration.
