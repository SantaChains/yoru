//! 临时调试：运行文件并打印结果/错误。
use yoru_syntax::parse_source;
use yoru_vm::{demo, Instance, Vm};

fn main() {
    let path = std::env::args().nth(1).expect("usage");
    let entry = std::env::args().nth(2);
    let src = std::fs::read_to_string(&path).expect("read");
    let program = parse_source(&src).expect("parse");
    let m = yoru_bytecode::lower(&program).expect("lower");
    yoru_bytecode::verify_module(&m).expect("verify");
    let mut inst = Instance::new(m);
    demo::install_demo_host(&mut inst);
    let mut vm = Vm::new();
    match vm.run(inst, entry.as_deref(), vec![]) {
        Ok(v) => println!("= {} ({:?})", v.display(), v),
        Err(e) => println!("! {e}"),
    }
    println!("out: {}", String::from_utf8_lossy(&vm.take_output()));
}
