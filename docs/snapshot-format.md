# Snapshot envelope format

RustHouse snapshot payloads use a versioned binary envelope. The envelope is a
corruption and resource boundary only: catalog serialization and atomic file
replacement are deliberately outside this format.

## Version 1 layout

All integers are unsigned and little-endian. Offsets are measured in bytes.

| Offset | Size | Field | Value |
| ---: | ---: | --- | --- |
| 0 | 8 | Magic | ASCII `RHOUSESN` |
| 8 | 2 | Version | `1` (`u16`) |
| 10 | 8 | Payload length | Number of payload bytes (`u64`) |
| 18 | 4 | Checksum | CRC-32/ISO-HDLC of the payload (`u32`) |
| 22 | Declared length | Payload | Opaque bytes |

The CRC parameters are polynomial `0x04C11DB7` (reflected representation
`0xEDB88320`), initial value `0xFFFFFFFF`, reflected input and output, and final
XOR `0xFFFFFFFF`. The standard check value for ASCII `123456789` is
`0xCBF43926`.

## Validation

Callers configure an inclusive maximum payload length. Encoding rejects a
payload above that bound. Decoding validates, in order:

1. the complete 22-byte header is present;
2. magic and version are supported;
3. the declared length is representable and within the configured bound;
4. the input length exactly matches the declared envelope length; and
5. the payload checksum matches.

Short input, trailing input, incompatible formats, unsupported versions,
oversized declarations, and checksum mismatches produce distinct typed errors.
The decoder borrows the payload from the input and performs no allocation.
