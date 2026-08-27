#!/usr/bin/env python3
"""Raw HID capture + decode for Maschine MK3. Logs every changed field."""
import os, sys, time, select, glob

def find_hidraw():
    for p in glob.glob('/sys/class/hidraw/hidraw*'):
        try:
            u = open(os.path.join(p, 'device/uevent')).read()
        except OSError:
            continue
        if '17CC' in u.upper() and '1600' in u:
            return '/dev/' + os.path.basename(p)
    return None

DUR = float(sys.argv[1]) if len(sys.argv) > 1 else 90.0
LOG = sys.argv[2] if len(sys.argv) > 2 else '/tmp/mk3_capture.log'

dev = find_hidraw()
if not dev:
    sys.exit('Maschine MK3 hidraw node not found')

fd = os.open(dev, os.O_RDONLY | os.O_NONBLOCK)
log = open(LOG, 'w')

def emit(s):
    print(s, flush=True)
    log.write(s + '\n')
    log.flush()

emit(f'# device {dev}  duration {DUR}s')

prev01 = None
prev_knobs = None
t0 = time.time()
n = 0
while time.time() - t0 < DUR:
    r, _, _ = select.select([fd], [], [], 0.25)
    if not r:
        continue
    try:
        d = os.read(fd, 128)
    except BlockingIOError:
        continue
    if not d:
        continue
    n += 1
    t = time.time() - t0
    rid = d[0]

    if rid == 0x01:
        btn = d[1:11]                      # 80 bits
        nib = d[11]                        # 2 x 4-bit
        knobs = [int.from_bytes(d[12 + 2*i:14 + 2*i], 'little') for i in range(8)]
        tail = d[28:]
        if prev01 is None:
            emit(f'{t:7.3f} R01 INIT btn={btn.hex(" ")} nib={nib:02x} knobs={knobs} tail={tail.hex(" ")}')
        else:
            pb, pn, pk, pt = prev01
            # which button bits changed
            for i in range(10):
                x = btn[i] ^ pb[i]
                while x:
                    b = (x & -x).bit_length() - 1
                    idx = i * 8 + b
                    st = 'DOWN' if (btn[i] >> b) & 1 else 'up  '
                    emit(f'{t:7.3f} R01 BTN bit={idx:3d} (byte{i} bit{b}) {st}')
                    x &= x - 1
            if nib != pn:
                emit(f'{t:7.3f} R01 NIB {pn:02x} -> {nib:02x}  lo={nib&0xf} hi={nib>>4}')
            for i in range(8):
                if knobs[i] != pk[i]:
                    emit(f'{t:7.3f} R01 KNOB{i} {pk[i]:4d} -> {knobs[i]:4d}')
            if tail != pt:
                emit(f'{t:7.3f} R01 TAIL {pt.hex(" ")} -> {tail.hex(" ")}')
        prev01 = (btn, nib, knobs, tail)

    elif rid == 0x02:
        body = d[1:]
        out = []
        for i in range(0, len(body) - 2, 3):
            idx = body[i]
            ev = body[i+1] & 0xF0
            val = ((body[i+1] & 0x0F) << 8) | body[i+2]
            if i > 0 and idx == 0 and ev == 0 and val == 0:
                break
            out.append(f'pad{idx:02d}:{ev:02x}@{val:4d}')
        if out:
            emit(f'{t:7.3f} R02 ' + ' '.join(out))
    else:
        emit(f'{t:7.3f} R{rid:02x} len={len(d)} {d.hex(" ")}')

emit(f'# done, {n} reports')
os.close(fd)
