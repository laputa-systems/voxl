# voxl-frame-encoder: BCTL-81 S1 helper

`voxl-frame-encoder` is Voxl's encoder core, extracted into the `voxl-encoder`
workspace crate (`encoder/`) and built as a separate helper binary. bctl's VM
supervisor spawns it on the first encoding-50 subscription, owns the Unix socket,
and stops it on the last. Capture, transport, audio, input and the web client stay
in Voxl and out of this crate.

The crate is MIT. It links x264, which is GPL-2.0-or-later (`x264.h:10-25`; Voxl's
`README.md:375-376` says a Voxl binary is GPL for the same reason), so **the helper
binary is GPL**. Any release that ships it must offer the helper's source together
with x264 and any x264 patches. The exchange below is plain frame rows and access
units; that is what lets the MIT supervisor and the GPL helper ship as an
aggregation. This is the licence reading recorded in the BCTL-81 research record,
not legal advice. H.264 patent licensing is not assessed.

## Build and run

On macOS with Homebrew x264, the crate builds against `/opt/homebrew` through
`pkg-config` and links the static `libx264.a`; it does not use `patches/x264`.

```sh
cargo build -p voxl-encoder --locked
./target/debug/voxl-frame-encoder /path/to/encoder.sock
```

The helper takes exactly one argument, the socket path. It removes a stale socket
at that path, binds it, and sets mode 0600. It accepts one connection at a time and
keeps accepting after a bad connection. Exit status: 0 after `Shutdown`, 2 for a
missing argument or a non-Unix build, 1 if the socket cannot be created or the
listener fails. Encode failures never exit the process; they are reported as an
`Error` message (see below).

The tests use `cargo test -p voxl-encoder`.

## Protocol

All integers are little-endian. The socket is a Unix stream socket. The path is
argv[1].

### Framing

Every message is a `u32` payload length followed by exactly that many payload
bytes. Payloads start with a one-byte message type.

- A length of 0, or more than 64 MiB (67,108,864), is a protocol error.
- An unknown message type is a protocol error.
- A payload shorter than its fields require, or with trailing bytes, is a
  protocol error.
- A truncated length prefix or a payload shorter than its declared length is a
  protocol error. EOF before a length prefix is an ordinary disconnect and
  produces no `Error`.

The protocol version is 1, carried in `Configure`. A mismatch is a configuration
error.

### Parent to helper

| Type | Name | Body after the type byte |
| --- | --- | --- |
| 1 | `Configure` | `version u16`, `width u32`, `height u32`, `stride u32`, `pixel_format u8`, `fps u32`, `crf f32`, `vbv_kbps u32` |
| 2 | `Frame` | `generation u64`, `full u8`, `rect_count u16`, `rect_count × {x u32, y u32, w u32, h u32}`, `y0 u32`, `y1 u32`, `row_bytes u32`, pixel bytes |
| 3 | `RequestKeyframe` | none |
| 4 | `Shutdown` | none |

`pixel_format` is 0 for BGRX. Width and height must be even and non-zero (I420).
Stride is in bytes; it must be at least `width * 4`. `crf` is an IEEE-754 `f32`
bit pattern and must be in 0..=51. `fps` must be non-zero. `vbv_kbps` is a cap in
kbit/s; 0 means uncapped. Configure exactly once per connection; a second
`Configure` is an error and closes the connection.

A `Frame` describes new pixels for one generation:

- `full = 1`: `rect_count` must be 0, `y0 = 0`, `y1 = height`,
  `row_bytes = stride`, and the pixel bytes must be exactly `stride * height`.
  Every macroblock is treated as changed.
- `full = 0`: `damage` lists the changed pixel rectangles. `w` and `h` are
  non-zero and the rectangle must lie inside the frame. `y0` and `y1` are
  even-aligned pixel rows within the frame, `row_bytes = stride`, and the pixel
  bytes must be exactly `(y1 - y0) * stride`. The rows must cover every macroblock
  row the damage rectangles touch, rounded outward to 16 pixels: a rectangle in
  rows 320..335 requires `y0 <= 320` and `y1 >= 336`. A frame with no damage
  rectangles must carry `y0 = y1` and no pixel bytes.

Generations must strictly increase across a connection; a repeated or older
generation is a protocol error. The helper handles one `Frame` at a time, in
order. The parent sends the next frame only after the access unit for the previous
one, or after deciding the previous frame yields none (see below).

`RequestKeyframe` applies to the next `Frame`, which is encoded as an IDR with all
macroblocks analysed. A request without a following frame produces nothing.

`Shutdown` closes the encoder, removes the socket and exits 0. No reply is sent.

### Helper to parent

| Type | Name | Body after the type byte |
| --- | --- | --- |
| 1 | `AccessUnit` | `generation u64`, `keyframe u8`, `len u32`, Annex-B bytes |
| 2 | `Error` | `code u16`, `message_len u16`, UTF-8 message bytes |

`AccessUnit` is emitted for a `Frame` that produced an encoder output. The first
access unit of a session is an IDR and carries SPS, PPS and an IDR slice (NAL
types 7, 8 and 5; an SEI may precede them). Subsequent units are one P slice,
always with start codes because Annex-B is enabled; with one thread and one
slice, x264 does not split a frame.
`keyframe` is 1 exactly for the units x264 reports as keyframes.

A `Frame` with no damage and no refinement due produces **no** access unit: there
is nothing to send. Such a frame is fire-and-forget; the helper answers only
frames that yield output.

`Error` codes:

| Code | Meaning |
| --- | --- |
| 1 | Protocol error: framing, message fields, generation order, damage coverage, or a message before `Configure` |
| 2 | Configuration error: protocol version, geometry, stride, CRF, fps, or x264 refusing to open |
| 3 | Encode error: x264 failed while encoding |

After writing an `Error` the helper closes that connection; the process stays alive
and accepts the next connection. The parent decides whether to reconnect, respawn
or fall back to ZRLE.

### Lifecycle

1. The supervisor creates the socket path's directory, spawns the helper with the
   path, and waits for the socket to appear.
2. It connects, sends one `Configure`, then `Frame` messages.
3. It reads one `AccessUnit` per encoded frame. It may send `RequestKeyframe`
   before a frame (for a new viewer or after loss).
4. On the last subscription it sends `Shutdown` and reaps the process. A helper
   that has closed the connection after an `Error` can be replaced and respawned;
   nothing in the helper exits the process for an encode failure.

## x264 settings carried from Voxl

The wrapper is `encoder/src/x264_ffi.rs`; the line references name the upstream
source at the base revision.

| Setting | Value | Upstream |
| --- | --- | --- |
| Preset and tune | `ultrafast`, `zerolatency` | `src/lib.rs:150-153`, `src/encoder.rs:51-55` |
| Input | I420, fps numerator from `Configure`, denominator 1 | `src/encoder.rs:57-60` |
| Timebase | 1/1000, `b_vfr_input = 0` (VFR costs a frame of delay) | `src/encoder.rs:61-64` |
| Threads | 1, no sliced threads, one slice | `src/encoder.rs:65-67`; one per stripe in `src/video.rs:194-205` |
| Keyframes | `i_keyint_max = X264_KEYINT_MAX_INFINITE`, `i_keyint_min = 1`, scenecut 0, no intra refresh | `src/encoder.rs:68-71` |
| B-frames | none | `src/encoder.rs:72` |
| Bitstream | Annex-B, repeat headers, no AUD | `src/encoder.rs:73-75` |
| Rate control | CRF 24 (`Configure.crf`), no lookahead, no mb-tree, sync lookahead 0 | `src/encoder.rs:84-86`, `:94-95`, `src/lib.rs:164` |
| Motion search | UMH, range 64 | `src/encoder.rs:90-91` |
| Damage hints | `analyse.b_mb_info = 1` | `src/encoder.rs:92-93` |
| VBV | `vbv_kbps` cap, buffer at ~1.25 frames (`(max * 5) / (4 * fps)`, rounded up) | `src/encoder.rs:96-99`, `:217-219` |
| Colour | BT.709 primaries, sRGB transfer, BT.601 matrix, limited range | `src/encoder.rs:101-106` |
| Profile | high | `src/encoder.rs:107-108` |
| Keyframe squeeze | 4× cap or 16 Mbps, whichever is larger; 8 frames of buffer; steady-state VBV restored on the next frame | `src/video.rs:1062-1077` |
| Paint-over quality | CRF 16 | `src/lib.rs:167`, `src/video.rs:224`, `:1217` |
| Motion quality | `MotionQuality` takes x264's complexity estimate back off cheap motion frames, at most 12 QP steps, when recent motion costs under 40 bits per changed macroblock | `src/video.rs:953-1023` |

`VOXL_X264_INFO` and `VOXL_DEBUG_MARKER` are not carried; the helper logs at
x264's warning level and reports nothing else.

## Damage map and partial conversion

The damage path is the one in `src/capture.rs` and `src/video.rs`:

- A damage rectangle rounds outward to whole macroblocks: columns
  `x / 16 .. ceil((x + w) / 16)`, rows likewise (`src/capture.rs:241-244`).
- The macroblock map is one byte per 16×16 block, row-major. Damaged blocks are 1.
- For x264, the map is inverted: unchanged blocks become
  `X264_MBINFO_CONSTANT` (1), changed blocks 0 (`src/video.rs:1089-1096`).
  `encoder/src/damage.rs` carries the rounding, the inverted map and the row
  band a damage set requires.

Only the damaged macroblock rows are converted. Voxl takes the union of pending
and newly damaged rows and calls the converter on that band only
(`src/video.rs:715-725`); the remaining I420 rows keep the previous frame's
pixels, which is why the macroblock map must mark them constant. This crate does
the same in `Session::update_pixels`, converting `Frame::pixels` rows
`y0..y1` and reusing its `I420Frame` buffer.

The conversion itself is BGRX to I420 at BT.601 limited range, matching libyuv's
`ARGBToI420` coefficients used by Voxl on Linux and Windows (`src/convert.rs:1-19`,
`:82-118`). macOS has no libyuv and Voxl's macOS capture is NV12
(`src/convert.rs:123-177`), which does not apply to bctl's BGRX frames, so
`encoder/src/convert.rs` implements the integer BGRX path in Rust and averages
each 2×2 block for U and V. A band converted alone produces the same bytes as the
same rows of a full conversion (tested).

## Settle refinement

`Refiner` from `src/video.rs:422-518` is carried in `encoder/src/settle.rs` with
its constants: a block is still after 300 ms (`REFINE_STILL_MS`,
`src/video.rs:430`) and refinements run at most every 250 ms (`REFINE_EVERY`,
`src/video.rs:431`). A damaged block becomes pending; once it has been still long
enough, `due` returns the still blocks as a map and their row band. The session
then encodes the current picture again at CRF 16 with only those blocks analysed;
everything else stays constant. This is the paint-over in `src/video.rs:735-784`.
`MotionQuality` is carried alongside it, including its handling of the frame's
starting QP and VBV-raised rows (`src/video.rs:1007-1023`).

A frame with no damage is answered only when a refinement is due; otherwise it
produces no access unit.

## Left out

Capture (`src/capture.rs`, `src/mac/*`, `src/win/*`, `src/screen.rs`,
`src/corners.rs`), hub and server (`src/hub.rs`, `src/server.rs`), TLS and login
(`src/tls.rs`, `src/login.rs`, `src/access.rs`), input and desktop helpers
(`src/input.rs`, `src/desktop.rs`, `src/files.rs`), audio (`src/audio.rs`,
`src/mic.rs`) and the web client (`web/`) stay in Voxl. Stripes and
`ThreadGovernor` are dropped for the spike, as the BCTL-81 component map says;
one x264 thread is the starting point. `RateCtl` and `flow_ok` are replaced by
the primary viewer's request pacing. `src/process::exit` calls on capture and
encoder failure (`src/video.rs:97`, `:110`, `:124`) are not carried: the helper
reports over the socket and the parent decides.

## Acceptance record

`cargo test -p voxl-encoder` covers:

- the first access unit is an IDR in Annex-B (SPS/PPS/IDR, checked by parsing NAL
  types, no decoder involved);
- a 1280×800 frame whose damage is one 16-pixel macroblock row produces an access
  unit far smaller than a full-frame change (asserted ratio, printed numbers);
- an unchanged frame with no damage produces no output;
- the macroblock map marks unchanged blocks with `X264_MBINFO_CONSTANT` and
  rounds damage outward to macroblock bounds;
- a paint-over frame at CRF 16 after motion settles;
- malformed, unknown and truncated messages produce an `Error` and do not end the
  process;
- the helper binary, accepting the protocol above, encodes 30 synthetic
  1280×800 BGRX frames and returns 30 access units.
