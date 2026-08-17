# PCAN loopback qualification

Install the vendor-supported PCAN-Basic runtime on Linux or Windows and connect a known PCAN device
to an isolated loopback bus. Record the driver/runtime version, channel, nominal and data bitrate,
FD mode, payload sizes, disconnect and bus-off observations, and cleanup result. Do not record
serial numbers, raw payloads, access tokens, or credentials in acceptance evidence.

Invoke physical tests explicitly; they remain excluded from the default suite. Confirm that closing
the session restores any temporary configuration and that a stale lease cannot send after reuse.
