#!/usr/bin/env python3
"""dedup-diff.py baseline.tsv new.tsv — 查重重构验收对拍脚本(Task 12)。

比对两份 mem_probe_scan dump TSV 的重复分组。TSV 形状(一成员一行):
    hash<TAB>size<TAB>id<TAB>path
组间以空行分隔,尾行 `# groups: N`。

口径(实施计划 Task 12,约束实现):
- 两侧先剔除路径含 `$Recycle.Bin` 段的成员(段级匹配、大小写不敏感;
  `KRECYCLE.BIN` 之类的普通目录不受影响);剔除后成员 <2 的组消解。
  对新侧幂等——新引擎索引本就不含回收站行。
- 过滤后按 `(hash, size)` 键逐组比对成员 id 集。
- 完全一致打印 `IDENTICAL`;否则打印 diff 摘要(计数 + 前 N 个不一致组)。

仅用标准库;退出码 0=IDENTICAL,1=有差异,2=输入不合法。
"""

import sys

# 段级大小写不敏感的回收站段名(路径按 `\` 与 `/` 双分隔符切段)。
RECYCLE_SEGMENT = "$recycle.bin"
# diff 摘要最多列出的不一致组数。
MAX_SAMPLES = 10


def parse_tsv(path):
    """解析 dump TSV → {(hash, size): [(id, path), ...]}。

    返回 (groups, declared, errors):declared 是尾行 `# groups: N` 声明的
    组数(None=无尾行);errors 是格式不良的行数(打印到 stdout)。
    """
    groups = {}
    declared = None
    errors = 0
    with open(path, encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, 1):
            line = raw.rstrip("\r\n")
            if not line:
                continue  # 组分隔空行
            if line.startswith("# groups:"):
                declared = int(line.split(":", 1)[1].strip())
                continue
            parts = line.split("\t")
            if len(parts) != 4:
                errors += 1
                print(f"  !! {path}:{lineno}: 非 4 列(期望 hash/size/id/path)")
                continue
            digest, size, member_id, member_path = parts
            try:
                groups[(digest, int(size))].append((int(member_id), member_path))
            except ValueError:
                errors += 1
                print(f"  !! {path}:{lineno}: size/id 非整数: {size!r}/{member_id!r}")
            except KeyError:
                groups[(digest, int(size))] = [(int(member_id), member_path)]
    return groups, declared, errors


def apply_recycle_filter(groups):
    """口径过滤:剔除路径含 `$Recycle.Bin` 段的成员,组 <2 成员消解。

    返回 (过滤后 groups, 被剔除成员数, 消解组数)。
    """
    filtered = {}
    dropped_members = 0
    dissolved = 0
    for key, members in groups.items():
        kept = [
            (member_id, member_path)
            for member_id, member_path in members
            if not is_recycle_path(member_path)
        ]
        dropped_members += len(members) - len(kept)
        if len(kept) >= 2:
            filtered[key] = kept
        else:
            dissolved += 1
    return filtered, dropped_members, dissolved


def is_recycle_path(member_path):
    """段级匹配:任一路径段(大小写不敏感)等于 `$Recycle.Bin` 即命中。"""
    segments = member_path.replace("/", "\\").split("\\")
    return any(seg.lower() == RECYCLE_SEGMENT for seg in segments)


def member_ids(group):
    return {member_id for member_id, _ in group}


def compare(baseline, new):
    """逐组比对 → (仅基线组数, 仅新侧组数, 成员集不同组数, 不一致样本行)。

    成员比对按口径只看 id 集(顺序不敏感;路径同库同行,不另比)。
    """
    only_baseline = sorted(set(baseline) - set(new))
    only_new = sorted(set(new) - set(baseline))
    member_diffs = sorted(
        k for k in set(baseline) & set(new) if member_ids(baseline[k]) != member_ids(new[k])
    )
    samples = []
    for key in only_baseline[:MAX_SAMPLES]:
        samples.append(
            f"  仅基线: hash={key[0]} size={key[1]} ids={sorted(member_ids(baseline[key]))}"
        )
    for key in only_new[:MAX_SAMPLES]:
        samples.append(f"  仅新侧: hash={key[0]} size={key[1]} ids={sorted(member_ids(new[key]))}")
    for key in member_diffs[:MAX_SAMPLES]:
        samples.append(
            f"  成员不同: hash={key[0]} size={key[1]}\n"
            f"    基线 ids={sorted(member_ids(baseline[key]))}\n"
            f"    新侧 ids={sorted(member_ids(new[key]))}"
        )
    return len(only_baseline), len(only_new), len(member_diffs), samples


def main(argv):
    if len(argv) != 3:
        print(__doc__)
        return 2
    baseline_path, new_path = argv[1], argv[2]

    baseline, base_declared, base_errs = parse_tsv(baseline_path)
    new, new_declared, new_errs = parse_tsv(new_path)
    for name, path, declared, errs, groups in (
        ("基线", baseline_path, base_declared, base_errs, baseline),
        ("新侧", new_path, new_declared, new_errs, new),
    ):
        note = f", 尾行声明 {declared}" if declared is not None else ", 无尾行声明"
        if declared is not None and declared != len(groups):
            note += " (与实际组数不符)"
        print(f"{name} {path}: {len(groups)} 组{note}" + (f", {errs} 行解析失败" if errs else ""))
    if base_errs or new_errs:
        return 2

    baseline, base_dropped, base_dissolved = apply_recycle_filter(baseline)
    new, new_dropped, new_dissolved = apply_recycle_filter(new)
    print(
        f"回收站过滤: 基线剔除 {base_dropped} 成员/消解 {base_dissolved} 组; "
        f"新侧剔除 {new_dropped} 成员/消解 {new_dissolved} 组"
    )
    print(
        f"过滤后: 基线 {len(baseline)} 组 vs 新侧 {len(new)} 组 "
        f"(基线成员 {sum(len(v) for v in baseline.values())} / "
        f"新侧成员 {sum(len(v) for v in new.values())})"
    )

    only_base, only_new, member_diffs, samples = compare(baseline, new)
    if only_base == 0 and only_new == 0 and member_diffs == 0:
        print("IDENTICAL")
        return 0
    print(f"DIFF: 仅基线 {only_base} 组, 仅新侧 {only_new} 组, 成员集不同 {member_diffs} 组")
    for line in samples:
        print(line)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
