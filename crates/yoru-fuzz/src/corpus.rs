//! V0 语料生成（30 号 §6.1）：确定性 RNG + token 段级变异器。
//!
//! 设计依据（调研定夺）：
//! - Nowarp 编译器 fuzz 实战：**token/语法级变异是唯一能稳定穿过 parser
//!   到达 typeck/lower/VM 的路径**，纯字节变异的深层覆盖近乎为零；
//! - 种子质量决定变异质量（"seeds beat cleverness"）：examples 11 篇 +
//!   测试语料内联种子；
//! - 确定性可复现：xorshift64*，case 种子 = base_seed ^ case_id 派生，
//!   崩溃输入凭 `(seed, case_id)` 二元组可精确重放。

/// xorshift64* —— 零依赖确定性 RNG。
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        // 避免全零不动点
        Rng(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    pub fn chance(&mut self, pct: u32) -> bool {
        self.next_u64() % 100 < pct as u64
    }
}

/// 变异字典：关键字（lexer.rs 全集）+ 高频内建名 + 运算符/标点 + 字面量形态。
/// 注入目标是「半合法片段」——能穿过 lexer/parser 到达更深层（调研结论：
/// 纯噪声在 parser 即被拒绝，深层覆盖率趋零）。
pub const TOKEN_DICT: &[&str] = &[
    // 关键字
    "let",
    "mut",
    "fn",
    "return",
    "if",
    "else",
    "while",
    "for",
    "in",
    "match",
    "struct",
    "tag",
    "protocol",
    "impl",
    "type",
    "import",
    "const",
    "as",
    "try",
    "throw",
    "await",
    "distinct",
    "pub",
    "break",
    "continue",
    "rune",
    "true",
    "false",
    "nil",
    // 类型名 / 内建自由函数（含 R 线新增 get/get_or）
    "int",
    "float",
    "bool",
    "string",
    "list",
    "map",
    "len",
    "map",
    "filter",
    "fold",
    "sort",
    "sort_floats",
    "keys",
    "values",
    "get",
    "get_or",
    "abs",
    "min",
    "max",
    "to_runes",
    "parse_int",
    "parse_float",
    "print",
    "range",
    "assert",
    "sqrt",
    "floor",
    "ceil",
    "upper",
    "parse",
    "stringify",
    "main",
    "Some",
    "None",
    "Err",
    "Ok",
    // 运算符 / 标点
    "+",
    "-",
    "*",
    "/",
    "%",
    "==",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
    "&&",
    "||",
    "!",
    "<<",
    ">>",
    "&",
    "|",
    "=",
    "+=",
    "-=",
    "->",
    "=>",
    ".",
    "..",
    "..=",
    ":",
    ",",
    ";",
    "?",
    "@",
    "#",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    "|",
    "\\",
    "$",
    // 字面量形态（数值边界 / 字符串插值 / rune / 注释四形态）
    "0",
    "1",
    "-1",
    "9223372036854775807",
    "18446744073709551616",
    "1e999",
    "0.1",
    "1e308",
    "0x7fffffffffffffff",
    "\"\"",
    "\"${x}\"",
    "\"\\u{7f}\"",
    "\"\\n\"",
    "'a'",
    "'\\n'",
    "// c",
    "/* b */",
    "/// doc",
    "\n",
    "\n\n",
    "    ",
    "\t",
];

/// 种子语料：examples 11 篇全量 + 内联微语料（覆盖 struct/tag/match/lambda/链式赋值）。
pub fn seeds() -> Vec<&'static str> {
    vec![
        include_str!("../../../examples/math/01_vec2_lin_alg.yoru"),
        include_str!("../../../examples/math/02_mat2_transforms.yoru"),
        include_str!("../../../examples/math/03_polygon.yoru"),
        include_str!("../../../examples/math/04_trig_approx.yoru"),
        include_str!("../../../examples/math/05_vec3_pipeline.yoru"),
        include_str!("../../../examples/math/06_ascii_raster.yoru"),
        include_str!("../../../examples/math/07_game_math.yoru"),
        include_str!("../../../examples/math/08_physics.yoru"),
        include_str!("../../../examples/math/09_comp_geom.yoru"),
        include_str!("../../../examples/math/10_pip.yoru"),
        include_str!("../../../examples/math/11_overview.yoru"),
        // 内联微语料：examples 未覆盖的构造面
        "struct P { x: int, y: int }\nfn main() -> int {\n    let mut p = P { x: 1, y: 2 }\n    p.x = p.x + 1\n    return p.x\n}\n",
        "tag E = A | B(int) | C { v: int }\nfn f(e: E) -> int {\n    match e {\n        A -> 0\n        B(n) -> n\n        C { v } -> v\n    }\n}\nfn main() -> int {\n    return f(C { v: 3 })\n}\n",
        "fn main() -> int {\n    let mut m = [\"a\": 1, \"b\": 2]\n    m[\"c\"] = 3\n    let v = get(m, \"c\")\n    if v == nil {\n        return 0\n    }\n    return 1\n}\n",
        "fn main() -> int {\n    let xs = map(range(0, 5), fn(x: int) = x * x)\n    let s = fold(xs, 0, fn(a: int, b: int) = a + b)\n    return s\n}\n",
        "fn main() -> int {\n    let mut l = [3, 1, 2]\n    l[0] = 9\n    sort(l)\n    for x in l {\n        print(\"${x}\")\n    }\n    return 0\n}\n",
    ]
}

/// 括号嵌套深度上限：超过则丢弃该 case 重掷（深嵌套 → parser 递归 →
/// 栈溢出是 abort 类不可捕获，Nowarp 实战同款「良性 panic」缓解）。
const MAX_NEST: usize = 48;

/// token 段级变异：种子经 tokenize 切成段列表，在段层做结构感知变异。
/// 返回 None = 该轮变异被深度守卫拒绝（调用方重掷）。
pub fn mutate(src: &str, rng: &mut Rng) -> Option<String> {
    let (tokens, _) = yoru_syntax::tokenize(src);
    if tokens.is_empty() {
        return None;
    }
    // 段列表 = 全保真 span 切片（Newline/注释/字符串各自成段）。
    // 注意：token span 不含 token 间空白——重建必须走 rebuild 的空白
    // 重排（F1 同款教训：切片直拼 = 系统性损坏，`import math` → `importmath`）。
    let mut segs: Vec<&str> = tokens
        .iter()
        .map(|t| &src[t.span.start..t.span.end])
        .collect();

    // 变异强度分布化（业界共识：轻度变异为主保深层通过率，重度变异测
    // parser 鲁棒性）：期望 ~1.5 轮；删除/交换等高破坏操作降权。
    let rounds = 1 + usize::from(rng.chance(40)) + usize::from(rng.chance(15));
    for _ in 0..rounds {
        match rng.below(12) {
            // 删除片段（高破坏——降权至 2/12）
            0 | 1 => {
                let at = rng.below(segs.len());
                let len = 1 + rng.below(4.min(segs.len() - at));
                segs.drain(at..at + len);
            }
            // 字典注入（主力 5/12——局部半合法片段穿透 parser 到深层）
            3..=7 => {
                let at = rng.below(segs.len());
                let tok = TOKEN_DICT[rng.below(TOKEN_DICT.len())];
                if rng.chance(50) {
                    segs.insert(at, " ");
                }
                segs.insert(at, tok);
                if tok.starts_with("//") && tok != "/* b */" {
                    segs.insert(at + 1, "\n");
                }
            }
            // 复制片段（splice）
            2 | 8 => {
                let at = rng.below(segs.len());
                let len = 1 + rng.below(8.min(segs.len() - at));
                let clip: Vec<&str> = segs[at..at + len].to_vec();
                let to = rng.below(segs.len());
                for (i, s) in clip.iter().enumerate() {
                    segs.insert((to + i).min(segs.len()), s);
                }
            }
            // 交换两片段（最高破坏——1/12）
            9 => {
                if segs.len() > 4 {
                    let a = rng.below(segs.len() - 2);
                    let b = a + 1 + rng.below(segs.len() - a - 1);
                    segs.swap(a, b);
                }
            }
            // 换行扰动（Yoru 换行终止文法：换行注入/删除是最高价值的结构变异）
            10 | 11 => {
                let at = rng.below(segs.len());
                if rng.chance(50) {
                    segs.insert(at, "\n");
                } else if segs[at] == "\n" {
                    segs.remove(at);
                } else {
                    segs.insert(at, "\n");
                }
            }
            // 缩进注入
            _ => {
                let at = rng.below(segs.len());
                segs.insert(at, if rng.chance(50) { "    " } else { "\t" });
            }
        }
    }

    let out = rebuild(&segs);
    // 深度守卫：粗计数（字符串/注释内的括号也被计入——宁可错杀重掷）
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    for c in out.chars() {
        match c {
            '{' | '(' | '[' => {
                depth += 1;
                max_depth = max_depth.max(depth);
            }
            '}' | ')' | ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if max_depth > MAX_NEST {
        return None;
    }
    if out.len() > 64 * 1024 {
        return None;
    }
    Some(out)
}

/// 段列表 → 源码：token 间空白重排。
/// 规则：换行/缩进段自身是分隔；一侧已有空白则不重复；否则补单空格。
/// 贴靠形态（如 `a.foo` 的点号紧靠）在此不追求——目标是穿过 lexer，
/// 不是格式美化（F1 formatter 管美学）。
fn rebuild(segs: &[&str]) -> String {
    let mut out = String::with_capacity(segs.iter().map(|s| s.len() + 1).sum());
    for (i, s) in segs.iter().enumerate() {
        if i > 0 {
            let prev = segs[i - 1];
            let prev_is_ws = prev.ends_with('\n') || prev.ends_with(' ') || prev.ends_with('\t');
            let cur_is_ws = s.starts_with('\n') || s.starts_with(' ') || s.starts_with('\t');
            let is_ws_seg = *s == "\n" || *s == "    " || *s == "\t" || prev == "\n";
            if !prev_is_ws && !cur_is_ws && !is_ws_seg {
                out.push(' ');
            }
        }
        out.push_str(s);
    }
    out
}
