"""Run one isolated release qualification suite; never overlaps child measurements.
Build first with hybrid-ab-build.sh/hybrid-linux-build.sh. Each result has a binary/fixture
hash manifest. Select Linux quota and affinity outside this process before invoking it.
"""
import argparse
from pathlib import Path
import subprocess
import sys

parser = argparse.ArgumentParser()
parser.add_argument('--suite', required=True, choices=['tuning', 'controls', 'mixed', 'sustained', 'transport', 'linux'])
parser.add_argument('--release-dir', required=True)
parser.add_argument('--fixtures', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--transport-binary')
args = parser.parse_args()
root = Path(__file__).resolve().parent
fixtures = Path(args.fixtures)
release = Path(args.release_dir)
base = [sys.executable, str(root / 'hybrid_ab.py')]
for variant in ['original', 'dedicated', 'scheduled']:
    base += ['--' + variant, str(release / (variant + '-target/release/examples/hybrid_scheduler_benchmark'))]
base += ['--source', 'file', '--repeats', '3']
mp3 = str(fixtures / 'music3.mp3')
opus = str(fixtures / 'music3.opus')
if args.suite == 'tuning':
    extra = ['--fixtures', mp3 + ',' + opus, '--counts', '1,8,32,64,128,256,512', '--workers', '2,4,10', '--budgets', '1,8,32', '--formats', 'opus']
elif args.suite == 'controls':
    # Main's legacy seek output is not the corrected continuous-decode reference.
    extra = ['--skip-original', '--fixtures', mp3, '--counts', '8,64,256', '--workers', '4,10', '--budgets', '8', '--formats', 'f32le', '--controls', '4']
elif args.suite == 'mixed':
    extra = ['--fixtures', mp3 + ',' + opus + ',' + str(fixtures / 'music3.aac'), '--mixed', '--startup-stagger-ms', '1', '--counts', '32,128', '--workers', '4,10', '--budgets', '8', '--formats', 'f32le,opus']
elif args.suite == 'sustained':
    extra = ['--fixtures', str(fixtures / 'music15.opus'), '--counts', '128', '--workers', '10', '--budgets', '8', '--formats', 'opus', '--pacing', 'realtime']
elif args.suite == 'linux':
    extra = ['--fixtures', mp3 + ',' + opus, '--counts', '8,64,256,512', '--workers', '2,4', '--budgets', '8', '--formats', 'opus', '--pacing', 'realtime']
else:
    if not args.transport_binary:
        parser.error('--transport-binary is required for transport')
    base = [sys.executable, str(root / 'hybrid-transport.py'), '--binary', args.transport_binary, '--fixtures', mp3 + ',' + opus, '--counts', '8,128,512', '--workers', '4,10', '--budgets', '8', '--cases', 'paced,stagger,stalls', '--repeats', '3']
    extra = []
subprocess.run(base + extra + ['--output', args.output], check=True)
