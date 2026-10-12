//! 调试工具：lower + verify + 反汇编一个 .yoru 文件。
//! 用法：cargo run -p yoru-bytecode --example dump -- <file.yoru>
use yoru_syntax::parse_source;

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump <file.yoru>");
    let src = std::fs::read_to_string(&path).expect("read");
    let program = parse_source(&src).expect("parse");
    match yoru_bytecode::lower(&program) {
        Ok(m) => {
            println!("{}", yoru_bytecode::disasm::disassemble(&m));
            match yoru_bytecode::verify_module(&m) {
                Ok(()) => println!("verify: OK"),
                Err(errs) => {
                    for e in errs {
                        println!("verify: {e}");
                    }
                }
            }
        }
        Err(ds) => {
            for d in ds {
                println!(
                    "lower error: [{}] {}",
                    d.code.as_deref().unwrap_or("-"),
                    d.message
                );
            }
        }
    }
}
