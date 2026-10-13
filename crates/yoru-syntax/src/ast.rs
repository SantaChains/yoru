//! AST：Yoru 语法树（19 号语法规范 v2 EBNF 的 Rust 表示）。
//!
//! 所有节点携带 Span（诊断定位硬要求）。类型文法与表达式文法严格分离（19.2）。

use crate::diagnostic::Span;

// ---- 程序与顶层声明（top_decl，决策 32/50：顶层纯声明）----

#[derive(Debug, Clone)]
pub struct Program {
    pub decls: Vec<Decl>,
}

#[derive(Debug, Clone)]
pub struct Decl {
    pub pub_: bool,
    pub kind: DeclKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum DeclKind {
    /// `import id [as alias] from "path"`
    Import {
        name: String,
        alias: Option<String>,
        path: String,
    },
    /// `type Id[T] = Type`（别名）/ `= distinct Type`（决策 23）/ `= V1 | V2`（tag 联合，19.5 示例）
    Type {
        name: String,
        params: Vec<String>,
        body: TypeBody,
    },
    /// `tag Id = V1 | V2(T)`（决策 53 后数据分支的唯一声明形式）
    Tag {
        name: String,
        params: Vec<String>,
        variants: Vec<Variant>,
    },
    /// `struct Id[T] { fields }`
    Struct {
        name: String,
        params: Vec<String>,
        fields: Vec<Field>,
    },
    /// `protocol Id[T] { fn sig... }`
    Protocol {
        name: String,
        params: Vec<String>,
        fns: Vec<ProtocolFn>,
    },
    /// `impl Proto for Id[T] { fn ... }`（决策 21：显式 impl）
    Impl {
        proto: String,
        for_: String,
        params: Vec<String>,
        fns: Vec<FnDecl>,
    },
    /// `const ID: T = expr`（编译期可折叠，决策 50）
    Const { name: String, ty: Type, value: Expr },
    /// 顶层函数（pub fn = 宿主入口，决策 32）
    Fn(FnDecl),
}

#[derive(Debug, Clone)]
pub enum TypeBody {
    Alias(Type),
    Distinct(Type),
    /// tag 联合（`type Tree[T] = Node(Node2[T]) | Leaf`，19.5）
    Union(Vec<Variant>),
}

#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    /// 载荷类型（无元组：多载荷走 struct，决策 53/Tier C）
    pub payload: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ProtocolFn {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FnDecl {
    pub name: String,
    /// 泛型形参（19.2 `fn f<T>(...)`；D-13 实例化管线的声明侧载体）
    pub type_params: Vec<String>,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub body: FnBody,
    pub span: Span,
}

/// `fn_body ::= block | "=" expr`（19.2；lambda 与顶层 fn 同一形式）
#[derive(Debug, Clone)]
pub enum FnBody {
    Block(Block),
    Expr(Expr),
}

// ---- 类型文法 v2（与表达式文法严格分离）----

#[derive(Debug, Clone)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Rune,
    Unit,
    Json,
    /// 命名类型，可带类型实参（named ::= identifier [ type_args ]）
    Named(String, Vec<Type>),
    /// `T?`（决策 39：typeck 拒绝 T??）
    Option(Box<Type>),
    Result(Box<Type>, Box<Type>),
    List(Box<Type>),
    /// `[K: V]`（K 约束 Hash+Ord，决策 30）
    Map(Box<Type>, Box<Type>),
    Fn(Vec<Type>, Box<Type>),
    Distinct(Box<Type>),
}

// ---- 语句（stmt，决策 47）----

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Let {
        mut_: bool,
        name: String,
        ty: Option<Type>,
        value: Expr,
    },
    Expr(Expr),
    While {
        cond: Expr,
        body: Block,
    },
    For {
        var: String,
        iter: Expr,
        body: Block,
    },
    Return(Option<Expr>),
    Throw(Expr),
    Break,
    Continue,
    /// 裸 block 作为语句（决策 47）
    Block(Block),
}

// ---- 表达式（expr，19.2 优先级链）----

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Pipe,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    BitOr,
    BitXor,
    BitAnd,
    Shl,
    Shr,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Range,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    /// 插值切分后的字符串；纯文本串 parts.len()==1
    Str(Vec<StrItem>),
    Bool(bool),
    Nil,
    Ident(String),
    /// `fn(params) = expr` / `fn(params) { block }`（lambda 参数类型可省）
    Lambda {
        params: Vec<LamParam>,
        body: Box<FnBody>,
    },
    /// `if cond { } else ...`；else 为 None 时仅语句位合法（typeck 校验）
    If {
        cond: Box<Expr>,
        then_: Block,
        else_: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// `try expr`（决策 54：双传播）
    Try(Box<Expr>),
    /// `await expr`（仅宿主能力，14.5）
    Await(Box<Expr>),
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `.field` 后缀（仅 struct 字段，19.2 集合 API 约定）
    Field {
        base: Box<Expr>,
        field: String,
    },
    /// 单元素 `xs[i]`（Map 取值 → V?，决策 55）或切片 `xs[a..b]`（决策 42）
    Index {
        base: Box<Expr>,
        index: IndexKind,
    },
    /// 后缀 `!` 解包（T? → T）
    Unwrap(Box<Expr>),
    /// `expr as T`（决策 26：唯一显式转换；19.2 EBNF 未列产式，按 19.5 示例补入 postfix 位）
    Cast {
        operand: Box<Expr>,
        ty: Type,
    },
    /// `Type { field: value, ..base }`（函数式字段更新，决策 41）
    StructLit {
        name: String,
        type_args: Vec<Type>,
        inits: Vec<FieldInit>,
        spread: Option<Box<Expr>>,
    },
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    /// 裸 block（else 分支 / match arm block 的 AST 载体）
    Block(Block),
    Assign {
        target: Box<Expr>,
        op: AssignOp,
        value: Box<Expr>,
    },
}

#[derive(Debug, Clone)]
pub enum StrItem {
    Lit(String),
    Interp(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct LamParam {
    pub name: String,
    pub ty: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

/// `pattern ::= literal | identifier | variant_pattern | "_"`（无解构模式）
#[derive(Debug, Clone)]
pub enum Pattern {
    Int(i64),
    Float(f64),
    Str(Vec<StrItem>),
    Bool(bool),
    Nil,
    /// 标识符（绑定或 nil/常量名）
    Bind(String),
    /// `Variant(p)` 单载荷绑定（多载荷变体载荷为 struct，绑定为整体）；
    /// `Variant(_)` 显式丢弃载荷（决策 86）——`Some(Ignore)` 与省略括号的 `None`
    /// 语义不同：前者对带载荷变体合法，后者仅限无载荷变体。
    Variant {
        name: String,
        payload: Option<Payload>,
    },
    Wildcard,
}

/// 变体载荷模式（决策 86）：命名绑定或 `_` 显式丢弃。
#[derive(Debug, Clone)]
pub enum Payload {
    Bind(String),
    Ignore,
}

#[derive(Debug, Clone)]
pub struct FieldInit {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum IndexKind {
    Single(Box<Expr>),
    /// 半开区间 `a..b`（决策 42，非结合）
    Range(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    AddAssign,
    SubAssign,
}
