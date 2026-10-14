# Native `.irimg` image format, version 1

This reference describes the bytes written by the current `irimg` crate. It is an implementation reference for format version 1, not a promise of compatibility with future versions. Macrium `.mrimgx` has a separate reader and is not this format.

Sources: [format.rs](../crates/irimg/src/format.rs), [manifest.rs](../crates/irimg/src/manifest.rs), [crypto.rs](../crates/irimg/src/crypto.rs), [codec.rs](../crates/irimg/src/codec.rs), [split.rs](../crates/irimg/src/split.rs), [writer.rs](../crates/irimg/src/writer.rs), [index.rs](../crates/irimg/src/index.rs), [reader.rs](../crates/irimg/src/reader.rs), and [lib.rs](../crates/irimg/src/lib.rs).

## Conventions and file set

All binary integers are little-endian. `u8`, `u32` and `u64` mean unsigned integers of the indicated width; `i64` is signed. Offsets in tables are zero-based byte offsets relative to the structure being described. UUIDs in binary structures are the 16 bytes returned by Rust `Uuid::as_bytes()`, not Windows GUID mixed-endian encoding. JSON UUIDs use the serde UUID string representation.

One image consists of consecutive files sharing a base name and image UUID:

```text
name.irimg  (file_no = 0)
name.i01    (file_no = 1)
name.i02    (file_no = 2)
...
name.i100   (file_no = 100; two digits is a minimum width)
```

Every file begins with a 4096-byte header. Data block payloads follow back-to-back without per-block headers, alignment padding or separators. The final file additionally carries the stored metadata blob and a 128-byte footer at its very end:

```text
First/intermediate file: [4096-byte header] [block payloads ...]
Final file:              [4096-byte header] [block payloads ...] [metadata] [128-byte footer]
```

A single-file image uses the final-file layout in `name.irimg`. Only the final file has a footer. An interrupted, unfinished image generally lacks the final metadata/footer and is not a normal readable image.

The default logical image block size is 1 MiB (1,048,576 bytes). `ImageWriter::create` accepts powers of two from 256 KiB through 4 MiB inclusive. This is distinct from a source disk's logical sector size.

## File header: 4096 bytes

The writer initializes the entire header to zero and fills the following fields:

| Offset | Bytes | Type | Meaning |
| --- | --- | --- | --- |
| 0 | 8 | bytes | ASCII `INOCRIUM`. |
| 8 | 4 | u32 | Format version, currently `1`. |
| 12 | 4 | u32 | Header size, `4096`. |
| 16 | 4 | u32 | Header flags: bit 0 encrypted, bit 1 data compression enabled. |
| 20 | 4 | u32 | Split file number, starting at `0`. |
| 24 | 16 | UUID bytes | Image UUID shared by all files. |
| 40 | 4 | u32 | Logical image block size in bytes. |
| 44 | 4 | u32 | KDF identifier: `1` for Argon2id on encrypted images; zero otherwise. |
| 48 | 4 | u32 | Argon2 memory cost in KiB. |
| 52 | 4 | u32 | Argon2 iteration count. |
| 56 | 4 | u32 | Argon2 parallelism. |
| 60 | 32 | bytes | Password KDF salt. |
| 92 | 12 | bytes | Random AES-GCM nonce used to wrap the data key. |
| 104 | 48 | bytes | Wrapped 32-byte data key: ciphertext followed by a 16-byte GCM tag. |
| 152 | 8 | i64 | Creation time in Unix seconds. |
| 160 | 3904 | bytes | Reserved; written as zeros. |
| 4064 | 32 | bytes | Full BLAKE3 digest of header bytes `[0, 4064)`. |

Fields 44 through 151 are zero for unencrypted images. Encrypted split files repeat the same key slot; the file number and header digest differ. The image UUID is randomly generated (UUID v4).

The decoder checks header magic, format version, header-size field and digest. For encrypted images it requires KDF identifier `1`. The digest is unkeyed corruption detection; it is not a digital signature or MAC. The compression flag indicates that block compression was enabled, not that every block is compressed, and does not control metadata decompression.

## Data payloads and streams

The manifest groups content into disks and streams. The engine uses one stream per captured partition and additional streams for forensic gaps. Logical block `n` begins at byte `n * block_size` within its stream. The final block of a stream may be shorter than `block_size`.

The writer accepts already encoded blocks in any order; their physical order does not have to match disk/stream/block order. Each payload's file, offset, stored length, plaintext length, flags, nonce counter and plaintext hash are described only by the final index.

Encoding proceeds as follows:

1. Compute BLAKE3 over the plaintext block and retain its first 16 bytes.
2. If the configured compression level is positive, try independent zstd compression. Use it only if it succeeds and produces fewer bytes than the plaintext; otherwise store the plaintext.
3. If encrypted, AES-256-GCM seal that result and append its 16-byte tag.
4. Write those bytes and record an index entry.

Default data compression level is 3; level 0 disables data compression. The encoder treats nonpositive levels as disabled. Per-entry flags determine whether the reader decrypts and/or decompresses an individual block.

The engine omits blocks that are entirely zero and blocks outside Smart-mode extents. It zeroes unselected ranges within selected Smart blocks before encoding. `ImageWriter` itself does not eliminate zero blocks passed to it. `read_block` returns `None` for an absent entry, and `read_stream_at` reconstructs absent ranges and any missing tail of a short block as zeros. An omitted range is not evidence of captured nonzero data.

## Stored metadata

On finish, the writer assembles the plaintext metadata described below, zstd-compresses it at level **3 regardless of the data compression setting**, and optionally encrypts it with the image data key. The footer hashes the stored bytes, including the GCM tag when encrypted.

Metadata is one contiguous blob in the final file. There is no independent on-disk metadata-length prefix: its offset and lengths come from the footer.

### Plaintext section framing

The plaintext begins with the eight ASCII bytes `IRMETA01`. It then contains sections with no padding:

```text
u32 tag
u64 payload_length
payload_length bytes of payload
```

The writer emits these sections in order:

| Tag | Payload |
| --- | --- |
| 1 | Compact UTF-8 JSON manifest produced by `serde_json::to_vec`. |
| 2 | Binary raw disk regions. |
| 3 | Binary block index. |

There is no section-count field or terminator; parsing ends at the decompressed blob's end. The reader skips unknown tags by length. It requires a manifest and index; absent raw-region metadata yields an empty list. The current writer always emits all three sections.

### Manifest JSON (tag 1)

Manifest field names use their Rust snake_case spelling. Strings and numeric/boolean values are ordinary JSON; optional values serialize as `null` when absent. Defaults on selected fields allow the reader to accept omitted fields; the writer serializes the defined fields.

| Object | Fields |
| --- | --- |
| Manifest | `format_version`, `image_id`, `created`, `comment`, `tool_version`, `source`, `block_size`, `compression`, `encryption`, `definition_id`, `disks`. |
| `source` | `hostname`, `os_version`. |
| `compression` | `algorithm` (documented as `zstd` or `none`), `level` (signed integer). |
| `encryption` | `null`, or `{ "cipher": "aes-256-gcm", "kdf": "argon2id" }` for engine-produced encrypted images. |
| Disk | `index`, `source_number`, `model`, `serial`, `size`, `sector_size`, `physical_sector_size`, `layout`, `partitions`, `gaps`, `raw_regions`. |
| Partition | `number`, `stream`, `offset`, `length`, `captured`, `capture`, `gpt`, `mbr`, `filesystem`, `label`, `drive_letter`, `used_bytes`, `stored_bytes`, `is_system`, `is_boot`. |
| Partition `gpt` | `null`, or `type_id`, `id`, `name`, `attributes`. |
| Partition `mbr` | `null`, or `partition_type`, `bootable`, `logical`. |
| Gap | `stream`, `offset`, `length`. |
| Raw-region descriptor | `offset`, `length`, `kind`. |

`created` is an RFC 3339 backup-start timestamp; the header separately stores Unix seconds. `definition_id` is an optional string. `source_number` is the source machine's `PhysicalDriveN` number; `index` addresses a disk within this image. Disk sizes, partition/gap offsets and lengths are bytes; sector sizes and `block_size` are also bytes. GPT usable bounds below are LBAs. `stored_bytes` is the sum of plaintext bytes stored for a partition, not compressed on-disk size. Partition `number` is Windows' one-based number. Unselected partitions remain in the manifest with `captured = false` so a layout can be reproduced; their `stream` does not establish captured content.

`layout` is an internally tagged object:

```json
{ "style": "mbr", "signature": 123 }
{ "style": "gpt", "disk_id": "<uuid>", "first_usable_lba": 34, "last_usable_lba": 999966, "max_partition_count": 128 }
{ "style": "raw" }
```

Partition `capture` is `"smart"` or `"forensic"`. Raw-region `kind` is `"track0"`, `"gpt_backup"`, `"ebr"` or `"other"`. `drive_letter` is a one-character string or `null`.

The current engine writes `compression.algorithm = "zstd"` even when the data compression level is 0. Actual block encoding is determined by index flags, and metadata always remains zstd-compressed.

### Raw disk regions (tag 2)

The payload begins with `u32 region_count`, followed by that many records:

| Record offset | Bytes | Type | Meaning |
| --- | --- | --- | --- |
| 0 | 4 | u32 | Image disk index. |
| 4 | 8 | u64 | Byte offset on the original disk. |
| 12 | 8 | u64 | Byte length of the following data. |
| 20 | length | bytes | Original disk bytes. |

Records are packed consecutively. Region kind is recorded in the manifest, not this binary record. These regions hold partition-table/boot material such as TRACK0, the GPT backup area and extended boot records. They are protected as part of the metadata blob, rather than indexed as ordinary data blocks.

### Block index (tag 3)

The index begins with `u32 stream_count`. For each stream it writes this 16-byte header, immediately followed by `entry_count` entries:

| Stream-header offset | Bytes | Type | Meaning |
| --- | --- | --- | --- |
| 0 | 4 | u32 | Image disk index. |
| 4 | 4 | u32 | Stream ID. |
| 8 | 8 | u64 | Entry count. |

Streams are emitted in ascending `(disk, stream)` order from a `BTreeMap`. Entries within a stream are sorted strictly by logical block number; gaps between block numbers are allowed, duplicates are not. The writer's index insertion replaces an earlier entry for the same block.

Each index entry is exactly 56 bytes:

| Entry offset | Bytes | Type | Meaning |
| --- | --- | --- | --- |
| 0 | 8 | u64 | Logical block number within the stream. |
| 8 | 4 | u32 | Split file number containing its payload. |
| 12 | 1 | u8 | Entry flags: bit 0 zstd-compressed; bit 1 AES-GCM-encrypted. |
| 13 | 3 | bytes | Reserved; written as zeros. |
| 16 | 8 | u64 | Payload byte offset from the start of that split file. |
| 24 | 4 | u32 | Stored payload length, including any encryption tag. |
| 28 | 4 | u32 | Plaintext block length. |
| 32 | 8 | u64 | AES-GCM nonce counter (allocated even for unencrypted blocks). |
| 40 | 16 | bytes | First 16 bytes of BLAKE3 over plaintext. |

The encoded index size is `4 + sum(16 + 56 * entry_count)` bytes over all streams. Header flags and entry flags have different bit assignments: header bit 0 is encryption, while entry bit 0 is compression.

## Footer: 128 bytes

The footer occupies the final 128 bytes of the final split file. It is zero-initialized before the following fields are filled:

| Offset | Bytes | Type | Meaning |
| --- | --- | --- | --- |
| 0 | 8 | bytes | ASCII `INOCFOOT`. |
| 8 | 4 | u32 | Format version, `1`. |
| 12 | 4 | u32 | Total number of image files, including `.irimg`. |
| 16 | 16 | UUID bytes | Image UUID. |
| 32 | 8 | u64 | Metadata byte offset within the final file. |
| 40 | 8 | u64 | Stored metadata byte length. |
| 48 | 8 | u64 | Metadata length after decryption and decompression. |
| 56 | 8 | u64 | Metadata nonce counter. |
| 64 | 32 | bytes | Full BLAKE3 digest of stored metadata bytes. |
| 96 | 24 | bytes | Reserved; written as zeros. |
| 120 | 8 | bytes | ASCII `INOCEND` followed by a NUL byte (`INOCEND\0`). |

For writer-produced files, `metadata_offset + metadata_len + 128` equals the final file's length. There is no footer checksum or authentication tag of its own. The footer's metadata digest is unkeyed; encrypted metadata additionally has its AES-GCM authentication tag.

The reader walks parts from file 0, checking each header's file number and image UUID, and tries to decode a footer at each file's end. It accepts a final footer only when its UUID matches and its `total_files` equals the number opened. Missing required parts, unsupported versions, invalid headers or an absent/invalid final footer cause an error.

## Encryption

Encryption uses a random 32-byte data encryption key (DEK) for all data blocks and metadata. A 32-byte key encryption key (KEK) is derived from the UTF-8 password with Argon2id version `0x13` and a random 32-byte salt. The defaults are:

- Memory: 65,536 KiB (64 MiB).
- Iterations: 3.
- Parallelism: 4.
- Derived-key length: 32 bytes.

These costs are stored in every encrypted file header; readers use the stored parameters. Tests may use deliberately weak parameters, which are not the production defaults.

The DEK is wrapped using AES-256-GCM with the KEK, a random 12-byte wrap nonce, and the image UUID bytes as additional authenticated data (AAD). The 48-byte wrapped result is ciphertext plus tag. The current wrapping design allows replacement of key slots without re-encrypting payloads, but no password-change application workflow is claimed here.

Block/metadata nonces are exactly:

```text
u32 domain (little-endian) || u64 counter (little-endian)
```

Domain 0 is data; domain 1 is metadata. A shared atomic counter begins at 0, advances for each encoded block (even when unencrypted), and advances again for metadata. Counter order follows encoding, not necessarily physical write order.

Data-block AAD is 32 bytes:

```text
16-byte image UUID || u32 disk || u32 stream || u64 block
```

All numeric AAD fields are little-endian. This binds encrypted data to its logical position. Metadata AAD is only the 16-byte image UUID. Metadata is compressed before encryption; block compression, when used, also precedes encryption. Neither has a separate nonce prefix in its stored payload: counters come from the index/footer and domains are implied.

On resume, the writer starts the counter at `checkpoint.nonce_counter + 2^40` plus a random 56-bit offset, which skips counters consumed after the saved checkpoint. It refuses a start that leaves less than 2^48 counters before the counter would wrap. Before anything is sealed, the engine durably replaces the `.irimg.ckpt` with one holding the new counter, so a resume that dies before its first periodic checkpoint cannot make the next resume reuse its nonces. Later split parts are removed and the saved file is truncated to the checkpoint offset before writing continues.

Headers and footers are not encrypted. They reveal format version, flags, UUID, block size, creation time, KDF/key-slot parameters, file numbering and metadata location/lengths. An encrypted image's comments, source information, disk layout, raw disk regions and block index are inside encrypted metadata.

## Splitting and checkpoints

Before appending a data block, if it would exceed the split limit and the current file already contains data, the writer starts the next file with a new header. A block never straddles files. Creation rejects split limits smaller than `4096 + 2 * block_size`.

**The split size is not a strict final-file cap.** `finish` appends metadata and the footer to the current file without starting another part or checking the split limit. That last file can exceed the requested limit. Earlier parts have no footer or padding.

The engine's checkpoint sidecar is `<image>.irimg.ckpt`, separate from the completed image format. It is JSON with `image_id`, `file_no`, `file_offset`, `nonce_counter`, `block_size`, `compression_level`, `split_size`, `index` and `user`. `index` is the binary block index encoded as lowercase hexadecimal. The engine writes `user` as `{ "engine": { "created": "<RFC 3339 timestamp>", "disks": [...] }, "caller": <caller-supplied JSON> }`; its disk objects use the manifest schema above. Caller state can hold snapshot identities for recovery.

The writer does not put encryption keys or passwords in checkpoint state; callers must keep secrets out of their arbitrary `user`/`caller` JSON. **The sidecar is not encrypted**, even when image encryption is enabled: it exposes the saved index, plaintext block hashes and engine disk metadata. Protect checkpoint access along with the image destination. An encrypted resume needs the password again. Checkpoint writing syncs the image data before returning state; engine checkpoints default to a 30-second interval and are also written on cancellation. Successful finish attempts to remove the sidecar and reports a warning if removal fails.

A recoverable Windows backup also needs the original persistent VSS snapshot. The service keeps that snapshot after a cancelled or interrupted definition backup and resumes from the saved caller state (definition and snapshot-set ids). Direct CLI backups currently use nonpersistent snapshots, so a checkpoint from that path does not provide automatic restart recovery.

Opening via any split-part path resolves the base and starts at `.irimg`. File enumeration is consecutive and stops at the first missing part; preserve all parts together when moving or copying a backup.

## Reading and verification

Opening checks header hashes, locates the footer, reads and hashes stored metadata, decrypts it if necessary, decompresses it, parses sections, and checks the manifest UUID against the header. It does not eagerly read every data block.

Reading a stored block retrieves the exact indexed byte range, authenticates/decrypts when flagged, decompresses when flagged, checks the decoded length, and compares the first 16 bytes of the plaintext BLAKE3 digest. Native verification iterates all indexed blocks and reports corrupt blocks; metadata is checked during image opening. There is no single hash over every byte of the complete file set, and unkeyed BLAKE3 checks are not proof of image provenance.

Verification establishes integrity of the represented content. It does not validate that uncaptured partitions contain data, that live unsupported-volume reads were consistent, or that a restored operating system will boot.
