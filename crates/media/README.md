# seedwell-media

Images in chat (SPEC §7G.7 *Attachments*): what an image a message carries may be, made safe to send and to show.
No I/O and no networking; `seedwell-sync-net` stores and moves the bytes.

* `sniff`: PNG, JPEG, GIF, WebP or SVG, found from the bytes alone (never the extension). HEIC is refused with a
  sentence that says what to do.
* `prepare` (sending): caps (10 MiB raster, 1 MiB SVG, 64 megapixels, 16 384 pixels a side), metadata stripped
  without re-encoding by an allow-list walk of each container (PNG chunks, JPEG segments, GIF blocks, WebP chunks: EXIF
  with GPS, XMP, IPTC, comments and trailing data go; colour profiles stay), a photo whose EXIF turns it turned and
  encoded again, a decode to prove it is an image, and a small JPEG preview (160 px, at most 12 KiB).
* `check` (receiving): the bytes are the kind their message says, within the caps, and decode.
* `render`: the picture a host draws, always PNG or JPEG and at most 4096 px a side. An SVG is drawn here with
  `resvg`: no scripts, no `foreignObject`, no files or URLs (`<image>` reads only `data:` raster images, never a
  nested SVG), no XML entities, at most 100 000 elements, 5 s at most. Hosts never see SVG markup.
* `thumb_png`: a message's preview (a JPEG from another device), checked and made a PNG.

Dependencies:

| Crate | Licence | Why |
|---|---|---|
| `image` (features `png`, `jpeg`, `gif`, `webp`) | MIT OR Apache-2.0 | decoding with limits (proof that bytes are an image, sizes, EXIF orientation), resizing, PNG and JPEG encoding for previews and views. Pulls `png`, `zune-jpeg`, `gif`, `image-webp` and their small helpers (MIT/Apache-2.0/Zlib/BSD). |
| `resvg` (features `text`, `system-fonts`, `raster-images`) | Apache-2.0 OR MIT | SVG drawn in Rust (`usvg` parses into a tree without scripts or foreign content, `tiny-skia` (BSD-3-Clause) draws). Text in SVGs uses the system's fonts (`fontdb`, MIT). No `svgz`: a gzipped SVG is refused. |
