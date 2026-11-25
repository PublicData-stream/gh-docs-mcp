# 0005 — Authenticate private control messages

Use the Git-specific HTTP/1.1 envelope with full HMAC-SHA-384 over canonical
metadata and the exact body. Provision independent keys for feed-to-sync,
sync-to-serve, and cursor signing. Give each receiver a random boot epoch,
a bounded nonce cache, and a bounded previous-key overlap. Authenticate
acknowledgments against the original request nonce.

Sync owns durable hint receipts and publication delivery state. Serve uses a
read-only lookup that compares the event with the committed publication.
The Compose profile binds each receiver to its own private interface and
uses host traffic filtering to restrict its peer. Cross-host control
requires encrypted transport.

[Protocol, provisioning and rotation](../control.md)
