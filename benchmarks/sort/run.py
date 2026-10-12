"""基准计时 harness（方法论：预热丢弃 + 多次采样 + 中位数/最小值 + 输出校验）。

用法: python run.py --label NAME -- CMD ARGS...
对同一命令做 2 次预热（丢弃）+ 7 次测量，报告 min/median/max 与完整 stdout。
"""
import argparse
import statistics
import subprocess
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--warmup", type=int, default=2)
    ap.add_argument("--runs", type=int, default=7)
    ap.add_argument("cmd", nargs="+")
    args = ap.parse_args()

    samples = []
    out = ""
    for k in range(args.warmup + args.runs):
        t0 = time.perf_counter()
        r = subprocess.run(args.cmd, capture_output=True, text=True)
        dt = time.perf_counter() - t0
        if r.returncode != 0:
            raise SystemExit(f"FAIL rc={r.returncode}: {r.stderr[:500]}")
        if k == 0:
            out = r.stdout.strip()
        if k >= args.warmup:
            samples.append(dt)

    samples.sort()
    med = statistics.median(samples)
    print(f"[{args.label}]")
    print(f"  output      : {out!r}")
    print(f"  runs        : {args.runs} (warmup {args.warmup} discarded)")
    print(f"  min         : {samples[0]*1000:8.1f} ms")
    print(f"  median      : {med*1000:8.1f} ms")
    print(f"  max         : {samples[-1]*1000:8.1f} ms")
    print(f"  spread      : {(samples[-1]-samples[0])/med*100:5.1f} %")


if __name__ == "__main__":
    main()
