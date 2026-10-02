#!/usr/bin/env python3
"""Compare a retained redb-backed Varve load binary with the native-index binary.
Uses actual allocated files, identical I/O contracts, counterbalanced order,
full payload/CRC + point checks, resource counters and ownership-checked deletion.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import time
from load_scalable import atomic_json, case_command, execute, text

GIB = 1024 ** 3

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--native', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--data', type=Path, required=True)
    parser.add_argument('--gib', type=int, default=20)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--baseline-label', default='redb')
    parser.add_argument('--native-label', default='native')
    parser.add_argument('--compact-baseline', action='store_true')
    parser.add_argument('--comparison-note', help='Describe implementation and cache-budget differences')
    args = parser.parse_args()
    if args.baseline_label == args.native_label:
        parser.error('implementation labels must differ')
    binaries = {args.baseline_label: args.baseline.resolve(), args.native_label: args.native.resolve()}
    hashes = {key: hashlib.sha256(path.read_bytes()).hexdigest() for key,path in binaries.items()}
    output, data = args.output.resolve(), args.data.resolve()
    output.mkdir(parents=True, exist_ok=True)
    data.mkdir(parents=True, exist_ok=True)
    if any(data.iterdir()):
        raise SystemExit('data directory must be empty')
    marker = data / '.varve-load-owner'
    marker.write_text(str(output))
    total = args.gib * GIB
    epoch = 256 * 1024**2
    report = dict(status='running', platform=platform.platform(), cpu_max=text('/sys/fs/cgroup/cpu.max').strip(),
                  memory_max=text('/sys/fs/cgroup/memory.max').strip(), binary_sha256=hashes,
                  binaries={k:str(v) for k,v in binaries.items()}, gib=args.gib,repeats=args.repeats,cases=[],
                  scope='Identical native payload, CRC, batch/cache limits and durability boundaries. Each reader has its own handle. No profiler. Guest kernel I/O counters are not host device bandwidth.',
                  differences=args.comparison_note or 'Whole implementation comparison includes the removal of the manual FormatSpec contract RwLock. cache_bytes is the same option: native caches are private per reader, while old redb shares one cache; aggregate memory is not fixed. RSS is measured. Use index-only oracle for equivalent key/value engine work.')
    def save(): atomic_json(output/'report.json',report)
    save()
    try:
        for repeat in range(args.repeats):
            order=[(args.baseline_label,0),(args.native_label,0),(args.native_label,8),(args.baseline_label,8)]
            if repeat % 2: order.reverse()
            for engine,readers in order:
                if shutil.disk_usage(data).free < total * 1.025 + GIB:
                    raise RuntimeError('insufficient headroom for real non-sparse file')
                if hashlib.sha256(binaries[engine].read_bytes()).hexdigest()!=hashes[engine]:
                    raise RuntimeError('load binary changed')
                name=f'{repeat+1:02d}-{engine}-r{readers}'
                directory=data/name; directory.mkdir()
                logs=output/name; logs.mkdir()
                config=dict(name=name,mode='indexed',readers=readers,pinned=0,pause_us=0,
                            record=65536,batch=4*1024**2,index_batch=16384,cache=8*1024**2,crc=True)
                case=dict(name=name,engine=engine,readers=readers,repeat=repeat+1,config=config,stages=[],status='running')
                report['cases'].append(case); save()
                phases=['write','verify','point']
                if engine==args.native_label or args.compact_baseline: phases+=['compact','point']
                for index,phase in enumerate(phases):
                    label=f'{index:02d}-{phase}'
                    print(f'{name}: {label}',flush=True)
                    result=execute(case_command(binaries[engine],config,directory,phase,total,epoch,1),
                                   logs,label,os.environ.copy(),900,phase+'_complete',data)
                    case['stages'].append(dict(phase=phase,**result)); save()
                    if result['status']!='passed': raise RuntimeError(f'{name}/{phase} failed; data retained')
                    print(f"  passed {result['elapsed_seconds']:.3f}s, kernel write={result['child_write_bytes']/GIB:.3f} GiB",flush=True)
                    if phase=='write':
                        case['files']=[dict(name=p.name,logical_bytes=p.stat().st_size,allocated_bytes=p.stat().st_blocks*512)
                                       for p in sorted(directory.iterdir()) if p.is_file()]
                        native=next(f for f in case['files'] if f['name']=='data.varve')
                        if native['allocated_bytes'] < total * .99: raise RuntimeError('file is not fully allocated')
                if directory.parent!=data or directory.is_symlink() or marker.read_text()!=str(output):
                    raise RuntimeError('cleanup ownership mismatch')
                free=shutil.disk_usage(data).free; start=time.monotonic()
                shutil.rmtree(directory)
                fd=os.open(data,os.O_RDONLY|os.O_DIRECTORY)
                try: os.fsync(fd)
                finally: os.close(fd)
                case.update(status='passed',delete_seconds=time.monotonic()-start,
                            free_bytes_recovered=shutil.disk_usage(data).free-free,cleanup_verified=not directory.exists())
                save()
        report['status']='passed';report['completed_payload_gib']=args.gib*len(report['cases']);save()
    except BaseException as error:
        report.update(status='failed',error=repr(error));save();raise
    print(f"Compared and deleted {report['completed_payload_gib']} GiB; every oracle passed.",flush=True)

if __name__=='__main__': main()
