"""Run an opt-in physical-tablet performance matrix against the headless host.
Usage: python tools/performance-matrix.py --pin-file PATH --log PATH
Requires a connected tablet and a host started with --pin-file. Closes each
animation window automatically and restores resolution/FPS after the run.
"""
import argparse
import json
import pathlib
import re
import statistics
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--pin-file', type=pathlib.Path, required=True)
p.add_argument('--log', type=pathlib.Path, required=True)
p.add_argument('--output', type=pathlib.Path, default=ROOT / 'target/performance-matrix.json')
p.add_argument('--restore-scale', type=int, default=50)
p.add_argument('--restore-fps', type=int, default=120)
p.add_argument('--cases', nargs='+', default=['100:60','100:90','100:120','100:144','75:60','75:120','50:60','50:120','50:144'], help='Resolution percent:FPS pairs')
a = p.parse_args()
cli = ROOT / 'target/release/secondscrn-host-cli.exe'
ansi = re.compile(r'\x1b\[[0-9;]*m')
pattern = re.compile(r'^Streaming .*? (\d+x\d+) \| cap (\d+) enc (\d+) fps ([\d.]+) ms .*?rtt (\d+)ms loss ([\d.]+)% .*?client (\d+) fps dec (\d+) ms q (\d+) .*?stut (\d+)')
results = []

def control(ext, value):
    a.pin_file.with_suffix('.' + ext).write_text(str(value))

def lines():
    return ansi.sub('', a.log.read_text(encoding='utf-8', errors='replace')).splitlines()

try:
    for case in a.cases:
        scale, fps = map(int, case.split(':'))
        control('res', scale)
        time.sleep(2)
        control('fps', fps)
        time.sleep(20)  # allow driver reload, encoder and decoder reconfiguration
        monitors = subprocess.check_output([str(cli), 'monitors'], creationflags=subprocess.CREATE_NO_WINDOW).decode()
        candidates = re.findall(r'(\d+x\d+) at \((-?\d+), (-?\d+)\)(?! primary)', monitors)
        if not candidates:
            raise RuntimeError('No tablet display found')
        size, x, y = max(candidates, key=lambda m: int(m[1]))
        # Verify this monitor matches the requested tablet scale before using it.
        if int(x) < 7000:
            raise RuntimeError('Expected tablet monitor to the right of physical displays; check placement')
        start = len(lines())
        subprocess.run([sys.executable, str(ROOT/'tools/performance-animation.py'), str(int(x)+100), str(int(y)+100), '20'], check=True)
        parsed = [m.groups() for line in lines()[start:] if (m := pattern.search(line))]
        # Remove initial animation ramp-up; sample only the active animation.
        samples = parsed[2:]
        if len(samples) < 4:
            raise RuntimeError(f'Insufficient telemetry: {len(samples)} samples')
        avg = lambda col: round(statistics.mean(float(row[col]) for row in samples), 1)
        row = dict(scale_percent=scale, requested_fps=fps, resolution=size,
                   capture_fps=avg(1), encode_fps=avg(2), encode_ms=avg(3),
                   rtt_ms=avg(4), loss_percent=avg(5), tablet_fps=avg(6),
                   decode_ms=avg(7), queue_max=max(int(r[8]) for r in samples),
                   stutters=int(samples[-1][9])-int(samples[0][9]),
                   sample_seconds=2*(len(samples)-1))
        row['stutters_per_second'] = round(row['stutters']/row['sample_seconds'], 2)
        if row['stutters'] < 0:
            raise RuntimeError('Decoder counters restarted during measurement; rerun this case after settling')
        results.append(row)
        a.output.write_text(json.dumps(results, indent=2)+'\n')
        print(json.dumps(row), flush=True)
finally:
    control('res', a.restore_scale)
    time.sleep(2)
    control('fps', a.restore_fps)
