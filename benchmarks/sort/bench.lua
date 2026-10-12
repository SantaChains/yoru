-- 插入排序基线（与 benchmarks/sort/bench.yoru 严格同构）
-- N = 3000，公式生成 xs[i] = (i * 2654435761) % 100000，标准移位插入排序
-- LuaJIT double 精确整数域 2^53：i * 2654435761 < 3000 * 2.7e9 < 2^53，取模精确
local n = 3000
local xs = {}
for i = 0, n - 1 do
  xs[i] = (i * 2654435761) % 100000
end
for i = 1, n - 1 do
  local key = xs[i]
  local j = i - 1
  while j >= 0 and xs[j] > key do
    xs[j + 1] = xs[j]
    j = j - 1
  end
  xs[j + 1] = key
end
local s = 0
for i = 0, n - 1 do
  s = s + xs[i]
end
print(s, xs[0], xs[n - 1])
