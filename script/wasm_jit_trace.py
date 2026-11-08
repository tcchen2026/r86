#!/usr/bin/env python3
"""Analyze --jit-trace JSONL; output JSON, module CSV and a standalone HTML report.

Schema: src/x86/jit/wasm/trace.rs. This reads observations, never guest code.
Execution costs are periodic wall-time samples, not exact CPU attribution.
Module function-entry counts include failed guards and are a reuse proxy.

An optional offline linear classifier and a one-feature threshold compare
prediction of >=64 entries within two seconds of admission (or retirement).
Features exist at admission, never include PC/backing identity or future use,
and training ends before a chronological holdout. Right-censored modules are
excluded. Prediction accuracy cannot estimate speedup from changing admission:
that changes execution, compilation, and future observations themselves.
"""

import argparse
from collections import Counter, defaultdict
import csv
import hashlib
import html
import json
import math
from pathlib import Path
import statistics

KIND = {1: "heat", 2: "heat_reset", 3: "admit", 4: "publish", 5: "retire",
        6: "interpreter_sample", 7: "native_chain_sample", 8: "compile_cost"}
EXITS = {0: "dispatch", 1: "memory", 2: "short_budget", 3: "attention",
         4: "interrupt_shadow", 5: "semantic", 6: "repeat", 7: "segment", 9: "cold_or_unqualified"}
FEATURES = ["log_bytecode", "log_instructions", "log_functions", "burst", "reused",
            "log_guest_delay", "log_interpreted_delay", "log_prior_admissions", "log_prior_heat_resets"]
IDLE_LIMIT = 524288
HORIZON = 2.0
REUSE_ENTRIES = 64


def quantiles(values):
    ordered = sorted(values)
    return {f"p{p}": ordered[min(len(ordered) - 1, int((len(ordered) - 1) * p / 100))]
            for p in (50, 90, 99)} if ordered else {}


def metrics(labels, predictions):
    tp = sum(y and p for y, p in zip(labels, predictions))
    tn = sum(not y and not p for y, p in zip(labels, predictions))
    fp = sum(not y and p for y, p in zip(labels, predictions))
    fn = sum(y and not p for y, p in zip(labels, predictions))
    return {"tp": tp, "tn": tn, "fp": fp, "fn": fn,
            "precision": tp / max(1, tp + fp), "recall": tp / max(1, tp + fn),
            "balanced_accuracy": (tp / max(1, tp + fn) + tn / max(1, tn + fp)) / 2}


def prediction_probe(modules, finish):
    ordered = sorted(modules, key=lambda m: (m["admitted_at"], m["request"]))
    cut = ordered[int(len(ordered) * .7)]["admitted_at"] if ordered else finish
    train = [m for m in ordered if m["label_time"] is not None and m["label_time"] < cut]
    test = [m for m in ordered if m["admitted_at"] >= cut and m["label_time"] is not None]
    result = {"target": f">={REUSE_ENTRIES} generated entries within {HORIZON}s or retirement",
              "split": "chronological 70% admission boundary; train labels finish before test begins",
              "cut_seconds": cut, "train_modules": len(train), "test_modules": len(test),
              "right_censored": sum(m["label_time"] is None for m in ordered),
              "features": FEATURES, "scope": "offline reuse prediction, not a speedup estimate"}
    for name, data in [("train", train), ("test", test)]:
        result[name + "_positives"] = sum(m["label"] for m in data)
        if len(data) < 20 or min(sum(m["label"] for m in data), sum(not m["label"] for m in data)) < 5:
            result["decision"] = "Insufficient positive/negative chronological examples; no model fitted."
            return result
    x = [m["features"] for m in train]
    y = [m["label"] for m in train]
    test_y = [m["label"] for m in test]
    result["predict_all_reused"] = metrics(test_y, [True] * len(test))
    # One simple threshold, chosen only on the training segment.
    best = (-1, 0, 0, True)
    for j in range(len(FEATURES)):
        values = sorted(set(row[j] for row in x))
        for at in range(min(32, len(values))):
            threshold = values[at * (len(values) - 1) // max(1, min(32, len(values)) - 1)]
            for above in (False, True):
                score = metrics(y, [(row[j] >= threshold) == above for row in x])["balanced_accuracy"]
                if score > best[0]:
                    best = (score, j, threshold, above)
    _, j, threshold, above = best
    result["threshold_heuristic"] = {"feature": FEATURES[j], "threshold": threshold, "above": above,
        "test": metrics(test_y, [(m["features"][j] >= threshold) == above for m in test])}
    # Small class-balanced logistic model; normalization uses training only.
    means = [statistics.fmean(row[j] for row in x) for j in range(len(FEATURES))]
    scales = [max(1e-6, statistics.pstdev(row[j] for row in x)) for j in range(len(FEATURES))]
    normalize = lambda row: [1.] + [(v - mean) / scale for v, mean, scale in zip(row, means, scales)]
    rows = [normalize(row) for row in x]
    weights = [0.] * (len(FEATURES) + 1)
    positive = sum(y)
    class_weight = [len(y) / (2 * (positive if label else len(y) - positive)) for label in y]
    for _ in range(200):
        gradient = [0.] * len(weights)
        for row, label, importance in zip(rows, y, class_weight):
            z = max(-30., min(30., sum(a * b for a, b in zip(weights, row))))
            error = (1 / (1 + math.exp(-z)) - label) * importance
            for j, value in enumerate(row):
                gradient[j] += error * value
        for j in range(len(weights)):
            weights[j] -= .1 * (gradient[j] / len(rows) + (.01 * weights[j] if j else 0))
    predictions = [sum(a * b for a, b in zip(weights, normalize(m["features"]))) >= 0 for m in test]
    result["linear_model"] = {"type": "class-balanced L2 logistic regression", "epochs": 200,
        "weights": weights, "means": means, "scales": scales, "test": metrics(test_y, predictions)}
    result["decision"] = "Inspect holdout errors before any runtime policy; validate on another boot/workload."
    return result


def analyze(source):
    modules, warm, roots = {}, {}, Counter()
    reset_by_key, reset_causes, counts = Counter(), Counter(), Counter()
    costs, pcs = defaultdict(lambda: [0, 0, 0]), defaultdict(lambda: [0, 0, 0])
    idle_gaps, idle_work, timeline = [], [], []
    native_dominated = preserved_by_work_clock = dropped = counter_wraps = 0
    last_step = finish = 0
    footer, truncated = None, False
    pending_costs = {}
    sha = hashlib.sha256()
    with source.open("rb") as f:
        for line_number, raw in enumerate(f, 1):
            sha.update(raw)
            packet = json.loads(raw)
            if packet.get("end"):
                footer = packet
                continue
            if packet.get("truncated"):
                truncated = True
                continue
            t, trace = packet["elapsed_seconds"], packet["trace"]
            if trace["schema"] != 1 or trace["sample_period"] != 256:
                raise ValueError(f"Unsupported trace schema on line {line_number}")
            if t < finish:
                raise ValueError("Non-monotonic drain time")
            finish = t
            dropped = max(dropped, trace["dropped"])
            chunk_counts, chunk_ns = Counter(), Counter()
            for row in trace["events"]:
                if len(row) != 11 or any(not isinstance(v, int) or v < 0 for v in row):
                    raise ValueError(f"Invalid event on line {line_number}")
                native, interpreted, kind, *d = row
                step = native + interpreted
                if step < last_step or kind not in KIND:
                    raise ValueError(f"Invalid event order/type on line {line_number}")
                last_step = step
                counts[KIND[kind]] += 1
                chunk_counts[KIND[kind]] += 1
                if kind == 1:
                    warm[d[0]] = (step, interpreted, d[1])
                elif kind == 2:
                    key, count, seen, candidate, cause, new_key, seen_work, _ = d
                    gap, work = step - seen, interpreted - seen_work
                    if min(gap, work) < 0 or work > gap:
                        raise ValueError("Inconsistent heat clocks")
                    reset_by_key[key] += 1
                    reset_causes["idle" if cause == 1 else "collision_fresh" if gap <= IDLE_LIMIT else "collision_already_idle"] += 1
                    if cause == 1:
                        idle_gaps.append(gap)
                        idle_work.append(work)
                        native_dominated += int(work <= gap * .1)
                        preserved_by_work_clock += int(work <= IDLE_LIMIT)
                    warm.pop(key, None)
                elif kind == 3:
                    request, slot, key, size, charge, insns, functions, cause = d
                    seen_step, seen_work, heat = warm.get(key, (step, interpreted, 0))
                    features = [math.log1p(size), math.log1p(insns), math.log1p(functions),
                        int(cause == 2), int(cause == 3), math.log1p(step - seen_step),
                        math.log1p(interpreted - seen_work), math.log1p(roots[key]), math.log1p(reset_by_key[key])]
                    roots[key] += 1
                    modules[request] = dict(request=request, slot=slot, key=key,
                        pc=f"{key & 0xffffffff:08x}", backing=f"{key >> 32:08x}",
                        admitted_at=t, bytecode=size, charge=charge, instructions=insns,
                        functions=functions, cause=cause, heat=heat, features=features,
                        entries=0, last_active=t, retired_at=None, label=False, label_time=None,
                        core_compile_ns=0, host_submit_ns=0)
                    if request in pending_costs:
                        modules[request]["core_compile_ns"], modules[request]["host_submit_ns"] = pending_costs.pop(request)
                elif kind == 4:
                    if d[0] in modules:
                        modules[d[0]]["published_at"] = t
                elif kind == 5:
                    if d[0] in modules:
                        m = modules[d[0]]
                        counter_wraps += int(d[2] < m["entries"])
                        m.update(entries=d[2], retired_at=t, retirement_cause=d[3])
                        if m["label_time"] is None:
                            m.update(label=d[2] >= REUSE_ENTRIES, label_time=t)
                elif kind in (6, 7):
                    steps, ns = (d[2], d[3]) if kind == 6 else (d[3], d[4])
                    reason = (EXITS.get(d[4], str(d[4])) if d[5] else "ineligible") if kind == 6 else "native_chain"
                    for aggregate in [costs[reason], pcs[(kind, d[0], d[1] if kind == 6 else 0)]]:
                        aggregate[0] += 1
                        aggregate[1] += steps
                        aggregate[2] += ns
                    chunk_ns["interpreter" if kind == 6 else "native"] += ns
                elif kind == 8:
                    pending_costs[d[0]] = d[1:3]
            for request, slot, entries, charge, functions, backing, origin in trace["modules"]:
                if request not in modules:
                    continue  # Explicit loss is handled by the dropped-row total.
                m = modules[request]
                counter_wraps += int(entries < m["entries"])
                if entries != m["entries"]:
                    m["last_active"] = t
                m["entries"] = entries
                if m["label_time"] is None and t >= m["admitted_at"] + HORIZON:
                    m.update(label=entries >= REUSE_ENTRIES, label_time=t)
            timeline.append(dict(seconds=t, native_steps=trace["native_steps"],
                interpreted_steps=trace["interpreted_steps"], charged_mib=trace["charged_bytes"] / 1024 ** 2,
                live_modules=len(trace["modules"]), admissions=chunk_counts["admit"],
                retirements=chunk_counts["retire"],
                sampled_interpreter_ms=chunk_ns["interpreter"] / 1e6,
                sampled_native_ms=chunk_ns["native"] / 1e6,
                inactive_2s_mib=sum(m["charge"] for m in modules.values()
                    if m["retired_at"] is None and t - m["last_active"] >= HORIZON) / 1024 ** 2))
    if not timeline:
        raise ValueError("Trace contains no data chunks")
    distribution = Counter()
    for m in modules.values():
        n = m["entries"]
        distribution["0" if n == 0 else "1..8" if n <= 8 else "9..64" if n <= 64 else "65..1024" if n <= 1024 else ">1024"] += 1
    total_ns = sum(v[2] for v in costs.values())
    cost_table = [{"reason": k, "samples": v[0], "sampled_steps": v[1],
        "sampled_ms": v[2] / 1e6, "estimated_seconds": v[2] * 256 / 1e9,
        "fraction_of_sampled_time": v[2] / max(1, total_ns)} for k, v in sorted(costs.items(), key=lambda v: -v[1][2])]
    report = {"input": str(source), "input_sha256": sha.hexdigest(), "schema": 1,
        "complete": bool(footer and not dropped and not truncated and not counter_wraps
                         and not footer.get("poisoned") and not footer.get("truncated")),
        "endpoint": footer, "dropped_events": dropped, "truncated": truncated, "event_counts": counts,
        "entry_counter_wraps": counter_wraps,
        "last_observation_seconds": finish, "final": timeline[-1],
        "heat_resets": dict(reset_causes), "idle_gap_guest_steps": quantiles(idle_gaps),
        "idle_gap_interpreted_steps": quantiles(idle_work),
        "idle_resets_at_least_90_percent_native_work": native_dominated,
        "idle_resets_preserved_by_interpreted_clock": preserved_by_work_clock,
        "module_entry_distribution": dict(distribution),
        "compile_cost_ms": {"core_emission": sum(m["core_compile_ns"] for m in modules.values()) / 1e6,
                            "host_submission": sum(m["host_submit_ns"] for m in modules.values()) / 1e6},
        "sampled_costs": cost_table,
        "top_interpreter_entries": [{"pc": f"{pc:08x}", "cr3": f"{context:08x}",
            "samples": v[0], "sampled_steps": v[1], "estimated_seconds": v[2] * 256 / 1e9}
            for (kind, pc, context), v in sorted(pcs.items(), key=lambda kv: -kv[1][2]) if kind == 6][:20],
        "limitations": ["Instrumented run; not a clean timing or measured speedup.",
            "Periodic wall samples can alias loops and include host tiering pauses; totals are estimates.",
            "Timer/import overhead is included; estimated seconds are not reconciled CPU attribution.",
            "Function-entry counts include guards; chain samples do not measure per-module savings.",
            "Event wall times are assigned to the next drain (normally within 250 ms).",
            "29-bit entry counts can wrap; a single boot does not establish workload generalization.",
            "Changed admission alters future execution; offline predictions are not counterfactual timings."]}
    report["prediction"] = prediction_probe(list(modules.values()), finish) if report["complete"] else {
        "decision": "Incomplete trace: do not train on missing lifecycle observations."}
    return report, list(modules.values()), timeline


def plot(timeline, series, title):
    width, height, left, top = 1000, 245, 75, 35
    xspan, yspan = width - left - 25, height - top - 40
    xmax = max(1, timeline[-1]["seconds"])
    ymax = max(1, max(row[key] for row in timeline for key, _, _ in series))
    parts = [f'<h2>{html.escape(title)}</h2><svg viewBox="0 0 {width} {height}" role="img" aria-label="{html.escape(title)}">']
    for i in range(5):
        y = top + yspan * i / 4
        parts.append(f'<path d="M{left},{y}H{width-25}" stroke="#dae0e8"/><text x="65" y="{y+5}" text-anchor="end">{ymax*(4-i)/4:.1f}</text>')
    for key, name, color in series:
        points = " ".join(f'{left+r["seconds"]/xmax*xspan:.1f},{top+(1-r[key]/ymax)*yspan:.1f}' for r in timeline)
        parts.append(f'<polyline fill="none" stroke="{color}" stroke-width="2" points="{points}"/>')
    parts.append(f'<text x="{left}" y="{height-7}">0 s</text><text x="{width-80}" y="{height-7}">{xmax:.1f} s</text></svg>')
    parts.append('<p>' + ' | '.join(f'<span style="color:{color}">■ {html.escape(name)}</span>' for _, name, color in series) + '</p>')
    return ''.join(parts)


def write_report(prefix, report, modules, timeline):
    prefix.parent.mkdir(parents=True, exist_ok=True)
    prefix.with_suffix('.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    fields = ["request", "slot", "pc", "backing", "admitted_at", "published_at", "retired_at", "bytecode", "charge",
              "instructions", "functions", "cause", "entries", "last_active", "core_compile_ns", "host_submit_ns", "label", "label_time"]
    with prefix.with_suffix('.csv').open('w', newline='') as f:
        writer = csv.DictWriter(f, fieldnames=fields, extrasaction='ignore')
        writer.writeheader()
        writer.writerows(modules)
    esc = lambda value: html.escape(str(value))
    parts = ['<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">',
        '<title>r86 Wasm JIT Trace</title><style>body{font:16px system-ui;color:#172333;background:#f4f6fa;margin:0 auto;padding:24px;max-width:1100px}h1{font-size:28px}h2{margin-top:32px;font-size:21px}svg,table,pre{background:white;border-radius:8px;max-width:100%}svg{width:100%}text{font:13px system-ui;fill:#536273}table{border-collapse:collapse;width:100%}th,td{padding:9px;text-align:left;border-bottom:1px solid #e0e6ef}pre{padding:16px;white-space:pre-wrap;overflow-wrap:anywhere}.note{color:#536273}a{color:#005da7}</style>',
        '<h1>r86 Wasm JIT lifecycle report</h1><p class="note">Diagnostic run; sampled times are not acceptance timings. Module entries indicate reuse, not net savings.</p>',
        f'<p>Complete trace: <b>{esc(report["complete"])}</b> · Dropped events: {report["dropped_events"]} · Modules: {len(modules)} · Last observation: {report["last_observation_seconds"]:.2f} s</p>',
        plot(timeline, [("charged_mib", "Charged MiB", "#005da7"), ("inactive_2s_mib", "Inactive for 2 s (MiB)", "#b66300")], "Cache charge and inactivity"),
        plot(timeline, [("admissions", "Submissions per interval", "#005da7"), ("retirements", "Retirements", "#b66300")], "Submission and retirement"),
        '<h2>Sampled execution cost</h2><table><tr><th>Reason</th><th>Samples</th><th>Estimated seconds</th><th>Sampled share</th></tr>']
    for row in report["sampled_costs"]:
        parts.append(f'<tr><td>{esc(row["reason"])}</td><td>{row["samples"]}</td><td>{row["estimated_seconds"]:.2f}</td><td>{row["fraction_of_sampled_time"]:.1%}</td></tr>')
    parts += ['</table><h2>Heat aging</h2>',
        f'<p>Heat resets: {esc(report["heat_resets"])}. An interpreter-work clock would retain {report["idle_resets_preserved_by_interpreted_clock"]} observations lost to idle expiry; this does not predict savings.</p>',
        f'<p>Module entry distribution: {esc(report["module_entry_distribution"])}</p>',
        '<h2>Offline heuristics / linear model</h2><p>Chronological train/holdout split; no address features. Predicts short-term reuse, not boot time.</p><pre>',
        esc(json.dumps(report["prediction"], ensure_ascii=False, indent=2)), '</pre>',
        '<h2>Costly interpreter entries</h2><table><tr><th>PC</th><th>CR3</th><th>Samples</th><th>Estimated seconds</th></tr>']
    for row in report["top_interpreter_entries"]:
        parts.append(f'<tr><td>{row["pc"]}</td><td>{row["cr3"]}</td><td>{row["samples"]}</td><td>{row["estimated_seconds"]:.3f}</td></tr>')
    parts += ['</table><h2>Limits</h2><ul>'] + [f'<li>{esc(v)}</li>' for v in report['limitations']] + ['</ul>']
    prefix.with_suffix('.html').write_text(''.join(parts))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace', type=Path)
    parser.add_argument('--output', type=Path, required=True, help='Output prefix for .json/.csv/.html')
    args = parser.parse_args()
    report, modules, timeline = analyze(args.trace)
    write_report(args.output, report, modules, timeline)
    print(json.dumps({k: report[k] for k in ["complete", "event_counts", "heat_resets",
        "idle_resets_preserved_by_interpreted_clock", "module_entry_distribution", "prediction"]}, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
