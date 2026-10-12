//! 环境：内置 stdlib / 演示宿主类型 / 协议 / 用户声明表 + 类型解析。
//!
//! - `Env`：与程序无关的内建面——**由 `yoru_stdlib::BUILTINS` 注册表生成**
//!   （决策 77：单一事实源，终结 typeck/lower/VM 三面漂移）。
//! - `DeclTable`：单程序用户声明（struct/tag/alias/distinct/fn/const/protocol/impl）。
//! - 类型解析按需 + 代换，天然支持递归泛型类型（`Tree[T]`）。

use std::collections::HashMap;

use yoru_stdlib::{Builtin, Param};
use yoru_syntax::ast::{self, Type, TypeBody};
use yoru_syntax::{Diagnostic, Severity, Span};

use crate::ty::Ty;

// ---------- 内置环境 ----------

/// 内置协议（运算符过载以协议约定，决策 21）。
pub struct BuiltinProtocol {
    pub name: &'static str,
    /// 运算符对应的协议方法名（Add → add）
    pub method: &'static str,
    pub n_params: usize,
}

pub const BUILTIN_PROTOCOLS: &[BuiltinProtocol] = &[
    BuiltinProtocol {
        name: "Add",
        method: "add",
        n_params: 2,
    },
    BuiltinProtocol {
        name: "Sub",
        method: "sub",
        n_params: 2,
    },
    BuiltinProtocol {
        name: "Mul",
        method: "mul",
        n_params: 2,
    },
    BuiltinProtocol {
        name: "Div",
        method: "div",
        n_params: 2,
    },
    BuiltinProtocol {
        name: "Ord",
        method: "cmp",
        n_params: 2,
    },
    BuiltinProtocol {
        name: "Hash",
        method: "hash",
        n_params: 1,
    },
];

/// 内置环境：stdlib 模块、自由函数、演示宿主类型。
pub struct Env {
    /// 路径 → 模块 → 函数签名（阶段 1 仅 "std"）
    pub modules: HashMap<String, HashMap<String, HashMap<String, Ty>>>,
    /// 自由函数（数据第一参数，决策 25 配套）
    pub free_fns: HashMap<String, Ty>,
    /// 宿主注册类型（阶段 4 由注册表注入；--host demo 提供演示面）
    pub host_types: HashMap<String, Ty>,
}

impl Env {
    /// 内建面（决策 77）：由 `yoru_stdlib::BUILTINS` 注册表生成——
    /// 每个内建入自由函数表（bare 调用，数据第一参数，决策 25）；带 `module`
    /// 的同时入 std 模块表（`math.sqrt` 命名空间别名）。重名条目（range 的
    /// 两层元数）free 面取首层——值位置用；调用点按元数选层在 check.rs
    /// （`yoru_stdlib::select_free`）。
    pub fn core() -> Env {
        let mut std_mods: HashMap<String, HashMap<String, Ty>> = HashMap::new();
        let mut free_fns: HashMap<String, Ty> = HashMap::new();
        for b in yoru_stdlib::BUILTINS {
            let ty = builtin_face(b);
            free_fns.entry(b.name.to_string()).or_insert(ty.clone());
            if let Some(m) = b.module {
                std_mods
                    .entry(m.to_string())
                    .or_default()
                    .insert(b.name.to_string(), ty);
            }
        }
        let mut modules = HashMap::new();
        modules.insert("std".into(), std_mods);

        Env {
            modules,
            free_fns,
            host_types: HashMap::new(),
        }
    }

    /// 演示宿主面（19.5 权威样例所需）：`Ctx` / `Item` 宿主类型。
    /// print/assert 自决策 77 起为核内内建（VM 原生实现，输出缓冲即宿主通道，
    /// 决策 73），不再经 demo 注入——三面（typeck/lower/VM）以注册表对齐。
    /// 真实宿主面（12.2 契约）在阶段 4 由注册表注入。
    pub fn with_demo_host(mut self) -> Env {
        self.host_types
            .insert("Ctx".into(), Ty::named("Ctx", vec![]));
        self.host_types
            .insert("Item".into(), Ty::named("Item", vec![]));
        self
    }

    /// 注入宿主函数签名面（阶段 4 能力注册的编译期通道，12.2 契约）：
    /// 名字进 free_fns，调用点按签名检查；**未注入的名字调用 = 编译期
    /// Y-T0001**（能力未注入可测，退出标准）。与 `Vm::register_host_fn`
    /// 成对使用——签名契约单一闸口在 typeck。
    pub fn with_host_fns<I: IntoIterator<Item = (String, HostSig)>>(mut self, fns: I) -> Env {
        for (name, sig) in fns {
            self.free_fns.insert(name, sig.to_ty());
        }
        self
    }
}

/// 宿主函数签名（12.2 契约 HostSig）。编译期经 `Env::with_host_fns` 校验
/// 宿主调用点；运行时 T1 不做二次校验（VM 不依赖 typeck——字节码完整性
/// 是嵌入方责任，见 12 号边界声明）。
#[derive(Clone, Debug)]
pub struct HostSig {
    pub params: Vec<crate::ty::Ty>,
    pub ret: crate::ty::Ty,
}

impl HostSig {
    pub fn to_ty(&self) -> crate::ty::Ty {
        crate::ty::Ty::Fn(self.params.clone(), Box::new(self.ret.clone()))
    }
}

// ---------- 注册表 → typeck 面（决策 77） ----------

/// 注册表参数 → 语义类型。软类型（Number/Coll/Any）译为内建面软 Ty——
/// 只存在于内建签名，用户不可书写（解析器无对应产式）。
fn param_ty(p: &Param) -> Ty {
    match p {
        Param::Int => Ty::Int,
        Param::Float => Ty::Float,
        Param::Number => Ty::Num,
        Param::Str => Ty::Str,
        Param::Bool => Ty::Bool,
        Param::Unit => Ty::Unit,
        Param::Json => Ty::Json,
        Param::Rune => Ty::Rune,
        Param::Any => Ty::Any,
        Param::List(e) => Ty::list(param_ty(&e[0])),
        Param::MapKV(k, v) => Ty::Map(
            Box::new(Ty::Generic((*k).into())),
            Box::new(Ty::Generic((*v).into())),
        ),
        Param::Range => Ty::Range,
        Param::Collection => Ty::Coll,
        Param::Fn(ps, r) => Ty::Fn(ps.iter().map(param_ty).collect(), Box::new(param_ty(&r[0]))),
        Param::Var(n) => Ty::Generic((*n).into()),
        Param::Opt(inner) => Ty::opt(param_ty(&inner[0])),
    }
}

/// 内建完整签名面（自由函数值与模块函数值两用）。
pub fn builtin_face(b: &Builtin) -> Ty {
    Ty::Fn(
        b.params.iter().map(param_ty).collect(),
        Box::new(param_ty(&b.ret)),
    )
}

/// `param_ty` 的 crate 内可见包装（check.rs 的内建调用路径用）。
pub(crate) fn param_ty_pub(p: &Param) -> Ty {
    param_ty(p)
}

// ---------- 用户声明表 ----------

#[derive(Clone)]
pub enum TypeKind {
    Struct {
        params: Vec<String>,
        fields: Vec<(String, Type)>,
    },
    Tag {
        params: Vec<String>,
        variants: Vec<(String, Option<Type>)>,
    },
    Alias {
        params: Vec<String>,
        body: Type,
    },
    Distinct {
        base: Type,
    },
}

#[derive(Clone)]
pub struct FnInfo {
    pub name: String,
    pub params: Vec<(String, Type)>,
    pub ret: Option<Type>,
    pub span: Span,
}

/// 协议方法签名集：方法名 → (参数类型, 返回类型)。
pub type ProtoSigs = Vec<(String, Vec<Type>, Option<Type>)>;

#[derive(Clone)]
pub struct ImplInfo {
    pub proto: String,
    pub for_: String,
    pub params: Vec<String>,
    /// 协议方法名 → 实现
    pub methods: Vec<FnInfo>,
    pub span: Span,
}

/// 单程序用户声明集合。
#[derive(Default)]
pub struct DeclTable {
    pub types: HashMap<String, TypeKind>,
    pub fns: HashMap<String, FnInfo>,
    pub consts: HashMap<String, (Type, Option<ast::Expr>, Span)>,
    /// 协议名 → (类型参数, 方法签名集)
    pub protocols: HashMap<String, (Vec<String>, ProtoSigs)>,
    pub impls: Vec<ImplInfo>,
    /// 变体名 → (所属 tag, 载荷)
    pub variants: HashMap<String, (String, Option<Type>)>,
    /// import 绑定名 → (路径, 模块名, span)
    pub imports: HashMap<String, (String, String, Span)>,
}

impl DeclTable {
    pub fn collect(program: &ast::Program) -> Result<DeclTable, Diagnostic> {
        let mut t = DeclTable::default();
        for decl in &program.decls {
            match &decl.kind {
                ast::DeclKind::Import { name, alias, path } => {
                    let bind = alias.clone().unwrap_or_else(|| name.clone());
                    if t.imports
                        .insert(bind.clone(), (path.clone(), name.clone(), decl.span))
                        .is_some()
                    {
                        return Err(err_dup(&bind, decl.span));
                    }
                }
                ast::DeclKind::Type { name, params, body } => {
                    let kind = match body {
                        TypeBody::Alias(ty) => TypeKind::Alias {
                            params: params.clone(),
                            body: ty.clone(),
                        },
                        TypeBody::Distinct(ty) => TypeKind::Distinct { base: ty.clone() },
                        TypeBody::Union(vs) => TypeKind::Tag {
                            params: params.clone(),
                            variants: vs
                                .iter()
                                .map(|v| (v.name.clone(), v.payload.clone()))
                                .collect(),
                        },
                    };
                    if let TypeKind::Tag { variants, .. } = &kind {
                        for (vn, vp) in variants.clone() {
                            t.variants.insert(vn, (name.clone(), vp));
                        }
                    }
                    if t.types.insert(name.clone(), kind).is_some() {
                        return Err(err_dup(name, decl.span));
                    }
                }
                ast::DeclKind::Tag {
                    name,
                    params,
                    variants,
                } => {
                    for v in variants {
                        t.variants
                            .insert(v.name.clone(), (name.clone(), v.payload.clone()));
                    }
                    if t.types
                        .insert(
                            name.clone(),
                            TypeKind::Tag {
                                params: params.clone(),
                                variants: variants
                                    .iter()
                                    .map(|v| (v.name.clone(), v.payload.clone()))
                                    .collect(),
                            },
                        )
                        .is_some()
                    {
                        return Err(err_dup(name, decl.span));
                    }
                }
                ast::DeclKind::Struct {
                    name,
                    params,
                    fields,
                } => {
                    if t.types
                        .insert(
                            name.clone(),
                            TypeKind::Struct {
                                params: params.clone(),
                                fields: fields
                                    .iter()
                                    .map(|f| (f.name.clone(), f.ty.clone()))
                                    .collect(),
                            },
                        )
                        .is_some()
                    {
                        return Err(err_dup(name, decl.span));
                    }
                }
                ast::DeclKind::Protocol { name, params, fns } => {
                    let sigs = fns
                        .iter()
                        .map(|pf| {
                            (
                                pf.name.clone(),
                                pf.params.iter().map(|p| p.ty.clone()).collect(),
                                pf.ret.clone(),
                            )
                        })
                        .collect();
                    if t.protocols
                        .insert(name.clone(), (params.clone(), sigs))
                        .is_some()
                    {
                        return Err(err_dup(name, decl.span));
                    }
                }
                ast::DeclKind::Impl {
                    proto,
                    for_,
                    params,
                    fns,
                } => {
                    let methods = fns
                        .iter()
                        .map(|f| FnInfo {
                            name: f.name.clone(),
                            params: f
                                .params
                                .iter()
                                .map(|p| (p.name.clone(), p.ty.clone()))
                                .collect(),
                            ret: f.ret.clone(),
                            span: f.span,
                        })
                        .collect();
                    t.impls.push(ImplInfo {
                        proto: proto.clone(),
                        for_: for_.clone(),
                        params: params.clone(),
                        methods,
                        span: decl.span,
                    });
                }
                ast::DeclKind::Const { name, ty, value } => {
                    if t.consts
                        .insert(name.clone(), (ty.clone(), Some(value.clone()), decl.span))
                        .is_some()
                    {
                        return Err(err_dup(name, decl.span));
                    }
                }
                ast::DeclKind::Fn(f) => {
                    let info = FnInfo {
                        name: f.name.clone(),
                        params: f
                            .params
                            .iter()
                            .map(|p| (p.name.clone(), p.ty.clone()))
                            .collect(),
                        ret: f.ret.clone(),
                        span: f.span,
                    };
                    if t.fns.insert(f.name.clone(), info).is_some() {
                        return Err(err_dup(&f.name, f.span));
                    }
                }
            }
        }
        Ok(t)
    }
}

fn err_dup(name: &str, span: Span) -> Diagnostic {
    Diagnostic::coded(
        "Y-T0017",
        Severity::Error,
        format!("`{name}` 重复定义"),
        span,
    )
    .with_suggestion("删除或重命名其中一个定义（单一命名空间，19.2）")
}

// ---------- 类型解析 ----------

pub struct Resolver<'a> {
    pub decls: &'a DeclTable,
    pub env: &'a Env,
    /// 泛型形参 → 实参代换（检查泛型声明体时注入）
    pub subst: HashMap<String, Ty>,
}

impl<'a> Resolver<'a> {
    /// 语法类型 → 语义类型。subst 先于 decl 查找（泛型参数优先）。
    /// 注：类型节点无 span——定位错误由 caller 以所在声明 span 补充。
    pub fn resolve(&self, ty: &Type) -> Result<Ty, Diagnostic> {
        let span = Span::new(0, 0);
        Ok(match ty {
            Type::Int => Ty::Int,
            Type::Float => Ty::Float,
            Type::Bool => Ty::Bool,
            Type::Str => Ty::Str,
            Type::Rune => Ty::Rune,
            Type::Unit => Ty::Unit,
            Type::Json => Ty::Json,
            Type::Named(n, args) => {
                if let Some(t) = self.subst.get(n) {
                    return Ok(t.clone());
                }
                let rargs: Vec<Ty> = args
                    .iter()
                    .map(|a| self.resolve(a))
                    .collect::<Result<_, _>>()?;
                // 内建泛型的括号式拼写（19.5 示例形态）：Result[T, E] / Option[T]
                if n == "Result" {
                    if rargs.len() != 2 {
                        return Err(arity_err(n, 2, rargs.len(), span));
                    }
                    let mut it = rargs.into_iter();
                    let o = it.next().unwrap();
                    let e = it.next().unwrap();
                    return Ok(Ty::res(o, e));
                }
                if n == "Option" {
                    if rargs.len() != 1 {
                        return Err(arity_err(n, 1, rargs.len(), span));
                    }
                    let t = Ty::opt(rargs.into_iter().next().unwrap());
                    return Ok(t);
                }
                match self.decls.types.get(n) {
                    Some(TypeKind::Alias { params, body }) => {
                        if params.len() != rargs.len() {
                            return Err(arity_err(n, params.len(), rargs.len(), span));
                        }
                        let r2 = Resolver {
                            decls: self.decls,
                            env: self.env,
                            subst: params.iter().cloned().zip(rargs.iter().cloned()).collect(),
                        };
                        return r2.resolve(body);
                    }
                    Some(_) => {
                        let kind_params = match self.decls.types.get(n) {
                            Some(TypeKind::Struct { params, .. })
                            | Some(TypeKind::Tag { params, .. }) => Some(params),
                            _ => None,
                        };
                        if let Some(kp) = kind_params {
                            if kp.len() != rargs.len() {
                                return Err(arity_err(n, kp.len(), rargs.len(), span));
                            }
                        }
                        Ty::Named {
                            name: n.clone(),
                            args: rargs,
                        }
                    }
                    None => {
                        if self.env.host_types.contains_key(n) {
                            Ty::Named {
                                name: n.clone(),
                                args: rargs,
                            }
                        } else {
                            return Err(Diagnostic::coded(
                                "Y-T0007",
                                Severity::Error,
                                format!("未知类型 `{n}`"),
                                span,
                            )
                            .with_suggestion("检查拼写；宿主类型需由宿主注册表提供（12.2）"));
                        }
                    }
                }
            }
            Type::Option(inner) => {
                let t = self.resolve(inner)?;
                if matches!(t, Ty::Opt(_)) {
                    return Err(Diagnostic::coded(
                        "Y-T0006",
                        Severity::Error,
                        "不允许嵌套可选类型 `T??`（决策 39）",
                        span,
                    )
                    .with_suggestion("用 struct 包装，或改用 Result 表达两层失败"));
                }
                Ty::opt(t)
            }
            Type::Result(o, e) => Ty::res(self.resolve(o)?, self.resolve(e)?),
            Type::List(e) => Ty::list(self.resolve(e)?),
            Type::Map(k, v) => {
                let kt = self.resolve(k)?;
                self.check_map_key(&kt, span)?;
                Ty::Map(Box::new(kt), Box::new(self.resolve(v)?))
            }
            Type::Fn(ps, r) => Ty::Fn(
                ps.iter()
                    .map(|p| self.resolve(p))
                    .collect::<Result<_, _>>()?,
                Box::new(self.resolve(r)?),
            ),
            Type::Distinct(_) => {
                return Err(Diagnostic::coded(
                    "Y-T0007",
                    Severity::Error,
                    "`distinct` 只能出现在 type 别名声明中",
                    span,
                ));
            }
        })
    }

    /// Map 键约束：Hash + Ord（决策 30）。内置键型直接放行，命名类型查 impl。
    fn check_map_key(&self, kt: &Ty, span: Span) -> Result<(), Diagnostic> {
        let has_impl = |proto: &str, name: &str| {
            self.decls
                .impls
                .iter()
                .any(|i| i.proto == proto && i.for_ == name)
        };
        match kt {
            Ty::Str | Ty::Int | Ty::Rune | Ty::Bool => Ok(()),
            Ty::Named { name, .. } => {
                if has_impl("Hash", name) && has_impl("Ord", name) {
                    Ok(())
                } else {
                    Err(Diagnostic::coded(
                        "Y-T0007",
                        Severity::Error,
                        format!("Map 键 `{name}` 需实现 Hash 与 Ord 协议（决策 30）"),
                        span,
                    )
                    .with_suggestion(format!(
                        "为 `{name}` 显式 impl Hash 与 Ord，或改用 string/int 键"
                    )))
                }
            }
            other => Err(Diagnostic::coded(
                "Y-T0007",
                Severity::Error,
                format!("类型 `{other}` 不可作 Map 键（需 Hash + Ord，决策 30）"),
                span,
            )),
        }
    }
}

fn arity_err(name: &str, want: usize, got: usize, span: Span) -> Diagnostic {
    Diagnostic::coded(
        "Y-T0007",
        Severity::Error,
        format!("类型 `{name}` 需要 {want} 个类型实参，得到 {got}"),
        span,
    )
}
