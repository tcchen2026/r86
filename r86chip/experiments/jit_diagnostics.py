"""Read firmware JIT histograms; the ISS runner exports the existing trace buffer.

Flags 4|8 select this fixed observation format instead of CPU boundary records.
Counters are cumulative. Opcode counts are dynamic unsupported handoffs, not
cold/tick/service fallbacks. End counts are static compilation decisions.
"""
import struct

HISTOGRAM_FLAGS = 4 | 8


def histogram(directory):
    data = (directory / "iss-boundaries.bin").read_bytes()
    assert len(data) == 2176
    assert struct.unpack_from("<3I", data) == (0x4a495432, 1, len(data))
    counters = struct.unpack_from("<269Q", data, 16)
    return dict(leaves=dict(zip(("miss", "service", "tick", "interpret"), counters[:4])),
                services=dict(zip(("unused", "context", "segment", "split_or_alignment", "tlb"), counters[4:9])),
                ends=dict(zip(("branch", "unsupported", "window", "limit"), counters[9:13])),
                unsupported={f"{op:02x}": count for op, count in enumerate(counters[13:]) if count})


def difference(before, after):
    return {kind: {key: after[kind].get(key, 0) - before[kind].get(key, 0)
                   for key in sorted(before[kind].keys() | after[kind].keys())}
            for kind in after}
