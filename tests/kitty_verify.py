#!/usr/bin/env python3
"""Offscreen verification of cmdash's outer Kitty-graphics stream against a
REAL Kitty terminal emulator (the compiled C code, not our headless model).

It drives the same bytes cmdash emits for a scroll-move and asserts, against
Kitty's own `grman`/`update_layers`, that:

  * re-placing a placement with the same `p=` id MOVES it (no ghost at the
    old cells) instead of stacking a duplicate;
  * a placement-scoped delete `d=i,i=X,p=P` removes exactly that placement
    while the image data survives for its other placements;
  * lowercase `d=i` releases placements but RETAINS the image data (the
    protocol's re-display-without-retransmission contract), while uppercase
    `d=I` frees the data too;
  * text scrolling pushes a placement into history where it appears exactly
    once at any view depth and is reachable by scrolling back; a live-view /
    history-view / live-view / history-view round trip re-enters with the same
    Kitty placement identity;
  * Unicode-placeholder mode (the exact `write_placeholder_upload` /
    `write_placeholder_cells` / `clear_placeholder_cells` byte streams): the
    24-bit-color lower id, the 0-based row/col combining marks, and the
    1-based high-8-bits mark all decode to the right image and source rect,
    moves leave no ghost, and erasing the cells removes the cell images;
  * tmux-passthrough framing (`\x1bPtmux;` with doubled ESC bytes) is a
    lossless encoding of the direct-mode stream, and the unwrapped bytes
    drive real Kitty to the same end state;
  * animation frame composition (`a=f` deltas, `a=c` full/partial rects and
    alpha blending) and animated-GIF playback: `image_for_client_id` exposes
    Kitty's coalesced frame pixels, so the composed frame data is compared
    byte-for-byte against cmdash's `coalesce_frame`/`compose_animation_frame`
    semantics (alpha blending may differ by 1 in a low byte: Kitty blends in
    float and truncates, cmdash blends in integer math).

Prerequisites: a kitty installation whose Python package is importable (the
`kitty.fast_data_types` C extension). Run from the repo root with either the
system Python (when kitty's package is on the import path, e.g. Arch's
`/usr/lib/kitty`):

    python3 tests/kitty_verify.py

or with kitty's embedded Python, which works for any kitty install (including
the standalone kitty.app bundle used in CI):

    kitty +runpy "exec(open('tests/kitty_verify.py').read(), {'__name__': '__main__'})"

No display server is needed: the Screen is driven offscreen via the same
`test_create_write_buffer`/`test_parse_written_data` hooks Kitty's own test
suite uses.
"""
import sys

try:
    from kitty.fast_data_types import Screen  # noqa: E402
except ImportError:
    # Arch/pacman keeps the kitty package outside the default sys.path.
    # `kitty +runpy` (used in CI) already has it importable, so this fallback
    # only fires when running under a system Python.
    sys.path.insert(0, '/usr/lib/kitty')
    from kitty.fast_data_types import Screen  # noqa: E402


class Callbacks:
    """Minimal callback object for an offscreen Screen (mirrors kitty's own
    kitty_tests/__init__.py Callbacks)."""

    def __init__(self):
        self.clear()

    def clear(self):
        self.wtcbuf = b''
        self.iconbuf = self.colorbuf = self.ctbuf = ''
        self.titlebuf = []
        self.printbuf = []
        self.color_control_responses = []
        self.notifications = []
        self.open_urls = []
        self.cc_buf = []
        self.bell_count = 0
        self.num_of_resize_events = 0

    def write(self, data):
        self.wtcbuf += bytes(data)

    def notify_child_of_resize(self):
        self.num_of_resize_events += 1

    def on_reset(self, is_hard_reset=True):
        pass

    def color_control(self, code, data):
        pass

    def title_changed(self, data, is_base64=False):
        self.titlebuf.append(data)

    def osc_context(self, data):
        pass

    def icon_changed(self, data):
        self.iconbuf += str(data, 'utf-8')

    def set_dynamic_color(self, code, data=''):
        pass

    def set_color_table_color(self, code, data=''):
        pass

    def color_profile_popped(self, x):
        pass

    def cmd_output_marking(self, is_start, data=''):
        pass

    def request_capabilities(self, q):
        pass

    def desktop_notify(self, osc_code, raw_data):
        pass

    def open_url(self, url, hyperlink_id):
        pass

    def clipboard_control(self, data, is_partial=False):
        pass

    def on_bell(self):
        self.bell_count += 1

    def on_da1(self):
        pass

    def on_activity_since_last_focus(self):
        pass

    def finish_scroll_animation(self):
        pass

    def on_mouse_event(self, event):
        return False


def parse_bytes(screen, data):
    data = memoryview(data)
    while data:
        dest = screen.test_create_write_buffer()
        s = screen.test_commit_write_buffer(data, dest)
        data = data[s:]
        screen.test_parse_written_data(None)


def create_screen(cols, lines, scrollback=100, cw=10, ch=20):
    c = Callbacks()
    s = Screen(c, lines, cols, scrollback, cw, ch, 0, c)
    c.color_profile = s.color_profile
    return s, c


def layers(screen, scrolled_by=0):
    dx, dy = 2 / screen.columns, 2 / screen.lines
    return screen.grman.update_layers(
        scrolled_by, -1, 1, dx, dy, screen.columns, screen.lines, 10, 20
    )


def describe_layers(ls):
    out = []
    for lyr in ls:
        d = lyr['dest_rect']
        out.append(
            f"image_id={lyr['image_id']} ref_id={lyr['ref_id']} "
            f"z={lyr.get('z_index')} dest=({d['left']:.2f},{d['top']:.2f},"
            f"{d['right']:.2f},{d['bottom']:.2f})"
        )
    return out


def row_of(lyr, lines):
    """Convert a dest_rect top (normalized -1..1) into a 0-based screen row."""
    return int(((-lyr['dest_rect']['top'] + 1) / 2) * lines)


checks = []


def main():
    def check(name, cond, detail=''):
        checks.append((name, bool(cond), detail))
        print(('PASS' if cond else 'FAIL'), name, detail)

    # -----------------------------------------------------------------------
    # Scenario 1: the cmdash scroll-move stream. cmdash uploads with a stable
    # p=, then when a placement moves (scroll re-anchor) it re-places with the
    # SAME p= instead of deleting; the real Kitty must move the placement, not
    # stack it.
    s, c = create_screen(4, 3)

    # Frame 1: cmdash places at row 1 (0-based) with stable p=1.
    parse_bytes(s, b'\x1b[2;1H\x1b_Ga=T,f=24,i=7,s=1,v=1,c=1,r=1,C=1,q=2,p=1;AQID\x1b\\')
    ls = layers(s)
    check('1.1 upload creates one placement', len(ls) == 1, describe_layers(ls))
    check('1.2 placement at row 1', len(ls) == 1 and row_of(ls[0], 3) == 1, describe_layers(ls))
    ref_id = ls[0]['ref_id'] if ls else None
    img_id = ls[0]['image_id'] if ls else None

    # Frame 2: the placement moved to row 0; cmdash re-places with the same p=.
    parse_bytes(s, b'\x1b[1;1H\x1b_Ga=p,i=7,c=1,r=1,C=1,q=2,p=1;\x1b\\')
    ls = layers(s)
    check('2.1 re-place moves: exactly one placement', len(ls) == 1, describe_layers(ls))
    check('2.2 placement now at row 0', len(ls) == 1 and row_of(ls[0], 3) == 0, describe_layers(ls))
    check('2.3 same ref_id (placement identity preserved)', len(ls) == 1 and ls[0]['ref_id'] == ref_id,
          f"ref_id={ls[0]['ref_id'] if ls else None} wanted={ref_id}")
    check('2.4 same image id', len(ls) == 1 and ls[0]['image_id'] == img_id,
          f"image_id={ls[0]['image_id'] if ls else None} wanted={img_id}")
    check('2.5 no ghost at old row', len(ls) == 1, describe_layers(ls))

    # Frame 3: a second placement appears (p=2), then cmdash removes placement
    # p=1 with a placement-scoped delete; the image must survive for p=2.
    parse_bytes(s, b'\x1b[3;1H\x1b_Ga=p,i=7,c=1,r=1,C=1,q=2,p=2;\x1b\\')
    ls = layers(s)
    check('3.1 two placements', len(ls) == 2, describe_layers(ls))
    parse_bytes(s, b'\x1b_Ga=d,d=i,i=7,p=1;\x1b\\')
    ls = layers(s)
    check('3.2 scoped delete removes only p=1', len(ls) == 1 and ls[0]['ref_id'] == 2, describe_layers(ls))
    check('3.3 image data survives', s.grman.image_count == 1, f"image_count={s.grman.image_count}")
    check('3.4 remaining placement at row 2', len(ls) == 1 and row_of(ls[0], 3) == 2, describe_layers(ls))

    # Frame 4: last placement removed with cmdash's lowercase image-level
    # delete. Verified against real Kitty: lowercase `d=i` releases placements
    # but keeps the image data (protocol contract: re-display without
    # retransmission).
    parse_bytes(s, b'\x1b_Ga=d,d=i,i=7;\x1b\\')
    ls = layers(s)
    check('4.1 image-level delete frees all placements', len(ls) == 0, describe_layers(ls))
    check('4.2 lowercase d=i retains image data', s.grman.image_count == 1, f"image_count={s.grman.image_count}")
    # A bare re-place (no re-upload) must therefore re-display the image.
    parse_bytes(s, b'\x1b[2;1H\x1b_Ga=p,i=7,c=1,r=1,C=1,q=2,p=1;\x1b\\')
    ls = layers(s)
    check('4.3 retained data re-displays without retransmission',
          len(ls) == 1 and row_of(ls[0], 3) == 1 and s.grman.image_count == 1,
          describe_layers(ls))
    # Uppercase d=I frees the data as well.
    parse_bytes(s, b'\x1b_Ga=d,d=I,i=7;\x1b\\')
    ls = layers(s)
    check('4.4 uppercase d=I frees the image data',
          len(ls) == 0 and s.grman.image_count == 0, f"image_count={s.grman.image_count}")

    # -----------------------------------------------------------------------
    # Scenario 2: real content scroll. Text pushes the image into history; at
    # every view depth the image must appear exactly once (its true grid
    # position), never duplicated, and must be reachable by scrolling back.
    s, c = create_screen(4, 3, scrollback=100)
    parse_bytes(s, b'\x1b[2;1H\x1b_Ga=T,f=24,i=9,s=1,v=1,c=1,r=1,C=1,q=2,p=1;AQID\x1b\\')
    parse_bytes(s, b'row0\nrow1\nrow2\n')
    counts = [
        len([lyr for lyr in layers(s, scrolled_by=dep) if lyr['image_id'] == 1])
        for dep in range(0, 12)
    ]
    check('5.1 never more than one placement at any view depth (no ghost)',
          all(c <= 1 for c in counts), f'counts={counts}')
    check('5.2 image reachable by scrolling back', sum(counts) >= 1, f'counts={counts}')

    # -----------------------------------------------------------------------
    # Scenario 3: Unicode-placeholder mode. This feeds the exact byte streams
    # cmdash's backend emits for placeholder terminals (src/backend.rs
    # write_placeholder_upload / write_placeholder_cells /
    # clear_placeholder_cells): a virtual U=1 upload, then 24-bit-color + U+10EEEE
    # cells with row/col/high-bits combining marks. Real Kitty decodes these
    # into cell images via screen_render_line_graphics.
    DIACRITICS = [0x305, 0x30d, 0x30e, 0x310, 0x312, 0x33d, 0x33e, 0x33f, 0x346]

    def ph_upload(x, y, image_id, pw, ph, cw, ch):
        # write_placeholder_upload: MoveTo(x,y) then a=T with U=1,C=1,q=2,m=0.
        payload = 'A' * (pw * ph * 3)
        import base64 as _b64
        return (
            f'\x1b[{y + 1};{x + 1}H'
            f'\x1b_Ga=T,f=24,i={image_id},s={pw},v={ph},c={cw},r={ch},U=1,C=1,q=2,m=0;'
            f'{_b64.b64encode(payload.encode()).decode()}'
            f'\x1b\\'
        ).encode()

    def ph_cells(x, y, image_id, cw, ch):
        # write_placeholder_cells: 38;2;R;G;B then per row MoveTo + placeholder
        # char + row/col/high diacritics, reset to default fg.
        red, green, blue = (image_id >> 16) & 0xff, (image_id >> 8) & 0xff, image_id & 0xff
        high = (image_id >> 24) & 0xff
        out = f'\x1b[38;2;{red};{green};{blue}m'
        for r in range(ch):
            out += f'\x1b[{y + r + 1};{x + 1}H'
            for c in range(cw):
                out += chr(0x10EEEE) + chr(DIACRITICS[r]) + chr(DIACRITICS[c]) + chr(DIACRITICS[high])
        out += '\x1b[39m'
        return out.encode()

    def ph_clear(x, y, cw, ch):
        # clear_placeholder_cells: spaces over the old cells.
        out = b''
        for r in range(ch):
            out += f'\x1b[{y + r + 1};{x + 1}H'.encode() + b' ' * cw
        return out

    s, c = create_screen(6, 4)
    parse_bytes(s, ph_upload(1, 1, 7, 2, 2, 2, 2))
    parse_bytes(s, ph_cells(1, 1, 7, 2, 2))
    s.update_only_line_graphics_data()
    ls = layers(s)
    check('3.1 placeholder cells decode to one cell image per row (2 rows)',
          len(ls) == 2, describe_layers(ls))
    check('3.2 rows map to the correct screen rows 1 and 2',
          sorted(row_of(lyr, 4) for lyr in ls) == [1, 2], describe_layers(ls))
    # Each row run is 2 cells wide (2 of 6 columns = 0.6667 normalized). The
    # src rect maps each row to the correct half of the 2-row image; the exact
    # bottom is Kitty's fit-to-box artifact for a tiny image in a 10x20-cell
    # box, so we assert the row tops and full width instead.
    widths = sorted(round(lyr['dest_rect']['right'] - lyr['dest_rect']['left'], 4) for lyr in ls)
    check('3.3 dest spans 2 cells per row', widths == [0.6667, 0.6667], f'widths={widths}')
    srcs = sorted((round(lyr['src_rect']['top'], 4), round(lyr['src_rect']['left'], 4),
                   round(lyr['src_rect']['right'], 4)) for lyr in ls)
    check('3.4 src rects map rows 0/1 of the 2x2 image',
          srcs == [(0.0, 0.0, 1.0), (0.5, 0.0, 1.0)], f'srcs={srcs}')
    check('3.5 virtual upload kept exactly one image in the grman',
          s.grman.image_count == 1, f'image_count={s.grman.image_count}')

    # High 8 bits of the image id: cmdash encodes them as a third combining
    # mark with table index == high bits. Kitty's diacritic_to_num is 1-based
    # and subtracts 1, so index N decodes to high bits N.
    parse_bytes(s, ph_upload(3, 1, 0x01000007, 4, 2, 4, 2))
    parse_bytes(s, ph_cells(3, 1, 0x01000007, 1, 1))
    s.update_only_line_graphics_data()
    ls = layers(s)
    check('3.6 high-bits diacritic (idx 1) references image 0x01000007',
          len(ls) == 3 and any(round(lyr['src_rect']['right'], 4) == 0.25 for lyr in ls),
          describe_layers(ls))
    # A one-off index would decode to a different (unloaded) image id and must
    # NOT create a cell image: the lookup fails and the cell stays text.
    parse_bytes(s, ph_upload(3, 3, 0x01000007, 4, 2, 4, 2))  # no-op: same image
    wrong = ph_cells(3, 3, 0x01000007, 1, 1).replace(chr(DIACRITICS[1]).encode(), chr(DIACRITICS[2]).encode())
    parse_bytes(s, wrong)
    s.update_only_line_graphics_data()
    ls = layers(s)
    check('3.7 wrong high-bits mark decodes to an unloaded id (no cell image)',
          len(ls) == 3, describe_layers(ls))

    # Move the 2x2: cmdash clears the old cells with spaces and writes new
    # ones at the new position; rescanning must not leave ghosts.
    parse_bytes(s, ph_clear(1, 1, 2, 2))
    parse_bytes(s, ph_cells(0, 0, 7, 2, 2))
    s.update_only_line_graphics_data()
    ls = layers(s)
    rows = [row_of(lyr, 4) for lyr in ls if lyr['image_id'] == 1]
    check('3.8 moved placeholder leaves no ghost at the old cells',
          len(ls) == 3 and sorted(rows) == [0, 1], describe_layers(ls))
    # Erase all placeholder cells: cell images vanish, image data is retained.
    parse_bytes(s, ph_clear(0, 0, 2, 2))
    parse_bytes(s, ph_clear(3, 1, 1, 1))
    s.update_only_line_graphics_data()
    ls = layers(s)
    check('3.9 erasing placeholder cells removes cell images',
          len(ls) == 0 and s.grman.image_count == 2,
          f'layers={len(ls)} image_count={s.grman.image_count}')

    # -----------------------------------------------------------------------
    # Scenario 4: tmux-passthrough framing. cmdash wraps every graphics
    # command in \x1bPtmux; ... \x1b\\ with doubled ESC bytes; tmux strips the
    # wrapper and un-doubles, so the outer terminal receives exactly the
    # direct-mode bytes. Kitty itself does not unwrap the DCS wrapper, so the
    # harness verifies losslessness byte-for-byte and feeds the unwrapped
    # stream to real Kitty alongside the direct stream.
    direct = (
        b'\x1b[2;1H\x1b_Ga=T,f=24,i=7,s=1,v=1,c=1,r=1,C=1,q=2,p=1;AQID\x1b\\'
        b'\x1b[1;1H\x1b_Ga=p,i=7,c=1,r=1,C=1,q=2,p=1;\x1b\\'
        b'\x1b_Ga=d,d=i,i=7,p=1;\x1b\\'
        b'\x1b_Ga=d,d=i,i=7;\x1b\\'
    )

    def wrap_passthrough(raw):
        inner = b''
        for byte in raw:
            if byte == 0x1b:
                inner += b'\x1b'
            inner += bytes([byte])
        return b'\x1bPtmux;' + inner + b'\x1b\\'

    def unwrap_passthrough(wrapped):
        assert wrapped.startswith(b'\x1bPtmux;'), 'missing wrapper prefix'
        body = wrapped[len(b'\x1bPtmux;'):]
        i = 0
        while i < len(body):
            if body[i] == 0x1b and i + 1 < len(body) and body[i + 1] == 0x5c:
                body = body[:i]
                break
            if body[i] == 0x1b and i + 1 < len(body) and body[i + 1] == 0x1b:
                i += 2
            else:
                i += 1
        out = b''
        i = 0
        while i < len(body):
            if body[i] == 0x1b and i + 1 < len(body) and body[i + 1] == 0x1b:
                out += b'\x1b'
                i += 2
            else:
                out += bytes([body[i]])
                i += 1
        return out

    wrapped = wrap_passthrough(direct)
    check('4.1 wrapper uses the Ptmux; DCS prefix', wrapped.startswith(b'\x1bPtmux;'), str(wrapped[:12]))
    check('4.2 unwrapping yields the exact direct-mode bytes',
          unwrap_passthrough(wrapped) == direct, f'len={len(unwrap_passthrough(wrapped))}')
    # End-to-end: direct and unwrapped streams must produce identical layers.
    s_direct, _ = create_screen(4, 3)
    parse_bytes(s_direct, direct)
    s_passthru, _ = create_screen(4, 3)
    parse_bytes(s_passthru, unwrap_passthrough(wrapped))
    ld, lp = layers(s_direct), layers(s_passthru)
    check('4.3 direct and passthrough streams agree frame-by-frame',
          ld == lp, f'direct={describe_layers(ld)} passthrough={describe_layers(lp)}')
    check('4.4 both streams end with no visible placements',
          len(ld) == 0 and len(lp) == 0, describe_layers(lp))

    # -----------------------------------------------------------------------
    # Scenario 5: animation frame composition and GIF playback. Real Kitty's
    # grman coalesces animation frames server-side; `image_for_client_id`
    # exposes the coalesced pixels of the root frame and every extra frame,
    # so we can compare Kitty's composition results byte-for-byte against
    # cmdash's store semantics (src/graphics.rs `coalesce_frame` /
    # `compose_animation_frame`, which mirror Kitty's `a=f` / `a=c`).
    def apc(cmd, payload=b''):
        import base64 as _b64
        out = b'\x1b_G' + cmd
        if payload:
            out += b';' + _b64.b64encode(payload)
        return out + b'\x1b\\'

    def frame_data(screen, image_id, frame_idx):
        return screen.grman.image_for_client_id(image_id)['extra_frames'][frame_idx]['data']

    def close_pixels(got, expected, tol=0):
        return len(got) == len(expected) and all(abs(a - b) <= tol for a, b in zip(got, expected))

    s, c = create_screen(8, 6)

    # Root: 4x2 raw RGB, a distinct value per pixel.
    root = bytes([
        10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120,
        130, 140, 150, 160, 170, 180, 190, 200, 210, 220, 230, 240,
    ])
    parse_bytes(s, apc(b'a=T,f=24,i=7,s=4,v=2,c=4,r=2,C=1,q=2,m=0', root))

    # a=f delta: 2x1 opaque white at (0,0), replace mode (X=1), 100 ms gap.
    # cmdash and Kitty both coalesce this onto a blank canvas.
    parse_bytes(s, apc(b'a=f,f=24,i=7,r=2,s=2,v=1,x=0,y=0,X=1,z=100',
                       bytes([255, 255, 255, 255, 255, 255])))
    f2 = frame_data(s, 7, 0)
    check('5.1 a=f delta coalesces onto a blank canvas (replace at 0,0)',
          f2 == bytes([255] * 6) + bytes(18), f'len={len(f2)} data={f2.hex()}')
    check('5.2 frame gap preserved',
          s.grman.image_for_client_id(7)['extra_frames'][0]['gap'] == 100,
          f"gap={s.grman.image_for_client_id(7)['extra_frames'][0]['gap']}")

    # a=c full-frame overwrite: frame 1 (root) onto frame 2 (C=1, no alpha).
    parse_bytes(s, apc(b'a=c,i=7,r=1,c=2,C=1'))
    f2 = frame_data(s, 7, 0)
    check('5.3 a=c full overwrite copies the root pixels', f2 == root, f2.hex())

    # a=c partial rectangle with source crop: root's 2x1 rect at (1,0) onto
    # frame 2's (0,0). After the overwrite above, frame 2 equals the root, so
    # only row 0 columns 0-1 change.
    parse_bytes(s, apc(b'a=c,i=7,r=1,c=2,X=1,Y=0,x=0,y=0,w=2,h=1,C=1'))
    f2 = frame_data(s, 7, 0)
    expected = bytes([40, 50, 60, 70, 80, 90, 70, 80, 90, 100, 110, 120]) + root[12:]
    check('5.4 a=c partial rect crops the source and replaces at the destination',
          f2 == expected, f2.hex())

    # Alpha blend: source frame has a half-transparent pixel (f=32).
    # Kitty blends with float math that truncates, cmdash with integer math,
    # so the low byte can differ by 1; assert within tolerance.
    parse_bytes(s, apc(b'a=T,f=32,i=8,s=2,v=1,c=2,r=1,C=1,q=2,m=0',
                       bytes([255, 0, 0, 255, 0, 0, 255, 128])))
    parse_bytes(s, apc(b'a=f,f=32,i=8,r=2,s=2,v=1',
                       bytes([255, 255, 255, 255, 255, 255, 255, 255])))
    parse_bytes(s, apc(b'a=c,i=8,r=1,c=2,C=0'))
    f2 = frame_data(s, 8, 0)
    ours = bytes([255, 0, 0, 255, 127, 127, 255, 255])  # cmdash integer blend
    check('5.5 a=c alpha blend matches cmdash within float-vs-int rounding',
          close_pixels(f2, ours, tol=1),
          f'kitty={f2.hex()} cmdash={ours.hex()}')

    # GIF playback: cmdash decodes an animated GIF client-side and serves the
    # coalesced RGBA frames (verified in Rust, `animated_gif_for_test`: root
    # [red, green], frame 2 [blue, green], 100 ms gap). The harness replays
    # that exact stream — f=32 root upload + a=f frame — against real Kitty.
    parse_bytes(s, apc(b'a=T,f=32,i=9,s=2,v=1,c=2,r=1,C=1,q=2,m=0',
                       bytes([255, 0, 0, 255, 0, 255, 0, 255])))
    parse_bytes(s, apc(b'a=f,f=32,i=9,r=2,s=2,v=1,z=100',
                       bytes([0, 0, 255, 255, 0, 255, 0, 255])))
    d = s.grman.image_for_client_id(9)
    check('5.6 GIF root frame matches cmdash extraction',
          d['data'] == bytes([255, 0, 0, 255, 0, 255, 0, 255]), d['data'].hex())
    check('5.7 GIF frame 2 matches cmdash extraction',
          len(d['extra_frames']) == 1 and d['extra_frames'][0]['data']
          == bytes([0, 0, 255, 255, 0, 255, 0, 255]),
          f"frames={len(d['extra_frames'])}")
    check('5.8 GIF frame gap and total duration match cmdash',
          len(d['extra_frames']) == 1 and d['extra_frames'][0]['gap'] == 100
          and d['animation_duration'] == 100,
          f"gap={d['extra_frames'][0]['gap'] if d['extra_frames'] else None} "
          f"duration={d['animation_duration']}")

    # -----------------------------------------------------------------------
    # Scenario 6: erase mutations → delete command stream. cmdash's
    # apply_erase maps terminal erases to the same scoped/whole deletes the
    # adapters already serialize: ED 2 (clear screen) deletes only the visible
    # placements (history survives), ED 3 (clear scrollback) deletes only the
    # history placements (visible survives), and RIS (reset) deletes every
    # placement. Replay those streams against real Kitty and assert the scoped
    # end states.

    def upload(row, image_id):
        return (
            f'\x1b[{row + 1};1H'
            f'\x1b_Ga=T,f=24,i={image_id},s=1,v=1,c=1,r=1,C=1,q=2,p=1;AQID\x1b\\'
        ).encode()

    def image_ids_at(screen, scrolled_by):
        return sorted(lyr['image_id'] for lyr in layers(screen, scrolled_by))

    def reachable(screen, server_id, depth=10):
        return any(
            server_id in image_ids_at(screen, dep) for dep in range(depth)
        )

    # ED 2 (clear screen): the visible placement is deleted; the placement
    # scrolled into history survives and is reachable by scrolling back. To
    # keep one visible and one in history, the history image is placed and
    # scrolled away first, then the visible image is placed after the scroll.
    s, c = create_screen(4, 3, scrollback=100)
    parse_bytes(s, upload(0, 11))  # history image: server image_id 1
    parse_bytes(s, b'row0\nrow1\nrow2\nrow3\n')  # scroll it out of view
    parse_bytes(s, upload(1, 10))  # visible image: server image_id 2
    check('6.1 only the visible placement shows in the live view',
          image_ids_at(s, 0) == [2], describe_layers(layers(s)))
    # cmdash's ED 2 stream: scoped delete of the visible placement only.
    parse_bytes(s, b'\x1b_Ga=d,d=i,i=10,p=1;\x1b\\')
    check('6.2 clear-screen delete removes the visible placement',
          image_ids_at(s, 0) == [], describe_layers(layers(s)))
    check('6.3 history placement survives and is reachable by scrolling back',
          reachable(s, 1), describe_layers(layers(s, scrolled_by=4)))
    check('6.4 clear-screen delete retains the image data',
          s.grman.image_count == 2, f'image_count={s.grman.image_count}')

    # ED 3 (clear scrollback): only the history placement is deleted; the
    # visible placement stays put.
    s, c = create_screen(4, 3, scrollback=100)
    parse_bytes(s, upload(0, 11))  # history image: server image_id 1
    parse_bytes(s, b'row0\nrow1\nrow2\nrow3\n')
    parse_bytes(s, upload(1, 10))  # visible image: server image_id 2
    parse_bytes(s, b'\x1b_Ga=d,d=i,i=11,p=1;\x1b\\')
    check('6.5 clear-scrollback keeps the visible placement',
          image_ids_at(s, 0) == [2], describe_layers(layers(s)))
    check('6.6 clear-scrollback delete removed the history placement',
          not reachable(s, 1), describe_layers(layers(s, scrolled_by=4)))

    # RIS (reset): whole-image deletes for every placement; the data is
    # released with an uppercase d=I so the outer terminal frees it too.
    s, c = create_screen(4, 3)
    parse_bytes(s, upload(1, 10))
    parse_bytes(s, upload(2, 11))
    parse_bytes(s, b'\x1b_Ga=d,d=I,i=10;\x1b\\')
    parse_bytes(s, b'\x1b_Ga=d,d=I,i=11;\x1b\\')
    ls = layers(s)
    check('6.7 reset deletes remove every placement', len(ls) == 0, describe_layers(ls))
    check('6.8 reset deletes free the image data',
          s.grman.image_count == 0, f'image_count={s.grman.image_count}')

    # -----------------------------------------------------------------------
    # Scenario 7: DECSTBM region scroll. A child sets a partial scroll region,
    # places an image inside it, then scrolls the region with a linefeed. Real
    # Kitty must move the placement up by one row inside the region (it follows
    # the text) and keep it reachable in history — exactly the behavior cmdash's
    # ScrollRegionTracker models in
    # `session_graphics_follow_decstbm_scrolling_without_primary_scrollback`.
    s, c = create_screen(4, 6, scrollback=100)
    parse_bytes(s, b'\x1b[2;5r')  # region rows 2..5 => 0-based rows 1..5
    parse_bytes(s, b'\x1b[5;1H\x1b_Ga=T,f=24,i=7,s=1,v=1,c=1,r=1,C=1,q=2,p=1;AQID\x1b\\')

    def region_rows(screen, scrolled_by):
        return [(lyr['image_id'], row_of(lyr, 6)) for lyr in layers(screen, scrolled_by)]

    check('7.1 placement starts inside the region at row 4',
          region_rows(s, 0) == [(1, 4)], describe_layers(layers(s)))
    parse_bytes(s, b'\n')  # linefeed at the region bottom scrolls the region
    check('7.2 region scroll moves the placement up one row to row 3',
          region_rows(s, 0) == [(1, 3)], describe_layers(layers(s)))
    check('7.3 region-scrolled placement stays reachable in region history',
          region_rows(s, 1) == [(1, 4)] and region_rows(s, 2) == [(1, 5)],
          f"dep1={region_rows(s, 1)} dep2={region_rows(s, 2)}")

    # -----------------------------------------------------------------------
    # Scenario 8: history re-entry. This is the exact projection lifecycle
    # that previously exposed missing outer image replay: the placement starts
    # in retained history, is absent from the live projection, appears after
    # scrolling into history, disappears again when returning live, and must
    # reappear with the same Kitty `ref_id` when history is revisited.
    s, c = create_screen(4, 3, scrollback=100)
    parse_bytes(s, b'\x1b[2;1H\x1b_Ga=T,f=24,i=17,s=1,v=1,c=1,r=1,C=1,q=2,p=17;AQID\x1b\\')
    parse_bytes(s, b'row0\nrow1\nrow2\nrow3\nrow4\n')

    def image_layers_at(screen, depth):
        return [lyr for lyr in layers(screen, scrolled_by=depth) if lyr['image_id'] == 1]

    live = image_layers_at(s, 0)
    check('8.1 history placement is absent from the live projection',
          len(live) == 0, describe_layers(layers(s, scrolled_by=0)))

    reentry_depth = next(
        (depth for depth in range(1, 20) if image_layers_at(s, depth)),
        None,
    )
    history = image_layers_at(s, reentry_depth) if reentry_depth is not None else []
    original_ref_id = history[0]['ref_id'] if history else None
    original_row = row_of(history[0], 3) if history else None
    check('8.2 scrolling into history reveals exactly one placement',
          len(history) == 1 and reentry_depth is not None,
          f'depth={reentry_depth} layers={describe_layers(history)}')

    live_again = image_layers_at(s, 0)
    check('8.3 returning to the live viewport removes the projection',
          len(live_again) == 0, describe_layers(layers(s, scrolled_by=0)))

    history_again = image_layers_at(s, reentry_depth) if reentry_depth is not None else []
    check('8.4 history re-entry restores the same placement identity and row',
          len(history_again) == 1
          and history_again[0]['ref_id'] == original_ref_id
          and row_of(history_again[0], 3) == original_row,
          f'original_ref={original_ref_id} reentry={describe_layers(history_again)}')

    # -----------------------------------------------------------------------
    # Scenario 9: full-screen image scroll loop — no re-upload. cmdash keeps
    # image data alive across a lowercase `d=i` delete (the delete-ack
    # tombstone fix), so a full-screen placement that scrolls out of the view
    # and back in is re-displayed with a bare `a=p` and never re-transmits its
    # payload. Real Kitty must retain the data, keep exactly one image, and
    # restore the placement on every cycle; if the data had been freed, the
    # bare re-place would fail (ENOENT) and no placement would appear.
    import base64 as _b64_9
    FULL_SCREEN_PAYLOAD = bytes(range(36))  # 4x3 RGB, a distinct byte per pixel
    full_screen_b64 = _b64_9.b64encode(FULL_SCREEN_PAYLOAD).decode()

    def full_screen_upload():
        return (
            f'\x1b[1;1H\x1b_Ga=T,f=24,i=21,s=4,v=3,c=4,r=3,C=1,q=2,p=21;'
            f'{full_screen_b64}\x1b\\'
        ).encode()

    def full_screen_replace():
        return b'\x1b[1;1H\x1b_Ga=p,i=21,c=4,r=3,C=1,q=2,p=21;\x1b\\'

    s, c = create_screen(4, 3, scrollback=100)
    parse_bytes(s, full_screen_upload())
    ls = layers(s)
    check('9.1 full-screen upload creates one placement at the top row',
          len(ls) == 1 and row_of(ls[0], 3) == 0, describe_layers(ls))
    check('9.2 upload retained exactly one image', s.grman.image_count == 1,
          f'image_count={s.grman.image_count}')

    for cycle in range(1, 6):
        # Three lines of text scroll the full-screen image into history.
        parse_bytes(s, b'row0\nrow1\nrow2\n')
        hist = [lyr for lyr in layers(s, scrolled_by=3) if lyr['image_id'] == 1]
        check(f'9.{cycle}.1 cycle {cycle}: reachable in history after scrolling out',
              len(hist) == 1, describe_layers(hist))
        # Leaving the view: cmdash emits a data-retaining lowercase delete.
        parse_bytes(s, b'\x1b_Ga=d,d=i,i=21,p=21;\x1b\\')
        check(f'9.{cycle}.2 cycle {cycle}: scrolled-out delete leaves no placement',
              len(layers(s, 0)) == 0 and len(layers(s, 3)) == 0,
              describe_layers(layers(s, 3)))
        check(f'9.{cycle}.3 cycle {cycle}: lowercase delete retained the image data',
              s.grman.image_count == 1, f'image_count={s.grman.image_count}')
        # Re-entry: a bare a=p re-place must restore the placement from the
        # retained data — no re-upload, no payload retransmission.
        parse_bytes(s, full_screen_replace())
        ls = layers(s)
        check(f'9.{cycle}.4 cycle {cycle}: bare re-place restores the full-screen placement',
              len(ls) == 1 and row_of(ls[0], 3) == 0, describe_layers(ls))
        check(f'9.{cycle}.5 cycle {cycle}: no re-upload — image data count stays one',
              s.grman.image_count == 1, f'image_count={s.grman.image_count}')
        d = s.grman.image_for_client_id(21)
        check(f'9.{cycle}.6 cycle {cycle}: image data byte-identical across cycles',
              d is not None and d['data'] == FULL_SCREEN_PAYLOAD,
              f"len={len(d['data']) if d else None}")

    # -----------------------------------------------------------------------
    failed = [name for name, ok, _ in checks if not ok]
    print()
    print(f"{len(checks) - len(failed)}/{len(checks)} checks passed")
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
