# Server Wire Protocol

This documents the protocol used to communicate between the client and server
of Ensync. This is mainly useful if you want to write some other server
backend.

## Conventions

MUST, MAY, and so forth follow the usual meaning.

## Transport

No underlying transport protocol is specified, though it must inherently be an
ordered byte stream. No encryption or authentication is included, as this is is
left to the underlying transport. Similarly, there are no defined keep-alives
or timeouts here.

## Protocol

The server and the client send streams of fourleaf values conforming to the
`Response` and `Request` enums defined in `rpc.rs`, respectively.

Depending on the type, each client frame may require the server to send one or
more response frames. The server MUST NOT send unsolicited frames, except as
enabled by specific requests and that it MAY send an unsolicited `FatalError`
response immediately before terminating communication to indicate a fatal
error, since `FatalError` is an acceptable response for whatever frame the
client may later send.

The client MAY continue sending frames even when the most recent frame is still
pending a response from the server. The server MUST send responses in the same
order as their corresponding requests. The server MAY process requests in a
different order than they are received, except that requests involving a single
transaction may not be reordered with each other, and requests which read items
MUST NOT be ordered before a `Commit` which was received first if this would
produce an observable difference.

## Semantics

Each request type (other than `ClientInfo`) corresponds to exactly one method
in the `Storage` trait; the exact semantics are documented there.
