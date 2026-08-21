#!/usr/bin/env python3
"""XVF3800 USB control (Seeed reSpeaker 2886:001a).
Adapted from pollen-robotics/reachy_mini audio_control_utils.py (Apache-2.0).
Usage: xvf.py NAME [--values V1 V2 ...]   |   xvf.py --list
"""
import sys, time, struct, argparse
import usb.core, usb.util

# name: (resid, cmdid, count, mode, typ)
P = {
    "VERSION": (48,0,3,"ro","uint8"),
    "BLD_MSG": (48,1,50,"ro","char"),
    "SAVE_CONFIGURATION": (48,9,1,"wo","uint8"),
    "CLEAR_CONFIGURATION": (48,10,1,"wo","uint8"),
    "SHF_BYPASS": (33,70,1,"rw","uint8"),
    "AEC_AECCONVERGED": (33,3,1,"ro","int32"),
    "AEC_RT60": (33,9,1,"ro","float"),
    "AEC_ASROUTONOFF": (33,35,1,"rw","int32"),
    "AEC_ASROUTGAIN": (33,36,1,"rw","float"),
    "AEC_FIXEDBEAMSONOFF": (33,37,1,"rw","int32"),
    "AEC_FIXEDBEAMNOISETHR": (33,38,2,"rw","float"),
    "AEC_FIXEDBEAMSAZIMUTH_VALUES": (33,81,2,"rw","radians"),
    "AEC_FIXEDBEAMSELEVATION_VALUES": (33,82,2,"rw","radians"),
    "AEC_FIXEDBEAMSGATING": (33,83,1,"rw","uint8"),
    "AEC_AZIMUTH_VALUES": (33,75,4,"ro","radians"),
    "AEC_SPENERGY_VALUES": (33,80,4,"ro","float"),
    "AUDIO_MGR_MIC_GAIN": (35,0,1,"rw","float"),
    "AUDIO_MGR_REF_GAIN": (35,1,1,"rw","float"),
    "AUDIO_MGR_SELECTED_AZIMUTHS": (35,11,2,"ro","radians"),
    "AUDIO_MGR_SELECTED_CHANNELS": (35,12,2,"rw","uint8"),
    "AUDIO_MGR_OP_L": (35,15,2,"rw","uint8"),
    "AUDIO_MGR_OP_R": (35,19,2,"rw","uint8"),
    "AUDIO_MGR_OP_ALL": (35,23,12,"rw","uint8"),
    "AUDIO_MGR_SYS_DELAY": (35,26,1,"rw","int32"),
    "DOA_VALUE": (20,18,2,"ro","uint32"),
    "DOA_VALUE_RADIANS": (20,19,2,"ro","radians"),
    "LED_EFFECT": (20,12,1,"rw","uint8"),
    "LED_BRIGHTNESS": (20,13,1,"rw","uint8"),
    "LED_COLOR": (20,16,1,"rw","uint32"),
    "LED_DOA_COLOR": (20,17,2,"rw","uint32"),
    "PP_AGCONOFF": (17,10,1,"rw","int32"),
    "PP_AGCMAXGAIN": (17,11,1,"rw","float"),
    "PP_AGCDESIREDLEVEL": (17,12,1,"rw","float"),
    "PP_AGCGAIN": (17,13,1,"rw","float"),
    "PP_MIN_NS": (17,21,1,"rw","float"),
    "PP_MIN_NN": (17,22,1,"rw","float"),
    "PP_ECHOONOFF": (17,23,1,"rw","int32"),
    "PP_ATTNS_MODE": (17,32,1,"rw","int32"),
    "PP_ATTNS_NOMINAL": (17,33,1,"rw","float"),
    "PP_DTSENSITIVE": (17,31,1,"rw","int32"),
}

def find_dev():
    for vid, pid in ((0x2886, 0x001A), (0x38FB, 0x1001)):
        d = usb.core.find(idVendor=vid, idProduct=pid)
        if d is not None: return d
    return None

def unpack(typ, raw):
    b = bytes(raw)
    if typ == 'char': return b.split(b'\x00')[0].decode(errors='replace')
    if typ == 'uint8': return list(b)
    n = len(b)//4
    if typ == 'int32' or typ == 'uint32':
        return list(struct.unpack('<%di' % n, b[:n*4]))
    return [round(x,6) for x in struct.unpack('<%df' % n, b[:n*4])]

def read(dev, name):
    resid, cmdid, cnt, mode, typ = P[name]
    if typ == 'char': size = cnt + 1
    elif typ == 'uint8': size = cnt + 1
    else: size = cnt*4 + 1
    for _ in range(64):
        raw = dev.ctrl_transfer(0xC0, 0, cmdid | 0x80, resid, size, 100000)
        if raw[0] == 0: return unpack(typ, raw[1:])
        time.sleep(0.02)
    raise RuntimeError('read %s never settled' % name)

def write(dev, name, vals):
    resid, cmdid, cnt, mode, typ = P[name]
    if mode == 'ro': raise ValueError(name + ' is read-only')
    if len(vals) != cnt: raise ValueError('%s needs %d values' % (name, cnt))
    payload = b''
    for v in vals:
        if typ in ('float','radians'): payload += struct.pack('<f', float(v))
        elif typ == 'uint8': payload += int(v).to_bytes(1,'little')
        elif typ == 'uint32': payload += struct.pack('<I', int(v))
        else: payload += struct.pack('<i', int(v))
    dev.ctrl_transfer(0x40, 0, cmdid, resid, payload, 100000)
    time.sleep(0.1)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('name', nargs='?')
    ap.add_argument('--values', nargs='+')
    ap.add_argument('--list', action='store_true')
    a = ap.parse_args()
    if a.list or not a.name:
        for k in sorted(P): print(k, P[k])
        return
    dev = find_dev()
    if dev is None: sys.exit('no XVF3800 found')
    if a.values:
        write(dev, a.name, a.values)
        if P[a.name][3] == 'rw': print(a.name, '=', read(dev, a.name))
        else: print(a.name, 'written')
    else:
        print(a.name, '=', read(dev, a.name))

if __name__ == '__main__':
    main()
