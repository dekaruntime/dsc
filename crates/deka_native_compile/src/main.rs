use std::{env, fs, process::ExitCode};
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dsc-native: {error}");
            ExitCode::FAILURE
        }
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [mode, source] = args.as_slice() else {
        return Err("usage: dsc-native <program|rust> <source.dsx> (output goes to stdout)".into());
    };
    if !matches!(mode.as_str(), "program" | "rust") {
        return Err("mode must be program or rust".into());
    }
    let program = deka_native_compile::compile(&fs::read_to_string(source)?)?;
    if mode == "rust" {
        print!("{}", deka_native_compile::emit_rust(&program));
    } else {
        println!("{}", serde_json::to_string(&program)?);
    }
    Ok(())
}
