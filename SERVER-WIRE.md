# Server Wire Protocol

This documents the protocol used to communicate between the client and server
of Ensync. This is mainly useful if you want to write some other server
backend.

## Conventions

MUST, MAY, and so forth follow the usual meaning.

All integers are written in little-endian order. Integer types are named using
their Rust names.

## Transport

No underlying transport protocol is specified, though it must inherently be an
ordered byte stream. No encryption or authentication is included, as this is is
left to the underlying transport. Similarly, there are no defined keep-alives
or timeouts here.

## Protocol

The client and server streams are both separated into frames. Each frame begins
with a `u32` indicating the number of bytes following the frame header. The
frame payload follows; it is a CBOR object corresponding to a
`serde_types::rpc::Request` or `serde_types::rpc::Response`; the exact CBOR
format is simply however Serde causes that enumeration to be serialised.

Depending on the type, each client frame may require the server to send one or
more response frames. The server MUST NOT send unsolicited frames, except that
it MAY send an unsolicited `Error` response immediately before terminating
communication to indicate a fatal error, since `Error` is an acceptable
response for whatever frame the client may later send.

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
