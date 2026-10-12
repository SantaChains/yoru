//! yoru-stdlib：**内建注册表——单一事实源（决策 77）**。
//!
//! 背景（12 号边界声明 + 20 号阶段 4 T2 首项）：内建签名曾有三套并行面——
//! typeck `Env::core()` 手写表、lower `BUILTIN_FNS` 名单、VM `dispatch_builtin`
//! match 臂——三者漂移已产生实测 bug（`string.upper` 被 Y-T0007 误拦、`print`
//! typeck 面 `print(string)` 与 VM 任意值不符、`assert` 二参面被当必选、
//! `math.floor/ceil` typeck 有面而 VM 无实现、模块限定调用运行期断链）。
//!
//! 本注册表收敛为一处权威定义：
//! - **typeck** 从 `BUILTINS` 生成 `Env` 的自由函数面与 std 模块面；
//! - **lower** 从 `builtin_names()` 得内建名单，并把 `模块.函数` 限定调用
//!   归一为同一 bare 名分发约定（与内建/宿主同构，决策 76）；
//! - **VM** 保持 match 分派，但受对账测试约束：注册表每个名字必须有分派臂
//!   （分发空参数不得落入 Y-H9000 未知名）。
//!
//! 访问模型（决策 25 配套）：**每个内建均可裸名调用**（数据第一参数，token
//! 经济）；`module` 字段是命名空间别名糖（`math.sqrt` ≡ `sqrt`）。用户代码
//! 不得遮蔽内建名（单一命名空间，19.2）。
//!
//! 签名表达是**封闭小 DSL**（`Param`），不引入新 Ty 变体给用户：`Number`/
//! `Collection`/`Any` 是仅存在于内建面的软类型，typeck 侧译为软 Ty 做参数
//! 位检查；`Var` 是泛型变量，走既有 unify 机制。

/// 内建参数/返回的类型描述（封闭 DSL，const 可构造）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Param {
    Int,
    Float,
    /// int | float（abs/min/max；min/max 另要求同具体类型，见 `same_numeric`）
    Number,
    Str,
    Bool,
    Unit,
    Json,
    Rune,
    /// 任意值（print 变参）；仅参数位
    Any,
    /// `[T]`
    List(&'static [Param]),
    /// `[K: V]`（keys/values 的 Map 面）
    MapKV(&'static str, &'static str),
    /// `a..b` 区间（range 产物；仅合法出现在切片下标与 for）
    Range,
    /// str | list | map（len 的编译期可查并集）
    Collection,
    /// 函数值：`fn(ps) -> ret`
    Fn(&'static [Param], &'static [Param]),
    /// 泛型变量（map 的 T/U、fold 的 A……）
    Var(&'static str),
    /// 可选尾参（assert 的 message）：实参缺省合法，在场须匹配内层
    Opt(&'static [Param]),
}

/// 一条内建定义。
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    /// bare 名（分发键、typeck free_fns 键）
    pub name: &'static str,
    /// std 模块别名（`Some("math")` → `math.sqrt`）；None = 仅裸名
    pub module: Option<&'static str>,
    /// 固定参位（`Opt` 尾参可缺省）
    pub params: &'static [Param],
    /// 变参：`params.len()` 之后接受任意数量 Any（print）
    pub variadic: bool,
    /// Number 实参须同为 int 或同为 float（决策 26：无隐式转换）——min/max
    pub same_numeric: bool,
    pub ret: Param,
    /// 一句话文档（阶段 5 doctest 与 LSP hover 的来源）
    pub doc: &'static str,
}

impl Builtin {
    /// 必选参数数（`Opt` 不计）。
    pub fn min_args(&self) -> usize {
        self.params
            .iter()
            .filter(|p| !matches!(p, Param::Opt(_)))
            .count()
    }
    /// 最大参数数（变参 = 无上界，用 usize::MAX 表达）。
    pub fn max_args(&self) -> usize {
        if self.variadic {
            usize::MAX
        } else {
            self.params.len()
        }
    }
    /// 该实参数是否可调（必选位 ≤ argc ≤ 最大位；变参只查下界）。
    pub fn accepts_argc(&self, argc: usize) -> bool {
        argc >= self.min_args() && argc <= self.max_args()
    }
}

// ---------- 注册表 ----------

/// 内建全表（决策 77）。**顺序即文档顺序**；`range` 两条按元数重载。
pub const BUILTINS: &[Builtin] = &[
    // ---- 集合（数据第一参数，决策 25）----
    Builtin {
        name: "len",
        module: Some("string"),
        params: &[Param::Collection],
        variadic: false,
        same_numeric: false,
        ret: Param::Int,
        doc: "长度：str 按码点计（决策 46），list/map 按项数",
    },
    Builtin {
        name: "map",
        module: None,
        params: &[
            Param::List(&[Param::Var("T")]),
            Param::Fn(&[Param::Var("T")], &[Param::Var("U")]),
        ],
        variadic: false,
        same_numeric: false,
        ret: Param::List(&[Param::Var("U")]),
        doc: "逐元素变换，产出新列表（迭代按值快照，决策 31）",
    },
    Builtin {
        name: "filter",
        module: None,
        params: &[
            Param::List(&[Param::Var("T")]),
            Param::Fn(&[Param::Var("T")], &[Param::Bool]),
        ],
        variadic: false,
        same_numeric: false,
        ret: Param::List(&[Param::Var("T")]),
        doc: "按谓词过滤，保留谓词为真的元素",
    },
    Builtin {
        name: "fold",
        module: None,
        params: &[
            Param::List(&[Param::Var("T")]),
            Param::Var("A"),
            Param::Fn(&[Param::Var("A"), Param::Var("T")], &[Param::Var("A")]),
        ],
        variadic: false,
        same_numeric: false,
        ret: Param::Var("A"),
        doc: "左折叠：以初值 A 逐元素累积",
    },
    Builtin {
        name: "sort",
        module: None,
        params: &[Param::List(&[Param::Var("T")])],
        variadic: false,
        same_numeric: false,
        ret: Param::List(&[Param::Var("T")]),
        doc: "同质 int/str 排序（决策 47：float 走 sort_floats）",
    },
    Builtin {
        name: "sort_floats",
        module: None,
        params: &[Param::List(&[Param::Float])],
        variadic: false,
        same_numeric: false,
        ret: Param::List(&[Param::Float]),
        doc: "float 排序（total_cmp 语义；NaN 抛错，决策 45/47）",
    },
    Builtin {
        name: "keys",
        module: None,
        params: &[Param::MapKV("K", "V")],
        variadic: false,
        same_numeric: false,
        ret: Param::List(&[Param::Var("K")]),
        doc: "Map 键列表（插入序，决策 36）",
    },
    Builtin {
        name: "values",
        module: None,
        params: &[Param::MapKV("K", "V")],
        variadic: false,
        same_numeric: false,
        ret: Param::List(&[Param::Var("V")]),
        doc: "Map 值列表（插入序，决策 36）",
    },
    // ---- 数值 ----
    Builtin {
        name: "abs",
        module: Some("math"),
        params: &[Param::Number],
        variadic: false,
        same_numeric: false,
        ret: Param::Number,
        doc: "绝对值：int 溢出抛错（决策 29），float 走 IEEE",
    },
    Builtin {
        name: "min",
        module: None,
        params: &[Param::Number, Param::Number],
        variadic: false,
        same_numeric: true,
        ret: Param::Number,
        doc: "较小者（两参须同为 int 或同为 float，决策 26）",
    },
    Builtin {
        name: "max",
        module: None,
        params: &[Param::Number, Param::Number],
        variadic: false,
        same_numeric: true,
        ret: Param::Number,
        doc: "较大者（两参须同为 int 或同为 float，决策 26）",
    },
    Builtin {
        name: "sqrt",
        module: Some("math"),
        params: &[Param::Float],
        variadic: false,
        same_numeric: false,
        ret: Param::Float,
        doc: "平方根（int 须显式 `as float`，决策 26）",
    },
    Builtin {
        name: "floor",
        module: Some("math"),
        params: &[Param::Float],
        variadic: false,
        same_numeric: false,
        ret: Param::Int,
        doc: "向下取整（非有限值抛错）",
    },
    Builtin {
        name: "ceil",
        module: Some("math"),
        params: &[Param::Float],
        variadic: false,
        same_numeric: false,
        ret: Param::Int,
        doc: "向上取整（非有限值抛错）",
    },
    // ---- 字符串 ----
    Builtin {
        name: "to_runes",
        module: Some("string"),
        params: &[Param::Str],
        variadic: false,
        same_numeric: false,
        ret: Param::List(&[Param::Rune]),
        doc: "字符串按码点转 [rune]（决策 46；禁 s[i] 的下标替代）",
    },
    Builtin {
        name: "upper",
        module: Some("string"),
        params: &[Param::Str],
        variadic: false,
        same_numeric: false,
        ret: Param::Str,
        doc: "转大写（Unicode simple case mapping）",
    },
    // ---- 解析 ----
    Builtin {
        name: "parse_int",
        module: None,
        params: &[Param::Str],
        variadic: false,
        same_numeric: false,
        ret: Param::Opt(&[Param::Int]),
        doc: "解析 int；失败 = None（决策 27 的 T? 通道）",
    },
    Builtin {
        name: "parse_float",
        module: None,
        params: &[Param::Str],
        variadic: false,
        same_numeric: false,
        ret: Param::Opt(&[Param::Float]),
        doc: "解析 float；失败 = None",
    },
    Builtin {
        name: "parse",
        module: Some("json"),
        params: &[Param::Str],
        variadic: false,
        same_numeric: false,
        ret: Param::Json,
        doc: "JSON 解析（T1 为宿主不透明值，决策 28）",
    },
    Builtin {
        name: "stringify",
        module: Some("json"),
        params: &[Param::Json],
        variadic: false,
        same_numeric: false,
        ret: Param::Str,
        doc: "JSON 序列化（T1 透传解析文本）",
    },
    // ---- 区间 ----
    Builtin {
        name: "range",
        module: None,
        params: &[Param::Int],
        variadic: false,
        same_numeric: false,
        ret: Param::Range,
        doc: "0..n 半开区间（决策 42）",
    },
    Builtin {
        name: "range",
        module: None,
        params: &[Param::Int, Param::Int],
        variadic: false,
        same_numeric: false,
        ret: Param::Range,
        doc: "a..b 半开区间（决策 42）",
    },
    // ---- 宿主通道（决策 73：print 走输出缓冲，宿主经 take_output 取走）----
    Builtin {
        name: "print",
        module: None,
        params: &[],
        variadic: true,
        same_numeric: false,
        ret: Param::Unit,
        doc: "打印任意值到宿主输出缓冲（display 语义，空格连接）",
    },
    Builtin {
        name: "assert",
        module: None,
        params: &[Param::Bool, Param::Opt(&[Param::Str])],
        variadic: false,
        same_numeric: false,
        ret: Param::Unit,
        doc: "断言：失败抛 Y-R9005；strip 产物 no-op（决策 70）",
    },
];

// ---------- 查询 API ----------

/// 全部内建 bare 名（去重、保序）——lower 名单与对账测试的来源。
pub fn builtin_names() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for b in BUILTINS {
        if !out.contains(&b.name) {
            out.push(b.name);
        }
    }
    out
}

/// 同名内建的全部重载层（按实参数选层）。
pub fn builtins_named(name: &str) -> Vec<&'static Builtin> {
    BUILTINS.iter().filter(|b| b.name == name).collect()
}

/// 自由函数按元数选层（管道送入计入 lhs）。
pub fn select_free(name: &str, argc: usize) -> Option<&'static Builtin> {
    builtins_named(name)
        .into_iter()
        .find(|b| b.accepts_argc(argc))
}

/// 模块限定函数选层（`math.sqrt` → bare 名同一分派约定）。
pub fn select_module(module: &str, field: &str, argc: usize) -> Option<&'static Builtin> {
    BUILTINS
        .iter()
        .find(|b| b.module == Some(module) && b.name == field && b.accepts_argc(argc))
}

/// 模块限定函数（不限元数）——lower 的限定调用归一用。
pub fn module_fn(module: &str, field: &str) -> Option<&'static Builtin> {
    BUILTINS
        .iter()
        .find(|b| b.module == Some(module) && b.name == field)
}

/// 该 bare 名是否是内建（禁遮蔽判据）。
pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|b| b.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_internal_consistency() {
        // 名字非空、文档非空、软类型不出现在返回位之外的位置
        for b in BUILTINS {
            assert!(
                !b.name.is_empty() && !b.doc.is_empty(),
                "empty name/doc in {b:?}"
            );
            assert!(b.min_args() <= b.max_args(), "arity inverted: {}", b.name);
            // Opt 只允许出现在尾位
            let first_opt = b.params.iter().position(|p| matches!(p, Param::Opt(_)));
            if let Some(i) = first_opt {
                assert!(
                    b.params[i..].iter().all(|p| matches!(p, Param::Opt(_))),
                    "Opt not in tail position: {}",
                    b.name
                );
            }
        }
        // (module, name) 无重复（free 名允许按元数重载——range 两层）
        for (i, a) in BUILTINS.iter().enumerate() {
            for b in &BUILTINS[i + 1..] {
                if let (Some(am), Some(bm)) = (a.module, b.module) {
                    assert!(
                        !(am == bm && a.name == b.name),
                        "duplicate module fn {am}.{}",
                        a.name
                    );
                }
            }
        }
    }

    #[test]
    fn arity_selection() {
        // range 两条重载按元数选层
        assert_eq!(select_free("range", 1).unwrap().params.len(), 1);
        assert_eq!(select_free("range", 2).unwrap().params.len(), 2);
        assert!(select_free("range", 3).is_none());
        assert!(select_free("range", 0).is_none());
        // print 变参：0 起无上界
        assert!(select_free("print", 0).is_some());
        assert!(select_free("print", 9).is_some());
        // assert：1-2
        assert!(select_free("assert", 1).is_some());
        assert!(select_free("assert", 2).is_some());
        assert!(select_free("assert", 3).is_none());
        // 模块面
        assert_eq!(module_fn("math", "sqrt").unwrap().name, "sqrt");
        assert!(module_fn("math", "nope").is_none());
        assert!(module_fn("string", "upper").is_some());
        assert!(module_fn("json", "stringify").is_some());
    }

    #[test]
    fn names_deduped_and_stable() {
        let names = builtin_names();
        assert!(names.contains(&"len") && names.contains(&"print"));
        assert_eq!(
            names.iter().filter(|&&n| n == "range").count(),
            1,
            "range 去重"
        );
        assert!(names.len() < BUILTINS.len(), "有重名条目不致膨胀名单");
    }
}
