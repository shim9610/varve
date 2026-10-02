#!/usr/bin/env python3
import argparse
import json
import statistics
from pathlib import Path
parser = argparse.ArgumentParser(description="Summarize a completed compare_native_index.py report; no load is rerun.")
parser.add_argument("report", type=Path)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
report = json.loads(args.report.read_text())
if report["status"] != "passed" or len(report["cases"]) != report["repeats"] * 4:
    raise SystemExit("comparison is incomplete")
rows=[]
for case in report['cases']:
    if case['status'] != 'passed' or not case['cleanup_verified']:
        raise SystemExit(f"incomplete case: {case['name']}")
    stages=case['stages']; w=next(s for s in stages if s['phase']=='write')
    epochs=[e for e in w['events'] if e['event']=='epoch']
    half=len(epochs)//2; seconds=epochs[-1]['elapsed_seconds']-epochs[half-1]['elapsed_seconds']
    end=next(e for e in w['events'] if e['event']=='write_complete')
    verify=next(s for s in stages if s['phase']=='verify')
    verification=next(e for e in verify['events'] if e['event']=='verify_complete')
    point=next(s for s in stages if s['phase']=='point')
    lookups=next(e for e in point['events'] if e['event']=='point_complete')
    for phase in (s for s in stages if s['phase'] == 'point'):
        result = next(e for e in phase['events'] if e['event'] == 'point_complete')
        if result['keys_checked'] != lookups['keys_checked']:
            raise SystemExit(f"point count changed after compaction: {case['name']}")
    readers=[e for e in w['events'] if e['event']=='reader']
    sidecar=next(f for f in case['files'] if f['name'].endswith('.vki'))
    compact=next((e for s in stages for e in s['events'] if e['event']=='compact_complete'),None)
    row=dict(name=case['name'],engine=case['engine'],readers=case['readers'],repeat=case['repeat'],
             write_seconds=w['elapsed_seconds'],concurrent_half_seconds=seconds,
             writer_mib_s=report['gib']*512/seconds,all_payload_mib_s=report['gib']*1024/end['seconds'],
             append_seconds=sum(e['append_ns'] for e in epochs[half:])/1e9,
             sync_seconds=sum(e['sync_ns'] for e in epochs[half:])/1e9,
             mutation_seconds=sum(e['mutation_ns'] for e in epochs[half:])/1e9,
             handoff_seconds=sum(e['handoff_ns'] for e in epochs[half:])/1e9,
             cpu_seconds=w['user_seconds']+w['system_seconds'] if 'user_seconds' in w else None,
             cpu_throttled_seconds=w['cpu_stat_delta']['throttled_usec']/1e6,
             peak_rss_bytes=w['peak_rss_bytes'],kernel_read_bytes=w['child_read_bytes'],kernel_write_bytes=w['child_write_bytes'],
             reader_checks=sum(e['latency']['count'] for e in readers),
             verify_seconds=verify['elapsed_seconds'],point_seconds=point['elapsed_seconds'],
             sidecar_bytes=sidecar['logical_bytes'],native_bytes=next(f['logical_bytes'] for f in case['files'] if f['name']=='data.varve'),
             files=case['files'],compact=compact,cleanup_verified=case['cleanup_verified'],delete_seconds=case['delete_seconds'],
             signature=dict(records=verification['records'],markers=verification['markers'],payload_bytes=verification['payload_bytes'],keys_checked=lookups['keys_checked']))
    rows.append(row)
if len({json.dumps(r['signature'],sort_keys=True) for r in rows}) != 1:
    raise SystemExit('logical outcomes differ between engines/runs')
medians=[]
for readers in (0,8):
    for engine in report['binary_sha256']:
        samples=[r for r in rows if r['engine']==engine and r['readers']==readers]
        metrics={k:statistics.median(r[k] for r in samples) for k in ('write_seconds','concurrent_half_seconds','writer_mib_s','all_payload_mib_s','append_seconds','sync_seconds','handoff_seconds','cpu_seconds','cpu_throttled_seconds','peak_rss_bytes','kernel_read_bytes','kernel_write_bytes','reader_checks','verify_seconds','point_seconds','sidecar_bytes','delete_seconds')}
        metrics.update(engine=engine,readers=readers,samples=len(samples),writer_mib_s_min=min(r['writer_mib_s'] for r in samples),writer_mib_s_max=max(r['writer_mib_s'] for r in samples))
        medians.append(metrics)
summary=dict(status='passed',platform=report['platform'],cpu_max=report['cpu_max'],memory_max=report['memory_max'],source_report=str(args.report.resolve()),scope=report['scope'],differences=report['differences'],binary_sha256=report['binary_sha256'],logical_results_identical=True,
             completed_payload_gib=report['completed_payload_gib'],total_checked_reader_gets=sum(r['reader_checks'] for r in rows),rows=rows,medians=medians)
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(medians,indent=2))
