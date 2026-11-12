#!/usr/bin/env python3
"""
把 assets/icon.ico 写进 exe 的 .rsrc 段。

GNU 工具链下不保证有 windres，所以这里直接按 PE 结构追加一个资源节，
只放 Windows 读取 exe 图标所需的两类条目：RT_ICON 和 RT_GROUP_ICON。

用法: python embed_icon.py <exe 路径> <ico 路径>
幂等：exe 已有 .rsrc 时直接跳过。
"""
import struct
import sys

RT_ICON = 3
RT_GROUP_ICON = 14
LANG_EN_US = 0x0409


# ------------------------------------------------------------------ PE 读取

def align(value, alignment):
    return (value + alignment - 1) // alignment * alignment


class Section:
    __slots__ = ("name", "vaddr", "vsize", "rawptr", "rawsize")

    def __init__(self, name, vsize, vaddr, rawsize, rawptr):
        self.name = name
        self.vaddr = vaddr
        self.vsize = vsize
        self.rawptr = rawptr
        self.rawsize = rawsize


def parse_pe(data):
    if data[:2] != b"MZ":
        raise SystemExit("不是 PE 文件")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise SystemExit("PE 签名不对")

    coff = pe + 4
    num_sections = struct.unpack_from("<H", data, coff + 2)[0]
    opt_size = struct.unpack_from("<H", data, coff + 16)[0]

    opt = coff + 20
    magic = struct.unpack_from("<H", data, opt)[0]
    if magic == 0x10B:
        data_dirs = opt + 96
        ptr_size = 4
    elif magic == 0x20B:
        data_dirs = opt + 112
        ptr_size = 8
    else:
        raise SystemExit(f"未知可选头 magic {magic:#x}")

    file_align = struct.unpack_from("<I", data, opt + 36)[0]
    section_align = struct.unpack_from("<I", data, opt + 32)[0]
    size_of_image_off = opt + 56

    sec_table = opt + opt_size
    sections = []
    for i in range(num_sections):
        o = sec_table + i * 40
        name = data[o:o + 8].rstrip(b"\0").decode("ascii", "replace")
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", data, o + 8)
        sections.append(Section(name, vsize, vaddr, rawsize, rawptr))

    return {
        "pe": pe, "coff": coff, "opt": opt, "sec_table": sec_table,
        "num_sections": num_sections, "opt_size": opt_size,
        "data_dirs": data_dirs, "sections": sections,
        "file_align": file_align or 0x200,
        "section_align": section_align or 0x1000,
        "size_of_image_off": size_of_image_off,
        "ptr_size": ptr_size,
    }


# --------------------------------------------------------------- 资源段构造

def build_rsrc(ico_bytes, rsrc_rva):
    _, kind, count = struct.unpack_from("<HHH", ico_bytes, 0)
    if kind != 1:
        raise SystemExit("不是 ico 文件")

    images = []
    for i in range(count):
        o = 6 + i * 16
        w, h, colors, _, planes, bpp, size, off = struct.unpack_from("<BBBBHHII", ico_bytes, o)
        images.append({
            "w": w or 256, "h": h or 256, "colors": colors,
            "planes": planes, "bpp": bpp, "data": ico_bytes[off:off + size],
        })

    icon_ids = list(range(1, len(images) + 1))
    group_id = 1

    # RT_GROUP_ICON 资源体：原样描述这组图标，条目 id 指回 RT_ICON
    group = struct.pack("<HHH", 0, 1, len(images))
    for img, iid in zip(images, icon_ids):
        group += struct.pack(
            "<BBBBHHIH",
            0 if img["w"] >= 256 else img["w"],
            0 if img["h"] >= 256 else img["h"],
            img["colors"], 0,
            img["planes"], img["bpp"],
            len(img["data"]), iid,
        )

    entries = [(RT_ICON, iid, img["data"]) for img, iid in zip(images, icon_ids)]
    entries.append((RT_GROUP_ICON, group_id, group))

    # 三层目录布局：根 -> 类型 -> 名称 -> 数据条目，最后是数据本体
    by_type = {}
    for e in entries:
        by_type.setdefault(e[0], []).append(e)

    def dir_bytes(n):
        return 16 + 8 * n

    root_off = 0
    cursor = dir_bytes(len(by_type))

    type_off = {}
    for t in by_type:
        type_off[t] = cursor
        cursor += dir_bytes(len(by_type[t]))

    name_off = {}
    for t, items in by_type.items():
        for e in items:
            name_off[(t, e[1])] = cursor
            cursor += dir_bytes(1)

    entry_off = {}
    for t, items in by_type.items():
        for e in items:
            entry_off[(t, e[1])] = cursor
            cursor += 16

    cursor = align(cursor, 4)
    blob = bytearray()
    data_off = {}
    for t, items in by_type.items():
        for e in items:
            while (cursor + len(blob)) % 4:
                blob.append(0)
            data_off[(t, e[1])] = cursor + len(blob)
            blob += e[2]
            while len(blob) % 4:
                blob.append(0)

    rsrc = bytearray(cursor + len(blob))

    # 根目录（全部是 id）
    types = sorted(by_type)
    struct.pack_into("<IIHHHH", rsrc, root_off, 0, 0, 0, 0, 0, len(types))
    for i, t in enumerate(types):
        struct.pack_into("<II", rsrc, root_off + 16 + i * 8,
                         t, (1 << 31) | type_off[t])

    # 类型目录
    for t in types:
        items = by_type[t]
        o = type_off[t]
        struct.pack_into("<IIHHHH", rsrc, o, 0, 0, 0, 0, 0, len(items))
        for i, e in enumerate(items):
            struct.pack_into("<II", rsrc, o + 16 + i * 8,
                             e[1], (1 << 31) | name_off[(t, e[1])])

    # 名称目录（一个语言）
    for t, items in by_type.items():
        for e in items:
            o = name_off[(t, e[1])]
            struct.pack_into("<IIHHHH", rsrc, o, 0, 0, 0, 0, 0, 1)
            struct.pack_into("<II", rsrc, o + 16,
                             LANG_EN_US, entry_off[(t, e[1])])

    # 数据条目：OffsetToData 是资源段内的 RVA，不是文件偏移
    for t, items in by_type.items():
        for e in items:
            struct.pack_into("<IIII", rsrc, entry_off[(t, e[1])],
                             rsrc_rva + data_off[(t, e[1])], len(e[2]), 0, 0)

    rsrc[cursor:cursor + len(blob)] = blob
    return bytes(rsrc)


# -------------------------------------------------------------------- 主流程

def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    exe_path, ico_path = sys.argv[1], sys.argv[2]

    data = bytearray(open(exe_path, "rb").read())
    ico = open(ico_path, "rb").read()
    info = parse_pe(data)

    if any(s.name == ".rsrc" for s in info["sections"]):
        print("exe 已有 .rsrc 段，跳过")
        return 0

    fa = info["file_align"]
    sa = info["section_align"]

    sections = info["sections"]
    new_va = align(max(s.vaddr + (s.vsize or s.rawsize) for s in sections), sa)
    rsrc = build_rsrc(ico, new_va)
    raw_ptr = align(max(s.rawptr + s.rawsize for s in sections), fa)
    raw_size = align(len(rsrc), fa)

    new_sec_table = info["sec_table"] + info["num_sections"] * 40
    first_raw = min(s.rawptr for s in sections if s.rawptr)
    if new_sec_table + 40 > first_raw:
        raise SystemExit("没有空间追加节表项")

    header = b".rsrc\0\0\0"
    header += struct.pack("<IIII", len(rsrc), new_va, raw_size, raw_ptr)
    header += struct.pack("<II", 0, 0)
    header += struct.pack("<HHI", 0, 0, 0x40000040)
    data[new_sec_table:new_sec_table + 40] = header

    struct.pack_into("<H", data, info["coff"] + 2, info["num_sections"] + 1)
    struct.pack_into("<I", data, info["size_of_image_off"],
                     align(new_va + len(rsrc), sa))
    struct.pack_into("<II", data, info["data_dirs"] + 2 * 8, new_va, len(rsrc))

    if len(data) < raw_ptr:
        data.extend(b"\0" * (raw_ptr - len(data)))
    data.extend(rsrc)
    data.extend(b"\0" * (raw_size - len(rsrc)))

    open(exe_path, "wb").write(bytes(data))
    print(f"已嵌入图标: .rsrc {len(rsrc)} 字节, RVA {new_va:#x}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
