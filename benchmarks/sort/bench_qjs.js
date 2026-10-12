// 插入排序基线（与 benchmarks/sort/bench.yoru / bench.lua 严格同构）
// N = 3000，公式生成 xs[i] = (i * 2654435761) % 100000，标准移位插入排序
// JS double 精确整数域 2^53：i * 2654435761 < 3000 * 2.7e9 < 2^53，取模精确
const n = 3000;
const xs = new Array(n);
for (let i = 0; i < n; i++) {
  xs[i] = (i * 2654435761) % 100000;
}
for (let i = 1; i < n; i++) {
  const key = xs[i];
  let j = i - 1;
  while (j >= 0 && xs[j] > key) {
    xs[j + 1] = xs[j];
    j = j - 1;
  }
  xs[j + 1] = key;
}
let s = 0;
for (let i = 0; i < n; i++) {
  s += xs[i];
}
print(s, xs[0], xs[n - 1]);
