//! 深嵌套输入探针：验证 parser 在恶意深嵌套下的行为。
//! 用法：cargo run -p yoru-syntax --example nest_probe -- <depth>...
use yoru_syntax::parse_source;

fn main() {
    let depths: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let depths = if depths.is_empty() { vec![400, 2_000, 10_000] } else { depths };
    for depth in depths {
        let mut src = String::from("pub fn main() -> int {\n    return ");
        src.push_str(&"(".repeat(depth));
        src.push('1');
        src.push_str(&")".repeat(depth));
        src.push_str("\n}\n");
        let r = std::panic::catch_unwind(|| match parse_source(&src) {
            Ok(_) => "ok".to_string(),
            Err(ds) => format!("err x{} first={:?} last={:?}", ds.len(), ds.first().map(|d| &d.message), ds.last().map(|d| &d.message)),
        });
        println!("depth {depth}: {:?}", r.unwrap_or_else(|_| "PANIC".into()));

    }
}
