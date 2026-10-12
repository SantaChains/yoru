//! 诊断结构：人+AI 双端友好（设计 03 章 3.8，12 章 12.2 API 契约）。

use serde_json::Value;

/// 源码位置（字节偏移区间）。行/列计算在格式化阶段完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// 结构化诊断：message 人类可读，`to_json()` 供 AI 定位修复（12.2 API 契约）。
/// JSON 不落字段——完全可由其余字段派生，急切构建+双写同步属冗余（T2 审计去除）。
/// stable 下 serde_json 无法声明为 public dependency，此处显式豁免该告警。
#[allow(exported_private_dependencies)]
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    /// 错误码（20 号 §2：Y-S 语法 / Y-T 类型 / Y-R 运行时 / Y-H 宿主 / Y-I 内部）
    pub code: Option<String>,
    pub span: Option<Span>,
    pub suggestions: Vec<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Self::with_code(None, Severity::Error, message, span)
    }

    /// 带错误码构造（JSON schema v1：code 为一等字段，快照测试锁定）。
    pub fn coded(code: &str, severity: Severity, message: impl Into<String>, span: Span) -> Self {
        Self::with_code(Some(code.to_string()), severity, message, span)
    }

    fn with_code(
        code: Option<String>,
        severity: Severity,
        message: impl Into<String>,
        span: Span,
    ) -> Self {
        Diagnostic {
            severity,
            message: message.into(),
            code,
            span: Some(span),
            suggestions: Vec::new(),
        }
    }

    /// JSON schema v1（快照测试锁定）：severity/message/span 必有，code/suggestions 按需。
    pub fn to_json(&self) -> Value {
        let mut obj = serde_json::json!({
            "severity": match self.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info => "info",
            },
            "message": self.message,
            "span": match self.span {
                Some(s) => serde_json::json!({ "start": s.start, "end": s.end }),
                None => Value::Null,
            },
        });
        let map = obj.as_object_mut().expect("json object");
        if let Some(c) = &self.code {
            map.insert("code".into(), Value::String(c.clone()));
        }
        if !self.suggestions.is_empty() {
            map.insert(
                "suggestions".into(),
                Value::Array(self.suggestions.iter().map(|s| Value::String(s.clone())).collect()),
            );
        }
        obj
    }

    pub fn warning(message: impl Into<String>, span: Span) -> Self {
        Self::with_code(None, Severity::Warning, message, span)
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestions.push(suggestion.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}
