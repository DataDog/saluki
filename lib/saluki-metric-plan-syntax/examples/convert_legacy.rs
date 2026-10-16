//! Converts a source in the old node-and-edge syntax, read from standard input, into the pipeline syntax on standard
//! output. A tool for converting the sources in tests; removed with the old syntax.

use std::io::Read as _;

fn main() {
    let mut source = String::new();
    std::io::stdin()
        .read_to_string(&mut source)
        .expect("standard input is text");
    match saluki_metric_plan_syntax::legacy_to_pipeline(&source) {
        Ok(text) => print!("{}", text),
        Err(error) => {
            eprintln!("{}", error);
            std::process::exit(1);
        }
    }
}
