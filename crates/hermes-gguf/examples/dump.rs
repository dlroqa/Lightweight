//! Dump a GGUF file's metadata keys. `cargo run -p hermes-gguf --example dump -- <file>`
use hermes_gguf::GgufFile;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: dump <file.gguf>");
        return;
    };
    let filter = std::env::args().nth(2).unwrap_or_default();
    match GgufFile::open_header_only(&path) {
        Ok(file) => {
            for (key, value) in file.metadata() {
                if !filter.is_empty() && !key.contains(&filter) {
                    continue;
                }
                let rendered = match value {
                    hermes_gguf::GgufValue::String(s) if s.len() > 60 => {
                        format!("String(<{} bytes>)", s.len())
                    }
                    hermes_gguf::GgufValue::Array(summary) if summary.len <= 64 => {
                        match file.read_u64_array(key) {
                            Ok(Some(values)) => format!("Array{values:?}"),
                            _ => format!("{value:?}"),
                        }
                    }
                    other => format!("{other:?}"),
                };
                println!("{key} = {rendered}");
            }
        }
        Err(err) => eprintln!("error: {err}"),
    }
}
